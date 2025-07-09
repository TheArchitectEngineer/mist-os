// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_PCI_SDHCI_PCI_SDHCI_H_
#define SRC_DEVICES_BLOCK_DRIVERS_PCI_SDHCI_PCI_SDHCI_H_

#include <fidl/fuchsia.hardware.sdhci/cpp/driver/fidl.h>
#include <lib/device-protocol/pci.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>

#include <optional>

#include <ddktl/device.h>

namespace sdhci {

class PciSdhci;
using DeviceType = ddk::Device<PciSdhci>;

class PciSdhci final : public DeviceType, public fdf::WireServer<fuchsia_hardware_sdhci::Device> {
 public:
  explicit PciSdhci(zx_device_t*);

  static zx_status_t Bind(void*, zx_device_t* parent);

  zx_status_t Init();

  // fuchsia.hardware.sdhci/Device protocol implementation
  void GetInterrupt(fdf::Arena& arena, GetInterruptCompleter::Sync& completer) override;
  void GetMmio(fdf::Arena& arena, GetMmioCompleter::Sync& completer) override;
  void GetBti(GetBtiRequestView request, fdf::Arena& arena,
              GetBtiCompleter::Sync& completer) override;
  void GetBaseClock(fdf::Arena& arena, GetBaseClockCompleter::Sync& completer) override;
  void GetQuirks(fdf::Arena& arena, GetQuirksCompleter::Sync& completer) override;
  void HwReset(fdf::Arena& arena, HwResetCompleter::Sync& completer) override;
  void VendorSetBusClock(VendorSetBusClockRequestView request, fdf::Arena& arena,
                         VendorSetBusClockCompleter::Sync& completer) override;
  void VendorPerformTuning(VendorPerformTuningRequestView request, fdf::Arena& arena,
                           VendorPerformTuningCompleter::Sync& completer) override;

  void DdkUnbind(ddk::UnbindTxn txn);
  void DdkRelease();

 private:
  ddk::Pci pci_;

  std::optional<fdf::MmioBuffer> mmio_;
  zx::bti bti_;
  fdf::ServerBindingGroup<fuchsia_hardware_sdhci::Device> bindings_;
  fdf::OutgoingDirectory outgoing_{
      fdf::OutgoingDirectory::Create(fdf::Dispatcher::GetCurrent()->get())};
};

}  // namespace sdhci

#endif  // SRC_DEVICES_BLOCK_DRIVERS_PCI_SDHCI_PCI_SDHCI_H_
