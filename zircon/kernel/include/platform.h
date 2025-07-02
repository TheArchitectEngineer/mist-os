// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_H_

#include <lib/zbi-format/reboot.h>
#include <lib/zx/result.h>
#include <sys/types.h>
#include <zircon/boot/crash-reason.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <dev/power.h>
#include <kernel/cpu.h>

#define BOOT_CPU_ID 0

typedef enum {
  HALT_ACTION_HALT = 0,           // Spin forever.
  HALT_ACTION_REBOOT,             // Reset the CPU.
  HALT_ACTION_REBOOT_BOOTLOADER,  // Reboot into the bootloader.
  HALT_ACTION_REBOOT_RECOVERY,    // Reboot into the recovery partition.
  HALT_ACTION_SHUTDOWN,           // Shutdown and power off.
} platform_halt_action;

// Holds platform-specific, per-cpu state used to suspend/resume a CPU.
struct PlatformCpuResumeState {
#if defined(__aarch64__)
  uint64_t cntkctl_el1{};
#endif
};

/* super early platform initialization, before almost everything */
void platform_early_init();

/* Perform any set up required before virtual memory is enabled, or the heap is set up. */
void platform_prevm_init();

/* later init, after the kernel has come up */
void platform_init();

/* platform_halt halts the system and performs the |suggested_action|.
 *
 * This function is used in both the graceful shutdown and panic paths so it
 * does not perform more complex actions like switching to the primary CPU,
 * unloading the run queue of secondary CPUs, stopping secondary CPUs, etc.
 *
 * There is no returning from this function.
 */
void platform_halt(platform_halt_action suggested_action, zircon_crash_reason_t reason) __NO_RETURN;

/* The platform specific actions to be taken in a halt situation.  This is a
 * weak symbol meant to be overloaded by platform specific implementations and
 * called from the common |platform_halt| implementation.  Do not call this
 * function directly, call |platform_halt| instead.
 *
 * There is no returning from this function.
 */
void platform_specific_halt(platform_halt_action suggested_action, zircon_crash_reason_t reason,
                            bool halt_on_panic) __NO_RETURN;

/* optionally stop the current cpu in a way the platform finds appropriate */
void platform_halt_cpu();

// Returns true iff this platform supports |platform_suspend_cpu|.
bool platform_supports_suspend_cpu();

// Suspend the calling CPU.
//
// On success, the CPU will enter an implementation-defined suspend state.
//
// Prior to calling this function, it's critical to set up some kind of
// interrupt that will wake the CPU and resume execution.  Upon successful
// suspend and resume, this call returns ZX_OK.
//
// Prior to calling, interrupts must be disabled, preemption must be disabled,
// and the caller must be pinned to the calling CPU.
//
// The calling thread must be a kernel-only thread.  Because this routine saves
// only the minimum required register state, it is an error to call this
// function on a thread that has a "user half" (ThreadDispatcher).
//
// Possible error results include (but are not limited to),
//
// ZX_ERR_NOT_SUPPORTED if unsupported on this platform.
//
// ZX_ERR_ACCESS_DENIED or ZX_ERR_INVALID_ARGS if the suspend power state is
// incompatible with the current power state of other CPUs.  This is a somewhat
// normal error and must be handled by callers.
//
// TODO(https://fxbug.dev/414456459): Consider adding a platform-specific or
// custom result type to differentiates between success-but-did-not-power-down
// and powered-down.
//
// TODO(https://fxbug.dev/414456459): Consider checking the state of the
// secondary CPUs in order to determine which power state value to use so that
// we can eliminate the need to return ZX_ERR_ACCESS_DENIED or
// ZX_ERR_INVALID_ARGS in "normal" situations.
//
// TODO(https://fxbug.dev/414456459): Currently, the caller is responsible for
// determining if pausing/resuming the monotonic clock is necessary and then
// actually doing it.  More thought needs to be given to where that logic should
// live.  Right now, it's done in |IdlePowerThread::UpdateMonotonicClock|,
// however, it might be better to move some of that logic down into the platform
// layer (perhaps within |platform_suspend_cpu|).
zx_status_t platform_suspend_cpu();

// Returns true if this system has a debug serial port that is enabled
bool platform_serial_enabled();

// Accessors for the HW reboot reason which may or may not have been delivered
// by the bootloader.
void platform_set_hw_reboot_reason(zbi_hw_reboot_reason_t reason);
zbi_hw_reboot_reason_t platform_hw_reboot_reason();

// platform_panic_start informs the system that a panic message is about
// to be printed and that platform_halt will be called shortly.  The
// platform should stop other CPUs if requested and do whatever is necessary
// to safely ensure that the panic message will be visible to the user.
enum class PanicStartHaltOtherCpus { No = 0, Yes };
void platform_panic_start(PanicStartHaltOtherCpus option = PanicStartHaltOtherCpus::Yes);

/* start the given cpu in a way the platform finds appropriate */
zx_status_t platform_start_cpu(cpu_num_t cpu_id, uint64_t mpid);

// Get the state of a CPU.
zx::result<power_cpu_state> platform_get_cpu_state(cpu_num_t cpu_id);

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_H_
