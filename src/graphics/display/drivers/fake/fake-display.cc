// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/fake/fake-display.h"

#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <fuchsia/hardware/display/controller/cpp/banjo.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/result.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/inspect/cpp/inspector.h>
#include <lib/sysmem-version/sysmem-version.h>
#include <lib/zx/channel.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>
#include <lib/zx/vmo.h>
#include <threads.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>
#include <zircon/threads.h>
#include <zircon/types.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cinttypes>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <initializer_list>
#include <iterator>
#include <limits>
#include <mutex>
#include <utility>

#include <fbl/alloc_checker.h>
#include <fbl/vector.h>

#include "src/graphics/display/drivers/coordinator/preferred-scanout-image-type.h"
#include "src/graphics/display/drivers/fake/image-info.h"
#include "src/graphics/display/lib/api-types/cpp/color.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/display-timing.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-capture-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/driver-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"
#include "src/lib/fsl/handles/object_info.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace fake_display {

namespace {

// List of supported pixel formats
constexpr fuchsia_images2_pixel_format_enum_value_t kSupportedPixelFormats[] = {
    static_cast<fuchsia_images2_pixel_format_enum_value_t>(
        fuchsia_images2::wire::PixelFormat::kB8G8R8A8),
    static_cast<fuchsia_images2_pixel_format_enum_value_t>(
        fuchsia_images2::wire::PixelFormat::kR8G8B8A8),
};

// Arbitrary dimensions - the same as sherlock.
constexpr int32_t kWidth = 1280;
constexpr int32_t kHeight = 800;

constexpr display::DisplayId kDisplayId(1);

constexpr int32_t kRefreshRateFps = 60;

display_mode_t CreateBanjoDisplayMode() {
  static constexpr int64_t kPixelClockFrequencyHz = int64_t{kWidth} * kHeight * kRefreshRateFps;
  static_assert(kPixelClockFrequencyHz >= 0);
  static_assert(kPixelClockFrequencyHz <= display::kMaxPixelClockHz);

  static constexpr display::DisplayTiming kDisplayTiming = {
      .horizontal_active_px = kWidth,
      .horizontal_front_porch_px = 0,
      .horizontal_sync_width_px = 0,
      .horizontal_back_porch_px = 0,
      .vertical_active_lines = kHeight,
      .vertical_front_porch_lines = 0,
      .vertical_sync_width_lines = 0,
      .vertical_back_porch_lines = 0,
      .pixel_clock_frequency_hz = kPixelClockFrequencyHz,
      .fields_per_frame = display::FieldsPerFrame::kProgressive,
      .hsync_polarity = display::SyncPolarity::kNegative,
      .vsync_polarity = display::SyncPolarity::kNegative,
      .vblank_alternates = false,
      .pixel_repetition = 0,
  };

  return display::ToBanjoDisplayMode(kDisplayTiming);
}

// `banjo_display_mode` must outlive the returned value.
raw_display_info_t CreateRawDisplayInfo(const display_mode_t* banjo_display_mode) {
  raw_display_info_t args = {
      .display_id = display::ToBanjoDisplayId(kDisplayId),
      .preferred_modes_list = banjo_display_mode,
      .preferred_modes_count = 1,
      .edid_bytes_list = nullptr,
      .edid_bytes_count = 0,
      .pixel_formats_list = kSupportedPixelFormats,
      .pixel_formats_count = std::size(kSupportedPixelFormats),
  };
  return args;
}

}  // namespace

FakeDisplay::FakeDisplay(FakeDisplayDeviceConfig device_config,
                         fidl::ClientEnd<fuchsia_sysmem2::Allocator> sysmem_allocator,
                         inspect::Inspector inspector)
    : display_engine_banjo_protocol_({&display_engine_protocol_ops_, this}),
      device_config_(device_config),
      sysmem_(std::move(sysmem_allocator)),
      applied_fallback_color_({
          .format = display::PixelFormat::kB8G8R8A8,
          .bytes = std::initializer_list<uint8_t>{0, 0, 0, 0, 0, 0, 0, 0},
      }),
      inspector_(std::move(inspector)) {
  ZX_DEBUG_ASSERT(sysmem_.is_valid());
  InitializeSysmemClient();

  if (device_config_.periodic_vsync) {
    vsync_thread_.emplace([](FakeDisplay* fake_display) { fake_display->VSyncThread(); }, this);
  }
  if (IsCaptureSupported()) {
    capture_thread_.emplace([](FakeDisplay* fake_display) { fake_display->CaptureThread(); }, this);
  }

  RecordDisplayConfigToInspectRootNode();
}

FakeDisplay::~FakeDisplay() {
  vsync_thread_shutdown_requested_.store(true, std::memory_order_relaxed);
  capture_thread_shutdown_requested_.store(true, std::memory_order_relaxed);

  if (vsync_thread_.has_value()) {
    vsync_thread_->join();
  }
  if (capture_thread_.has_value()) {
    capture_thread_->join();
  }
}

zx_status_t FakeDisplay::DisplayEngineSetMinimumRgb(uint8_t minimum_rgb) {
  std::lock_guard lock(mutex_);

  clamp_rgb_value_ = minimum_rgb;
  return ZX_OK;
}

