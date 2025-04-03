// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{format_err, Context};
use fidl::endpoints::ClientEnd;
use {
    fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_fullmac as fidl_fullmac,
    fidl_fuchsia_wlan_mlme as fidl_mlme, fidl_fuchsia_wlan_stats as fidl_stats,
};

/// This trait abstracts how Device accomplish operations. Test code
/// can then implement trait methods instead of mocking an underlying DeviceInterface
/// and FIDL proxy.
pub trait DeviceOps {
    fn init(
        &mut self,
        fullmac_ifc_client_end: ClientEnd<fidl_fullmac::WlanFullmacImplIfcMarker>,
    ) -> Result<fidl::Channel, zx::Status>;
    fn query_device_info(&self) -> anyhow::Result<fidl_fullmac::WlanFullmacImplQueryResponse>;
    fn query_security_support(&self) -> anyhow::Result<fidl_common::SecuritySupport>;
    fn query_spectrum_management_support(
        &self,
    ) -> anyhow::Result<fidl_common::SpectrumManagementSupport>;
    fn query_telemetry_support(&self) -> anyhow::Result<Result<fidl_stats::TelemetrySupport, i32>>;
    fn start_scan(&self, req: fidl_fullmac::WlanFullmacImplStartScanRequest) -> anyhow::Result<()>;
    fn connect(&self, req: fidl_fullmac::WlanFullmacImplConnectRequest) -> anyhow::Result<()>;
    fn reconnect(&self, req: fidl_fullmac::WlanFullmacImplReconnectRequest) -> anyhow::Result<()>;
    fn roam(&self, req: fidl_fullmac::WlanFullmacImplRoamRequest) -> anyhow::Result<()>;
    fn auth_resp(&self, resp: fidl_fullmac::WlanFullmacImplAuthRespRequest) -> anyhow::Result<()>;
    fn deauth(&self, req: fidl_fullmac::WlanFullmacImplDeauthRequest) -> anyhow::Result<()>;
    fn assoc_resp(&self, resp: fidl_fullmac::WlanFullmacImplAssocRespRequest)
        -> anyhow::Result<()>;
    fn disassoc(&self, req: fidl_fullmac::WlanFullmacImplDisassocRequest) -> anyhow::Result<()>;
    fn start_bss(&self, req: fidl_fullmac::WlanFullmacImplStartBssRequest) -> anyhow::Result<()>;
    fn stop_bss(&self, req: fidl_fullmac::WlanFullmacImplStopBssRequest) -> anyhow::Result<()>;
    fn set_keys(
        &self,
        req: fidl_fullmac::WlanFullmacImplSetKeysRequest,
    ) -> anyhow::Result<fidl_fullmac::WlanFullmacSetKeysResp>;
    fn eapol_tx(&self, req: fidl_fullmac::WlanFullmacImplEapolTxRequest) -> anyhow::Result<()>;
    fn get_iface_stats(&self) -> anyhow::Result<fidl_mlme::GetIfaceStatsResponse>;
    fn get_iface_histogram_stats(
        &self,
    ) -> anyhow::Result<fidl_mlme::GetIfaceHistogramStatsResponse>;
    fn sae_handshake_resp(
        &self,
        resp: fidl_fullmac::WlanFullmacImplSaeHandshakeRespRequest,
    ) -> anyhow::Result<()>;
    fn sae_frame_tx(&self, frame: fidl_fullmac::SaeFrame) -> anyhow::Result<()>;
    fn wmm_status_req(&self) -> anyhow::Result<()>;
    fn on_link_state_changed(
        &self,
        req: fidl_fullmac::WlanFullmacImplOnLinkStateChangedRequest,
    ) -> anyhow::Result<()>;
}

pub struct FullmacDevice {
    fullmac_impl_sync_proxy: fidl_fullmac::WlanFullmacImpl_SynchronousProxy,
}

/// TODO(https://fxbug.dev/368323681): Users should be notified when the WlanFullmacImpl channel
/// closes.
impl FullmacDevice {
    pub fn new(
        fullmac_impl_sync_proxy: fidl_fullmac::WlanFullmacImpl_SynchronousProxy,
    ) -> FullmacDevice {
        FullmacDevice { fullmac_impl_sync_proxy }
    }
}

