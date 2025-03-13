// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/engine-driver-client.h"

#include <fuchsia/hardware/display/controller/c/banjo.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zx/channel.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/errors.h>

#include <cstdint>

#include <fbl/alloc_checker.h>

#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-capture-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/image-buffer-usage.h"
#include "src/graphics/display/lib/api-types/cpp/image-id.h"
#include "src/graphics/display/lib/api-types/cpp/image-metadata.h"

namespace display_coordinator {

namespace {

static constexpr fdf_arena_tag_t kArenaTag = 'DISP';

zx::result<std::unique_ptr<EngineDriverClient>> CreateFidlEngineDriverClient(
    fdf::Namespace& incoming) {
  zx::result<fdf::ClientEnd<fuchsia_hardware_display_engine::Engine>> connect_engine_client_result =
      incoming.Connect<fuchsia_hardware_display_engine::Service::Engine>();
  if (connect_engine_client_result.is_error()) {
    fdf::warn("Failed to connect to display engine FIDL client: {}", connect_engine_client_result);
    return connect_engine_client_result.take_error();
  }
  fdf::ClientEnd<fuchsia_hardware_display_engine::Engine> engine_client =
      std::move(connect_engine_client_result).value();

  if (!engine_client.is_valid()) {
    fdf::warn("Display engine FIDL device is invalid");
    return zx::error(ZX_ERR_BAD_HANDLE);
  }

  fdf::Arena arena(kArenaTag);
  fdf::WireUnownedResult result = fdf::WireCall(engine_client).buffer(arena)->IsAvailable();
  if (!result.ok()) {
    fdf::warn("Display engine FIDL device is not available: {}", result.FormatDescription());
    return zx::error(result.status());
  }

  fbl::AllocChecker alloc_checker;
  auto engine_driver_client =
      fbl::make_unique_checked<EngineDriverClient>(&alloc_checker, std::move(engine_client));
  if (!alloc_checker.check()) {
    fdf::warn("Failed to allocate memory for EngineDriverClient");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  return zx::ok(std::move(engine_driver_client));
}

zx::result<std::unique_ptr<EngineDriverClient>> CreateBanjoEngineDriverClient(
    std::shared_ptr<fdf::Namespace> incoming) {
  zx::result<ddk::DisplayEngineProtocolClient> dc_result =
      compat::ConnectBanjo<ddk::DisplayEngineProtocolClient>(incoming);
  if (dc_result.is_error()) {
    fdf::warn("Failed to connect to Banjo server via the compat client: {}", dc_result);
    return dc_result.take_error();
  }
  ddk::DisplayEngineProtocolClient dc = std::move(dc_result).value();
  if (!dc.is_valid()) {
    fdf::warn("Failed to get Banjo display controller protocol");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  fbl::AllocChecker alloc_checker;
  auto engine_driver_client =
      fbl::make_unique_checked<EngineDriverClient>(&alloc_checker, std::move(dc));
  if (!alloc_checker.check()) {
    fdf::warn("Failed to allocate memory for EngineDriverClient");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  return zx::ok(std::move(engine_driver_client));
}

}  // namespace

// static
zx::result<std::unique_ptr<EngineDriverClient>> EngineDriverClient::Create(
    std::shared_ptr<fdf::Namespace> incoming) {
  ZX_DEBUG_ASSERT(incoming != nullptr);

  // Attempt to connect to FIDL protocol.
  zx::result<std::unique_ptr<EngineDriverClient>> fidl_engine_driver_client_result =
      CreateFidlEngineDriverClient(*incoming);
  if (fidl_engine_driver_client_result.is_ok()) {
    fdf::info("Using the FIDL Engine driver client");
    return fidl_engine_driver_client_result.take_value();
  }
  fdf::warn("Failed to create FIDL Engine driver client: {}; fallback to banjo",
            fidl_engine_driver_client_result);

  // Fallback to Banjo protocol.
  zx::result<std::unique_ptr<EngineDriverClient>> banjo_engine_driver_client_result =
      CreateBanjoEngineDriverClient(incoming);
  if (banjo_engine_driver_client_result.is_error()) {
    fdf::error("Failed to create banjo Engine driver client: {}",
               banjo_engine_driver_client_result);
  }
  return banjo_engine_driver_client_result;
}

EngineDriverClient::EngineDriverClient(ddk::DisplayEngineProtocolClient banjo_engine)
    : use_engine_(false), banjo_engine_(banjo_engine) {
  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
}

EngineDriverClient::EngineDriverClient(
    fdf::ClientEnd<fuchsia_hardware_display_engine::Engine> fidl_engine)
    : use_engine_(true), fidl_engine_(std::move(fidl_engine)) {
  ZX_DEBUG_ASSERT(fidl_engine_.is_valid());
}

EngineDriverClient::~EngineDriverClient() { fdf::trace("EngineDriverClient::~EngineDriverClient"); }

void EngineDriverClient::ReleaseImage(display::DriverImageId driver_image_id) {
  if (use_engine_) {
    fdf::Arena arena(kArenaTag);
    fidl::OneWayStatus result =
        fidl_engine_.buffer(arena)->ReleaseImage(ToFidlDriverImageId(driver_image_id));
    if (!result.ok()) {
      fdf::error("ReleaseImage failed: {}", result.status_string());
    }
    return;
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  banjo_engine_.ReleaseImage(ToBanjoDriverImageId(driver_image_id));
}

zx::result<> EngineDriverClient::ReleaseCapture(
    display::DriverCaptureImageId driver_capture_image_id) {
  if (use_engine_) {
    fdf::Arena arena(kArenaTag);
    fdf::WireUnownedResult result = fidl_engine_.buffer(arena)->ReleaseCapture(
        ToFidlDriverCaptureImageId(driver_capture_image_id));
    return zx::make_result(result.status());
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  zx_status_t banjo_status =
      banjo_engine_.ReleaseCapture(ToBanjoDriverCaptureImageId(driver_capture_image_id));
  return zx::make_result(banjo_status);
}

config_check_result_t EngineDriverClient::CheckConfiguration(
    const display_config_t* display_config,
    layer_composition_operations_t* out_layer_composition_operations_list,
    size_t layer_composition_operations_count, size_t* out_layer_composition_operations_actual) {
  if (use_engine_) {
    return CONFIG_CHECK_RESULT_UNSUPPORTED_MODES;
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  return banjo_engine_.CheckConfiguration(display_config, out_layer_composition_operations_list,
                                          layer_composition_operations_count,
                                          out_layer_composition_operations_actual);
}

void EngineDriverClient::ApplyConfiguration(const display_config_t* display_config,
                                            const config_stamp_t* config_stamp) {
  if (use_engine_) {
    return;
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  banjo_engine_.ApplyConfiguration(display_config, config_stamp);
}

display::EngineInfo EngineDriverClient::CompleteCoordinatorConnection(
    const display_engine_listener_protocol_t& protocol) {
  if (use_engine_) {
    return display::EngineInfo({});
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  engine_info_t banjo_engine_info;
  banjo_engine_.CompleteCoordinatorConnection(protocol.ctx, protocol.ops, &banjo_engine_info);
  if (!display::EngineInfo::IsValid(banjo_engine_info)) {
    fdf::fatal("CompleteCoordinatorConnection returned invalid EngineInfo");
  }
  return display::EngineInfo::From(banjo_engine_info);
}

void EngineDriverClient::UnsetListener() {
  if (use_engine_) {
    return;
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  banjo_engine_.UnsetListener();
}

zx::result<display::DriverImageId> EngineDriverClient::ImportImage(
    const display::ImageMetadata& image_metadata, display::DriverBufferCollectionId collection_id,
    uint32_t index) {
  if (use_engine_) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  const image_metadata_t banjo_image_metadata = image_metadata.ToBanjo();
  uint64_t image_handle = 0;
  zx_status_t banjo_status = banjo_engine_.ImportImage(
      &banjo_image_metadata, display::ToBanjoDriverBufferCollectionId(collection_id), index,
      &image_handle);
  if (banjo_status != ZX_OK) {
    return zx::error(banjo_status);
  }
  return zx::ok(display::DriverImageId(image_handle));
}

zx::result<display::DriverCaptureImageId> EngineDriverClient::ImportImageForCapture(
    display::DriverBufferCollectionId collection_id, uint32_t index) {
  if (use_engine_) {
    fdf::Arena arena(kArenaTag);
    fdf::WireUnownedResult result =
        fidl_engine_.buffer(arena)->ImportImageForCapture(display::ToFidlDriverBufferId({
            .buffer_collection_id = collection_id,
            .buffer_index = index,
        }));
    if (!result.ok()) {
      return zx::error(result.status());
    }
    if (result->is_error()) {
      return zx::error(result->error_value());
    }
    fuchsia_hardware_display_engine::wire::ImageId image_id = result->value()->capture_image_id;
    return zx::ok(display::ToDriverCaptureImageId(image_id.value));
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  uint64_t banjo_capture_image_handle = 0;
  zx_status_t banjo_status = banjo_engine_.ImportImageForCapture(
      display::ToBanjoDriverBufferCollectionId(collection_id), index, &banjo_capture_image_handle);
  if (banjo_status != ZX_OK) {
    return zx::error(banjo_status);
  }
  return zx::ok(display::ToDriverCaptureImageId(banjo_capture_image_handle));
}

zx::result<> EngineDriverClient::ImportBufferCollection(
    display::DriverBufferCollectionId collection_id,
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> collection_token) {
  if (use_engine_) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  zx_status_t banjo_status =
      banjo_engine_.ImportBufferCollection(display::ToBanjoDriverBufferCollectionId(collection_id),
                                           std::move(collection_token).TakeChannel());
  return zx::make_result(banjo_status);
}

zx::result<> EngineDriverClient::ReleaseBufferCollection(
    display::DriverBufferCollectionId collection_id) {
  if (use_engine_) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  zx_status_t banjo_status = banjo_engine_.ReleaseBufferCollection(
      display::ToBanjoDriverBufferCollectionId(collection_id));
  return zx::make_result(banjo_status);
}

zx::result<> EngineDriverClient::SetBufferCollectionConstraints(
    const display::ImageBufferUsage& usage, display::DriverBufferCollectionId collection_id) {
  if (use_engine_) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  const image_buffer_usage_t banjo_usage = display::ToBanjoImageBufferUsage(usage);
  zx_status_t banjo_status = banjo_engine_.SetBufferCollectionConstraints(
      &banjo_usage, display::ToBanjoDriverBufferCollectionId(collection_id));
  return zx::make_result(banjo_status);
}

zx::result<> EngineDriverClient::StartCapture(
    display::DriverCaptureImageId driver_capture_image_id) {
  if (use_engine_) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  zx_status_t banjo_status =
      banjo_engine_.StartCapture(ToBanjoDriverCaptureImageId(driver_capture_image_id));
  return zx::make_result(banjo_status);
}

zx::result<> EngineDriverClient::SetDisplayPower(display::DisplayId display_id, bool power_on) {
  if (use_engine_) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  zx_status_t banjo_status = banjo_engine_.SetDisplayPower(ToBanjoDisplayId(display_id), power_on);
  return zx::make_result(banjo_status);
}

zx::result<> EngineDriverClient::SetMinimumRgb(uint8_t minimum_rgb) {
  if (use_engine_) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  ZX_DEBUG_ASSERT(banjo_engine_.is_valid());
  zx_status_t banjo_status = banjo_engine_.SetMinimumRgb(minimum_rgb);
  return zx::make_result(banjo_status);
}

}  // namespace display_coordinator
