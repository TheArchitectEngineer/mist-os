// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use {
    fidl_fuchsia_driver_development as fdd, fidl_fuchsia_driver_registrar as fdr,
    fidl_fuchsia_test_manager as ftm,
};

#[async_trait::async_trait]
pub trait DriverConnector {
    async fn get_driver_development_proxy(&self, select: bool) -> Result<fdd::ManagerProxy>;
    async fn get_driver_registrar_proxy(&self, select: bool) -> Result<fdr::DriverRegistrarProxy>;
    async fn get_suite_runner_proxy(&self) -> Result<ftm::SuiteRunnerProxy>;
}
