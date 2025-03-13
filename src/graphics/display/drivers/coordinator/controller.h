// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CONTROLLER_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CONTROLLER_H_

#include <fidl/fuchsia.hardware.display/cpp/wire.h>
#include <fuchsia/hardware/audiotypes/c/banjo.h>
#include <fuchsia/hardware/display/controller/cpp/banjo.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/channel.h>
#include <lib/zx/time.h>
#include <lib/zx/vmo.h>
#include <threads.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <cstdint>
#include <cstdlib>
#include <list>
#include <memory>
#include <span>

#include <fbl/mutex.h>
#include <fbl/vector.h>

#include "src/graphics/display/drivers/coordinator/added-display-info.h"
#include "src/graphics/display/drivers/coordinator/capture-image.h"
#include "src/graphics/display/drivers/coordinator/client-id.h"
#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/coordinator/display-info.h"
#include "src/graphics/display/drivers/coordinator/engine-driver-client.h"
#include "src/graphics/display/drivers/coordinator/id-map.h"
#include "src/graphics/display/drivers/coordinator/image.h"
#include "src/graphics/display/drivers/coordinator/vsync-monitor.h"
#include "src/graphics/display/lib/api-types/cpp/config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/display-timing.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-capture-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/engine-info.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"

namespace display_coordinator {

class ClientProxy;
class Controller;
class ControllerTest;
class DisplayConfig;
class IntegrationTest;

// Multiplexes between display controller clients and display engine drivers.
class Controller : public ddk::DisplayEngineListenerProtocol<Controller>,
                   public fidl::WireServer<fuchsia_hardware_display::Provider> {
 public:
  // Factory method for production use.
  // Creates and initializes a Controller instance.
  //
  // Asynchronous work that manages the state of the display clients and
  // coordinates the display state between clients and engine drivers runs on
  // `client_dispatcher`.
  //
  // `engine_driver_client` must not be null.
  //
  // `client_dispatcher` must be running until `PrepareStop()` is called.
  // `client_dispatcher` must be shut down when `Stop()` is called.
  static zx::result<std::unique_ptr<Controller>> Create(
      std::unique_ptr<EngineDriverClient> engine_driver_client,
      fdf::UnownedSynchronizedDispatcher client_dispatcher);

  // Creates a new coordinator Controller instance. It creates a new Inspector
  // which will be solely owned by the Controller instance.
  //
  // `engine_driver_client` must not be null.
  explicit Controller(std::unique_ptr<EngineDriverClient> engine_driver_client,
                      fdf::UnownedSynchronizedDispatcher client_dispatcher);

  // Creates a new coordinator Controller instance with an injected `inspector`.
  // The `inspector` and inspect data may be duplicated and shared.
  //
  // `engine_driver_client` must not be null.
  Controller(std::unique_ptr<EngineDriverClient> engine_driver_client,
             fdf::UnownedSynchronizedDispatcher dispatcher, inspect::Inspector inspector);

  Controller(const Controller&) = delete;
  Controller& operator=(const Controller&) = delete;

  ~Controller() override;

  // References the `PrepareStop()` method in the DFv2 (fdf::DriverBase) driver
  // lifecycle.
  void PrepareStop();

  // References the `Stop()` method in the DFv2 (fdf::DriverBase) driver
  // lifecycle.
  //
  // Must be called after `client_dispatcher_` is shut down.
  void Stop();

  // fuchsia.hardware.display.controller/DisplayEngineListener:
  void DisplayEngineListenerOnDisplayAdded(const raw_display_info_t* banjo_display_info);
  void DisplayEngineListenerOnDisplayRemoved(uint64_t display_id);
  void DisplayEngineListenerOnDisplayVsync(uint64_t banjo_display_id, zx_time_t timestamp,
                                           const config_stamp_t* config_stamp);
  void DisplayEngineListenerOnCaptureComplete();

  void OnClientDead(ClientProxy* client);
  void SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode virtcon_mode);

  void ApplyConfig(std::span<DisplayConfig*> display_configs,
                   display::ConfigStamp client_config_stamp, uint32_t layer_stamp,
                   ClientId client_id) __TA_EXCLUDES(mtx());

  void ReleaseImage(display::DriverImageId driver_image_id);
  void ReleaseCaptureImage(display::DriverCaptureImageId driver_capture_image_id);

  // On success, the span of DisplayTiming objects is guaranteed to be
  // non-empty.
  //
  // The display timings are guaranteed to be valid as long as the display with
  // `display_id` is valid.
  //
  // `mtx()` must be held for as long as the return value is retained.
  zx::result<std::span<const display::DisplayTiming>> GetDisplayTimings(
      display::DisplayId display_id) __TA_REQUIRES(mtx());

  zx::result<fbl::Vector<display::PixelFormat>> GetSupportedPixelFormats(
      display::DisplayId display_id) __TA_REQUIRES(mtx());

  // Calls `callback` with a const DisplayInfo& matching the given `display_id`.
  //
  // Returns true iff a DisplayInfo with `display_id` was found and `callback`
  // was called.
  //
  // The controller mutex is guaranteed to be held while `callback` is called.
  template <typename Callback>
  bool FindDisplayInfo(display::DisplayId display_id, Callback callback) __TA_REQUIRES(mtx());

  EngineDriverClient* engine_driver_client() { return engine_driver_client_.get(); }

  bool supports_capture() { return engine_info_->is_capture_supported(); }

