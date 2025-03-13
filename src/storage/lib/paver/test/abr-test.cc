// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/abr/abr.h"

#include <endian.h>
#include <fidl/fuchsia.device/cpp/wire.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/cksum.h>
#include <lib/driver-integration-test/fixture.h>
#include <lib/fdio/directory.h>
#include <zircon/hw/gpt.h>

#include <algorithm>
#include <iostream>

#include <mock-boot-arguments/server.h>
#include <zxtest/zxtest.h>

#include "src/lib/uuid/uuid.h"
#include "src/storage/lib/block_client/cpp/remote_block_device.h"
#include "src/storage/lib/paver/abr-client.h"
#include "src/storage/lib/paver/astro.h"
#include "src/storage/lib/paver/luis.h"
#include "src/storage/lib/paver/moonflower.h"
#include "src/storage/lib/paver/sherlock.h"
#include "src/storage/lib/paver/test/test-utils.h"
#include "src/storage/lib/paver/uefi.h"

namespace abr {

using device_watcher::RecursiveWaitForFile;
using driver_integration_test::IsolatedDevmgr;
using paver::MoonflowerGptEntryAttributes;

TEST(AstroAbrTests, CreateFails) {
  IsolatedDevmgr devmgr;
  IsolatedDevmgr::Args args;
  args.disable_block_watcher = false;
  args.board_name = "sherlock";

  ASSERT_OK(IsolatedDevmgr::Create(&args, &devmgr));
  ASSERT_OK(RecursiveWaitForFile(devmgr.devfs_root().get(), "sys/platform").status_value());

  zx::result devices = paver::BlockDevices::CreateDevfs(devmgr.devfs_root().duplicate());
  ASSERT_OK(devices);
  std::shared_ptr<paver::Context> context;
  zx::result partitioner = paver::AstroPartitionerFactory().New(*devices, devmgr.RealmExposedDir(),
                                                                paver::Arch::kArm64, context, {});
  ASSERT_NOT_OK(partitioner);
}

TEST(SherlockAbrTests, CreateFails) {
  IsolatedDevmgr devmgr;
  IsolatedDevmgr::Args args;
  args.disable_block_watcher = false;
  args.board_name = "astro";

  ASSERT_OK(IsolatedDevmgr::Create(&args, &devmgr));
  ASSERT_OK(RecursiveWaitForFile(devmgr.devfs_root().get(), "sys/platform").status_value());

  zx::result devices = paver::BlockDevices::CreateDevfs(devmgr.devfs_root().duplicate());
  ASSERT_OK(devices);
  std::shared_ptr<paver::Context> context;
  zx::result partitioner = paver::SherlockPartitionerFactory().New(
      *devices, devmgr.RealmExposedDir(), paver::Arch::kArm64, context, {});
  ASSERT_NOT_OK(partitioner);
}

TEST(MoonflowerAbrTests, CreateFails) {
  IsolatedDevmgr devmgr;
  IsolatedDevmgr::Args args;
  args.disable_block_watcher = false;
  args.board_name = "astro";

  ASSERT_OK(IsolatedDevmgr::Create(&args, &devmgr));
  ASSERT_OK(RecursiveWaitForFile(devmgr.devfs_root().get(), "sys/platform").status_value());

  zx::result devices = paver::BlockDevices::CreateDevfs(devmgr.devfs_root().duplicate());
  ASSERT_OK(devices);
  std::shared_ptr<paver::Context> context;
  zx::result partitioner = paver::MoonflowerPartitionerFactory().New(
      *devices, devmgr.RealmExposedDir(), paver::Arch::kArm64, context, {});
  ASSERT_NOT_OK(partitioner);
}

TEST(LuisAbrTests, CreateFails) {
  IsolatedDevmgr devmgr;
  IsolatedDevmgr::Args args;
  args.disable_block_watcher = false;
  args.board_name = "astro";

  ASSERT_OK(IsolatedDevmgr::Create(&args, &devmgr));
  ASSERT_OK(RecursiveWaitForFile(devmgr.devfs_root().get(), "sys/platform").status_value());

  zx::result devices = paver::BlockDevices::CreateDevfs(devmgr.devfs_root().duplicate());
  ASSERT_OK(devices);
  std::shared_ptr<paver::Context> context;
  zx::result partitioner = paver::LuisPartitionerFactory().New(*devices, devmgr.RealmExposedDir(),
                                                               paver::Arch::kArm64, context, {});
  ASSERT_NOT_OK(partitioner);
}

TEST(X64AbrTests, CreateFails) {
  IsolatedDevmgr devmgr;
  IsolatedDevmgr::Args args;
  args.disable_block_watcher = false;
  args.board_name = "x64";

  ASSERT_OK(IsolatedDevmgr::Create(&args, &devmgr));
  ASSERT_OK(RecursiveWaitForFile(devmgr.devfs_root().get(), "sys/platform").status_value());

  zx::result devices = paver::BlockDevices::CreateDevfs(devmgr.devfs_root().duplicate());
  ASSERT_OK(devices);
  std::shared_ptr<paver::Context> context;
  zx::result partitioner = paver::UefiPartitionerFactory().New(*devices, devmgr.RealmExposedDir(),
                                                               paver::Arch::kX64, context, {});
  ASSERT_NOT_OK(partitioner);
}

class CurrentSlotUuidTest : public PaverTest {
 protected:
  static constexpr int kBlockSize = 512;
  static constexpr int kDiskBlocks = 1024;
  static constexpr uuid::Uuid kEmptyGuid;
  static constexpr uint8_t kEmptyType[GPT_GUID_LEN] = GUID_EMPTY_VALUE;
  static constexpr uint8_t kZirconType[GPT_GUID_LEN] = GPT_ZIRCON_ABR_TYPE_GUID;
  static constexpr uint8_t kTestUuid[GPT_GUID_LEN] = {0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
                                                      0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
                                                      0xcc, 0xdd, 0xee, 0xff};
  void SetUp() override {
    PaverTest::SetUp();

    IsolatedDevmgr::Args args = DevmgrArgs();
    ASSERT_OK(IsolatedDevmgr::Create(&args, &devmgr_));
    ASSERT_OK(RecursiveWaitForFile(devmgr_.devfs_root().get(), "sys/platform/ram-disk/ramctl")
                  .status_value());
  }

