// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_ADDED_DISPLAY_INFO_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_ADDED_DISPLAY_INFO_H_

#include <fuchsia/hardware/display/controller/c/banjo.h>
#include <lib/zx/result.h>

#include <cstdint>
#include <memory>

#include <fbl/vector.h>

#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"

namespace display_coordinator {

// Bundles engine driver information about a newly added display.
//
// This structure was designed to shuttle display information between
// dispatchers. It is not thread-safe, and it is not suitable for more
// complex processing.
struct AddedDisplayInfo {
  // Returns a valid instance.
  //
  // Fails with ZX_ERR_INVALID_ARGS if `banjo_display_info` cannot be used to
  // produce a valid instance. Fails with ZX_ERR_NO_MEMORY on OOM. All failures
  // result in logging.
  //
  // Instances are always created on the heap so they can be conveniently passed
  // between dispatchers.
  static zx::result<std::unique_ptr<AddedDisplayInfo>> Create(
      const raw_display_info_t& banjo_display_info);

  // TODO(https://fxbug.dev/314126494): Add Create() overload for FIDL clients.

  // Guaranteed to be valid in valid instances.
  //
  // This constraint is particularly important, because it lets us choose to
  // store instances in a dictionary keyed by `display_id` in the future.
  display::DisplayId display_id;

  // Empty if no EDID information is provided.
  fbl::Vector<uint8_t> edid_bytes;

  // Guaranteed to be non-empty in valid instances.
  fbl::Vector<display::PixelFormat> pixel_formats;

  // Empty if no preferred modes are provided.
  //
  // This data member uses a Banjo-generated type because the display::Mode
  // equivalent does not convey detailed timing information. We will be able to
  // remove this Banjo dependency once we move detailed timing information
  // management to engine / panel drivers.
  fbl::Vector<display_mode_t> banjo_preferred_modes;
};

}  // namespace display_coordinator

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_ADDED_DISPLAY_INFO_H_
