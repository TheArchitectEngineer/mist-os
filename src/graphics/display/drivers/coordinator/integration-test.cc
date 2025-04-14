// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-testing/test_loop.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fidl/cpp/wire/array.h>
#include <lib/fit/result.h>
#include <lib/sysmem-version/sysmem-version.h>
#include <lib/zx/clock.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <cstdint>
#include <memory>
#include <optional>
#include <vector>

#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>
#include <gtest/gtest.h>

#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/coordinator/client.h"
#include "src/graphics/display/drivers/coordinator/controller.h"
#include "src/graphics/display/drivers/coordinator/post-display-task.h"
#include "src/graphics/display/drivers/coordinator/testing/base.h"
#include "src/graphics/display/drivers/coordinator/testing/mock-coordinator-listener.h"
#include "src/graphics/display/drivers/fake/fake-display.h"
#include "src/graphics/display/lib/api-types/cpp/buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/event-id.h"
#include "src/graphics/display/lib/api-types/cpp/image-id.h"
#include "src/graphics/display/lib/api-types/cpp/image-metadata.h"
#include "src/graphics/display/lib/api-types/cpp/image-tiling-type.h"
#include "src/graphics/display/lib/api-types/cpp/mode.h"
#include "src/graphics/display/lib/api-types/cpp/vsync-ack-cookie.h"
#include "src/graphics/display/lib/driver-utils/post-task.h"
#include "src/lib/fsl/handles/object_info.h"
#include "src/lib/testing/predicates/status.h"

namespace display_coordinator {

namespace {

// Cached information about a display reported by the coordinator.
struct TestDisplayInfo {
 public:
  static TestDisplayInfo From(const fuchsia_hardware_display::wire::Info& fidl_display_info);

  display::DisplayId id;

  // Represents an image that covers the entire display.
  display::ImageMetadata fullscreen_image_metadata;
};

// static
TestDisplayInfo TestDisplayInfo::From(
    const fuchsia_hardware_display::wire::Info& fidl_display_info) {
  const display::DisplayId display_id = display::ToDisplayId(fidl_display_info.id);
  ZX_ASSERT(display_id != display::kInvalidDisplayId);

  ZX_ASSERT(!fidl_display_info.modes.empty());
  display::Mode display_mode = display::Mode::From(fidl_display_info.modes[0]);

  const display::ImageMetadata fullscreen_image_metadata = display::ImageMetadata({
      .width = display_mode.active_area().width(),
      .height = display_mode.active_area().height(),
      .tiling_type = display::ImageTilingType::kLinear,
  });

  return TestDisplayInfo{
      .id = display_id,
      .fullscreen_image_metadata = fullscreen_image_metadata,
  };
}

// Coordinator client state updated by the listener protocol.
//
// This class is thread-safe.
class TestClientState {
 public:
  TestClientState() = default;
  TestClientState(const TestClientState&) = delete;
  TestClientState& operator=(const TestClientState&) = delete;
  ~TestClientState() = default;

  // The returned count is guaranteed to be monotonically increasing across the
  // instance's lifetime.
  uint64_t vsync_count() const;

  bool has_display_ownership() const;
  display::ConfigStamp last_vsync_config_stamp() const;
  display::VsyncAckCookie last_vsync_ack_cookie() const;

  bool HasConnectedDisplay() const;

  // The first connected display's ID.
  //
  // Crashes if no display is connected.
  display::DisplayId display_id() const;

  // Metadata for an image that fully covers the first connected display.
  //
  // Crashes if no display is connected.
  display::ImageMetadata FullscreenImageMetadata() const;

  // MockCoordinatorListener implementation
  void OnDisplaysChanged(std::span<const fuchsia_hardware_display::wire::Info> added_displays,
                         std::span<const display::DisplayId> removed_display_ids);
  void OnClientOwnershipChange(bool has_ownership);
  void OnVsync(display::DisplayId display_id, zx::time timestamp,
               display::ConfigStamp applied_config_stamp, display::VsyncAckCookie vsync_ack_cookie);

 private:
  // Locks all the state in this class.
  mutable std::mutex mutex_;

  std::vector<TestDisplayInfo> connected_displays_ __TA_GUARDED(mutex_);
  bool has_display_ownership_ __TA_GUARDED(mutex_) = false;
  uint64_t vsync_count_ TA_GUARDED(mutex_) = 0;
  display::VsyncAckCookie last_vsync_ack_cookie_ __TA_GUARDED(mutex_) =
      display::kInvalidVsyncAckCookie;
  display::ConfigStamp last_vsync_config_stamp_ __TA_GUARDED(mutex_);
};

uint64_t TestClientState::vsync_count() const {
  std::lock_guard lock(mutex_);
  return vsync_count_;
}

bool TestClientState::has_display_ownership() const {
  std::lock_guard lock(mutex_);
  return has_display_ownership_;
}

display::ConfigStamp TestClientState::last_vsync_config_stamp() const {
  std::lock_guard lock(mutex_);
  return last_vsync_config_stamp_;
}

display::VsyncAckCookie TestClientState::last_vsync_ack_cookie() const {
  std::lock_guard lock(mutex_);
  return last_vsync_ack_cookie_;
}

bool TestClientState::HasConnectedDisplay() const {
  std::lock_guard lock(mutex_);
  return !connected_displays_.empty();
}

display::DisplayId TestClientState::display_id() const {
  std::lock_guard lock(mutex_);
  ZX_ASSERT(!connected_displays_.empty());
  return connected_displays_[0].id;
}

display::ImageMetadata TestClientState::FullscreenImageMetadata() const {
  std::lock_guard lock(mutex_);
  ZX_ASSERT(!connected_displays_.empty());
  return connected_displays_[0].fullscreen_image_metadata;
}

void TestClientState::OnDisplaysChanged(
    std::span<const fuchsia_hardware_display::wire::Info> added_displays,
    std::span<const display::DisplayId> removed_display_ids) {
  ZX_ASSERT(removed_display_ids.empty());

  std::lock_guard lock(mutex_);
  for (const fuchsia_hardware_display::wire::Info& added_display : added_displays) {
    connected_displays_.push_back(TestDisplayInfo::From(added_display));
  }
}

void TestClientState::OnClientOwnershipChange(bool has_ownership) {
  std::lock_guard lock(mutex_);
  has_display_ownership_ = has_ownership;
}

void TestClientState::OnVsync(display::DisplayId display_id, zx::time timestamp,
                              display::ConfigStamp applied_config_stamp,
                              display::VsyncAckCookie vsync_ack_cookie) {
  std::lock_guard lock(mutex_);
  ++vsync_count_;
  last_vsync_config_stamp_ = applied_config_stamp;
  if (vsync_ack_cookie != display::kInvalidVsyncAckCookie) {
    last_vsync_ack_cookie_ = vsync_ack_cookie;
  }
}

// Convenience method for creating a B8R8G8A8 color.
constexpr display::Color BgraColor(uint32_t bgra_color) {
  const uint8_t blue = static_cast<uint8_t>(bgra_color);
  const uint8_t green = static_cast<uint8_t>(bgra_color >> 8);
  const uint8_t red = static_cast<uint8_t>(bgra_color >> 16);
  const uint8_t alpha = static_cast<uint8_t>(bgra_color >> 24);
  return display::Color(
      {.format = display::PixelFormat::kB8G8R8A8,
       .bytes = std::initializer_list<uint8_t>{blue, green, red, alpha, 0, 0, 0, 0}});
}

constexpr display::Color kFuchsiaBgra = BgraColor(0xffff00ff);

// Encapsulates boilerplate for driving the Coordinator via FIDL.
//
// This class is not thead-safe. Instances must be accessed on a single thread,
// or on a single synchronized dispatcher. Exception: both `state()` and the
// returned `TestClientState` instance can be accessed from any thread.
class TestFidlClient {
 public:
  struct EventInfo {
    display::EventId id;
    zx::event event;
  };

  // One layer's configuration.
  //
  // This structure only covers the information that varies across integration
  // tests.
  struct LayerConfig {
    // Valid layer configurations must have valid layer IDs.
    display::LayerId layer_id;

    // Invalid if the layer does not include an image.
    display::ImageId image_id = display::kInvalidImageId;

    // Invalid if the layer does not specify a wait event.
    display::EventId image_ready_wait_event_id = display::kInvalidEventId;
  };

  // `sysmem` must outlive this instance.
  explicit TestFidlClient(const fidl::WireSyncClient<fuchsia_sysmem2::Allocator>* sysmem);
  TestFidlClient(const TestFidlClient&) = delete;
  TestFidlClient& operator=(const TestFidlClient&) = delete;
  ~TestFidlClient();

  // Thread-safe.
  TestClientState& state() { return state_; }

  // `coordinator_listener_dispatcher` must be non-null and must be running
  // throughout the test.
  zx::result<> OpenCoordinator(
      const fidl::WireSyncClient<fuchsia_hardware_display::Provider>& provider,
      ClientPriority client_priority, async_dispatcher_t* coordinator_listener_dispatcher);

  zx::result<> EnableVsyncEventDelivery();

  zx::result<> SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode virtcon_mode);
  zx::result<display::LayerId> CreateLayer();
  zx::result<> ImportImage(const display::ImageMetadata& image_metadata,
                           display::BufferId image_buffer_id, display::ImageId image_id);
  zx::result<> ImportEvent(zx::event event, display::EventId event_id);

  // The std::vector can be converted to std::span once we adopt C++23, which has
  // more ergonoic span handling.
  zx::result<> SetDisplayLayers(display::DisplayId display_id,
                                const std::vector<LayerConfig>& layer_configs);

  zx::result<> SetLayerImage(display::LayerId layer_id, display::ImageId image_id,
                             display::EventId event_id);
  zx::result<> SetLayerColor(display::LayerId layer_id, const display::Color& fallback_color);
  zx::result<> CheckConfig();
  zx::result<> ApplyConfig(display::ConfigStamp config_stamp);
  zx::result<> AcknowledgeVsync(display::VsyncAckCookie vsync_ack_cookie);
  zx::result<> SetMinimumRgb(uint8_t minimum_rgb);
  zx::result<display::ConfigStamp> GetLastAppliedConfigStamp();

  zx::result<display::ImageId> ImportImageWithSysmem(const display::ImageMetadata& image_metadata);

  // Imports an image that covers the first connected display.
  //
  // Crashes if no display is connected.
  zx::result<display::ImageId> CreateFullscreenImage();

  // Creates a layer that covers the first connected display.
  //
  // Crashes if no display is connected.
  zx::result<display::LayerId> CreateFullscreenImageLayer();

  // Creates a solid color fill layer that covers the first connected display.
  //
  // Crashes if no display is connected.
  zx::result<display::LayerId> CreateFullscreenColorLayer(display::Color color);

  zx::result<EventInfo> CreateEvent();

  // Returns a one-layer configuration that covers the first connected display.
  //
  // Crashes if no display is connected.
  std::vector<LayerConfig> CreateFullscreenLayerConfig();

