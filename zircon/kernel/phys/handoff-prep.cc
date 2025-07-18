// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "handoff-prep.h"

#include <lib/boot-options/boot-options.h>
#include <lib/instrumentation/debugdata.h>
#include <lib/llvm-profdata/llvm-profdata.h>
#include <lib/memalloc/pool-mem-config.h>
#include <lib/memalloc/pool.h>
#include <lib/memalloc/range.h>
#include <lib/trivial-allocator/new.h>
#include <lib/zbitl/error-stdio.h>
#include <stdio.h>
#include <string-file.h>
#include <zircon/assert.h>

#include <ktl/tuple.h>
#include <ktl/utility.h>
#include <phys/allocation.h>
#include <phys/arch/arch-handoff.h>
#include <phys/elf-image.h>
#include <phys/handoff.h>
#include <phys/kernel-package.h>
#include <phys/main.h>
#include <phys/new.h>
#include <phys/stdio.h>
#include <phys/symbolize.h>
#include <phys/uart.h>

#include "log.h"
#include "physboot.h"

#include <ktl/enforce.h>

namespace {

// Carve out some physical pages requested for testing before handing off.
void FindTestRamReservation(RamReservation& ram) {
  ZX_ASSERT_MSG(!ram.paddr, "Must use kernel.test.ram.reserve=SIZE without ,ADDRESS!");

  memalloc::Pool& pool = Allocation::GetPool();

  // Don't just use Pool::Allocate because that will use the first (lowest)
  // address with space.  The kernel's PMM initialization doesn't like the
  // earliest memory being split up too small, and anyway that's not very
  // representative of just a normal machine with some device memory elsewhere,
  // which is what the test RAM reservation is really meant to simulate.
  // Instead, find the highest-addressed, most likely large chunk that is big
  // enough and just make it a little smaller, which is probably more like what
  // an actual machine with a little less RAM would look like.

  auto it = pool.end();
  while (true) {
    if (it == pool.begin()) {
      break;
    }
    --it;
    if (it->type == memalloc::Type::kFreeRam && it->size >= ram.size) {
      uint64_t aligned_start = (it->addr + it->size - ram.size) & -uint64_t{ZX_PAGE_SIZE};
      uint64_t aligned_end = aligned_start + ram.size;
      if (aligned_start >= it->addr && aligned_end <= aligned_start + ram.size) {
        if (pool.UpdateRamSubranges(memalloc::Type::kTestRamReserve, aligned_start, ram.size)
                .is_ok()) {
          ram.paddr = aligned_start;
          debugf("%s: kernel.test.ram.reserve carve-out: [%#" PRIx64 ", %#" PRIx64 ")\n",
                 ProgramName(), aligned_start, aligned_end);
          return;
        }
        // Don't try another spot if something went wrong.
        break;
      }
    }
  }

  printf("%s: ERROR: Cannot reserve %#" PRIx64
         " bytes of RAM for kernel.test.ram.reserve request!\n",
         ProgramName(), ram.size);
}

// Returns a pointer into the array that was passed by reference.
constexpr ktl::string_view VmoNameString(const PhysVmo::Name& name) {
  ktl::string_view str(name.data(), name.size());
  return str.substr(0, str.find_first_of('\0'));
}

}  // namespace

HandoffPrep::HandoffPrep(ElfImage kernel)
    : kernel_(ktl::move(kernel)),
      temporary_data_allocator_(VirtualAddressAllocator::TemporaryHandoffDataAllocator(kernel_)),
      permanent_data_allocator_(VirtualAddressAllocator::PermanentHandoffDataAllocator(kernel_)),
      first_class_mapping_allocator_(VirtualAddressAllocator::FirstClassMappingAllocator(kernel_)) {
  PhysHandoffTemporaryPtr<const PhysHandoff> handoff;
  fbl::AllocChecker ac;
  handoff_ = New(handoff, ac);
  ZX_ASSERT_MSG(ac.check(), "Failed to allocate PhysHandoff!");

  ktl::optional spec = kernel_.GetZirconInfo<ZirconAbiSpec>();
  ZX_ASSERT_MSG(spec, "no Zircon ELF note containing the ZirconAbiSpec!");
  spec->AssertValid<ZX_PAGE_SIZE>();
  abi_spec_ = *spec;
}

