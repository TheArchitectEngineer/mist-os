// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_GRACEFUL_REBOOT_REASON_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_GRACEFUL_REBOOT_REASON_H_

#include <fuchsia/hardware/power/statecontrol/cpp/fidl.h>

#include <string>

#include "src/developer/forensics/utils/cobalt/logger.h"

namespace forensics {
namespace feedback {

// Feedback's internal representation of why a device rebooted gracefully.
//
// These values should not be used to understand why a device has rebooted outside of this
// component.
enum class GracefulRebootReason {
  kNotSet,
  kUserRequest,
  kSystemUpdate,
  kRetrySystemUpdate,
  kHighTemperature,
  kSessionFailure,
  kSysmgrFailure,
  kCriticalComponentFailure,
  kFdr,
  kZbiSwap,
  kOutOfMemory,
  // TODO(https://fxbug.dev/42081574): Remove this reason once Netstack2 is
  // fully migrated to Netstack3.
  kNetstackMigration,
  kAndroidUnexpectedReason,
  kNotSupported,
  kNotParseable,
};

std::string ToString(GracefulRebootReason reason);

std::vector<GracefulRebootReason> ToGracefulRebootReasons(
    fuchsia::hardware::power::statecontrol::RebootOptions options);

// The input is limited to values corresponding to |power::statecontrol::RebootReason|.
std::vector<GracefulRebootReason> FromFileContent(std::string content);

// The input is limited to values corresponding to |power::statecontrol::RebootReason|.
std::string ToFileContent(const std::vector<GracefulRebootReason>& reasons);

std::string ToLog(const std::vector<GracefulRebootReason>& reasons);

// Writes the graceful reboot reason to `path` and records metrics about the write.
void WriteGracefulRebootReasons(const std::vector<GracefulRebootReason>& reasons,
                                cobalt::Logger* cobalt, const std::string& path);

}  // namespace feedback
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_GRACEFUL_REBOOT_REASON_H_
