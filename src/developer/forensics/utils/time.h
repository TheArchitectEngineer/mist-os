// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_UTILS_TIME_H_
#define SRC_DEVELOPER_FORENSICS_UTILS_TIME_H_

#include <lib/zx/time.h>

#include <optional>
#include <string>
#include <string_view>

#include "src/lib/timekeeper/clock.h"

namespace forensics {

// Formats the provided duration as WdXhYmZs e.g., 1d14h7m32s
std::optional<std::string> FormatDuration(zx::duration duration);

// Formats the provided seconds since unix epoch as a date like, YYYY-MM-DDTHH:MM:SS+00:00
std::optional<std::string> FormatSecondsSinceEpoch(std::string_view seconds);

// Returns the non-localized current time according to |clock|.
timekeeper::time_utc CurrentUtcTimeRaw(timekeeper::Clock* clock);

// Returns a non-localized human-readable timestamp of the current time according to |clock|.
std::string CurrentUtcTime(timekeeper::Clock* clock);

}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_UTILS_TIME_H_
