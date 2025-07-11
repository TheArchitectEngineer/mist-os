// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/lib/api-types/cpp/coordinate-transformation.h"

#include <fuchsia/hardware/display/controller/c/banjo.h>

#include <type_traits>

namespace display {

static_assert(std::is_standard_layout_v<CoordinateTransformation>);
static_assert(std::is_trivially_assignable_v<CoordinateTransformation, CoordinateTransformation>);
static_assert(std::is_trivially_copyable_v<CoordinateTransformation>);
static_assert(std::is_trivially_copy_constructible_v<CoordinateTransformation>);
static_assert(std::is_trivially_destructible_v<CoordinateTransformation>);
static_assert(std::is_trivially_move_assignable_v<CoordinateTransformation>);
static_assert(std::is_trivially_move_constructible_v<CoordinateTransformation>);

// Ensure that the Banjo constants match the FIDL constants.
static_assert(CoordinateTransformation::kIdentity.ToBanjo() == COORDINATE_TRANSFORMATION_IDENTITY);
static_assert(CoordinateTransformation::kReflectX.ToBanjo() == COORDINATE_TRANSFORMATION_REFLECT_X);
static_assert(CoordinateTransformation::kReflectY.ToBanjo() == COORDINATE_TRANSFORMATION_REFLECT_Y);
static_assert(CoordinateTransformation::kRotateCcw90.ToBanjo() ==
              COORDINATE_TRANSFORMATION_ROTATE_CCW_90);
static_assert(CoordinateTransformation::kRotateCcw180.ToBanjo() ==
              COORDINATE_TRANSFORMATION_ROTATE_CCW_180);
static_assert(CoordinateTransformation::kRotateCcw270.ToBanjo() ==
              COORDINATE_TRANSFORMATION_ROTATE_CCW_270);
static_assert(CoordinateTransformation::kRotateCcw90ReflectX.ToBanjo() ==
              COORDINATE_TRANSFORMATION_ROTATE_CCW_90_REFLECT_X);
static_assert(CoordinateTransformation::kRotateCcw90ReflectY.ToBanjo() ==
              COORDINATE_TRANSFORMATION_ROTATE_CCW_90_REFLECT_Y);

}  // namespace display
