// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/lib/api-types/cpp/alpha-mode.h"

#include <fuchsia/hardware/display/controller/c/banjo.h>

#include <type_traits>

namespace display {

static_assert(std::is_standard_layout_v<AlphaMode>);
static_assert(std::is_trivially_assignable_v<AlphaMode, AlphaMode>);
static_assert(std::is_trivially_copyable_v<AlphaMode>);
static_assert(std::is_trivially_copy_constructible_v<AlphaMode>);
static_assert(std::is_trivially_destructible_v<AlphaMode>);
static_assert(std::is_trivially_move_assignable_v<AlphaMode>);
static_assert(std::is_trivially_move_constructible_v<AlphaMode>);

// Ensure that the Banjo constants match the FIDL constants.
static_assert(AlphaMode::kDisable.ToBanjo() == ALPHA_DISABLE);
static_assert(AlphaMode::kPremultiplied.ToBanjo() == ALPHA_PREMULTIPLIED);
static_assert(AlphaMode::kHwMultiply.ToBanjo() == ALPHA_HW_MULTIPLY);

}  // namespace display
