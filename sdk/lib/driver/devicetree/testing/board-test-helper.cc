// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/driver/devicetree/testing/board-test-helper.h"

#include <fcntl.h>
#include <fidl/fuchsia.driver.test/cpp/wire.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/driver_test_realm/realm_builder/cpp/lib.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/io.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/fit/defer.h>
#include <lib/sync/cpp/completion.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/vfs/cpp/service.h>
#include <lib/zbi-format/zbi.h>
#include <sys/stat.h>
#include <zircon/errors.h>

namespace fdf_devicetree::testing {

namespace {

using namespace component_testing;
namespace fdt = fuchsia_driver_test;

zx::result<zx::vmo> LoadDevicetreeBlob(const char* path) {
  int file = open(path, O_RDONLY);
  if (file == -1) {
    return zx::error(ZX_ERR_IO);
  }
  zx::vmo out;
  zx_status_t status = fdio_get_vmo_copy(file, out.reset_and_get_address());
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(out));
}

}  // namespace

BoardTestHelper::~BoardTestHelper() {
  libsync::Completion teardown_complete;
  realm_->Teardown(
      [&](fit::result<fuchsia::component::Error> result) { teardown_complete.Signal(); });
  teardown_complete.Wait();
}

void BoardTestHelper::SetupRealm() {
  auto realm_builder = component_testing::RealmBuilder::Create();
  realm_builder_ = std::make_unique<component_testing::RealmBuilder>(std::move(realm_builder));
  driver_test_realm::Setup(*realm_builder_);
}

zx::result<> BoardTestHelper::StartRealm() {
  realm_ = std::make_unique<component_testing::RealmRoot>(realm_builder_->Build(dispatcher_));

  auto client = realm_->component().Connect<fdt::Realm>();
  if (client.is_error()) {
    FX_LOGS(ERROR) << "Failed to connect to driver test realm : " << client.status_string();
    return client.take_error();
  }

  zx::result dtb = LoadDevicetreeBlob(dtb_path_.c_str());
  if (dtb.is_error()) {
    FX_LOGS(ERROR) << "Failed to open dtb : " << dtb.status_string();
    return dtb.take_error();
  }

  fidl::WireSyncClient driver_test_realm{std::move(*client)};
  fidl::Arena arena;
  auto args = fdt::wire::RealmArgs::Builder(arena)
                  .root_driver("fuchsia-boot:///platform-bus#meta/platform-bus.cm")
                  .devicetree(std::move(*dtb))
                  .platform_vid(platform_id_.vid)
                  .platform_pid(platform_id_.pid)
                  .board_name(platform_id_.board_name)
                  .Build();

  auto start_result = driver_test_realm->Start(args);
  if (start_result.status() != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to start driver test realm : " << start_result.status_string();
    return zx::error(start_result.status());
  }

  if (start_result->is_error()) {
    FX_LOGS(ERROR) << "Failed to start driver test realm : " << start_result.error();
    return start_result->take_error();
  }

  return zx::ok();
}

zx::result<> BoardTestHelper::WaitOnDevices(const std::vector<std::string>& device_paths) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  auto result = realm_->component().exposed()->Open("dev-topological", fuchsia::io::PERM_READABLE,
                                                    {}, server_end.TakeChannel());
  if (result != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to open dev-topological : " << zx_status_get_string(result);
    return zx::error(result);
  }

  int dev_fd;
  result = fdio_fd_create(client_end.TakeChannel().release(), &dev_fd);
  if (result != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create fd : " << zx_status_get_string(result);
    return zx::error(result);
  }

  auto close_fd = fit::defer([&dev_fd]() { close(dev_fd); });

  for (const auto& path : device_paths) {
    FX_LOGS(INFO) << "Waiting for " << path;
    auto wait_result = device_watcher::RecursiveWaitForFile(dev_fd, path.c_str());
    if (wait_result.is_error()) {
      FX_LOGS(ERROR) << "Failed to wait for " << path << " : " << wait_result.status_string();
      return wait_result.take_error();
    }
    FX_LOGS(INFO) << "Found " << path;
  }
  return zx::ok();
}

}  // namespace fdf_devicetree::testing