  // Applies a configuration to the first connected display.
  //
  // Blocks until the Display Coordinator processes the underlying ApplyConfig()
  // call. Crashes if no display is connected.
  //
  // The std::vector can be converted to std::span once we adopt C++23, which has
  // more ergonoic span handling.
  zx::result<> ApplyLayers(display::ConfigStamp config_stamp,
                           const std::vector<LayerConfig>& layer_configs);

 private:
  display::ImageId next_imported_image_id_{1};

  fidl::WireSyncClient<fuchsia_hardware_display::Coordinator> coordinator_fidl_client_;
  const fidl::WireSyncClient<fuchsia_sysmem2::Allocator>& sysmem_;

  // Must outlive `coordinator_listener_`.
  TestClientState state_;

  // Must outlive `coordinator_listener_binding_`.
  MockCoordinatorListener coordinator_listener_{
      fit::bind_member<&TestClientState::OnDisplaysChanged>(&state_),
      fit::bind_member<&TestClientState::OnVsync>(&state_),
      fit::bind_member<&TestClientState::OnClientOwnershipChange>(&state_)};
  async_dispatcher_t* coordinator_listener_dispatcher_ = nullptr;
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_display::CoordinatorListener>>
      coordinator_listener_binding_;
};

TestFidlClient::TestFidlClient(const fidl::WireSyncClient<fuchsia_sysmem2::Allocator>* sysmem)
    : sysmem_(*sysmem) {
  ZX_ASSERT(sysmem != nullptr);
}

TestFidlClient::~TestFidlClient() {
  if (coordinator_listener_binding_.has_value()) {
    ZX_ASSERT(coordinator_listener_dispatcher_ != nullptr);
    // We can call Unbind() on any thread, but it's async and previously-started dispatches can
    // still be in-flight after this call.
    coordinator_listener_binding_->Unbind();
    // The Unbind() above will prevent starting any new dispatches, but previously-started
    // dispatches can still be in-flight. For this reason we must fence the Bind's dispatcher thread
    // before we delete stuff used during dispatch such as on_vsync_callback_.
    libsync::Completion done;
    zx::result<> post_task_result = display::PostTask<display_coordinator::kDisplayTaskTargetSize>(
        *coordinator_listener_dispatcher_, [&done] { done.Signal(); });
    ZX_ASSERT(post_task_result.is_ok());
    done.Wait();
    // Now it's safe to delete on_vsync_callback_ (for example).
  }
}

zx::result<> TestFidlClient::OpenCoordinator(
    const fidl::WireSyncClient<fuchsia_hardware_display::Provider>& provider,
    ClientPriority client_priority, async_dispatcher_t* coordinator_listener_dispatcher) {
  ZX_ASSERT(coordinator_listener_dispatcher != nullptr);
  ZX_ASSERT_MSG(!coordinator_listener_binding_.has_value(), "OpenCoordinator() already called");
  ZX_ASSERT_MSG(coordinator_listener_dispatcher_ == nullptr, "OpenCoordinator() already called");

  auto [coordinator_client, coordinator_server] =
      fidl::Endpoints<fuchsia_hardware_display::Coordinator>::Create();
  auto [coordinator_listener_client, coordinator_listener_server] =
      fidl::Endpoints<fuchsia_hardware_display::CoordinatorListener>::Create();
  fdf::info("Opening coordinator");
  if (client_priority == ClientPriority::kVirtcon) {
    fidl::Arena arena;
    auto request = fidl::WireRequest<fuchsia_hardware_display::Provider::
                                         OpenCoordinatorWithListenerForVirtcon>::Builder(arena)
                       .coordinator(std::move(coordinator_server))
                       .coordinator_listener(std::move(coordinator_listener_client))
                       .Build();
    auto response = provider->OpenCoordinatorWithListenerForVirtcon(std::move(request));
    if (!response.ok()) {
      fdf::error("Could not open Virtcon coordinator, error={}", response.FormatDescription());
      return zx::make_result(response.status());
    }
  } else {
    ZX_DEBUG_ASSERT(client_priority == ClientPriority::kPrimary);
    fidl::Arena arena;
    auto request = fidl::WireRequest<fuchsia_hardware_display::Provider::
                                         OpenCoordinatorWithListenerForPrimary>::Builder(arena)
                       .coordinator(std::move(coordinator_server))
                       .coordinator_listener(std::move(coordinator_listener_client))
                       .Build();
    auto response = provider->OpenCoordinatorWithListenerForPrimary(std::move(request));
    if (!response.ok()) {
      fdf::error("Could not open coordinator, error={}", response.FormatDescription());
      return zx::make_result(response.status());
    }
  }

  coordinator_fidl_client_.Bind(std::move(coordinator_client));
  coordinator_listener_dispatcher_ = coordinator_listener_dispatcher;
  coordinator_listener_binding_.emplace(fidl::BindServer(coordinator_listener_dispatcher,
                                                         std::move(coordinator_listener_server),
                                                         &coordinator_listener_));
  return zx::ok();
}

zx::result<> TestFidlClient::SetVirtconMode(
    fuchsia_hardware_display::wire::VirtconMode virtcon_mode) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  fidl::OneWayStatus fidl_status = coordinator_fidl_client_->SetVirtconMode(virtcon_mode);
  if (!fidl_status.ok()) {
    fdf::error("SetVirtconMode() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  return zx::ok();
}

zx::result<> TestFidlClient::ImportEvent(zx::event event, display::EventId event_id) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fuchsia_hardware_display::wire::EventId fidl_event_id = display::ToFidlEventId(event_id);

  fidl::OneWayStatus fidl_status =
      coordinator_fidl_client_->ImportEvent(std::move(event), fidl_event_id);
  if (!fidl_status.ok()) {
    fdf::error("ImportEvent() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  return zx::ok();
}

zx::result<display::LayerId> TestFidlClient::CreateLayer() {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fidl::WireResult<fuchsia_hardware_display::Coordinator::CreateLayer> fidl_status =
      coordinator_fidl_client_->CreateLayer();
  if (!fidl_status.ok()) {
    fdf::error("CreateLayer() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }

  const fit::result<zx_status_t, fuchsia_hardware_display::wire::CoordinatorCreateLayerResponse*>&
      fidl_value = fidl_status.value();
  if (fidl_value.is_error()) {
    fdf::error("CreateLayer() returned error: {}", zx::make_result(fidl_value.error_value()));
    return zx::error(fidl_value.error_value());
  }

  return zx::ok(display::ToLayerId(fidl_value.value()->layer_id));
}

zx::result<> TestFidlClient::ImportImage(const display::ImageMetadata& image_metadata,
                                         display::BufferId image_buffer_id,
                                         display::ImageId image_id) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fuchsia_hardware_display::wire::BufferId fidl_image_buffer_id =
      display::ToFidlBufferId(image_buffer_id);
  const fuchsia_hardware_display::wire::ImageId fidl_image_id = display::ToFidlImageId(image_id);

  const fidl::WireResult<fuchsia_hardware_display::Coordinator::ImportImage> fidl_status =
      coordinator_fidl_client_->ImportImage(image_metadata.ToFidl(), fidl_image_buffer_id,
                                            fidl_image_id);
  if (!fidl_status.ok()) {
    fdf::error("ImportImage() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }

  const fit::result<zx_status_t>& fidl_value = fidl_status.value();
  if (fidl_value.is_error()) {
    fdf::error("ImportImage() returned error: {}", zx::make_result(fidl_value.error_value()));
    return zx::error(fidl_value.error_value());
  }
  return zx::ok();
}

zx::result<> TestFidlClient::SetDisplayLayers(display::DisplayId display_id,
                                              const std::vector<LayerConfig>& layer_configs) {
  ZX_ASSERT_MSG(!layer_configs.empty(), "Empty configurations are not supported");

  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  std::vector<fuchsia_hardware_display::wire::LayerId> fidl_layer_ids;
  fidl_layer_ids.reserve(layer_configs.size());
  for (const LayerConfig& layer_config : layer_configs) {
    ZX_ASSERT(layer_config.layer_id != display::kInvalidLayerId);
    const fuchsia_hardware_display::wire::LayerId fidl_layer_id =
        display::ToFidlLayerId(layer_config.layer_id);
    fidl_layer_ids.push_back(fidl_layer_id);
  }

  const fidl::OneWayStatus fidl_status = coordinator_fidl_client_->SetDisplayLayers(
      display::ToFidlDisplayId(display_id),
      fidl::VectorView<fuchsia_hardware_display::wire::LayerId>::FromExternal(fidl_layer_ids));
  if (!fidl_status.ok()) {
    fdf::error("SetDisplayLayers() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  return zx::ok();
}

zx::result<> TestFidlClient::SetLayerImage(display::LayerId layer_id, display::ImageId image_id,
                                           display::EventId event_id) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fuchsia_hardware_display::wire::LayerId fidl_layer_id = display::ToFidlLayerId(layer_id);
  const fuchsia_hardware_display::wire::ImageId fidl_image_id = display::ToFidlImageId(image_id);
  const fuchsia_hardware_display::wire::EventId fidl_event_id = display::ToFidlEventId(event_id);

  const fidl::OneWayStatus fidl_status =
      coordinator_fidl_client_->SetLayerImage2(fidl_layer_id, fidl_image_id, fidl_event_id);
  if (!fidl_status.ok()) {
    fdf::error("SetLayerImage2() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  return zx::ok();
}

zx::result<> TestFidlClient::SetLayerColor(display::LayerId layer_id,
                                           const display::Color& fallback_color) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fuchsia_hardware_display::wire::LayerId fidl_layer_id = display::ToFidlLayerId(layer_id);
  const fuchsia_hardware_display_types::wire::Color fidl_fallback_color = fallback_color.ToFidl();

  const fidl::OneWayStatus fidl_status =
      coordinator_fidl_client_->SetLayerColorConfig(fidl_layer_id, fidl_fallback_color);
  if (!fidl_status.ok()) {
    fdf::error("SetLayerColorConfig() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  return zx::ok();
}

zx::result<> TestFidlClient::CheckConfig() {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fidl::WireResult<fuchsia_hardware_display::Coordinator::CheckConfig> fidl_status =
      coordinator_fidl_client_->CheckConfig(/*discard=*/false);
  if (!fidl_status.ok()) {
    fdf::error("CheckConfig() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  const fidl::WireResponse<fuchsia_hardware_display::Coordinator::CheckConfig>& fidl_result =
      fidl_status.value();
  if (fidl_result.res != fuchsia_hardware_display_types::wire::ConfigResult::kOk) {
    fdf::error("CheckConfig() rejected the config: code {}",
               static_cast<uint32_t>(fidl_result.res));
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok();
}

zx::result<> TestFidlClient::ApplyConfig(display::ConfigStamp config_stamp) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fuchsia_hardware_display::wire::ConfigStamp fidl_config_stamp =
      display::ToFidlConfigStamp(config_stamp);
  fidl::Arena arena;
  fuchsia_hardware_display::wire::CoordinatorApplyConfig3Request request =
      fidl::WireRequest<fuchsia_hardware_display::Coordinator::ApplyConfig3>::Builder(arena)
          .stamp(fidl_config_stamp)
          .Build();

  const fidl::OneWayStatus fidl_status = coordinator_fidl_client_->ApplyConfig3(std::move(request));
  if (!fidl_status.ok()) {
    fdf::error("ApplyConfig() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  return zx::ok();
}

zx::result<> TestFidlClient::AcknowledgeVsync(display::VsyncAckCookie vsync_ack_cookie) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fuchsia_hardware_display::wire::VsyncAckCookie fidl_vsync_ack_cookie =
      display::ToFidlVsyncAckCookie(vsync_ack_cookie);
  const fidl::OneWayStatus fidl_status =
      coordinator_fidl_client_->AcknowledgeVsync(fidl_vsync_ack_cookie.value);
  if (!fidl_status.ok()) {
    fdf::error("AcknowledgeVsync() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  return zx::ok();
}

zx::result<> TestFidlClient::SetMinimumRgb(uint8_t minimum_rgb) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  const fidl::WireResult<fuchsia_hardware_display::Coordinator::SetMinimumRgb> fidl_status =
      coordinator_fidl_client_->SetMinimumRgb(minimum_rgb);
  if (!fidl_status.ok()) {
    fdf::error("SetMinimumRgb() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }

  const fit::result<zx_status_t>& fidl_value = fidl_status.value();
  if (fidl_value.is_error()) {
    fdf::error("SetMinimumRgb() returned error: {}", zx::make_result(fidl_value.error_value()));
    return zx::error(fidl_value.error_value());
  }
  return zx::ok();
}

zx::result<display::ImageId> TestFidlClient::CreateFullscreenImage() {
  return ImportImageWithSysmem(state_.FullscreenImageMetadata());
}

zx::result<display::LayerId> TestFidlClient::CreateFullscreenImageLayer() {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  zx::result<display::LayerId> layer_id_result = CreateLayer();
  if (layer_id_result.is_error()) {
    // CreateLayer() has already logged the error.
    return layer_id_result;
  }

  fidl::OneWayStatus fidl_status = coordinator_fidl_client_->SetLayerPrimaryConfig(
      display::ToFidlLayerId(layer_id_result.value()), state_.FullscreenImageMetadata().ToFidl());
  if (!fidl_status.ok()) {
    fdf::error("SetLayerPrimaryConfig() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  return layer_id_result;
}

zx::result<display::LayerId> TestFidlClient::CreateFullscreenColorLayer(display::Color color) {
  ZX_ASSERT(coordinator_fidl_client_.is_valid());

  zx::result<display::LayerId> layer_id_result = CreateLayer();
  if (layer_id_result.is_error()) {
    // CreateLayer() has already logged the error.
    return layer_id_result;
  }
  const display::LayerId layer_id = layer_id_result.value();
  zx::result<> set_color_result = SetLayerColor(layer_id, color);
  if (set_color_result.is_error()) {
    // SetLayerColor() has already logged the error.
    return set_color_result.take_error();
  }

  return zx::ok(layer_id);
}

zx::result<TestFidlClient::EventInfo> TestFidlClient::CreateEvent() {
  zx::event event;
  zx_status_t create_status = zx::event::create(0u, &event);
  if (create_status != ZX_OK) {
    fdf::error("zx::event::create() failed: {}", zx::make_result(create_status));
    return zx::error(create_status);
  }

  zx_info_handle_basic_t event_handle_info;
  zx_status_t get_info_status = event.get_info(ZX_INFO_HANDLE_BASIC, &event_handle_info,
                                               sizeof(event_handle_info), nullptr, nullptr);
  if (get_info_status != ZX_OK) {
    fdf::error("zx::event::get_info() failed: {}", zx::make_result(get_info_status));
    return zx::error(get_info_status);
  }

  zx::event event_duplicate;
  zx_status_t duplicate_status = event.duplicate(ZX_RIGHT_SAME_RIGHTS, &event_duplicate);
  if (duplicate_status != ZX_OK) {
    fdf::error("zx::event::duplicate() failed: {}", zx::make_result(duplicate_status));
    return zx::error(duplicate_status);
  }

  const display::EventId event_id(event_handle_info.koid);
  zx::result<> import_result = ImportEvent(std::move(event), event_id);
  if (import_result.is_error()) {
    // ImportEvent() has already logged the error.
    return import_result.take_error();
  }

  return zx::ok(EventInfo{
      .id = event_id,
      .event = std::move(event_duplicate),
  });
}

zx::result<> TestFidlClient::EnableVsyncEventDelivery() {
  return zx::make_result(coordinator_fidl_client_->SetVsyncEventDelivery(true).status());
}

zx::result<> TestFidlClient::ApplyLayers(display::ConfigStamp config_stamp,
                                         const std::vector<LayerConfig>& layer_configs) {
  ZX_ASSERT_MSG(!layer_configs.empty(), "Empty configurations are not supported");

  zx::result<> set_display_layers_result = SetDisplayLayers(state_.display_id(), layer_configs);
  if (set_display_layers_result.is_error()) {
    // SetDisplayLayers() has already logged the error.
    return set_display_layers_result;
  }

  for (const LayerConfig& layer_config : layer_configs) {
    if (layer_config.image_id == display::kInvalidImageId) {
      // The layer is configured as a solid color fill layer.
      continue;
    }

    zx::result<> set_layer_image_result = SetLayerImage(
        layer_config.layer_id, layer_config.image_id, layer_config.image_ready_wait_event_id);
    if (set_layer_image_result.is_error()) {
      // SetLayerImage() has already logged the error.
      return set_layer_image_result;
    }
  }

  zx::result<> check_config_result = CheckConfig();
  if (check_config_result.is_error()) {
    // CheckConfig() has already logged the error.
    return check_config_result;
  }

  zx::result<> apply_config_result = ApplyConfig(config_stamp);
  if (apply_config_result.is_error()) {
    // ApplyConfig() has already logged the error.
    return apply_config_result;
  }

  zx::result<display::ConfigStamp> get_last_config_stamp_result = GetLastAppliedConfigStamp();
  if (get_last_config_stamp_result.is_error()) {
    // GetLastAppliedConfigStamp() has already logged the error.
    return get_last_config_stamp_result.take_error();
  }

  if (get_last_config_stamp_result.value() != config_stamp) {
    fdf::error("GetLastAppliedConfigStamp() returned {}, expected {}",
               get_last_config_stamp_result->value(), config_stamp.value());
    return zx::error(ZX_ERR_INTERNAL);
  }
  return zx::ok();
}

zx::result<display::ConfigStamp> TestFidlClient::GetLastAppliedConfigStamp() {
  EXPECT_TRUE(coordinator_fidl_client_);
  fidl::WireResult<fuchsia_hardware_display::Coordinator::GetLatestAppliedConfigStamp> fidl_status =
      coordinator_fidl_client_->GetLatestAppliedConfigStamp();
  if (!fidl_status.ok()) {
    fdf::error("GetLatestAppliedConfigStamp() failed: {}", fidl_status.status_string());
    return zx::error(fidl_status.status());
  }
  const fidl::WireResponse<fuchsia_hardware_display::Coordinator::GetLatestAppliedConfigStamp>&
      fidl_value = fidl_status.value();
  return zx::ok(display::ToConfigStamp(fidl_value.stamp));
}

std::vector<TestFidlClient::LayerConfig> TestFidlClient::CreateFullscreenLayerConfig() {
  zx::result<display::LayerId> layer_id_result = CreateFullscreenImageLayer();
  ZX_ASSERT_MSG(layer_id_result.is_ok(), "%s", layer_id_result.status_string());

  zx::result<display::ImageId> image_id_result =
      ImportImageWithSysmem(state_.FullscreenImageMetadata());
  ZX_ASSERT_MSG(image_id_result.is_ok(), "%s", image_id_result.status_string());

  return {
      LayerConfig{.layer_id = layer_id_result.value(), .image_id = image_id_result.value()},
  };
}

zx::result<display::ImageId> TestFidlClient::ImportImageWithSysmem(
    const display::ImageMetadata& image_metadata) {
  // Create all the tokens.
  fidl::WireSyncClient<fuchsia_sysmem2::BufferCollectionToken> local_token;
  {
    auto [client, server] = fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
    fidl::Arena arena;
    auto allocate_shared_request =
        fuchsia_sysmem2::wire::AllocatorAllocateSharedCollectionRequest::Builder(arena);
    allocate_shared_request.token_request(std::move(server));
    auto result = sysmem_->AllocateSharedCollection(allocate_shared_request.Build());
    if (!result.ok()) {
      fdf::error("Failed to allocate shared collection: {}", result.status_string());
      return zx::error(result.status());
    }
    local_token = fidl::WireSyncClient<fuchsia_sysmem2::BufferCollectionToken>(std::move(client));
    EXPECT_NE(ZX_HANDLE_INVALID, local_token.client_end().channel().get());
  }
  auto [client, server] = fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
  {
    fidl::Arena arena;
    auto duplicate_request =
        fuchsia_sysmem2::wire::BufferCollectionTokenDuplicateRequest::Builder(arena);
    duplicate_request.rights_attenuation_mask(ZX_RIGHT_SAME_RIGHTS);
    duplicate_request.token_request(std::move(server));
    if (auto result = local_token->Duplicate(duplicate_request.Build()); !result.ok()) {
      fdf::error("Failed to duplicate token: {}", result.FormatDescription());
      return zx::error(ZX_ERR_NO_MEMORY);
    }
  }

  // Set display buffer constraints.
  static display::BufferCollectionId next_display_collection_id(0);
  const display::BufferCollectionId display_collection_id = ++next_display_collection_id;
  if (auto result = local_token->Sync(); !result.ok()) {
    fdf::error("Failed to sync token {} {}", result.status(), result.FormatDescription());
    return zx::error(result.status());
  }

  const fuchsia_hardware_display::wire::BufferCollectionId fidl_display_collection_id =
      ToFidlBufferCollectionId(display_collection_id);
  const auto result = coordinator_fidl_client_->ImportBufferCollection(fidl_display_collection_id,
                                                                       std::move(client));
  if (!result.ok()) {
    fdf::error("Failed to call FIDL ImportBufferCollection {} ({})", display_collection_id.value(),
               result.status_string());
    return zx::error(result.status());
  }
  if (result.value().is_error()) {
    fdf::error("Failed to import buffer collection {} ({})", display_collection_id.value(),
               zx::make_result(result.value().error_value()));
    return zx::error(result.value().error_value());
  }

  const fuchsia_hardware_display_types::wire::ImageBufferUsage image_buffer_usage = {
      .tiling_type = image_metadata.tiling_type().ToFidl(),
  };

  const auto set_constraints_result = coordinator_fidl_client_->SetBufferCollectionConstraints(
      fidl_display_collection_id, image_buffer_usage);

  if (!set_constraints_result.ok()) {
    fdf::error("Failed to call FIDL SetBufferCollectionConstraints {} ({})",
               display_collection_id.value(), set_constraints_result.status_string());
    (void)coordinator_fidl_client_->ReleaseBufferCollection(fidl_display_collection_id);
    return zx::error(set_constraints_result.status());
  }
  if (set_constraints_result.value().is_error()) {
    fdf::error("Failed to set buffer collection constraints: {}",
               zx::make_result(set_constraints_result.value().error_value()));
    (void)coordinator_fidl_client_->ReleaseBufferCollection(fidl_display_collection_id);
    return zx::error(set_constraints_result.value().error_value());
  }

  // Use the local collection so we can read out the error if allocation
  // fails, and to ensure everything's allocated before trying to import it
  // into another process.
  fidl::WireSyncClient<fuchsia_sysmem2::BufferCollection> sysmem_collection;
  {
    auto [client, server] = fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
    fidl::Arena arena;
    auto bind_shared_request =
        fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena);
    bind_shared_request.token(local_token.TakeClientEnd());
    bind_shared_request.buffer_collection_request(std::move(server));
    if (auto result = sysmem_->BindSharedCollection(bind_shared_request.Build()); !result.ok()) {
      fdf::error("Failed to bind shared collection: {}", result.FormatDescription());
      return zx::error(result.status());
    }
    sysmem_collection = fidl::WireSyncClient<fuchsia_sysmem2::BufferCollection>(std::move(client));
  }
  // TODO(https://fxbug.dev/42180237) Consider handling the error instead of ignoring it.
  fidl::Arena arena;
  auto set_name_request = fuchsia_sysmem2::wire::NodeSetNameRequest::Builder(arena);
  set_name_request.priority(10000u);
  set_name_request.name("display-client-unittest");
  (void)sysmem_collection->SetName(set_name_request.Build());
  arena.Reset();
  auto constraints = fuchsia_sysmem2::wire::BufferCollectionConstraints::Builder(arena);
  constraints.min_buffer_count(1);
  constraints.usage(fuchsia_sysmem2::wire::BufferUsage::Builder(arena)
                        .none(fuchsia_sysmem2::wire::kNoneUsage)
                        .Build());
  // We specify min_size_bytes 1 so that something is specifying a minimum size. More typically the
  // display client would specify ImageFormatConstraints that implies a non-zero min_size_bytes.
  constraints.buffer_memory_constraints(
      fuchsia_sysmem2::wire::BufferMemoryConstraints::Builder(arena)
          .min_size_bytes(1)
          .ram_domain_supported(true)
          .Build());
  auto set_constraints_request =
      fuchsia_sysmem2::wire::BufferCollectionSetConstraintsRequest::Builder(arena);
  set_constraints_request.constraints(constraints.Build());
  zx_status_t status = sysmem_collection->SetConstraints(set_constraints_request.Build()).status();
  if (status != ZX_OK) {
    fdf::error("Unable to set constraints ({})", status);
    return zx::error(status);
  }
  // Wait for the buffers to be allocated.
  auto info_result = sysmem_collection->WaitForAllBuffersAllocated();
  if (!info_result.ok()) {
    fdf::error("Waiting for buffers failed (fidl={} res={})", info_result.status(),
               fidl::ToUnderlying(info_result->error_value()));
    zx_status_t status = info_result.status();
    if (status == ZX_OK) {
      status = sysmem::V1CopyFromV2Error(info_result->error_value());
    }
    return zx::error(status);
  }

  auto& info = info_result.value()->buffer_collection_info();
  if (info.buffers().count() < 1) {
    fdf::error("Incorrect buffer collection count {}", info.buffers().count());
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  const display::ImageId image_id = next_imported_image_id_;
  ++next_imported_image_id_;

  const display::BufferId image_buffer_id{
      .buffer_collection_id = display_collection_id,
      .buffer_index = 0,
  };
  zx::result<> import_image_result = ImportImage(image_metadata, image_buffer_id, image_id);
  if (import_image_result.is_error()) {
    // ImportImage() has already logged the error.
    return import_image_result.take_error();
  }

  // TODO(https://fxbug.dev/42180237) Consider handling the error instead of ignoring it.
  (void)sysmem_collection->Release();
  return zx::ok(image_id);
}

}  // namespace

class IntegrationTest : public TestBase {
 public:
  // Returns null if there is no client connected at `client_priority`.
  static ClientProxy* GetClientProxy(Controller& coordinator_controller,
                                     ClientPriority client_priority)
      __TA_REQUIRES(coordinator_controller.mtx()) {
    switch (client_priority) {
      case ClientPriority::kPrimary:
        return coordinator_controller.primary_client_;
      case ClientPriority::kVirtcon:
        return coordinator_controller.virtcon_client_;
    }
    ZX_DEBUG_ASSERT_MSG(false, "Unimplemtened client priority: %d",
                        static_cast<int>(client_priority));
    return nullptr;
  }

  display::VsyncAckCookie LastAckedCookie(ClientPriority client_priority) {
    Controller& coordinator_controller = *CoordinatorController();
    fbl::AutoLock<fbl::Mutex> controller_lock(coordinator_controller.mtx());
    ClientProxy* client_proxy = GetClientProxy(coordinator_controller, client_priority);
    ZX_ASSERT(client_proxy != nullptr);

    return client_proxy->LastVsyncAckCookieForTesting();
  }

  void SendVsyncAfterUnbind(std::unique_ptr<TestFidlClient> client, display::DisplayId display_id) {
    fbl::AutoLock<fbl::Mutex> controller_lock(CoordinatorController()->mtx());
    ClientProxy* client_proxy = CoordinatorController()->client_owning_displays_;

    // Resetting the client will *start* client tear down.
    //
    // ~MockCoordinatorListener fences the server-side dispatcher thread (consistent with the
    // threading model of its fidl server binding), but that doesn't sync with the client end
    // (intentionally).
    client.reset();
    ZX_ASSERT_MSG(CoordinatorController()->client_owning_displays_ == client_proxy,
                  "The display owner changed while holding the controller mutex");
    EXPECT_OK(
        sync_completion_wait(client_proxy->FidlUnboundCompletionForTesting(), zx::sec(1).get()));
    // SetVsyncEventDelivery(false) has not completed here, because we are still
    // holding controller()->mtx()
    client_proxy->OnDisplayVsync(display_id, 0, display::kInvalidDriverConfigStamp);
  }

  bool IsClientConnected(ClientPriority client_priority) {
    Controller& coordinator_controller = *CoordinatorController();
    fbl::AutoLock<fbl::Mutex> controller_lock(coordinator_controller.mtx());
    return GetClientProxy(coordinator_controller, client_priority) != nullptr;
  }

  void SendVsyncFromCoordinatorClientProxy() {
    fbl::AutoLock<fbl::Mutex> controller_lock(CoordinatorController()->mtx());
    CoordinatorController()->client_owning_displays_->OnDisplayVsync(
        display::kInvalidDisplayId, 0, display::kInvalidDriverConfigStamp);
  }

  void TriggerDisplayEngineVsync() { FakeDisplayEngine().TriggerVsync(); }

  display::DriverConfigStamp DisplayEngineAppliedConfigStamp() {
    return FakeDisplayEngine().LastAppliedConfigStamp();
  }

  // Sets up a Display Coordinator client connection that delivers VSync events.
  //
  // The returned client is guaranteed to have a connected display. However, the
  // client is not guaranteed to own the displays.
  //
  // The returned client has requested VSync event delivery. However, the request
  // may not have been processed by the Display Coordinator.
  //
  // Crashes if any error occurs during the setup.
  std::unique_ptr<TestFidlClient> OpenCoordinatorTestFidlClient(
      const fidl::WireSyncClient<fuchsia_sysmem2::Allocator>* sysmem_client,
      const fidl::WireSyncClient<fuchsia_hardware_display::Provider>& display_provider_client,
      ClientPriority client_priority) {
    ZX_ASSERT(sysmem_client != nullptr);
    ZX_ASSERT(sysmem_client->is_valid());
    ZX_ASSERT(display_provider_client.is_valid());

    auto coordinator_client = std::make_unique<TestFidlClient>(&sysmem_client_);
    zx::result<> open_coordinator_result =
        coordinator_client->OpenCoordinator(display_provider_client, client_priority, dispatcher());
    ZX_ASSERT_MSG(open_coordinator_result.is_ok(), "Failed to open coordinator: %s",
                  open_coordinator_result.status_string());

    zx::result<> enable_vsync_result = coordinator_client->EnableVsyncEventDelivery();
    ZX_ASSERT_MSG(enable_vsync_result.is_ok(), "Failed to enable Vsync delivery for client: %s",
                  enable_vsync_result.status_string());

    bool poll_success =
        PollUntilOnLoop([&]() { return coordinator_client->state().HasConnectedDisplay(); });
    ZX_ASSERT_MSG(poll_success, "Loop shut down while waiting for display info");

    return coordinator_client;
  }

  // |TestBase|
  void SetUp() override {
    TestBase::SetUp();
    fidl::SyncClient sysmem_client(ConnectToSysmemAllocatorV2());
    EXPECT_TRUE(sysmem_client.is_valid());

    fuchsia_sysmem2::AllocatorSetDebugClientInfoRequest request;
    request.name() = fsl::GetCurrentProcessName();
    request.id() = fsl::GetCurrentProcessKoid();
    fit::result<fidl::OneWayStatus> set_debug_result =
        sysmem_client->SetDebugClientInfo(std::move(request));
    EXPECT_TRUE(set_debug_result.is_ok());
    sysmem_client_ =
        fidl::WireSyncClient<fuchsia_sysmem2::Allocator>(sysmem_client.TakeClientEnd());
  }

  // |TestBase|
  void TearDown() override {
    // Wait until the display core has processed all client disconnections.
    EXPECT_TRUE(PollUntilOnLoop([&]() { return !IsClientConnected(ClientPriority::kPrimary); }));
    EXPECT_TRUE(PollUntilOnLoop([&]() { return !IsClientConnected(ClientPriority::kVirtcon); }));

    TestBase::TearDown();
  }

 protected:
  fidl::WireSyncClient<fuchsia_sysmem2::Allocator> sysmem_client_;
};

TEST_F(IntegrationTest, DISABLED_ClientsCanBail) {
  for (size_t i = 0; i < 100; i++) {
    std::unique_ptr<TestFidlClient> client = OpenCoordinatorTestFidlClient(
        &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  }
}

TEST_F(IntegrationTest, MustUseUniqueEventIDs) {
  std::unique_ptr<TestFidlClient> client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  zx::event event_a, event_b, event_c;
  ASSERT_OK(zx::event::create(0, &event_a));
  ASSERT_OK(zx::event::create(0, &event_b));
  ASSERT_OK(zx::event::create(0, &event_c));
  {
    static constexpr display::EventId kEventId(123);
    ASSERT_OK(client->ImportEvent(std::move(event_a), kEventId));
    ASSERT_OK(client->ImportEvent(std::move(event_b), kEventId));
    // This test passes if it closes without deadlocking.
  }
  // TODO: Use LLCPP epitaphs when available to detect ZX_ERR_PEER_CLOSED.
}

TEST_F(IntegrationTest, VsyncEventForImageConfig) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  static constexpr display::ConfigStamp kInitialConfigStamp(42);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp,
                                        primary_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));

  // Wait for a VSync acknowledging the displayed configuration.
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());
}

TEST_F(IntegrationTest, VsyncEventForImagelessConfig) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  static constexpr display::ConfigStamp kInitialConfigStamp(42);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));

  // Wait for a VSync acknowledging the displayed configuration.
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());
}

TEST_F(IntegrationTest, VsyncEventAfterImageLayerConvertsToColorLayer) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp,
                                        primary_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp initial_driver_config_stamp = DisplayEngineAppliedConfigStamp();

  // Wait for a VSync acknowledging the displayed configuration.
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  // Replace the image layer with a color layer.
  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  static constexpr display::ConfigStamp kSecondConfigStamp(2);
  ASSERT_EQ(initial_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kSecondConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > initial_driver_config_stamp; }));

  // Wait for a VSync acknowledging the configuration with a layer change.
  ASSERT_EQ(1u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 2; }));
  EXPECT_EQ(kSecondConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(2u, primary_client->state().vsync_count());
}

TEST_F(IntegrationTest, DisplayOwnershipChangeEvents) {
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));
  EXPECT_FALSE(virtcon_client->state().has_display_ownership());

  primary_client.reset();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));
}

TEST_F(IntegrationTest, ApplyConfigAfterOwnerChangeWithImageLayers) {
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  static constexpr display::ConfigStamp kVirtconConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(virtcon_client->ApplyLayers(kVirtconConfigStamp,
                                        virtcon_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp virtcon_driver_config_stamp = DisplayEngineAppliedConfigStamp();

  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  static constexpr display::ConfigStamp kPrimaryConfigStamp(2);
  ASSERT_EQ(virtcon_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kPrimaryConfigStamp,
                                        primary_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > virtcon_driver_config_stamp; }));
}

TEST_F(IntegrationTest, ApplyConfigAfterOwnerChangeWithColorLayers) {
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_virtcon_color_layer_result =
      virtcon_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_virtcon_color_layer_result);
  display::LayerId virtcon_color_layer_id = create_virtcon_color_layer_result.value();

  static constexpr display::ConfigStamp kVirtconConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(
      virtcon_client->ApplyLayers(kVirtconConfigStamp, {{.layer_id = virtcon_color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp virtcon_driver_config_stamp = DisplayEngineAppliedConfigStamp();

  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_primary_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_primary_color_layer_result);
  display::LayerId primary_color_layer_id = create_primary_color_layer_result.value();

  static constexpr display::ConfigStamp kPrimaryConfigStamp(2);
  ASSERT_EQ(virtcon_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(
      primary_client->ApplyLayers(kPrimaryConfigStamp, {{.layer_id = primary_color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > virtcon_driver_config_stamp; }));
}

TEST_F(IntegrationTest, VsyncEventAfterOwnerChangeWithImageLayers) {
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  static constexpr display::ConfigStamp kVirtconConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(virtcon_client->ApplyLayers(kVirtconConfigStamp,
                                        virtcon_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp virtcon_driver_config_stamp = DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(0u, virtcon_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kVirtconConfigStamp, virtcon_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, virtcon_client->state().vsync_count());

  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  static constexpr display::ConfigStamp kPrimaryConfigStamp(2);
  ASSERT_EQ(virtcon_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kPrimaryConfigStamp,
                                        primary_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > virtcon_driver_config_stamp; }));

  EXPECT_EQ(1u, virtcon_client->state().vsync_count());
  EXPECT_EQ(kVirtconConfigStamp, virtcon_client->state().last_vsync_config_stamp());

  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kPrimaryConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  EXPECT_EQ(1u, virtcon_client->state().vsync_count());
  EXPECT_EQ(kVirtconConfigStamp, virtcon_client->state().last_vsync_config_stamp());
}

TEST_F(IntegrationTest, VsyncEventAfterOwnerChangeWithColorLayers) {
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_virtcon_color_layer_result =
      virtcon_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_virtcon_color_layer_result);
  display::LayerId virtcon_color_layer_id = create_virtcon_color_layer_result.value();

  static constexpr display::ConfigStamp kVirtconConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(
      virtcon_client->ApplyLayers(kVirtconConfigStamp, {{.layer_id = virtcon_color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp virtcon_driver_config_stamp = DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(0u, virtcon_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kVirtconConfigStamp, virtcon_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, virtcon_client->state().vsync_count());

  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_primary_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_primary_color_layer_result);
  display::LayerId primary_color_layer_id = create_primary_color_layer_result.value();

  static constexpr display::ConfigStamp kPrimaryConfigStamp(2);
  ASSERT_EQ(virtcon_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(
      primary_client->ApplyLayers(kPrimaryConfigStamp, {{.layer_id = primary_color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > virtcon_driver_config_stamp; }));

  EXPECT_EQ(1u, virtcon_client->state().vsync_count());
  EXPECT_EQ(kVirtconConfigStamp, virtcon_client->state().last_vsync_config_stamp());

  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kPrimaryConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  EXPECT_EQ(1u, virtcon_client->state().vsync_count());
  EXPECT_EQ(kVirtconConfigStamp, virtcon_client->state().last_vsync_config_stamp());
}

TEST_F(IntegrationTest, VsyncEventsAfterClientChange) {
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  // The Virtcon client does not apply any configuration, so it will never be
  // eligible for VSync events.

  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  // Display an image.
  static constexpr display::ConfigStamp kPrimary1InitialConfigStamp(2);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kPrimary1InitialConfigStamp,
                                        primary_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp primary1_initial_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kPrimary1InitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  // Replace the image layer with a color layer.
  zx::result<display::LayerId> create_primary_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_primary_color_layer_result);
  display::LayerId primary_color_layer_id = create_primary_color_layer_result.value();

  static constexpr display::ConfigStamp kPrimary1SecondConfigStamp(3);
  ASSERT_EQ(primary1_initial_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kPrimary1SecondConfigStamp,
                                        {{.layer_id = primary_color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > primary1_initial_driver_config_stamp; }));
  const display::DriverConfigStamp primary1_second_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  // The primary client disconnects, and the Virtcon client receives display
  // ownership. The old primary client's config remains applied, because the
  // Virtcon client did not apply any config.
  primary_client.reset();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));
  EXPECT_EQ(primary1_second_driver_config_stamp, DisplayEngineAppliedConfigStamp());

  // A new primary client connects.
  primary_client = OpenCoordinatorTestFidlClient(&sysmem_client_, DisplayProviderClient(),
                                                 ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  // The VSync must be routed to the client that applied the configuration,
  // which is now disconnected. Nothing should be sent to the new client.
  EXPECT_EQ(primary1_second_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  TriggerDisplayEngineVsync();

  static constexpr display::ConfigStamp kPrimary2InitialConfigStamp(4);
  ASSERT_EQ(primary1_second_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kPrimary2InitialConfigStamp,
                                        primary_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > primary1_second_driver_config_stamp; }));

  // Send a VSync using the config the client applied.
  EXPECT_EQ(0u, virtcon_client->state().vsync_count());
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kPrimary2InitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());
  EXPECT_EQ(0u, virtcon_client->state().vsync_count());
}

TEST_F(IntegrationTest, DISABLED_SendVsyncsAfterClientsBail) {
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kVirtconInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(virtcon_client->ApplyLayers(kVirtconInitialConfigStamp, {}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp virtcon_initial_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  // Present an image
  static constexpr display::ConfigStamp kPrimaryInitialConfigStamp(2);
  ASSERT_EQ(virtcon_initial_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kPrimaryInitialConfigStamp,
                                        primary_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > virtcon_initial_driver_config_stamp; }));

  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kPrimaryInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  // Send the controller a vsync for an image / a config it won't recognize anymore.
  //
  // TODO(https://fxbug.dev/388885807): The comment above describes the behavior
  // of a misbehaving display engine driver. Consider whether it's suitable to
  // disconnect the driver, rather than working around the error.
  const config_stamp_t invalid_banjo_config_stamp =
      display::ToBanjoDriverConfigStamp(virtcon_initial_driver_config_stamp);
  CoordinatorController()->DisplayEngineListenerOnDisplayVsync(
      ToBanjoDisplayId(primary_client->state().display_id()), 0u, &invalid_banjo_config_stamp);

  // Send a second vsync, using the config the client applied.
  ASSERT_EQ(1u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 2; }));
  EXPECT_EQ(kPrimaryInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(2u, primary_client->state().vsync_count());
}

TEST_F(IntegrationTest, SendVsyncsAfterClientDies) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));
  display::DisplayId display_id = primary_client->state().display_id();
  SendVsyncAfterUnbind(std::move(primary_client), display_id);
}

TEST_F(IntegrationTest, AcknowledgeVsync) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));

  // send vsyncs up to watermark level
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  for (uint32_t i = 0; i < ClientProxy::kVsyncMessagesWatermark; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return primary_client->state().last_vsync_ack_cookie() != display::kInvalidVsyncAckCookie;
  }));
  EXPECT_EQ(ClientProxy::kVsyncMessagesWatermark, primary_client->state().vsync_count());

  // acknowledge
  ASSERT_OK(primary_client->AcknowledgeVsync(primary_client->state().last_vsync_ack_cookie()));
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return LastAckedCookie(ClientPriority::kPrimary) ==
           primary_client->state().last_vsync_ack_cookie();
  }));
}