  virtual IsolatedDevmgr::Args DevmgrArgs() {
    IsolatedDevmgr::Args args;
    // storage-host publishes devices synchronously so it's easier to test with.
    args.enable_storage_host = true;
    return args;
  }

  zx::result<paver::BlockDevices> CreateBlockDevices() {
    if (DevmgrArgs().enable_storage_host) {
      return paver::BlockDevices::CreateFromPartitionService(devmgr_.RealmExposedDir().borrow());
    }
    return paver::BlockDevices::CreateDevfs(devmgr_.devfs_root().duplicate());
  }

  void CreateGptDevice(const std::vector<PartitionDescription>& partitions) {
    ASSERT_NO_FATAL_FAILURE(BlockDevice::CreateWithGpt(devmgr_.devfs_root(), kDiskBlocks,
                                                       kBlockSize, partitions, &disk_));
  }

  IsolatedDevmgr devmgr_;
  std::unique_ptr<BlockDevice> disk_;
};

TEST_F(CurrentSlotUuidTest, TestZirconAIsSlotA) {
  ASSERT_NO_FATAL_FAILURE(CreateGptDevice({
      PartitionDescription{"zircon_a", uuid::Uuid(kZirconType), 0x22, 0x1, uuid::Uuid(kTestUuid)},
  }));

  zx::result devices = CreateBlockDevices();
  ASSERT_OK(devices);
  auto result = abr::PartitionUuidToConfiguration(*devices, uuid::Uuid(kTestUuid));
  ASSERT_OK(result);
  ASSERT_EQ(result.value(), fuchsia_paver::wire::Configuration::kA);
}

TEST_F(CurrentSlotUuidTest, TestZirconAWithUnderscore) {
  ASSERT_NO_FATAL_FAILURE(CreateGptDevice({
      PartitionDescription{"zircon_a", uuid::Uuid(kZirconType), 0x22, 0x1, uuid::Uuid(kTestUuid)},
  }));

  zx::result devices = CreateBlockDevices();
  ASSERT_OK(devices);
  auto result = abr::PartitionUuidToConfiguration(*devices, uuid::Uuid(kTestUuid));
  ASSERT_OK(result);
  ASSERT_EQ(result.value(), fuchsia_paver::wire::Configuration::kA);
}

TEST_F(CurrentSlotUuidTest, TestZirconAMixedCase) {
  ASSERT_NO_FATAL_FAILURE(CreateGptDevice({
      PartitionDescription{"ZiRcOn_A", uuid::Uuid(kZirconType), 0x22, 0x1, uuid::Uuid(kTestUuid)},
  }));

  zx::result devices = CreateBlockDevices();
  ASSERT_OK(devices);
  auto result = abr::PartitionUuidToConfiguration(*devices, uuid::Uuid(kTestUuid));
  ASSERT_OK(result);
  ASSERT_EQ(result.value(), fuchsia_paver::wire::Configuration::kA);
}

TEST_F(CurrentSlotUuidTest, TestZirconB) {
  ASSERT_NO_FATAL_FAILURE(CreateGptDevice({
      PartitionDescription{"zircon_b", uuid::Uuid(kZirconType), 0x22, 0x1, uuid::Uuid(kTestUuid)},
  }));

  zx::result devices = CreateBlockDevices();
  ASSERT_OK(devices);
  auto result = abr::PartitionUuidToConfiguration(*devices, uuid::Uuid(kTestUuid));
  ASSERT_OK(result);
  ASSERT_EQ(result.value(), fuchsia_paver::wire::Configuration::kB);
}

TEST_F(CurrentSlotUuidTest, TestZirconR) {
  ASSERT_NO_FATAL_FAILURE(CreateGptDevice({
      PartitionDescription{"ZIRCON_R", uuid::Uuid(kZirconType), 0x22, 0x1, uuid::Uuid(kTestUuid)},
  }));

  zx::result devices = CreateBlockDevices();
  ASSERT_OK(devices);
  auto result = abr::PartitionUuidToConfiguration(*devices, uuid::Uuid(kTestUuid));
  ASSERT_OK(result);
  ASSERT_EQ(result.value(), fuchsia_paver::wire::Configuration::kRecovery);
}

TEST_F(CurrentSlotUuidTest, TestInvalid) {
  ASSERT_NO_FATAL_FAILURE(CreateGptDevice({
      PartitionDescription{"ZERCON_R", uuid::Uuid(kZirconType), 0x22, 0x1, uuid::Uuid(kTestUuid)},
  }));

  zx::result devices = CreateBlockDevices();
  ASSERT_OK(devices);
  auto result = abr::PartitionUuidToConfiguration(*devices, uuid::Uuid(kTestUuid));
  ASSERT_TRUE(result.is_error());
  ASSERT_EQ(result.error_value(), ZX_ERR_NOT_SUPPORTED);
}

TEST(CurrentSlotTest, TestA) {
  auto result = abr::CurrentSlotToConfiguration("_a");
  ASSERT_OK(result);
  ASSERT_EQ(result.value(), fuchsia_paver::wire::Configuration::kA);
}

TEST(CurrentSlotTest, TestB) {
  auto result = abr::CurrentSlotToConfiguration("_b");
  ASSERT_OK(result);
  ASSERT_EQ(result.value(), fuchsia_paver::wire::Configuration::kB);
}

TEST(CurrentSlotTest, TestR) {
  auto result = abr::CurrentSlotToConfiguration("_r");
  ASSERT_OK(result);
  ASSERT_EQ(result.value(), fuchsia_paver::wire::Configuration::kRecovery);
}

TEST(CurrentSlotTest, TestInvalid) {
  auto result = abr::CurrentSlotToConfiguration("_x");
  ASSERT_TRUE(result.is_error());
  ASSERT_EQ(result.error_value(), ZX_ERR_NOT_SUPPORTED);
}

class FakeBootArgs : public fidl::WireServer<fuchsia_boot::Arguments> {
 public:
  void GetStrings(GetStringsRequestView request, GetStringsCompleter::Sync& completer) override {
    std::vector<fidl::StringView> response = {
        fidl::StringView(),
        fidl::StringView(),
        fidl::StringView::FromExternal("_a"),
    };
    completer.Reply(fidl::VectorView<fidl::StringView>::FromExternal(response));
  }

