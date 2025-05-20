// Copyright 2024 Mist Tecnologia LTDA. All rights reserved.
// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef VENDOR_MISTTECH_ZIRCON_KERNEL_LIB_STARNIX_KERNEL_INCLUDE_LIB_MISTOS_STARNIX_TESTING_TESTING_H_
#define VENDOR_MISTTECH_ZIRCON_KERNEL_LIB_STARNIX_KERNEL_INCLUDE_LIB_MISTOS_STARNIX_TESTING_TESTING_H_

#include <lib/mistos/starnix/kernel/task/current_task.h>
#include <lib/mistos/starnix/kernel/vfs/anon_node.h>
#include <lib/mistos/starnix/kernel/vfs/file_ops.h>
#include <lib/mistos/starnix/kernel/vfs/file_system_ops.h>
#include <lib/mistos/starnix/kernel/vfs/fs_node_ops.h>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <ktl/optional.h>
#include <ktl/utility.h>
#include <ktl/string_view.h>

namespace starnix {

class Kernel;

namespace testing {

// An FsNodeOps implementation that panics if you try to open it. Useful as a stand-in for testing
// APIs that require a FsNodeOps implementation but don't actually use it.
class PanickingFsNode : public FsNodeOps {
 public:
  PanickingFsNode() = default;

  fs_node_impl_not_dir();

  fit::result<Errno, ktl::unique_ptr<FileOps>> create_file_ops(const FsNode& node,
                                                               const CurrentTask& current_task,
                                                               OpenFlags flags) const override {
    panic("should not be called");
  }
};

// An implementation of FileOps that panics on any read, write, or ioctl operation.
class PanickingFile : public FileOps {
 public:
  PanickingFile() = default;

  // Creates a FileObject whose implementation panics on reads, writes, and ioctls.
  static fbl::RefPtr<FileObject> new_file(const CurrentTask& current_task) {
    fbl::AllocChecker ac;
    auto file = ktl::make_unique<PanickingFile>(&ac);
    ASSERT(ac.check());
    return Anon::new_file(current_task, ktl::move(file), OpenFlags(OpenFlagsEnum::RDWR));
  }

  // impl FileOps for PanickingFile
  fileops_impl_nonseekable();
  fileops_impl_noop_sync();

  fit::result<Errno, size_t> write(const FileObject& file, const CurrentTask& current_task,
                                   size_t offset, InputBuffer* data) const final {
    panic("write called on PanickingFile");
  }

  fit::result<Errno, size_t> read(const FileObject& file, const CurrentTask& current_task,
                                  size_t offset, OutputBuffer* data) const final {
    panic("read called on PanickingFile");
  }

  fit::result<Errno, SyscallResult> ioctl(const FileObject& file, const CurrentTask& current_task,
                                          uint32_t request, SyscallArg arg) const final {
    panic("ioctl called on PanickingFile");
  }
};

class AutoReleasableTask {
 public:
  static AutoReleasableTask From(starnix::TaskBuilder builder) {
    return AutoReleasableTask::From(starnix::CurrentTask::From(ktl::move(builder)));
  }

  static AutoReleasableTask From(starnix::CurrentTask task) {
    return AutoReleasableTask({ktl::move(task)});
  }

  starnix::CurrentTask& operator*() {
    ASSERT_MSG(task_.has_value(),
               "called `operator*` on ktl::optional that does not contain a value.");
    return task_.value();
  }

  const starnix::CurrentTask& operator*() const {
    ASSERT_MSG(task_.has_value(),
               "called `operator*` on ktl::optional that does not contain a value.");
    return task_.value();
  }

  starnix::CurrentTask* operator->() {
    ASSERT_MSG(task_.has_value(),
               "called `operator->` on ktl::optional that does not contain a value.");
    return &task_.value();
  }

  const starnix::CurrentTask* operator->() const {
    ASSERT_MSG(task_.has_value(),
               "called `operator->` on ktl::optional that does not contain a value.");
    return &task_.value();
  }

  ~AutoReleasableTask() { task_->release(); }

  AutoReleasableTask(AutoReleasableTask&& other) noexcept {
    task_ = ktl::move(other.task_);
    other.task_ = ktl::nullopt;
  }

  AutoReleasableTask& operator=(AutoReleasableTask&& other) noexcept {
    task_ = ktl::move(other.task_);
    other.task_ = ktl::nullopt;
    return *this;
  }

