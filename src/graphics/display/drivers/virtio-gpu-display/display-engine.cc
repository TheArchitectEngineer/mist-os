// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/virtio-gpu-display/display-engine.h"

#include <fidl/fuchsia.images2/cpp/wire.h>
#include <fidl/fuchsia.sysmem2/cpp/wire.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/stdcompat/span.h>
#include <lib/virtio/driver_utils.h>
#include <lib/zx/bti.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <array>
#include <cinttypes>
#include <cstdint>
#include <cstring>
#include <memory>
#include <utility>

#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>
#include <fbl/string_buffer.h>

#include "src/graphics/display/drivers/virtio-gpu-display/imported-image.h"
#include "src/graphics/display/drivers/virtio-gpu-display/virtio-gpu-device.h"
#include "src/graphics/display/drivers/virtio-gpu-display/virtio-pci-device.h"
#include "src/graphics/display/lib/api-protocols/cpp/display-engine-events-interface.h"
#include "src/graphics/display/lib/api-types/cpp/alpha-mode.h"
#include "src/graphics/display/lib/api-types/cpp/config-check-result.h"
#include "src/graphics/display/lib/api-types/cpp/coordinate-transformation.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/driver-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-layer.h"
#include "src/graphics/display/lib/api-types/cpp/engine-info.h"
#include "src/graphics/display/lib/api-types/cpp/image-buffer-usage.h"
#include "src/graphics/display/lib/api-types/cpp/image-metadata.h"
#include "src/graphics/display/lib/api-types/cpp/image-tiling-type.h"
#include "src/graphics/display/lib/api-types/cpp/layer-composition-operations.h"
#include "src/graphics/display/lib/api-types/cpp/mode-and-id.h"
#include "src/graphics/display/lib/api-types/cpp/mode-id.h"
#include "src/graphics/display/lib/api-types/cpp/mode.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"
#include "src/graphics/display/lib/api-types/cpp/rectangle.h"
#include "src/graphics/lib/virtio/virtio-abi.h"