TEST_F(IntegrationTest, AcknowledgeVsyncAfterQueueFull) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));

  // send vsyncs until max vsync
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  uint32_t vsync_count = ClientProxy::kMaxVsyncMessages;
  while (vsync_count--) {
    SendVsyncFromCoordinatorClientProxy();
  }
  {
    static constexpr uint64_t expected_vsync_count = ClientProxy::kMaxVsyncMessages;
    ASSERT_TRUE(PollUntilOnLoop(
        [&]() { return (primary_client->state().vsync_count() >= expected_vsync_count); }));
    EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
  }
  EXPECT_NE(display::kInvalidVsyncAckCookie, primary_client->state().last_vsync_ack_cookie());

  // At this point, display will not send any more vsync events. Let's confirm by sending a few
  constexpr uint32_t kNumVsync = 5;
  for (uint32_t i = 0; i < kNumVsync; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }
  EXPECT_EQ(ClientProxy::kMaxVsyncMessages, primary_client->state().vsync_count());

  // now let's acknowledge vsync
  ASSERT_OK(primary_client->AcknowledgeVsync(primary_client->state().last_vsync_ack_cookie()));
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return LastAckedCookie(ClientPriority::kPrimary) ==
           primary_client->state().last_vsync_ack_cookie();
  }));

  // After acknowledge, we should expect to get all the stored messages + the latest vsync
  SendVsyncFromCoordinatorClientProxy();
  {
    static constexpr uint64_t expected_vsync_count = ClientProxy::kMaxVsyncMessages + kNumVsync + 1;
    ASSERT_TRUE(PollUntilOnLoop(
        [&]() { return primary_client->state().vsync_count() >= expected_vsync_count; }));
    EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
  }
}

