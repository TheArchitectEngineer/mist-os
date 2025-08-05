// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::sysfs::{SysfsError, SysfsOps};
use crate::sysfs_error;
use fnanohub::{DisplayState, DisplaySyncInfo};
use starnix_uapi::errno;
use {fidl_fuchsia_hardware_google_nanohub as fnanohub, zx};

fn format_display_state(state: &DisplayState) -> String {
    let mode = state.mode.unwrap().into_primitive();
    format!("{}\n", mode)
}

fn format_display_info(info: &DisplaySyncInfo) -> String {
    format!(
        "display_mode: {}\npanel_mode: {}\nnbm_brightness: {}\naod_brightness: {}\n",
        info.display_mode.unwrap().into_primitive(),
        info.panel_mode.unwrap(),
        info.normal_brightness.unwrap(),
        info.always_on_display_brightness.unwrap()
    )
}

fn format_display_select(response: &fnanohub::DisplayDeviceGetDisplaySelectResponse) -> String {
    format!("{}\n", response.display_select.unwrap().into_primitive())
}

fn parse_display_select(value: &str) -> Result<fnanohub::DisplaySelect, SysfsError> {
    match value.trim() {
        "0" => Ok(fnanohub::DisplaySelect::Low),
        "1" => Ok(fnanohub::DisplaySelect::High),
        _ => sysfs_error!(EINVAL),
    }
}

#[derive(Default)]
pub struct DisplayStateSysFsOps {}

impl SysfsOps<fnanohub::DisplayDeviceSynchronousProxy> for DisplayStateSysFsOps {
    fn show(
        &self,
        service: &fnanohub::DisplayDeviceSynchronousProxy,
    ) -> Result<String, SysfsError> {
        let state = service.get_display_state(zx::MonotonicInstant::INFINITE)??;
        Ok(format_display_state(&state))
    }
}

#[derive(Default)]
pub struct DisplayInfoSysFsOps {}

impl SysfsOps<fnanohub::DisplayDeviceSynchronousProxy> for DisplayInfoSysFsOps {
    fn show(
        &self,
        service: &fnanohub::DisplayDeviceSynchronousProxy,
    ) -> Result<String, SysfsError> {
        let info = service.get_display_info(zx::MonotonicInstant::INFINITE)??;
        Ok(format_display_info(&info))
    }
}

#[derive(Default)]
pub struct DisplaySelectSysFsOps {}

impl SysfsOps<fnanohub::DisplayDeviceSynchronousProxy> for DisplaySelectSysFsOps {
    fn show(
        &self,
        service: &fnanohub::DisplayDeviceSynchronousProxy,
    ) -> Result<String, SysfsError> {
        let value = service.get_display_select(zx::MonotonicInstant::INFINITE)??;
        Ok(format_display_select(&value))
    }

    fn store(
        &self,
        service: &fnanohub::DisplayDeviceSynchronousProxy,
        value: String,
    ) -> Result<(), SysfsError> {
        let display_select = parse_display_select(&value)?;
        let request = fnanohub::DisplayDeviceSetDisplaySelectRequest {
            display_select: Some(display_select),
            ..Default::default()
        };
        service
            .set_display_select(&request, zx::MonotonicInstant::INFINITE)?
            .map_err(|_| SysfsError(errno!(EIO)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnanohub::DisplayDeviceGetDisplaySelectResponse;

    #[::fuchsia::test]
    fn test_format_display_state() {
        let state = DisplayState { mode: Some(fnanohub::DisplayMode::On), ..Default::default() };
        assert_eq!(format_display_state(&state), "4\n");

        let state = DisplayState { mode: Some(fnanohub::DisplayMode::Off), ..Default::default() };
        assert_eq!(format_display_state(&state), "2\n");
    }

    #[::fuchsia::test]
    fn test_format_display_info() {
        let info = DisplaySyncInfo {
            display_mode: Some(fnanohub::DisplayMode::On),
            panel_mode: Some(1),
            normal_brightness: Some(2),
            always_on_display_brightness: Some(3),
            ..Default::default()
        };
        assert_eq!(
            format_display_info(&info),
            "display_mode: 4\npanel_mode: 1\nnbm_brightness: 2\naod_brightness: 3\n"
        );
    }

    #[::fuchsia::test]
    fn test_format_display_select() {
        let response = DisplayDeviceGetDisplaySelectResponse {
            display_select: Some(fnanohub::DisplaySelect::High),
            ..Default::default()
        };
        assert_eq!(format_display_select(&response), "1\n");

        let response = DisplayDeviceGetDisplaySelectResponse {
            display_select: Some(fnanohub::DisplaySelect::Low),
            ..Default::default()
        };
        assert_eq!(format_display_select(&response), "0\n");
    }

    #[::fuchsia::test]
    fn test_parse_display_select_valid() {
        assert_eq!(parse_display_select("0"), Ok(fnanohub::DisplaySelect::Low));
        assert_eq!(parse_display_select("1"), Ok(fnanohub::DisplaySelect::High));
        assert_eq!(parse_display_select(" 0 "), Ok(fnanohub::DisplaySelect::Low));
        assert_eq!(parse_display_select(" 1 "), Ok(fnanohub::DisplaySelect::High));
    }

    #[::fuchsia::test]
    fn test_parse_display_select_invalid() {
        assert_eq!(parse_display_select("2").unwrap_err().0, errno!(EINVAL));
        assert_eq!(parse_display_select("foo").unwrap_err().0, errno!(EINVAL));
        assert_eq!(parse_display_select("").unwrap_err().0, errno!(EINVAL));
    }
}