namespace virtio_display {

namespace {

constexpr display::EngineInfo kEngineInfo({
    .max_layer_count = 1,
    .max_connected_display_count = 1,
    .is_capture_supported = false,
});

// TODO(https://fxbug.dev/42073721): Support more formats.
constexpr display::PixelFormat kSupportedPixelFormat = display::PixelFormat::kB8G8R8A8;
constexpr uint32_t kRefreshRateHz = 30;
constexpr display::DisplayId kDisplayId(1);
constexpr display::ModeId kDisplayModeId(1);

}  // namespace

display::EngineInfo DisplayEngine::CompleteCoordinatorConnection() {
  const display::ModeAndId mode_and_id({
      .id = kDisplayModeId,
      .mode = display::Mode({
          .active_width = static_cast<int32_t>(current_display_.scanout_info.geometry.width),
          .active_height = static_cast<int32_t>(current_display_.scanout_info.geometry.height),
          .refresh_rate_millihertz = kRefreshRateHz * 1'000,
      }),
  });

  const cpp20::span<const display::ModeAndId> preferred_modes(&mode_and_id, 1);
  const cpp20::span<const display::PixelFormat> pixel_formats(&kSupportedPixelFormat, 1);
  engine_events_.OnDisplayAdded(kDisplayId, preferred_modes, current_display_edid_bytes_,
                                pixel_formats);

  return kEngineInfo;
}

zx::result<> DisplayEngine::ImportBufferCollection(
    display::DriverBufferCollectionId buffer_collection_id,
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> buffer_collection_token) {
  return imported_images_.ImportBufferCollection(buffer_collection_id,
                                                 std::move(buffer_collection_token));
}

zx::result<> DisplayEngine::ReleaseBufferCollection(
    display::DriverBufferCollectionId buffer_collection_id) {
  return imported_images_.ReleaseBufferCollection(buffer_collection_id);
}

zx::result<display::DriverImageId> DisplayEngine::ImportImage(
    const display::ImageMetadata& image_metadata,
    display::DriverBufferCollectionId buffer_collection_id, uint32_t buffer_index) {
  if (image_metadata.tiling_type() != display::ImageTilingType::kLinear) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  zx::result<display::DriverImageId> image_id_result =
      imported_images_.ImportImage(buffer_collection_id, buffer_index);
  if (image_id_result.is_error()) {
    // ImportImage() already logged the error.
    return image_id_result.take_error();
  }

  const display::DriverImageId image_id = image_id_result.value();
  SysmemBufferInfo* sysmem_buffer_info = imported_images_.FindSysmemInfoById(image_id);
  ZX_DEBUG_ASSERT(sysmem_buffer_info != nullptr);

  ZX_DEBUG_ASSERT(sysmem_buffer_info->pixel_format == kSupportedPixelFormat);
  static constexpr int kBytesPerPixel = 4;

  ZX_DEBUG_ASSERT(sysmem_buffer_info->pixel_format_modifier ==
                  fuchsia_images2::wire::PixelFormatModifier::kLinear);

  size_t image_size = static_cast<size_t>(image_metadata.width()) *
                      static_cast<size_t>(image_metadata.height()) * kBytesPerPixel;

  zx::result<ImportedImage> imported_image_result =
      ImportedImage::Create(gpu_device_->bti(), sysmem_buffer_info->image_vmo,
                            sysmem_buffer_info->image_vmo_offset, image_size);
  if (imported_image_result.is_error()) {
    // Create() already logged the error.
    return imported_image_result.take_error();
  }

  ImportedImage* imported_image = imported_images_.FindImageById(image_id);
  ZX_DEBUG_ASSERT(imported_image != nullptr);
  *imported_image = std::move(imported_image_result).value();

  zx::result<uint32_t> create_resource_result = gpu_device_->Create2DResource(
      image_metadata.width(), image_metadata.height(), sysmem_buffer_info->pixel_format);
  if (create_resource_result.is_error()) {
    fdf::error("Failed to allocate 2D resource: {}", create_resource_result);
    return create_resource_result.take_error();
  }
  imported_image->set_virtio_resource_id(create_resource_result.value());

  zx::result<> attach_result = gpu_device_->AttachResourceBacking(
      imported_image->virtio_resource_id(), imported_image->physical_address(), image_size);
  if (attach_result.is_error()) {
    fdf::error("Failed to attach resource backing store: {}", attach_result);
    return attach_result.take_error();
  }

  return zx::ok(image_id);
}

zx::result<display::DriverCaptureImageId> DisplayEngine::ImportImageForCapture(
    display::DriverBufferCollectionId driver_buffer_collection_id, uint32_t index) {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

void DisplayEngine::ReleaseImage(display::DriverImageId image_id) {
  zx::result result = imported_images_.ReleaseImage(image_id);
  if (result.is_error()) {
    // ReleaseImage() already logged the error.
    // The display coordinator API does not have error reporting for this call.
    return;
  }
}

display::ConfigCheckResult DisplayEngine::CheckConfiguration(
    display::DisplayId display_id, display::ModeId display_mode_id,
    cpp20::span<const display::DriverLayer> layers,
    cpp20::span<display::LayerCompositionOperations> layer_composition_operations) {
  ZX_DEBUG_ASSERT(display_id == kDisplayId);

  ZX_DEBUG_ASSERT(layer_composition_operations.size() == layers.size());
  ZX_DEBUG_ASSERT(layers.size() == 1);

  if (display_mode_id != kDisplayModeId) {
    return display::ConfigCheckResult::kUnsupportedDisplayModes;
  }

  const display::DriverLayer& layer = layers[0];
  const display::Rectangle display_area({
      .x = 0,
      .y = 0,
      .width = static_cast<int32_t>(current_display_.scanout_info.geometry.width),
      .height = static_cast<int32_t>(current_display_.scanout_info.geometry.height),
  });

  display::ConfigCheckResult result = display::ConfigCheckResult::kOk;
  if (layer.display_destination() != display_area) {
    // TODO(https://fxbug.dev/388602122): Revise the definition of MERGE to
    // include this case, or replace with a different opcode.
    layer_composition_operations[0] = layer_composition_operations[0].WithMerge();
    result = display::ConfigCheckResult::kUnsupportedConfig;
  }
  if (layer.image_source() != layer.display_destination()) {
    layer_composition_operations[0] = layer_composition_operations[0].WithFrameScale();
    result = display::ConfigCheckResult::kUnsupportedConfig;
  }
  if (layer.image_metadata().dimensions() != layer.image_source().dimensions()) {
    layer_composition_operations[0] = layer_composition_operations[0].WithSrcFrame();
    result = display::ConfigCheckResult::kUnsupportedConfig;
  }
  if (layer.alpha_mode() != display::AlphaMode::kDisable) {
    layer_composition_operations[0] = layer_composition_operations[0].WithAlpha();
    result = display::ConfigCheckResult::kUnsupportedConfig;
  }
  if (layer.image_source_transformation() != display::CoordinateTransformation::kIdentity) {
    layer_composition_operations[0] = layer_composition_operations[0].WithTransform();
    result = display::ConfigCheckResult::kUnsupportedConfig;
  }
  return result;
}

void DisplayEngine::ApplyConfiguration(display::DisplayId display_id,
                                       display::ModeId display_mode_id,
                                       cpp20::span<const display::DriverLayer> layers,
                                       display::DriverConfigStamp config_stamp) {
  ZX_DEBUG_ASSERT(display_id == kDisplayId);
  ZX_DEBUG_ASSERT(display_mode_id == kDisplayModeId);

  ZX_DEBUG_ASSERT(layers.size() == 1);
  const display::DriverImageId image_id = layers[0].image_id();
  const ImportedImage* imported_image = imported_images_.FindImageById(image_id);
  if (imported_image == nullptr) {
    fdf::error("ApplyConfiguration() used invalid image ID");
    return;
  }

  {
    fbl::AutoLock al(&flush_lock_);
    latest_framebuffer_resource_id_ = imported_image->virtio_resource_id();
    latest_config_stamp_ = config_stamp;
  }
}

zx::result<> DisplayEngine::SetBufferCollectionConstraints(
    const display::ImageBufferUsage& image_buffer_usage,
    display::DriverBufferCollectionId buffer_collection_id) {
  ImportedBufferCollection* imported_buffer_collection =
      imported_images_.FindBufferCollectionById(buffer_collection_id);
  if (imported_buffer_collection == nullptr) {
    fdf::warn("Rejected request to set constraints on BufferCollection with unknown ID: {}",
              buffer_collection_id.value());
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  // TODO(costan): fidl::Arena may allocate memory and crash. Find a way to get
  // control over memory allocation.
  fidl::Arena arena;
  auto buffer_collection_constraints_builder =
      fuchsia_sysmem2::wire::BufferCollectionConstraints::Builder(arena);
  buffer_collection_constraints_builder.usage(
      fuchsia_sysmem2::wire::BufferUsage::Builder(arena)
          .display(fuchsia_sysmem2::wire::kDisplayUsageLayer)
          .Build());
  buffer_collection_constraints_builder.buffer_memory_constraints(
      fuchsia_sysmem2::wire::BufferMemoryConstraints::Builder(arena)
          .min_size_bytes(0)
          .max_size_bytes(std::numeric_limits<uint32_t>::max())
          .physically_contiguous_required(true)
          .secure_required(false)
          .ram_domain_supported(true)
          .cpu_domain_supported(true)
          .Build());

  const fuchsia_sysmem2::wire::ImageFormatConstraints image_format_constraints[] = {
      fuchsia_sysmem2::wire::ImageFormatConstraints::Builder(arena)
          .pixel_format(kSupportedPixelFormat.ToFidl())
          .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
          .color_spaces(std::array{fuchsia_images2::wire::ColorSpace::kSrgb})
          .bytes_per_row_divisor(4)
          .Build()};
  buffer_collection_constraints_builder.image_format_constraints(image_format_constraints);

  fidl::OneWayStatus set_constraints_status =
      imported_buffer_collection->sysmem_client()->SetConstraints(
          fuchsia_sysmem2::wire::BufferCollectionSetConstraintsRequest::Builder(arena)
              .constraints(buffer_collection_constraints_builder.Build())
              .Build());
  if (!set_constraints_status.ok()) {
    fdf::error("SetConstraints() FIDL call failed: {}", set_constraints_status.status_string());
    return zx::error(set_constraints_status.status());
  }

  return zx::ok();
}

zx::result<> DisplayEngine::SetDisplayPower(display::DisplayId display_id, bool power_on) {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<> DisplayEngine::StartCapture(display::DriverCaptureImageId capture_image_id) {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<> DisplayEngine::ReleaseCapture(display::DriverCaptureImageId capture_image_id) {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<> DisplayEngine::SetMinimumRgb(uint8_t minimum_rgb) {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

DisplayEngine::DisplayEngine(display::DisplayEngineEventsInterface* engine_events,
                             fidl::ClientEnd<fuchsia_sysmem2::Allocator> sysmem_client,
                             std::unique_ptr<VirtioGpuDevice> gpu_device)
    : imported_images_(std::move(sysmem_client)),
      engine_events_(*engine_events),
      gpu_device_(std::move(gpu_device)) {
  ZX_DEBUG_ASSERT(engine_events != nullptr);
  ZX_DEBUG_ASSERT(gpu_device_);
}

DisplayEngine::~DisplayEngine() = default;

// static
zx::result<std::unique_ptr<DisplayEngine>> DisplayEngine::Create(
    fidl::ClientEnd<fuchsia_sysmem2::Allocator> sysmem_client, zx::bti bti,
    std::unique_ptr<virtio::Backend> backend,
    display::DisplayEngineEventsInterface* engine_events) {
  zx::result<std::unique_ptr<VirtioPciDevice>> virtio_device_result =
      VirtioPciDevice::Create(std::move(bti), std::move(backend));
  if (!virtio_device_result.is_ok()) {
    // VirtioPciDevice::Create() logs on error.
    return virtio_device_result.take_error();
  }

  fbl::AllocChecker alloc_checker;
  auto gpu_device = fbl::make_unique_checked<VirtioGpuDevice>(
      &alloc_checker, std::move(virtio_device_result).value());
  if (!alloc_checker.check()) {
    fdf::error("Failed to allocate memory for VirtioGpuDevice");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  auto display_engine = fbl::make_unique_checked<DisplayEngine>(
      &alloc_checker, engine_events, std::move(sysmem_client), std::move(gpu_device));
  if (!alloc_checker.check()) {
    fdf::error("Failed to allocate memory for DisplayEngine");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  zx_status_t status = display_engine->Init();
  if (status != ZX_OK) {
    fdf::error("Failed to initialize device");
    return zx::error(status);
  }

  return zx::ok(std::move(display_engine));
}

void DisplayEngine::virtio_gpu_flusher() {
  fdf::trace("Entering VirtioGpuFlusher()");

  zx_time_t next_deadline = zx_clock_get_monotonic();
  zx_time_t period = ZX_SEC(1) / kRefreshRateHz;
  for (;;) {
    zx_nanosleep(next_deadline);

    bool fb_change;
    {
      fbl::AutoLock al(&flush_lock_);
      fb_change = displayed_framebuffer_resource_id_ != latest_framebuffer_resource_id_;
      displayed_framebuffer_resource_id_ = latest_framebuffer_resource_id_;
      displayed_config_stamp_ = latest_config_stamp_;
    }

    fdf::trace("flushing");

    if (fb_change) {
      uint32_t resource_id = displayed_framebuffer_resource_id_ ? displayed_framebuffer_resource_id_
                                                                : virtio_abi::kInvalidResourceId;
      zx::result<> set_scanout_result = gpu_device_->SetScanoutProperties(
          current_display_.scanout_id, resource_id, current_display_.scanout_info.geometry.width,
          current_display_.scanout_info.geometry.height);
      if (set_scanout_result.is_error()) {
        fdf::error("Failed to set scanout: {}", set_scanout_result);
        continue;
      }
    }

    if (displayed_framebuffer_resource_id_) {
      zx::result<> transfer_result = gpu_device_->TransferToHost2D(
          displayed_framebuffer_resource_id_, current_display_.scanout_info.geometry.width,
          current_display_.scanout_info.geometry.height);
      if (transfer_result.is_error()) {
        fdf::error("Failed to transfer resource: {}", transfer_result);
        continue;
      }

      zx::result<> flush_result = gpu_device_->FlushResource(
          displayed_framebuffer_resource_id_, current_display_.scanout_info.geometry.width,
          current_display_.scanout_info.geometry.height);
      if (flush_result.is_error()) {
        fdf::error("Failed to flush resource: {}", flush_result);
        continue;
      }
    }

    {
      fbl::AutoLock al(&flush_lock_);
      engine_events_.OnDisplayVsync(kDisplayId, zx::time(next_deadline), displayed_config_stamp_);
    }
    next_deadline = zx_time_add_duration(next_deadline, period);
  }
}

zx_status_t DisplayEngine::Start() {
  fdf::trace("Start()");

  // virtio13 5.7.5 "Device Requirements: Device Initialization"

  zx::result<fbl::Vector<DisplayInfo>> display_infos_result = gpu_device_->GetDisplayInfo();
  if (display_infos_result.is_error()) {
    fdf::error("Failed to get display info: {}", display_infos_result);
    return display_infos_result.error_value();
  }

  const DisplayInfo* current_display = FirstValidDisplay(display_infos_result.value());
  if (current_display == nullptr) {
    fdf::error("Failed to find a usable display");
    return ZX_ERR_NOT_FOUND;
  }
  current_display_ = *current_display;

  zx::result<fbl::Vector<uint8_t>> display_edid_result =
      gpu_device_->GetDisplayEdid(current_display->scanout_id);

  // EDID support is optional, and the driver can proceed without it.
  if (display_edid_result.is_ok()) {
    current_display_edid_bytes_ = std::move(display_edid_result).value();
  }

  fdf::info("Found display at ({}, {}) size {}x{}, flags 0x{:08x}",
            current_display_.scanout_info.geometry.x, current_display_.scanout_info.geometry.y,
            current_display_.scanout_info.geometry.width,
            current_display_.scanout_info.geometry.height, current_display_.scanout_info.flags);
  LogEdidBytes();

  // Set the mouse cursor position to (0,0); the result is not critical.
  zx::result<uint32_t> move_cursor_result =
      gpu_device_->SetCursorPosition(current_display_.scanout_id, 0, 0);
  if (move_cursor_result.is_error()) {
    fdf::warn("Failed to move cursor: {}", move_cursor_result);
  }

  // Run a worker thread to shove in flush events
  auto virtio_gpu_flusher_entry = [](void* arg) {
    static_cast<DisplayEngine*>(arg)->virtio_gpu_flusher();
    return 0;
  };
  thrd_create(&flush_thread_, virtio_gpu_flusher_entry, this);
  thrd_detach(flush_thread_);

  fdf::trace("Start() completed");
  return ZX_OK;
}

const DisplayInfo* DisplayEngine::FirstValidDisplay(cpp20::span<const DisplayInfo> display_infos) {
  return display_infos.empty() ? nullptr : &display_infos.front();
}

zx_status_t DisplayEngine::Init() {
  fdf::trace("Init()");

  zx::result<> imported_images_init_result = imported_images_.Initialize();
  if (imported_images_init_result.is_error()) {
    // Initialize() already logged the error.
    return imported_images_init_result.error_value();
  }

  return ZX_OK;
}

void DisplayEngine::LogEdidBytes() {
  if (current_display_edid_bytes_.is_empty()) {
    fdf::info("EDID not available");
    return;
  }

  if constexpr (ZX_DEBUG_ASSERT_IMPLEMENTED) {
    std::span<const uint8_t> bytes(current_display_edid_bytes_);

    // The virtio-gpu implementation in QEmu 9.2 reports a zero-padded EDID that
    // takes up the maximum buffer size in the virtio-gpu 1.3 specification.
    //
    // Trimming the trailing zeros significantly reduces the log output size.
    const size_t original_size = bytes.size();
    while (!bytes.empty() && bytes.back() == 0) {
      bytes = bytes.subspan(0, bytes.size() - 1);
    }

    fdf::info("--- BEGIN EDID DATA: {} BYTES ---", original_size);
    for (size_t line_start = 0; line_start < bytes.size();) {
      // The logger truncates lines that exceed 1,024 bytes. We pack the bytes
      // as compactly as possible, while meeting the constraint of mapping to
      // the C++ initializer syntax used in our unit tests.
      static constexpr int kMaxLoggingLineSize = 1020;
      // Each byte is logged using 6 characters -- "0xcc, ".
      static constexpr int kByteLoggingSize = 6;
      static constexpr int kMaxLineBytes = kMaxLoggingLineSize / kByteLoggingSize;
      const size_t line_byte_count = std::min<size_t>(kMaxLineBytes, bytes.size() - line_start);
      fbl::StringBuffer<1020> line;
      for (size_t i = 0; i < line_byte_count; ++i) {
        line.AppendPrintf("0x%02x, ", bytes[line_start + i]);
      }
      fdf::info("{}", line.c_str());
      line_start += line_byte_count;
    }
    fdf::info("--- END EDID DATA: {} BYTES; SKIPPED {} ZERO BYTES ---", original_size,
              original_size - bytes.size());
  } else {
    fdf::info("EDID available, uses {} bytes", current_display_edid_bytes_.size());
  }
}

}  // namespace virtio_display