impl DeviceOps for FullmacDevice {
    fn init(
        &mut self,
        fullmac_ifc_client_end: ClientEnd<fidl_fullmac::WlanFullmacImplIfcMarker>,
    ) -> Result<fidl::Channel, zx::Status> {
        let req = fidl_fullmac::WlanFullmacImplInitRequest {
            ifc: Some(fullmac_ifc_client_end),
            ..Default::default()
        };
        let resp = self
            .fullmac_impl_sync_proxy
            .init(req, zx::MonotonicInstant::INFINITE)
            .map_err(|e| {
                log::error!("FIDL error on Start: {}", e);
                zx::Status::INTERNAL
            })?
            .map_err(|e| zx::Status::from_raw(e))?;

        resp.sme_channel.ok_or(zx::Status::INVALID_ARGS)
    }

    fn query_device_info(&self) -> anyhow::Result<fidl_fullmac::WlanFullmacImplQueryResponse> {
        self.fullmac_impl_sync_proxy
            .query(zx::MonotonicInstant::INFINITE)
            .context("FIDL error on QueryDeviceInfo")?
            .map_err(|e| format_err!("Driver returned error on QueryDeviceInfo: {}", e))
    }

    fn query_security_support(&self) -> anyhow::Result<fidl_common::SecuritySupport> {
        self.fullmac_impl_sync_proxy
            .query_security_support(zx::MonotonicInstant::INFINITE)
            .context("FIDL error on QuerySecuritySupport")?
            .map_err(|e| format_err!("Driver returned error on QuerySecuritySupport: {}", e))
    }

    fn query_spectrum_management_support(
        &self,
    ) -> anyhow::Result<fidl_common::SpectrumManagementSupport> {
        self.fullmac_impl_sync_proxy
            .query_spectrum_management_support(zx::MonotonicInstant::INFINITE)
            .context("FIDL error on QuerySpectrumManagementSupport")?
            .map_err(|e| {
                format_err!("Driver returned error on QuerySpectrumManagementSupport: {}", e)
            })
    }

    fn query_telemetry_support(&self) -> anyhow::Result<Result<fidl_stats::TelemetrySupport, i32>> {
        self.fullmac_impl_sync_proxy
            .query_telemetry_support(zx::MonotonicInstant::INFINITE)
            .context("FIDL error on QueryTelemetrySupport")
    }

