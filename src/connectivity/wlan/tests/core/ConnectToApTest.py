# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for connecting to an access point.
"""
import logging

logger = logging.getLogger(__name__)


import fidl_fuchsia_wlan_common as fidl_common
import fidl_fuchsia_wlan_common_security as fidl_security
import fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211
import fidl_fuchsia_wlan_sme as fidl_sme
from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_2G,
    AP_SSID_LENGTH_2G,
)
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from antlion.controllers.ap_lib.hostapd_utils import generate_random_password
from core_testing import base_test
from core_testing.handlers import ConnectTransactionEventHandler
from core_testing.ies import Ie, read_ssid
from fuchsia_controller_py.wrappers import asyncmethod
from mobly import test_runner
from mobly.asserts import assert_equal, assert_true, fail


class ConnectToApTest(base_test.ConnectionBaseTestClass):
    def pre_run(self) -> None:
        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self.name_func,
            arg_sets=[
                (Security(security_mode=SecurityMode.OPEN),),
                (
                    Security(
                        security_mode=SecurityMode.WPA2,
                        password=generate_random_password(),
                    ),
                ),
            ],
        )

    def name_func(self, security: Security) -> str:
        return f"test_successfully_connect_to_ap_{security.security_mode}"

    @asyncmethod
    async def _test_logic(self, security: Security) -> None:
        ssid = utils.rand_ascii_str(AP_SSID_LENGTH_2G)

        setup_ap(
            access_point=self.access_point(),
            profile_name="whirlwind",
            channel=AP_DEFAULT_CHANNEL_2G,
            ssid=ssid,
            security=security,
        )

        scan_results = (
            (
                await self.client_sme_proxy.scan_for_controller(
                    req=fidl_sme.ScanRequest(
                        passive=fidl_sme.PassiveScanRequest()
                    )
                )
            )
            .unwrap()
            .scan_results
        )
        assert (
            scan_results is not None
        ), "ClientSme.ScanForController() response is missing scan_results"

        bss_description = None
        for scan_result in scan_results:
            assert (
                scan_result.bss_description is not None
            ), "ScanResult is missing bss_description"
            assert (
                scan_result.bss_description.ies is not None
            ), "ScanResult.BssDescription is missing ies"
            scanned_ssid = read_ssid(bytes(scan_result.bss_description.ies))
            if scanned_ssid == ssid:
                logger.info(f"Found SSID: {scanned_ssid}")
                logger.info(f"Scan result: {scan_result!r}")
                logger.info(
                    f"IEs: {Ie.read_ies(bytes(scan_result.bss_description.ies))!r}"
                )
                bss_description = scan_result.bss_description
                break
        assert bss_description is not None, f"Failed to find SSID: {ssid}"

        with ConnectTransactionEventHandler() as ctx:
            txn_queue = ctx.txn_queue
            server = ctx.server

            credentials = None
            protocol = fidl_security.Protocol.OPEN
            if security.security_mode == SecurityMode.OPEN:
                pass
            elif security.security_mode == SecurityMode.WPA2:
                protocol = fidl_security.Protocol.WPA2_PERSONAL
                credentials = fidl_security.Credentials(
                    wpa=fidl_security.WpaCredentials(
                        passphrase=list(security.password.encode("ascii"))
                    )
                )
            else:
                fail(f"Unsupported security mode: {security.security_mode}")

            connect_request = fidl_sme.ConnectRequest(
                ssid=list(ssid.encode("ascii")),
                bss_description=bss_description,
                multiple_bss_candidates=False,
                authentication=fidl_security.Authentication(
                    protocol=protocol,
                    credentials=credentials,
                ),
                deprecated_scan_type=fidl_common.ScanType.PASSIVE,
            )
            logger.info(f"ConnectRequest: {connect_request!r}")
            self.client_sme_proxy.connect(
                req=connect_request, txn=server.take()
            )

            next_txn = await txn_queue.get()
            assert_equal(
                next_txn,
                fidl_sme.ConnectTransactionOnConnectResultRequest(
                    result=fidl_sme.ConnectResult(
                        code=fidl_ieee80211.StatusCode.SUCCESS,
                        is_credential_rejected=False,
                        is_reconnect=False,
                    )
                ),
            )
            assert_true(
                txn_queue.empty(),
                "Unexpectedly received additional callback messages.",
            )


if __name__ == "__main__":
    test_runner.main()
