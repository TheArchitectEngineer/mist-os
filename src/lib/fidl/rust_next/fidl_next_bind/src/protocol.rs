// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// A discoverable protocol.
pub trait Discoverable {
    /// The service name to use to connect to this discoverable protocol.
    const PROTOCOL_NAME: &'static str;
}

/// A method of a protocol.
pub trait Method {
    /// The ordinal associated with the method;
    const ORDINAL: u64;

    /// The protocol the method is a member of.
    type Protocol;

    /// The request payload for the method.
    type Request;

    /// The response payload for the method.
    type Response;
}

/// The request or response type of a method which does not have a request or response.
pub enum Never {}