PhysVmo HandoffPrep::MakePhysVmo(ktl::span<const ktl::byte> data, ktl::string_view name,
                                 size_t content_size) {
  uintptr_t addr = reinterpret_cast<uintptr_t>(data.data());
  ZX_ASSERT((addr % ZX_PAGE_SIZE) == 0);
  ZX_ASSERT((data.size_bytes() % ZX_PAGE_SIZE) == 0);
  ZX_ASSERT(((content_size + ZX_PAGE_SIZE - 1) & -ZX_PAGE_SIZE) == data.size_bytes());

  PhysVmo vmo{.addr = addr, .content_size = content_size};
  vmo.set_name(name);
  return vmo;
}

void HandoffPrep::SetInstrumentation() {
  auto publish_debugdata = [this](ktl::string_view sink_name, ktl::string_view vmo_name,
                                  ktl::string_view vmo_name_suffix, size_t content_size) {
    PhysVmo::Name phys_vmo_name =
        instrumentation::DebugdataVmoName(sink_name, vmo_name, vmo_name_suffix, /*is_static=*/true);

    size_t aligned_size = (content_size + ZX_PAGE_SIZE - 1) & -ZX_PAGE_SIZE;
    fbl::AllocChecker ac;
    ktl::span contents =
        Allocation::New(ac, memalloc::Type::kPhysDebugdata, aligned_size, ZX_PAGE_SIZE).release();
    ZX_ASSERT_MSG(ac.check(), "cannot allocate %zu bytes for instrumentation phys VMO",
                  aligned_size);
    PublishExtraVmo(MakePhysVmo(contents, VmoNameString(phys_vmo_name), content_size));
    return contents;
  };
  for (const ElfImage* module : gSymbolize->modules()) {
    module->PublishDebugdata(publish_debugdata);
  }
}

void HandoffPrep::PublishExtraVmo(PhysVmo&& vmo) { extra_vmos_.push_front(HandoffVmo::New(vmo)); }

void HandoffPrep::FinishVmObjects() {
  ZX_ASSERT_MSG(extra_vmos_.size() <= PhysVmo::kMaxExtraHandoffPhysVmos,
                "Too many phys VMOs in hand-off! %zu > max %zu", extra_vmos_.size(),
                PhysVmo::kMaxExtraHandoffPhysVmos);

  auto populate_vmar = [this](PhysVmar* vmar, ktl::string_view name,
                              HandoffMappingList mapping_list) {
    vmar->set_name(name);
    ktl::span mappings = NewFromList(vmar->mappings, ktl::move(mapping_list));
    ZX_DEBUG_ASSERT(!mappings.empty());
    vmar->base = mappings.front().vaddr;
    uintptr_t vmar_end = mappings.back().vaddr_end();
    vmar->size = vmar_end - vmar->base;
  };

  fbl::AllocChecker ac;
  PhysVmar* temporary_vmar = New(handoff()->temporary_vmar, ac);
  ZX_ASSERT(ac.check());
  populate_vmar(temporary_vmar, "temporary hand-off data",
                temporary_data_allocator_.allocate_function().memory().TakeMappings());

  PhysVmar permanent_data_vmar;
  populate_vmar(&permanent_data_vmar, "permanent hand-off data",
                permanent_data_allocator_.allocate_function().memory().TakeMappings());
  vmars_.push_front(HandoffVmar::New(ktl::move(permanent_data_vmar)));

  NewFromList(handoff()->vmars, ktl::move(vmars_));
  NewFromList(handoff()->extra_vmos, ktl::move(extra_vmos_));
}