  fdf::UnownedSynchronizedDispatcher client_dispatcher() const {
    return client_dispatcher_->borrow();
  }
  bool IsRunningOnClientDispatcher() {
    return fdf::Dispatcher::GetCurrent()->get() == client_dispatcher_->get();
  }

  // Thread-safety annotations currently don't deal with pointer aliases. Use this to document
  // places where we believe a mutex aliases mtx()
  void AssertMtxAliasHeld(fbl::Mutex& m) __TA_ASSERT(m) { ZX_DEBUG_ASSERT(&m == mtx()); }
  fbl::Mutex* mtx() const { return &mtx_; }
  const inspect::Inspector& inspector() const { return inspector_; }

  size_t ImportedImagesCountForTesting() const;

  // Identifies the most recent completely applied display configuration.
  //
  // The returned stamp is updated after the display engine driver acknowledges
  // having applied the configuration.
  display::DriverConfigStamp last_applied_driver_config_stamp() const;

  // Typically called by OpenController/OpenVirtconController. However, this is made public
  // for use by testing services which provide a fake display controller.
  zx_status_t CreateClient(
      ClientPriority client_priority,
      fidl::ServerEnd<fuchsia_hardware_display::Coordinator> coordinator_server_end,
      fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener>
          coordinator_listener_client_end,
      fit::function<void()> on_client_disconnected);

  display::DriverBufferCollectionId GetNextDriverBufferCollectionId();

  // `fidl::WireServer<fuchsia_hardware_display::Provider>`:
  void OpenCoordinatorWithListenerForVirtcon(
      OpenCoordinatorWithListenerForVirtconRequestView request,
      OpenCoordinatorWithListenerForVirtconCompleter::Sync& completer) override;
  void OpenCoordinatorWithListenerForPrimary(
      OpenCoordinatorWithListenerForPrimaryRequestView request,
      OpenCoordinatorWithListenerForPrimaryCompleter::Sync& completer) override;

 private:
  friend ControllerTest;
  friend IntegrationTest;

  // Initializes logic that is not suitable for the constructor.
  zx::result<> Initialize();

  void HandleClientOwnershipChanges() __TA_REQUIRES(mtx());

  // Processes a display addition notification from an engine driver.
  //
  // Must be called on the client dispatcher.
  void AddDisplay(std::unique_ptr<AddedDisplayInfo> added_display_info);

  // Processes a display removal notification from an engine driver.
  //
  // Must be called on the client dispatcher.
  void RemoveDisplay(display::DisplayId removed_display_id);

  // Must be called on the client dispatcher.
  void PopulateDisplayTimings(DisplayInfo& info) __TA_EXCLUDES(mtx());

  inspect::Inspector inspector_;
  // Currently located at bootstrap/driver_manager:root/display.
  inspect::Node root_;

  fdf::UnownedSynchronizedDispatcher client_dispatcher_;

  VsyncMonitor vsync_monitor_;

  // mtx_ is a global lock on state shared among clients.
  mutable fbl::Mutex mtx_;
  bool unbinding_ __TA_GUARDED(mtx()) = false;

  DisplayInfo::Map displays_ __TA_GUARDED(mtx());
  uint32_t applied_layer_stamp_ = UINT32_MAX;
  ClientId applied_client_id_ = kInvalidClientId;
  display::DriverCaptureImageId pending_release_capture_image_id_ =
      display::kInvalidDriverCaptureImageId;

  // Populated after the engine is initialized.
  std::optional<display::EngineInfo> engine_info_;

  display::DriverBufferCollectionId next_driver_buffer_collection_id_ __TA_GUARDED(mtx()) =
      display::DriverBufferCollectionId(1);

  std::list<std::unique_ptr<ClientProxy>> clients_ __TA_GUARDED(mtx());
  ClientId next_client_id_ __TA_GUARDED(mtx()) = ClientId(1);

  // Pointers to instances owned by `clients_`.
  ClientProxy* client_owning_displays_ __TA_GUARDED(mtx()) = nullptr;
  ClientProxy* virtcon_client_ __TA_GUARDED(mtx()) = nullptr;
  ClientProxy* primary_client_ __TA_GUARDED(mtx()) = nullptr;

  // True iff the corresponding client can dispatch FIDL events.
  bool virtcon_client_ready_ __TA_GUARDED(mtx()) = false;
  bool primary_client_ready_ __TA_GUARDED(mtx()) = false;

  fuchsia_hardware_display::wire::VirtconMode virtcon_mode_ __TA_GUARDED(mtx()) =
      fuchsia_hardware_display::wire::VirtconMode::kInactive;

  std::unique_ptr<EngineDriverClient> engine_driver_client_;

  zx_time_t last_valid_apply_config_timestamp_{};
  inspect::UintProperty last_valid_apply_config_timestamp_ns_property_;
  inspect::UintProperty last_valid_apply_config_interval_ns_property_;
  inspect::UintProperty last_valid_apply_config_config_stamp_property_;

  display::DriverConfigStamp last_issued_driver_config_stamp_ __TA_GUARDED(mtx()) =
      display::kInvalidDriverConfigStamp;
  display::DriverConfigStamp last_applied_driver_config_stamp_ __TA_GUARDED(mtx()) =
      display::kInvalidDriverConfigStamp;
};

template <typename Callback>
bool Controller::FindDisplayInfo(display::DisplayId display_id, Callback callback) {
  for (const DisplayInfo& display : displays_) {
    if (display.id() == display_id) {
      callback(display);
      return true;
    }
  }
  return false;
}

}  // namespace display_coordinator

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_CONTROLLER_H_
