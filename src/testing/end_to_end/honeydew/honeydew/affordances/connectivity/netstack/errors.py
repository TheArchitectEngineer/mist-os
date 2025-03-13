# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Contains errors raised by session affordance."""

from honeydew import errors


class HoneydewNetstackError(errors.HoneydewError):
    """Raised by netstack affordances."""
