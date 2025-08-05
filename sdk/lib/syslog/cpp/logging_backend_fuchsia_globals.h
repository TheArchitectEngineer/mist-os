// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYSLOG_CPP_LOGGING_BACKEND_FUCHSIA_GLOBALS_H_
#define LIB_SYSLOG_CPP_LOGGING_BACKEND_FUCHSIA_GLOBALS_H_

#include <zircon/types.h>

namespace fuchsia_logging::internal {
class LogState;

// These functions are an internal contract between the Fuchsia logging
// backend and the logging state shared library. API users should
// not call these directly, but they need to be exported to allow
// for global state management of logs within a single process.

extern "C" {

// Acquires the state lock.
void FuchsiaLogAcquireState();

// Updates the log state, requires that the state lock is held.
void FuchsiaLogSetStateLocked(LogState* new_state);

// Releases the state lock.
void FuchsiaLogReleaseState();

// Returns the current log state.
LogState* FuchsiaLogGetStateLocked();

// Returns the current thread's koid.
zx_koid_t FuchsiaLogGetCurrentThreadKoid();

}  // extern "C"
}  // namespace fuchsia_logging::internal

#endif  // LIB_SYSLOG_CPP_LOGGING_BACKEND_FUCHSIA_GLOBALS_H_
