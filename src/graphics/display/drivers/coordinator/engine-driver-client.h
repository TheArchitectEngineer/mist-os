// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_ENGINE_DRIVER_CLIENT_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_ENGINE_DRIVER_CLIENT_H_

#include <fidl/fuchsia.sysmem2/cpp/wire.h>
#include <fuchsia/hardware/display/controller/cpp/banjo.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/zx/result.h>

#include <cstdint>
#include <memory>

#include "src/graphics/display/lib/api-types/cpp/config-check-result.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-capture-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/driver-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/engine-info.h"
#include "src/graphics/display/lib/api-types/cpp/image-buffer-usage.h"
#include "src/graphics/display/lib/api-types/cpp/image-metadata.h"

namespace display_coordinator {

class Controller;

// C++ bridge to a display engine driver.
//
// This abstract base class represents interfaces to the
// [`fuchsia.hardware.display.engine/Engine`] FIDL interface,
// as well as the
// [`fuchsia.hardware.display.controller/DisplayEngine`] Banjo
// interface.
class EngineDriverClient {
 public:
  static zx::result<std::unique_ptr<EngineDriverClient>> Create(
      std::shared_ptr<fdf::Namespace> incoming);

  EngineDriverClient() = default;
  EngineDriverClient(const EngineDriverClient&) = delete;
  EngineDriverClient& operator=(const EngineDriverClient&) = delete;

  virtual ~EngineDriverClient() = default;

  virtual void ReleaseImage(display::DriverImageId driver_image_id) = 0;
  virtual zx::result<> ReleaseCapture(display::DriverCaptureImageId driver_capture_image_id) = 0;
  virtual display::ConfigCheckResult CheckConfiguration(const display_config_t* display_config) = 0;
  virtual void ApplyConfiguration(const display_config_t* display_config,
                                  display::DriverConfigStamp config_stamp) = 0;
  virtual display::EngineInfo CompleteCoordinatorConnection(
      const display_engine_listener_protocol_t& protocol) = 0;
  virtual void UnsetListener() = 0;
  virtual zx::result<display::DriverImageId> ImportImage(
      const display::ImageMetadata& image_metadata, display::DriverBufferCollectionId collection_id,
      uint32_t index) = 0;
  virtual zx::result<display::DriverCaptureImageId> ImportImageForCapture(
      display::DriverBufferCollectionId collection_id, uint32_t index) = 0;
  virtual zx::result<> ImportBufferCollection(
      display::DriverBufferCollectionId collection_id,
      fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> collection_token) = 0;
  virtual zx::result<> ReleaseBufferCollection(display::DriverBufferCollectionId collection_id) = 0;
  virtual zx::result<> SetBufferCollectionConstraints(
      const display::ImageBufferUsage& usage, display::DriverBufferCollectionId collection_id) = 0;
  virtual zx::result<> StartCapture(display::DriverCaptureImageId driver_capture_image_id) = 0;
  virtual zx::result<> SetDisplayPower(display::DisplayId display_id, bool power_on) = 0;
  virtual zx::result<> SetMinimumRgb(uint8_t minimum_rgb) = 0;
};

}  // namespace display_coordinator

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_ENGINE_DRIVER_CLIENT_H_
