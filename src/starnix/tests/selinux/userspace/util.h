// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STARNIX_TESTS_SELINUX_USERSPACE_UTIL_H_
#define SRC_STARNIX_TESTS_SELINUX_USERSPACE_UTIL_H_

#include <string.h>

#include <string>

#include <gmock/gmock.h>

// Loads the policy |name|.
void LoadPolicy(const std::string& name);

// Atomically writes |contents| to |file|, and fails the test otherwise.
void WriteContents(const std::string& file, const std::string& contents, bool create = false);

// Reads |file|, or fail the test.
std::string ReadFile(const std::string& file);

MATCHER(SyscallSucceeds, "syscall succeeds") {
  if (arg != -1) {
    return true;
  }
  *result_listener << "syscall failed with error " << strerror(errno);
  return false;
}

MATCHER_P(SyscallFailsWithErrno, expected_errno,
          std::string("syscall fails with error ") + strerror(expected_errno)) {
  if (arg != -1) {
    *result_listener << "syscall succeeded";
    return false;
  } else if (errno == expected_errno) {
    return true;
  } else {
    *result_listener << "syscall failed with error " << strerror(errno);
    return false;
  }
}

MATCHER_P(FdIsLabeled, expected_label, std::string("fd is labeled with ") + expected_label) {
  if (arg < 0) {
    *result_listener << "invalid fd";
    return false;
  }
  char label[256] = {};
  ssize_t len = fgetxattr(arg, "security.selinux", label, sizeof(label));
  if (len < 0) {
    *result_listener << "fgetxattr failed with error: " << strerror(errno);
    return false;
  }
  // TODO: https://fxbug.dev/395625171 - This ignores the final '\0' added by Linux.
  return ExplainMatchResult(testing::Eq(expected_label),
                            std::string(std::string(label, len).c_str()), result_listener);
}

#endif  // SRC_STARNIX_TESTS_SELINUX_USERSPACE_UTIL_H_