  // Not implemented.
  void GetString(GetStringRequestView request, GetStringCompleter::Sync& completer) override {}
  void GetBool(GetBoolRequestView request, GetBoolCompleter::Sync& completer) override {}
  void GetBools(GetBoolsRequestView request, GetBoolsCompleter::Sync& completer) override {}
  void Collect(CollectRequestView request, CollectCompleter::Sync& completer) override {}
};

class MoonflowerAbrClientTest : public CurrentSlotUuidTest {
 protected:
  static constexpr uint8_t kFvmType[GPT_GUID_LEN] = GPT_FVM_TYPE_GUID;
  static constexpr uint8_t kVbMetaType[GPT_GUID_LEN] = GPT_VBMETA_ABR_TYPE_GUID;
  static constexpr uint8_t kBootloaderType[GPT_GUID_LEN] = GPT_BOOTLOADER_ABR_TYPE_GUID;
  static constexpr uint8_t kFactoryType[GPT_GUID_LEN] = GPT_FACTORY_TYPE_GUID;

  IsolatedDevmgr::Args DevmgrArgs() override {
    IsolatedDevmgr::Args args;
    args.board_name = "sorrel";
    args.fake_boot_args = std::make_unique<FakeBootArgs>();
    args.disable_block_watcher = false;
    return args;
  }

