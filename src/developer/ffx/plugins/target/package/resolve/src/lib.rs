// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use ffx_target_package_resolve_args::ResolveCommand;
use ffx_writer::VerifiedMachineWriter;
use fho::{user_error, Error, FfxMain, FfxTool, Result};
use fidl::marker::SourceBreaking;
use fidl_fuchsia_pkg_resolution::{PackageResolverProxy, PackageResolverResolveRequest};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use target_holders::toolbox;

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Successfully resolved all packages.
    Ok {},
    /// Unexpected error with string denoting error message.
    UnexpectedError { message: String },
    /// A known error that can be reported to the user.
    UserError { message: String },
}

#[derive(FfxTool)]
pub struct ResolveTool {
    #[command]
    cmd: ResolveCommand,
    #[with(toolbox())]
    resolver_proxy: PackageResolverProxy,
}

fho::embedded_plugin!(ResolveTool);

#[async_trait(?Send)]
impl FfxMain for ResolveTool {
    type Writer = VerifiedMachineWriter<CommandStatus>;
    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        match self.resolve_cmd().await {
            Ok(()) => {
                writer.machine(&CommandStatus::Ok {})?;
                Ok(())
            }
            Err(e @ Error::User(_)) => {
                writer.machine(&CommandStatus::UserError { message: e.to_string() })?;
                Err(e)
            }
            Err(e) => {
                writer.machine(&CommandStatus::UnexpectedError { message: e.to_string() })?;
                Err(e)
            }
        }
    }
}

impl ResolveTool {
    pub async fn resolve_cmd(&self) -> Result<()> {
        let resolver = &self.resolver_proxy;
        if let Some(package) = &self.cmd.package {
            log::info!("Attempting to resolve: {package}");
            resolver
                .resolve(PackageResolverResolveRequest {
                    package_url: Some(package.to_string()),
                    __source_breaking: SourceBreaking,
                })
                .await
                .map_err(|e| user_error!("{e}"))?
                .map_err(|e| user_error!("Failed to resolve {package}: {e:?}"))?;
        } else {
            return Err(user_error!("No package URL given."));
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use ffx_writer::TestBuffers;
    use fidl_fuchsia_pkg_resolution::{PackageResolverRequest, ResolveError, ResolveResult};
    use target_holders::fake_proxy;

    async fn setup_fake_package_resolver_proxy(urls: Vec<String>) -> PackageResolverProxy {
        let proxy = fake_proxy(move |req| match req {
            PackageResolverRequest::Resolve { payload, responder } => {
                if let Some(package_url) = payload.package_url {
                    if urls.contains(&package_url) {
                        responder
                            .send(Ok(ResolveResult { __source_breaking: SourceBreaking }))
                            .unwrap();
                    } else {
                        responder.send(Err(ResolveError::PackageNotFound)).unwrap();
                    }
                } else {
                    responder.send(Err(ResolveError::InvalidUrl)).unwrap();
                }
            }
            PackageResolverRequest::_UnknownMethod { .. } => unimplemented!(),
        });
        proxy
    }

    #[fuchsia::test]
    async fn no_package_given() {
        let test_buffers = TestBuffers::default();
        let writer = <ResolveTool as fho::FfxMain>::Writer::new_test(None, &test_buffers);
        let pkg = vec!["fuchsia-pkg://fuchsia.com/existing_package".to_string()];
        let resolver = setup_fake_package_resolver_proxy(pkg.clone()).await;

        let tool = ResolveTool { cmd: ResolveCommand { package: None }, resolver_proxy: resolver };
        assert!(tool.main(writer).await.is_err());
    }

    #[fuchsia::test]
    async fn resolve_existing_package() {
        let test_buffers = TestBuffers::default();
        let writer = <ResolveTool as fho::FfxMain>::Writer::new_test(None, &test_buffers);
        let pkg = vec!["fuchsia-pkg://fuchsia.com/existing_package".to_string()];
        let resolver = setup_fake_package_resolver_proxy(pkg.clone()).await;

        let tool = ResolveTool {
            cmd: ResolveCommand { package: Some(pkg[0].clone()) },
            resolver_proxy: resolver,
        };
        assert!(tool.main(writer).await.is_ok());
    }

    #[fuchsia::test]
    async fn resolve_non_existing_package() {
        let test_buffers = TestBuffers::default();
        let writer = <ResolveTool as fho::FfxMain>::Writer::new_test(None, &test_buffers);
        let pkg = vec!["fuchsia-pkg://fuchsia.com/existing_package".to_string()];
        let resolver = setup_fake_package_resolver_proxy(pkg.clone()).await;

        let tool = ResolveTool {
            cmd: ResolveCommand { package: Some("fuchsia-pkg://fuchsia.com/nonexistent".into()) },
            resolver_proxy: resolver,
        };
        assert!(tool.main(writer).await.is_err());
    }

    #[fuchsia::test]
    async fn resolve_multiple_packages_in_multiple_calls() {
        let test_buffers = TestBuffers::default();
        let writer = <ResolveTool as fho::FfxMain>::Writer::new_test(None, &test_buffers);
        let pkg = vec![
            "fuchsia-pkg://fuchsia.com/pkg1_multiple_calls".to_string(),
            "fuchsia-pkg://fuchsia.com/pkg2_multiple_calls".to_string(),
        ];
        let resolver = setup_fake_package_resolver_proxy(pkg.clone()).await;

        let tool = ResolveTool {
            cmd: ResolveCommand {
                package: Some("fuchsia-pkg://fuchsia.com/pkg1_multiple_calls".into()),
            },
            resolver_proxy: resolver.clone(),
        };
        assert!(tool.main(writer).await.is_ok());

        let writer = <ResolveTool as fho::FfxMain>::Writer::new_test(None, &test_buffers);
        let tool = ResolveTool {
            cmd: ResolveCommand {
                package: Some("fuchsia-pkg://fuchsia.com/pkg2_multiple_calls".into()),
            },
            resolver_proxy: resolver,
        };
        assert!(tool.main(writer).await.is_ok());
    }
}
