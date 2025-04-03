// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use aes_gcm_siv::aead::Aead;
use aes_gcm_siv::{Aes256GcmSiv, Key, KeyInit as _, Nonce};
use anyhow::{Context, Error};
use fidl_fuchsia_fxfs::{
    CryptCreateKeyResult, CryptCreateKeyWithIdResult, CryptManagementAddWrappingKeyResult,
    CryptManagementForgetWrappingKeyResult, CryptManagementRequest, CryptManagementRequestStream,
    CryptManagementSetActiveKeyResult, CryptRequest, CryptRequestStream, CryptUnwrapKeyResult,
    KeyPurpose,
};

use fuchsia_sync::Mutex;
use futures::stream::TryStreamExt;
use std::collections::hash_map::{Entry, HashMap};

pub mod log;
use log::*;

pub enum Services {
    Crypt(CryptRequestStream),
    CryptManagement(CryptManagementRequestStream),
}

#[derive(Default)]
struct CryptServiceInner {
    ciphers: HashMap<u128, Aes256GcmSiv>,
    active_data_key: Option<u128>,
    active_metadata_key: Option<u128>,
}

pub struct CryptService {
    inner: Mutex<CryptServiceInner>,
}

fn zero_extended_nonce(val: u64) -> Nonce {
    let mut nonce = Nonce::default();
    nonce.as_mut_slice()[..8].copy_from_slice(&val.to_le_bytes());
    nonce
}

impl CryptService {
    pub fn new() -> Self {
        Self { inner: Mutex::new(CryptServiceInner::default()) }
    }

    fn create_key(&self, owner: u64, purpose: KeyPurpose) -> CryptCreateKeyResult {
        let inner = self.inner.lock();
        let wrapping_key_id = match purpose {
            KeyPurpose::Data => inner.active_data_key.as_ref(),
            KeyPurpose::Metadata => inner.active_metadata_key.as_ref(),
            _ => return Err(zx::Status::INVALID_ARGS.into_raw()),
        }
        .ok_or_else(|| zx::Status::BAD_STATE.into_raw())?;
        let cipher =
            inner.ciphers.get(wrapping_key_id).ok_or_else(|| zx::Status::BAD_STATE.into_raw())?;
        let nonce = zero_extended_nonce(owner);

        let mut key = [0u8; 32];
        zx::cprng_draw(&mut key);

        let wrapped = cipher.encrypt(&nonce, &key[..]).map_err(|e| {
            error!(error:? = e; "Failed to wrap key");
            zx::Status::INTERNAL.into_raw()
        })?;

        Ok((wrapping_key_id.to_le_bytes(), wrapped.into(), key.into()))
    }

    fn create_key_with_id(&self, owner: u64, wrapping_key_id: u128) -> CryptCreateKeyWithIdResult {
        let inner = self.inner.lock();
        let cipher =
            inner.ciphers.get(&wrapping_key_id).ok_or_else(|| zx::Status::NOT_FOUND.into_raw())?;
        let nonce = zero_extended_nonce(owner);

        let mut key = [0u8; 32];
        zx::cprng_draw(&mut key);

        let wrapped = cipher.encrypt(&nonce, &key[..]).map_err(|error| {
            error!(error:?; "Failed to wrap key");
            zx::Status::INTERNAL.into_raw()
        })?;

        Ok((wrapped.into(), key.into()))
    }

    fn unwrap_key(&self, wrapping_key_id: u128, owner: u64, key: Vec<u8>) -> CryptUnwrapKeyResult {
        let inner = self.inner.lock();
        let cipher =
            inner.ciphers.get(&wrapping_key_id).ok_or_else(|| zx::Status::NOT_FOUND.into_raw())?;
        let nonce = zero_extended_nonce(owner);

        cipher.decrypt(&nonce, &key[..]).map_err(|_| zx::Status::IO_DATA_INTEGRITY.into_raw())
    }