  void SetUp() override {
    CurrentSlotUuidTest::SetUp();

    ASSERT_NO_FATAL_FAILURE(CreateGptDevice({
        PartitionDescription{"boot_a", uuid::Uuid(kZirconType), 0x22, 0x1},
        PartitionDescription{"boot_b", uuid::Uuid(kBootloaderType), 0x23, 0x1},
        PartitionDescription{"super", uuid::Uuid(kFvmType), 0x24, 0x1},
        PartitionDescription{"vbmeta_a", uuid::Uuid(kVbMetaType), 0x25, 0x1},
        PartitionDescription{"vbmeta_b", uuid::Uuid(kBootloaderType), 0x26, 0x1},
        PartitionDescription{"flipped_guid_a", uuid::Uuid(kBootloaderType), 0x27, 0x1},
        PartitionDescription{"flipped_guid_b", uuid::Uuid(kFactoryType), 0x28, 0x1},
    }));

    zx::result devices = CreateBlockDevices();
    ASSERT_OK(devices);
    std::shared_ptr<paver::Context> context;
    zx::result partitioner = paver::MoonflowerPartitionerFactory().New(
        *devices, devmgr_.RealmExposedDir(), paver::Arch::kArm64, context, {});
    ASSERT_OK(partitioner);
    zx::result abr_client = partitioner->CreateAbrClient();
    ASSERT_OK(abr_client);
    partitioner_ = std::move(*partitioner);
    abr_client_ = std::move(*abr_client);
  }

