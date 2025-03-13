# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.__init__.py."""

import unittest
from typing import Any
from unittest import mock

import fuchsia_controller_py as fuchsia_controller

import honeydew
from honeydew import errors
from honeydew.fuchsia_device import fuchsia_device_impl
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import ffx_impl
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller_impl as fuchsia_controller_transport,
)
from honeydew.transports.sl4f import sl4f as sl4f_transport
from honeydew.typing import custom_types

_TARGET_NAME: str = "fuchsia-emulator"

_REMOTE_TARGET_IP_PORT: str = "[::1]:8088"
_REMOTE_TARGET_IP_PORT_OBJ: custom_types.IpPort = (
    custom_types.IpPort.create_using_ip_and_port(_REMOTE_TARGET_IP_PORT)
)

_INPUT_ARGS: dict[str, Any] = {
    "ffx_config_data": ffx_config.FfxConfigData(
        isolate_dir=fuchsia_controller.IsolateDir("/tmp/isolate"),
        logs_dir="/tmp/logs",
        binary_path="/bin/ffx",
        logs_level="debug",
        mdns_enabled=False,
        subtools_search_path=None,
        proxy_timeout_secs=None,
        ssh_keepalive_timeout=None,
    ),
    "target_name": _TARGET_NAME,
    "target_ip_port": _REMOTE_TARGET_IP_PORT_OBJ,
}


# pylint: disable=protected-access
class InitTests(unittest.TestCase):
    """Unit tests for honeydew.__init__.py."""

    # List all the tests related to public methods
    @mock.patch.object(
        sl4f_transport.SL4F,
        "check_connection",
        autospec=True,
    )
    @mock.patch.object(ffx_impl.FfxImpl, "check_connection", autospec=True)
    @mock.patch.object(
        fuchsia_controller_transport.FuchsiaControllerImpl,
        "check_connection",
        autospec=True,
    )
    @mock.patch("fuchsia_controller_py.Context", autospec=True)
    def test_create_device_return(
        self,
        mock_fc_context: mock.Mock,
        mock_ffx_check_connection: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_sl4f_check_connection: mock.Mock,
    ) -> None:
        """Test case for honeydew.create_device()."""
        self.assertIsInstance(
            honeydew.create_device(
                device_info=custom_types.DeviceInfo(
                    name=_INPUT_ARGS["target_name"],
                    ip_port=None,
                    serial_socket=None,
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
            ),
            fuchsia_device_impl.FuchsiaDeviceImpl,
        )

        mock_fc_context.assert_called_once_with(
            config={},
            isolate_dir=_INPUT_ARGS["ffx_config_data"].isolate_dir,
            target=_INPUT_ARGS["target_name"],
        )
        mock_fc_check_connection.assert_called()

        mock_ffx_check_connection.assert_called()

        mock_sl4f_check_connection.assert_not_called()

    @mock.patch.object(ffx_impl.FfxImpl, "check_connection", autospec=True)
    @mock.patch.object(
        ffx_impl.FfxImpl,
        "add_target",
        autospec=True,
    )
    @mock.patch.object(
        fuchsia_controller_transport.FuchsiaControllerImpl,
        "check_connection",
        autospec=True,
    )
    @mock.patch("fuchsia_controller_py.Context", autospec=True)
    def test_create_device_using_device_ip_port(
        self,
        mock_fc_context: mock.Mock,
        mock_fc_check_connection: mock.Mock,
        mock_ffx_add_target: mock.Mock,
        mock_ffx_check_connection: mock.Mock,
    ) -> None:
        """Test case for honeydew.create_device() where it returns a device
        from an IpPort."""
        self.assertIsInstance(
            honeydew.create_device(
                custom_types.DeviceInfo(
                    name=_INPUT_ARGS["target_name"],
                    ip_port=_INPUT_ARGS["target_ip_port"],
                    serial_socket=None,
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
            ),
            fuchsia_device_impl.FuchsiaDeviceImpl,
        )

        mock_fc_context.assert_called_once_with(
            config={},
            isolate_dir=_INPUT_ARGS["ffx_config_data"].isolate_dir,
            target=str(_INPUT_ARGS["target_ip_port"]),
        )
        mock_fc_check_connection.assert_called()

        mock_ffx_add_target.assert_called()
        mock_ffx_check_connection.assert_called()

    @mock.patch.object(
        fuchsia_device_impl.FuchsiaDeviceImpl,
        "__init__",
        side_effect=fc_errors.FuchsiaControllerConnectionError("Error"),
        autospec=True,
    )
    def test_create_device_using_device_ip_port_throws_error(
        self,
        mock_fc_fuchsia_device: mock.Mock,
    ) -> None:
        """Test case for honeydew.create_device() where it raises an error."""
        with self.assertRaises(errors.FuchsiaDeviceError):
            honeydew.create_device(
                custom_types.DeviceInfo(
                    name=_INPUT_ARGS["target_name"],
                    ip_port=_INPUT_ARGS["target_ip_port"],
                    serial_socket=None,
                ),
                ffx_config_data=_INPUT_ARGS["ffx_config_data"],
            )

        mock_fc_fuchsia_device.assert_called()


if __name__ == "__main__":
    unittest.main()
