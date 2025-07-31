// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.test/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/driver_test_realm/realm_builder/cpp/lib.h>
#include <lib/fdio/fd.h>

#include <vector>

#include <bind/fuchsia/platform/cpp/bind.h>
#include <zxtest/zxtest.h>

#include "src/storage/fvm/format.h"
#include "src/storage/fvm/test_support.h"

namespace fvm {
namespace {

// Shared constants for all resize tests.
constexpr uint64_t kTestBlockSize = 512;
constexpr uint64_t kSliceSize = 1 << 20;

constexpr uint64_t kDataSizeInBlocks = 10;
constexpr uint64_t kDataSize = kTestBlockSize * kDataSizeInBlocks;

constexpr char kPartitionName[] = "partition-name";
constexpr uint8_t kPartitionUniqueGuid[] = {0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
                                            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f};
constexpr uint8_t kPartitionTypeGuid[] = {0xAA, 0xFF, 0xBB, 0x00, 0x33, 0x44, 0x88, 0x99,
                                          0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17};
constexpr uint64_t kPartitionSliceCount = 1;

struct GrowParams {
  // random seed.
  unsigned int seed;

  // Target size of the ramdisk.
  uint64_t target_size;

  // The expected format info at each step.
  Header format;

  // Attempt to allocate, read and write to new slices.
  bool validate_new_slices;
};

void GrowFvm(const fbl::unique_fd& devfs_root, const GrowParams& params,
             std::unique_ptr<RamdiskRef>& ramdisk, std::unique_ptr<FvmAdapter>& fvm_adapter) {
  std::unique_ptr<VPartitionAdapter> vpartition = nullptr;
  ASSERT_OK(fvm_adapter->AddPartition(devfs_root, kPartitionName, Guid(kPartitionUniqueGuid),
                                      Guid(kPartitionTypeGuid), kPartitionSliceCount, &vpartition),
            "Failed to add partition.");
  ASSERT_TRUE(vpartition);

  // Get current state of the FVM.
  VolumeManagerInfo before_grow_info;
  ASSERT_OK(fvm_adapter->Query(&before_grow_info));
  ASSERT_EQ(kSliceSize, before_grow_info.slice_size);
  ASSERT_EQ(kPartitionSliceCount, before_grow_info.assigned_slice_count);

  unsigned int initial_seed = params.seed;
  auto random_data = MakeRandomBuffer(kDataSize, &initial_seed);
  ASSERT_NO_FATAL_FAILURE(vpartition->WriteAt(random_data, 0));

  // Clone the device to a new ramdisk with the specified target size.
  zx::result new_ramdisk = ramdisk->Clone(params.target_size);
  ASSERT_EQ(new_ramdisk.status_value(), ZX_OK);

  // This will destroy the old ramdisk.
  ramdisk = *std::move(new_ramdisk);

  // Bind a new FVM to the new device.
  fvm_adapter = FvmAdapter::Bind(devfs_root, ramdisk.get());
  ASSERT_NE(fvm_adapter, nullptr);

  // Find the partition on the new device.  This will try and destroy the old partition which no
  // longer exists but that doesn't matter.
  vpartition = VPartitionAdapter::Create(devfs_root, kPartitionName, Guid(kPartitionUniqueGuid),
                                         Guid(kPartitionTypeGuid));
  ASSERT_NE(vpartition, nullptr);
  vpartition->WaitUntilVisible();

  // Get stats after growth.
  VolumeManagerInfo after_grow_info;
  ASSERT_OK(fvm_adapter->Query(&after_grow_info));
  ASSERT_TRUE(IsConsistentAfterGrowth(before_grow_info, after_grow_info));
  ASSERT_EQ(params.format.pslice_count, after_grow_info.slice_count);
  // Data should still be present.
  ASSERT_NO_FATAL_FAILURE(vpartition->CheckContentsAt(random_data, 0));

  // Verify new slices can be allocated, written to and read from.
  if (params.validate_new_slices) {
    ASSERT_OK(vpartition->Extend(kPartitionSliceCount,
                                 after_grow_info.slice_count - kPartitionSliceCount));

    auto random_data_2 = MakeRandomBuffer(kDataSize, &initial_seed);
    uint64_t offset = (params.format.pslice_count - 1) * kSliceSize;
    ASSERT_NO_FATAL_FAILURE(vpartition->WriteAt(random_data_2, offset));
    ASSERT_NO_FATAL_FAILURE(vpartition->CheckContentsAt(random_data_2, offset));
  }

  ASSERT_OK(vpartition->Destroy());
}

class FvmResizeTest : public zxtest::Test {
 protected:
  void SetUp() override {
    loop_ = std::make_unique<async::Loop>(&kAsyncLoopConfigNoAttachToCurrentThread);
    loop_->StartThread();

    auto realm_builder = component_testing::RealmBuilder::Create();
    driver_test_realm::Setup(realm_builder);
    realm_ =
        std::make_unique<component_testing::RealmRoot>(realm_builder.Build(loop_->dispatcher()));

    zx::result dtr = realm_->component().Connect<fuchsia_driver_test::Realm>();
    ASSERT_OK(dtr.status_value());
    fidl::Arena arena;
    auto args_builder = fuchsia_driver_test::wire::RealmArgs::Builder(arena);
    args_builder.root_driver("fuchsia-boot:///platform-bus#meta/platform-bus.cm");
    args_builder.software_devices(std::vector{
        fuchsia_driver_test::wire::SoftwareDevice{
            .device_name = "ram-disk",
            .device_id = bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_RAM_DISK,
        },
    });
    fidl::WireResult result = fidl::WireCall(*dtr)->Start(args_builder.Build());
    ASSERT_OK(result.status());
    ASSERT_TRUE(result.value().is_ok());

    auto [devfs_client, server] = fidl::Endpoints<fuchsia_io::Node>::Create();
    fidl::UnownedClientEnd<fuchsia_io::Directory> exposed(
        realm_->component().exposed().unowned_channel());
    ASSERT_OK(fidl::WireCall(exposed)
                  ->Open("dev-topological", fuchsia_io::kPermReadable, {}, server.TakeChannel())
                  .status());
    ASSERT_OK(
        fdio_fd_create(devfs_client.TakeChannel().release(), devfs_root_.reset_and_get_address()));
  }