    pub fn add_wrapping_key(
        &self,
        wrapping_key_id: u128,
        key: Vec<u8>,
    ) -> CryptManagementAddWrappingKeyResult {
        let mut inner = self.inner.lock();
        match inner.ciphers.entry(wrapping_key_id) {
            Entry::Occupied(_) => Err(zx::Status::ALREADY_EXISTS.into_raw()),
            Entry::Vacant(vacant) => {
                info!(wrapping_key_id; "Adding wrapping key");
                vacant.insert(Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key[..])));
                Ok(())
            }
        }
    }

    pub fn set_active_key(
        &self,
        purpose: KeyPurpose,
        wrapping_key_id: u128,
    ) -> CryptManagementSetActiveKeyResult {
        let mut inner = self.inner.lock();
        if !inner.ciphers.contains_key(&wrapping_key_id) {
            return Err(zx::Status::NOT_FOUND.into_raw());
        }
        match purpose {
            KeyPurpose::Data => inner.active_data_key = Some(wrapping_key_id),
            KeyPurpose::Metadata => inner.active_metadata_key = Some(wrapping_key_id),
            _ => return Err(zx::Status::INVALID_ARGS.into_raw()),
        }
        Ok(())
    }

    fn forget_wrapping_key(&self, wrapping_key_id: u128) -> CryptManagementForgetWrappingKeyResult {
        info!(wrapping_key_id; "Removing wrapping key");
        let mut inner = self.inner.lock();
        if let Some(id) = &inner.active_data_key {
            if *id == wrapping_key_id {
                return Err(zx::Status::INVALID_ARGS.into_raw());
            }
        }
        if let Some(id) = &inner.active_metadata_key {
            if *id == wrapping_key_id {
                return Err(zx::Status::INVALID_ARGS.into_raw());
            }
        }
        inner.ciphers.remove(&wrapping_key_id);
        Ok(())
    }

    pub async fn handle_request(&self, stream: Services) -> Result<(), Error> {
        match stream {
            Services::Crypt(mut stream) => {
                while let Some(request) = stream.try_next().await.context("Reading request")? {
                    match request {
                        CryptRequest::CreateKey { owner, purpose, responder } => {
                            responder
                                .send(match &self.create_key(owner, purpose) {
                                    Ok((id, ref wrapped, ref key)) => Ok((id, wrapped, key)),
                                    Err(e) => Err(*e),
                                })
                                .unwrap_or_else(|e| {
                                    // TODO(https://fxbug.dev/360919323): we can use `:err` when we
                                    // enable the log kv_std feature.
                                    error!(
                                        error:? = e;
                                        "Failed to send CreateKey response"
                                    )
                                });
                        }
                        CryptRequest::CreateKeyWithId { owner, wrapping_key_id, responder } => {
                            responder
                                .send(
                                    match self.create_key_with_id(
                                        owner,
                                        u128::from_le_bytes(wrapping_key_id),
                                    ) {
                                        Ok((ref wrapped, ref key)) => Ok((wrapped, key)),
                                        Err(e) => Err(e),
                                    },
                                )
                                .unwrap_or_else(|e| {
                                    // TODO(https://fxbug.dev/360919323): we can use `:err` when we
                                    // enable the log kv_std feature.
                                    error!(
                                        error:? = e;
                                        "Failed to send CreateKeyWithId response"
                                    )
                                });
                        }
                        CryptRequest::UnwrapKey { wrapping_key_id, owner, key, responder } => {
                            responder
                                .send(
                                    match self.unwrap_key(
                                        u128::from_le_bytes(wrapping_key_id),
                                        owner,
                                        key,
                                    ) {
                                        Ok(ref unwrapped) => Ok(unwrapped),
                                        Err(e) => Err(e),
                                    },
                                )
                                .unwrap_or_else(|e| {
                                    // TODO(https://fxbug.dev/360919323): we can use `:err` when we
                                    // enable the log kv_std feature.
                                    error!(
                                        error:? = e;
                                        "Failed to send UnwrapKey response"
                                    )
                                });
                        }
                    }
                }
            }
            Services::CryptManagement(mut stream) => {
                while let Some(request) = stream.try_next().await.context("Reading request")? {
                    match request {
                        CryptManagementRequest::AddWrappingKey {
                            wrapping_key_id,
                            key,
                            responder,
                        } => {
                            let response =
                                self.add_wrapping_key(u128::from_le_bytes(wrapping_key_id), key);
                            responder.send(response).unwrap_or_else(|e| {
                                // TODO(https://fxbug.dev/360919323): we can use `:err` when we
                                // enable the log kv_std feature.
                                error!(
                                    error:? = e;
                                    "Failed to send AddWrappingKey response"
                                )
                            });
                        }
                        CryptManagementRequest::SetActiveKey {
                            purpose,
                            wrapping_key_id,
                            responder,
                        } => {
                            let response =
                                self.set_active_key(purpose, u128::from_le_bytes(wrapping_key_id));
                            responder.send(response).unwrap_or_else(
                                // TODO(https://fxbug.dev/360919323): we can use `:err` when we
                                // enable the log kv_std feature.
                                |e| error!(error:? = e;"Failed to send SetActiveKey response"),
                            );
                        }
                        CryptManagementRequest::ForgetWrappingKey {
                            wrapping_key_id,
                            responder,
                        } => {
                            let response =
                                self.forget_wrapping_key(u128::from_le_bytes(wrapping_key_id));
                            responder.send(response).unwrap_or_else(|e| {
                                // TODO(https://fxbug.dev/360919323): we can use `:err` when we
                                // enable the log kv_std feature.
                                error!(
                                    error:? = e;
                                    "Failed to send ForgetWrappingKey response"
                                )
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::CryptService;
    use fidl_fuchsia_fxfs::KeyPurpose;

    #[test]
    fn wrap_unwrap_key() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];
        service.add_wrapping_key(1, key.clone()).expect("add_key failed");
        service.set_active_key(KeyPurpose::Data, 1).expect("set_active_key failed");

        let (wrapping_key_id, wrapped, unwrapped) =
            service.create_key(0, KeyPurpose::Data).expect("create_key failed");
        let wrapping_key_id_int = u128::from_le_bytes(wrapping_key_id);
        assert_eq!(wrapping_key_id_int, 1);
        let unwrap_result =
            service.unwrap_key(wrapping_key_id_int, 0, wrapped).expect("unwrap_key failed");
        assert_eq!(unwrap_result, unwrapped);

        // Do it twice to make sure the service can use the same key repeatedly.
        let (wrapping_key_id, wrapped, unwrapped) =
            service.create_key(1, KeyPurpose::Data).expect("create_key failed");
        let wrapping_key_id_int = u128::from_le_bytes(wrapping_key_id);
        assert_eq!(wrapping_key_id_int, 1);
        let unwrap_result =
            service.unwrap_key(wrapping_key_id_int, 1, wrapped).expect("unwrap_key failed");
        assert_eq!(unwrap_result, unwrapped);
    }

    #[test]
    fn wrap_unwrap_key_with_arbitrary_wrapping_key() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];
        service.add_wrapping_key(2, key.clone()).expect("add_key failed");

        let (wrapped, unwrapped) =
            service.create_key_with_id(0, 2).expect("create_key_with_id failed");
        let unwrap_result = service.unwrap_key(2, 0, wrapped).expect("unwrap_key failed");
        assert_eq!(unwrap_result, unwrapped);

        // Do it twice to make sure the service can use the same key repeatedly.
        let (wrapped, unwrapped) =
            service.create_key_with_id(1, 2).expect("create_key_with_id failed");
        let unwrap_result = service.unwrap_key(2, 1, wrapped).expect("unwrap_key failed");
        assert_eq!(unwrap_result, unwrapped);
    }

    #[test]
    fn create_key_with_wrapping_key_that_does_not_exist() {
        let service = CryptService::new();
        service
            .create_key_with_id(0, 2)
            .expect_err("create_key_with_id should fail if the wrapping key does not exist");

        let wrapping_key = vec![0xABu8; 32];
        service.add_wrapping_key(2, wrapping_key.clone()).expect("add_key failed");

        let (wrapped, unwrapped) =
            service.create_key_with_id(0, 2).expect("create_key_with_id failed");
        let unwrap_result = service.unwrap_key(2, 0, wrapped).expect("unwrap_key failed");
        assert_eq!(unwrap_result, unwrapped);
    }

    #[test]
    fn unwrap_key_wrong_key() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];
        service.add_wrapping_key(0, key.clone()).expect("add_key failed");
        service.set_active_key(KeyPurpose::Data, 0).expect("set_active_key failed");

        let (wrapping_key_id, mut wrapped, _) =
            service.create_key(0, KeyPurpose::Data).expect("create_key failed");
        for byte in &mut wrapped {
            *byte ^= 0xff;
        }
        service
            .unwrap_key(u128::from_le_bytes(wrapping_key_id), 0, wrapped)
            .expect_err("unwrap_key should fail");
    }

    #[test]
    fn unwrap_key_wrong_owner() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];
        service.add_wrapping_key(0, key.clone()).expect("add_key failed");
        service.set_active_key(KeyPurpose::Data, 0).expect("set_active_key failed");

        let (wrapping_key_id, wrapped, _) =
            service.create_key(0, KeyPurpose::Data).expect("create_key failed");
        service
            .unwrap_key(u128::from_le_bytes(wrapping_key_id), 1, wrapped)
            .expect_err("unwrap_key should fail");
    }

    #[test]
    fn add_forget_key() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];
        service.add_wrapping_key(0, key.clone()).expect("add_key failed");
        service.add_wrapping_key(0, key.clone()).expect_err("add_key should fail on a used slot");
        service.add_wrapping_key(1, key.clone()).expect("add_key failed");

        service.forget_wrapping_key(0).expect("forget_key failed");

        service.add_wrapping_key(0, key.clone()).expect("add_key failed");
    }

    #[test]
    fn set_active_key() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];

        service
            .set_active_key(KeyPurpose::Data, 0)
            .expect_err("set_active_key should fail when targeting nonexistent keys");

        service.add_wrapping_key(0, key.clone()).expect("add_key failed");
        service.add_wrapping_key(1, key.clone()).expect("add_key failed");

        service.set_active_key(KeyPurpose::Data, 0).expect("set_active_key failed");
        service.set_active_key(KeyPurpose::Metadata, 1).expect("set_active_key failed");

        service.forget_wrapping_key(0).expect_err("forget_key should fail on an active key");
        service.forget_wrapping_key(1).expect_err("forget_key should fail on an active key");
    }
}
