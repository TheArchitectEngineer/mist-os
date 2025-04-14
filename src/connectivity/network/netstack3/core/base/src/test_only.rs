// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Abstractions enabling test-only behavior.

pub use inner::{TestOnlyFrom, TestOnlyPartialEq};

// The implementation for test code.
#[cfg(any(test, feature = "testutils"))]
mod inner {
    pub use crate::Counter;

    /// Applies `PartialEq` bounds, only in tests.
    pub trait TestOnlyPartialEq: PartialEq {}

    impl<T: PartialEq> TestOnlyPartialEq for T {}

    // This implementation is necessary to satisfy trait bounds, but it's
    // likely a programming error to try to use it.
    impl PartialEq for Counter {
        fn eq(&self, _other: &Self) -> bool {
            panic!("The `Counter` type shouldn't be checked for equality")
        }
    }

    /// Applies `From` bounds, only in tests.
    pub trait TestOnlyFrom<T>: From<T> {}

    impl<T1, T2: From<T1>> TestOnlyFrom<T1> for T2 {}
}

// The implementation for non-test code
#[cfg(not(any(test, feature = "testutils")))]
mod inner {

    /// Applies `PartialEq` bounds, only in tests.
    pub trait TestOnlyPartialEq {}

    impl<T> TestOnlyPartialEq for T {}

    /// Applies `From` bounds, only in tests.
    pub trait TestOnlyFrom<T> {}

    impl<T1, T2> TestOnlyFrom<T1> for T2 {}
}
