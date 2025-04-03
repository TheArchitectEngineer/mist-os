# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""FuchsiaDevice abstract base class implementation."""

import asyncio
import ipaddress
import json
import logging
import os
from collections.abc import Callable
from datetime import datetime
from functools import cached_property
from typing import Any

import fidl.fuchsia_buildinfo as f_buildinfo
import fidl.fuchsia_developer_remotecontrol as fd_remotecontrol
import fidl.fuchsia_diagnostics as f_diagnostics
import fidl.fuchsia_feedback as f_feedback
import fidl.fuchsia_hardware_power_statecontrol as fhp_statecontrol
import fidl.fuchsia_hwinfo as f_hwinfo
import fidl.fuchsia_io as f_io
import fuchsia_controller_py as fcp
import fuchsia_inspect

from honeydew import affordances_capable, errors
from honeydew.affordances.connectivity.bluetooth.avrcp import (
    avrcp,
    avrcp_using_sl4f,
)
from honeydew.affordances.connectivity.bluetooth.gap import gap, gap_using_fc
from honeydew.affordances.connectivity.bluetooth.le import le, le_using_fc
from honeydew.affordances.connectivity.bluetooth.utils import (
    types as bluetooth_types,
)
from honeydew.affordances.connectivity.netstack import (
    netstack,
    netstack_using_fc,
)
from honeydew.affordances.connectivity.wlan.utils import types as wlan_types
from honeydew.affordances.connectivity.wlan.wlan import wlan, wlan_using_fc
from honeydew.affordances.connectivity.wlan.wlan_policy import (
    wlan_policy,
    wlan_policy_using_fc,
)
from honeydew.affordances.connectivity.wlan.wlan_policy_ap import (
    wlan_policy_ap,
    wlan_policy_ap_using_fc,
)
from honeydew.affordances.location import location, location_using_fc
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as system_power_state_controller_interface,
)
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller_using_starnix,
)
from honeydew.affordances.rtc import rtc, rtc_using_fc
from honeydew.affordances.session import session, session_using_ffx
from honeydew.affordances.tracing import tracing, tracing_using_fc
from honeydew.affordances.ui.screenshot import screenshot, screenshot_using_ffx
from honeydew.affordances.ui.user_input import user_input, user_input_using_fc
from honeydew.auxiliary_devices.power_switch import (
    power_switch as power_switch_interface,
)
from honeydew.fuchsia_device import fuchsia_device as fuchsia_device_interface
from honeydew.transports.fastboot import (
    fastboot as fastboot_transport_interface,
)
from honeydew.transports.fastboot import fastboot_impl
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_transport_interface
from honeydew.transports.ffx import ffx_impl
from honeydew.transports.ffx.config import FfxConfigData
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fuchsia_controller_transport_interface,
)
from honeydew.transports.fuchsia_controller import fuchsia_controller_impl
from honeydew.transports.serial import serial as serial_transport_interface
from honeydew.transports.serial import serial_using_unix_socket
from honeydew.transports.sl4f import sl4f as sl4f_transport_interface
from honeydew.transports.sl4f import sl4f_impl
from honeydew.typing import custom_types
from honeydew.utils import common, properties

_FC_PROXIES: dict[str, custom_types.FidlEndpoint] = {
    "BuildInfo": custom_types.FidlEndpoint(
        "/core/build-info", "fuchsia.buildinfo.Provider"
    ),
    "DeviceInfo": custom_types.FidlEndpoint(
        "/core/hwinfo", "fuchsia.hwinfo.Device"
    ),
    "Feedback": custom_types.FidlEndpoint(
        "/core/feedback", "fuchsia.feedback.DataProvider"
    ),
    "ProductInfo": custom_types.FidlEndpoint(
        "/core/hwinfo", "fuchsia.hwinfo.Product"
    ),
    "PowerAdmin": custom_types.FidlEndpoint(
        "/bootstrap/shutdown_shim", "fuchsia.hardware.power.statecontrol.Admin"
    ),
    "RemoteControl": custom_types.FidlEndpoint(
        "/core/remote-control", "fuchsia.developer.remotecontrol.RemoteControl"
    ),
}

