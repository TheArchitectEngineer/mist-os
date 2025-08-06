// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_DISPLAY_TILING_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_DISPLAY_TILING_H_

#include <fuchsia/hardware/intelgpucore/c/banjo.h>

#include <cinttypes>

#include <fbl/algorithm.h>

#include "src/graphics/display/lib/api-types/cpp/image-tiling-type.h"

namespace intel_display {

constexpr int get_tile_byte_width(display::ImageTilingType tiling) {
  if (tiling == display::ImageTilingType::kLinear) {
    return 64;
  }
  if (tiling == display::ImageTilingType(IMAGE_TILING_TYPE_X_TILED)) {
    return 512;
  }
  if (tiling == display::ImageTilingType(IMAGE_TILING_TYPE_Y_LEGACY_TILED)) {
    return 128;
  }
  if (tiling == display::ImageTilingType(IMAGE_TILING_TYPE_YF_TILED)) {
    // TODO(https://fxbug.dev/42076787): For 1-byte-per-pixel formats (e.g. R8), the
    // tile width is 64. We need to check the pixel format once we support
    // importing such formats.
    return 128;
  }
  ZX_ASSERT_MSG(false, "Unsupported tiling type: %" PRIu32, tiling.ValueForLogging());
  return 0;
}

constexpr int get_tile_byte_size(display::ImageTilingType tiling) {
  return tiling == display::ImageTilingType::kLinear ? 64 : 4096;
}

constexpr int get_tile_px_height(display::ImageTilingType tiling) {
  return get_tile_byte_size(tiling) / get_tile_byte_width(tiling);
}

constexpr uint32_t width_in_tiles(display::ImageTilingType tiling, int width, int bytes_per_pixel) {
  int tile_width = get_tile_byte_width(tiling);
  return ((width * bytes_per_pixel) + tile_width - 1) / tile_width;
}

constexpr uint32_t height_in_tiles(display::ImageTilingType tiling, int height) {
  int tile_height = get_tile_px_height(tiling);
  return (height + tile_height - 1) / tile_height;
}

}  // namespace intel_display

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_DISPLAY_TILING_H_
