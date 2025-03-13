// Copyright 2025 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/misc/drivers/virtio-pmem/pmem.h"

#include <fidl/fuchsia.kernel/cpp/wire.h>
#include <lib/virtio/driver_utils.h>
#include <limits.h>
#include <zircon/status.h>

#include <memory>
#include <utility>

#include <fbl/auto_lock.h>

#include "src/devices/misc/drivers/virtio-pmem/virtio/pmem.h"

namespace virtio {

PmemDevice::PmemDevice(zx::bti bti, std::unique_ptr<Backend> backend, zx::resource mmio_resource)
    : virtio::Device(std::move(bti), std::move(backend)),
      request_virtio_queue_(this),
      mmio_resource_(std::move(mmio_resource)) {}

PmemDevice::~PmemDevice() {}

zx_status_t PmemDevice::Init() {
  FDF_LOG(DEBUG, "initialization starting");
  // reset the device
  DeviceReset();

  // ack and set the driver status bit
  DriverStatusAck();

  // Note: We don't support VIRTIO_PMEM_F_SHMEM_REGION
  if (DeviceFeaturesSupported() & VIRTIO_F_VERSION_1) {
    DriverFeaturesAck(VIRTIO_F_VERSION_1);
    if (zx_status_t status = DeviceStatusFeaturesOk(); status != ZX_OK) {
      FDF_LOG(ERROR, "Feature negotiation failed: %s", zx_status_get_string(status));
      return status;
    }
  }

  // Read device configuration space.
  virtio_pmem_config config{};
  ReadDeviceConfig(offsetof(virtio_pmem_config, start), &config.start);
  ReadDeviceConfig(offsetof(virtio_pmem_config, size), &config.size);
  FDF_LOG(DEBUG, "config address: %#lx length %#lx", config.start, config.size);

  zx_status_t status =
      zx::vmo::create_physical(mmio_resource_, config.start, config.size, &phys_vmo_);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "failed to create VMO: %s", zx_status_get_string(status));
    return status;
  }

  // Initialize request virtqueue.
  status = request_virtio_queue_.Init(0);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "failed to initialize req_vq : %s", zx_status_get_string(status));
    return status;
  }

  // set DRIVER_OK
  DriverStatusOk();

  FDF_LOG(DEBUG, "initialization succeeded");

  return ZX_OK;
}

void PmemDevice::IrqRingUpdate() { FDF_LOG(DEBUG, "%s: Got irq ring update, ignoring", tag()); }

void PmemDevice::IrqConfigChange() { FDF_LOG(DEBUG, "%s: Got irq config change, ignoring", tag()); }

zx::result<zx::vmo> PmemDevice::clone_vmo() {
  zx::vmo vmo;
  zx_status_t status = phys_vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(vmo));
}

PmemDriver::PmemDriver(fdf::DriverStartArgs start_args,
                       fdf::UnownedSynchronizedDispatcher dispatcher)
    : fdf::DriverBase(kDriverName, std::move(start_args), std::move(dispatcher)) {}

zx::result<> PmemDriver::Start() {
  zx::result device = CreatePmemDevice();

  if (device.is_error()) {
    return device.take_error();
  }

  device_ = std::move(*device);

  zx_status_t status = device_->Init();
  if (status != ZX_OK) {
    return zx::error(status);
  }

  // Advertise service.
  fuchsia_hardware_virtio_pmem::Service::InstanceHandler handler({
      .device = bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                        fidl::kIgnoreBindingClosure),
  });
  zx::result add_result =
      outgoing()->AddService<fuchsia_hardware_virtio_pmem::Service>(std::move(handler));
  if (add_result.is_error()) {
    FDF_LOG(ERROR, "Unable to add service: %s", add_result.status_string());
    return add_result.take_error();
  }

  return zx::ok();
}

void PmemDriver::Get(GetCompleter::Sync& completer) {
  if (device_) {
    zx::result cloned_vmo = device_->clone_vmo();
    completer.Reply({std::move(cloned_vmo)});
  } else {
    FDF_LOG(WARNING, "Get called with uninitialized device.");
    completer.Close(ZX_ERR_BAD_STATE);
  }
}

void PmemDriver::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_hardware_virtio_pmem::Device> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  FDF_LOG(WARNING, "Unknown FIDL method received ordinal %lu, closing channel",
          metadata.method_ordinal);
  completer.Close(ZX_ERR_NOT_SUPPORTED);
}

zx::result<std::unique_ptr<PmemDevice>> PmemDriver::CreatePmemDevice() {
  zx::result pci_client_result = incoming()->Connect<fuchsia_hardware_pci::Service::Device>();
  if (pci_client_result.is_error()) {
    FDF_LOG(ERROR, "Failed to get pci client: %s", pci_client_result.status_string());
    return pci_client_result.take_error();
  }

  zx::result mmio_result = incoming()->Connect<fuchsia_kernel::MmioResource>();
  if (mmio_result.is_error()) {
    FDF_LOG(ERROR, "Failed to connect to MmioResource: %s", mmio_result.status_string());
    return mmio_result.take_error();
  }
  fidl::WireResult mmio_resource = fidl::WireCall(*mmio_result)->Get();
  if (!mmio_resource.ok()) {
    FDF_LOG(ERROR, "Failed to get mmio resource: %s", mmio_resource.status_string());
    return zx::error(mmio_resource.status());
  }

  zx::result bti_and_backend_result =
      virtio::GetBtiAndBackend(std::move(pci_client_result).value());
  if (!bti_and_backend_result.is_ok()) {
    FDF_LOG(ERROR, "GetBtiAndBackend failed: %s", bti_and_backend_result.status_string());
    return bti_and_backend_result.take_error();
  }
  auto [bti, backend] = std::move(bti_and_backend_result).value();

  return zx::ok(std::make_unique<PmemDevice>(std::move(bti), std::move(backend),
                                             std::move(mmio_resource->resource)));
}

}  // namespace virtio
