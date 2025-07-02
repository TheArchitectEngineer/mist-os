// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_BOOT_SHIM_INCLUDE_PHYS_BOOT_SHIM_DEVICETREE_H_
#define ZIRCON_KERNEL_PHYS_BOOT_SHIM_INCLUDE_PHYS_BOOT_SHIM_DEVICETREE_H_

#include <lib/boot-options/boot-options.h>
#include <lib/boot-shim/devicetree-boot-shim.h>
#include <lib/boot-shim/devicetree.h>
#include <lib/boot-shim/pool-mem-config.h>
#include <lib/boot-shim/reboot-reason.h>
#include <lib/boot-shim/uart.h>
#include <lib/devicetree/devicetree.h>
#include <lib/memalloc/range.h>
#include <lib/mmio-ptr/mmio-ptr.h>
#include <lib/zbi-format/memory.h>
#include <lib/zbitl/view.h>

#include <ktl/align.h>
#include <ktl/bit.h>
#include <ktl/optional.h>
#include <ktl/string_view.h>
#include <ktl/type_traits.h>
#include <phys/address-space.h>
#include <phys/arch/arch-phys-info.h>
#include <phys/boot-zbi.h>
#include <phys/main.h>
#include <phys/new.h>
#include <phys/stdio.h>
#include <phys/symbolize.h>
#include <phys/uart.h>

// Bootstrapping data from the devicetree blob.
struct DevicetreeBoot {
  // Command line from the the devicetree.
  ktl::string_view cmdline;

  // Ramdisk encoded in the devicetree.
  zbitl::ByteView ramdisk;

  // Devicetree span.
  devicetree::Devicetree fdt;

  // Possible nvram range provided by the bootloader.
  ktl::optional<zbi_nvram_t> nvram;
};

// Generic Watchdog MMIO abstraction implementation for phys environment.
struct WatchdogMmioHelper {
  static uint32_t Read(uint64_t addr) {
    auto* ptr = reinterpret_cast<MMIO_PTR volatile uint32_t*>(addr);
    return MmioRead32(ptr);
  }

  static void Write(uint64_t addr, uint32_t value) {
    auto* ptr = reinterpret_cast<MMIO_PTR volatile uint32_t*>(addr);
    MmioWrite32(value, ptr);
  }
};

// Instance populated by InitMemory().
extern DevicetreeBoot gDevicetreeBoot;

using Arm64StandardBootShimItems =
    boot_shim::StandardBootShimItems<boot_shim::UartItem<>,                               //
                                     boot_shim::PoolMemConfigItem,                        //
                                     boot_shim::NvramItem,                                //
                                     boot_shim::DevicetreeSerialNumberItem,               //
                                     boot_shim::ArmDevicetreePsciItem,                    //
                                     boot_shim::ArmDevicetreeGicItem,                     //
                                     boot_shim::DevicetreeDtbItem,                        //
                                     boot_shim::GenericWatchdogItem<WatchdogMmioHelper>,  //
                                     boot_shim::ArmDevicetreeCpuTopologyItem,             //
                                     boot_shim::RebootReasonItem,                         //
                                     boot_shim::ArmDevicetreeTimerItem>;

// Accessor obtaining the boot hart ID, RISCV only.
struct BootHartIdGetter {
  static uint64_t Get();
};

using Riscv64StandardBootShimItems = boot_shim::StandardBootShimItems<
    boot_shim::UartItem<>,                                        //
    boot_shim::PoolMemConfigItem,                                 //
    boot_shim::NvramItem,                                         //
    boot_shim::DevicetreeSerialNumberItem,                        //
    boot_shim::RiscvDevicetreePlicItem,                           //
    boot_shim::RiscvDevicetreeTimerItem,                          //
    boot_shim::RiscvDevicetreeCpuTopologyItem<BootHartIdGetter>,  //
    boot_shim::DevicetreeDtbItem>;

struct ShimOptions {
  // When true the BootDriver will generated peripheral ranges from its items.
  bool generate_peripheral_ranges = false;

  // When true the BootDriver will enable the MMU.
  bool enable_mmu = false;
};

