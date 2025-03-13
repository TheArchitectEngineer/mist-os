# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for ffx_impl.py."""

import ipaddress
import json
import unittest
from collections.abc import Callable
from typing import Any
from unittest import mock

import fuchsia_controller_py as fuchsia_controller
from parameterized import param, parameterized

from honeydew import errors
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx_impl
from honeydew.transports.ffx import types as ffx_types
from honeydew.typing import custom_types
from honeydew.utils import host_shell

# pylint: disable=protected-access
_TARGET_NAME: str = "fuchsia-emulator"

_IPV6: str = "fe80::4fce:3102:ef13:888c%qemu"
_IPV6_OBJ: ipaddress.IPv6Address = ipaddress.IPv6Address(_IPV6)

_SSH_ADDRESS: ipaddress.IPv6Address = _IPV6_OBJ
_SSH_PORT = 8022
_TARGET_SSH_ADDRESS = custom_types.TargetSshAddress(
    ip=_SSH_ADDRESS, port=_SSH_PORT
)

_ISOLATE_DIR: str = "/tmp/isolate"
_LOGS_DIR: str = "/tmp/logs"
_BINARY_PATH: str = "ffx"
_LOGS_LEVEL: str = "debug"
_MDNS_ENABLED: bool = False
_SUBTOOLS_SEARCH_PATH: str = "/subtools"
_PROXY_TIMEOUT_SECS: int = 30
_SSH_KEEPALIVE_TIMEOUT: int = 60

_FFX_TARGET_SHOW_JSON: dict[str, Any] = {
    "target": {
        "name": _TARGET_NAME,
        "ssh_address": {"host": f"{_SSH_ADDRESS}", "port": _SSH_PORT},
        "compatibility_state": "supported",
        "compatibility_message": "",
        "last_reboot_graceful": "false",
        "last_reboot_reason": None,
        "uptime_nanos": -1,
    },
    "board": {
        "name": "default-board",
        "revision": None,
        "instruction_set": "x64",
    },
    "device": {
        "serial_number": "1234321",
        "retail_sku": None,
        "retail_demo": None,
        "device_id": None,
    },
    "product": {
        "audio_amplifier": None,
        "build_date": None,
        "build_name": None,
        "colorway": None,
        "display": None,
        "emmc_storage": None,
        "language": None,
        "regulatory_domain": None,
        "locale_list": None,
        "manufacturer": None,
        "microphone": None,
        "model": None,
        "name": None,
        "nand_storage": None,
        "memory": None,
        "sku": None,
    },
    "update": {"current_channel": None, "next_channel": None},
    "build": {
        "version": "2023-02-01T17:26:40+00:00",
        "product": "workstation_eng",
        "board": "x64",
        "commit": "2023-02-01T17:26:40+00:00",
    },
}

_FFX_TARGET_SHOW_OUTPUT: str = json.dumps(_FFX_TARGET_SHOW_JSON)
_FFX_TARGET_SHOW_INFO = ffx_types.TargetInfoData(**_FFX_TARGET_SHOW_JSON)

_FFX_TARGET_LIST_OUTPUT: str = (
    '[{"nodename":"fuchsia-emulator","rcs_state":"Y","serial":"<unknown>",'
    '"target_type":"workstation_eng.x64","target_state":"Product",'
    '"addresses":["fe80::6a47:a931:1e84:5077%qemu"],"is_default":true}]\n'
)

_FFX_TARGET_LIST_JSON: list[dict[str, Any]] = [
    {
        "nodename": _TARGET_NAME,
        "rcs_state": "Y",
        "serial": "<unknown>",
        "target_type": "workstation_eng.x64",
        "target_state": "Product",
        "addresses": ["fe80::6a47:a931:1e84:5077%qemu"],
        "is_default": True,
    }
]


_INPUT_ARGS: dict[str, Any] = {
    "target_name": _TARGET_NAME,
    "target_ip_port": _TARGET_SSH_ADDRESS,
    "ffx_config_data": ffx_config.FfxConfigData(
        isolate_dir=fuchsia_controller.IsolateDir(_ISOLATE_DIR),
        logs_dir=_LOGS_DIR,
        binary_path=_BINARY_PATH,
        logs_level=_LOGS_LEVEL,
        mdns_enabled=_MDNS_ENABLED,
        subtools_search_path=_SUBTOOLS_SEARCH_PATH,
        proxy_timeout_secs=_PROXY_TIMEOUT_SECS,
        ssh_keepalive_timeout=_SSH_KEEPALIVE_TIMEOUT,
    ),
    "run_cmd": ffx_impl._FFX_CMDS["TARGET_SHOW"],
}