    fn start_scan(&self, req: fidl_fullmac::WlanFullmacImplStartScanRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .start_scan(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on StartScan")
    }
    fn connect(&self, req: fidl_fullmac::WlanFullmacImplConnectRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .connect(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on Connect")
    }
    fn reconnect(&self, req: fidl_fullmac::WlanFullmacImplReconnectRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .reconnect(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on Reconnect")
    }
    fn roam(&self, req: fidl_fullmac::WlanFullmacImplRoamRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .roam(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on Roam")
    }
    fn auth_resp(&self, resp: fidl_fullmac::WlanFullmacImplAuthRespRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .auth_resp(&resp, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on AuthResp")
    }
    fn deauth(&self, req: fidl_fullmac::WlanFullmacImplDeauthRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .deauth(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on Deauth")
    }
    fn assoc_resp(
        &self,
        resp: fidl_fullmac::WlanFullmacImplAssocRespRequest,
    ) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .assoc_resp(&resp, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on AssocResp")
    }
    fn disassoc(&self, req: fidl_fullmac::WlanFullmacImplDisassocRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .disassoc(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on Disassoc")
    }
    fn start_bss(&self, req: fidl_fullmac::WlanFullmacImplStartBssRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .start_bss(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on StartBss")
    }
    fn stop_bss(&self, req: fidl_fullmac::WlanFullmacImplStopBssRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .stop_bss(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on StopBss")
    }
    fn set_keys(
        &self,
        req: fidl_fullmac::WlanFullmacImplSetKeysRequest,
    ) -> anyhow::Result<fidl_fullmac::WlanFullmacSetKeysResp> {
        self.fullmac_impl_sync_proxy
            .set_keys(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on SetKeysReq")
    }
    fn eapol_tx(&self, req: fidl_fullmac::WlanFullmacImplEapolTxRequest) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .eapol_tx(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on EapolTx")
    }
    fn get_iface_stats(&self) -> anyhow::Result<fidl_mlme::GetIfaceStatsResponse> {
        match self
            .fullmac_impl_sync_proxy
            .get_iface_stats(zx::MonotonicInstant::INFINITE)
            .context("FIDL error on GetIfaceStats")?
        {
            Ok(stats) => Ok(fidl_mlme::GetIfaceStatsResponse::Stats(stats)),
            Err(e) => Ok(fidl_mlme::GetIfaceStatsResponse::ErrorStatus(e)),
        }
    }
    fn get_iface_histogram_stats(
        &self,
    ) -> anyhow::Result<fidl_mlme::GetIfaceHistogramStatsResponse> {
        match self
            .fullmac_impl_sync_proxy
            .get_iface_histogram_stats(zx::MonotonicInstant::INFINITE)
            .context("FIDL error on GetIfaceHistogramStats")?
        {
            Ok(stats) => Ok(fidl_mlme::GetIfaceHistogramStatsResponse::Stats(stats)),
            Err(e) => Ok(fidl_mlme::GetIfaceHistogramStatsResponse::ErrorStatus(e)),
        }
    }
    fn sae_handshake_resp(
        &self,
        resp: fidl_fullmac::WlanFullmacImplSaeHandshakeRespRequest,
    ) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .sae_handshake_resp(&resp, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on SaeHandshakeResp")
    }
    fn sae_frame_tx(&self, frame: fidl_fullmac::SaeFrame) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .sae_frame_tx(&frame, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on SaeFrameTx")
    }
    fn wmm_status_req(&self) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .wmm_status_req(zx::MonotonicInstant::INFINITE)
            .context("FIDL error on WmmStatusReq")
    }
    fn on_link_state_changed(
        &self,
        req: fidl_fullmac::WlanFullmacImplOnLinkStateChangedRequest,
    ) -> anyhow::Result<()> {
        self.fullmac_impl_sync_proxy
            .on_link_state_changed(&req, zx::MonotonicInstant::INFINITE)
            .context("FIDL error on OnLinkStateChanged")
    }
}

#[cfg(test)]
pub mod test_utils {
    use super::*;
    use fidl_fuchsia_wlan_sme as fidl_sme;
    use futures::channel::mpsc;
    use std::sync::{Arc, Mutex};
    use wlan_common::sink::UnboundedSink;

    #[derive(Debug)]
    pub enum DriverCall {
        StartScan { req: fidl_fullmac::WlanFullmacImplStartScanRequest },
        ConnectReq { req: fidl_fullmac::WlanFullmacImplConnectRequest },
        ReconnectReq { req: fidl_fullmac::WlanFullmacImplReconnectRequest },
        RoamReq { req: fidl_fullmac::WlanFullmacImplRoamRequest },
        AuthResp { resp: fidl_fullmac::WlanFullmacImplAuthRespRequest },
        DeauthReq { req: fidl_fullmac::WlanFullmacImplDeauthRequest },
        AssocResp { resp: fidl_fullmac::WlanFullmacImplAssocRespRequest },
        Disassoc { req: fidl_fullmac::WlanFullmacImplDisassocRequest },
        StartBss { req: fidl_fullmac::WlanFullmacImplStartBssRequest },
        StopBss { req: fidl_fullmac::WlanFullmacImplStopBssRequest },
        SetKeys { req: fidl_fullmac::WlanFullmacImplSetKeysRequest },
        EapolTx { req: fidl_fullmac::WlanFullmacImplEapolTxRequest },
        QueryTelemetrySupport,
        GetIfaceStats,
        GetIfaceHistogramStats,
        SaeHandshakeResp { resp: fidl_fullmac::WlanFullmacImplSaeHandshakeRespRequest },
        SaeFrameTx { frame: fidl_fullmac::SaeFrame },
        WmmStatusReq,
        OnLinkStateChanged { req: fidl_fullmac::WlanFullmacImplOnLinkStateChangedRequest },
    }

    pub struct FakeFullmacDeviceMocks {
        pub start_fn_status_mock: Option<zx::sys::zx_status_t>,

        // Note: anyhow::Error isn't cloneable, so the query mocks are all optionals to make this
        // easier to work with.
        //
        // If any of the query mocks are None, then an Err is returned from DeviceOps with an empty
        // error message.
        pub query_device_info_mock: Option<fidl_fullmac::WlanFullmacImplQueryResponse>,
        pub query_security_support_mock: Option<fidl_common::SecuritySupport>,
        pub query_spectrum_management_support_mock: Option<fidl_common::SpectrumManagementSupport>,
        pub query_telemetry_support_mock: Option<Result<fidl_stats::TelemetrySupport, i32>>,

        pub set_keys_resp_mock: Option<fidl_fullmac::WlanFullmacSetKeysResp>,
        pub get_iface_stats_mock: Option<fidl_mlme::GetIfaceStatsResponse>,
        pub get_iface_histogram_stats_mock: Option<fidl_mlme::GetIfaceHistogramStatsResponse>,

        pub fullmac_ifc_client_end: Option<ClientEnd<fidl_fullmac::WlanFullmacImplIfcMarker>>,
    }

    unsafe impl Send for FakeFullmacDevice {}
    pub struct FakeFullmacDevice {
        pub usme_bootstrap_client_end:
            Option<fidl::endpoints::ClientEnd<fidl_sme::UsmeBootstrapMarker>>,
        pub usme_bootstrap_server_end:
            Option<fidl::endpoints::ServerEnd<fidl_sme::UsmeBootstrapMarker>>,
        driver_call_sender: UnboundedSink<DriverCall>,

        // This is boxed because tests want a reference to this to check captured calls, but in
        // production we pass ownership of the DeviceOps to FullmacMlme. This avoids changing
        // ownership semantics for tests.
        pub mocks: Arc<Mutex<FakeFullmacDeviceMocks>>,
    }

    impl FakeFullmacDevice {
        pub fn new() -> (Self, mpsc::UnboundedReceiver<DriverCall>) {
            // Create a channel for SME requests, to be surfaced by init().
            let (usme_bootstrap_client_end, usme_bootstrap_server_end) =
                fidl::endpoints::create_endpoints::<fidl_sme::UsmeBootstrapMarker>();

            let (driver_call_sender, driver_call_receiver) = mpsc::unbounded();

            let device = Self {
                usme_bootstrap_client_end: Some(usme_bootstrap_client_end),
                usme_bootstrap_server_end: Some(usme_bootstrap_server_end),
                driver_call_sender: UnboundedSink::new(driver_call_sender),
                mocks: Arc::new(Mutex::new(FakeFullmacDeviceMocks {
                    fullmac_ifc_client_end: None,
                    start_fn_status_mock: None,
                    query_device_info_mock: Some(fidl_fullmac::WlanFullmacImplQueryResponse {
                        sta_addr: Some([0u8; 6]),
                        role: Some(fidl_common::WlanMacRole::Client),
                        band_caps: Some(vec![]),
                        ..Default::default()
                    }),
                    query_security_support_mock: Some(fidl_common::SecuritySupport {
                        sae: fidl_common::SaeFeature {
                            driver_handler_supported: false,
                            sme_handler_supported: true,
                        },
                        mfp: fidl_common::MfpFeature { supported: false },
                    }),
                    query_spectrum_management_support_mock: Some(
                        fidl_common::SpectrumManagementSupport {
                            dfs: fidl_common::DfsFeature { supported: false },
                        },
                    ),
                    query_telemetry_support_mock: Some(Ok(fidl_stats::TelemetrySupport {
                        ..Default::default()
                    })),
                    set_keys_resp_mock: None,
                    get_iface_stats_mock: None,
                    get_iface_histogram_stats_mock: None,
                })),
            };

            (device, driver_call_receiver)
        }
    }

    impl DeviceOps for FakeFullmacDevice {
        fn init(
            &mut self,
            fullmac_ifc_client_end: ClientEnd<fidl_fullmac::WlanFullmacImplIfcMarker>,
        ) -> Result<fidl::Channel, zx::Status> {
            let mut mocks = self.mocks.lock().unwrap();

            mocks.fullmac_ifc_client_end = Some(fullmac_ifc_client_end);
            match mocks.start_fn_status_mock {
                Some(status) => Err(zx::Status::from_raw(status)),

                // Start can only be called once since this moves usme_bootstrap_server_end.
                None => Ok(self.usme_bootstrap_server_end.take().unwrap().into_channel()),
            }
        }

        fn query_device_info(&self) -> anyhow::Result<fidl_fullmac::WlanFullmacImplQueryResponse> {
            self.mocks.lock().unwrap().query_device_info_mock.clone().ok_or(format_err!(""))
        }

        fn query_security_support(&self) -> anyhow::Result<fidl_common::SecuritySupport> {
            self.mocks.lock().unwrap().query_security_support_mock.clone().ok_or(format_err!(""))
        }

        fn query_spectrum_management_support(
            &self,
        ) -> anyhow::Result<fidl_common::SpectrumManagementSupport> {
            self.mocks
                .lock()
                .unwrap()
                .query_spectrum_management_support_mock
                .clone()
                .ok_or(format_err!(""))
        }

        fn query_telemetry_support(
            &self,
        ) -> anyhow::Result<Result<fidl_stats::TelemetrySupport, i32>> {
            self.driver_call_sender.send(DriverCall::QueryTelemetrySupport);
            self.mocks.lock().unwrap().query_telemetry_support_mock.clone().ok_or(format_err!(""))
        }

        // Cannot mark fn unsafe because it has to match fn signature in FullDeviceInterface
        fn start_scan(
            &self,
            req: fidl_fullmac::WlanFullmacImplStartScanRequest,
        ) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::StartScan { req });
            Ok(())
        }

        fn connect(&self, req: fidl_fullmac::WlanFullmacImplConnectRequest) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::ConnectReq { req });
            Ok(())
        }
        fn reconnect(
            &self,
            req: fidl_fullmac::WlanFullmacImplReconnectRequest,
        ) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::ReconnectReq { req });
            Ok(())
        }
        fn roam(&self, req: fidl_fullmac::WlanFullmacImplRoamRequest) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::RoamReq { req });
            Ok(())
        }
        fn auth_resp(
            &self,
            resp: fidl_fullmac::WlanFullmacImplAuthRespRequest,
        ) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::AuthResp { resp });
            Ok(())
        }
        fn deauth(&self, req: fidl_fullmac::WlanFullmacImplDeauthRequest) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::DeauthReq { req });
            Ok(())
        }
        fn assoc_resp(
            &self,
            resp: fidl_fullmac::WlanFullmacImplAssocRespRequest,
        ) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::AssocResp { resp });
            Ok(())
        }
        fn disassoc(
            &self,
            req: fidl_fullmac::WlanFullmacImplDisassocRequest,
        ) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::Disassoc { req });
            Ok(())
        }
        fn start_bss(
            &self,
            req: fidl_fullmac::WlanFullmacImplStartBssRequest,
        ) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::StartBss { req });
            Ok(())
        }
        fn stop_bss(&self, req: fidl_fullmac::WlanFullmacImplStopBssRequest) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::StopBss { req });
            Ok(())
        }
        fn set_keys(
            &self,
            req: fidl_fullmac::WlanFullmacImplSetKeysRequest,
        ) -> anyhow::Result<fidl_fullmac::WlanFullmacSetKeysResp> {
            let num_keys = req.keylist.as_ref().unwrap().len();
            self.driver_call_sender.send(DriverCall::SetKeys { req });
            match &self.mocks.lock().unwrap().set_keys_resp_mock {
                Some(resp) => Ok(resp.clone()),
                None => {
                    Ok(fidl_fullmac::WlanFullmacSetKeysResp { statuslist: vec![0i32; num_keys] })
                }
            }
        }
        fn eapol_tx(&self, req: fidl_fullmac::WlanFullmacImplEapolTxRequest) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::EapolTx { req });
            Ok(())
        }
        fn get_iface_stats(&self) -> anyhow::Result<fidl_mlme::GetIfaceStatsResponse> {
            self.driver_call_sender.send(DriverCall::GetIfaceStats);
            Ok(self.mocks.lock().unwrap().get_iface_stats_mock.clone().unwrap_or(
                fidl_mlme::GetIfaceStatsResponse::ErrorStatus(zx::sys::ZX_ERR_NOT_SUPPORTED),
            ))
        }
        fn get_iface_histogram_stats(
            &self,
        ) -> anyhow::Result<fidl_mlme::GetIfaceHistogramStatsResponse> {
            self.driver_call_sender.send(DriverCall::GetIfaceHistogramStats);
            Ok(self.mocks.lock().unwrap().get_iface_histogram_stats_mock.clone().unwrap_or(
                fidl_mlme::GetIfaceHistogramStatsResponse::ErrorStatus(
                    zx::sys::ZX_ERR_NOT_SUPPORTED,
                ),
            ))
        }
        fn sae_handshake_resp(
            &self,
            resp: fidl_fullmac::WlanFullmacImplSaeHandshakeRespRequest,
        ) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::SaeHandshakeResp { resp });
            Ok(())
        }
        fn sae_frame_tx(&self, frame: fidl_fullmac::SaeFrame) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::SaeFrameTx { frame });
            Ok(())
        }
        fn wmm_status_req(&self) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::WmmStatusReq);
            Ok(())
        }
        fn on_link_state_changed(
            &self,
            req: fidl_fullmac::WlanFullmacImplOnLinkStateChangedRequest,
        ) -> anyhow::Result<()> {
            self.driver_call_sender.send(DriverCall::OnLinkStateChanged { req });
            Ok(())
        }
    }
}
