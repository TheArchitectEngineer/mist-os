// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod adapters;
mod fho_env;
mod from_env;
mod try_from_env;

pub mod subtool;

pub use subtool::{FfxMain, FfxTool};

// Re-export TryFromEnv related symbols
pub use fho_env::{EnvironmentInterface, FhoEnvironment};
pub use from_env::{AvailabilityFlag, CheckEnv};

pub use try_from_env::{deferred, Deferred, TryFromEnv, TryFromEnvWith};

// Used for deriving an FFX tool.
pub use fho_macro::FfxTool;

// Re-expose the Error, Result, and FfxContext types from ffx_command
// so you don't have to pull both in all the time.
pub use ffx_command_error::{
    bug, exit_with_code, return_bug, return_user_error, user_error, Error, FfxContext,
    NonFatalError, Result,
};

// FfxCommandLine is being re-exported so that, it can easily be used by the derive macros for
// subtools.
pub use ffx_command::FfxCommandLine;

#[doc(hidden)]
pub mod macro_deps {
    pub use async_trait::async_trait;
    pub use ffx_command::{
        bug, check_strict_constraints, return_bug, return_user_error, Ffx, ToolRunner,
    };
    pub use ffx_config::{global_env_context, EnvironmentContext};
    pub use ffx_core::Injector;
    pub use {crate as fho, anyhow, argh, async_lock, futures, serde, writer};
}
