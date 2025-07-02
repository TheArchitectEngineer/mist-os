// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_POWER_MANAGER_H_
#define SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_POWER_MANAGER_H_

#include <fidl/fuchsia.ui.display.singleton/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/fit/function.h>
#include <lib/inspect/cpp/inspect.h>

#include "src/lib/fxl/macros.h"
#include "src/ui/scenic/lib/display/display_manager.h"

namespace scenic_impl::display {

// Implements the |fuchsia::ui::display::singleton::DisplayPower| protocol,
// Internal protocol clients are able to control the power of all available
// display devices through this protocol.
class DisplayPowerManager : public fidl::Server<fuchsia_ui_display_singleton::DisplayPower> {
 public:
  DisplayPowerManager(DisplayManager& display_manager, inspect::Node& parent_node);

  // |fuchsia::ui::display::singleton::DisplayPower|
  void SetDisplayPower(SetDisplayPowerRequest& request,
                       SetDisplayPowerCompleter::Sync& completer) override;
  void SetDisplayPower(bool power_on, fit::function<void(fit::result<zx_status_t>)> completer);

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_ui_display_singleton::DisplayPower> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    FX_LOGS(WARNING) << "Received an unknown method with ordinal " << metadata.method_ordinal;
  }

  fidl::ProtocolHandler<fuchsia_ui_display_singleton::DisplayPower> GetHandler() {
    return bindings_.CreateHandler(this, async_get_default_dispatcher(),
                                   fidl::kIgnoreBindingClosure);
  }

 private:
  DisplayManager& display_manager_;
  inspect::BoundedListNode inspect_display_power_events_;
  fidl::ServerBindingGroup<fuchsia_ui_display_singleton::DisplayPower> bindings_;

  FXL_DISALLOW_COPY_AND_ASSIGN(DisplayPowerManager);
};

}  // namespace scenic_impl::display

#endif  // SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_POWER_MANAGER_H_