// Used to conditionally provide an `AddressSpace` object when `ShimOptions::enable_mmu` is true.
template <bool EnableMmu>
struct MaybeAddressSpace {
  constexpr AddressSpace* Get() { return &aspace; }

  AddressSpace aspace;
};

template <>
struct MaybeAddressSpace<false> {
  constexpr AddressSpace* Get() { return nullptr; }
};

// Helper class that drives `PhysMain` on different boot shims.
//
// This provides a mechanism for external users to fork a shim, inject custom code and still benefit
// from upstream changes, without requiring manual soft-migrations.
//
// After selecting the `Shim` type through `boot_shim::XXXXStandardBootShimItems<...>::Shim`, the
// user may customize the setup sequence through `ShimOption` template values. The default set of
// options is none (no extra-work).
//
// Example:
// my-boot-shim.cc
// ```
// namespace {
// // Options that configure the helper class.
// constexpr ShimOptions kOptions = {
//    .generate_peripheral_ranges = true,
//    .enable_mmu = true,
// };
//
// // This generates a `boot_shim::DevicetreeBootShim` with all the items in
// // `Arm64StandardBootShimItems`. Can use `Add` or `Remove` to customize the list,
// // or instantiate`boot_shim::DevicetreeBootShim` directly. using Shim =
// Arm64StandardBootShimItems::Shim<boot_shim::DevicetreeBootShim>;
// }
//
//  // Entry point for the shim.
//  void PhysMain(void* flat_devicetree_blob, arch::EarlyTicks ticks) {
//    BootShimHelper<Shim, kOptions> shim_helper("foo-bar-shim", flat_devicetree_blob);
//    // UART, memory and `gDevicetreeBoot` is available now.
//    // Depending on options MMU may be enabled as well.
//
//    // Items will be initialized, and the matching process will begin.
//    ZX_ASSERT(shim_helper.InitItems());
//    // At this point the shim has initialized its items, and is able to determine how much
//    // space its needed for the ZBI.
//
//    // Item state can be programmatically changed here. In this example we use one item to
//    // initialize another.
//    shim_helper.shim().Get<MyItemTypeFoo>.FromOtherItem(shim_helper.shim().Get<MyOtherTime>());
//
//    // Items will be appended to the zbi, and we will boot into the kernel in that zbi.
//    shim_helper.Boot();
// }
// ```
//
template <typename Shim, ShimOptions options = {}>
class BootShimHelper {
 public:
  // After calling the constructor, memory is fully initialized and `gDevicetreeBoot` is
  // initialized.
  explicit BootShimHelper(const char* shim_name, void* boot_payload)
      : BootShimHelper(shim_name, boot_payload, []() {}) {}

  // Same as above, but allows injecting custom code to execute before `InitMemory` or `ArchSetup`.
  explicit BootShimHelper(const char* shim_name, void* boot_payload, auto&& after_relocs_cb)
      : symbolize_((
            // Must happen before initializing the symbolize object.
            ApplyRelocations(), InitStdout(), after_relocs_cb(), shim_name)),
        shim_(shim_name, [this, boot_payload]() {
          // Will initialize `gDevicetreeBoot`.
          InitMemory(boot_payload, {}, maybe_aspace_.Get());
          void* zbi =
              reinterpret_cast<void*>(const_cast<std::byte*>(gDevicetreeBoot.ramdisk.data()));
          EarlyBootZbiBytes early_zbi_bytes{zbi};
          EarlyBootZbi early_zbi{&early_zbi_bytes};
          ArchSetUp(early_zbi);
          return devicetree::Devicetree(devicetree::ByteView(
              static_cast<const uint8_t*>(boot_payload), std::numeric_limits<uintptr_t>::max()));
        }()) {}