_MOCK_ARGS: dict[str, Any] = {
    "ffx_target_show_output": _FFX_TARGET_SHOW_OUTPUT,
    "ffx_target_show_json": _FFX_TARGET_SHOW_JSON,
    "ffx_target_show_object": _FFX_TARGET_SHOW_INFO,
    "ffx_target_ssh_address_output": f"[{_SSH_ADDRESS}]:{_SSH_PORT}",
    "ffx_target_list_output": _FFX_TARGET_LIST_OUTPUT,
    "ffx_target_list_json": _FFX_TARGET_LIST_JSON,
}

_EXPECTED_VALUES: dict[str, Any] = {
    "ffx_target_show_output": _FFX_TARGET_SHOW_OUTPUT,
    "ffx_target_show_object": _FFX_TARGET_SHOW_INFO,
    "ffx_target_show_json": _FFX_TARGET_SHOW_JSON,
    "ffx_target_list_json": _FFX_TARGET_LIST_JSON,
}


def _custom_test_name_func(
    testcase_func: Callable[..., None], _: str, param_arg: param
) -> str:
    """Custom name function method."""
    test_func_name: str = testcase_func.__name__

    params_dict: dict[str, Any] = param_arg.args[0]
    test_label: str = parameterized.to_safe_name(params_dict["label"])

    return f"{test_func_name}_with_{test_label}"


