// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;

use assembly_container::WalkPaths;
use camino::Utf8PathBuf;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::option_path_schema;

/// Platform configuration options for recovery.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema, WalkPaths)]
#[serde(default, deny_unknown_fields)]
pub struct RecoveryConfig {
    /// Include the factory-reset-trigger package, and configure it using the given file.
    ///
    /// This is a a map of channel names to indices, when the current OTA
    /// channel matches one of the names in the file, if a stored index is less
    /// than the index value in the file, a factory reset is triggered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory_reset_trigger_config: Option<BTreeMap<String, i32>>,

    /// Which system_recovery implementation to include
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_recovery: Option<SystemRecovery>,

    /// The path to the logo for the recovery process to use.
    ///
    /// This must be a rive file (.riv).
    #[schemars(schema_with = "option_path_schema")]
    #[walk_paths]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<Utf8PathBuf>,

    /// The path to the instructions to display.
    ///
    /// This file must be raw text for displaying.
    #[schemars(schema_with = "option_path_schema")]
    #[walk_paths]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Utf8PathBuf>,

    /// Perform a managed-mode check before doing an FDR.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub check_for_managed_mode: bool,
}

/// Which system recovery implementation to include in the image
#[derive(Debug, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemRecovery {
    Fdr,
}
