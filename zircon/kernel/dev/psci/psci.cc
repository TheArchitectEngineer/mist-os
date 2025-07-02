// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include <inttypes.h>
#include <lib/console.h>
#include <lib/zbi-format/driver-config.h>
#include <string.h>
#include <trace.h>

#include <arch/arm64.h>
#include <arch/arm64/smccc.h>
#include <dev/psci.h>
#include <pdev/power.h>
#include <vm/handoff-end.h>

#define LOCAL_TRACE 0

namespace {

bool psci_cpu_suspend_supported = false;
bool psci_set_suspend_mode_supported = false;

uint64_t shutdown_args[3] = {0, 0, 0};
uint64_t reboot_args[3] = {0, 0, 0};
uint64_t reboot_bootloader_args[3] = {0, 0, 0};
uint64_t reboot_recovery_args[3] = {0, 0, 0};
uint32_t reset_command = PSCI64_SYSTEM_RESET;

zx_status_t psci_status_to_zx_status(uint64_t psci_result);
zx_status_t psci_status_to_zx_status(uint64_t psci_result) {
  int64_t status = static_cast<int64_t>(psci_result);
  switch (status) {
    case PSCI_SUCCESS:
      return ZX_OK;
    case PSCI_NOT_SUPPORTED:
    case PSCI_DISABLED:
      return ZX_ERR_NOT_SUPPORTED;
    case PSCI_INVALID_PARAMETERS:
      return ZX_ERR_INVALID_ARGS;
    case PSCI_INVALID_ADDRESS:
      return ZX_ERR_OUT_OF_RANGE;
    case PSCI_DENIED:
      return ZX_ERR_ACCESS_DENIED;
    case PSCI_ALREADY_ON:
      return ZX_ERR_BAD_STATE;
    case PSCI_ON_PENDING:
      return ZX_ERR_SHOULD_WAIT;
    case PSCI_INTERNAL_FAILURE:
      return ZX_ERR_INTERNAL;
    case PSCI_NOT_PRESENT:
      return ZX_ERR_NOT_FOUND;
    default:
      return ZX_ERR_BAD_STATE;
  }
}

uint64_t psci_smc_call(uint32_t function, uint64_t arg0, uint64_t arg1, uint64_t arg2) {
  LTRACEF("0x%x 0x%" PRIx64 " 0x%" PRIx64 " 0x%" PRIx64 "\n", function, arg0, arg1, arg2);
  return arm_smccc_smc(function, arg0, arg1, arg2, 0, 0, 0, 0).x0;
}

uint64_t psci_hvc_call(uint32_t function, uint64_t arg0, uint64_t arg1, uint64_t arg2) {
  return arm_smccc_hvc(function, arg0, arg1, arg2, 0, 0, 0, 0).x0;
}

using psci_call_proc = uint64_t (*)(uint32_t, uint64_t, uint64_t, uint64_t);

psci_call_proc do_psci_call = psci_smc_call;

}  // anonymous namespace

// Saves register state in |context|, then issues a PSCI call, using
// |psci_call|, for the specified |psci_func|, passing along the supplied
// |power_state|, |entry|, and |context| arguments.
//
// This function is designed to be called with PSCI64_CPU_SUSPEND and work with
// |psci_do_resume| and |arm64_secondary_start|.
//
// In all cases, this function will always appear to return to its caller.
// Depending on the PSCI implementation and the conditions, it may do so
// directly, or in the case of a successful CPU_SUSPEND, it may do so via
// |arm64_secondary_start| and |psci_do_resume|.
//
// If the return value is less than or equal to zero, then control did not
// branch to |entry| and the return value should be interpreted as a PSCI return
// value (see section 5.4.5 of DEN0022F.b):
//
//   PSCI_SUCCESS - The CPU_SUSPEND call succeeded.  However, the CPU did not
//   reach a power down state because of a pending interrupt, or simply because
//   the requested |power_state| is not a power down state.
//
//   PSCI_INVALID_PARAMETERS - The |power_state| is invalid, or a low-power
//   state was requested for a higher-than-core-level topology node
//   (e.g. cluster) and at least one of the children in that node is in a local
//   low-power state that is incompatible with the request.
//
//   PSCI_DENIED - A low-power state is requested for a higher-than-core-level
//   topology node (e.g. cluster) and all the cores that are in an incompatible
//   state with the request are running, as opposed to being in a low-power
//   state.
//
//   PSCI_INVALID_ADDRESS - |entry| is not a valid physical address.
//
// If the return value is greater than zero, then the CPU did in fact suspend
// and resume execution at |entry| before returning to the caller.
//
// Implemented in assembly.
extern "C" int64_t psci_do_suspend(psci_call_proc psci_call, uint32_t power_state, paddr_t entry,
                                   psci_cpu_resume_context* context);
