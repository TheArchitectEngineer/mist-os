// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CLIENT_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CLIENT_H_

#include <fidl/fuchsia.hardware.display.types/cpp/wire.h>
#include <fidl/fuchsia.hardware.display/cpp/wire.h>
#include <fidl/fuchsia.sysmem2/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/function.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/sync/completion.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <cstdint>
#include <list>
#include <map>
#include <memory>
#include <type_traits>
#include <variant>
#include <vector>

#include <fbl/auto_lock.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/ref_ptr.h>
#include <fbl/ring_buffer.h>
#include <fbl/vector.h>

#include "src/graphics/display/drivers/coordinator/capture-image.h"
#include "src/graphics/display/drivers/coordinator/client-id.h"
#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/coordinator/controller.h"
#include "src/graphics/display/drivers/coordinator/fence.h"
#include "src/graphics/display/drivers/coordinator/id-map.h"
#include "src/graphics/display/drivers/coordinator/image.h"
#include "src/graphics/display/drivers/coordinator/layer.h"
#include "src/graphics/display/lib/api-types/cpp/buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/buffer-id.h"
#include "src/graphics/display/lib/api-types/cpp/config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-layer-id.h"
#include "src/graphics/display/lib/api-types/cpp/event-id.h"
#include "src/graphics/display/lib/api-types/cpp/image-id.h"
#include "src/graphics/display/lib/api-types/cpp/layer-id.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"
#include "src/graphics/display/lib/api-types/cpp/vsync-ack-cookie.h"

namespace display_coordinator {

// Almost-POD used by Client to manage display configuration. Public state is used by Controller.
class DisplayConfig : public IdMappable<std::unique_ptr<DisplayConfig>, display::DisplayId> {
 public:
  explicit DisplayConfig(display::DisplayId display_id);

  DisplayConfig(const DisplayConfig&) = delete;
  DisplayConfig& operator=(const DisplayConfig&) = delete;
  DisplayConfig(DisplayConfig&&) = delete;
  DisplayConfig& operator=(DisplayConfig&&) = delete;

  ~DisplayConfig();

  void InitializeInspect(inspect::Node* parent);

  bool apply_layer_change() {
    bool ret = pending_apply_layer_change_;
    pending_apply_layer_change_ = false;
    pending_apply_layer_change_property_.Set(false);
    return ret;
  }

  // Discards all the draft changes (except for draft layers lists)
  // of a Display's `config`.
  //
  // The display draft layers' draft configs must be discarded before
  // `DiscardNonLayerDraftConfig()` is called.
  void DiscardNonLayerDraftConfig();

  int applied_layer_count() const { return static_cast<int>(applied_.layer_count); }
  const display_config_t* applied_config() const { return &applied_; }
  const fbl::DoublyLinkedList<LayerNode*>& get_applied_layers() const { return applied_layers_; }

 private:
  // The last configuration sent to the display engine.
  display_config_t applied_;

  // The display configuration modified by client calls.
  display_config_t draft_;

  // If true, the draft configuration's layer list may differ from the current
  // configuration's list.
  bool draft_has_layer_list_change_ = false;

  bool pending_apply_layer_change_ = false;
  fbl::DoublyLinkedList<LayerNode*> draft_layers_;
  fbl::DoublyLinkedList<LayerNode*> applied_layers_;

  fbl::Vector<display::PixelFormat> pixel_formats_;

  bool has_draft_nonlayer_config_change_ = false;

  friend Client;
  friend ClientProxy;

  inspect::Node node_;
  // Reflects `draft_has_layer_list_change_`.
  inspect::BoolProperty draft_has_layer_list_change_property_;
  // Reflects `pending_apply_layer_change_`.
  inspect::BoolProperty pending_apply_layer_change_property_;
};

// Manages the state associated with a display coordinator client connection.
//
// This class is not thread-safe. After initialization, all methods must be
// executed on the same thread.
class Client final : public fidl::WireServer<fuchsia_hardware_display::Coordinator> {
 public:
  // `controller` must outlive both this client and `proxy`.
  Client(Controller* controller, ClientProxy* proxy, ClientPriority priority, ClientId client_id);

