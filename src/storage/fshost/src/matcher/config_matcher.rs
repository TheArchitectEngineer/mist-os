// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::{Device, DeviceTag, Parent};
use crate::environment::Environment;

use super::Matcher;
use anyhow::{Context as _, Error};
use async_trait::async_trait;
use fshost_assembly_config::{BlockDeviceConfig, BlockDeviceParent};

pub async fn get_config_matchers() -> Result<Vec<Box<dyn Matcher>>, Error> {
    let mut matchers: Vec<Box<dyn Matcher>> = Vec::new();
    let devices_str =
        match fuchsia_fs::file::read_in_namespace_to_string("/boot/config/fshost").await {
            Ok(devices_str) => devices_str,
            Err(error) => {
                log::warn!(error:?; "Could not read fshost config, skipping configured matchers");
                return Ok(Vec::new());
            }
        };
    let devices: Vec<BlockDeviceConfig> =
        serde_json::from_str(&devices_str).context("deserializing config")?;
    for device in devices {
        let parent = match device.from.parent {
            BlockDeviceParent::Gpt => Parent::SystemPartitionTable,
            BlockDeviceParent::Dev => Parent::Dev,
        };
        matchers.push(ConfigMatcher::new(device.device, device.from.label, parent));
    }

    Ok(matchers)
}

pub struct ConfigMatcher {
    name: String,
    label: String,
    parent: Parent,
    already_matched: bool,
}

impl ConfigMatcher {
    pub fn new(name: String, label: String, parent: Parent) -> Box<Self> {
        Box::new(ConfigMatcher { name, label, parent, already_matched: false })
    }
}

#[async_trait]
impl Matcher for ConfigMatcher {
    async fn match_device(&self, device: &mut dyn Device) -> bool {
        !self.already_matched
            && device.partition_label().await.is_ok_and(|label| label == &self.label)
            && device.parent() == self.parent
    }

    async fn process_device(
        &mut self,
        device: &mut dyn Device,
        env: &mut dyn Environment,
    ) -> Result<Option<DeviceTag>, Error> {
        log::info!("publishing device to /block/{}", &self.name);
        env.publish_device(device, &self.name)?;
        self.already_matched = true;
        Ok(None)
    }
}
