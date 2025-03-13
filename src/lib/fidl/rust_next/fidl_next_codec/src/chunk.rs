// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::WireU64;

/// FIDL alignment, used for buffer alignment to ensure decoding in-place is
/// possible.
pub const CHUNK_SIZE: usize = 8;

/// A group of eight bytes, aligned to an 8-byte boundary.
pub type Chunk = WireU64;

/// Returns a slice of chunks with the same bytewise value as the given bytes.
#[macro_export]
macro_rules! chunks {
    ($(
        $b0:literal, $b1:literal, $b2:literal, $b3:literal,
        $b4:literal, $b5:literal, $b6:literal, $b7:literal
    ),* $(,)?) => {
        [
            $($crate::WireU64(u64::from_le_bytes([
                $b0, $b1, $b2, $b3, $b4, $b5, $b6, $b7,
            ])),)*
        ]
    }
}
