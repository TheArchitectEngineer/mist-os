// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/cpp/task.h>
#include <lib/driver/devicetree/visitors/drivers/pci/pci.h>
#include <lib/fdf/cpp/arena.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <stdint.h>
#include <zircon/rights.h>
#include <zircon/status.h>

#include "crosvm.h"

namespace board_crosvm {

zx::result<> Pciroot::CreateInterruptsAndRouting(
    std::span<const pci_dt::Gicv3InterruptMapElement> interrupts) {
  // This is only used for allocating the interrupt objects mapped to PCI.
  for (const auto& entry : interrupts) {
    FDF_LOG(DEBUG, "%02X.%02X.%02x: pin %u int %#x %s %s", entry.child_unit_address.bus(),
            entry.child_unit_address.device(), entry.child_unit_address.function(), entry.pin,
            entry.parent.int_number, pci_dt::Gicv3InterruptTypeLabel(entry.parent.type),
            pci_dt::Gicv3InterruptFlagsLabel(entry.parent.flags));
    ZX_DEBUG_ASSERT_MSG(entry.parent.flags == pci_dt::Gicv3InterruptFlags::LevelTriggered,
                        "Expected interrupt-map to contain level triggered interrupts");
    uint32_t vector = entry.parent.int_number;
    if (!std::any_of(interrupts_.begin(), interrupts_.end(),
                     [vector](const auto& irq) { return irq.vector == vector; })) {
      zx::interrupt interrupt;
      zx_status_t status = zx::interrupt::create(/*resource=*/irq_resource_,
                                                 /*vector=*/vector,
                                                 /*options=*/0,
                                                 /*result=*/&interrupt);
      if (status != ZX_OK) {
        FDF_LOG(ERROR, "Couldn't create interrupt object for vector %#x (handle: %#x): %s", vector,
                irq_resource_.get(), zx_status_get_string(status));
        return zx::error(status);
      }
      interrupts_.push_back(
          pci_legacy_irq_t{.interrupt = interrupt.release(), .vector = entry.parent.int_number});
    }

    pci_irq_routing_entry_t routing_entry{
        .port_device_id = PCI_IRQ_ROUTING_NO_PARENT,
        .port_function_id = PCI_IRQ_ROUTING_NO_PARENT,
        .device_id = static_cast<uint8_t>(entry.child_unit_address.device())};
    // Pins in devicetree are indexed from 1.
    routing_entry.pins[entry.pin - 1] = static_cast<uint8_t>(entry.parent.int_number);
    irq_routing_entries_.push_back(routing_entry);
  }

  return zx::ok();
}

zx_status_t Pciroot::PcirootGetBti(uint32_t bdf, uint32_t index, zx::bti* bti) {
  zx_status_t status = ZX_ERR_INTERNAL;
  libsync::Completion completion;
  async::PostTask(dispatcher_, [this, &status, &completion, &bti, bdf, index]() {
    auto complete = fit::defer([&completion]() { completion.Signal(); });
    fdf::Arena arena('PCIR');
    fdf::WireUnownedResult result =
        fdf::WireCall(iommu_).buffer(arena)->GetBti(/*iommu_index=*/index, /*bti_id=*/bdf);
    if (!result.ok()) {
      FDF_LOG(ERROR, "GetBti failed, transport error: %s",
              result.error().FormatDescription().c_str());
      status = result.error().status();
      return;
    }

    if (result->is_error()) {
      FDF_LOG(ERROR, "GetBti failed, method error: %s", result.status_string());
      status = result->error_value();
      return;
    }

    *bti = std::move(result->value()->bti);
    status = ZX_OK;
  });
  completion.Wait();
  return status;
}

zx_status_t Pciroot::PcirootGetPciPlatformInfo(pci_platform_info_t* info) {
  info->start_bus_num = 0;
  info->end_bus_num = 0;
  info->segment_group = 0;
  info->legacy_irqs_list = interrupts_.data();
  info->legacy_irqs_count = interrupts_.size();
  info->irq_routing_list = irq_routing_entries_.data();
  info->irq_routing_count = irq_routing_entries_.size();
  info->acpi_bdfs_count = 0;
  node_name_.copy(info->name, node_name_.size(), 0);

  zx::vmo cam{};
  if (zx_status_t status = cam_.duplicate(ZX_RIGHT_SAME_RIGHTS, &cam); status != ZX_OK) {
    FDF_LOG(WARNING, "couldn't duplicate ecam handle: %s", zx_status_get_string(status));
  }
  info->cam = {.vmo = cam.release(), .is_extended = is_extended_};
  return ZX_OK;
}

}  // namespace board_crosvm
