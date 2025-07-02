// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adc-buttons.h"

#include <fidl/fuchsia.hardware.platform.device/cpp/driver/fidl.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/platform-device/cpp/pdev.h>
#include <lib/mmio/mmio.h>

namespace adc_buttons {

namespace {

constexpr uint32_t kDefaultPollingRateUsec = 1'000;

struct MetadataValues {
  uint32_t polling_rate_usec = kDefaultPollingRateUsec;
  std::map<uint32_t, std::vector<fuchsia_buttons::Button>> configs;
  std::set<fuchsia_input_report::ConsumerControlButton> buttons;
};

zx::result<MetadataValues> ParseMetadata(const fuchsia_buttons::Metadata& metadata) {
  if (!metadata.polling_rate_usec().has_value()) {
    FDF_LOG(ERROR, "Metadata missing `polling_rate_usec` field");
    return zx::error(ZX_ERR_INTERNAL);
  }

  if (!metadata.buttons().has_value()) {
    FDF_LOG(ERROR, "Metadata missing `buttons` field");
    return zx::error(ZX_ERR_INTERNAL);
  }

  MetadataValues ret{.polling_rate_usec = metadata.polling_rate_usec().value()};

  const auto& buttons = metadata.buttons().value();
  for (size_t i = 0; i < buttons.size(); ++i) {
    const auto& button = buttons[i];
    if (button.button_config()->Which() != fuchsia_buttons::ButtonConfig::Tag::kAdc) {
      FDF_LOG(WARNING, "Ignoring button %lu: Button config not of type adc", i);
      continue;
    }
    ret.buttons.insert(button.types()->begin(), button.types()->end());
    ret.configs[*button.button_config()->adc()->channel_idx()].emplace_back(std::move(button));
  }
  if (ret.buttons.size() >= fuchsia_input_report::kConsumerControlMaxNumButtons) {
    FDF_LOG(
        ERROR,
        "%s: More buttons than expected (max = %d). Please increase kConsumerControlMaxNumButtons",
        __func__, fuchsia_input_report::kConsumerControlMaxNumButtons);
    return zx::error(ZX_ERR_INTERNAL);
  }

  if (ret.configs.empty() || ret.buttons.empty()) {
    FDF_LOG(ERROR, "%s: failed to get button configs in metadata.", __func__);
    return zx::error(ZX_ERR_INTERNAL);
  }

  return zx::ok(std::move(ret));
}

}  // namespace

zx::result<> AdcButtons::Start() {
  zx::result pdev_client_end =
      incoming()->Connect<fuchsia_hardware_platform_device::Service::Device>("pdev");
  if (pdev_client_end.is_error()) {
    FDF_LOG(ERROR, "Failed to connect to platform device: %s", pdev_client_end.status_string());
    return pdev_client_end.take_error();
  }
  fdf::PDev pdev{std::move(pdev_client_end.value())};

  // Get metadata.
  zx::result metadata_result = pdev.GetFidlMetadata<fuchsia_buttons::Metadata>();
  if (metadata_result.is_error()) {
    FDF_LOG(ERROR, "Failed to get metadata: %s", metadata_result.status_string());
    return metadata_result.take_error();
  }
  zx::result values = ParseMetadata(metadata_result.value());
  if (values.is_error()) {
    FDF_LOG(ERROR, "Failed to parse metadata: %s", values.status_string());
    return values.take_error();
  }

  // Get adc.
  std::vector<adc_buttons_device::AdcButtonsDevice::AdcButtonClient> clients;
  for (auto& [idx, buttons] : values.value().configs) {
    char adc_name[32];
    sprintf(adc_name, "adc-%u", idx);
    zx::result result = incoming()->Connect<fuchsia_hardware_adc::Service::Device>(adc_name);
    if (result.is_error()) {
      FDF_LOG(ERROR, "Failed to open adc service: %s", result.status_string());
      return result.take_error();
    }
    clients.emplace_back(std::move(result.value()), std::move(buttons));
  }

  device_ = std::make_unique<adc_buttons_device::AdcButtonsDevice>(
      dispatcher(), std::move(clients), values.value().polling_rate_usec,
      std::move(values.value().buttons));

  auto result = outgoing()->component().AddUnmanagedProtocol<fuchsia_input_report::InputDevice>(
      input_report_bindings_.CreateHandler(device_.get(), dispatcher(),
                                           fidl::kIgnoreBindingClosure),
      kDeviceName);
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to add Device service %s", result.status_string());
    return result.take_error();
  }

  if (zx::result result = CreateDevfsNode(); result.is_error()) {
    FDF_LOG(ERROR, "Failed to export to devfs %s", result.status_string());
    return result.take_error();
  }

  return zx::ok();
}

void AdcButtons::Stop() { device_->Shutdown(); }

zx::result<> AdcButtons::CreateDevfsNode() {
  zx::result connector = devfs_connector_.Bind(dispatcher());
  if (connector.is_error()) {
    return connector.take_error();
  }

  fuchsia_driver_framework::DevfsAddArgs devfs_args{
      {.connector = std::move(connector.value()), .class_name = "input-report"}};

  zx::result child = AddOwnedChild(kDeviceName, devfs_args);
  if (child.is_error()) {
    FDF_LOG(ERROR, "Failed to add child: %s", child.status_string());
    return child.take_error();
  }
  child_ = std::move(child.value());

  return zx::ok();
}

}  // namespace adc_buttons

FUCHSIA_DRIVER_EXPORT(adc_buttons::AdcButtons);
