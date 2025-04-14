# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""UserInput affordance implementation using FuchsiaController."""

import asyncio
import time

import fidl_fuchsia_math as f_math
import fidl_fuchsia_ui_test_input as f_test_input
import fuchsia_controller_py as fcp

from honeydew import errors
from honeydew.affordances.ui.user_input import errors as user_input_errors
from honeydew.affordances.ui.user_input import types as ui_custom_types
from honeydew.affordances.ui.user_input import user_input
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing import custom_types

_INPUT_HELPER_COMPONENT: str = "core/ui/input-helper"


class _FcProxies:
    INPUT_REGISTRY: custom_types.FidlEndpoint = custom_types.FidlEndpoint(
        "/core/ui", "fuchsia.ui.test.input.Registry"
    )


class TouchDevice(user_input.TouchDevice):
    """Virtual TouchDevice for testing using FuchsiaController.

    Args:
        device_name: name of testing device.
        fuchsia_controller: FuchsiaController transport.

    Raises:
        UserInputError: if failed to create virtual touch device.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
    ) -> None:
        self._device_name = device_name
        channel_server, channel_client = fcp.Channel.create()

        try:
            input_registry_proxy = f_test_input.RegistryClient(
                fuchsia_controller.connect_device_proxy(
                    _FcProxies.INPUT_REGISTRY
                )
            )
            asyncio.run(
                input_registry_proxy.register_touch_screen(
                    device=channel_server.take(),
                )
            )
        except fcp.ZxStatus as status:
            raise user_input_errors.UserInputError(
                f"Failed to initialize touch device on {self._device_name}"
            ) from status

        self._touch_screen_proxy = f_test_input.TouchScreenClient(
            channel_client
        )

    def tap(
        self,
        location: ui_custom_types.Coordinate,
        tap_event_count: int = user_input.DEFAULTS["TAP_EVENT_COUNT"],
        duration_ms: int = user_input.DEFAULTS["TAP_DURATION_MS"],
    ) -> None:
        """Instantiates Taps at coordinates (x, y) for a touchscreen with
           default or custom width, height, duration, and tap event counts.

        Args:
            location: tap location in X, Y axis coordinate.

            tap_event_count: Number of tap events to send (`duration` is
                divided over the tap events), defaults to 1.

            duration_ms: Duration of the event(s) in milliseconds, defaults to
                300.

        Raises:
            UserInputError: if failed tap operation.
        """

        try:
            interval: float = duration_ms / tap_event_count

            for _ in range(tap_event_count):
                asyncio.run(
                    self._touch_screen_proxy.simulate_tap(
                        tap_location=f_math.Vec(x=location.x, y=location.y)
                    )
                )
                time.sleep(interval / 1000)  # Sleep in seconds

        except fcp.ZxStatus as status:
            raise user_input_errors.UserInputError(
                f"tap operation failed on {self._device_name}"
            ) from status

    def swipe(
        self,
        start_location: ui_custom_types.Coordinate,
        end_location: ui_custom_types.Coordinate,
        move_event_count: int,
        duration_ms: int = user_input.DEFAULTS["SWIPE_DURATION_MS"],
    ) -> None:
        """Instantiates a swipe event sequence that starts at `start_location` and ends at
           `end_location`, with a total number of move events equal to `move_event_count`.

           Events are injected with no explicit delay in between.

        Args:
            start_location: swipe start location in X, Y axis coordinate.

            end_location: swipe end location in X, Y axis coordinate.

            move_event_count: Number of move events.

            duration_ms: Duration of the swipe gesture in milliseconds, defaults to 0.

        Raises:
            UserInputError: if failed swipe operation.
        """

        try:
            asyncio.run(
                self._touch_screen_proxy.simulate_swipe(
                    start_location=f_math.Vec(
                        x=start_location.x, y=start_location.y
                    ),
                    end_location=f_math.Vec(x=end_location.x, y=end_location.y),
                    move_event_count=move_event_count,
                    duration=duration_ms
                    * 1000000,  # milliseconds to nanoseconds
                )
            )
        except fcp.ZxStatus as status:
            raise user_input_errors.UserInputError(
                f"swipe operation failed on {self._device_name}"
            ) from status


class KeyboardDevice(user_input.KeyboardDevice):
    """Virtual KeyboardDevice for testing using FuchsiaController.

    Args:
        device_name: name of testing device.
        fuchsia_controller: FuchsiaController transport.

    Raises:
        UserInputError: if failed to create virtual keyboard device.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
    ) -> None:
        self._device_name = device_name
        channel_server, channel_client = fcp.Channel.create()

        try:
            input_registry_proxy = f_test_input.RegistryClient(
                fuchsia_controller.connect_device_proxy(
                    _FcProxies.INPUT_REGISTRY
                )
            )
            asyncio.run(
                input_registry_proxy.register_keyboard(
                    device=channel_server.take(),
                )
            )
        except fcp.ZxStatus as status:
            raise user_input_errors.UserInputError(
                f"Failed to initialize keyboard device on {self._device_name}"
            ) from status

        self._keyboard_proxy = f_test_input.KeyboardClient(channel_client)

    def key_press(
        self,
        key_code: int,
    ) -> None:
        """Instantiates key press includes down and up.

        Args:
            key_code: key code you can find in fuchsia.input.Key

        Raises:
            UserInputError: if failed key press operation.
        """
        try:
            asyncio.run(
                self._keyboard_proxy.simulate_key_press(key_code=key_code)
            )
        except fcp.ZxStatus as status:
            raise user_input_errors.UserInputError(
                f"key press operation failed on {self._device_name}"
            ) from status


class UserInputUsingFc(user_input.UserInput):
    """UserInput affordance implementation using FuchsiaController.

    Args:
        device_name: name of testing device.
        fuchsia_controller: FuchsiaController transport.

    Raises:
        NotSupportedError: if device does not support virtual input device.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        ffx_transport: ffx.FFX,
    ) -> None:
        self._device_name = device_name
        self._fc_transport: fc_transport.FuchsiaController = fuchsia_controller

        # check if the device have component to support virtual devices.
        components = ffx_transport.run(["component", "list"])
        if _INPUT_HELPER_COMPONENT not in components.splitlines():
            raise errors.NotSupportedError(
                f"{_INPUT_HELPER_COMPONENT} is not available in device {device_name}"
            )

    def create_touch_device(
        self,
        touch_screen_size: ui_custom_types.Size = user_input.DEFAULTS[
            "TOUCH_SCREEN_SIZE"
        ],
    ) -> user_input.TouchDevice:
        """Create a virtual touch device for testing touch input.

        Args:
            touch_screen_size: ignore.

        Raises:
            UserInputError: if failed to create virtual touch device.
        """
        return TouchDevice(
            device_name=self._device_name, fuchsia_controller=self._fc_transport
        )

    def create_keyboard_device(self) -> user_input.KeyboardDevice:
        return KeyboardDevice(
            device_name=self._device_name, fuchsia_controller=self._fc_transport
        )