  const fbl::unique_fd& devfs_root_fd() const { return devfs_root_; }

 private:
  std::unique_ptr<async::Loop> loop_;
  std::unique_ptr<component_testing::RealmRoot> realm_;
  fbl::unique_fd devfs_root_;
};

TEST_F(FvmResizeTest, PreallocatedMetadataGrowsCorrectly) {
  constexpr uint64_t kInitialBlockCount = (50 * kSliceSize) / kTestBlockSize;
  constexpr uint64_t kMaxBlockCount = (4 << 10) * kSliceSize / kTestBlockSize;

  std::unique_ptr<RamdiskRef> ramdisk =
      RamdiskRef::Create(devfs_root_fd(), kTestBlockSize, kInitialBlockCount);
  ASSERT_TRUE(ramdisk);
  std::unique_ptr<FvmAdapter> fvm =
      FvmAdapter::CreateGrowable(devfs_root_fd(), kTestBlockSize, kInitialBlockCount,
                                 kMaxBlockCount, kSliceSize, ramdisk.get());
  ASSERT_TRUE(fvm);

  GrowParams params;
  params.target_size = kMaxBlockCount * kTestBlockSize;
  // Data stays the same size, so there are no new slices.
  params.validate_new_slices = true;
  params.format =
      Header::FromDiskSize(fvm::kMaxUsablePartitions, kMaxBlockCount * kTestBlockSize, kSliceSize);
  params.seed = zxtest::Runner::GetInstance()->options().seed;

  ASSERT_NO_FATAL_FAILURE(GrowFvm(devfs_root_fd(), params, ramdisk, fvm));
}

TEST_F(FvmResizeTest, PreallocatedMetadataGrowsAsMuchAsPossible) {
  constexpr uint64_t kInitialBlockCount = (50 * kSliceSize) / kTestBlockSize;
  constexpr uint64_t kMaxBlockCount = (4 << 10) * kSliceSize / kTestBlockSize;

  std::unique_ptr<RamdiskRef> ramdisk =
      RamdiskRef::Create(devfs_root_fd(), kTestBlockSize, kInitialBlockCount);
  ASSERT_TRUE(ramdisk);
  std::unique_ptr<FvmAdapter> fvm =
      FvmAdapter::CreateGrowable(devfs_root_fd(), kTestBlockSize, kInitialBlockCount,
                                 kMaxBlockCount, kSliceSize, ramdisk.get());
  ASSERT_TRUE(fvm);

  // Compute the expected header information. This is the header computed for the original slice
  // size, expanded by as many slices as possible.
  Header expected =
      Header::FromDiskSize(kMaxUsablePartitions, kMaxBlockCount * kTestBlockSize, kSliceSize);
  expected.SetSliceCount(expected.GetAllocationTableAllocatedEntryCount());

  GrowParams params;
  // This defines a target size much larger than our header could handle so the resize will max
  // out the slices in the headeer.
  params.target_size = 2 * expected.fvm_partition_size;
  // Data stays the same size, so there are no new slices.
  params.validate_new_slices = false;
  params.format = expected;
  params.seed = zxtest::Runner::GetInstance()->options().seed;

  ASSERT_NO_FATAL_FAILURE(GrowFvm(devfs_root_fd(), params, ramdisk, fvm));
}

TEST_F(FvmResizeTest, PreallocatedMetadataRemainsValidInPartialGrowths) {
  constexpr uint64_t kInitialBlockCount = (50 * kSliceSize) / kTestBlockSize;
  constexpr uint64_t kMidBlockCount = (4 << 10) * kSliceSize / kTestBlockSize;
  constexpr uint64_t kMaxBlockCount = (8 << 10) * kSliceSize / kTestBlockSize;

  std::unique_ptr<RamdiskRef> ramdisk =
      RamdiskRef::Create(devfs_root_fd(), kTestBlockSize, kInitialBlockCount);
  ASSERT_TRUE(ramdisk);
  std::unique_ptr<FvmAdapter> fvm =
      FvmAdapter::CreateGrowable(devfs_root_fd(), kTestBlockSize, kInitialBlockCount,
                                 kMaxBlockCount, kSliceSize, ramdisk.get());
  ASSERT_TRUE(fvm);

  GrowParams params;
  params.target_size = kMidBlockCount * kTestBlockSize;
  // Data stays the same size, so there are no new slices.
  params.validate_new_slices = true;
  params.format =
      Header::FromGrowableDiskSize(kMaxUsablePartitions, kMidBlockCount * kTestBlockSize,
                                   kMaxBlockCount * kTestBlockSize, kSliceSize);
  params.seed = zxtest::Runner::GetInstance()->options().seed;

  ASSERT_NO_FATAL_FAILURE(GrowFvm(devfs_root_fd(), params, ramdisk, fvm));

  params.format =
      Header::FromGrowableDiskSize(kMaxUsablePartitions, kMaxBlockCount * kTestBlockSize,
                                   kMaxBlockCount * kTestBlockSize, kSliceSize);
  params.target_size = kMaxBlockCount * kTestBlockSize;
  ASSERT_NO_FATAL_FAILURE(GrowFvm(devfs_root_fd(), params, ramdisk, fvm));
}

}  // namespace
}  // namespace fvm