extern "C" zx_status_t psci_do_resume(psci_cpu_resume_context* context) __NO_RETURN;

zx_status_t psci_system_off() {
  return psci_status_to_zx_status(
      do_psci_call(PSCI64_SYSTEM_OFF, shutdown_args[0], shutdown_args[1], shutdown_args[2]));
}

uint32_t psci_get_version() {
  return static_cast<uint32_t>(do_psci_call(PSCI64_PSCI_VERSION, 0, 0, 0));
}

/* powers down the calling cpu - only returns if call fails */
zx_status_t psci_cpu_off() {
  return psci_status_to_zx_status(do_psci_call(PSCI64_CPU_OFF, 0, 0, 0));
}

zx_status_t psci_cpu_on(uint64_t mpid, paddr_t entry, uint64_t context) {
  LTRACEF("CPU_ON mpid %#" PRIx64 ", entry %#" PRIx64 "\n", mpid, entry);
  return psci_status_to_zx_status(do_psci_call(PSCI64_CPU_ON, mpid, entry, context));
}

PsciCpuSuspendResult psci_cpu_suspend(uint32_t power_state) {
  LTRACE_ENTRY;

  DEBUG_ASSERT(arch_ints_disabled());

  if (!psci_cpu_suspend_supported) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  const paddr_t entry_pa = KernelPhysicalAddressOf<arm64_secondary_start>();

  psci_cpu_resume_context context{};

  LTRACEF("cpu %u, psci_call_routine 0x%" PRIx64 ", power_state 0x%x, entry 0x%" PRIx64
          ", context %p\n",
          arch_curr_cpu_num(), reinterpret_cast<uintptr_t>(do_psci_call), power_state, entry_pa,
          &context);

  const int64_t result = psci_do_suspend(do_psci_call, power_state, entry_pa, &context);
  if (result > 0) {
    // We took the "long way" and restored CPU context from a power down state.
    return zx::ok(CpuPoweredDown::Yes);
  }

  switch (result) {
    case PSCI_SUCCESS:
      return zx::ok(CpuPoweredDown::No);
    case PSCI_INVALID_PARAMETERS:
      return zx::error(ZX_ERR_INVALID_ARGS);
    case PSCI_DENIED:
      return zx::error(ZX_ERR_ACCESS_DENIED);
    default:
      panic("cpu %u, psci_call_routine 0x%" PRIx64 ", power_state 0x%x, entry 0x%" PRIx64
            ", context %p\n",
            arch_curr_cpu_num(), reinterpret_cast<uintptr_t>(do_psci_call), power_state, entry_pa,
            &context);
  };
}

int64_t psci_get_affinity_info(uint64_t mpid) {
  return static_cast<int64_t>(do_psci_call(PSCI64_AFFINITY_INFO, mpid, 0, 0));
}

zx::result<power_cpu_state> psci_get_cpu_state(uint64_t mpid) {
  int64_t aff_info = psci_get_affinity_info(mpid);
  switch (aff_info) {
    case 0:
      return zx::success(power_cpu_state::ON);
    case 1:
      return zx::success(power_cpu_state::OFF);
    case 2:
      return zx::success(power_cpu_state::ON_PENDING);
    default:
      return zx::error(psci_status_to_zx_status(aff_info));
  }
}