void HandoffPrep::SetMemory() {
  // Normalizes types so that only those that are of interest to the kernel
  // remain.
  auto normed_type = [](memalloc::Type type) -> ktl::optional<memalloc::Type> {
    switch (type) {
      // The allocations that should survive into the hand-off.
      case memalloc::Type::kDataZbi:
      case memalloc::Type::kKernel:
      case memalloc::Type::kKernelPageTables:
      case memalloc::Type::kBootMachineStack:
      case memalloc::Type::kBootShadowCallStack:
      case memalloc::Type::kPhysDebugdata:
      case memalloc::Type::kPermanentPhysHandoff:
      case memalloc::Type::kPhysLog:
      case memalloc::Type::kReservedLow:
      case memalloc::Type::kTemporaryPhysHandoff:
      case memalloc::Type::kTestRamReserve:
      case memalloc::Type::kUserboot:
      case memalloc::Type::kVdso:
        return type;

      // The identity map needs to be installed at the time of hand-off, but
      // shouldn't actually be used by the kernel after that; mark it for
      // clean-up.
      case memalloc::Type::kTemporaryIdentityPageTables:
        // TODO(https://fxbug.dev/398950948): Ideally these ranges would be
        // passed on as temporary handoff data, but the kernel currently
        // expects this memory to persist past boot (e.g, for later
        // hotplugging). Pending revisiting that in the kernel, we hand off all
        // "temporary" identity tables as permanent for now.
        return memalloc::Type::kKernelPageTables;

      // An NVRAM range should no longer be treated like normal RAM. The kernel
      // will access it through the mapping provided with PhysHandoff::nvram,
      // and will further key off that to restrict userspace access to this
      // range of memory.
      case memalloc::Type::kNvram:
      // Truncations should now go into effect.
      case memalloc::Type::kTruncatedRam:
      // kPeripheral range content has been distilled in
      // PhysHandoff::periph_ranges and does not need to be present in this
      // accounting.
      case memalloc::Type::kPeripheral:
        return ktl::nullopt;

      default:
        ZX_DEBUG_ASSERT(type != memalloc::Type::kReserved);
        break;
    }

    if (memalloc::IsRamType(type)) {
      return memalloc::Type::kFreeRam;
    }

    // Anything unknown should be ignored.
    return ktl::nullopt;
  };

  auto& pool = Allocation::GetPool();

  // Iterate through once to determine how many normalized ranges there are,
  // informing our allocation of its storage in the handoff.
  size_t len = 0;
  auto count_ranges = [&len](const memalloc::Range& range) {
    ++len;
    return true;
  };
  memalloc::NormalizeRanges(pool, count_ranges, normed_type);

  // Note, however, that New() has allocation side-effects around the creation
  // of temporary hand-off memory. Accordingly, overestimate the length by one
  // possible ranges when allocating the array, and adjust it after the fact.

  fbl::AllocChecker ac;
  ktl::span handoff_ranges = New(handoff()->memory, ac, len + 1);
  ZX_ASSERT_MSG(ac.check(), "cannot allocate %zu bytes for memory handoff",
                len * sizeof(memalloc::Range));

  // Now simply record the normalized ranges.
  auto it = handoff_ranges.begin();
  auto record_ranges = [&it](const memalloc::Range& range) {
    *it++ = range;
    return true;
  };
  memalloc::NormalizeRanges(pool, record_ranges, normed_type);

  handoff()->memory.size_ = it - handoff_ranges.begin();
  handoff_ranges = ktl::span(handoff_ranges.begin(), it);

  if (gBootOptions->phys_verbose) {
    printf("%s: Physical memory handed off to the kernel:\n", ProgramName());
    memalloc::PrintRanges(handoff_ranges, ProgramName());
  }
}

BootOptions& HandoffPrep::SetBootOptions(const BootOptions& boot_options) {
  fbl::AllocChecker ac;
  BootOptions* handoff_options = New(handoff()->boot_options, ac, *gBootOptions);
  ZX_ASSERT_MSG(ac.check(), "cannot allocate handoff BootOptions!");

  if (handoff_options->test_ram_reserve) {
    FindTestRamReservation(*handoff_options->test_ram_reserve);
  }

  return *handoff_options;
}