TEST_F(IntegrationTest, AcknowledgeVsyncAfterLongTime) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));

  // send vsyncs until max vsyncs
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  for (uint32_t i = 0; i < ClientProxy::kMaxVsyncMessages; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return primary_client->state().vsync_count() >= ClientProxy::kMaxVsyncMessages; }));
  EXPECT_EQ(ClientProxy::kMaxVsyncMessages, primary_client->state().vsync_count());
  EXPECT_NE(display::kInvalidVsyncAckCookie, primary_client->state().last_vsync_ack_cookie());

  // At this point, display will not send any more vsync events. Let's confirm by sending a lot
  constexpr uint32_t kNumVsync = ClientProxy::kVsyncBufferSize * 10;
  for (uint32_t i = 0; i < kNumVsync; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }
  EXPECT_EQ(ClientProxy::kMaxVsyncMessages, primary_client->state().vsync_count());

  // now let's acknowledge vsync
  ASSERT_OK(primary_client->AcknowledgeVsync(primary_client->state().last_vsync_ack_cookie()));
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return LastAckedCookie(ClientPriority::kPrimary) ==
           primary_client->state().last_vsync_ack_cookie();
  }));

  // After acknowledge, we should expect to get all the stored messages + the latest vsync
  SendVsyncFromCoordinatorClientProxy();
  {
    static constexpr uint64_t expected_vsync_count =
        ClientProxy::kMaxVsyncMessages + ClientProxy::kVsyncBufferSize + 1;
    ASSERT_TRUE(PollUntilOnLoop(
        [&]() { return primary_client->state().vsync_count() >= expected_vsync_count; }));
    EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
  }
}

