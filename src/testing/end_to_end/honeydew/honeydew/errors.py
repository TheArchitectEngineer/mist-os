# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Contains errors raised by Honeydew."""

import logging

_LOGGER: logging.Logger = logging.getLogger(__name__)


class HoneydewError(Exception):
    """Base exception for Honeydew module.

    More specific exceptions will be created by inheriting from this exception.
    """

    def __init__(self, msg: str | Exception) -> None:
        """Inits HoneydewError with 'msg' (an error message string).

        Args:
            msg: an error message string or an Exception instance.

        Note: Additionally, logs 'msg' to debug log level file.
        """
        super().__init__(msg)
        _LOGGER.debug(repr(self), exc_info=True)


class ConfigError(HoneydewError):
    """Exception for reporting invalid config passed to Honeydew."""


class HostCmdError(HoneydewError):
    """Exception for reporting host command failures."""


class TransportError(HoneydewError):
    """Exception for errors raised by Honeydew transports."""


class HttpRequestError(TransportError):
    """Exception for errors raised by HTTP requests running on host machine."""


class HttpTimeoutError(HttpRequestError, TimeoutError):
    """Exception for errors raised by HTTP requests timing out on host machine."""


class HealthCheckError(HoneydewError):
    """Raised when health_check fails."""


class TransportConnectionError(HoneydewError):
    """Raised when transport's check_connection fails."""


class HoneydewTimeoutError(HoneydewError):
    """Exception for timeout based raised by Honeydew."""


class HoneydewDataResourceError(HoneydewError):
    """Raised when Honeydew fails to fetch its data resources."""


class FuchsiaStateError(HoneydewError):
    """Exception for state errors."""


class FuchsiaDeviceError(HoneydewError):
    """Base exception for errors raised by fuchsia device."""


class DeviceNotConnectedError(HoneydewError):
    """Exception to be raised when device is not connected to host."""


class NotSupportedError(HoneydewError):
    """Exception to be raised if an operation is not yet supported by
    underlying Fuchsia platform."""


class StarnixError(HoneydewError):
    """Exception to be raised if a starnix operation fails."""


class InspectError(HoneydewError):
    """Exception to be raised for Inspect affordance related failures."""