void FakeDisplay::InitializeSysmemClient() {
  std::string debug_name = fxl::StringPrintf("fake-display[%lu]", fsl::GetCurrentProcessKoid());
  fuchsia_sysmem2::AllocatorSetDebugClientInfoRequest request;
  request.name() = debug_name;
  request.id() = fsl::GetCurrentProcessKoid();

  std::lock_guard lock(mutex_);
  fit::result<fidl::OneWayStatus> set_debug_status =
      sysmem_->SetDebugClientInfo(std::move(request));
  if (!set_debug_status.is_ok()) {
    // Errors here mean that the FIDL transport was not set up correctly, and
    // all future Sysmem client calls will fail. Crashing here exposes the
    // failure early.
    fdf::fatal("SetDebugClientInfo() FIDL call failed: {}",
               set_debug_status.error_value().status_string());
  }
}

void FakeDisplay::DisplayEngineCompleteCoordinatorConnection(
    const display_engine_listener_protocol_t* display_engine_listener,
    engine_info_t* out_banjo_engine_info) {
  ZX_DEBUG_ASSERT(display_engine_listener != nullptr);
  ZX_DEBUG_ASSERT(out_banjo_engine_info != nullptr);

  {
    std::lock_guard engine_listener_lock(engine_listener_mutex_);
    engine_listener_client_ = ddk::DisplayEngineListenerProtocolClient(display_engine_listener);
  }
  SendDisplayInformation();

  *out_banjo_engine_info = {
      .max_layer_count = 1,
      .max_connected_display_count = 1,
      .is_capture_supported = IsCaptureSupported(),
  };
}

void FakeDisplay::SendDisplayInformation() {
  const display_mode_t banjo_display_mode = CreateBanjoDisplayMode();
  const raw_display_info_t banjo_display_info = CreateRawDisplayInfo(&banjo_display_mode);

  std::lock_guard engine_listener_lock(engine_listener_mutex_);
  if (!engine_listener_client_.is_valid()) {
    fdf::warn("OnDisplayAdded() emitted with invalid event listener; event dropped");
    return;
  }
  engine_listener_client_.OnDisplayAdded(&banjo_display_info);
}

void FakeDisplay::DisplayEngineUnsetListener() {
  std::lock_guard engine_listener_lock(engine_listener_mutex_);
  engine_listener_client_ = ddk::DisplayEngineListenerProtocolClient();
}

