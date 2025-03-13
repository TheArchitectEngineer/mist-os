// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Platform configuration options for the starnix area.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformStarnixConfig {
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub enabled: bool,

    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub enable_android_support: bool,
}