  Client(const Client&) = delete;
  Client& operator=(const Client&) = delete;

  ~Client() override;

  // Binds the `Client` to the server-side channel of the `Coordinator`
  // protocol.
  //
  // Must be called exactly once in production code.
  //
  // `coordinator_server_end` and `coordinator_listener_client_end` must be valid.
  void Bind(fidl::ServerEnd<fuchsia_hardware_display::Coordinator> coordinator_server_end,
            fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener>
                coordinator_listener_client_end,
            fidl::OnUnboundFn<Client> unbound_callback);

  void OnDisplaysChanged(std::span<const display::DisplayId> added_display_ids,
                         std::span<const display::DisplayId> removed_display_ids);
  void SetOwnership(bool is_owner);

  fidl::Status NotifyDisplayChanges(
      std::span<const fuchsia_hardware_display::wire::Info> added_display_infos,
      std::span<const fuchsia_hardware_display_types::wire::DisplayId> removed_display_ids);
  fidl::Status NotifyOwnershipChange(bool client_has_ownership);
  fidl::Status NotifyVsync(display::DisplayId display_id, zx::time timestamp,
                           display::ConfigStamp config_stamp,
                           display::VsyncAckCookie vsync_ack_cookie);

  void ApplyConfig();
  void ReapplyConfig();

  void OnFenceFired(FenceReference* fence);

  void TearDown(zx_status_t epitaph);
  void TearDownForTesting();

  bool IsValid() const { return valid_; }
  ClientId id() const { return id_; }
  ClientPriority priority() const { return priority_; }
  void CaptureCompleted();

  uint8_t GetMinimumRgb() const { return client_minimum_rgb_; }

  display::VsyncAckCookie LastAckedCookie() const { return acked_cookie_; }

  size_t ImportedImagesCountForTesting() const { return images_.size(); }

  // fidl::WireServer<fuchsia_hardware_display::Coordinator> overrides:
  void ImportImage(ImportImageRequestView request, ImportImageCompleter::Sync& _completer) override;
  void ReleaseImage(ReleaseImageRequestView request,
                    ReleaseImageCompleter::Sync& _completer) override;
  void ImportEvent(ImportEventRequestView request, ImportEventCompleter::Sync& _completer) override;
  void ReleaseEvent(ReleaseEventRequestView request,
                    ReleaseEventCompleter::Sync& _completer) override;
  void CreateLayer(CreateLayerCompleter::Sync& _completer) override;
  void DestroyLayer(DestroyLayerRequestView request,
                    DestroyLayerCompleter::Sync& _completer) override;
  void SetDisplayMode(SetDisplayModeRequestView request,
                      SetDisplayModeCompleter::Sync& _completer) override;
  void SetDisplayColorConversion(SetDisplayColorConversionRequestView request,
                                 SetDisplayColorConversionCompleter::Sync& _completer) override;
  void SetDisplayLayers(SetDisplayLayersRequestView request,
                        SetDisplayLayersCompleter::Sync& _completer) override;
  void SetLayerPrimaryConfig(SetLayerPrimaryConfigRequestView request,
                             SetLayerPrimaryConfigCompleter::Sync& _completer) override;
  void SetLayerPrimaryPosition(SetLayerPrimaryPositionRequestView request,
                               SetLayerPrimaryPositionCompleter::Sync& _completer) override;
  void SetLayerPrimaryAlpha(SetLayerPrimaryAlphaRequestView request,
                            SetLayerPrimaryAlphaCompleter::Sync& _completer) override;
  void SetLayerColorConfig(SetLayerColorConfigRequestView request,
                           SetLayerColorConfigCompleter::Sync& _completer) override;
  void SetLayerImage2(SetLayerImage2RequestView request,
                      SetLayerImage2Completer::Sync& _completer) override;
  void CheckConfig(CheckConfigRequestView request, CheckConfigCompleter::Sync& _completer) override;
  void DiscardConfig(DiscardConfigCompleter::Sync& _completer) override;
  void ApplyConfig3(ApplyConfig3RequestView request,
                    ApplyConfig3Completer::Sync& _completer) override;
  void GetLatestAppliedConfigStamp(GetLatestAppliedConfigStampCompleter::Sync& _completer) override;
  void SetVsyncEventDelivery(SetVsyncEventDeliveryRequestView request,
                             SetVsyncEventDeliveryCompleter::Sync& _completer) override;
  void SetVirtconMode(SetVirtconModeRequestView request,
                      SetVirtconModeCompleter::Sync& _completer) override;
  void ImportBufferCollection(ImportBufferCollectionRequestView request,
                              ImportBufferCollectionCompleter::Sync& _completer) override;
  void SetBufferCollectionConstraints(
      SetBufferCollectionConstraintsRequestView request,
      SetBufferCollectionConstraintsCompleter::Sync& _completer) override;
  void ReleaseBufferCollection(ReleaseBufferCollectionRequestView request,
                               ReleaseBufferCollectionCompleter::Sync& _completer) override;