TEST_F(IntegrationTest, AcknowledgeVsyncWithUnissuedCookie) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));

  // send vsyncs until max vsync
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  for (uint32_t i = 0; i < ClientProxy::kMaxVsyncMessages; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return (primary_client->state().vsync_count() >= ClientProxy::kMaxVsyncMessages); }));
  EXPECT_EQ(ClientProxy::kMaxVsyncMessages, primary_client->state().vsync_count());
  EXPECT_NE(display::kInvalidVsyncAckCookie, primary_client->state().last_vsync_ack_cookie());

  // At this point, display will not send any more vsync events. Let's confirm by sending a few
  constexpr uint32_t kNumVsync = 5;
  for (uint32_t i = 0; i < kNumVsync; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }

  // TODO(https://fxbug.dev/388885807): This test is racy. There's no guarantee
  // that the TestFidlClient processed all events coming from the Coordinator.
  EXPECT_EQ(ClientProxy::kMaxVsyncMessages, primary_client->state().vsync_count());

  // now let's acknowledge vsync with invalid cookie
  static constexpr display::VsyncAckCookie kInvalidCookie(0xdeadbeef);
  ASSERT_NE(primary_client->state().last_vsync_ack_cookie(), kInvalidCookie);
  ASSERT_OK(primary_client->AcknowledgeVsync(kInvalidCookie));

  // This check can have a false positive pass, due to using a hard-coded
  // timeout.
  {
    zx::time deadline = zx::deadline_after(zx::sec(1));
    PollUntilOnLoop([&]() {
      if (zx::clock::get_monotonic() >= deadline)
        return true;
      return LastAckedCookie(ClientPriority::kPrimary) ==
             primary_client->state().last_vsync_ack_cookie();
    });
  }
  EXPECT_NE(LastAckedCookie(ClientPriority::kPrimary),
            primary_client->state().last_vsync_ack_cookie());

  // We should still not receive vsync events since acknowledge did not use valid cookie
  SendVsyncFromCoordinatorClientProxy();
  constexpr uint64_t expected_vsync_count = ClientProxy::kMaxVsyncMessages;

  // This check can have a false positive pass, due to using a hard-coded
  // timeout.
  {
    zx::time deadline = zx::deadline_after(zx::sec(1));
    PollUntilOnLoop([&]() {
      if (zx::clock::get_monotonic() >= deadline)
        return true;
      return primary_client->state().vsync_count() >= expected_vsync_count + 1;
    });
  }
  EXPECT_LT(primary_client->state().vsync_count(), expected_vsync_count + 1);

  EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
}