 private:
  explicit AutoReleasableTask(ktl::optional<starnix::CurrentTask> task) : task_(ktl::move(task)) {}

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(AutoReleasableTask);

  ktl::optional<starnix::CurrentTask> task_ = ktl::nullopt;
};

/// An old way of creating a task for testing
///
/// This way of creating a task has problems because the test isn't actually run with that task
/// being current, which means that functions that expect a CurrentTask to actually be mapped into
/// memory can operate incorrectly.
///
/// Please use `spawn_kernel_and_run` instead. If there isn't a variant of `spawn_kernel_and_run`
/// for this use case, please consider adding one that follows the new pattern of actually running
/// the test on the spawned task.
ktl::pair<fbl::RefPtr<Kernel>, starnix::testing::AutoReleasableTask>
create_kernel_task_and_unlocked_with_bootfs();

ktl::pair<fbl::RefPtr<Kernel>, starnix::testing::AutoReleasableTask>
create_kernel_task_and_unlocked_with_bootfs_current_zbi();

/// An old way of creating a task for testing
///
/// This way of creating a task has problems because the test isn't actually run with that task
/// being current, which means that functions that expect a CurrentTask to actually be mapped into
/// memory can operate incorrectly.
///
/// Please use `spawn_kernel_and_run` instead. If there isn't a variant of `spawn_kernel_and_run`
/// for this use case, please consider adding one that follows the new pattern of actually running
/// the test on the spawned task.
ktl::pair<fbl::RefPtr<starnix::Kernel>, AutoReleasableTask> create_kernel_and_task();

/// Create a Kernel object and run the given callback in the init process for that kernel.
///
/// This function is useful if you want to test code that requires a CurrentTask because
/// your callback is called with the init process as the CurrentTask.
// pub fn spawn_kernel_and_run<F>(callback: F)

/// An old way of creating a task for testing
///
/// This way of creating a task has problems because the test isn't actually run with that task
/// being current, which means that functions that expect a CurrentTask to actually be mapped into
/// memory can operate incorrectly.
///
/// Please use `spawn_kernel_and_run` instead. If there isn't a variant of `spawn_kernel_and_run`
/// for this use case, please consider adding one that follows the new pattern of actually running
/// the test on the spawned task.
ktl::pair<fbl::RefPtr<starnix::Kernel>, AutoReleasableTask> create_kernel_task_and_unlocked();

/// An old way of creating a task for testing
///
/// This way of creating a task has problems because the test isn't actually run with that task
/// being current, which means that functions that expect a CurrentTask to actually be mapped
/// into memory can operate incorrectly.
///
/// Please use `spawn_kernel_and_run` instead. If there isn't a variant of
/// `spawn_kernel_and_run` for this use case, please consider adding one that follows the new
/// pattern of actually running the test on the spawned task.
AutoReleasableTask create_task(fbl::RefPtr<starnix::Kernel>& kernel,
                               const ktl::string_view& task_name);

// Maps `length` at `address` with `PROT_READ | PROT_WRITE`, `MAP_ANONYMOUS | MAP_PRIVATE`.
//
// Returns the address returned by `sys_mmap`.
UserAddress map_memory(starnix::CurrentTask& current_task, UserAddress address, uint64_t length);

// Maps `length` at `address` with `PROT_READ | PROT_WRITE` and the specified flags.
//
// Returns the address returned by `sys_mmap`.
UserAddress map_memory_with_flags(starnix::CurrentTask& current_task, UserAddress address,
                                  uint64_t length, uint32_t flags);

class TestFs : public FileSystemOps {
 public:
  fit::result<Errno, struct statfs> statfs(const FileSystem& fs,
                                           const CurrentTask& current_task) const override {
    return fit::ok(default_statfs(0));
  }

  const FsStr& name() const override { return kTestFs; }

  bool generate_node_ids() const override { return false; }

 private:
  constexpr static FsStr kTestFs = "test";
};

FileSystemHandle create_fs(fbl::RefPtr<starnix::Kernel>& kernel, FsNodeOps* ops);

}  // namespace testing
}  // namespace starnix

#endif  // VENDOR_MISTTECH_ZIRCON_KERNEL_LIB_STARNIX_KERNEL_INCLUDE_LIB_MISTOS_STARNIX_TESTING_TESTING_H_
