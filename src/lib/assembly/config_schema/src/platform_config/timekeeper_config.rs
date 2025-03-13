// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Platform configuration options for the input area.
#[derive(Debug, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TimekeeperConfig {
    /// The time to wait until retrying to sample the pull time source,
    /// expressed in seconds.
    pub back_off_time_between_pull_samples_sec: i64,
    /// The time to wait before sampling the time source for the first time,
    /// expressed in seconds.
    pub first_sampling_delay_sec: i64,
    /// If set, the device's real time clock is only ever read from, but
    /// not written to.
    pub time_source_endpoint_url: String,
    /// If set, Timekeeper will serve test-only protocols from the library
    /// `fuchsia.time.test`.
    pub serve_test_protocols: bool,
    /// If set, the UTC clock will be started if we attempt to read the RTC,
    /// but the reading of the RTC is known invalid.
    pub utc_start_at_startup_when_invalid_rtc: bool,
    /// If set, Timekeeper will serve `fuchsia.time.alarms` and will connect
    /// to the appropriate hardware device to do so.
    pub serve_fuchsia_time_alarms: bool,
    /// If set, the hardware has a counter that is always on and operates even
    /// if the rest of the hardware system is in a low power state.
    pub always_on_counter: bool,
    /// If set, assembly should configure and route persistent storage to
    /// Timekeeper.
    pub use_persistent_storage: bool,
    /// If set, Timekeeper should serve the FIDL protocol that allows external
    /// time adjustment, `fuchsia.time.external/Adjust`.
    ///
    /// This is a security sensitive protocol, and very few assemblies are
    /// expected to have it turned on.
    pub serve_fuchsia_time_external_adjust: bool,
    /// Maximum absolute difference between proposed UTC reference and actual UTC
    /// reference, expressed in seconds, when the proposed UTC reference is
    /// in the "past" with respect of actual UTC reference.
    ///
    /// This is always expressed as a non-negative value.
    pub utc_max_allowed_delta_past_sec: u64,
    /// Maximum absolute difference between proposed UTC reference and actual UTC
    /// reference, expressed in seconds, when the proposed UTC reference is
    /// in the "future" with respect of actual UTC reference.
    ///
    /// This is always expressed as a non-negative value.
    pub utc_max_allowed_delta_future_sec: u64,
}

impl Default for TimekeeperConfig {
    fn default() -> Self {
        // Values applied here are taken from static configuration defaults.
        Self {
            back_off_time_between_pull_samples_sec: 300,
            first_sampling_delay_sec: 0,
            time_source_endpoint_url: "https://clients3.google.com/generate_204".into(),
            serve_test_protocols: false,
            utc_start_at_startup_when_invalid_rtc: false,
            serve_fuchsia_time_alarms: false,
            always_on_counter: false,
            use_persistent_storage: false,
            serve_fuchsia_time_external_adjust: false,
            utc_max_allowed_delta_past_sec: 0,
            utc_max_allowed_delta_future_sec: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_default_serde() {
        let v: TimekeeperConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(v, Default::default());
    }

    #[test]
    fn test_default_serialization() {
        crate::common::tests::default_serialization_helper::<TimekeeperConfig>();
    }
}