TEST_F(IntegrationTest, AcknowledgeVsyncWithOldCookie) {
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));

  // send vsyncs until max vsync
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  for (uint32_t i = 0; i < ClientProxy::kMaxVsyncMessages; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }
  {
    static constexpr uint64_t expected_vsync_count = ClientProxy::kMaxVsyncMessages;
    ASSERT_TRUE(PollUntilOnLoop(
        [&]() { return primary_client->state().vsync_count() >= expected_vsync_count; }));
    EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
  }
  EXPECT_NE(display::kInvalidVsyncAckCookie, primary_client->state().last_vsync_ack_cookie());

  // At this point, display will not send any more vsync events. Let's confirm by sending a few
  constexpr uint32_t kNumVsync = 5;
  for (uint32_t i = 0; i < kNumVsync; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }

  // TODO(https://fxbug.dev/388885807): This test is racy. There's no guarantee
  // that the TestFidlClient processed all events coming from the Coordinator.
  EXPECT_EQ(ClientProxy::kMaxVsyncMessages, primary_client->state().vsync_count());

  // now let's acknowledge vsync

  ASSERT_OK(primary_client->AcknowledgeVsync(primary_client->state().last_vsync_ack_cookie()));
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return LastAckedCookie(ClientPriority::kPrimary) ==
           primary_client->state().last_vsync_ack_cookie();
  }));

  // After acknowledge, we should expect to get all the stored messages + the latest vsync
  SendVsyncFromCoordinatorClientProxy();
  {
    static constexpr uint64_t expected_vsync_count = ClientProxy::kMaxVsyncMessages + kNumVsync + 1;
    ASSERT_TRUE(PollUntilOnLoop(
        [&]() { return (primary_client->state().vsync_count() >= expected_vsync_count); }));
    EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
  }

  // save old cookie
  display::VsyncAckCookie old_vsync_ack_cookie = primary_client->state().last_vsync_ack_cookie();

  // send vsyncs until max vsync
  for (uint32_t i = 0; i < ClientProxy::kMaxVsyncMessages; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }

  {
    static constexpr uint64_t expected_vsync_count = ClientProxy::kMaxVsyncMessages * 2;
    ASSERT_TRUE(PollUntilOnLoop(
        [&]() { return (primary_client->state().vsync_count() >= expected_vsync_count); }));
    EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
  }
  EXPECT_NE(display::kInvalidVsyncAckCookie, primary_client->state().last_vsync_ack_cookie());

  // At this point, display will not send any more vsync events. Let's confirm by sending a few
  for (uint32_t i = 0; i < ClientProxy::kVsyncBufferSize; i++) {
    SendVsyncFromCoordinatorClientProxy();
  }
  EXPECT_EQ(ClientProxy::kMaxVsyncMessages * 2, primary_client->state().vsync_count());

  // now let's acknowledge vsync with old cookie
  ASSERT_OK(primary_client->AcknowledgeVsync(old_vsync_ack_cookie));

  // This check can have a false positive pass, due to using a hard-coded
  // timeout.
  {
    zx::time deadline = zx::deadline_after(zx::sec(1));
    PollUntilOnLoop([&]() {
      if (zx::clock::get_monotonic() >= deadline)
        return true;
      return LastAckedCookie(ClientPriority::kPrimary) ==
             primary_client->state().last_vsync_ack_cookie();
    });
  }
  EXPECT_NE(LastAckedCookie(ClientPriority::kPrimary),
            primary_client->state().last_vsync_ack_cookie());

  // Since we did not acknowledge with most recent cookie, we should not get any vsync events back
  SendVsyncFromCoordinatorClientProxy();
  {
    static constexpr uint64_t expected_vsync_count = ClientProxy::kMaxVsyncMessages * 2;

    // This check can have a false positive pass, due to using a hard-coded
    // timeout.
    {
      zx::time deadline = zx::deadline_after(zx::sec(1));
      PollUntilOnLoop([&]() {
        if (zx::clock::get_monotonic() >= deadline)
          return true;
        return primary_client->state().vsync_count() >= expected_vsync_count + 1;
      });
    }
    EXPECT_LT(primary_client->state().vsync_count(), expected_vsync_count + 1);

    // count should still remain the same
    EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
  }

  // now let's acknowledge with valid cookie
  ASSERT_OK(primary_client->AcknowledgeVsync(primary_client->state().last_vsync_ack_cookie()));
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return LastAckedCookie(ClientPriority::kPrimary) ==
           primary_client->state().last_vsync_ack_cookie();
  }));

  // After acknowledge, we should expect to get all the stored messages + the latest vsync
  SendVsyncFromCoordinatorClientProxy();
  {
    static constexpr uint64_t expected_vsync_count =
        ClientProxy::kMaxVsyncMessages * 2 + ClientProxy::kVsyncBufferSize + 1;
    ASSERT_TRUE(PollUntilOnLoop(
        [&]() { return primary_client->state().vsync_count() >= expected_vsync_count; }));
    EXPECT_EQ(expected_vsync_count, primary_client->state().vsync_count());
  }
}

TEST_F(IntegrationTest, CreateImageLayer) {
  std::unique_ptr<TestFidlClient> client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);

  EXPECT_OK(client->CreateFullscreenImageLayer());
}

TEST_F(IntegrationTest, CreateColorLayer) {
  std::unique_ptr<TestFidlClient> client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);

  EXPECT_OK(client->CreateFullscreenColorLayer(kFuchsiaBgra));
}

TEST_F(IntegrationTest, ImportImageWithInvalidImageId) {
  std::unique_ptr<TestFidlClient> client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);

  constexpr display::ImageId image_id = display::kInvalidImageId;
  constexpr display::BufferCollectionId buffer_collection_id(0xffeeeedd);

  zx::result<> import_image_result = client->ImportImage(
      client->state().FullscreenImageMetadata(),
      display::BufferId{.buffer_collection_id = buffer_collection_id, .buffer_index = 0}, image_id);
  EXPECT_NE(ZX_OK, import_image_result.status_value()) << import_image_result.status_string();
}

