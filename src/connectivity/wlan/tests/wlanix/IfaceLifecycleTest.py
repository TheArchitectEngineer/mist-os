# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Battery of tests of the lifecycle of ifaces managed by wlanix.
"""

import asyncio

import fidl_fuchsia_wlan_wlanix as fidl_wlanix
from fuchsia_controller_py import Channel
from mobly import base_test, test_runner
from mobly.asserts import assert_equal
from wlanix_testing import base_test


class IfaceLifecycleTest(base_test.WifiChipBaseTestClass):
    def test_create_and_destroy_iface(self) -> None:
        response = asyncio.run(
            self.wifi_chip_proxy.get_sta_iface_names()
        ).unwrap()
        assert_equal(
            len(response.iface_names),
            0,
            "WifiChip should have returned an empty list of iface names",
        )

        proxy, server = Channel.create()
        asyncio.run(
            self.wifi_chip_proxy.create_sta_iface(iface=server.take())
        ).unwrap()
        wifi_sta_iface = fidl_wlanix.WifiStaIfaceClient(proxy)

        response = asyncio.run(
            self.wifi_chip_proxy.get_sta_iface_names()
        ).unwrap()
        assert_equal(
            len(response.iface_names),
            1,
            "WifiChip should have returned the iface just created",
        )

        iface_name = response.iface_names[0]
        response = asyncio.run(wifi_sta_iface.get_name()).unwrap()
        assert_equal(
            iface_name,
            response.iface_name,
            "WifiStaIface returns a different name than WifiChip",
        )
        asyncio.run(
            self.wifi_chip_proxy.remove_sta_iface(iface_name=iface_name)
        ).unwrap()

        response = asyncio.run(
            self.wifi_chip_proxy.get_sta_iface_names()
        ).unwrap()
        assert_equal(
            len(response.iface_names),
            0,
            "WifiChip should no longer contain the iface just removed",
        )

        # TODO(https://fxbug.dev/365110075): Uncomment this check once wlanix supports
        # stopping a WifiStaIface after its iface is removed via WifiChip.RemoveStaIface
        # with assertRaises(Exception):
        #     asyncio.run(wifi_sta_iface.get_name()).unwrap())


if __name__ == "__main__":
    test_runner.main()