  zx::result<std::unique_ptr<gpt::GptDevice>> OpenGptDevice() {
    zx::result new_connection = GetNewConnections(disk_->block_controller_interface());
    if (new_connection.is_error()) {
      return new_connection.take_error();
    }
    fidl::ClientEnd<fuchsia_hardware_block_volume::Volume> volume(
        std::move(new_connection->device));
    zx::result remote_device = block_client::RemoteBlockDevice::Create(
        std::move(volume), std::move(new_connection->controller));
    if (remote_device.is_error()) {
      return remote_device.take_error();
    }
    return gpt::GptDevice::Create(std::move(remote_device.value()),
                                  /*blocksize=*/disk_->block_size(),
                                  /*blocks=*/disk_->block_count());
  }

  void CheckPartitionState(uint32_t index, std::string_view name, const uint8_t* type_guid,
                           MoonflowerGptEntryAttributes* out) {
    zx::result gpt_result = OpenGptDevice();
    ASSERT_OK(gpt_result);
    std::unique_ptr<gpt::GptDevice> gpt = std::move(gpt_result.value());
    zx::result gpt_entry = gpt->GetPartition(index);
    ASSERT_OK(gpt_entry);

    char cstring_name[GPT_NAME_LEN / 2 + 1] = {0};
    ::utf16_to_cstring(cstring_name, reinterpret_cast<const uint16_t*>(gpt_entry->name),
                       sizeof(cstring_name));
    const std::string_view partition_name = cstring_name;
    ASSERT_EQ(partition_name, name);

    ASSERT_EQ(uuid::Uuid(gpt_entry->type), uuid::Uuid(type_guid));

    *out = MoonflowerGptEntryAttributes{gpt_entry->flags};
  }

  void AbrClientFlush() { ASSERT_OK(abr_client_->Flush()); }

  void MoonflowerTest();

