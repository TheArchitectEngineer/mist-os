// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_PAVER_MOONFLOWER_H_
#define SRC_STORAGE_LIB_PAVER_MOONFLOWER_H_

#include <hwreg/bitfields.h>

#include "src/storage/lib/paver/abr-client.h"
#include "src/storage/lib/paver/device-partitioner.h"
#include "src/storage/lib/paver/gpt.h"
#include "src/storage/lib/paver/partition-client.h"

namespace paver {

using FindPartitionDetailsResult = GptDevicePartitioner::FindPartitionDetailsResult;
using FilterCallback = GptDevicePartitioner::FilterCallback;

struct MoonflowerGptEntryAttributes {
  static constexpr uint8_t kMoonflowerMaxPriority = 3;

  explicit MoonflowerGptEntryAttributes(uint64_t flags) : flags(flags) {}

  uint64_t flags;
  DEF_SUBFIELD(flags, 49, 48, priority);
  DEF_SUBBIT(flags, 50, active);
  DEF_SUBFIELD(flags, 53, 51, retry_count);
  DEF_SUBBIT(flags, 54, boot_success);
  DEF_SUBBIT(flags, 55, unbootable);
};

class MoonflowerPartitioner : public DevicePartitioner {
 public:
  static zx::result<std::unique_ptr<DevicePartitioner>> Initialize(
      const BlockDevices& devices, fidl::UnownedClientEnd<fuchsia_io::Directory> svc_root,
      fidl::ClientEnd<fuchsia_device::Controller> block_device);

  zx::result<std::unique_ptr<abr::Client>> CreateAbrClient() const override;

  const paver::BlockDevices& Devices() const override;

  fidl::UnownedClientEnd<fuchsia_io::Directory> SvcRoot() const override;

  bool IsFvmWithinFtl() const override { return false; }

  bool SupportsPartition(const PartitionSpec& spec) const override;

  zx::result<std::unique_ptr<PartitionClient>> FindPartition(
      const PartitionSpec& spec) const override;

  zx::result<> WipeFvm() const override;

  zx::result<> ResetPartitionTables() const override;

  zx::result<> ValidatePayload(const PartitionSpec& spec,
                               std::span<const uint8_t> data) const override;

  zx::result<> Flush() const override { return zx::ok(); }

  zx::result<> OnStop() const override { return zx::ok(); }

  // Like FindPartition() above, but returns all matching entries.
  zx::result<std::vector<std::unique_ptr<BlockPartitionClient>>> FindAllPartitions(
      FilterCallback filter) const;

  // Like FindPartition() above, but returns a BlockPartitionClient instead, which has additional
  // GPT-related methods.
  zx::result<std::unique_ptr<BlockPartitionClient>> FindGptPartition(
      const PartitionSpec& spec) const;

  // Like FindPartition() above, but also returns the GPT partition entry.
  zx::result<FindPartitionDetailsResult> FindPartitionDetails(const PartitionSpec& spec) const;

  // TODO(https://fxbug.dev/339491886): Remove when storage-host is enabled.
  zx::result<std::unique_ptr<GptDevice>> ConnectToGpt() const { return gpt_->ConnectToGpt(); }

 private:
  explicit MoonflowerPartitioner(std::unique_ptr<GptDevicePartitioner> gpt)
      : gpt_(std::move(gpt)) {}

  zx::result<std::string> PartitionNameForSpec(const PartitionSpec& spec) const;

  std::unique_ptr<GptDevicePartitioner> gpt_;
};

class MoonflowerPartitionerFactory : public DevicePartitionerFactory {
 public:
  zx::result<std::unique_ptr<DevicePartitioner>> New(
      const BlockDevices& devices, fidl::UnownedClientEnd<fuchsia_io::Directory> svc_root,
      Arch arch, std::shared_ptr<Context> context,
      fidl::ClientEnd<fuchsia_device::Controller> block_device) final;
};

}  // namespace paver

#endif  // SRC_STORAGE_LIB_PAVER_MOONFLOWER_H_