uint32_t psci_get_feature(uint32_t psci_call) {
  return static_cast<uint32_t>(do_psci_call(PSCI64_PSCI_FEATURES, psci_call, 0, 0));
}

zx_status_t psci_system_reset2_raw(uint32_t reset_type, uint32_t cookie) {
  dprintf(INFO, "PSCI SYSTEM_RESET2: %#" PRIx32 " %#" PRIx32 "\n", reset_type, cookie);

  uint64_t psci_status = do_psci_call(PSCI64_SYSTEM_RESET2, reset_type, cookie, 0);

  dprintf(INFO, "PSCI SYSTEM_RESET2 returns %" PRIi64 "\n", static_cast<int64_t>(psci_status));

  return psci_status_to_zx_status(psci_status);
}

zx_status_t psci_system_reset(power_reboot_flags flags) {
  uint64_t* args = reboot_args;

  if (flags == power_reboot_flags::REBOOT_BOOTLOADER) {
    args = reboot_bootloader_args;
  } else if (flags == power_reboot_flags::REBOOT_RECOVERY) {
    args = reboot_recovery_args;
  }

  dprintf(INFO, "PSCI reboot: %#" PRIx32 " %#" PRIx64 " %#" PRIx64 " %#" PRIx64 "\n", reset_command,
          args[0], args[1], args[2]);
  return psci_status_to_zx_status(do_psci_call(reset_command, args[0], args[1], args[2]));
}

zx_status_t psci_set_suspend_mode(psci_suspend_mode mode) {
  return psci_status_to_zx_status(do_psci_call(PSCI64_PSCI_SET_SUSPEND_MODE, mode, 0, 0));
}

bool psci_is_set_suspend_mode_supported() { return psci_set_suspend_mode_supported; }

bool psci_is_cpu_suspend_supported() { return psci_cpu_suspend_supported; }

void PsciInit(const zbi_dcfg_arm_psci_driver_t& config) {
  do_psci_call = config.use_hvc ? psci_hvc_call : psci_smc_call;
  memcpy(shutdown_args, config.shutdown_args, sizeof(shutdown_args));
  memcpy(reboot_args, config.reboot_args, sizeof(reboot_args));
  memcpy(reboot_bootloader_args, config.reboot_bootloader_args, sizeof(reboot_bootloader_args));
  memcpy(reboot_recovery_args, config.reboot_recovery_args, sizeof(reboot_recovery_args));

  // read information about the psci implementation
  uint32_t result = psci_get_version();
  uint32_t major = (result >> 16) & 0xffff;
  uint32_t minor = result & 0xffff;
  dprintf(INFO, "PSCI version %u.%u\n", major, minor);

  if (major >= 1 && major != 0xffff) {
    // query features
    dprintf(INFO, "PSCI supported features:\n");

    // Prints info about the features and returns supported flags or nullopt if not supported.
    auto probe_feature = [](uint32_t feature, const char* feature_name) -> ktl::optional<uint32_t> {
      uint32_t result = psci_get_feature(feature);
      if (static_cast<int32_t>(result) < 0) {
        // Not supported
        return ktl::nullopt;
      }
      dprintf(INFO, "\t%s (0x%x)\n", feature_name, result);
      return result;
    };

    psci_cpu_suspend_supported = probe_feature(PSCI64_CPU_SUSPEND, "CPU_SUSPEND").has_value();
    probe_feature(PSCI64_CPU_OFF, "CPU_OFF");
    probe_feature(PSCI64_CPU_ON, "CPU_ON");
    probe_feature(PSCI64_AFFINITY_INFO, "AFFINITY_INFO");
    probe_feature(PSCI64_MIGRATE, "MIGRATE");
    probe_feature(PSCI64_MIGRATE_INFO_TYPE, "MIGRATE_INFO_TYPE");
    probe_feature(PSCI64_MIGRATE_INFO_UP_CPU, "MIGRATE_INFO_UP_CPU");
    probe_feature(PSCI64_SYSTEM_OFF, "SYSTEM_OFF");
    probe_feature(PSCI64_SYSTEM_RESET, "SYSTEM_RESET");
    if (probe_feature(PSCI64_SYSTEM_RESET2, "SYSTEM_RESET2").has_value()) {
      // Prefer RESET2 if present. It explicitly supports arguments, but some vendors have
      // extended RESET to behave the same way.
      reset_command = PSCI64_SYSTEM_RESET2;
    }
    probe_feature(PSCI64_CPU_FREEZE, "CPU_FREEZE");
    probe_feature(PSCI64_CPU_DEFAULT_SUSPEND, "CPU_DEFAULT_SUSPEND");
    probe_feature(PSCI64_NODE_HW_STATE, "NODE_HW_STATE");
    probe_feature(PSCI64_SYSTEM_SUSPEND, "SYSTEM_SUSPEND");
    psci_set_suspend_mode_supported =
        probe_feature(PSCI64_PSCI_SET_SUSPEND_MODE, "PSCI_SET_SUSPEND_MODE").has_value();
    probe_feature(PSCI64_PSCI_STAT_RESIDENCY, "PSCI_STAT_RESIDENCY");
    probe_feature(PSCI64_PSCI_STAT_COUNT, "PSCI_STAT_COUNT");
    probe_feature(PSCI64_MEM_PROTECT, "MEM_PROTECT");
    probe_feature(PSCI64_MEM_PROTECT_RANGE, "MEM_PROTECT_RANGE");

    probe_feature(PSCI64_SMCCC_VERSION, "PSCI64_SMCCC_VERSION");
  }

  // Register with the pdev power driver.
  static const pdev_power_ops psci_ops = {
      .reboot = psci_system_reset,
      .shutdown = psci_system_off,
      .cpu_off = psci_cpu_off,
      .cpu_on = psci_cpu_on,
      .get_cpu_state = psci_get_cpu_state,
  };

  pdev_register_power(&psci_ops);
}