TEST_F(IntegrationTest, ImportImageWithNonExistentBufferCollectionId) {
  std::unique_ptr<TestFidlClient> client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);

  constexpr display::BufferCollectionId kNonExistentCollectionId(0xffeeeedd);
  constexpr display::ImageId image_id(1);
  zx::result<> import_image_result = client->ImportImage(
      client->state().FullscreenImageMetadata(),
      display::BufferId{.buffer_collection_id = kNonExistentCollectionId, .buffer_index = 0},
      image_id);
  EXPECT_NE(ZX_OK, import_image_result.status_value()) << import_image_result.status_string();
}

TEST_F(IntegrationTest, ClampRgb) {
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  {
    // Clamp RGB to a minimum value
    ASSERT_OK(virtcon_client->SetMinimumRgb(32));
    ASSERT_TRUE(PollUntilOnLoop([&]() { return FakeDisplayEngine().GetClampRgbValue() == 32; }));
  }

  // Create a primary client
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));
  // Clamp RGB to a new value
  ASSERT_OK(primary_client->SetMinimumRgb(1));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return FakeDisplayEngine().GetClampRgbValue() == 1; }));

  // Close the primary client, wait for the virtcon client to regain display
  // ownership.
  primary_client.reset(nullptr);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_virtcon_color_layer_result =
      virtcon_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_virtcon_color_layer_result);
  display::LayerId virtcon_color_layer_id = create_virtcon_color_layer_result.value();

  // Apply a config so the virtcon client's RGB clamp is set.
  static constexpr display::ConfigStamp kVirtconInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(virtcon_client->ApplyLayers(kVirtconInitialConfigStamp,
                                        {{.layer_id = virtcon_color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));

  TriggerDisplayEngineVsync();
  // TODO(https://fxbug.dev/388885807): This test is racy. There's no guarantee
  // that the VSync has reached the Display Coordinator. Figure out the right
  // contract for RGB clamp application, and write the test against that
  // contract.

  // make sure clamp value was restored
  ASSERT_TRUE(PollUntilOnLoop([&]() { return FakeDisplayEngine().GetClampRgbValue() == 32; }));
}

TEST_F(IntegrationTest, VsyncGoesToClientWhoAppliedConfig) {
  // Create and bind virtcon client.
  std::unique_ptr<TestFidlClient> virtcon_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kVirtcon);
  ASSERT_OK(virtcon_client->SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode::kFallback));
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_virtcon_color_layer_result =
      virtcon_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_virtcon_color_layer_result);
  display::LayerId virtcon_color_layer_id = create_virtcon_color_layer_result.value();

  static constexpr display::ConfigStamp kVirtconInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(virtcon_client->ApplyLayers(kVirtconInitialConfigStamp,
                                        {{.layer_id = virtcon_color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp virtcon_initial_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  // Create and bind primary client.
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  // The Virtcon client should receive VSync events while its config is applied.
  // This is the case until the primary client applies a config.
  EXPECT_EQ(0u, primary_client->state().vsync_count());
  ASSERT_EQ(0u, virtcon_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return virtcon_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kVirtconInitialConfigStamp, virtcon_client->state().last_vsync_config_stamp());
  EXPECT_EQ(0u, primary_client->state().vsync_count());
  EXPECT_EQ(1u, virtcon_client->state().vsync_count());

  // Present an image from the primary client.
  static constexpr display::ConfigStamp kPrimaryInitialConfigStamp(2);
  ASSERT_EQ(virtcon_initial_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kPrimaryInitialConfigStamp,
                                        primary_client->CreateFullscreenLayerConfig()));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > virtcon_initial_driver_config_stamp; }));

  // Primary client should receive VSync events after applying a config.
  ASSERT_EQ(0u, primary_client->state().vsync_count());
  EXPECT_EQ(1u, virtcon_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kPrimaryInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());
  EXPECT_EQ(1u, virtcon_client->state().vsync_count());
}

// This test case covers the basic interaction between ApplyConfig() and VSync
// events.
//
// The test uses configurations with images without any wait fences. These
// images are ready for use when the Coordinator receives the ApplyConfig()
// call. In this case, each VSync event should report the ConfigStamp used
// in the last ApplyConfig() call.
//
// Here we test the following case:
//
//  * ApplyConfig({layer1: image1}) ==> kNoFence1ConfigStamp
//  - Vsync now should have kNoFence1ConfigStamp
//  * ApplyConfig({layer1: image2}) ==> kNoFence2ConfigStamp
//  - Vsync now should have kNoFence2ConfigStamp
//  * ApplyConfig({}) ==> kNoImageConfigStamp
//  - Vsync now should have kNoImageConfigStamp
TEST_F(IntegrationTest, VsyncReflectsAppliedConfig) {
  // Create and bind primary client.
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp initial_driver_config_stamp = DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  zx::result<display::LayerId> create_layer1_result = primary_client->CreateFullscreenImageLayer();
  zx::result<display::ImageId> create_image1_result = primary_client->CreateFullscreenImage();
  zx::result<display::ImageId> create_image2_result = primary_client->CreateFullscreenImage();

  ASSERT_OK(create_layer1_result);
  ASSERT_OK(create_image1_result);
  ASSERT_OK(create_image2_result);

  display::LayerId layer1_id = create_layer1_result.value();
  display::ImageId image1_id = create_image1_result.value();
  display::ImageId image2_id = create_image2_result.value();

  // Present one single image without wait.
  static constexpr display::ConfigStamp kNoFence1ConfigStamp(2);
  ASSERT_EQ(initial_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kNoFence1ConfigStamp,
                                        {{.layer_id = layer1_id, .image_id = image1_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > initial_driver_config_stamp; }));
  const display::DriverConfigStamp no_fence1_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(1u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 2; }));
  EXPECT_EQ(kNoFence1ConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(2u, primary_client->state().vsync_count());

  // Present another image layer without a wait fence.
  static constexpr display::ConfigStamp kNoFence2ConfigStamp(3);
  ASSERT_EQ(no_fence1_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kNoFence2ConfigStamp,
                                        {{.layer_id = layer1_id, .image_id = image2_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > no_fence1_driver_config_stamp; }));
  const display::DriverConfigStamp no_fence2_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(2u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 3; }));
  EXPECT_EQ(kNoFence2ConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(3u, primary_client->state().vsync_count());

  // Hide the image layer and replace it with a color layer.
  static constexpr display::ConfigStamp kNoImageConfigStamp(4);
  ASSERT_EQ(no_fence2_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kNoImageConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > no_fence2_driver_config_stamp; }));

  ASSERT_EQ(3u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 4; }));
  EXPECT_EQ(kNoImageConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(4u, primary_client->state().vsync_count());
}

// This test case covers ApplyConfig() with configurations that include waiting
// images. This matches the usage pattern of Scenic with GPU composition.
//
// When applying configurations with waiting images, the ConfigStamp reported by
// VSync events should match the latest applied configuration that doesn't have
// any waiting image.
//
// Here we test the following case:
//
//  * ApplyConfig({layer1: image_without_fence}) ==> kImageWithoutFenceConfigStamp
//  - Vsync now should have kImageWithoutFenceConfigStamp
//  * ApplyConfig({layer1: image_with_fence}) ==> kImageWithFenceConfigStamp
//  - Vsync now should have kImageWithoutFenceConfigStamp
//  * Signal kImageWithoutFenceConfigStamp
//  - Vsync now should have kImageWithFenceConfigStamp
TEST_F(IntegrationTest, ApplyConfigWithWaitingImage) {
  // Create and bind primary client.
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp initial_driver_config_stamp = DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  zx::result<display::LayerId> create_layer1_result = primary_client->CreateFullscreenImageLayer();
  zx::result<display::ImageId> create_image_without_fence_result =
      primary_client->CreateFullscreenImage();
  zx::result<display::ImageId> create_image_with_fence_result =
      primary_client->CreateFullscreenImage();
  zx::result<TestFidlClient::EventInfo> create_image_ready_fence_result =
      primary_client->CreateEvent();

  ASSERT_OK(create_layer1_result);
  ASSERT_OK(create_image_without_fence_result);
  ASSERT_OK(create_image_with_fence_result);
  ASSERT_OK(create_image_ready_fence_result);

  display::LayerId layer1_id = create_layer1_result.value();
  display::ImageId image_without_fence_id = create_image_without_fence_result.value();
  display::ImageId image_with_fence_id = create_image_with_fence_result.value();
  TestFidlClient::EventInfo image_ready_fence = std::move(create_image_ready_fence_result.value());

  // Present one image layer without a wait event.
  static constexpr display::ConfigStamp kImageWithoutFenceConfigStamp(2);
  ASSERT_EQ(initial_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(
      primary_client->ApplyLayers(kImageWithoutFenceConfigStamp,
                                  {{.layer_id = layer1_id, .image_id = image_without_fence_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > initial_driver_config_stamp; }));
  const display::DriverConfigStamp image_without_fence_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(1u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 2; }));
  EXPECT_EQ(kImageWithoutFenceConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(2u, primary_client->state().vsync_count());

  // Present an image layer whose image is not ready yet. The Display
  // Coordinator must wait on the fence. VSync events must report the previous
  // configuration.
  static constexpr display::ConfigStamp kImageWithFenceConfigStamp(3);
  ASSERT_OK(primary_client->ApplyLayers(kImageWithFenceConfigStamp,
                                        {{.layer_id = layer1_id,
                                          .image_id = image_with_fence_id,
                                          .image_ready_wait_event_id = image_ready_fence.id}}));

  ASSERT_EQ(2u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 3; }));
  EXPECT_EQ(kImageWithoutFenceConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(3u, primary_client->state().vsync_count());

  // Signal the event, marking the image ready. The Coordinator must apply the
  // configuration kImageWithFenceConfigStamp, which includes the image that is
  // now ready. Once the configuration is applied, the next VSync must reflect
  // it.
  ASSERT_EQ(image_without_fence_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  image_ready_fence.event.signal(0u, ZX_EVENT_SIGNALED);
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return DisplayEngineAppliedConfigStamp() > image_without_fence_driver_config_stamp;
  }));

  ASSERT_EQ(3u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 4; }));
  EXPECT_EQ(kImageWithFenceConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(4u, primary_client->state().vsync_count());
}