  // Initializes standard items that are not `DevicetreeItems`.
  // Extra items in `Shim` must be initialized before calling this method.
  bool InitItems() {
    shim_.set_cmdline(gDevicetreeBoot.cmdline);
    shim_.set_allocator([](size_t size, size_t align, fbl::AllocChecker& ac) -> void* {
      return new (ktl::align_val_t{align}, gPhysNew<memalloc::Type::kPhysScratch>, ac)
          uint8_t[size];
    });

    // Determine at compile time if the item is present, if so initialize them.
    if constexpr (options.generate_peripheral_ranges) {
      shim_.set_mmio_observer([](boot_shim::DevicetreeMmioRange mmio_range) {
        auto& pool = Allocation::GetPool();
        memalloc::Range peripheral_range = {
            .addr = mmio_range.address,
            .size = mmio_range.size,
            .type = memalloc::Type::kPeripheral,
        };
        // This may reintroduce reserved ranges from the initial memory bootstrap as peripheral
        // ranges, since reserved ranges are no longer tracked and are represented as wholes in the
        // memory. This should be harmless, since the implications is that an uncached mapping will
        // be created but not touched.
        if (pool.MarkAsPeripheral(peripheral_range).is_error()) {
          printf("Failed to mark [%#" PRIx64 ", %#" PRIx64 "] as peripheral.\n",
                 peripheral_range.addr, peripheral_range.end());
        }
      });

      GetUartDriver().Visit([this]<typename KernelDriver>(const KernelDriver& driver) {
        // Mark the UART MMIO range as peripheral range.
        if (auto uart_mmio = GetUartMmioRange(driver, ZX_PAGE_SIZE)) {
          if (Allocation::GetPool().MarkAsPeripheral(*uart_mmio).is_error()) {
            printf("%s: Failed to mark [%#" PRIx64 ", %#" PRIx64 "] as peripheral.\n",
                   shim_.shim_name(), uart_mmio->addr, uart_mmio->end());
          }
        }
      });
    }

    // This will initialize these common set of items if they are present, otherwise they will
    // collapse to no-ops and be compiled out.
    shim_.OnInvocableItems(
        [](boot_shim::DevicetreeDtbItem& item) {
          item.set_payload({reinterpret_cast<const ktl::byte*>(gDevicetreeBoot.fdt.fdt().data()),
                            gDevicetreeBoot.fdt.size_bytes()});
        },
        [](boot_shim::PoolMemConfigItem& item) { item.Init(Allocation::GetPool()); },
        [](boot_shim::NvramItem& item) {
          if (gDevicetreeBoot.nvram) {
            item.set_payload(*gDevicetreeBoot.nvram);
          }
        },
        [this](boot_shim::RebootReasonItem& item) {
          item.Init(gDevicetreeBoot.cmdline, shim_.shim_name());
        },
        [](boot_shim::UartItem<>& item) { item.Init(GetUartDriver().config()); });

    return shim_.Init();
  }

  [[noreturn]] void Boot() {
    // Finally we can boot into the kernel image.
    BootZbi::InputZbi zbi_view(gDevicetreeBoot.ramdisk);
    BootZbi boot;

    if (shim_.Check("Not a bootable ZBI", boot.Init(zbi_view))) {
      BootPostInit();
      if (shim_.Check("Failed to load ZBI", boot.Load(static_cast<uint32_t>(shim_.size_bytes()))) &&
          shim_.Check("Failed to append boot loader items to data ZBI",
                      shim_.AppendItems(boot.DataZbi()))) {
        boot.Log();
        boot.Boot();
      }
    }
    printf("Boot failure. Aborting...\n");
    abort();
  }

  Shim& shim() { return shim_; }

 private:
  void BootPostInit() {
    if constexpr (options.generate_peripheral_ranges) {
      // All MMIO Ranges have been observed by now.
      if (Allocation::GetPool().CoalescePeripherals(kAddressSpacePageSizeShifts).is_error()) {
        printf(
            "%s: WARNING Failed to inflate peripheral ranges, page allocation may be suboptimal.\n",
            shim_.shim_name());
      }
    }
  }

  MaybeAddressSpace<options.enable_mmu> maybe_aspace_;
  MainSymbolize symbolize_;
  Shim shim_;
};

#endif  // ZIRCON_KERNEL_PHYS_BOOT_SHIM_INCLUDE_PHYS_BOOT_SHIM_DEVICETREE_H_
