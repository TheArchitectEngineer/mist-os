// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_utils::hanging_get::error::HangingGetServerError;
use bt_hfp::call::list as call_list;
use bt_hfp::sco;
use fidl_fuchsia_bluetooth_hfp::CallState;
use profile_client::Error as ProfileError;
use std::error::Error as StdError;
use thiserror::Error;

/// Errors that occur during the operation of the HFP Bluetooth Profile component.
#[derive(Error, Debug)]
pub enum Error {
    #[error("Error using BR/EDR resource {:?}", .resource)]
    ProfileResourceError {
        #[from]
        resource: ProfileError,
    },
    #[error("Error connecting SCO: {:?}", .source)]
    ScoConnection { source: Box<dyn StdError> },
    #[error("System error encountered: {}", .message)]
    System { message: String, source: Box<dyn StdError> },
    #[error("Peer removed")]
    PeerRemoved,
    #[error("Value out of range")]
    OutOfRange,
    #[error("Error managing a hanging get request for a client: {}", .0)]
    HangingGet(#[from] HangingGetServerError),
    #[error("Missing required parameter: {}", .0)]
    MissingParameter(String),
    #[error("Fidl Error: {}", .0)]
    Fidl(#[from] fidl::Error),
}

impl Error {
    /// An error occurred connecting an SCO channel or during audio setup.
    pub fn sco_connection<E: StdError + 'static>(e: E) -> Self {
        Self::ScoConnection { source: Box::new(e) }
    }
    /// An error occurred when interacting with the system.
    ///
    /// This allocates memory which could fail if the error is an OOM.
    pub fn system<E: StdError + 'static>(message: impl Into<String>, e: E) -> Self {
        Self::System { message: message.into(), source: Box::new(e) }
    }
}

/// A request was made using an unknown call.
#[derive(Debug, PartialEq, Clone, Error)]
pub enum CallError {
    #[error("Unknown call index {}", .0)]
    UnknownIndexError(call_list::Idx),
    #[error("No call in states {:?}", .0)]
    None(Vec<CallState>),
}

impl From<sco::ConnectError> for Error {
    fn from(x: sco::ConnectError) -> Self {
        Self::sco_connection(x)
    }
}