namespace {

int cmd_psci(int argc, const cmd_args* argv, uint32_t flags) {
  if (argc < 2) {
  notenoughargs:
    printf("not enough arguments\n");
    printf("%s system_reset\n", argv[0].str);
    printf("%s system_off\n", argv[0].str);
    printf("%s cpu_on <mpidr>\n", argv[0].str);
    printf("%s affinity_info <cluster> <cpu>\n", argv[0].str);
    printf("%s <function_id> [arg0] [arg1] [arg2]\n", argv[0].str);
    return -1;
  }

  if (!strcmp(argv[1].str, "system_reset")) {
    psci_system_reset(power_reboot_flags::REBOOT_NORMAL);
  } else if (!strcmp(argv[1].str, "system_off")) {
    psci_system_off();
  } else if (!strcmp(argv[1].str, "cpu_on")) {
    if (argc < 3) {
      goto notenoughargs;
    }

    paddr_t secondary_entry_paddr = KernelPhysicalAddressOf<arm64_secondary_start>();
    uint32_t ret = psci_cpu_on(argv[2].u, secondary_entry_paddr, 0);
    printf("psci_cpu_on returns %u\n", ret);
  } else if (!strcmp(argv[1].str, "affinity_info")) {
    if (argc < 4) {
      goto notenoughargs;
    }

    int64_t ret = psci_get_affinity_info(ARM64_MPID(argv[2].u, argv[3].u));
    printf("affinity info returns %ld\n", ret);
  } else {
    uint32_t function = static_cast<uint32_t>(argv[1].u);
    uint64_t arg0 = (argc >= 3) ? argv[2].u : 0;
    uint64_t arg1 = (argc >= 4) ? argv[3].u : 0;
    uint64_t arg2 = (argc >= 5) ? argv[4].u : 0;

    uint64_t ret = do_psci_call(function, arg0, arg1, arg2);
    printf("do_psci_call returned %" PRIu64 "\n", ret);
  }
  return 0;
}

STATIC_COMMAND_START
STATIC_COMMAND_MASKED("psci", "execute PSCI command", &cmd_psci, CMD_AVAIL_ALWAYS)
STATIC_COMMAND_END(psci)

}  // anonymous namespace
