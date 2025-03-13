// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_scrutiny_package_args::ScrutinyPackageCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool, Result};
use scrutiny_frontend::Scrutiny;

#[derive(FfxTool)]
pub struct ScrutinyPackageTool {
    #[command]
    pub cmd: ScrutinyPackageCommand,
}

fho::embedded_plugin!(ScrutinyPackageTool);

#[async_trait::async_trait(?Send)]
impl FfxMain for ScrutinyPackageTool {
    type Writer = SimpleWriter;
    async fn main(self, _writer: Self::Writer) -> fho::Result<()> {
        let artifacts = if self.cmd.recovery {
            Scrutiny::from_product_bundle_recovery(&self.cmd.product_bundle)
        } else {
            Scrutiny::from_product_bundle(&self.cmd.product_bundle)
        }?
        .collect()?;
        let response = artifacts.extract_package(self.cmd.url, self.cmd.output)?;
        let s = serde_json::to_string_pretty(&response).map_err(|e| fho::Error::User(e.into()))?;
        println!("{}", s);
        Ok(())
    }
}
