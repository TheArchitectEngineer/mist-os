// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/display_coordinator_listener.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/ui/scenic/lib/display/tests/mock_display_coordinator.h"

namespace scenic_impl::display::test {

namespace {

class DisplayCoordinatorListenerTest : public gtest::TestLoopFixture {
 public:
  void SetUp() override {
    auto [coordinator_client, coordinator_server] =
        fidl::Endpoints<fuchsia_hardware_display::Coordinator>::Create();
    auto [listener_client, listener_server] =
        fidl::Endpoints<fuchsia_hardware_display::CoordinatorListener>::Create();

    mock_display_coordinator_ =
        std::make_unique<MockDisplayCoordinator>(fuchsia_hardware_display::wire::Info{});
    mock_display_coordinator_->Bind(std::move(coordinator_server), std::move(listener_client));

    listener_server_end_ = std::move(listener_server);
  }

  DisplayCoordinatorListener* display_coordinator_listener() {
    return display_coordinator_listener_.get();
  }

  void ResetMockDisplayCoordinator() { mock_display_coordinator_.reset(); }
  void ResetDisplayCoordinatorListener() { display_coordinator_listener_.reset(); }

  MockDisplayCoordinator* mock_display_coordinator() { return mock_display_coordinator_.get(); }

  // Must be called no more than once per test case.
  fidl::ServerEnd<fuchsia_hardware_display::CoordinatorListener> TakeListenerServerEnd() {
    FX_DCHECK(listener_server_end_.is_valid());
    return std::move(listener_server_end_);
  }

 private:
  std::unique_ptr<MockDisplayCoordinator> mock_display_coordinator_;
  std::unique_ptr<DisplayCoordinatorListener> display_coordinator_listener_;

