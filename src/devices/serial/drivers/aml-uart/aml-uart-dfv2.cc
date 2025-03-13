// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/serial/drivers/aml-uart/aml-uart-dfv2.h"

#include <fidl/fuchsia.hardware.power/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/structured_logger.h>
#include <lib/driver/platform-device/cpp/pdev.h>
#include <lib/driver/power/cpp/element-description-builder.h>
#include <lib/driver/power/cpp/power-support.h>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/serial/cpp/bind.h>

namespace serial {

namespace {

constexpr std::string_view kPdevName = "pdev";
constexpr std::string_view kChildName = "aml-uart";
constexpr std::string_view kDriverName = "aml-uart";

}  // namespace

AmlUartV2::AmlUartV2(fdf::DriverStartArgs start_args,
                     fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : fdf::DriverBase(kDriverName, std::move(start_args), std::move(driver_dispatcher)),
      driver_config_(take_config<aml_uart_config::Config>()) {}

void AmlUartV2::Start(fdf::StartCompleter completer) {
  start_completer_.emplace(std::move(completer));

  parent_node_client_.Bind(std::move(node()), dispatcher());

  device_server_.Begin(
      incoming(), outgoing(), node_name(), kChildName,
      fit::bind_member<&AmlUartV2::OnDeviceServerInitialized>(this),
      // TODO(b/373918767): Don't forward DEVICE_METADATA_MAC_ADDRESS once no longer retrieved.
      compat::ForwardMetadata::Some({DEVICE_METADATA_MAC_ADDRESS}));
}

void AmlUartV2::PrepareStop(fdf::PrepareStopCompleter completer) {
  if (aml_uart_.has_value()) {
    aml_uart_->Enable(false);
  }

  completer(zx::ok());
}

AmlUart& AmlUartV2::aml_uart_for_testing() {
  ZX_ASSERT(aml_uart_.has_value());
  return aml_uart_.value();
}

void AmlUartV2::OnDeviceServerInitialized(zx::result<> device_server_init_result) {
  if (device_server_init_result.is_error()) {
    CompleteStart(device_server_init_result.take_error());
    return;
  }

  auto pdev_client_end =
      incoming()->Connect<fuchsia_hardware_platform_device::Service::Device>(kPdevName);
  if (pdev_client_end.is_error()) {
    FDF_LOG(ERROR, "Failed to connect to platform device: %s", pdev_client_end.status_string());
    CompleteStart(pdev_client_end.take_error());
    return;
  }

  fdf::PDev pdev{std::move(pdev_client_end.value())};

  if (zx::result result = mac_address_metadata_server_.SetMetadataFromPDevIfExists(pdev);
      result.is_error()) {
    FDF_LOG(ERROR, "Failed to set mac address metadata from platform device: %s",
            result.status_string());
    CompleteStart(result.take_error());
    return;
  }
  if (zx::result result = mac_address_metadata_server_.Serve(*outgoing(), dispatcher());
      result.is_error()) {
    FDF_LOG(ERROR, "Failed to serve mac address metadata: %s", result.status_string());
    CompleteStart(result.take_error());
    return;
  }

  zx::result metadata = pdev.GetFidlMetadata<fuchsia_hardware_serial::SerialPortInfo>(
      fuchsia_hardware_serial::SerialPortInfo::kSerializableName);
  if (metadata.is_error()) {
    if (metadata.status_value() == ZX_ERR_NOT_FOUND) {
      FDF_LOG(DEBUG, "Serial port info metadata not found.");
    } else {
      FDF_LOG(ERROR, "Failed to get metadata: %s", metadata.status_string());
      CompleteStart(metadata.take_error());
      return;
    }
  } else {
    serial_port_info_ = {
        .serial_class = metadata->serial_class(),
        .serial_vid = metadata->serial_vid(),
        .serial_pid = metadata->serial_pid(),
    };
  }

  zx::result mmio = pdev.MapMmio(0);
  if (mmio.is_error()) {
    FDF_SLOG(ERROR, "Failed to map mmio.", KV("status", mmio.status_string()));
    CompleteStart(mmio.take_error());
    return;
  }

  fidl::ClientEnd<fuchsia_power_system::ActivityGovernor> sag;
  if (driver_config_.enable_suspend()) {
    zx::result result = incoming()->Connect<fuchsia_power_system::ActivityGovernor>();
    if (result.is_error() || !result->is_valid()) {
      FDF_LOG(WARNING, "Failed to connect to activity governor: %s", result.status_string());
      CompleteStart(result.take_error());
    }
    sag = std::move(result.value());
  }

  aml_uart_.emplace(std::move(pdev), serial_port_info_, std::move(mmio.value()),
                    driver_config_.enable_suspend(), std::move(sag));

  // Default configuration for the case that serial_impl_config is not called.
  constexpr uint32_t kDefaultBaudRate = 115200;
  constexpr uint32_t kDefaultConfig = fuchsia_hardware_serialimpl::kSerialDataBits8 |
                                      fuchsia_hardware_serialimpl::kSerialStopBits1 |
                                      fuchsia_hardware_serialimpl::kSerialParityNone;
  aml_uart_->Config(kDefaultBaudRate, kDefaultConfig);

  zx::result node_controller_endpoints =
      fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  if (node_controller_endpoints.is_error()) {
    FDF_LOG(ERROR, "Failed to create NodeController endpoints %s",
            node_controller_endpoints.status_string());
    CompleteStart(node_controller_endpoints.take_error());
    return;
  }

  fuchsia_hardware_serialimpl::Service::InstanceHandler handler({
      .device =
          [this](fdf::ServerEnd<fuchsia_hardware_serialimpl::Device> server_end) {
            serial_impl_bindings_.AddBinding(driver_dispatcher()->get(), std::move(server_end),
                                             &aml_uart_.value(), fidl::kIgnoreBindingClosure);
          },
  });
  zx::result<> add_result =
      outgoing()->AddService<fuchsia_hardware_serialimpl::Service>(std::move(handler), kChildName);
  if (add_result.is_error()) {
    FDF_LOG(ERROR, "Failed to add fuchsia_hardware_serialimpl::Service %s",
            add_result.status_string());
    CompleteStart(add_result.take_error());
    return;
  }

  auto offers = device_server_.CreateOffers2();
  offers.push_back(fdf::MakeOffer2<fuchsia_hardware_serialimpl::Service>(kChildName));
  offers.push_back(mac_address_metadata_server_.MakeOffer());

  fuchsia_driver_framework::NodeAddArgs args{
      {
          .name = std::string(kChildName),
          .properties = {{
              fdf::MakeProperty(bind_fuchsia::SERIAL_CLASS,
                                static_cast<uint32_t>(aml_uart_->serial_port_info().serial_class)),
          }},
          .offers2 = std::move(offers),
      },
  };

  fidl::Arena arena;
  parent_node_client_
      ->AddChild(fidl::ToWire(arena, std::move(args)), std::move(node_controller_endpoints->server),
                 {})
      .Then(fit::bind_member<&AmlUartV2::OnAddChildResult>(this));
}

void AmlUartV2::OnAddChildResult(
    fidl::WireUnownedResult<fuchsia_driver_framework::Node::AddChild>& add_child_result) {
  if (!add_child_result.ok()) {
    FDF_LOG(ERROR, "Failed to add child %s", add_child_result.status_string());
    CompleteStart(zx::error(add_child_result.status()));
    return;
  }

  if (add_child_result.value().is_error()) {
    FDF_LOG(ERROR, "Failed to add child. NodeError: %d",
            static_cast<uint32_t>(add_child_result.value().error_value()));
    CompleteStart(zx::error(ZX_ERR_INTERNAL));
    return;
  }

  FDF_LOG(INFO, "Successfully started aml-uart-dfv2 driver.");
  CompleteStart(zx::ok());
}

void AmlUartV2::CompleteStart(zx::result<> result) {
  ZX_ASSERT(start_completer_.has_value());
  start_completer_.value()(result);
  start_completer_.reset();
}

}  // namespace serial

FUCHSIA_DRIVER_EXPORT(serial::AmlUartV2);