  void IsCaptureSupported(IsCaptureSupportedCompleter::Sync& _completer) override;

  void StartCapture(StartCaptureRequestView request,
                    StartCaptureCompleter::Sync& _completer) override;

  void AcknowledgeVsync(AcknowledgeVsyncRequestView request,
                        AcknowledgeVsyncCompleter::Sync& _completer) override;

  void SetMinimumRgb(SetMinimumRgbRequestView request,
                     SetMinimumRgbCompleter::Sync& _completer) override;

  void SetDisplayPower(SetDisplayPowerRequestView request,
                       SetDisplayPowerCompleter::Sync& _completer) override;

 private:
  // Cleans up states of all current Images.
  // Returns true if any current layer has been modified.
  bool CleanUpAllImages();

  // Cleans up layer state associated with an Image. `image` must be valid.
  // Returns true if a current layer has been modified.
  bool CleanUpImage(Image& image);
  void CleanUpCaptureImage(display::ImageId id);

  // Displays' draft layers list may have been changed by SetDisplayLayers().
  //
  // Restores the draft layer lists of all the displays to their applied layer
  // list state respectively, undoing all draft changes to the layer lists.
  void SetAllConfigDraftLayersToAppliedLayers();

  // `fuchsia.hardware.display/Coordinator.ImportImage()` helper for display
  // images.
  //
  // `image_id` must be unused and `image_metadata` contains metadata for an
  // image used for display.
  zx_status_t ImportImageForDisplay(const display::ImageMetadata& image_metadata,
                                    display::BufferId buffer_id, display::ImageId image_id);

  // `fuchsia.hardware.display/Coordinator.ImportImage()` helper for capture
  // images.
  //
  // `image_id` must be unused and `image_metadata` contains metadata for an
  // image used for capture.
  zx_status_t ImportImageForCapture(const display::ImageMetadata& image_metadata,
                                    display::BufferId buffer_id, display::ImageId image_id);

  // Discards all the draft configs on all displays and layers.
  void DiscardConfig();

  void SetLayerImageImpl(display::LayerId layer_id, display::ImageId image_id,
                         display::EventId wait_event_id);

  Controller& controller_;
  ClientProxy* const proxy_;
  const ClientPriority priority_;
  const ClientId id_;
  bool valid_ = false;

  Image::Map images_;
  CaptureImage::Map capture_images_;

  // Maps each known display ID to this client's display config.
  //
  // The client's knowledge of the connected displays can fall out of sync with
  // this map. This is because the map is modified when the Coordinator
  // processes display change events from display engine drivers, which happens
  // before the client receives the display driver.
  DisplayConfig::Map display_configs_;

  // True iff CheckConfig() succeeded on the draft configuration.
  //
  // Set to false any time when the client modifies the draft configuration. Set
  // to true when the client calls CheckConfig() and the check passes.
  bool draft_display_config_was_validated_ = false;