void HandoffPrep::PublishLog(ktl::string_view name, Log&& log) {
  if (log.empty()) {
    return;
  }

  const size_t content_size = log.size_bytes();
  Allocation buffer = ktl::move(log).TakeBuffer();
  ZX_ASSERT(content_size <= buffer.size_bytes());

  PublishExtraVmo(MakePhysVmo(buffer.data(), name, content_size));

  // Intentionally leak as the PhysVmo now tracks this memory.
  ktl::ignore = buffer.release();
}

void HandoffPrep::UsePackageFiles(KernelStorage::Bootfs kernel_package) {
  auto& pool = Allocation::GetPool();
  const ktl::string_view userboot = gBootOptions->userboot.data();
  for (auto it = kernel_package.begin(); it != kernel_package.end(); ++it) {
    ktl::span data = it->data;
    uintptr_t start = reinterpret_cast<uintptr_t>(data.data());
    // These are decompressed BOOTFS payloads, so there is only padding up to
    // the next page boundary.
    ktl::span aligned_data{data.data(), (data.size_bytes() + ZX_PAGE_SIZE - 1) & -ZX_PAGE_SIZE};
    if (it->name == userboot) {
      ZX_ASSERT(
          pool.UpdateRamSubranges(memalloc::Type::kUserboot, start, aligned_data.size()).is_ok());
      handoff_->userboot = MakePhysElfImage(it, it->name);
    }
    if (it->name == "version-string.txt"sv) {
      ktl::string_view version{reinterpret_cast<const char*>(data.data()), data.size()};
      SetVersionString(version);
    } else if (it->name == "vdso"sv) {
      ZX_ASSERT(pool.UpdateRamSubranges(memalloc::Type::kVdso, start, aligned_data.size()).is_ok());
      handoff_->vdso = MakePhysElfImage(it, "vdso/next"sv);
    }
  }
  if (auto result = kernel_package.take_error(); result.is_error()) {
    zbitl::PrintBootfsError(result.error_value());
  }
  ZX_ASSERT_MSG(handoff_->vdso.vmar != PhysVmar{},
                "\n*** No vdso ELF file found "
                " in kernel package %.*s (VMO size %#zx) ***",
                static_cast<int>(kernel_package.directory().size()),
                kernel_package.directory().data(), handoff_->userboot.vmo.content_size);
  ZX_ASSERT_MSG(handoff_->userboot.vmar != PhysVmar{},
                "\n*** kernel.select.userboot=%.*s but no such ELF file"
                " in kernel package %.*s (VMO size %#zx) ***",
                static_cast<int>(userboot.size()), userboot.data(),
                static_cast<int>(kernel_package.directory().size()),
                kernel_package.directory().data(), handoff_->userboot.vmo.content_size);
  ZX_ASSERT_MSG(!handoff_->version_string.empty(), "no version.txt file in kernel package");
}

void HandoffPrep::SetVersionString(ktl::string_view version) {
  constexpr ktl::string_view kSpace = " \t\r\n";
  size_t skip = version.find_first_not_of(kSpace);
  size_t trim = version.find_last_not_of(kSpace);
  if (skip == ktl::string_view::npos || trim == ktl::string_view::npos) {
    ZX_PANIC("version.txt of %zu chars empty after trimming whitespace", version.size());
  }
  trim = version.size() - (trim + 1);
  version.remove_prefix(skip);
  version.remove_suffix(trim);

  fbl::AllocChecker ac;
  ktl::string_view installed = New(handoff_->version_string, ac, version);
  if (!ac.check()) {
    ZX_PANIC("cannot allocate %zu chars of handoff space for version string", version.size());
  }
  ZX_ASSERT(installed == version);
  if (gBootOptions->phys_verbose) {
    if (skip + trim == 0) {
      printf("%s: zx_system_get_version_string (%zu chars): %.*s\n", ProgramName(), version.size(),
             static_cast<int>(version.size()), version.data());
    } else {
      printf("%s: zx_system_get_version_string (%zu chars trimmed from %zu): %.*s\n", ProgramName(),
             version.size(), version.size() + skip + trim, static_cast<int>(version.size()),
             version.data());
    }
  }
}