  fidl::ServerEnd<fuchsia_hardware_display::CoordinatorListener> listener_server_end_;
};

using DisplayCoordinatorListenerBasicTest = gtest::TestLoopFixture;

// Verify the documented constructor behavior doesn't cause any crash.
TEST_F(DisplayCoordinatorListenerBasicTest, ConstructorArgs) {
  auto [listener_client, listener_server] =
      fidl::Endpoints<fuchsia_hardware_display::CoordinatorListener>::Create();
  DisplayCoordinatorListener listener(std::move(listener_server), /*on_displays_changed=*/nullptr,
                                      /*on_vsync=*/nullptr, /*on_client_ownership_change=*/nullptr);
}

TEST_F(DisplayCoordinatorListenerTest, OnDisplaysChanged) {
  std::vector<fuchsia_hardware_display::Info> displays_added;
  std::vector<fuchsia_hardware_display_types::DisplayId> displays_removed;
  auto displays_changed_cb =
      [&displays_added, &displays_removed](
          fidl::VectorView<fuchsia_hardware_display::wire::Info> added,
          fidl::VectorView<fuchsia_hardware_display_types::wire::DisplayId> removed) {
        displays_added = fidl::ToNatural(added).value();
        displays_removed = fidl::ToNatural(removed).value();
      };

  DisplayCoordinatorListener listener(TakeListenerServerEnd(), std::move(displays_changed_cb),
                                      /*on_vsync=*/nullptr, /*on_client_ownership_change=*/nullptr);

  fuchsia_hardware_display_types::wire::Mode test_mode = {
      .active_area = fuchsia_math::wire::SizeU{.width = 1024, .height = 800},
      .refresh_rate_millihertz = 60'000,
  };
  auto pixel_format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8;
  fuchsia_hardware_display::wire::Info test_display = {
      .id = {.value = 1},
      .modes =
          fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(&test_mode, 1),
      .pixel_format =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(&pixel_format, 1),
      .manufacturer_name = "fake_manufacturer_name",
      .monitor_name = "fake_monitor_name",
      .monitor_serial = "fake_monitor_serial",
  };
  fuchsia_hardware_display_types::wire::DisplayId removed = {.value = 2};
  fidl::OneWayError result = mock_display_coordinator()->listener().sync()->OnDisplaysChanged(
      fidl::VectorView<fuchsia_hardware_display::wire::Info>::FromExternal(&test_display, 1),
      fidl::VectorView<fuchsia_hardware_display_types::wire::DisplayId>::FromExternal(&removed, 1));
  ASSERT_TRUE(result.ok());

  ASSERT_EQ(0u, displays_added.size());
  ASSERT_EQ(0u, displays_removed.size());
  RunLoopUntilIdle();
  ASSERT_EQ(1u, displays_added.size());
  ASSERT_EQ(1u, displays_removed.size());
  EXPECT_EQ(displays_added[0], fidl::ToNatural(test_display));
  EXPECT_EQ(displays_removed[0].value(), 2u);

  // Expect no crashes on teardown.
  ResetDisplayCoordinatorListener();
  RunLoopUntilIdle();
}

TEST_F(DisplayCoordinatorListenerTest, OnClientOwnershipChangeCallback) {
  bool has_ownership = false;
  auto client_ownership_change_cb = [&has_ownership](bool ownership) { has_ownership = ownership; };

  DisplayCoordinatorListener listener(TakeListenerServerEnd(), /*on_displays_changed=*/nullptr,
                                      /*on_vsync=*/nullptr, std::move(client_ownership_change_cb));

  fidl::OneWayStatus result =
      mock_display_coordinator()->listener().sync()->OnClientOwnershipChange(true);
  ASSERT_TRUE(result.ok());

  EXPECT_FALSE(has_ownership);
  RunLoopUntilIdle();
  EXPECT_TRUE(has_ownership);

  // Expect no crashes on teardown.
  ResetDisplayCoordinatorListener();
  RunLoopUntilIdle();
}

TEST_F(DisplayCoordinatorListenerTest, OnVsyncCallback) {
  fuchsia_hardware_display_types::wire::DisplayId last_display_id = {
      .value = fuchsia_hardware_display_types::kInvalidDispId,
  };
  zx::time last_timestamp = zx::time::infinite_past();
  fuchsia_hardware_display::wire::ConfigStamp last_config_stamp = {
      .value = fuchsia_hardware_display::kInvalidConfigStampValue,
  };

  auto vsync_cb = [&](fuchsia_hardware_display_types::wire::DisplayId display_id,
                      zx::time timestamp, fuchsia_hardware_display::wire::ConfigStamp stamp,
                      fuchsia_hardware_display::wire::VsyncAckCookie cookie) {
    last_display_id = display_id;
    last_timestamp = timestamp;
    last_config_stamp = stamp;
  };
  DisplayCoordinatorListener listener(TakeListenerServerEnd(), /*on_displays_changed=*/nullptr,
                                      std::move(vsync_cb), /*on_client_ownership_change=*/nullptr);

  const fuchsia_hardware_display_types::wire::DisplayId kTestDisplayId = {.value = 1};
  const fuchsia_hardware_display_types::wire::DisplayId kInvalidDisplayId = {.value = 2};
  const zx::time kTestTimestamp(111111);
  const fuchsia_hardware_display::wire::ConfigStamp kConfigStamp = {.value = 2u};

  fidl::OneWayStatus result = mock_display_coordinator()->listener().sync()->OnVsync(
      kTestDisplayId, kTestTimestamp.get(), kConfigStamp, {0});
  ASSERT_EQ(fuchsia_hardware_display::kInvalidConfigStampValue, last_config_stamp.value);
  RunLoopUntilIdle();
  EXPECT_EQ(kTestDisplayId.value, last_display_id.value);
  EXPECT_EQ(kTestTimestamp, last_timestamp);
  EXPECT_EQ(last_config_stamp.value, kConfigStamp.value);

  // Expect no crashes on teardown.
  ResetDisplayCoordinatorListener();
  RunLoopUntilIdle();
}

}  // namespace

}  // namespace scenic_impl::display::test