  bool is_owner_ = false;

  // A counter for the number of times the client has successfully applied
  // a configuration. This does not account for changes due to waiting images.
  display::ConfigStamp latest_config_stamp_ = display::kInvalidConfigStamp;

  // This is the client's clamped RGB value.
  uint8_t client_minimum_rgb_ = 0;

  struct Collections {
    // The BufferCollection ID used in fuchsia.hardware.display.Controller
    // protocol.
    display::DriverBufferCollectionId driver_buffer_collection_id;
  };
  std::map<display::BufferCollectionId, Collections> collection_map_;

  FenceCollection fences_;

  Layer::Map layers_;

  // TODO(fxbug.com/129082): Move to Controller, so values issued using this
  // counter are globally unique. Do not pass to display::DriverLayerId values to drivers
  // until this issue is fixed.
  display::DriverLayerId next_driver_layer_id = display::DriverLayerId(1);

  void NotifyDisplaysChanged(const int32_t* displays_added, uint32_t added_count,
                             const int32_t* displays_removed, uint32_t removed_count);
  bool CheckConfig(fuchsia_hardware_display_types::wire::ConfigResult* res,
                   std::vector<fuchsia_hardware_display::wire::ClientCompositionOp>* ops);

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_display::Coordinator>> binding_;
  fidl::WireSharedClient<fuchsia_hardware_display::CoordinatorListener> coordinator_listener_;

  // Capture related bookkeeping.
  display::EventId capture_fence_id_ = display::kInvalidEventId;

  // Points to the image whose contents is modified by the current capture.
  //
  // Invalid when no is capture in progress.
  display::ImageId current_capture_image_id_ = display::kInvalidImageId;

  // Tracks an image released by the client while used by a capture.
  //
  // The coordinator must ensure that an image remains valid while a display
  // engine is writing to it. If a client attempts to release the image used by
  // an in-progress capture, we defer the release operation until the capture
  // completes. The deferred release is tracked here.
  display::ImageId pending_release_capture_image_id_ = display::kInvalidImageId;

  display::VsyncAckCookie acked_cookie_ = display::kInvalidVsyncAckCookie;
};

// ClientProxy manages interactions between its Client instance and the
// controller. Methods on this class are thread safe.
class ClientProxy {
 public:
  // `client_id` is assigned by the Controller to distinguish clients.
  // `controller` must outlive ClientProxy.
  ClientProxy(Controller* controller, ClientPriority client_priority, ClientId client_id,
              fit::function<void()> on_client_disconnected);

  ~ClientProxy();

  zx_status_t Init(inspect::Node* parent_node,
                   fidl::ServerEnd<fuchsia_hardware_display::Coordinator> server_end,
                   fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener>
                       coordinator_listener_client_end);

  zx::result<> InitForTesting(fidl::ServerEnd<fuchsia_hardware_display::Coordinator> server_end,
                              fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener>
                                  coordinator_listener_client_end);

  // Schedule a task on the controller loop to close this ClientProxy and
  // have it be freed.
  void CloseOnControllerLoop();

  // Requires holding `controller_.mtx()` lock.
  zx_status_t OnDisplayVsync(display::DisplayId display_id, zx_time_t timestamp,
                             display::DriverConfigStamp driver_config_stamp);
  void OnDisplaysChanged(std::span<const display::DisplayId> added_display_ids,
                         std::span<const display::DisplayId> removed_display_ids);
  void SetOwnership(bool is_owner);
  void ReapplyConfig();
  zx_status_t OnCaptureComplete();

  void SetVsyncEventDelivery(bool vsync_delivery_enabled) {
    fbl::AutoLock lock(&mtx_);
    vsync_delivery_enabled_ = vsync_delivery_enabled;
  }

  void EnableCapture(bool enable) {
    fbl::AutoLock lock(&mtx_);
    enable_capture_ = enable;
  }
  void OnClientDead();