class FfxImplTests(unittest.TestCase):
    """Unit tests for ffx_impl.FfxImpl"""

    def setUp(self) -> None:
        super().setUp()

        with (
            mock.patch.object(
                ffx_impl.FfxImpl,
                "check_connection",
                autospec=True,
            ) as mock_ffx_check_connection,
        ):
            self.ffx_obj_wo_ip = ffx_impl.FfxImpl(
                target_name=_INPUT_ARGS["target_name"],
                config_data=_INPUT_ARGS["ffx_config_data"],
            )
        mock_ffx_check_connection.assert_called()

        mock_ffx_check_connection.reset_mock()

        with (
            mock.patch.object(
                ffx_impl.FfxImpl,
                "check_connection",
                autospec=True,
            ) as mock_ffx_check_connection,
            mock.patch.object(
                ffx_impl.FfxImpl,
                "add_target",
                autospec=True,
            ) as mock_ffx_add_target,
        ):
            self.ffx_obj_with_ip = ffx_impl.FfxImpl(
                target_name=_INPUT_ARGS["target_name"],
                target_ip_port=_INPUT_ARGS["target_ip_port"],
                config_data=_INPUT_ARGS["ffx_config_data"],
            )
        mock_ffx_check_connection.assert_called()
        mock_ffx_add_target.assert_called()

    def test_ffx_init_with_ip_as_target_name(self) -> None:
        """Test case for ffx_impl.FfxImpl() when called with target_name=<ip>."""
        with self.assertRaises(ValueError):
            ffx_impl.FfxImpl(
                target_name=_IPV6,
                config_data=_INPUT_ARGS["ffx_config_data"],
            )

    @mock.patch.object(
        ffx_impl.FfxImpl, "wait_for_rcs_connection", autospec=True
    )
    def test_check_connection(
        self, mock_wait_for_rcs_connection: mock.Mock
    ) -> None:
        """Test case for check_connection()"""
        self.ffx_obj_with_ip.check_connection()

        mock_wait_for_rcs_connection.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "wait_for_rcs_connection",
        side_effect=errors.DeviceNotConnectedError(
            ffx_impl._DEVICE_NOT_CONNECTED
        ),
        autospec=True,
    )
    def test_check_connection_raises(
        self, mock_wait_for_rcs_connection: mock.Mock
    ) -> None:
        """Test case for check_connection() raising ffx_errors.FfxConnectionError"""
        with self.assertRaises(ffx_errors.FfxConnectionError):
            self.ffx_obj_with_ip.check_connection()

        mock_wait_for_rcs_connection.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        return_value=_MOCK_ARGS["ffx_target_show_output"],
        autospec=True,
    )
    def test_get_target_information(self, mock_ffx_run: mock.Mock) -> None:
        """Verify get_target_information()."""
        self.assertEqual(
            self.ffx_obj_with_ip.get_target_information(),
            _EXPECTED_VALUES["ffx_target_show_object"],
        )

        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        return_value=_MOCK_ARGS["ffx_target_list_output"],
        autospec=True,
    )
    def test_get_target_info_from_target_list(
        self, mock_ffx_run: mock.Mock
    ) -> None:
        """Test case for get_target_info_from_target_list()."""
        mock_ffx_run.return_value = _MOCK_ARGS["ffx_target_list_output"]

        self.assertEqual(
            self.ffx_obj_with_ip.get_target_info_from_target_list(),
            _EXPECTED_VALUES["ffx_target_list_json"][0],
        )

        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        return_value="[]",
        autospec=True,
    )
    def test_get_target_info_from_target_list_exception(
        self,
        mock_ffx_run: mock.Mock,
    ) -> None:
        """Test case for get_target_info_from_target_list() raising exception."""
        with self.assertRaises(ffx_errors.FfxCommandError):
            self.ffx_obj_with_ip.get_target_info_from_target_list()
        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        return_value=_MOCK_ARGS["ffx_target_ssh_address_output"],
        autospec=True,
    )
    def test_get_target_ssh_address(self, mock_ffx_run: mock.Mock) -> None:
        """Verify get_target_ssh_address returns SSH information of the fuchsia
        device."""
        self.assertEqual(
            self.ffx_obj_with_ip.get_target_ssh_address(), _TARGET_SSH_ADDRESS
        )
        mock_ffx_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "get_target_information",
        return_value=_MOCK_ARGS["ffx_target_show_object"],
        autospec=True,
    )
    def test_get_target_board(
        self, mock_get_target_information: mock.Mock
    ) -> None:
        """Verify ffx_impl.get_target_board returns board value of fuchsia device."""
        result: str = self.ffx_obj_with_ip.get_target_board()
        expected: str | None = _FFX_TARGET_SHOW_INFO.build.board

        self.assertEqual(result, expected)

        mock_get_target_information.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "get_target_information",
        return_value=_MOCK_ARGS["ffx_target_show_object"],
        autospec=True,
    )
    def test_get_target_product(
        self, mock_get_target_information: mock.Mock
    ) -> None:
        """Verify ffx_impl.get_target_product returns product value of fuchsia
        device."""
        result: str = self.ffx_obj_with_ip.get_target_product()
        expected: str | None = _FFX_TARGET_SHOW_INFO.build.product

        self.assertEqual(result, expected)

        mock_get_target_information.assert_called()

    @mock.patch.object(
        host_shell,
        "run",
        return_value=_MOCK_ARGS["ffx_target_show_output"],
        autospec=True,
    )
    def test_ffx_run(self, mock_host_shell_run: mock.Mock) -> None:
        """Test case for ffx_impl.run()"""
        self.assertEqual(
            self.ffx_obj_with_ip.run(cmd=_INPUT_ARGS["run_cmd"]),
            _EXPECTED_VALUES["ffx_target_show_output"],
        )

        mock_host_shell_run.assert_called_with(
            [
                _BINARY_PATH,
                "-t",
                str(_TARGET_SSH_ADDRESS),
                "--isolate-dir",
                _ISOLATE_DIR,
            ]
            + ffx_impl._FFX_CMDS["TARGET_SHOW"],
            capture_output=True,
            log_output=True,
            timeout=None,
        )

    @parameterized.expand(
        [
            (
                {
                    "label": "DeviceNotConnectedError",
                    "side_effect": errors.HostCmdError(
                        ffx_impl._DEVICE_NOT_CONNECTED,
                    ),
                    "expected_error": errors.DeviceNotConnectedError,
                },
            ),
            (
                {
                    "label": "FfxCommandError",
                    "side_effect": errors.HostCmdError(
                        "command output and error",
                    ),
                    "expected_error": ffx_errors.FfxCommandError,
                },
            ),
            (
                {
                    "label": "TimeoutExpired",
                    "side_effect": errors.HoneydewTimeoutError(
                        "timed out",
                    ),
                    "expected_error": ffx_errors.FfxTimeoutError,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    @mock.patch.object(
        host_shell,
        "run",
        autospec=True,
    )
    def test_ffx_run_exceptions(
        self,
        parameterized_dict: dict[str, Any],
        mock_host_shell_run: mock.Mock,
    ) -> None:
        """Test case for ffx_impl.run() raising different
        exceptions."""
        mock_host_shell_run.side_effect = parameterized_dict["side_effect"]

        with self.assertRaises(parameterized_dict["expected_error"]):
            self.ffx_obj_with_ip.run(cmd=_INPUT_ARGS["run_cmd"])

        mock_host_shell_run.assert_called()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        autospec=True,
    )
    def test_ffx_run_test_component(self, mock_ffx_run: mock.Mock) -> None:
        """Test case for ffx_impl.run_test_component()"""
        self.ffx_obj_with_ip.run_test_component(
            "fuchsia-pkg://fuchsia.com/testing#meta/test.cm",
            ffx_test_args=["--foo", "bar"],
            test_component_args=["baz", "--x", "2"],
            capture_output=False,
        )

        mock_ffx_run.assert_called_with(
            self.ffx_obj_with_ip,
            [
                "test",
                "run",
                "fuchsia-pkg://fuchsia.com/testing#meta/test.cm",
                "--foo",
                "bar",
                "--",
                "baz",
                "--x",
                "2",
            ],
            capture_output=False,
        )

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "run",
        autospec=True,
    )
    def test_ffx_run_ssh_cmd(self, mock_ffx_run: mock.Mock) -> None:
        """Test case for ffx_impl.run_ssh_cmd()"""
        self.ffx_obj_with_ip.run_ssh_cmd(
            cmd="killall iperf3",
            capture_output=True,
        )

        mock_ffx_run.assert_called_with(
            self.ffx_obj_with_ip,
            [
                "target",
                "ssh",
                "killall iperf3",
            ],
            capture_output=True,
        )

    @mock.patch.object(
        host_shell,
        "popen",
        return_value=None,
        autospec=True,
    )
    def test_ffx_popen(self, mock_host_shell_popen: mock.Mock) -> None:
        """Test case for ffx_impl.popen()"""
        self.ffx_obj_with_ip.popen(
            cmd=["a", "b", "c"],
            # Popen forwards arbitrary kvargs to subprocess.Popen
            stdout="abc",
        )

        mock_host_shell_popen.assert_called_with(
            [
                _BINARY_PATH,
                "-t",
                str(_TARGET_SSH_ADDRESS),
                "--isolate-dir",
                _ISOLATE_DIR,
            ]
            + ["a", "b", "c"],
            stdout="abc",
        )

    @mock.patch.object(host_shell, "run", autospec=True)
    def test_add_target(self, mock_host_shell_run: mock.Mock) -> None:
        """Test case for ffx_cli.add_target()."""
        self.ffx_obj_with_ip.add_target()

        mock_host_shell_run.assert_called_once()

    @parameterized.expand(
        [
            (
                {
                    "label": "DeviceNotConnectedError",
                    "side_effect": errors.HostCmdError(
                        ffx_impl._DEVICE_NOT_CONNECTED,
                    ),
                    "expected_error": errors.DeviceNotConnectedError,
                },
            ),
            (
                {
                    "label": "FfxCommandError",
                    "side_effect": errors.HostCmdError(
                        "command output and error",
                    ),
                    "expected_error": ffx_errors.FfxCommandError,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    @mock.patch.object(
        host_shell,
        "run",
        autospec=True,
    )
    def test_add_target_exception(
        self,
        parameterized_dict: dict[str, Any],
        mock_host_shell_run: mock.Mock,
    ) -> None:
        """Verify ffx_cli.add_target raise exception in failure cases."""
        mock_host_shell_run.side_effect = parameterized_dict["side_effect"]

        expected = parameterized_dict["expected_error"]

        with self.assertRaises(expected):
            self.ffx_obj_with_ip.add_target()

        mock_host_shell_run.assert_called_once()

    @mock.patch.object(
        ffx_impl.FfxImpl,
        "get_target_information",
        return_value=_MOCK_ARGS["ffx_target_show_object"],
        autospec=True,
    )
    def test_get_target_name(
        self, mock_ffx_get_target_information: mock.Mock
    ) -> None:
        """Verify get_target_name returns the name of the fuchsia device."""
        self.assertEqual(self.ffx_obj_with_ip.get_target_name(), _TARGET_NAME)

        mock_ffx_get_target_information.assert_called()

    @mock.patch.object(ffx_impl.FfxImpl, "run", return_value="", autospec=True)
    def test_wait_for_rcs_connection(self, mock_ffx_run: mock.Mock) -> None:
        """Test case for ffx_impl.wait_for_rcs_connection()"""
        self.ffx_obj_with_ip.wait_for_rcs_connection()
        mock_ffx_run.assert_called()

    @mock.patch.object(ffx_impl.FfxImpl, "run", return_value="", autospec=True)
    def test_wait_for_rcs_disconnection(self, mock_ffx_run: mock.Mock) -> None:
        """Test case for ffx_impl.wait_for_rcs_disconnection()"""
        self.ffx_obj_with_ip.wait_for_rcs_disconnection()
        self.assertEqual(mock_ffx_run.call_count, 2)
