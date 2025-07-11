// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adc-buttons.h"

#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <gtest/gtest.h>

namespace adc_buttons_device {

constexpr uint32_t kChannel = 2;
constexpr uint32_t kReleaseThreshold = 30;
constexpr uint32_t kPressThreshold = 10;
constexpr uint32_t kPollingRateUsec = 1'000;

class FakeAdcServer : public fidl::Server<fuchsia_hardware_adc::Device> {
 public:
  void set_resolution(uint8_t resolution) { resolution_ = resolution; }
  void set_sample(uint32_t sample) { sample_ = sample; }
  void set_normalized_sample(float normalized_sample) { normalized_sample_ = normalized_sample; }

  void GetResolution(GetResolutionCompleter::Sync& completer) override {
    completer.Reply(fit::ok(resolution_));
  }
  void GetSample(GetSampleCompleter::Sync& completer) override {
    completer.Reply(fit::ok(sample_));
  }
  void GetNormalizedSample(GetNormalizedSampleCompleter::Sync& completer) override {
    completer.Reply(fit::ok(normalized_sample_));
  }

  fuchsia_hardware_adc::Service::InstanceHandler GetInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_adc::Service::InstanceHandler({
        .device = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

 private:
  uint8_t resolution_ = 0;
  uint32_t sample_ = 0;
  float normalized_sample_ = 0;

  fidl::ServerBindingGroup<fuchsia_hardware_adc::Device> bindings_;
};

class TestEnv : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    auto* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    // Serve metadata.
    std::vector<fuchsia_input_report::ConsumerControlButton> func_types = {
        fuchsia_input_report::ConsumerControlButton::kFunction};
    fuchsia_buttons::AdcButtonConfig func_adc_config{{
        .channel_idx = kChannel,
        .release_threshold = kReleaseThreshold,
        .press_threshold = kPressThreshold,
    }};
    auto func_config = fuchsia_buttons::ButtonConfig::WithAdc(std::move(func_adc_config));
    std::vector<fuchsia_buttons::Button> buttons = {
        {{.types = std::move(func_types), .button_config = std::move(func_config)}}};

    fuchsia_buttons::Metadata metadata{
        {.polling_rate_usec = kPollingRateUsec, .buttons = std::move(buttons)}};
    zx_status_t status =
        pdev_.AddFidlMetadata(fuchsia_buttons::Metadata::kSerializableName, metadata);
    if (status != ZX_OK) {
      return zx::error(status);
    }

    {
      zx::result result = to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
          pdev_.GetInstanceHandler(dispatcher), "pdev");
      if (result.is_error()) {
        return result.take_error();
      }
    }

    {
      zx::result result = to_driver_vfs.AddService<fuchsia_hardware_adc::Service>(
          fake_adc_server_.GetInstanceHandler(dispatcher), "adc-2");
      if (result.is_error()) {
        return result.take_error();
      }
    }

    return zx::ok();
  }

  void FakeAdcSetSample(uint32_t sample) { fake_adc_server_.set_sample(sample); }

 private:
  FakeAdcServer fake_adc_server_;
  fdf_fake::FakePDev pdev_;
};

class TestConfig final {
 public:
  using DriverType = adc_buttons::AdcButtons;
  using EnvironmentType = TestEnv;
};

class AdcButtonsDeviceTest : public ::testing::Test {
 public:
  void TearDown() override {
    zx::result<> result = driver_test().StopDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }

  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
    // Connect to InputDevice.
    auto connect_result =
        driver_test().RunInNodeContext<zx::result<zx::channel>>([](fdf_testing::TestNode& node) {
          return node.children().at("adc-buttons").ConnectToDevice();
        });
    EXPECT_EQ(ZX_OK, connect_result.status_value());
    client_.Bind(
        fidl::ClientEnd<fuchsia_input_report::InputDevice>(std::move(connect_result.value())));
  }

  void FakeAdcSetSample(uint32_t sample) {
    driver_test().RunInEnvironmentTypeContext(
        [sample](TestEnv& env) { env.FakeAdcSetSample(sample); });
  }

  void DrainInitialReport(fidl::WireSyncClient<fuchsia_input_report::InputReportsReader>& reader) {
    auto result = reader->ReadInputReports();
    EXPECT_EQ(ZX_OK, result.status());
    ASSERT_FALSE(result.value().is_error());
    auto& reports = result.value().value()->reports;

    ASSERT_EQ(1, reports.size());
    auto report = reports[0];

    ASSERT_TRUE(report.has_event_time());
    ASSERT_TRUE(report.has_consumer_control());
    auto& consumer_control = report.consumer_control();

    ASSERT_TRUE(consumer_control.has_pressed_buttons());
  }

  fidl::WireSyncClient<fuchsia_input_report::InputDevice>& client() { return client_; }

  fdf_testing::BackgroundDriverTest<TestConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test_;
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client_;
};