PhysElfImage HandoffPrep::MakePhysElfImage(KernelStorage::Bootfs::iterator file,
                                           ktl::string_view name) {
  ElfImage elf;
  if (auto result = elf.InitFromFile(file, false); result.is_error()) {
    elf.Printf(result.error_value());
    abort();
  }
  elf.set_load_address(0);

  if (auto result = elf.SeparateZeroFill(); result.is_error()) {
    elf.Printf(result.error_value());
    abort();
  }

  PhysElfImage handoff_elf = {
      .vmo = MakePhysVmo(elf.aligned_memory_image(), name, file->data.size()),
      .vmar = {.size = elf.vaddr_size()},
      .info = {
          .relative_entry_point = elf.entry(),
          .stack_size = elf.stack_size(),
      }};

  fbl::AllocChecker ac;
  ktl::span<PhysMapping> mappings =
      New(handoff_elf.vmar.mappings, ac, elf.load_info().segments().size());
  if (!ac.check()) {
    ZX_PANIC("cannot allocate %zu bytes of handoff space for ELF image details",
             sizeof(PhysMapping) * elf.load_info().segments().size());
  }
  elf.load_info().VisitSegments(
      [load_bias = elf.load_bias(), &mappings](const auto& segment) -> bool {
        PhysMapping& mapping = mappings.front();
        mappings = mappings.subspan(1);
        mapping = PhysMapping{
            "",
            PhysMapping::Type::kNormal,
            segment.vaddr() + load_bias,
            segment.memsz(),
            segment.filesz() == 0 ? PhysElfImage::kZeroFill : segment.offset(),
            PhysMapping::Permissions::FromSegment(segment),
        };
        return true;
      });
  ZX_DEBUG_ASSERT(mappings.empty());

  return handoff_elf;
}

[[noreturn]] void HandoffPrep::DoHandoff(UartDriver& uart, ktl::span<ktl::byte> zbi,
                                         const KernelStorage::Bootfs& kernel_package,
                                         const ArchPatchInfo& patch_info) {
  // Hand off the boot options first, which don't really change.  But keep a
  // mutable reference to update boot_options.serial later to include live
  // driver state and not just configuration like other BootOptions members do.
  BootOptions& handoff_options = SetBootOptions(*gBootOptions);

  // Use the updated copy from now on.
  gBootOptions = &handoff_options;

  UsePackageFiles(kernel_package);

  SummarizeMiscZbiItems(zbi);
  gBootTimes.SampleNow(PhysBootTimes::kZbiDone);

  SetInstrumentation();

  // This transfers the log, so logging after this is not preserved.
  // Extracting the log buffer will automatically detach it from stdout.
  // TODO(mcgrathr): Rename to physboot.log with some prefix.
  PublishLog("i/logs/physboot", ktl::move(*ktl::exchange(gLog, nullptr)));

  handoff()->kernel_physical_load_address = kernel_.physical_load_address();
  ZirconAbi abi = ConstructKernelAddressSpace(uart);

  // Finalize the published VMOs (e.g., the log published just above), VMARs,
  // and mappings.
  FinishVmObjects();

  // This must be called last, as this finalizes the state of memory to hand off
  // to the kernel, which is affected by other set-up routines.
  SetMemory();

  // One last log before the next line where we effectively disable logging
  // altogether.
  debugf("%s: Handing off at physical load address %#" PRIxPTR ", entry %#" PRIx64 "...\n",
         gSymbolize->name(), kernel_.physical_load_address(), kernel_.entry());

  // Hand-off the serial driver. There may be no more logging beyond this point.
  handoff()->uart = ktl::move(uart).TakeUart();

  // Now that all time samples have been collected, copy gBootTimes into the
  // hand-off.
  handoff()->times = gBootTimes;

  // Now for the remaining arch-specific settings and the actual hand-off...
  ArchDoHandoff(abi, patch_info);
}
