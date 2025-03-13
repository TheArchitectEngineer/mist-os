// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assembly_constants::ZeroPageScanCount;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What should happen if the device runs out-of-memory.
#[derive(Debug, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OOMBehavior {
    Reboot { timeout: OOMRebootTimeout },
    JobKill,
    Disable,
}

impl Default for OOMBehavior {
    fn default() -> Self {
        OOMBehavior::Reboot { timeout: OOMRebootTimeout::default() }
    }
}

/// The reboot timeout if the device runs out-of-memory.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OOMRebootTimeout {
    #[default]
    Normal,
    Low,
}

/// Sets the memory reclamation strategy of the device's kernel.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryReclamationStrategy {
    /// Default strategy that balances memory with performance.
    #[default]
    Balanced,
    /// Try hard to reclaim memory, even recently-used pages.
    Eager,
}

/// Platform configuration options for the kernel area.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformKernelConfig {
    /// What should happen if the device runs out-of-memory.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub oom_behavior: OOMBehavior,

    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub memory_compression: bool,

    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub lru_memory_compression: bool,

    /// Configures kernel eviction to run continually in the background to try
    /// and keep the system out of memory pressure, as opposed to triggering
    /// one-shot eviction only at memory pressure level transitions.
    /// Enables the `kernel_evict_continuous` assembly input bundle.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub continuous_eviction: bool,

    /// Configures cprng related behaviors
    #[serde(default)]
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub cprng: CprngConfig,

    /// For address spaces that use ASLR this controls the number of bits of
    /// entropy in the randomization. Higher entropy results in a sparser
    /// address space and uses more memory for page tables. Valid values range
    /// from 0-36. Default value is 30.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aslr_entropy_bits: Option<u8>,

    /// Upper-bound in megabytes for the system memory.
    /// It simulates a system with less physical memory than it actually has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit_mb: Option<u64>,

    /// Configuration for the kernel memory reclamation strategy.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub memory_reclamation_strategy: MemoryReclamationStrategy,

    /// Configurations related to page scanner behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_scanner: Option<PageScannerConfig>,
}

/// Options for cprng behaviors
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CprngConfig {
    /// When enabled and if jitterentropy fails at initial seeding, CPRNG panics.
    #[serde(default)]
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub seed_require_jitterentropy: bool,

    /// When enabled and if you do not provide entropy input from the kernel
    /// command line, CPRNG panics.
    #[serde(default)]
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub seed_require_cmdline: bool,

    /// When enabled and if jitterentropy fails at reseeding, CPRNG panics.
    #[serde(default)]
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub reseed_require_jitterentropy: bool,
}

/// Options for user page tables the reclamation policy.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PagetableEvictionPolicy {
    /// Unused page tables are evicted periodically. The period
    /// can be controlled by kernel.page-scanner.page-table-eviction-period.
    #[default]
    Always,

    /// Page tables are never evicted.
    Never,

    /// Only performs eviction on request, such as in
    /// response to a low memory scenario.
    OnRequest,
}

/// Configurations related to page scanner behavior.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PageScannerConfig {
    /// Sets the reclamation policy for user page tables that are not accessed.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub page_table_eviction_policy: PagetableEvictionPolicy,

    /// This option causes the kernels active memory scanner to be initially
    /// disabled on startup if the value is true. You can also enable and
    /// disable it using the kernel console. If you disable the scanner, you
    /// can have additional system predictability since it removes time based
    /// and background memory eviction.
    ///
    /// Every action the scanner performs can be individually configured and
    /// disabled. If all actions are disabled then enabling the scanner has no
    /// effect.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub disable_at_boot: bool,

    /// This option configures the maximal number of candidate pages the zero
    /// page scanner will consider every second.
    ///
    /// The page scanner must be running for this option to have any effect. It
    /// can be enabled at boot unless `disable_at_boot` is set to True.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub zero_page_scans_per_second: ZeroPageScanCount,

    /// When set, disable the page scanner to evict user pager backed pages.
    /// Eviction can reduce memory usage and prevent out of memory scenarios,
    /// but removes some timing predictability from system behavior.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub disable_eviction: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_serialization() {
        crate::common::tests::default_serialization_helper::<PlatformKernelConfig>();
    }
}
