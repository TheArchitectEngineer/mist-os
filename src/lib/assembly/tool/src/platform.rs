// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{Tool, ToolCommand, ToolCommandLog, ToolProvider};

use anyhow::{anyhow, Context, Result};
use camino::Utf8PathBuf;
use std::path::PathBuf;
use std::process::Command;
use utf8_path::PathToStringExt;

/// A provider for tools from a platform artifacts directory.
#[derive(Clone)]
pub struct PlatformToolProvider {
    tools_dir: Utf8PathBuf,
    log: ToolCommandLog,
}

impl PlatformToolProvider {
    /// Construct a new PlatformToolProvider.
    pub fn new(platform_artifacts_dir: Utf8PathBuf) -> Self {
        Self { tools_dir: platform_artifacts_dir.join("tools"), log: ToolCommandLog::default() }
    }
}

impl ToolProvider for PlatformToolProvider {
    fn get_tool(&self, name: &str) -> Result<Box<dyn Tool>> {
        let path = self.tools_dir.join(name).as_std_path().to_path_buf();
        self.get_tool_with_path(path)
    }

    fn get_tool_with_path(&self, path: PathBuf) -> Result<Box<dyn Tool>> {
        let tool = PlatformTool::new(path, self.log.clone());
        Ok(Box::new(tool))
    }

    fn log(&self) -> &ToolCommandLog {
        &self.log
    }
}

#[derive(Clone, Debug)]
struct PlatformTool {
    path: PathBuf,
    log: ToolCommandLog,
}

impl PlatformTool {
    pub fn new(path: PathBuf, log: ToolCommandLog) -> Self {
        Self { path, log }
    }
}

impl Tool for PlatformTool {
    fn run(&self, args: &[String]) -> Result<()> {
        let path = self.path.path_to_string()?;
        self.log.add(ToolCommand::new(path.clone(), args.into()));
        let output = Command::new(&self.path)
            .args(args)
            .output()
            .context(format!("Failed to run the tool: {}", path))?;
        if !output.status.success() {
            let command = format!("{} {}", path, args.join(" "));
            return Err(anyhow!("{} exited with status: {}", path, output.status)
                .context(format!("stderr: {}", String::from_utf8_lossy(&output.stderr)))
                .context(command));
        }
        Ok(())
    }
}
