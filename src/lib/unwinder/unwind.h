// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_UNWINDER_UNWIND_H_
#define SRC_LIB_UNWINDER_UNWIND_H_

#include <cstdint>
#include <utility>
#include <vector>

#include "sdk/lib/fit/include/lib/fit/function.h"
#include "src/lib/unwinder/cfi_unwinder.h"
#include "src/lib/unwinder/frame.h"
#include "src/lib/unwinder/memory.h"
#include "src/lib/unwinder/module.h"
#include "src/lib/unwinder/registers.h"
#include "src/lib/unwinder/unwinder_base.h"

namespace unwinder {

// The main unwinder. It caches the unwind tables so repeated unwinding is faster.
//
// The API is designed to be flexible so that it can support both online unwinding from a process'
// memory, and offline unwinding from stack snapshots with ELF files on disk.
class Unwinder {
 public:
  // Initialize the unwinder from a vector of modules.
  //
  // Each module can supply its own data accessor and address mode.
  explicit Unwinder(const std::vector<Module>& modules);

  // Unwind from a stack and a set of registers up to given |max_depth|.
  //
  // |stack| could be null, in which case it will become an |UnavailableMemory|.
  std::vector<Frame> Unwind(Memory* stack, const Registers& registers, size_t max_depth = 50);

 private:
  // Unwind one frame.
  //
  // |current.regs| will be used. |current.error| and |current.fatal_error| will be populated.
  // If |current.fatal_error| is false, |next.regs| and |next.trust| will be populated.
  void Step(Memory* stack, Frame& current, Frame& next);

  CfiUnwinder cfi_unwinder_;
};

class AsyncUnwinder {
 public:
  explicit AsyncUnwinder(const std::vector<Module>& modules);

  void Unwind(AsyncMemory::Delegate* delegate, const Registers& registers, size_t max_depth,
              fit::callback<void(std::vector<Frame>)> cb);

 private:
  void Step(Frame& current);
  void OnStep(Frame next);

  fit::callback<void(std::vector<Frame>)> on_done_;
  size_t max_depth_;
  std::vector<Frame> result_;
  std::unique_ptr<AsyncMemory> stack_;
  CfiUnwinder cfi_unwinder_;
};

// Unwind with given memory, modules, and registers.
//
// This provides an simplified API than the above Unwinder class but comes without a cache.
// The modules are provided as base addresses and are accessed through the memory.
std::vector<Frame> Unwind(Memory* memory, const std::vector<uint64_t>& modules,
                          const Registers& registers, size_t max_depth = 50);

}  // namespace unwinder

#endif  // SRC_LIB_UNWINDER_UNWIND_H_
