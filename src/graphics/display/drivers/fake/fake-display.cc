// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/fake/fake-display.h"

#include <fidl/fuchsia.images2/cpp/wire.h>
#include <fidl/fuchsia.math/cpp/wire.h>
#include <fidl/fuchsia.sysmem2/cpp/wire.h>
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
#include <cstddef>
#include <cstdint>
#include <format>
#include <initializer_list>
#include <limits>
#include <mutex>
#include <string>
#include <utility>
#include <vector>

#include "src/graphics/display/drivers/coordinator/preferred-scanout-image-type.h"
#include "src/graphics/display/drivers/fake/image-info.h"
#include "src/graphics/display/lib/api-protocols/cpp/display-engine-events-interface.h"
#include "src/graphics/display/lib/api-types/cpp/alpha-mode.h"
#include "src/graphics/display/lib/api-types/cpp/color.h"
#include "src/graphics/display/lib/api-types/cpp/config-check-result.h"
#include "src/graphics/display/lib/api-types/cpp/coordinate-transformation.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-capture-image-id.h"
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
#include "src/lib/fsl/handles/object_info.h"

namespace fake_display {

namespace {

// List of supported pixel formats.
constexpr auto kSupportedPixelFormats = std::to_array<display::PixelFormat>({
    display::PixelFormat::kB8G8R8A8,
    display::PixelFormat::kR8G8B8A8,
});
constexpr auto kSupportedFormatModifiers =
    std::to_array<fuchsia_images2::wire::PixelFormatModifier>({
        fuchsia_images2::wire::PixelFormatModifier::kLinear,
        fuchsia_images2::wire::PixelFormatModifier::kGoogleGoldfishOptimal,
    });
constexpr fuchsia_images2::wire::ColorSpace kSupportedColorSpaces[] = {
    fuchsia_images2::wire::ColorSpace::kSrgb,
};

// Arbitrary dimensions - the same as sherlock.
constexpr int32_t kWidth = 1280;
constexpr int32_t kHeight = 800;

constexpr display::DisplayId kDisplayId(1);
constexpr display::ModeId kDisplayModeId(1);

constexpr int32_t kRefreshRateHz = 60;

}  // namespace

FakeDisplay::FakeDisplay(display::DisplayEngineEventsInterface* engine_events,
                         fidl::ClientEnd<fuchsia_sysmem2::Allocator> sysmem_client,
                         const FakeDisplayDeviceConfig& device_config, inspect::Inspector inspector)
    : engine_events_(*engine_events),
      device_config_(device_config),
      sysmem_client_(std::move(sysmem_client)),
      applied_fallback_color_({
          .format = display::PixelFormat::kB8G8R8A8,
          .bytes = std::initializer_list<uint8_t>{0, 0, 0, 0, 0, 0, 0, 0},
      }),
      inspector_(std::move(inspector)) {
  ZX_DEBUG_ASSERT(engine_events != nullptr);
  ZX_DEBUG_ASSERT(sysmem_client_.is_valid());
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

zx::result<> FakeDisplay::SetMinimumRgb(uint8_t minimum_rgb) {
  std::lock_guard lock(mutex_);

  clamp_rgb_value_ = minimum_rgb;
  return zx::ok();
}

void FakeDisplay::InitializeSysmemClient() {
  zx_koid_t koid = fsl::GetCurrentProcessKoid();

  std::string debug_name = std::format("virtio-gpu-display[{}]", koid);

  fidl::Arena arena;
  std::lock_guard lock(mutex_);
  fidl::OneWayStatus set_debug_status = sysmem_client_->SetDebugClientInfo(
      fuchsia_sysmem2::wire::AllocatorSetDebugClientInfoRequest::Builder(arena)
          .name(fidl::StringView::FromExternal(debug_name))
          .id(koid)
          .Build());
  if (!set_debug_status.ok()) {
    // Errors here mean that the FIDL transport was not set up correctly, and
    // all future Sysmem client calls will fail. Crashing here exposes the
    // failure early.
    fdf::fatal("SetDebugClientInfo() FIDL call failed: {}", set_debug_status.status_string());
  }
}

display::EngineInfo FakeDisplay::CompleteCoordinatorConnection() {
  const display::ModeAndId mode_and_id({
      .id = kDisplayModeId,
      .mode = display::Mode({
          .active_width = kWidth,
          .active_height = kHeight,
          .refresh_rate_millihertz = kRefreshRateHz * 1'000,
      }),
  });

  const cpp20::span<const display::ModeAndId> preferred_modes(&mode_and_id, 1);
  engine_events_.OnDisplayAdded(kDisplayId, preferred_modes, kSupportedPixelFormats);

  return display::EngineInfo({
      .max_layer_count = 1,
      .max_connected_display_count = 1,
      .is_capture_supported = IsCaptureSupported(),
  });
}

zx::result<display::DriverImageId> FakeDisplay::ImportVmoImageForTesting(zx::vmo vmo,
                                                                         size_t vmo_offset) {
  std::lock_guard lock(mutex_);

  display::DriverImageId driver_image_id = next_imported_display_driver_image_id_++;

  // Image metadata for testing only and may not reflect the actual image
  // buffer format.
  SysmemBufferInfo sysmem_buffer_info = {
      .image_vmo = std::move(vmo),
      .image_vmo_offset = vmo_offset,
      .pixel_format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .pixel_format_modifier = fuchsia_images2::wire::PixelFormatModifier::kLinear,
      .minimum_size = fuchsia_math::wire::SizeU{.width = 0, .height = 0},
      .minimum_bytes_per_row = 0,
      .coherency_domain = fuchsia_sysmem2::wire::CoherencyDomain::kRam,
  };

  auto import_info =
      std::make_unique<DisplayImageInfo>(driver_image_id, std::move(sysmem_buffer_info));

  imported_images_.insert(std::move(import_info));
  return zx::ok(driver_image_id);
}

namespace {

bool IsAcceptableImageTilingType(display::ImageTilingType image_tiling_type) {
  return image_tiling_type == display::ImageTilingType::kLinear ||
         image_tiling_type.ToFidl() == IMAGE_TILING_TYPE_PREFERRED_SCANOUT;
}

}  // namespace

zx::result<> FakeDisplay::ImportBufferCollection(
    display::DriverBufferCollectionId buffer_collection_id,
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> buffer_collection_token) {
  std::lock_guard lock(mutex_);

  auto buffer_collection_it = buffer_collections_.find(buffer_collection_id);
  if (buffer_collection_it != buffer_collections_.end()) {
    fdf::warn("Rejected BufferCollection import request with existing ID: {}",
              buffer_collection_id.value());
    return zx::error(ZX_ERR_ALREADY_EXISTS);
  }

  auto [collection_client_endpoint, collection_server_endpoint] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();

  // TODO(costan): fidl::Arena may allocate memory and crash. Find a way to get
  // control over memory allocation.
  fidl::Arena arena;
  fidl::OneWayStatus bind_result = sysmem_client_->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(std::move(buffer_collection_token))
          .buffer_collection_request(std::move(collection_server_endpoint))
          .Build());
  if (!bind_result.ok()) {
    fdf::error("FIDL call BindSharedCollection failed: {}", bind_result.status_string());
    return zx::error(ZX_ERR_INTERNAL);
  }

  buffer_collections_.insert(
      buffer_collection_it,
      std::make_pair(buffer_collection_id, fidl::WireSyncClient<fuchsia_sysmem2::BufferCollection>(
                                               std::move(collection_client_endpoint))));

  return zx::ok();
}

zx::result<> FakeDisplay::ReleaseBufferCollection(
    display::DriverBufferCollectionId buffer_collection_id) {
  std::lock_guard lock(mutex_);

  auto buffer_collection_it = buffer_collections_.find(buffer_collection_id);
  if (buffer_collection_it == buffer_collections_.end()) {
    fdf::warn("Rejected request to release BufferCollection with unknown ID: {}",
              buffer_collection_id.value());
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  buffer_collections_.erase(buffer_collection_it);
  return zx::ok();
}

zx::result<display::DriverImageId> FakeDisplay::ImportImage(
    const display::ImageMetadata& image_metadata,
    display::DriverBufferCollectionId buffer_collection_id, uint32_t buffer_index) {
  std::lock_guard lock(mutex_);

  auto buffer_collection_it = buffer_collections_.find(buffer_collection_id);
  if (buffer_collection_it == buffer_collections_.end()) {
    fdf::error("ImportImage: Cannot find imported buffer collection ID: {}",
               buffer_collection_id.value());
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  fidl::WireSyncClient<fuchsia_sysmem2::BufferCollection>& buffer_collection =
      buffer_collection_it->second;

  if (!IsAcceptableImageTilingType(image_metadata.tiling_type())) {
    fdf::info("ImportImage: Invalid image tiling type: {}",
              image_metadata.tiling_type().ValueForLogging());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  zx::result<SysmemBufferInfo> sysmem_buffer_info_result =
      SysmemBufferInfo::GetSysmemMetadata(buffer_collection, buffer_index);
  if (sysmem_buffer_info_result.is_error()) {
    // SysmemBufferInfo::GetSysmemMetadata() has already logged the error.
    return sysmem_buffer_info_result.take_error();
  }

  // TODO(https://fxbug.dev/42079320): When capture is enabled
  // (IsCaptureSupported() is true), we should perform a check to ensure that
  // the display images should not be of "inaccessible" coherency domain.

  display::DriverImageId driver_image_id = next_imported_display_driver_image_id_++;

  auto display_image_info = std::make_unique<DisplayImageInfo>(
      driver_image_id, std::move(sysmem_buffer_info_result).value());

  imported_images_.insert(std::move(display_image_info));
  return zx::ok(driver_image_id);
}

void FakeDisplay::ReleaseImage(display::DriverImageId image_id) {
  std::lock_guard lock(mutex_);

  if (applied_image_id_ == image_id) {
    fdf::fatal("Cannot safely release an image used in currently applied configuration");
    return;
  }

  auto image_it = imported_images_.find(image_id);
  if (image_it == imported_images_.end()) {
    fdf::warn("Rejected request to release Image with unknown ID: {}", image_id.value());
    return;
  }
  imported_images_.erase(image_it);
}

display::ConfigCheckResult FakeDisplay::CheckConfiguration(
    display::DisplayId display_id, display::ModeId display_mode_id,
    cpp20::span<const display::DriverLayer> layers,
    cpp20::span<display::LayerCompositionOperations> layer_composition_operations) {
  ZX_DEBUG_ASSERT(display_id == kDisplayId);

  ZX_DEBUG_ASSERT(layer_composition_operations.size() == layers.size());

  // TODO(https://fxbug.dev/412450577): Remove the single-layer assumption.
  ZX_DEBUG_ASSERT(layers.size() == 1);

  if (display_mode_id != kDisplayModeId) {
    return display::ConfigCheckResult::kUnsupportedDisplayModes;
  }

  const display::DriverLayer& layer = layers[0];
  const display::Rectangle display_area({
      .x = 0,
      .y = 0,
      .width = kWidth,
      .height = kHeight,
  });

  display::ConfigCheckResult result = display::ConfigCheckResult::kOk;
  if (layer.display_destination() != display_area) {
    // TODO(https://fxbug.dev/388602122): Revise the definition of MERGE to
    // include this case, or replace with a different opcode.
    layer_composition_operations[0] = layer_composition_operations[0].WithMerge();
    result = display::ConfigCheckResult::kUnsupportedConfig;
  }
  if (layer.image_source().dimensions().IsEmpty()) {
    // Solid color fill layer.
    if (layer.fallback_color().format().EncodingSize() != sizeof(uint32_t)) {
      // The capture simulation implementation is currently optimized for 32-bit
      // colors. Removing this constraint will require updating that
      // implementation.
      layer_composition_operations[0] = layer_composition_operations[0].WithUseImage();
      result = display::ConfigCheckResult::kUnsupportedConfig;
    }
  } else {
    // Image layer.
    if (layer.image_source() != layer.display_destination()) {
      layer_composition_operations[0] = layer_composition_operations[0].WithFrameScale();
      result = display::ConfigCheckResult::kUnsupportedConfig;
    }
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

void FakeDisplay::ApplyConfiguration(display::DisplayId display_id, display::ModeId display_mode_id,
                                     cpp20::span<const display::DriverLayer> layers,
                                     display::DriverConfigStamp config_stamp) {
  ZX_DEBUG_ASSERT(display_id == kDisplayId);
  ZX_DEBUG_ASSERT(display_mode_id == kDisplayModeId);
  ZX_DEBUG_ASSERT(config_stamp != display::kInvalidDriverConfigStamp);

  ZX_DEBUG_ASSERT(layers.size() == 1);
  std::lock_guard lock(mutex_);

  if (layers[0].image_id() != display::kInvalidDriverImageId) {
    ZX_DEBUG_ASSERT_MSG(imported_images_.find(layers[0].image_id()) != imported_images_.end(),
                        "Configuration contains invalid image ID: %" PRIu64,
                        layers[0].image_id().value());
  }
  applied_image_id_ = layers[0].image_id();
  applied_fallback_color_ = layers[0].fallback_color();

  applied_config_stamp_ = config_stamp;
}

enum class FakeDisplay::BufferCollectionUsage {
  kPrimaryLayer = 1,
  kCapture = 2,
};

namespace {

fuchsia_sysmem2::wire::BufferMemoryConstraints CreateBufferMemoryConstraints(
    fidl::AnyArena& arena) {
  return fuchsia_sysmem2::wire::BufferMemoryConstraints::Builder(arena)
      .min_size_bytes(0)
      .max_size_bytes(std::numeric_limits<uint32_t>::max())
      .physically_contiguous_required(false)
      .secure_required(false)
      .ram_domain_supported(true)
      .cpu_domain_supported(true)
      .inaccessible_domain_supported(true)
      .Build();
}

void SetLayerImageFormatConstraints(
    fidl::WireTableBuilder<fuchsia_sysmem2::wire::ImageFormatConstraints>& constraints_builder) {
  constraints_builder.min_size(fuchsia_math::wire::SizeU{.width = 0, .height = 0})
      .max_size(fuchsia_math::wire::SizeU{.width = std::numeric_limits<uint32_t>::max(),
                                          .height = std::numeric_limits<uint32_t>::max()})
      .min_bytes_per_row(0)
      .max_bytes_per_row(std::numeric_limits<uint32_t>::max())
      .max_width_times_height(std::numeric_limits<uint32_t>::max());
}

}  // namespace

void FakeDisplay::SetCaptureImageFormatConstraints(
    fidl::WireTableBuilder<fuchsia_sysmem2::wire::ImageFormatConstraints>& constraints_builder) {
  constraints_builder.min_size(fuchsia_math::wire::SizeU{.width = kWidth, .height = kHeight})
      .max_size(fuchsia_math::wire::SizeU{.width = kWidth, .height = kHeight})
      .min_bytes_per_row(kWidth * 4)
      .max_bytes_per_row(kWidth * 4)
      .max_width_times_height(kWidth * kHeight);
}

fuchsia_sysmem2::wire::BufferCollectionConstraints FakeDisplay::CreateBufferCollectionConstraints(
    BufferCollectionUsage usage, fidl::AnyArena& arena) {
  fidl::WireTableBuilder<fuchsia_sysmem2::wire::BufferCollectionConstraints> constraints_builder =
      fuchsia_sysmem2::wire::BufferCollectionConstraints::Builder(arena);

  fidl::WireTableBuilder<fuchsia_sysmem2::wire::BufferUsage> usage_builder =
      fuchsia_sysmem2::wire::BufferUsage::Builder(arena);
  switch (usage) {
    case BufferCollectionUsage::kCapture:
      usage_builder.cpu(fuchsia_sysmem2::kCpuUsageReadOften | fuchsia_sysmem2::kCpuUsageWriteOften);
      break;
    case BufferCollectionUsage::kPrimaryLayer:
      if (IsCaptureSupported()) {
        usage_builder.cpu(fuchsia_sysmem2::kCpuUsageReadOften);
      }
      usage_builder.display(fuchsia_sysmem2::kDisplayUsageLayer);
      break;
  }
  constraints_builder.usage(usage_builder.Build());

  // TODO(https://fxbug.dev/42079320): In order to support capture, both capture sources
  // and capture targets must not be in the "inaccessible" coherency domain.
  constraints_builder.buffer_memory_constraints(CreateBufferMemoryConstraints(arena));

  std::vector<fuchsia_sysmem2::wire::ImageFormatConstraints> image_format_constraints;
  image_format_constraints.reserve(kSupportedPixelFormats.size() *
                                   kSupportedFormatModifiers.size());
  for (display::PixelFormat pixel_format : kSupportedPixelFormats) {
    for (fuchsia_images2::wire::PixelFormatModifier format_modifier : kSupportedFormatModifiers) {
      fidl::WireTableBuilder<fuchsia_sysmem2::wire::ImageFormatConstraints>
          image_constraints_builder = fuchsia_sysmem2::wire::ImageFormatConstraints::Builder(arena);
      image_constraints_builder.pixel_format(pixel_format.ToFidl())
          .pixel_format_modifier(format_modifier)
          .color_spaces(kSupportedColorSpaces)
          .size_alignment(fuchsia_math::wire::SizeU{.width = 1, .height = 1})
          .bytes_per_row_divisor(1)
          .start_offset_divisor(1)
          .display_rect_alignment(fuchsia_math::wire::SizeU{.width = 1, .height = 1});
      switch (usage) {
        case BufferCollectionUsage::kCapture:
          SetCaptureImageFormatConstraints(image_constraints_builder);
          break;
        case BufferCollectionUsage::kPrimaryLayer:
          SetLayerImageFormatConstraints(image_constraints_builder);
          break;
      }
      image_format_constraints.push_back(image_constraints_builder.Build());
    }
  }
  constraints_builder.image_format_constraints(image_format_constraints);

  return constraints_builder.Build();
}

zx::result<> FakeDisplay::SetBufferCollectionConstraints(
    const display::ImageBufferUsage& image_buffer_usage,
    display::DriverBufferCollectionId buffer_collection_id) {
  std::lock_guard lock(mutex_);

  const auto buffer_collection_it = buffer_collections_.find(buffer_collection_id);
  if (buffer_collection_it == buffer_collections_.end()) {
    fdf::error("SetBufferCollectionConstraints: Cannot find imported buffer collection ID: {}",
               buffer_collection_id.value());
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  fidl::WireSyncClient<fuchsia_sysmem2::BufferCollection>& buffer_collection =
      buffer_collection_it->second;

  BufferCollectionUsage buffer_collection_usage =
      (image_buffer_usage.tiling_type == display::ImageTilingType::kCapture)
          ? BufferCollectionUsage::kCapture
          : BufferCollectionUsage::kPrimaryLayer;

  fidl::Arena arena;
  fidl::OneWayStatus set_constraints_status = buffer_collection->SetConstraints(
      fuchsia_sysmem2::wire::BufferCollectionSetConstraintsRequest::Builder(arena)
          .constraints(CreateBufferCollectionConstraints(buffer_collection_usage, arena))
          .Build());
  if (!set_constraints_status.ok()) {
    fdf::error("SetConstraints() FIDL call failed: {}", set_constraints_status.status_string());
    return zx::error(set_constraints_status.status());
  }

  return zx::ok();
}

zx::result<> FakeDisplay::SetDisplayPower(display::DisplayId display_id, bool power_on) {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<display::DriverCaptureImageId> FakeDisplay::ImportImageForCapture(
    display::DriverBufferCollectionId buffer_collection_id, uint32_t buffer_index) {
  if (!IsCaptureSupported()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  std::lock_guard lock(mutex_);

  auto buffer_collection_it = buffer_collections_.find(buffer_collection_id);
  if (buffer_collection_it == buffer_collections_.end()) {
    fdf::error("ImportImage: Cannot find imported buffer collection ID: {}",
               buffer_collection_id.value());
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  fidl::WireSyncClient<fuchsia_sysmem2::BufferCollection>& buffer_collection =
      buffer_collection_it->second;

  zx::result<SysmemBufferInfo> sysmem_buffer_info_result =
      SysmemBufferInfo::GetSysmemMetadata(buffer_collection, buffer_index);
  if (sysmem_buffer_info_result.is_error()) {
    // SysmemBufferInfo::GetSysmemMetadata() has already logged the error.
    return sysmem_buffer_info_result.take_error();
  }

  // TODO(https://fxbug.dev/42079320): Capture target images should not be of
  // "inaccessible" coherency domain. We should add a check here.
  display::DriverCaptureImageId driver_capture_image_id = next_imported_driver_capture_image_id_++;

  auto capture_image_info = std::make_unique<CaptureImageInfo>(
      driver_capture_image_id, std::move(sysmem_buffer_info_result).value());

  imported_captures_.insert(std::move(capture_image_info));
  return zx::ok(driver_capture_image_id);
}

zx::result<> FakeDisplay::StartCapture(display::DriverCaptureImageId capture_image_id) {
  if (!IsCaptureSupported()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  std::lock_guard lock(mutex_);

  if (started_capture_target_id_ != display::kInvalidDriverCaptureImageId) {
    fdf::error("Capture start request declined while a capture is already in-progress");
    return zx::error(ZX_ERR_SHOULD_WAIT);
  }

  // Confirm the handle was previously imported (hence valid)
  auto imported_capture_it = imported_captures_.find(capture_image_id);
  if (imported_capture_it == imported_captures_.end()) {
    fdf::error("Capture start request with invalid handle: {}", capture_image_id.value());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  started_capture_target_id_ = capture_image_id;
  return zx::ok();
}

zx::result<> FakeDisplay::ReleaseCapture(display::DriverCaptureImageId capture_image_id) {
  if (!IsCaptureSupported()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  std::lock_guard lock(mutex_);

  if (started_capture_target_id_ == capture_image_id) {
    fdf::fatal("Refusing to release the target of an in-progress capture");

    // TODO(https://fxrev.dev/394954078): The return code is not meaningful. It will be
    // removed when the ReleaseCapture() error code is eliminated.
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  if (imported_captures_.erase(capture_image_id) == nullptr) {
    fdf::error("Capture release request with unused handle: {}", capture_image_id.value());

    // TODO(https://fxrev.dev/394954078): The return code is not meaningful. It will be
    // removed when the ReleaseCapture() error code is eliminated.
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok();
}

bool FakeDisplay::IsCaptureSupported() const { return !device_config_.no_buffer_access; }

void FakeDisplay::CaptureThread() {
  ZX_DEBUG_ASSERT(IsCaptureSupported());

  while (!capture_thread_shutdown_requested_.load(std::memory_order_relaxed)) {
    [[maybe_unused]] zx::result<> capture_result = ServiceAnyCaptureRequest();
    // ServiceAnyCaptureRequest() has already logged the error.

    zx::nanosleep(zx::deadline_after(zx::sec(1) / kRefreshRateHz));
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

  engine_events_.OnCaptureComplete();

  started_capture_target_id_ = display::kInvalidDriverCaptureImageId;

  return zx::ok();
}

// static
zx::result<> FakeDisplay::DoImageCapture(DisplayImageInfo& source_info,
                                         CaptureImageInfo& destination_info) {
  if (source_info.sysmem_buffer_info().pixel_format !=
      destination_info.sysmem_buffer_info().pixel_format) {
    fdf::error("Capture will fail; trying to capture format={} as format={}\n",
               static_cast<uint32_t>(source_info.sysmem_buffer_info().pixel_format),
               static_cast<uint32_t>(destination_info.sysmem_buffer_info().pixel_format));
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

  if (source_info.sysmem_buffer_info().coherency_domain == fuchsia_sysmem2::CoherencyDomain::kRam) {
    zx_cache_flush(source_mapper.start(), source_vmo_size,
                   ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
  }
  std::ranges::copy(source_colors, destination_colors.begin());
  if (destination_info.sysmem_buffer_info().coherency_domain ==
      fuchsia_sysmem2::CoherencyDomain::kRam) {
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
  if (fill_color.format().ToFidl() != destination_info.sysmem_buffer_info().pixel_format) {
    fdf::error("Capture will fail; trying to capture format={} as format={}\n",
               fill_color.format().ValueForLogging(),
               static_cast<uint32_t>(destination_info.sysmem_buffer_info().pixel_format));
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
  if (destination_info.sysmem_buffer_info().coherency_domain ==
      fuchsia_sysmem2::CoherencyDomain::kRam) {
    zx_cache_flush(destination_mapper.start(), destination_vmo_size,
                   ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
  }

  return zx::ok();
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
    zx::nanosleep(zx::deadline_after(zx::sec(1) / kRefreshRateHz));
  }
}

void FakeDisplay::SendVsync() {
  zx::time vsync_timestamp = zx::clock::get_monotonic();

  display::DriverConfigStamp vsync_config_stamp;
  {
    std::lock_guard lock(mutex_);
    vsync_config_stamp = applied_config_stamp_;
  }
  if (vsync_config_stamp == display::kInvalidDriverConfigStamp) {
    // No configuration was applied yet.
    return;
  }

  engine_events_.OnDisplayVsync(kDisplayId, vsync_timestamp, vsync_config_stamp);
}

void FakeDisplay::RecordDisplayConfigToInspectRootNode() {
  inspect::Node& root_node = inspector_.GetRoot();
  ZX_ASSERT(root_node);
  root_node.RecordChild("device_config", [&](inspect::Node& config_node) {
    config_node.RecordInt("width_px", kWidth);
    config_node.RecordInt("height_px", kHeight);
    config_node.RecordDouble("refresh_rate_hz", kRefreshRateHz);
    config_node.RecordBool("periodic_vsync", device_config_.periodic_vsync);
    config_node.RecordBool("no_buffer_access", device_config_.no_buffer_access);
  });
}

}  // namespace fake_display
