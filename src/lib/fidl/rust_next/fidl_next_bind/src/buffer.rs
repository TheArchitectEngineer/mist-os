// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::PhantomData;

use fidl_next_codec::{Decode, DecodeError, DecoderExt as _, Owned};
use fidl_next_protocol::Transport;

use super::Method;

macro_rules! buffer {
    ($name:ident, $trait:ident::$type:ident) => {
        /// A strongly typed receive buffer.
        pub struct $name<T: Transport, M> {
            buffer: T::RecvBuffer,
            _method: PhantomData<M>,
        }

        impl<T: Transport, M> $name<T, M> {
            /// Creates a new strongly typed receive buffer from an untyped receive buffer.
            pub fn from_untyped(buffer: T::RecvBuffer) -> Self {
                Self { buffer, _method: PhantomData }
            }

            /// Returns the underlying untyped receive buffer.
            pub fn into_untyped(self) -> T::RecvBuffer {
                self.buffer
            }

            /// Decodes the buffer.
            pub fn decode(&mut self) -> Result<Owned<'_, M::$type>, DecodeError>
            where
                M: $trait,
                M::$type: Decode<T::RecvBuffer>,
            {
                (&mut self.buffer).decode_last::<M::$type>()
            }
        }
    };
}

buffer!(RequestBuffer, Method::Request);
buffer!(ResponseBuffer, Method::Response);