  // This function restores client configurations that are not part of
  // the standard configuration. These configurations are typically one-time
  // settings that need to get restored once the client takes control again.
  void ReapplySpecialConfigs();

  ClientId client_id() const { return handler_.id(); }
  ClientPriority client_priority() const { return handler_.priority(); }

  inspect::Node& node() { return node_; }

  struct ConfigStampPair {
    display::DriverConfigStamp driver_stamp;
    display::ConfigStamp client_stamp;
  };
  std::list<ConfigStampPair>& pending_applied_config_stamps() {
    return pending_applied_config_stamps_;
  }

  // Add a new mapping entry from `stamps.controller_stamp` to `stamp.config_stamp`.
  // Controller should guarantee that `stamps.controller_stamp` is strictly
  // greater than existing pending controller stamps.
  void UpdateConfigStampMapping(ConfigStampPair stamps);

  void CloseForTesting();

  display::VsyncAckCookie LastVsyncAckCookieForTesting();

  // Fired after the FIDL client is unbound.
  sync_completion_t* FidlUnboundCompletionForTesting();

  size_t ImportedImagesCountForTesting() const { return handler_.ImportedImagesCountForTesting(); }

  // Define these constants here so we can access them in tests.

  static constexpr uint32_t kVsyncBufferSize = 10;

  // Maximum number of vsync messages sent before an acknowledgement is required.
  // Half of this limit is provided to clients as part of display info. Assuming a
  // frame rate of 60hz, clients will be required to acknowledge at least once a second
  // and driver will stop sending messages after 2 seconds of no acknowledgement
  static constexpr uint32_t kMaxVsyncMessages = 120;
  static constexpr uint32_t kVsyncMessagesWatermark = (kMaxVsyncMessages / 2);
  // At the moment, maximum image handles returned by any driver is 4 which is
  // equal to number of hardware layers. 8 should be more than enough to allow for
  // a simple statically allocated array of image_ids for vsync events that are being
  // stored due to client non-acknowledgement.
  static constexpr uint32_t kMaxImageHandles = 8;

 private:
  fbl::Mutex mtx_;
  Controller& controller_;

  Client handler_;
  bool vsync_delivery_enabled_ __TA_GUARDED(&mtx_) = false;
  bool enable_capture_ __TA_GUARDED(&mtx_) = false;

  fbl::Mutex task_mtx_;
  std::vector<std::unique_ptr<async::Task>> client_scheduled_tasks_ __TA_GUARDED(task_mtx_);

  // This variable is used to limit the number of errors logged in case of channel OOM error.
  static constexpr uint32_t kChannelOomPrintFreq = 600;  // 1 per 10 seconds (assuming 60fps)
  uint32_t chn_oom_print_freq_ = 0;
  uint64_t total_oom_errors_ = 0;

  struct VsyncMessageData {
    display::DisplayId display_id;
    zx_time_t timestamp;
    display::ConfigStamp config_stamp;
  };

  fbl::RingBuffer<VsyncMessageData, kVsyncBufferSize> buffered_vsync_messages_;
  display::VsyncAckCookie initial_cookie_ = display::VsyncAckCookie(0);
  display::VsyncAckCookie cookie_sequence_ = display::VsyncAckCookie(0);

  uint64_t number_of_vsyncs_sent_ = 0;
  display::VsyncAckCookie last_cookie_sent_ = display::kInvalidVsyncAckCookie;
  bool acknowledge_request_sent_ = false;

  fit::function<void()> on_client_disconnected_;

  // Fired when the FIDL connection is unbound.
  //
  // This member is thread-safe.
  sync_completion_t fidl_unbound_completion_;

  // Mapping from controller_stamp to client_stamp for all configurations that
  // are already applied and pending to be presented on the display.
  // Ordered by `controller_stamp_` in increasing order.
  std::list<ConfigStampPair> pending_applied_config_stamps_;

  inspect::Node node_;
  inspect::BoolProperty is_owner_property_;
};

}  // namespace display_coordinator

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CLIENT_H_
