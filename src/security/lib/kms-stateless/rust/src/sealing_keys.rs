// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_security_keymint::{CreateError, SealError, UnsealError};
use fuchsia_component::client;

#[derive(Debug, thiserror::Error)]
pub enum SealingKeysError {
    #[error("Failed to connect to protocol: {0:?}")]
    ConnectToProtocol(#[from] anyhow::Error),
    #[error(transparent)]
    Fidl(#[from] fidl::Error),
    #[error("Failed to create {0:?}")]
    Create(CreateError),
    #[error("Failed to seal {0:?}")]
    Seal(SealError),
    #[error("Failed to unseal {0:?}")]
    Unseal(UnsealError),
}

impl From<CreateError> for SealingKeysError {
    fn from(e: CreateError) -> Self {
        Self::Create(e)
    }
}

impl From<SealError> for SealingKeysError {
    fn from(e: SealError) -> Self {
        Self::Seal(e)
    }
}

impl From<UnsealError> for SealingKeysError {
    fn from(e: UnsealError) -> Self {
        Self::Unseal(e)
    }
}

/// Convenience wrapper around fuchsia.security.keymint/SealingKeys::CreateSealingKey()
///
/// See //sdk/fidl/fuchsia.security.keymint/sealing_keys.fidl for more details.
///
/// Requires the caller to have the capability fuchsia.security.keymint.SealingKeys
pub async fn create_sealing_key(key_info: &[u8]) -> Result<Vec<u8>, SealingKeysError> {
    client::connect_to_protocol::<fidl_fuchsia_security_keymint::SealingKeysMarker>()?
        .create_sealing_key(key_info)
        .await?
        .map_err(Into::into)
}

/// Convenience wrapper around fuchsia.security.keymint/SealingKeys:Seal()
///
/// See //sdk/fidl/fuchsia.security.keymint/sealing_keys.fidl for more details.
///
/// Requires the caller to have the capability fuchsia.security.keymint.SealingKeys
pub async fn seal(
    key_info: &[u8],
    key_blob: &[u8],
    secret: &[u8],
) -> Result<Vec<u8>, SealingKeysError> {
    client::connect_to_protocol::<fidl_fuchsia_security_keymint::SealingKeysMarker>()?
        .seal(key_info, key_blob, secret)
        .await?
        .map_err(Into::into)
}

/// Convenience wrapper around fuchsia.security.keymint/SealingKeys::Unseal()
///
/// See //sdk/fidl/fuchsia.security.keymint/sealing_keys.fidl for more details.
///
/// Requires the caller to have the capability fuchsia.security.keymint.SealingKeys
pub async fn unseal(
    key_info: &[u8],
    key_blob: &[u8],
    sealed_secret: &[u8],
) -> Result<Vec<u8>, SealingKeysError> {
    client::connect_to_protocol::<fidl_fuchsia_security_keymint::SealingKeysMarker>()?
        .unseal(key_info, key_blob, sealed_secret)
        .await?
        .map_err(Into::into)
}
