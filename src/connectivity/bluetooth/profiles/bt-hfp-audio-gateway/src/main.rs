// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#![recursion_limit = "1024"]

use anyhow::{anyhow, Context, Error};
use battery_client::BatteryClient;
use bt_hfp::codec_id::CodecId;
use bt_hfp::{audio, sco};
use fidl_fuchsia_media as media;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect_derive::Inspect;
use futures::channel::mpsc;
use futures::future;
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::pin::pin;

use crate::config::AudioGatewayFeatureSupport;
use crate::fidl_service::run_services;
use crate::hfp::Hfp;
use crate::profile::register_audio_gateway;

mod config;
mod error;
mod features;
mod fidl_service;
mod hfp;
mod inspect;
mod peer;
mod profile;
mod service_definitions;

fn controller_codecs(config: &hfp_profile_config::Config) -> HashSet<CodecId> {
    let mut controller_codecs = HashSet::new();
    if config.controller_encoding_cvsd {
        let _ = controller_codecs.insert(CodecId::CVSD);
    }
    if config.controller_encoding_msbc {
        let _ = controller_codecs.insert(CodecId::MSBC);
    }
    controller_codecs
}

async fn setup_audio(
    config: &hfp_profile_config::Config,
    controller_codecs: HashSet<CodecId>,
) -> Result<Box<dyn audio::Control>, Error> {
    if config.offload_type == "dai" {
        let audio_proxy =
            fuchsia_component::client::connect_to_protocol::<media::AudioDeviceEnumeratorMarker>()
                .with_context(|| format!("Error connecting to audio_core"))?;
        Ok(Box::new(
            audio::PartialOffloadControl::setup_audio_core(audio_proxy, controller_codecs).await?,
        ))
    } else if config.offload_type == "codec" {
        let provider = fuchsia_component::client::connect_to_protocol::<
            fidl_fuchsia_audio_device::ProviderMarker,
        >()
        .with_context(|| format!("Error connecting to audio_device_registry"))?;
        let codec = audio::CodecControl::new(provider);
        Ok(Box::new(codec))
    } else {
        error!(r#type = config.offload_type.as_str(); "Unrecognized offload type");
        Err(anyhow!("Can't setup audio"))
    }
}

#[fuchsia::main(logging_tags = ["bt-hfp-ag"])]
async fn main() -> Result<(), Error> {
    let fs = ServiceFs::new();

    let inspector = fuchsia_inspect::Inspector::default();
    let _inspect_server_task =
        inspect_runtime::publish(&inspector, inspect_runtime::PublishOptions::default());

    let config = hfp_profile_config::Config::take_from_startup_handle();
    let controller_codecs = controller_codecs(&config);

    let audio = setup_audio(&config, controller_codecs.clone()).await?;

    let feature_support = AudioGatewayFeatureSupport::load_from_config(config);
    debug!(feature_support:?; "Starting HFP Audio Gateway");
    let (profile_client, profile_svc) = register_audio_gateway(feature_support)?;

    // Power integration is optional - the HFP component will function even power integration is
    // unavailable.
    let battery_client = match BatteryClient::create() {
        Err(e) => {
            info!("Power integration unavailable: {:?}", e);
            None
        }
        Ok(batt) => Some(batt),
    };

    let (call_manager_sender, call_manager_receiver) = mpsc::channel(1);
    let (test_request_sender, test_request_receiver) = mpsc::channel(1);

    let sco_connector = sco::Connector::build(profile_svc.clone(), controller_codecs);

    let mut hfp = Hfp::new(
        profile_client,
        profile_svc,
        battery_client,
        audio,
        call_manager_receiver,
        feature_support,
        sco_connector,
        test_request_receiver,
    );
    if let Err(e) = hfp.iattach(&inspector.root(), "hfp") {
        warn!("Couldn't attach inspect to HFP: {:?}", e);
    }

    let hfp = pin!(hfp.run());

    let services = pin!(run_services(fs, call_manager_sender, test_request_sender));

    info!("HFP Audio Gateway running.");

    match future::select(services, hfp).await {
        future::Either::Left((Ok(()), _)) => {
            warn!("Service FS directory handle closed. Exiting.");
        }
        future::Either::Left((Err(e), _)) => {
            error!("Error encountered running Service FS: {e}. Exiting");
        }
        future::Either::Right((Ok(()), _)) => {
            warn!("All HFP related connections to this component have been disconnected. Exiting.");
        }
        future::Either::Right((Err(e), _)) => {
            error!("Error encountered running main HFP loop: {e}. Exiting.");
        }
    }

    Ok(())
}