TEST_F(AdcButtonsDeviceTest, GetDescriptorTest) {
  auto result = client()->GetDescriptor();
  ASSERT_TRUE(result.ok());

  EXPECT_FALSE(result->descriptor.has_keyboard());
  EXPECT_FALSE(result->descriptor.has_mouse());
  EXPECT_FALSE(result->descriptor.has_sensor());
  EXPECT_FALSE(result->descriptor.has_touch());

  ASSERT_TRUE(result->descriptor.has_device_information());
  EXPECT_EQ(result->descriptor.device_information().vendor_id(),
            static_cast<uint32_t>(fuchsia_input_report::wire::VendorId::kGoogle));
  EXPECT_EQ(result->descriptor.device_information().product_id(),
            static_cast<uint32_t>(fuchsia_input_report::wire::VendorGoogleProductId::kAdcButtons));
  EXPECT_EQ(result->descriptor.device_information().polling_rate(), kPollingRateUsec);

  ASSERT_TRUE(result->descriptor.has_consumer_control());
  ASSERT_TRUE(result->descriptor.consumer_control().has_input());
  ASSERT_TRUE(result->descriptor.consumer_control().input().has_buttons());
  EXPECT_EQ(result->descriptor.consumer_control().input().buttons().size(), 1);
  EXPECT_EQ(result->descriptor.consumer_control().input().buttons()[0],
            fuchsia_input_report::wire::ConsumerControlButton::kFunction);
}

TEST_F(AdcButtonsDeviceTest, ReadInputReportsTest) {
  auto endpoints = fidl::Endpoints<fuchsia_input_report::InputReportsReader>::Create();
  auto result = client()->GetInputReportsReader(std::move(endpoints.server));
  ASSERT_TRUE(result.ok());
  // Ensure that the reader has been registered with the client before moving on.
  ASSERT_TRUE(client()->GetDescriptor().ok());
  auto reader =
      fidl::WireSyncClient<fuchsia_input_report::InputReportsReader>(std::move(endpoints.client));
  EXPECT_TRUE(reader.is_valid());
  DrainInitialReport(reader);

  FakeAdcSetSample(20);
  // Wait for the device to pick this up.
  usleep(2 * kPollingRateUsec);

  {
    auto result = reader->ReadInputReports();
    EXPECT_EQ(ZX_OK, result.status());
    ASSERT_FALSE(result.value().is_error());
    auto& reports = result.value().value()->reports;

    ASSERT_EQ(1, reports.size());
    auto report = reports[0];

    ASSERT_TRUE(report.has_event_time());
    ASSERT_TRUE(report.has_consumer_control());
    auto& consumer_control = report.consumer_control();

    ASSERT_TRUE(consumer_control.has_pressed_buttons());
    EXPECT_EQ(consumer_control.pressed_buttons().size(), 1);
    EXPECT_EQ(consumer_control.pressed_buttons()[0],
              fuchsia_input_report::wire::ConsumerControlButton::kFunction);
  };

  FakeAdcSetSample(40);
  // Wait for the device to pick this up.
  usleep(2 * kPollingRateUsec);

  {
    auto result = reader->ReadInputReports();
    EXPECT_EQ(ZX_OK, result.status());
    ASSERT_FALSE(result.value().is_error());
    auto& reports = result.value().value()->reports;

    ASSERT_EQ(1, reports.size());
    auto report = reports[0];

    ASSERT_TRUE(report.has_event_time());
    ASSERT_TRUE(report.has_consumer_control());
    auto& consumer_control = report.consumer_control();

    ASSERT_TRUE(consumer_control.has_pressed_buttons());
    EXPECT_EQ(consumer_control.pressed_buttons().size(), 0);
  };
}

}  // namespace adc_buttons_device