_LOG_SEVERITIES: dict[custom_types.LEVEL, f_diagnostics.Severity] = {
    custom_types.LEVEL.INFO: f_diagnostics.Severity.INFO,
    custom_types.LEVEL.WARNING: f_diagnostics.Severity.WARN,
    custom_types.LEVEL.ERROR: f_diagnostics.Severity.ERROR,
}

_LOGGER: logging.Logger = logging.getLogger(__name__)


class FuchsiaDeviceImpl(
    fuchsia_device_interface.FuchsiaDevice,
    affordances_capable.RebootCapableDevice,
    affordances_capable.FuchsiaDeviceLogger,
    affordances_capable.FuchsiaDeviceClose,
    affordances_capable.InspectCapableDevice,
):
    """FuchsiaDevice abstract base class implementation using
    Fuchsia-Controller.

    Args:
        device_info: Fuchsia device information.
        ffx_config_data: Config that need to be used while running FFX commands.
        config: Honeydew device configuration, if any.
            Format:
                {
                    "transports": {
                        <transport_name>: {
                            <key>: <value>,
                            ...
                        },
                        ...
                    },
                    "affordances": {
                        <affordance_name>: {
                            <key>: <value>,
                            ...
                        },
                        ...
                    },
                }
            Example:
                {
                    "transports": {
                        "fuchsia_controller": {
                            "timeout": 30,
                        }
                    },
                    "affordances": {
                        "bluetooth": {
                            "implementation": "fuchsia-controller",
                        },
                        "wlan": {
                            "implementation": "sl4f",
                        }
                    },
                }

    Raises:
        FFXCommandError: if FFX connection check fails.
        FuchsiaControllerError: if FC connection check fails.
    """

    def __init__(
        self,
        device_info: custom_types.DeviceInfo,
        ffx_config_data: FfxConfigData,
        # intentionally made this a Dict instead of dataclass to minimize the changes in remaining Lacewing stack every time we need to add a new configuration item
        config: dict[str, Any] | None = None,
    ) -> None:
        _LOGGER.debug("Initializing FuchsiaDevice")

        self._device_info: custom_types.DeviceInfo = device_info

        self._ffx_config_data: FfxConfigData = ffx_config_data

        self._on_device_boot_fns: list[Callable[[], None]] = []
        self._on_device_close_fns: list[Callable[[], None]] = []

        self._config: dict[str, Any] | None = config

        self.health_check()

        _LOGGER.debug("Initialized FuchsiaDevice")

    # List all the persistent properties
    @properties.PersistentProperty
    def board(self) -> str:
        """Returns the board value of the device.

        Returns:
            board value of the device.

        Raises:
            FfxCommandError: On failure.
        """
        return self.ffx.get_target_board()

    @properties.PersistentProperty
    def device_name(self) -> str:
        """Returns the name of the device.

        Returns:
            Name of the device.
        """
        return self._device_info.name

    @properties.PersistentProperty
    def manufacturer(self) -> str:
        """Returns the manufacturer of the device.

        Returns:
            Manufacturer of device.

        Raises:
            FuchsiaDeviceError: On failure.
        """
        return self._product_info["manufacturer"]

    @properties.PersistentProperty
    def model(self) -> str:
        """Returns the model of the device.

        Returns:
            Model of device.

        Raises:
            FuchsiaDeviceError: On failure.
        """
        return self._product_info["model"]

    @properties.PersistentProperty
    def product(self) -> str:
        """Returns the product value of the device.

        Returns:
            product value of the device.

        Raises:
            FfxCommandError: On failure.
        """
        return self.ffx.get_target_product()

    @properties.PersistentProperty
    def product_name(self) -> str:
        """Returns the product name of the device.

        Returns:
            Product name of the device.

        Raises:
            FuchsiaDeviceError: On failure.
        """
        return self._product_info["name"]

    @properties.PersistentProperty
    def serial_number(self) -> str:
        """Returns the serial number of the device.

        Returns:
            Serial number of the device.
        """
        return self._device_info_from_fidl["serial_number"]

    # List all the dynamic properties
    @properties.DynamicProperty
    def firmware_version(self) -> str:
        """Returns the firmware version of the device.

        Returns:
            Firmware version of the device.
        """
        return self._build_info["version"]

    # List all transports
    @properties.Transport
    def ffx(self) -> ffx_transport_interface.FFX:
        """Returns the FFX transport object.

        Returns:
            FFX transport interface implementation.

        Raises:
            FfxCommandError: Failed to instantiate.
        """
        ffx_obj: ffx_transport_interface.FFX = ffx_impl.FfxImpl(
            target_name=self.device_name,
            config_data=self._ffx_config_data,
            target_ip_port=self._device_info.ip_port,
        )
        return ffx_obj

    @properties.Transport
    def fuchsia_controller(
        self,
    ) -> fuchsia_controller_transport_interface.FuchsiaController:
        """Returns the Fuchsia-Controller transport object.

        Returns:
            Fuchsia-Controller transport interface implementation.

        Raises:
            FuchsiaControllerError: Failed to instantiate.
        """
        fuchsia_controller_obj: (
            fuchsia_controller_transport_interface.FuchsiaController
        ) = fuchsia_controller_impl.FuchsiaControllerImpl(
            target_name=self.device_name,
            ffx_config_data=self._ffx_config_data,
            target_ip_port=self._device_info.ip_port,
        )
        return fuchsia_controller_obj

    @properties.Transport
    def fastboot(self) -> fastboot_transport_interface.Fastboot:
        """Returns the Fastboot transport object.

        Returns:
            Fastboot transport interface implementation.

        Raises:
            FuchsiaDeviceError: Failed to instantiate.
        """
        fastboot_obj: fastboot_transport_interface.Fastboot = (
            fastboot_impl.FastbootImpl(
                device_name=self.device_name,
                reboot_affordance=self,
                ffx_transport=self.ffx,
            )
        )
        return fastboot_obj

    @properties.Transport
    def serial(self) -> serial_transport_interface.Serial:
        """Returns the Serial transport object.

        Returns:
            Serial transport object.
        """
        if self._device_info.serial_socket is None:
            raise errors.FuchsiaDeviceError(
                "'serial_socket' arg need to be provided during the init to use Serial affordance"
            )

        serial_obj: serial_transport_interface.Serial = (
            serial_using_unix_socket.SerialUsingUnixSocket(
                device_name=self.device_name,
                socket_path=self._device_info.serial_socket,
            )
        )
        return serial_obj

    @properties.Transport
    def sl4f(self) -> sl4f_transport_interface.SL4F:
        """Returns the SL4F transport object.

        Returns:
            SL4F transport interface implementation.

        Raises:
            Sl4fError: Failed to instantiate.
        """
        device_ip: ipaddress.IPv4Address | ipaddress.IPv6Address | None = None
        if self._device_info.ip_port:
            device_ip = self._device_info.ip_port.ip

        sl4f_obj: sl4f_transport_interface.SL4F = sl4f_impl.Sl4fImpl(
            device_name=self.device_name,
            device_ip=device_ip,
            ffx_transport=self.ffx,
        )
        return sl4f_obj

    # List all the affordances
    @properties.Affordance
    def session(self) -> session.Session:
        """Returns a session affordance object.

        Returns:
            session.Session object
        """
        return session_using_ffx.SessionUsingFfx(
            device_name=self.device_name, ffx=self.ffx
        )

    @properties.Affordance
    def screenshot(self) -> screenshot.Screenshot:
        """Returns a screenshot affordance object.

        Returns:
            screenshot.Screenshot object
        """
        return screenshot_using_ffx.ScreenshotUsingFfx(self.ffx)

    @properties.Affordance
    def system_power_state_controller(
        self,
    ) -> system_power_state_controller_interface.SystemPowerStateController:
        """Returns a SystemPowerStateController affordance object.

        Returns:
            system_power_state_controller_interface.SystemPowerStateController object

        Raises:
            errors.NotSupportedError: If Fuchsia device does not support Starnix
        """
        return system_power_state_controller_using_starnix.SystemPowerStateControllerUsingStarnix(
            device_name=self.device_name,
            ffx=self.ffx,
            inspect=self,
            device_logger=self,
        )

    @properties.Affordance
    def rtc(self) -> rtc.Rtc:
        """Returns an Rtc affordance object.

        Returns:
            rtc.Rtc object
        """
        return rtc_using_fc.RtcUisngFc(
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def tracing(self) -> tracing.Tracing:
        """Returns a tracing affordance object.

        Returns:
            tracing.Tracing object
        """
        return tracing_using_fc.TracingUsingFc(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def user_input(self) -> user_input.UserInput:
        """Returns an user input affordance object.

        Returns:
            user_input.UserInput object
        """
        return user_input_using_fc.UserInputUsingFc(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            ffx_transport=self.ffx,
        )

    @properties.Affordance
    def bluetooth_avrcp(self) -> avrcp.Avrcp:
        """Returns a Bluetooth Avrcp affordance object.

        Returns:
            Bluetooth Avrcp object
        """
        if (
            self._get_bluetooth_affordances_implementation()
            == bluetooth_types.Implementation.SL4F
        ):
            return avrcp_using_sl4f.AvrcpUsingSl4f(
                device_name=self.device_name,
                sl4f=self.sl4f,
                reboot_affordance=self,
            )
        raise NotImplementedError

    @properties.Affordance
    def bluetooth_le(self) -> le.LE:
        """Returns a Bluetooth LE affordance object.

        Returns:
            Bluetooth LE object
        """
        return le_using_fc.LEUsingFc(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def bluetooth_gap(self) -> gap.Gap:
        """Returns a Bluetooth Gap affordance object.

        Returns:
            Bluetooth Gap object
        """
        return gap_using_fc.GapUsingFc(
            device_name=self.device_name,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def wlan_policy(self) -> wlan_policy.WlanPolicy:
        """Returns a wlan_policy affordance object.

        Returns:
            wlan_policy.WlanPolicy object
        """
        return wlan_policy_using_fc.WlanPolicy(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
        )

    @properties.Affordance
    def wlan_policy_ap(self) -> wlan_policy_ap.WlanPolicyAp:
        """Returns a wlan_policy_ap affordance object.

        Returns:
            wlan_policy_ap.WlanPolicyAp object
        """
        return wlan_policy_ap_using_fc.WlanPolicyAp(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
        )

    @properties.Affordance
    def wlan(self) -> wlan.Wlan:
        """Returns a wlan affordance object.

        Returns:
            wlan.Wlan object
        """
        return wlan_using_fc.Wlan(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
            fuchsia_device_close=self,
        )

    @properties.Affordance
    def netstack(self) -> netstack.Netstack:
        """Returns a netstack affordance object.

        Returns:
            netstack.Netstack object
        """
        return netstack_using_fc.NetstackUsingFc(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    @properties.Affordance
    def location(self) -> location.Location:
        """Returns a location affordance object.

        Returns:
            location.Location object
        """
        return location_using_fc.LocationUsingFc(
            device_name=self.device_name,
            ffx=self.ffx,
            fuchsia_controller=self.fuchsia_controller,
            reboot_affordance=self,
        )

    # List all the public methods
    def close(self) -> None:
        """Clean up method."""
        for on_device_close_fns in self._on_device_close_fns:
            _LOGGER.info("Calling %s", on_device_close_fns.__qualname__)
            on_device_close_fns()

    def health_check(self) -> None:
        """Ensure device is healthy.

        Raises:
            errors.HealthCheckError
        """
        try:
            with common.time_limit(
                timeout=60,
                exception_message=f"Timeout occurred during the health check of '{self._device_info.name}'",
            ):
                _LOGGER.info(
                    "Starting the health check on %s...",
                    self.device_name,
                )

                # Note - FFX need to be invoked first before FC as FC depends on the daemon that
                # will be created by FFX
                self.ffx.check_connection()

                self.fuchsia_controller.check_connection()

                if self._is_sl4f_needed:
                    self.sl4f.check_connection()

                _LOGGER.info(
                    "Completed the health check successfully on %s...",
                    self.device_name,
                )
        except (
            errors.HoneydewTimeoutError,
            errors.TransportConnectionError,
        ) as err:
            raise errors.HealthCheckError(
                f"health check failed on '{self._device_info.name}'"
            ) from err

    def get_inspect_data(
        self,
        selectors: list[str] | None = None,
        monikers: list[str] | None = None,
    ) -> fuchsia_inspect.InspectDataCollection:
        """Return the inspect data associated with the given selectors and
        monikers.

        Args:
            selectors: selectors to be queried.
            monikers: component monikers.

        Note: If both `selectors` and `monikers` lists are empty, inspect data
        for the whole system will be returned.

        Returns:
            Inspect data collection

        Raises:
            InspectError: Failed to return inspect data.
        """
        selectors_and_monikers: list[str] = []
        if selectors:
            selectors_and_monikers += selectors
        if monikers:
            for moniker in monikers:
                selectors_and_monikers.append(moniker.replace(":", r"\:"))

        cmd: list[str] = [
            "--machine",
            "json",
            "inspect",
            "show",
        ] + selectors_and_monikers

        try:
            message: str = (
                f"Collecting the inspect data from {self.device_name}"
            )
            if selectors:
                message = f"{message}, with selectors={selectors}"
            if monikers:
                message = f"{message}, with monikers={monikers}"
            _LOGGER.info(message)
            inspect_data_json_str: str = self.ffx.run(
                cmd=cmd,
                log_output=False,
            )
            _LOGGER.info(
                "Collected the inspect data from %s.", self.device_name
            )

            inspect_data_json_obj: list[dict[str, Any]] = json.loads(
                inspect_data_json_str
            )
            return fuchsia_inspect.InspectDataCollection.from_list(
                inspect_data_json_obj
            )
        except (
            ffx_errors.FfxCommandError,
            errors.DeviceNotConnectedError,
            fuchsia_inspect.InspectDataError,
        ) as err:
            raise errors.InspectError(
                f"Failed to collect the inspect data from {self.device_name}"
            ) from err

    def log_message_to_device(
        self, message: str, level: custom_types.LEVEL
    ) -> None:
        """Log message to fuchsia device at specified level.

        Args:
            message: Message that need to logged.
            level: Log message level.

        Raises:
            FuchsiaControllerError: On communications failure.
            Sl4fError: On communications failure.
        """
        timestamp: str = datetime.now().strftime("%Y-%m-%d-%I-%M-%S-%p")
        message = f"[Host Time: {timestamp}] - {message}"
        self._send_log_command(tag="lacewing", message=message, level=level)

    def on_device_boot(self) -> None:
        """Take actions after the device is rebooted.

        Raises:
            FuchsiaControllerError: On communications failure.
            Sl4fError: On communications failure.
        """
        # Restart the SL4F server on device boot up.
        if self._is_sl4f_needed:
            common.retry(fn=self.sl4f.start_server, wait_time=5)

        # Create a new Fuchsia controller context for new device connection.
        self.fuchsia_controller.create_context()

        # Ensure device is healthy
        self.health_check()

        for on_device_boot_fn in self._on_device_boot_fns:
            _LOGGER.info("Calling %s", on_device_boot_fn.__qualname__)
            on_device_boot_fn()

    def power_cycle(
        self,
        power_switch: power_switch_interface.PowerSwitch,
        outlet: int | None = None,
    ) -> None:
        """Power cycle (power off, wait for delay, power on) the device.

        Args:
            power_switch: Implementation of PowerSwitch interface.
            outlet (int): If required by power switch hardware, outlet on
                power switch hardware where this fuchsia device is connected.

        Raises:
            FuchsiaControllerError: On communications failure.
            Sl4fError: On communications failure.
        """
        _LOGGER.info("Power cycling %s...", self.device_name)

        try:
            self.log_message_to_device(
                message=f"Powering cycling {self.device_name}...",
                level=custom_types.LEVEL.INFO,
            )
        except Exception:  # pylint: disable=broad-except
            # power_cycle can be used as a recovery mechanism when device is
            # unhealthy. So any calls to device prior to power_cycle can
            # fail in such cases and thus ignore them.
            pass

        _LOGGER.info("Powering off %s...", self.device_name)
        power_switch.power_off(outlet)
        self.wait_for_offline()

        _LOGGER.info("Powering on %s...", self.device_name)
        power_switch.power_on(outlet)
        self.wait_for_online()

        self.on_device_boot()

        self.log_message_to_device(
            message=f"Successfully power cycled {self.device_name}...",
            level=custom_types.LEVEL.INFO,
        )

    def reboot(self) -> None:
        """Soft reboot the device.

        Raises:
            FuchsiaControllerError: On communications failure.
            Sl4fError: On communications failure.
        """
        _LOGGER.info("Lacewing is rebooting %s...", self.device_name)
        self.log_message_to_device(
            message=f"Rebooting {self.device_name}...",
            level=custom_types.LEVEL.INFO,
        )

        self._send_reboot_command()

        self.wait_for_offline()
        self.wait_for_online()
        self.on_device_boot()

        self.log_message_to_device(
            message=f"Successfully rebooted {self.device_name}...",
            level=custom_types.LEVEL.INFO,
        )

    def register_for_on_device_boot(self, fn: Callable[[], None]) -> None:
        """Register a function that will be called in `on_device_boot()`.

        Args:
            fn: Function that need to be called after FuchsiaDevice boot up.
        """
        self._on_device_boot_fns.append(fn)

    def register_for_on_device_close(self, fn: Callable[[], None]) -> None:
        """Register a function that will be called during device clean up in `close()`.

        Args:
            fn: Function that need to be called during FuchsiaDevice cleanup.
        """
        self._on_device_close_fns.append(fn)

    def snapshot(self, directory: str, snapshot_file: str | None = None) -> str:
        """Captures the snapshot of the device.

        Args:
            directory: Absolute path on the host where snapshot file will be
                saved. If this directory does not exist, this method will create
                it.

            snapshot_file: Name of the output snapshot file.
                If not provided, API will create a name using
                "Snapshot_{device_name}_{'%Y-%m-%d-%I-%M-%S-%p'}" format.

        Returns:
            Absolute path of the snapshot file.

        Raises:
            FuchsiaControllerError: On communications failure.
            Sl4fError: On communications failure.
        """
        _LOGGER.info("Collecting snapshot on %s...", self.device_name)
        # Take the snapshot before creating the directory or file, as
        # _send_snapshot_command may raise an exception.
        snapshot_bytes: bytes = self._send_snapshot_command()

        directory = os.path.abspath(directory)
        try:
            os.makedirs(directory)
        except FileExistsError:
            pass

        if not snapshot_file:
            timestamp: str = datetime.now().strftime("%Y-%m-%d-%I-%M-%S-%p")
            snapshot_file = f"Snapshot_{self.device_name}_{timestamp}.zip"
        snapshot_file_path: str = os.path.join(directory, snapshot_file)

        with open(snapshot_file_path, "wb") as snapshot_binary_zip:
            snapshot_binary_zip.write(snapshot_bytes)

        _LOGGER.info("Snapshot file has been saved @ '%s'", snapshot_file_path)
        return snapshot_file_path

    def wait_for_offline(self) -> None:
        """Wait for Fuchsia device to go offline.

        Raises:
            errors.FuchsiaDeviceError: If device is not offline.
        """
        _LOGGER.info("Waiting for %s to go offline...", self.device_name)
        try:
            self.ffx.wait_for_rcs_disconnection()
            _LOGGER.info("%s is offline.", self.device_name)
        except (
            errors.DeviceNotConnectedError,
            ffx_errors.FfxCommandError,
        ) as err:
            raise errors.FuchsiaDeviceError(
                f"'{self.device_name}' failed to go offline."
            ) from err

    def wait_for_online(self) -> None:
        """Wait for Fuchsia device to go online.

        Raises:
            errors.FuchsiaDeviceError: If device is not online.
        """
        _LOGGER.info("Waiting for %s to go online...", self.device_name)
        try:
            self.ffx.wait_for_rcs_connection()
            _LOGGER.info("%s is online.", self.device_name)
        except (
            errors.DeviceNotConnectedError,
            ffx_errors.FfxCommandError,
        ) as err:
            raise errors.FuchsiaDeviceError(
                f"'{self.device_name}' failed to go online."
            ) from err

    # List all private properties
    @property
    def _build_info(self) -> dict[str, Any]:
        """Returns the build information of the device.

        Returns:
            Build info dict.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            buildinfo_provider_proxy = f_buildinfo.ProviderClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["BuildInfo"]
                )
            )
            build_info_resp = asyncio.run(
                buildinfo_provider_proxy.get_build_info()
            )
            return build_info_resp.build_info
        except fcp.ZxStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    @property
    def _device_info_from_fidl(self) -> dict[str, Any]:
        """Returns the device information of the device.

        Returns:
            Device info dict.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            hwinfo_device_proxy = f_hwinfo.DeviceClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["DeviceInfo"]
                )
            )
            device_info_resp = asyncio.run(hwinfo_device_proxy.get_info())
            return device_info_resp.info
        except fcp.ZxStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    @property
    def _product_info(self) -> dict[str, Any]:
        """Returns the product information of the device.

        Returns:
            Product info dict.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            hwinfo_product_proxy = f_hwinfo.ProductClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["ProductInfo"]
                )
            )
            product_info_resp = asyncio.run(hwinfo_product_proxy.get_info())
            return product_info_resp.info
        except fcp.ZxStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    # List all private methods
    def _send_log_command(
        self, tag: str, message: str, level: custom_types.LEVEL
    ) -> None:
        """Send a device command to write to the syslog.

        Args:
            tag: Tag to apply to the message in the syslog.
            message: Message that need to logged.
            level: Log message level.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            rcs_proxy = fd_remotecontrol.RemoteControlClient(
                self.fuchsia_controller.ctx.connect_remote_control_proxy()
            )
            asyncio.run(
                rcs_proxy.log_message(
                    tag=tag, message=message, severity=_LOG_SEVERITIES[level]
                )
            )
        except fcp.ZxStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "Fuchsia Controller FIDL Error"
            ) from status

    def _send_reboot_command(self) -> None:
        """Send a device command to trigger a soft reboot.

        Raises:
            FuchsiaControllerError: On FIDL communication failure.
        """
        try:
            power_proxy = fhp_statecontrol.AdminClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["PowerAdmin"]
                )
            )
            asyncio.run(
                power_proxy.reboot(
                    reason=fhp_statecontrol.RebootReason.USER_REQUEST
                )
            )
        except fcp.ZxStatus as status:
            # ZX_ERR_PEER_CLOSED is expected in this instance because the device
            # powered off.
            zx_status: int | None = (
                status.args[0] if len(status.args) > 0 else None
            )
            if zx_status != fcp.ZxStatus.ZX_ERR_PEER_CLOSED:
                raise fc_errors.FuchsiaControllerError(
                    "Fuchsia Controller FIDL Error"
                ) from status

    def _read_snapshot_from_channel(self, channel_client: fcp.Channel) -> bytes:
        """Read snapshot data from client end of the transfer channel.

        Args:
            channel_client: Client end of the snapshot data channel.

        Raises:
            FuchsiaControllerError: On FIDL communication failure or on
              data transfer verification failure.

        Returns:
            Bytes containing snapshot data as a zip archive.
        """
        # Snapshot is sent over the channel as |fuchsia.io.File|.
        file_proxy = f_io.FileClient(channel_client)

        # Get file size for verification later.
        try:
            attr_resp: f_io.NodeGetAttrResponse = asyncio.run(
                file_proxy.get_attr()
            )
            if attr_resp.s != fcp.ZxStatus.ZX_OK:
                raise fc_errors.FuchsiaControllerError(
                    f"get_attr() returned status: {attr_resp.s}"
                )
        except fcp.ZxStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "get_attr() failed"
            ) from status

        # Read until channel is empty.
        ret: bytearray = bytearray()
        try:
            while True:
                result: f_io.ReadableReadResult = asyncio.run(
                    file_proxy.read(count=f_io.MAX_BUF)
                )
                if result.err:
                    raise fc_errors.FuchsiaControllerError(
                        "read() failed. Received zx.Status {result.err}"
                    )
                if not result.response.data:
                    break
                ret.extend(result.response.data)
        except fcp.ZxStatus as status:
            raise fc_errors.FuchsiaControllerError("read() failed") from status

        # Verify transfer.
        expected_size: int = attr_resp.attributes.content_size
        if len(ret) != expected_size:
            raise fc_errors.FuchsiaControllerError(
                f"Expected {expected_size} bytes, but read {len(ret)} bytes"
            )

        return bytes(ret)

    def _send_snapshot_command(self) -> bytes:
        """Send a device command to take a snapshot.

        Raises:
            FuchsiaControllerError: On FIDL communication failure or on
              data transfer verification failure.

        Returns:
            Bytes containing snapshot data as a zip archive.
        """
        # Ensure device is healthy and ready to send FIDL requests before
        # sending snapshot command.
        self.fuchsia_controller.create_context()
        self.health_check()

        channel_server, channel_client = fcp.Channel.create()
        params = f_feedback.GetSnapshotParameters(
            # Set timeout to 2 minutes in nanoseconds.
            collection_timeout_per_data=2 * 60 * 10**9,
            response_channel=channel_server.take(),
        )

        try:
            feedback_proxy = f_feedback.DataProviderClient(
                self.fuchsia_controller.connect_device_proxy(
                    _FC_PROXIES["Feedback"]
                )
            )
            # The data channel isn't populated until get_snapshot() returns so
            # there's no need to drain the channel in parallel.
            asyncio.run(feedback_proxy.get_snapshot(params=params))
        except fcp.ZxStatus as status:
            raise fc_errors.FuchsiaControllerError(
                "get_snapshot() failed"
            ) from status
        return self._read_snapshot_from_channel(channel_client)

    def _get_bluetooth_affordances_implementation(
        self,
        should_exist: bool = True,
    ) -> bluetooth_types.Implementation | None:
        """Parses the bluetooth affordance config information and returns which bluetooth
        implementation to use.

        Returns:
            bluetooth_types.Implementation

        Raises:
            errors.ConfigError: If bluetooth affordance implementation detail is missing or not valid.
        """
        if self._config is None:
            return None

        bluetooth_affordance_implementation: str | None = common.read_from_dict(
            self._config,
            key_path=("affordances", "bluetooth", "implementation"),
            should_exist=should_exist,
        )
        if bluetooth_affordance_implementation is None:
            return None

        try:
            return bluetooth_types.Implementation(
                bluetooth_affordance_implementation
            )
        except ValueError as err:
            raise errors.ConfigError(
                f"Invalid value passed in config['affordances']['bluetooth']['implementation]. "
                f"Valid values are: {list(map(str, bluetooth_types.Implementation))}"
            ) from err

    def _get_wlan_affordances_implementation(
        self,
        should_exist: bool = True,
    ) -> wlan_types.Implementation | None:
        """Parses the WLAN affordance config information and returns which WLAN implementation to use.

        Returns:
            wlan_types.Implementation

        Raises:
            ValueError: If wlan affordance implementation detail is missing or not valid.
        """
        if self._config is None:
            return None

        wlan_affordance_implementation: str | None = common.read_from_dict(
            self._config,
            key_path=("affordances", "wlan", "implementation"),
            should_exist=should_exist,
        )
        if wlan_affordance_implementation is None:
            return None

        try:
            return wlan_types.Implementation(wlan_affordance_implementation)
        except ValueError as err:
            raise ValueError(
                f"Invalid value passed in config['affordances']['wlan']['implementation]. "
                f"Valid values are: {list(map(str, wlan_types.Implementation))}"
            ) from err

    @cached_property
    def _is_sl4f_needed(self) -> bool:
        """Returns whether or not SL4F will be used.

        Returns:
            True if SL4F is needed. False, otherwise.
        """
        if (
            self._get_wlan_affordances_implementation(should_exist=False)
            == wlan_types.Implementation.SL4F
            or self._get_bluetooth_affordances_implementation(
                should_exist=False
            )
            == bluetooth_types.Implementation.SL4F
        ):
            return True
        return False