  std::unique_ptr<paver::DevicePartitioner> partitioner_;
  std::unique_ptr<abr::Client> abr_client_;
};

void MoonflowerAbrClientTest::MoonflowerTest() {
  // Initial active slot A.
  ASSERT_OK(abr_client_->MarkSlotActive(kAbrSlotIndexA));
  ASSERT_OK(abr_client_->MarkSlotSuccessful(kAbrSlotIndexA));
  AbrClientFlush();

  MoonflowerGptEntryAttributes attributes{0};
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(0, "boot_a", kZirconType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority);
  EXPECT_EQ(attributes.active(), true);
  EXPECT_EQ(attributes.retry_count(), 0);
  EXPECT_EQ(attributes.boot_success(), true);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(1, "boot_b", kBootloaderType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority - 1);
  EXPECT_EQ(attributes.active(), false);
  EXPECT_EQ(attributes.retry_count(), 0);
  EXPECT_EQ(attributes.boot_success(), false);
  EXPECT_EQ(attributes.unbootable(), true);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(2, "super", kFvmType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(3, "vbmeta_a", kVbMetaType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(4, "vbmeta_b", kBootloaderType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(5, "flipped_guid_a", kBootloaderType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(6, "flipped_guid_b", kFactoryType, &attributes));

  ASSERT_OK(abr_client_->MarkSlotActive(kAbrSlotIndexB));
  AbrClientFlush();

  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(0, "boot_a", kBootloaderType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority - 1);
  ASSERT_EQ(attributes.active(), false);
  EXPECT_EQ(attributes.retry_count(), 0);
  EXPECT_EQ(attributes.boot_success(), true);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(1, "boot_b", kZirconType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority);
  EXPECT_EQ(attributes.active(), true);
  EXPECT_EQ(attributes.retry_count(), kAbrMaxTriesRemaining);
  EXPECT_EQ(attributes.boot_success(), false);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(2, "super", kFvmType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(3, "vbmeta_a", kBootloaderType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(4, "vbmeta_b", kVbMetaType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(5, "flipped_guid_a", kBootloaderType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(6, "flipped_guid_b", kFactoryType, &attributes));

  ASSERT_OK(abr_client_->MarkSlotSuccessful(kAbrSlotIndexB));
  AbrClientFlush();

  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(0, "boot_a", kBootloaderType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority - 1);
  EXPECT_EQ(attributes.active(), false);
  EXPECT_EQ(attributes.retry_count(), kAbrMaxTriesRemaining);
  EXPECT_EQ(attributes.boot_success(), false);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(1, "boot_b", kZirconType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority);
  EXPECT_EQ(attributes.active(), true);
  EXPECT_EQ(attributes.retry_count(), 0);
  EXPECT_EQ(attributes.boot_success(), true);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(2, "super", kFvmType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(3, "vbmeta_a", kBootloaderType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(4, "vbmeta_b", kVbMetaType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(5, "flipped_guid_a", kBootloaderType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(6, "flipped_guid_b", kFactoryType, &attributes));

  ASSERT_OK(abr_client_->MarkSlotActive(kAbrSlotIndexA));
  AbrClientFlush();

  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(0, "boot_a", kZirconType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority);
  EXPECT_EQ(attributes.active(), true);
  EXPECT_EQ(attributes.retry_count(), kAbrMaxTriesRemaining);
  EXPECT_EQ(attributes.boot_success(), false);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(1, "boot_b", kBootloaderType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority - 1);
  EXPECT_EQ(attributes.active(), false);
  EXPECT_EQ(attributes.retry_count(), 0);
  EXPECT_EQ(attributes.boot_success(), true);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(2, "super", kFvmType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(3, "vbmeta_a", kVbMetaType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(4, "vbmeta_b", kBootloaderType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(5, "flipped_guid_a", kFactoryType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(6, "flipped_guid_b", kBootloaderType, &attributes));

  ASSERT_OK(abr_client_->MarkSlotSuccessful(kAbrSlotIndexA));
  AbrClientFlush();

  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(0, "boot_a", kZirconType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority);
  EXPECT_EQ(attributes.active(), true);
  EXPECT_EQ(attributes.retry_count(), 0);
  EXPECT_EQ(attributes.boot_success(), true);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(1, "boot_b", kBootloaderType, &attributes));
  EXPECT_EQ(attributes.priority(), MoonflowerGptEntryAttributes::kMoonflowerMaxPriority - 1);
  EXPECT_EQ(attributes.active(), false);
  EXPECT_EQ(attributes.retry_count(), kAbrMaxTriesRemaining);
  EXPECT_EQ(attributes.boot_success(), false);
  EXPECT_EQ(attributes.unbootable(), false);
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(2, "super", kFvmType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(3, "vbmeta_a", kVbMetaType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(4, "vbmeta_b", kBootloaderType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(5, "flipped_guid_a", kFactoryType, &attributes));
  ASSERT_NO_FATAL_FAILURE(CheckPartitionState(6, "flipped_guid_b", kBootloaderType, &attributes));
}

class MoonflowerAbrClientWithStorageHostTest : public MoonflowerAbrClientTest {
 protected:
  IsolatedDevmgr::Args DevmgrArgs() override {
    IsolatedDevmgr::Args args = MoonflowerAbrClientTest::DevmgrArgs();
    args.enable_storage_host = true;
    return args;
  }
};

TEST_F(MoonflowerAbrClientTest, MoonflowerTest) { ASSERT_NO_FATAL_FAILURE(MoonflowerTest()); }

TEST_F(MoonflowerAbrClientWithStorageHostTest, MoonflowerTest) {
  ASSERT_NO_FATAL_FAILURE(MoonflowerTest());
}

class FakePartitionClient final : public paver::PartitionClient {
 public:
  FakePartitionClient(size_t block_size, size_t partition_size)
      : block_size_(block_size), partition_size_(partition_size) {}

  zx::result<size_t> GetBlockSize() final {
    if (result_ == ZX_OK) {
      return zx::ok(block_size_);
    }
    return zx::error(result_);
  }
  zx::result<size_t> GetPartitionSize() final {
    if (result_ == ZX_OK) {
      return zx::ok(partition_size_);
    }
    return zx::error(result_);
  }

  zx::result<> Read(const zx::vmo& vmo, size_t size) final {
    if (size > partition_size_) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }
    return zx::make_result(result_);
  }
  zx::result<> Write(const zx::vmo& vmo, size_t vmo_size) final {
    if (vmo_size > partition_size_) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }
    return zx::make_result(result_);
  }
  zx::result<> Trim() final { return zx::make_result(result_); }
  zx::result<> Flush() final { return zx::make_result(result_); }

  void set_result(zx_status_t result) { result_ = result; }

 private:
  size_t block_size_;
  size_t partition_size_;

  zx_status_t result_ = ZX_OK;
};

class OneShotFlagsTest : public PaverTest {
 public:
  void SetUp() override {
    PaverTest::SetUp();
    auto partition_client = std::make_unique<FakePartitionClient>(10, 100);
    auto abr_partition_client = abr::AbrPartitionClient::Create(std::move(partition_client));
    ASSERT_OK(abr_partition_client);
    abr_client_ = std::move(abr_partition_client.value());

    // Clear flags
    ASSERT_OK(abr_client_->GetAndClearOneShotFlags());
  }

  std::unique_ptr<abr::Client> abr_client_;
};

TEST_F(OneShotFlagsTest, ClearFlags) {
  // Set some flags to see that they are cleared
  ASSERT_OK(abr_client_->SetOneShotRecovery());
  ASSERT_OK(abr_client_->SetOneShotBootloader());

  // First get flags would return flags
  auto abr_flags_res = abr_client_->GetAndClearOneShotFlags();
  ASSERT_OK(abr_flags_res);
  EXPECT_NE(abr_flags_res.value(), kAbrDataOneShotFlagNone);

  // Second get flags should be cleared
  abr_flags_res = abr_client_->GetAndClearOneShotFlags();
  ASSERT_OK(abr_flags_res);
  EXPECT_EQ(abr_flags_res.value(), kAbrDataOneShotFlagNone);
}

TEST_F(OneShotFlagsTest, SetOneShotRecovery) {
  ASSERT_OK(abr_client_->SetOneShotRecovery());

  // Check if flag is set
  auto abr_flags_res = abr_client_->GetAndClearOneShotFlags();
  ASSERT_OK(abr_flags_res);
  EXPECT_TRUE(AbrIsOneShotRecoveryBootSet(abr_flags_res.value()));
  EXPECT_FALSE(AbrIsOneShotBootloaderBootSet(abr_flags_res.value()));
}

TEST_F(OneShotFlagsTest, SetOneShotBootloader) {
  ASSERT_OK(abr_client_->SetOneShotBootloader());

  // Check if flag is set
  auto abr_flags_res = abr_client_->GetAndClearOneShotFlags();
  ASSERT_OK(abr_flags_res);
  EXPECT_TRUE(AbrIsOneShotBootloaderBootSet(abr_flags_res.value()));
  EXPECT_FALSE(AbrIsOneShotRecoveryBootSet(abr_flags_res.value()));
}

TEST_F(OneShotFlagsTest, Set2Flags) {
  ASSERT_OK(abr_client_->SetOneShotBootloader());
  ASSERT_OK(abr_client_->SetOneShotRecovery());

  // Check if flag is set
  auto abr_flags_res = abr_client_->GetAndClearOneShotFlags();
  ASSERT_OK(abr_flags_res);
  EXPECT_TRUE(AbrIsOneShotBootloaderBootSet(abr_flags_res.value()));
  EXPECT_TRUE(AbrIsOneShotRecoveryBootSet(abr_flags_res.value()));
}

}  // namespace abr
