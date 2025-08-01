// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::{create_endpoints, create_proxy};
use futures::StreamExt;
use lazy_static::lazy_static;
use wlan_common::test_utils::fake_stas::FakeProtectionCfg;
use wlan_common::{assert_variant, bss, fake_fidl_bss_description};
use {
    fidl_fuchsia_wlan_fullmac as fidl_fullmac, fidl_fuchsia_wlan_sme as fidl_sme,
    fidl_test_wlan_testcontroller as fidl_testcontroller,
};

pub mod config;
pub mod fake_ap;
pub mod recorded_request_stream;

// Compatible BSS descriptions are defined here.
// "Compatible" here means that that an SME  with default configuration
// will accept a connect request to a BSS with the returned BssDescription.
lazy_static! {
    pub static ref COMPATIBLE_OPEN_BSS: bss::BssDescription = fake_fidl_bss_description!(
        protection => FakeProtectionCfg::Open,
        channel: wlan_common::channel::Channel::new(1, wlan_common::channel::Cbw::Cbw20),
        rates: vec![2, 4, 11],
    )
    .try_into()
    .expect("Could not convert BSS description from FIDL");
    pub static ref COMPATIBLE_WPA2_BSS: bss::BssDescription = fake_fidl_bss_description!(
        protection => FakeProtectionCfg::Wpa2,
        channel: wlan_common::channel::Channel::new(1, wlan_common::channel::Cbw::Cbw20),
        rates: vec![2, 4, 11],
    )
    .try_into()
    .expect("Could not convert BSS description from FIDL");
    pub static ref COMPATIBLE_WPA3_BSS: bss::BssDescription = fake_fidl_bss_description!(
        protection => FakeProtectionCfg::Wpa3,
        channel: wlan_common::channel::Channel::new(1, wlan_common::channel::Cbw::Cbw20),
        rates: vec![2, 4, 11],
    )
    .try_into()
    .expect("Could not convert BSS description from FIDL");
}

/// Creates and starts fullmac driver using |testcontroller_proxy|.
/// This handles the request to start SME through the UsmeBootstrap protocol,
/// and the sequence of query requests that SME makes to the fullmac driver on startup.
///
/// After this function is called, the fullmac driver is ready to be used in the test suite.
pub async fn create_fullmac_driver(
    testcontroller_proxy: &fidl_testcontroller::TestControllerProxy,
    config: &config::FullmacDriverConfig,
) -> (
    fidl_testcontroller::FullmacId,
    fidl_fullmac::WlanFullmacImpl_RequestStream,
    fidl_fullmac::WlanFullmacImplIfcProxy,
    fidl_sme::GenericSmeProxy,
) {
    let (fullmac_bridge_client, fullmac_bridge_server) = create_endpoints();

    let fullmac_id = testcontroller_proxy
        .create_fullmac(fullmac_bridge_client)
        .await
        .expect("FIDL error on create_fullmac")
        .expect("TestController returned an error on create fullmac");

    let mut fullmac_bridge_stream = fullmac_bridge_server.into_stream();

    // Fullmac MLME queries driver before starting
    let (fullmac_ifc_proxy, generic_sme_proxy) =
        handle_fullmac_startup(&mut fullmac_bridge_stream, config).await;

    (fullmac_id, fullmac_bridge_stream, fullmac_ifc_proxy, generic_sme_proxy)
}

pub async fn handle_fullmac_startup(
    fullmac_bridge_stream: &mut fidl_fullmac::WlanFullmacImpl_RequestStream,
    config: &config::FullmacDriverConfig,
) -> (fidl_fullmac::WlanFullmacImplIfcProxy, fidl_sme::GenericSmeProxy) {
    let (usme_bootstrap_proxy, usme_bootstrap_server) =
        create_proxy::<fidl_sme::UsmeBootstrapMarker>();

    let fullmac_ifc_proxy = assert_variant!(fullmac_bridge_stream.next().await,
        Some(Ok(fidl_fullmac::WlanFullmacImpl_Request::Init { payload, responder })) => {
            responder
                .send(Ok(fidl_fullmac::WlanFullmacImplInitResponse {
                    sme_channel: Some(usme_bootstrap_server.into_channel()),
                    ..Default::default()
                }))
                .expect("Failed to respond to Init");
            let ifc = payload.ifc.expect("Init response missing ifc");
            ifc.into_proxy()
        }
    );

    let (generic_sme_proxy, generic_sme_server) = create_proxy::<fidl_sme::GenericSmeMarker>();

    let _bootstrap_result = usme_bootstrap_proxy
        .start(generic_sme_server, &config.sme_legacy_privacy_support)
        .await
        .expect("Failed to call usme_bootstrap.start");

    assert_variant!(fullmac_bridge_stream.next().await,
        Some(Ok(fidl_fullmac::WlanFullmacImpl_Request::Query { responder })) => {
            responder
                .send(Ok(&config.query_info))
                .expect("Failed to respond to Query");
        }
    );

    assert_variant!(fullmac_bridge_stream.next().await,
        Some(Ok(fidl_fullmac::WlanFullmacImpl_Request::QuerySecuritySupport {
            responder,
        })) => {
            responder
                .send(Ok(&config.security_support))
                .expect("Failed to respond to QuerySecuritySupport");
        }
    );

    assert_variant!(fullmac_bridge_stream.next().await,
        Some(Ok(fidl_fullmac::WlanFullmacImpl_Request::QuerySpectrumManagementSupport {
                responder,
        })) => {
            responder
                .send(Ok(&config.spectrum_management_support))
                .expect("Failed to respond to QuerySpectrumManagementSupport");
        }
    );

    assert_variant!(fullmac_bridge_stream.next().await,
        Some(Ok(fidl_fullmac::WlanFullmacImpl_Request::Query { responder })) => {
            responder
                .send(Ok(&config.query_info))
                .expect("Failed to respond to Query");
        }
    );

    (fullmac_ifc_proxy, generic_sme_proxy)
}