zx::result<display::DriverImageId> FakeDisplay::ImportVmoImageForTesting(zx::vmo vmo,
                                                                         size_t offset) {
  std::lock_guard lock(mutex_);

  display::DriverImageId driver_image_id = next_imported_display_driver_image_id_++;

  // Image metadata for testing only and may not reflect the actual image
  // buffer format.
  ImageMetadata display_image_metadata = {
      .pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8,
      .coherency_domain = fuchsia_sysmem2::CoherencyDomain::kCpu,
  };

  fbl::AllocChecker alloc_checker;
  auto import_info = fbl::make_unique_checked<DisplayImageInfo>(
      &alloc_checker, driver_image_id, display_image_metadata, std::move(vmo));
  if (!alloc_checker.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  imported_images_.insert(std::move(import_info));
  return zx::ok(driver_image_id);
}

namespace {

bool IsAcceptableImageTilingType(uint32_t image_tiling_type) {
  return image_tiling_type == IMAGE_TILING_TYPE_PREFERRED_SCANOUT ||
         image_tiling_type == IMAGE_TILING_TYPE_LINEAR;
}

}  // namespace

zx_status_t FakeDisplay::DisplayEngineImportBufferCollection(
    uint64_t banjo_driver_buffer_collection_id, zx::channel collection_token) {
  const display::DriverBufferCollectionId driver_buffer_collection_id =
      display::ToDriverBufferCollectionId(banjo_driver_buffer_collection_id);

  std::lock_guard lock(mutex_);

  if (buffer_collections_.find(driver_buffer_collection_id) != buffer_collections_.end()) {
    fdf::error("Buffer Collection (id={}) already exists", driver_buffer_collection_id.value());
    return ZX_ERR_ALREADY_EXISTS;
  }

  auto [collection_client_endpoint, collection_server_endpoint] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();

  fuchsia_sysmem2::AllocatorBindSharedCollectionRequest bind_request;
  bind_request.token() =
      fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>(std::move(collection_token));
  bind_request.buffer_collection_request() = std::move(collection_server_endpoint);
  auto bind_result = sysmem_->BindSharedCollection(std::move(bind_request));
  if (bind_result.is_error()) {
    fdf::error("Cannot complete FIDL call BindSharedCollection: {}",
               bind_result.error_value().status_string());
    return ZX_ERR_INTERNAL;
  }

  buffer_collections_[driver_buffer_collection_id] =
      fidl::SyncClient(std::move(collection_client_endpoint));
  return ZX_OK;
}

zx_status_t FakeDisplay::DisplayEngineReleaseBufferCollection(
    uint64_t banjo_driver_buffer_collection_id) {
  const display::DriverBufferCollectionId driver_buffer_collection_id =
      display::ToDriverBufferCollectionId(banjo_driver_buffer_collection_id);

  std::lock_guard lock(mutex_);

  if (buffer_collections_.find(driver_buffer_collection_id) == buffer_collections_.end()) {
    fdf::error("Cannot release buffer collection {}: buffer collection doesn't exist",
               driver_buffer_collection_id.value());
    return ZX_ERR_NOT_FOUND;
  }
  buffer_collections_.erase(driver_buffer_collection_id);
  return ZX_OK;
}

zx_status_t FakeDisplay::DisplayEngineImportImage(const image_metadata_t* image_metadata,
                                                  uint64_t banjo_driver_buffer_collection_id,
                                                  uint32_t index, uint64_t* out_image_handle) {
  const display::DriverBufferCollectionId driver_buffer_collection_id =
      display::ToDriverBufferCollectionId(banjo_driver_buffer_collection_id);

  std::lock_guard lock(mutex_);

  const auto it = buffer_collections_.find(driver_buffer_collection_id);
  if (it == buffer_collections_.end()) {
    fdf::error("ImportImage: Cannot find imported buffer collection (id={})",
               driver_buffer_collection_id.value());
    return ZX_ERR_NOT_FOUND;
  }
  const fidl::SyncClient<fuchsia_sysmem2::BufferCollection>& collection = it->second;
  if (!IsAcceptableImageTilingType(image_metadata->tiling_type)) {
    fdf::info("ImportImage() will fail due to invalid Image tiling type {}",
              image_metadata->tiling_type);
    return ZX_ERR_INVALID_ARGS;
  }

  auto check_result = collection->CheckAllBuffersAllocated();
  // TODO(https://fxbug.dev/42072690): The sysmem FIDL error logging patterns are
  // inconsistent across drivers. The FIDL error handling and logging should be
  // unified.
  if (check_result.is_error()) {
    if (check_result.error_value().is_framework_error()) {
      return check_result.error_value().framework_error().status();
    }
    fuchsia_sysmem2::Error check_error = check_result.error_value().domain_error();
    if (check_error == fuchsia_sysmem2::Error::kPending) {
      return ZX_ERR_SHOULD_WAIT;
    }
    return sysmem::V1CopyFromV2Error(check_error);
  }

  auto wait_result = collection->WaitForAllBuffersAllocated();
  // TODO(https://fxbug.dev/42072690): The sysmem FIDL error logging patterns are
  // inconsistent across drivers. The FIDL error handling and logging should be
  // unified.
  if (wait_result.is_error()) {
    if (wait_result.error_value().is_framework_error()) {
      return wait_result.error_value().framework_error().status();
    }
    fuchsia_sysmem2::Error wait_error = wait_result.error_value().domain_error();
    return sysmem::V1CopyFromV2Error(wait_error);
  }
  auto& wait_response = wait_result.value();
  auto& collection_info = wait_response.buffer_collection_info();

  fbl::AllocChecker alloc_checker;
  fbl::Vector<zx::vmo> vmos;
  vmos.reserve(collection_info->buffers()->size(), &alloc_checker);
  if (!alloc_checker.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  for (auto& buffer : *collection_info->buffers()) {
    ZX_DEBUG_ASSERT_MSG(vmos.size() < collection_info->buffers()->size(),
                        "Incorrect capacity passed to earlier reserve()");
    vmos.push_back(std::move(buffer.vmo().value()), &alloc_checker);
    ZX_DEBUG_ASSERT_MSG(alloc_checker.check(), "Incorrect capacity passed to earlier reserve()");
  }

  if (!collection_info->settings()->image_format_constraints().has_value() ||
      index >= vmos.size()) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // TODO(https://fxbug.dev/42079320): When capture is enabled
  // (IsCaptureSupported() is true), we should perform a check to ensure that
  // the display images should not be of "inaccessible" coherency domain.

  display::DriverImageId driver_image_id = next_imported_display_driver_image_id_++;
  ImageMetadata display_image_metadata = {
      .pixel_format =
          collection_info->settings()->image_format_constraints()->pixel_format().value(),
      .coherency_domain =
          collection_info->settings()->buffer_settings()->coherency_domain().value(),
  };

  auto import_info = fbl::make_unique_checked<DisplayImageInfo>(
      &alloc_checker, driver_image_id, std::move(display_image_metadata), std::move(vmos[index]));
  if (!alloc_checker.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  *out_image_handle = display::ToBanjoDriverImageId(driver_image_id);
  imported_images_.insert(std::move(import_info));
  return ZX_OK;
}

void FakeDisplay::DisplayEngineReleaseImage(uint64_t image_handle) {
  display::DriverImageId driver_image_id = display::ToDriverImageId(image_handle);

  std::lock_guard lock(mutex_);

  if (applied_image_id_ == driver_image_id) {
    fdf::fatal("Cannot safely release an image used in currently applied configuration");
    return;
  }

  if (imported_images_.erase(driver_image_id) == nullptr) {
    fdf::error("Image release request with unused handle: {}", driver_image_id.value());
  }
}

config_check_result_t FakeDisplay::DisplayEngineCheckConfiguration(
    const display_config_t* display_config_ptr,
    layer_composition_operations_t* out_layer_composition_operations_list,
    size_t layer_composition_operations_count, size_t* out_layer_composition_operations_actual) {
  ZX_DEBUG_ASSERT(display_config_ptr != nullptr);
  const display_config_t& display_config = *display_config_ptr;

  if (out_layer_composition_operations_actual != nullptr) {
    *out_layer_composition_operations_actual = 0;
  }

  ZX_DEBUG_ASSERT(display::ToDisplayId(display_config.display_id) == kDisplayId);

  ZX_DEBUG_ASSERT(layer_composition_operations_count >= display_config.layer_count);
  cpp20::span<layer_composition_operations_t> layer_composition_operations(
      out_layer_composition_operations_list, display_config.layer_count);
  std::fill(layer_composition_operations.begin(), layer_composition_operations.end(), 0);
  if (out_layer_composition_operations_actual != nullptr) {
    *out_layer_composition_operations_actual = layer_composition_operations.size();
  }

  config_check_result_t check_result = [&] {
    // TODO(https://fxbug.dev/394413629): Remove support for empty configs.
    if (display_config.layer_count == 0) {
      return CONFIG_CHECK_RESULT_OK;
    }

    if (display_config.layer_count > 1) {
      return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
    }
    ZX_DEBUG_ASSERT(display_config.layer_count == 1);
    const layer_t& layer = display_config.layer_list[0];
    const rect_u_t display_area = {.x = 0, .y = 0, .width = kWidth, .height = kHeight};
    if (memcmp(&layer.display_destination, &display_area, sizeof(rect_u_t)) != 0) {
      return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
    }

    if (memcmp(&layer.image_source, &display_area, sizeof(rect_u_t)) != 0) {
      // Allow solid color fill layers.
      if (layer.image_source.width != 0 || layer.image_source.height != 0) {
        return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
      }
      if (layer.image_source.x != 0 || layer.image_source.y != 0) {
        return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
      }

      if (!display::PixelFormat::IsSupported(layer.fallback_color.format)) {
        return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
      }

      // The capture simulation implementation is currently optimized for 32-bit
      // colors. Removing this constraint will require updating that
      // implementation.
      display::PixelFormat fallback_color_pixel_format =
          display::PixelFormat(layer.fallback_color.format);
      if (fallback_color_pixel_format.EncodingSize() != sizeof(uint32_t)) {
        return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
      }
    }

    if (layer.image_metadata.dimensions.width != layer.image_source.width) {
      return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
    }
    if (layer.image_metadata.dimensions.height != layer.image_source.height) {
      return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
    }

    if (layer.alpha_mode != ALPHA_DISABLE) {
      return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
    }
    if (layer.image_source_transformation != COORDINATE_TRANSFORMATION_IDENTITY) {
      return CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG;
    }
    return CONFIG_CHECK_RESULT_OK;
  }();

  if (check_result == CONFIG_CHECK_RESULT_UNSUPPORTED_CONFIG) {
    for (size_t i = 0; i < display_config.layer_count; ++i) {
      layer_composition_operations[i] = LAYER_COMPOSITION_OPERATIONS_MERGE;
    }
  }
  return check_result;
}

void FakeDisplay::DisplayEngineApplyConfiguration(const display_config_t* display_config_ptr,
                                                  const config_stamp_t* banjo_config_stamp) {
  ZX_DEBUG_ASSERT(display_config_ptr != nullptr);
  const display_config_t& display_config = *display_config_ptr;

  ZX_DEBUG_ASSERT(banjo_config_stamp != nullptr);
  const display::DriverConfigStamp config_stamp = display::ToDriverConfigStamp(*banjo_config_stamp);

  std::lock_guard lock(mutex_);
  if (display_config.layer_count) {
    // Only support one display.
    applied_image_id_ = display::ToDriverImageId(display_config.layer_list[0].image_handle);
    applied_fallback_color_ = display::Color::From(display_config.layer_list[0].fallback_color);
  } else {
    applied_image_id_ = display::kInvalidDriverImageId;
    static constexpr display::Color kBlackBgra(
        {.format = display::PixelFormat::kB8G8R8A8,
         .bytes = std::initializer_list<uint8_t>{0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00}});
    applied_fallback_color_ = kBlackBgra;
  }

  applied_config_stamp_ = config_stamp;
}

enum class FakeDisplay::BufferCollectionUsage {
  kPrimaryLayer = 1,
  kCapture = 2,
};

fuchsia_sysmem2::BufferCollectionConstraints FakeDisplay::CreateBufferCollectionConstraints(
    BufferCollectionUsage usage) {
  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  switch (usage) {
    case BufferCollectionUsage::kCapture:
      constraints.usage().emplace();
      constraints.usage()->cpu() =
          fuchsia_sysmem2::kCpuUsageReadOften | fuchsia_sysmem2::kCpuUsageWriteOften;
      break;
    case BufferCollectionUsage::kPrimaryLayer:
      constraints.usage().emplace();
      constraints.usage()->display() = fuchsia_sysmem2::kDisplayUsageLayer;
      break;
  }

  // TODO(https://fxbug.dev/42079320): In order to support capture, both capture sources
  // and capture targets must not be in the "inaccessible" coherency domain.
  constraints.buffer_memory_constraints().emplace();
  SetBufferMemoryConstraints(constraints.buffer_memory_constraints().value());

  // When we have C++20, we can use std::to_array to avoid specifying the array
  // size twice.
  static constexpr std::array<fuchsia_images2::PixelFormat, 2> kPixelFormats = {
      fuchsia_images2::PixelFormat::kR8G8B8A8, fuchsia_images2::PixelFormat::kB8G8R8A8};
  static constexpr std::array<fuchsia_images2::PixelFormatModifier, 2> kFormatModifiers = {
      fuchsia_images2::PixelFormatModifier::kLinear,
      fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal};

  constraints.image_format_constraints().emplace();
  for (auto pixel_format : kPixelFormats) {
    for (auto format_modifier : kFormatModifiers) {
      fuchsia_sysmem2::ImageFormatConstraints& image_constraints =
          constraints.image_format_constraints()->emplace_back();

      SetCommonImageFormatConstraints(pixel_format, format_modifier, image_constraints);
      switch (usage) {
        case BufferCollectionUsage::kCapture:
          SetCaptureImageFormatConstraints(image_constraints);
          break;
        case BufferCollectionUsage::kPrimaryLayer:
          SetLayerImageFormatConstraints(image_constraints);
          break;
      }
    }
  }
  return constraints;
}

void FakeDisplay::SetBufferMemoryConstraints(
    fuchsia_sysmem2::BufferMemoryConstraints& constraints) {
  constraints.min_size_bytes() = 0;
  constraints.max_size_bytes() = std::numeric_limits<uint32_t>::max();
  constraints.physically_contiguous_required() = false;
  constraints.secure_required() = false;
  constraints.ram_domain_supported() = true;
  constraints.cpu_domain_supported() = true;
  constraints.inaccessible_domain_supported() = true;
}

void FakeDisplay::SetCommonImageFormatConstraints(
    fuchsia_images2::PixelFormat pixel_format, fuchsia_images2::PixelFormatModifier format_modifier,
    fuchsia_sysmem2::ImageFormatConstraints& constraints) {
  constraints.pixel_format() = pixel_format;
  constraints.pixel_format_modifier() = format_modifier;

  constraints.color_spaces() = {fuchsia_images2::ColorSpace::kSrgb};

  constraints.size_alignment() = {1, 1};
  constraints.bytes_per_row_divisor() = 1;
  constraints.start_offset_divisor() = 1;
  constraints.display_rect_alignment() = {1, 1};
}

void FakeDisplay::SetCaptureImageFormatConstraints(
    fuchsia_sysmem2::ImageFormatConstraints& constraints) {
  constraints.min_size() = {kWidth, kHeight};
  constraints.max_size() = {kWidth, kHeight};
  constraints.min_bytes_per_row() = kWidth * 4;
  constraints.max_bytes_per_row() = kWidth * 4;
  constraints.max_width_times_height() = kWidth * kHeight;
}

void FakeDisplay::SetLayerImageFormatConstraints(
    fuchsia_sysmem2::ImageFormatConstraints& constraints) {
  constraints.min_size() = {0, 0};
  constraints.max_size() = {std::numeric_limits<uint32_t>::max(),
                            std::numeric_limits<uint32_t>::max()};
  constraints.min_bytes_per_row() = 0;
  constraints.max_bytes_per_row() = std::numeric_limits<uint32_t>::max();
  constraints.max_width_times_height() = std::numeric_limits<uint32_t>::max();
}

zx_status_t FakeDisplay::DisplayEngineSetBufferCollectionConstraints(
    const image_buffer_usage_t* usage, uint64_t banjo_driver_buffer_collection_id) {
  const display::DriverBufferCollectionId driver_buffer_collection_id =
      display::ToDriverBufferCollectionId(banjo_driver_buffer_collection_id);

  std::lock_guard lock(mutex_);

  const auto it = buffer_collections_.find(driver_buffer_collection_id);
  if (it == buffer_collections_.end()) {
    fdf::error("ImportImage: Cannot find imported buffer collection (id={})",
               driver_buffer_collection_id.value());
    return ZX_ERR_NOT_FOUND;
  }
  const fidl::SyncClient<fuchsia_sysmem2::BufferCollection>& collection = it->second;

  BufferCollectionUsage buffer_collection_usage = (usage->tiling_type == IMAGE_TILING_TYPE_CAPTURE)
                                                      ? BufferCollectionUsage::kCapture
                                                      : BufferCollectionUsage::kPrimaryLayer;

  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest request;
  request.constraints() = CreateBufferCollectionConstraints(buffer_collection_usage);
  auto set_result = collection->SetConstraints(std::move(request));
  if (set_result.is_error()) {
    fdf::error("Failed to set constraints on a sysmem BufferCollection: {}",
               set_result.error_value().status_string());
    return set_result.error_value().status();
  }

  return ZX_OK;
}

zx_status_t FakeDisplay::DisplayEngineSetDisplayPower(uint64_t display_id, bool power_on) {
  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t FakeDisplay::DisplayEngineImportImageForCapture(
    uint64_t banjo_driver_buffer_collection_id, uint32_t index, uint64_t* out_capture_handle) {
  if (!IsCaptureSupported()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  const display::DriverBufferCollectionId driver_buffer_collection_id =
      display::ToDriverBufferCollectionId(banjo_driver_buffer_collection_id);

  std::lock_guard lock(mutex_);

  const auto it = buffer_collections_.find(driver_buffer_collection_id);
  if (it == buffer_collections_.end()) {
    fdf::error("ImportImage: Cannot find imported buffer collection (id={})",
               driver_buffer_collection_id.value());
    return ZX_ERR_NOT_FOUND;
  }
  const fidl::SyncClient<fuchsia_sysmem2::BufferCollection>& collection = it->second;

  auto check_result = collection->CheckAllBuffersAllocated();
  // TODO(https://fxbug.dev/42072690): The sysmem FIDL error logging patterns are
  // inconsistent across drivers. The FIDL error handling and logging should be
  // unified.
  if (check_result.is_error()) {
    if (check_result.error_value().is_framework_error()) {
      return check_result.error_value().framework_error().status();
    }
    fuchsia_sysmem2::Error check_error = check_result.error_value().domain_error();
    if (check_error == fuchsia_sysmem2::Error::kPending) {
      return ZX_ERR_SHOULD_WAIT;
    }
    return sysmem::V1CopyFromV2Error(check_error);
  }

  auto wait_result = collection->WaitForAllBuffersAllocated();
  // TODO(https://fxbug.dev/42072690): The sysmem FIDL error logging patterns are
  // inconsistent across drivers. The FIDL error handling and logging should be
  // unified.
  if (wait_result.is_error()) {
    if (wait_result.error_value().is_framework_error()) {
      return wait_result.error_value().framework_error().status();
    }
    fuchsia_sysmem2::Error wait_error = wait_result.error_value().domain_error();
    return sysmem::V1CopyFromV2Error(wait_error);
  }
  auto& wait_response = wait_result.value();
  fuchsia_sysmem2::BufferCollectionInfo& collection_info =
      wait_response.buffer_collection_info().value();

  if (!collection_info.settings()->image_format_constraints().has_value()) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (index >= collection_info.buffers()->size()) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // TODO(https://fxbug.dev/42079320): Capture target images should not be of
  // "inaccessible" coherency domain. We should add a check here.
  display::DriverCaptureImageId driver_capture_image_id = next_imported_driver_capture_image_id_++;
  ImageMetadata capture_image_metadata = {
      .pixel_format =
          collection_info.settings()->image_format_constraints()->pixel_format().value(),
      .coherency_domain = collection_info.settings()->buffer_settings()->coherency_domain().value(),
  };
  zx::vmo vmo = std::move(collection_info.buffers()->at(index).vmo().value());

  fbl::AllocChecker alloc_checker;
  auto capture_image_info = fbl::make_unique_checked<CaptureImageInfo>(
      &alloc_checker, driver_capture_image_id, std::move(capture_image_metadata), std::move(vmo));
  if (!alloc_checker.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  *out_capture_handle = display::ToBanjoDriverCaptureImageId(driver_capture_image_id);
  imported_captures_.insert(std::move(capture_image_info));
  return ZX_OK;
}

zx_status_t FakeDisplay::DisplayEngineStartCapture(uint64_t capture_handle) {
  if (!IsCaptureSupported()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  std::lock_guard lock(mutex_);

  if (started_capture_target_id_ != display::kInvalidDriverCaptureImageId) {
    fdf::error("Capture start request declined while a capture is already in-progress");
    return ZX_ERR_SHOULD_WAIT;
  }

  // Confirm the handle was previously imported (hence valid)
  display::DriverCaptureImageId driver_capture_image_id =
      display::ToDriverCaptureImageId(capture_handle);
  auto it = imported_captures_.find(driver_capture_image_id);
  if (it == imported_captures_.end()) {
    fdf::error("Capture start request with invalid handle: {}", driver_capture_image_id.value());
    return ZX_ERR_INVALID_ARGS;
  }

  started_capture_target_id_ = driver_capture_image_id;
  return ZX_OK;
}

zx_status_t FakeDisplay::DisplayEngineReleaseCapture(uint64_t capture_handle) {
  if (!IsCaptureSupported()) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  display::DriverCaptureImageId driver_capture_image_id =
      display::ToDriverCaptureImageId(capture_handle);

  std::lock_guard lock(mutex_);

  if (started_capture_target_id_ == driver_capture_image_id) {
    fdf::fatal("Refusing to release the target of an in-progress capture");

    // TODO(https://fxrev.dev/394954078): The return code is not meaningful. It will be
    // removed when the ReleaseCapture() error code is eliminated.
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (imported_captures_.erase(driver_capture_image_id) == nullptr) {
    fdf::error("Capture release request with unused handle: {}", driver_capture_image_id.value());

    // TODO(https://fxrev.dev/394954078): The return code is not meaningful. It will be
    // removed when the ReleaseCapture() error code is eliminated.
    return ZX_ERR_INVALID_ARGS;
  }
  return ZX_OK;
}

bool FakeDisplay::IsCaptureSupported() const { return !device_config_.no_buffer_access; }

void FakeDisplay::CaptureThread() {
  ZX_DEBUG_ASSERT(IsCaptureSupported());

  while (!capture_thread_shutdown_requested_.load(std::memory_order_relaxed)) {
    [[maybe_unused]] zx::result<> capture_result = ServiceAnyCaptureRequest();
    // ServiceAnyCaptureRequest() has already logged the error.

    zx::nanosleep(zx::deadline_after(zx::sec(1) / kRefreshRateFps));
  }
}

zx::result<> FakeDisplay::ServiceAnyCaptureRequest() {
  std::lock_guard lock(mutex_);
  if (started_capture_target_id_ == display::kInvalidDriverCaptureImageId) {
    return zx::ok();
  }

  auto imported_captures_it = imported_captures_.find(started_capture_target_id_);

  ZX_ASSERT_MSG(imported_captures_it.IsValid(),
                "Driver allowed releasing the target of an in-progress capture");
  CaptureImageInfo& capture_destination_info = *imported_captures_it;

  if (applied_image_id_ == display::kInvalidDriverImageId) {
    // Solid color fill capture.
    zx::result<> color_fill_capture_result =
        DoColorFillCapture(applied_fallback_color_, capture_destination_info);
    if (color_fill_capture_result.is_error()) {
      // DoColorFillCapture() has already logged the error.
      return color_fill_capture_result;
    }
  } else {
    // Image capture.
    auto imported_images_it = imported_images_.find(applied_image_id_);

    ZX_ASSERT_MSG(imported_images_it.IsValid(),
                  "Driver allowed releasing an image used in the currently applied configuration");
    DisplayImageInfo& display_source_info = *imported_images_it;

    zx::result<> image_capture_result =
        DoImageCapture(display_source_info, capture_destination_info);
    if (image_capture_result.is_error()) {
      // DoImageCapture() has already logged the error.
      return image_capture_result;
    }
  }

  SendCaptureComplete();

  started_capture_target_id_ = display::kInvalidDriverCaptureImageId;

  return zx::ok();
}

// static
zx::result<> FakeDisplay::DoImageCapture(DisplayImageInfo& source_info,
                                         CaptureImageInfo& destination_info) {
  if (source_info.metadata().pixel_format != destination_info.metadata().pixel_format) {
    fdf::error("Capture will fail; trying to capture format={} as format={}\n",
               static_cast<uint32_t>(source_info.metadata().pixel_format),
               static_cast<uint32_t>(destination_info.metadata().pixel_format));
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  size_t source_vmo_size;
  zx_status_t status = source_info.vmo().get_size(&source_vmo_size);
  if (status != ZX_OK) {
    fdf::error("Failed to get the size of the displayed image VMO: {}", zx::make_result(status));
    return zx::error(status);
  }
  if (source_vmo_size % sizeof(uint32_t) != 0) {
    fdf::error("Capture will fail; the displayed image VMO size {} is not a 32-bit multiple",
               source_vmo_size);
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  size_t destination_vmo_size;
  status = destination_info.vmo().get_size(&destination_vmo_size);
  if (status != ZX_OK) {
    fdf::error("Failed to get the size of the VMO for the captured image: {}",
               zx::make_result(status));
    return zx::error(status);
  }
  if (destination_vmo_size != source_vmo_size) {
    fdf::error(
        "Capture will fail; the displayed image VMO size {} does not match the "
        "captured image VMO size {}",
        source_vmo_size, destination_vmo_size);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fzl::VmoMapper source_mapper;
  status = source_mapper.Map(source_info.vmo(), 0, source_vmo_size, ZX_VM_PERM_READ);
  if (status != ZX_OK) {
    fdf::error("Capture will fail; failed to map displayed image VMO: {}", zx::make_result(status));
    return zx::error(status);
  }

  // Inline implementation of std::is_sufficiently_aligned() from C++26.
  ZX_ASSERT_MSG(std::bit_cast<std::uintptr_t>(source_mapper.start()) % sizeof(uint32_t) == 0,
                "Page size <= 32 bits; the pointer cast below will cause UB");
  std::span<const uint32_t> source_colors(static_cast<const uint32_t*>(source_mapper.start()),
                                          source_vmo_size / sizeof(uint32_t));

  fzl::VmoMapper destination_mapper;
  status = destination_mapper.Map(destination_info.vmo(), 0, destination_vmo_size,
                                  ZX_VM_PERM_READ | ZX_VM_PERM_WRITE);
  if (status != ZX_OK) {
    fdf::error("Capture will fail; failed to map capture image VMO: {}", zx::make_result(status));
    return zx::error(status);
  }

  // Inline implementation of std::is_sufficiently_aligned() from C++26.
  ZX_ASSERT_MSG(std::bit_cast<std::uintptr_t>(destination_mapper.start()) % sizeof(uint32_t) == 0,
                "Page size <= 32 bits; the pointer cast below will cause UB");
  std::span<uint32_t> destination_colors(static_cast<uint32_t*>(destination_mapper.start()),
                                         destination_vmo_size / sizeof(uint32_t));

  if (source_info.metadata().coherency_domain == fuchsia_sysmem2::CoherencyDomain::kRam) {
    zx_cache_flush(source_mapper.start(), source_vmo_size,
                   ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
  }
  std::ranges::copy(source_colors, destination_colors.begin());
  if (destination_info.metadata().coherency_domain == fuchsia_sysmem2::CoherencyDomain::kRam) {
    zx_cache_flush(destination_mapper.start(), destination_vmo_size,
                   ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
  }

  return zx::ok();
}

// static
zx::result<> FakeDisplay::DoColorFillCapture(display::Color fill_color,
                                             CaptureImageInfo& destination_info) {
  // TODO(https://fxbug.dev/394954078): Capture requests issued before a
  // configuration is applied are constrained to the initial fill color format,
  // which happens to be 32-bit BGRA. This rough edge will be removed when we
  // explicitly disallow starting a capture before a config is applied.
  if (fill_color.format().ToFidl() != destination_info.metadata().pixel_format) {
    fdf::error("Capture will fail; trying to capture format={} as format={}\n",
               fill_color.format().ValueForLogging(),
               static_cast<uint32_t>(destination_info.metadata().pixel_format));
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  ZX_ASSERT_MSG(std::bit_cast<std::uintptr_t>(fill_color.bytes().data()) % sizeof(uint32_t) == 0,
                "Color byte buffer not 32-bit aligned; the pointer cast below will cause UB");
  const uint32_t source_color = *(reinterpret_cast<const uint32_t*>(fill_color.bytes().data()));

  size_t destination_vmo_size;
  zx_status_t status = destination_info.vmo().get_size(&destination_vmo_size);
  if (status != ZX_OK) {
    fdf::error("Failed to get the size of the VMO for the captured image: {}",
               zx::make_result(status));
    return zx::error(status);
  }
  if (destination_vmo_size % sizeof(uint32_t) != 0) {
    fdf::error("Capture will fail; the captured image VMO size {} is not a 32-bit multiple",
               destination_vmo_size);
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  fzl::VmoMapper destination_mapper;
  status = destination_mapper.Map(destination_info.vmo(), 0, destination_vmo_size,
                                  ZX_VM_PERM_READ | ZX_VM_PERM_WRITE);
  if (status != ZX_OK) {
    fdf::error("Capture will fail; failed to map capture image VMO: {}", zx::make_result(status));
    return zx::error(status);
  }

  // Inline implementation of std::is_sufficiently_aligned() from C++26.
  ZX_ASSERT_MSG(std::bit_cast<std::uintptr_t>(destination_mapper.start()) % sizeof(uint32_t) == 0,
                "Page size <= 32 bits; the pointer cast below will cause UB");
  std::span<uint32_t> destination_colors(static_cast<uint32_t*>(destination_mapper.start()),
                                         destination_vmo_size / sizeof(uint32_t));

  std::ranges::fill(destination_colors, source_color);
  if (destination_info.metadata().coherency_domain == fuchsia_sysmem2::CoherencyDomain::kRam) {
    zx_cache_flush(destination_mapper.start(), destination_vmo_size,
                   ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
  }

  return zx::ok();
}

void FakeDisplay::SendCaptureComplete() {
  std::lock_guard engine_listener_lock(engine_listener_mutex_);
  if (!engine_listener_client_.is_valid()) {
    return;
  }
  engine_listener_client_.OnCaptureComplete();
}

void FakeDisplay::TriggerVsync() {
  ZX_ASSERT_MSG(!device_config_.periodic_vsync,
                "TriggerVsync() called on a device with periodic VSync enabled");

  {
    std::lock_guard lock(mutex_);
    ZX_ASSERT_MSG(applied_config_stamp_ != display::kInvalidDriverConfigStamp,
                  "TriggerVsync() called before the driver received a display configuration");
  }
  // The check above may appear vulnerable to TOCTOU, but it is not. Once the predicate
  // becomes true, it will never be false again.

  SendVsync();
}

void FakeDisplay::VSyncThread() {
  while (!vsync_thread_shutdown_requested_.load(std::memory_order_relaxed)) {
    SendVsync();
    zx::nanosleep(zx::deadline_after(zx::sec(1) / kRefreshRateFps));
  }
}

void FakeDisplay::SendVsync() {
  display::DriverConfigStamp vsync_config_stamp;
  {
    std::lock_guard lock(mutex_);
    vsync_config_stamp = applied_config_stamp_;
  }
  if (vsync_config_stamp == display::kInvalidDriverConfigStamp) {
    // No configuration was applied yet.
    return;
  }

  const config_stamp_t banjo_vsync_config_stamp =
      display::ToBanjoDriverConfigStamp(vsync_config_stamp);

  zx_instant_mono_t banjo_vsync_timestamp = zx_clock_get_monotonic();

  std::lock_guard engine_listener_lock(engine_listener_mutex_);
  if (!engine_listener_client_.is_valid()) {
    return;
  }
  engine_listener_client_.OnDisplayVsync(ToBanjoDisplayId(kDisplayId), banjo_vsync_timestamp,
                                         &banjo_vsync_config_stamp);
}

void FakeDisplay::RecordDisplayConfigToInspectRootNode() {
  inspect::Node& root_node = inspector_.GetRoot();
  ZX_ASSERT(root_node);
  root_node.RecordChild("device_config", [&](inspect::Node& config_node) {
    config_node.RecordInt("width_px", kWidth);
    config_node.RecordInt("height_px", kHeight);
    config_node.RecordDouble("refresh_rate_hz", kRefreshRateFps);
    config_node.RecordBool("periodic_vsync", device_config_.periodic_vsync);
    config_node.RecordBool("no_buffer_access", device_config_.no_buffer_access);
  });
}

}  // namespace fake_display
