// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::common::{is_locked, lock_device, verify_variable_value};
use anyhow::Result;
use errors::ffx_bail;
use ffx_fastboot_interface::fastboot_interface::FastbootInterface;

const LOCKABLE_VAR: &str = "vx-unlockable";
const EPHEMERAL: &str = "ephemeral";
const EPHEMERAL_ERR: &str = "Cannot lock ephemeral devices. Reboot the device to unlock.";
const LOCKED_ERR: &str = "Target is already locked.";

pub async fn lock<F: FastbootInterface>(fastboot_interface: &mut F) -> Result<()> {
    if is_locked(fastboot_interface).await? {
        ffx_bail!("{}", LOCKED_ERR);
    }
    if verify_variable_value(LOCKABLE_VAR, EPHEMERAL, fastboot_interface).await? {
        ffx_bail!("{}", EPHEMERAL_ERR);
    }
    lock_device(fastboot_interface).await?;
    Ok(())
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use super::*;
    use crate::common::vars::LOCKED_VAR;
    use ffx_fastboot_interface::test::setup;

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_locked_device_throws_err() -> Result<()> {
        let (state, mut proxy) = setup();
        {
            let mut state = state.lock().unwrap();
            // is_locked
            state.set_var(LOCKED_VAR.to_string(), "yes".to_string());
        }
        let result = lock(&mut proxy).await;
        assert!(result.is_err());
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_ephemeral_locked_throws_err() -> Result<()> {
        let (state, mut proxy) = setup();
        {
            let mut state = state.lock().unwrap();
            state.set_var(LOCKABLE_VAR.to_string(), EPHEMERAL.to_string());
            // is_locked
            state.set_var(LOCKED_VAR.to_string(), "no".to_string());
        }
        let result = lock(&mut proxy).await;
        assert!(result.is_err());
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_lock_succeeds() -> Result<()> {
        let (state, mut proxy) = setup();
        {
            let mut state = state.lock().unwrap();
            // ephemeral
            state.set_var(LOCKABLE_VAR.to_string(), "whatever".to_string());
            // is_locked
            state.set_var(LOCKED_VAR.to_string(), "no".to_string());
        }
        lock(&mut proxy).await?;
        let state = state.lock().unwrap();
        assert_eq!(1, state.oem_commands.len());
        assert_eq!("vx-lock", state.oem_commands[0]);
        Ok(())
    }
}