// This test case covers ApplyConfig() when an applied configuration removes a layer
// with a waiting image from a previously applied configuration.
//
// VSync events should never include the ConfigStamp of the configuration with the
// waiting image, because that image never becomes ready.
//
// Here we test the following case:
//
//  * ApplyConfig({layer1: image_without_fence}) ==> kImageWithoutFenceConfigStamp
//  - Vsync now should have kImageWithoutFenceConfigStamp
//  * ApplyConfig({layerA: img1, waiting on fence}) ==> kImageWithFenceConfigStamp
//  - Vsync now should have kImageWithoutFenceConfigStamp
//  * ApplyConfig({}) ==> kNoImageConfigStamp
//  - Vsync now should have kNoImageConfigStamp
//
// The fence is never signaled.
TEST_F(IntegrationTest, ApplyConfigRemovesLayerWithWaitingImage) {
  // Create and bind primary client.
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_color_layer_result =
      primary_client->CreateFullscreenColorLayer(kFuchsiaBgra);
  ASSERT_OK(create_color_layer_result);
  display::LayerId color_layer_id = create_color_layer_result.value();

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kInitialConfigStamp(1);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kInitialConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp initial_driver_config_stamp = DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kInitialConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  zx::result<display::LayerId> create_layer1_result = primary_client->CreateFullscreenImageLayer();
  zx::result<display::ImageId> create_image_without_fence_result =
      primary_client->CreateFullscreenImage();
  zx::result<display::ImageId> create_image_with_fence_result =
      primary_client->CreateFullscreenImage();
  zx::result<TestFidlClient::EventInfo> create_image_ready_fence_result =
      primary_client->CreateEvent();

  ASSERT_OK(create_layer1_result);
  ASSERT_OK(create_image_without_fence_result);
  ASSERT_OK(create_image_with_fence_result);
  ASSERT_OK(create_image_ready_fence_result);

  display::LayerId layer1_id = create_layer1_result.value();
  display::ImageId image_without_fence_id = create_image_without_fence_result.value();
  display::ImageId image_with_fence_id = create_image_with_fence_result.value();
  TestFidlClient::EventInfo image_ready_fence = std::move(create_image_ready_fence_result.value());

  // Present an image layer.
  static constexpr display::ConfigStamp kImageWithoutFenceConfigStamp(2);
  ASSERT_EQ(initial_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(
      primary_client->ApplyLayers(kImageWithoutFenceConfigStamp,
                                  {{.layer_id = layer1_id, .image_id = image_without_fence_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() > initial_driver_config_stamp; }));
  const display::DriverConfigStamp image_without_fence_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(1u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 2; }));
  EXPECT_EQ(kImageWithoutFenceConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(2u, primary_client->state().vsync_count());

  // Present an image layer whose image is not ready yet. The Display
  // Coordinator must wait on the event. VSync events must report the previous
  // configuration.
  static constexpr display::ConfigStamp kImageWithFenceConfigStamp(3);
  ASSERT_EQ(image_without_fence_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kImageWithFenceConfigStamp,
                                        {{.layer_id = layer1_id,
                                          .image_id = image_with_fence_id,
                                          .image_ready_wait_event_id = image_ready_fence.id}}));

  ASSERT_EQ(2u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 3; }));
  EXPECT_EQ(kImageWithoutFenceConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(3u, primary_client->state().vsync_count());

  // Replace the image layer with a solid color fill layer. The Coordinator must
  // "skip over" the image layer that is not ready, and apply the configuration
  // with the color layer.
  static constexpr display::ConfigStamp kNoImageConfigStamp(4);
  ASSERT_EQ(image_without_fence_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(primary_client->ApplyLayers(kNoImageConfigStamp, {{.layer_id = color_layer_id}}));
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return DisplayEngineAppliedConfigStamp() > image_without_fence_driver_config_stamp;
  }));

  // On Vsync, the configuration stamp client receives on Vsync event message
  // will be the latest one applied to the display controller, since the waiting
  // image has been removed from the configuration.
  ASSERT_EQ(3u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 4; }));
  EXPECT_EQ(kNoImageConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(4u, primary_client->state().vsync_count());
}

// This test case covers ApplyConfig() assigning two different waiting images to
// the same layer in two different applied configs. The second image becomes
// ready at some point, while the first image remains waiting forever.
//
// VSync events should never include the ConfigStamp of the configuration with
// the first waiting image. After the second image's waiting fence is signaled,
// the ConfigStamp for the configuration using that second image should be
// included in VSync events.
//
// Here we test the following case:
//
//  * ApplyConfig({layer1: image_without_fence}) ==> kImageWithoutFenceConfigStamp
//  - Vsync now should have kImageWithoutFenceConfigStamp
//  * ApplyConfig({layer1: image_with_fence1, waiting on fence1}) ==> kImageWithFence1ConfigStamp
//  - Vsync now should have kImageWithoutFenceConfigStamp since fence1 is not signaled
//  * ApplyConfig({layerA: image_with_fence2, waiting on fence2}) ==> kImageWithFence2ConfigStamp
//  - Vsync now should have kImageWithoutFenceConfigStamp since fence1 and fence2 are not
//  signaled
//  * Signal fence2
//  - Vsync now should have kImageWithFence2ConfigStamp.
//  * Signal fence1
//  - Vsync should still have kImageWithFence2ConfigStamp.
//
// fence1, the first fence, is never signaled.
TEST_F(IntegrationTest, ApplyConfigSkipsConfigWithWaitingImage) {
  // Create and bind primary client.
  std::unique_ptr<TestFidlClient> primary_client = OpenCoordinatorTestFidlClient(
      &sysmem_client_, DisplayProviderClient(), ClientPriority::kPrimary);
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().has_display_ownership(); }));

  zx::result<display::LayerId> create_layer1_result = primary_client->CreateFullscreenImageLayer();
  zx::result<display::ImageId> create_image_without_fence_result =
      primary_client->CreateFullscreenImage();
  zx::result<display::ImageId> create_image_with_fence1_result =
      primary_client->CreateFullscreenImage();
  zx::result<display::ImageId> create_image_with_fence2_result =
      primary_client->CreateFullscreenImage();
  zx::result<TestFidlClient::EventInfo> create_image_ready_fence1_result =
      primary_client->CreateEvent();
  zx::result<TestFidlClient::EventInfo> create_image_ready_fence2_result =
      primary_client->CreateEvent();

  ASSERT_OK(create_layer1_result);
  ASSERT_OK(create_image_without_fence_result);
  ASSERT_OK(create_image_with_fence1_result);
  ASSERT_OK(create_image_with_fence2_result);
  ASSERT_OK(create_image_ready_fence1_result);
  ASSERT_OK(create_image_ready_fence2_result);

  display::LayerId layer1_id = create_layer1_result.value();
  display::ImageId image_without_fence_id = create_image_without_fence_result.value();
  display::ImageId image_with_fence1_id = create_image_with_fence1_result.value();
  display::ImageId image_with_fence2_id = create_image_with_fence2_result.value();
  TestFidlClient::EventInfo image_ready_fence1 =
      std::move(create_image_ready_fence1_result.value());
  TestFidlClient::EventInfo image_ready_fence2 =
      std::move(create_image_ready_fence2_result.value());

  // Apply a config so the client starts receiving VSync events.
  static constexpr display::ConfigStamp kImageWithoutFenceConfigStamp(2);
  ASSERT_EQ(display::kInvalidDriverConfigStamp, DisplayEngineAppliedConfigStamp());
  ASSERT_OK(
      primary_client->ApplyLayers(kImageWithoutFenceConfigStamp,
                                  {{.layer_id = layer1_id, .image_id = image_without_fence_id}}));
  ASSERT_TRUE(PollUntilOnLoop(
      [&]() { return DisplayEngineAppliedConfigStamp() != display::kInvalidDriverConfigStamp; }));
  const display::DriverConfigStamp image_without_fence_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(0u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 1; }));
  EXPECT_EQ(kImageWithoutFenceConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(1u, primary_client->state().vsync_count());

  // Present an image layer whose image is not ready yet. The Display
  // Coordinator must wait on the event. VSync events must report the previous
  // configuration.
  static constexpr display::ConfigStamp kImageWithFence1ConfigStamp(3);
  ASSERT_OK(primary_client->ApplyLayers(kImageWithFence1ConfigStamp,
                                        {{.layer_id = layer1_id,
                                          .image_id = image_with_fence1_id,
                                          .image_ready_wait_event_id = image_ready_fence1.id}}));

  ASSERT_EQ(1u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 2; }));
  EXPECT_EQ(kImageWithoutFenceConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(2u, primary_client->state().vsync_count());

  // Present another image layer whose image is not ready. By the same
  // reasoning as above, VSync events must still report the same
  // configuration as above.
  static constexpr display::ConfigStamp kImageWithFence2ConfigStamp(4);
  ASSERT_OK(primary_client->ApplyLayers(kImageWithFence2ConfigStamp,
                                        {{.layer_id = layer1_id,
                                          .image_id = image_with_fence2_id,
                                          .image_ready_wait_event_id = image_ready_fence2.id}}));

  ASSERT_EQ(2u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 3; }));
  EXPECT_EQ(kImageWithoutFenceConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(3u, primary_client->state().vsync_count());

  // Signal the second image's event. The Coordinator must "skip over" the
  // configuration that includes the first image, and apply the configuration
  // that includes the second image. Once the configuration is applied, the next
  // VSync must reflect it.
  ASSERT_EQ(image_without_fence_driver_config_stamp, DisplayEngineAppliedConfigStamp());
  image_ready_fence2.event.signal(0u, ZX_EVENT_SIGNALED);
  ASSERT_TRUE(PollUntilOnLoop([&]() {
    return DisplayEngineAppliedConfigStamp() > image_without_fence_driver_config_stamp;
  }));
  const display::DriverConfigStamp image_with_fence2_driver_config_stamp =
      DisplayEngineAppliedConfigStamp();

  ASSERT_EQ(3u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 4; }));
  EXPECT_EQ(kImageWithFence2ConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(4u, primary_client->state().vsync_count());

  // Signal the first image's event. Since we have displayed a newer image, signaling the
  // old event associated with the old image shouldn't trigger ReapplyConfig().
  // We should still see |apply_config_stamp_2| as the latest presented config
  // stamp in the client.
  image_ready_fence1.event.signal(0u, ZX_EVENT_SIGNALED);

  // TODO(https://fxbug.dev/388885807): This check can have a false positive
  // pass, due to using a hard-coded timeout.
  {
    zx::time deadline = zx::deadline_after(zx::sec(1));
    PollUntilOnLoop([&]() {
      if (zx::clock::get_monotonic() >= deadline)
        return true;
      return DisplayEngineAppliedConfigStamp() > image_with_fence2_driver_config_stamp;
    });
  }
  EXPECT_EQ(image_with_fence2_driver_config_stamp, DisplayEngineAppliedConfigStamp());

  ASSERT_EQ(4u, primary_client->state().vsync_count());
  TriggerDisplayEngineVsync();
  ASSERT_TRUE(PollUntilOnLoop([&]() { return primary_client->state().vsync_count() >= 5; }));
  EXPECT_EQ(kImageWithFence2ConfigStamp, primary_client->state().last_vsync_config_stamp());
  EXPECT_EQ(5u, primary_client->state().vsync_count());
}

// TODO(https://fxbug.dev/42171874): Currently the fake-display driver only supports one
// primary layer. In order to better test ApplyConfig() / OnVsync() behavior,
// we should make fake-display driver support multi-layer configurations and
// then we could add more multi-layer tests.

}  // namespace display_coordinator
