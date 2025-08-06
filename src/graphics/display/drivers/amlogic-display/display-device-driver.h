// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_DISPLAY_DEVICE_DRIVER_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_DISPLAY_DEVICE_DRIVER_H_

#include <fidl/fuchsia.driver.framework/cpp/wire.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/zx/result.h>

#include <memory>

#include "src/graphics/display/drivers/amlogic-display/display-engine.h"
#include "src/graphics/display/lib/api-protocols/cpp/display-engine-events-fidl.h"
#include "src/graphics/display/lib/api-protocols/cpp/display-engine-fidl-adapter.h"

namespace amlogic_display {

// Driver instance that binds to the amlogic-display board device.
//
// This class is responsible for interfacing with the Fuchsia Driver Framework.
class DisplayDeviceDriver : public fdf::DriverBase {
 public:
  explicit DisplayDeviceDriver(fdf::DriverStartArgs start_args,
                               fdf::UnownedSynchronizedDispatcher driver_dispatcher);
  ~DisplayDeviceDriver() override = default;

  DisplayDeviceDriver(const DisplayDeviceDriver&) = delete;
  DisplayDeviceDriver(DisplayDeviceDriver&&) = delete;
  DisplayDeviceDriver& operator=(const DisplayDeviceDriver&) = delete;
  DisplayDeviceDriver& operator=(DisplayDeviceDriver&&) = delete;

  // fdf::DriverBase:
  zx::result<> Start() override;
  void Stop() override;

 private:
  // Creates a ComponentInspector that serves the `inspector` to the driver
  // component's Inspect sink.
  zx::result<std::unique_ptr<inspect::ComponentInspector>> CreateComponentInspector(
      inspect::Inspector inspector);

  std::unique_ptr<inspect::ComponentInspector> component_inspector_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> controller_;

  // Must outlive `display_engine_` and `engine_fidl_adapter_`.
  std::unique_ptr<display::DisplayEngineEventsFidl> engine_events_;

  // Must outlive `engine_fidl_adapter_`.
  std::unique_ptr<DisplayEngine> display_engine_;

  std::unique_ptr<display::DisplayEngineFidlAdapter> engine_fidl_adapter_;
};

}  // namespace amlogic_display

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_DISPLAY_DEVICE_DRIVER_H_
