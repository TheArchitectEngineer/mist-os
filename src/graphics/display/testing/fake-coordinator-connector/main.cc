// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace-provider/provider.h>

#include "src/graphics/display/drivers/fake/fake-display-device-config.h"
#include "src/graphics/display/lib/api-types/cpp/engine-info.h"
#include "src/graphics/display/lib/api-types/cpp/mode.h"
#include "src/graphics/display/testing/fake-coordinator-connector/service.h"

int main(int argc, const char** argv) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  trace::TraceProviderWithFdio trace_provider(loop.dispatcher());
  component::OutgoingDirectory outgoing(loop.dispatcher());

  zx::result<> serve_outgoing_directory_result = outgoing.ServeFromStartupInfo();
  if (serve_outgoing_directory_result.is_error()) {
    FX_LOGS(ERROR) << "Failed to serve outgoing directory: "
                   << serve_outgoing_directory_result.status_string();
    return -1;
  }

  FX_LOGS(INFO) << "Starting fake fuchsia.hardware.display.Provider service.";

  static constexpr fake_display::FakeDisplayDeviceConfig kFakeDisplayDeviceConfig = {
      // TODO(https://fxbug.dev/42079786): Populate from structured configuration.
      .display_mode = display::Mode({
          .active_width = 1280,
          .active_height = 800,
          .refresh_rate_millihertz = 60'000,
      }),
      .engine_info = display::EngineInfo({
          .max_layer_count = 1,
          .max_connected_display_count = 1,
          .is_capture_supported = true,
      }),
      .periodic_vsync = true,
  };

  display::FakeDisplayCoordinatorConnector connector(loop.dispatcher(), kFakeDisplayDeviceConfig);

  zx::result<> publish_service_result =
      outgoing.AddUnmanagedProtocol<fuchsia_hardware_display::Provider>(
          connector.bind_handler(loop.dispatcher()));
  if (publish_service_result.is_error()) {
    FX_LOGS(ERROR) << "Cannot publish display Provider service to default service directory: "
                   << publish_service_result.status_string();
    return -1;
  }

  zx::result<> publish_devfs_result =
      outgoing.AddUnmanagedProtocolAt<fuchsia_hardware_display::Provider>(
          "dev-display-coordinator", connector.bind_handler(loop.dispatcher()));
  if (publish_devfs_result.is_error()) {
    FX_LOGS(ERROR) << "Cannot publish display Provider service to devfs: "
                   << publish_devfs_result.status_string();
    return -1;
  }

  loop.Run();

  FX_LOGS(INFO) << "Quit fake Display Coordinator Connector main loop.";

  return 0;
}
