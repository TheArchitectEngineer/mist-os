// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.power/cpp/fidl.h>
#include <lib/driver/power/cpp/element-description-builder.h>
#include <lib/zx/event.h>

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

namespace fdf_power {

ElementDesc ElementDescBuilder::Build() {
  ElementDesc to_return;
  to_return.element_config = element_config;
  to_return.tokens = std::move(tokens_);

  if (this->assertive_token_.has_value()) {
    to_return.assertive_token = std::move(this->assertive_token_.value());
  } else {
    // make an event instead
    zx::event::create(0, &to_return.assertive_token);
  }

  if (this->opportunistic_token_.has_value()) {
    to_return.opportunistic_token = std::move(this->opportunistic_token_.value());
  } else {
    // make an event instead
    zx::event::create(0, &to_return.opportunistic_token);
  }

  if (this->lessor_.has_value()) {
    to_return.lessor_server = std::move(this->lessor_.value());
  } else {
    // make a channel instead, include it in output
    fidl::Endpoints<fuchsia_power_broker::Lessor> endpoints =
        fidl::CreateEndpoints<fuchsia_power_broker::Lessor>().value();
    to_return.lessor_client = std::move(endpoints.client);
    to_return.lessor_server = std::move(endpoints.server);
  }

  if (this->element_control_.has_value()) {
    to_return.element_control_server = std::move(this->element_control_.value());
  } else {
    // make a channel instead, include it in output
    fidl::Endpoints<fuchsia_power_broker::ElementControl> endpoints =
        fidl::CreateEndpoints<fuchsia_power_broker::ElementControl>().value();
    to_return.element_control_client = std::move(endpoints.client);
    to_return.element_control_server = std::move(endpoints.server);
  }

  if (this->element_runner_.has_value()) {
    to_return.element_runner_client = std::move(this->element_runner_.value());
  } else {
    // Level control is deprecated, only use if element_runner is not supplied.
    fidl::ServerEnd<fuchsia_power_broker::RequiredLevel> required_level_server;
    std::optional<fidl::ClientEnd<fuchsia_power_broker::RequiredLevel>> required_level_client;

    if (this->required_level_.has_value()) {
      required_level_server = std::move(this->required_level_.value());
    } else {
      // make a channel instead, include it in output
      fidl::Endpoints<fuchsia_power_broker::RequiredLevel> endpoints =
          fidl::CreateEndpoints<fuchsia_power_broker::RequiredLevel>().value();
      required_level_server = std::move(endpoints.server);
      required_level_client = std::move(endpoints.client);
    }

    fidl::ServerEnd<fuchsia_power_broker::CurrentLevel> current_level_server;
    std::optional<fidl::ClientEnd<fuchsia_power_broker::CurrentLevel>> current_level_client;
    if (this->current_level_.has_value()) {
      current_level_server = std::move(this->current_level_.value());
    } else {
      // make a channel instead, include it in output
      fidl::Endpoints<fuchsia_power_broker::CurrentLevel> endpoints =
          fidl::CreateEndpoints<fuchsia_power_broker::CurrentLevel>().value();
      current_level_server = std::move(endpoints.server);
      current_level_client = std::move(endpoints.client);
    }

    to_return.level_control_servers =
        std::make_pair(std::move(current_level_server), std::move(required_level_server));
    to_return.current_level_client = std::move(current_level_client);
    to_return.required_level_client = std::move(required_level_client);
  }

  return to_return;
}

ElementDescBuilder& ElementDescBuilder::SetAssertiveToken(
    const zx::unowned_event& assertive_token) {
  zx::event dupe;
  assertive_token->duplicate(ZX_RIGHT_SAME_RIGHTS, &dupe);
  assertive_token_ = std::move(dupe);
  return *this;
}

ElementDescBuilder& ElementDescBuilder::SetOpportunisticToken(
    const zx::unowned_event& opportunistic_token) {
  zx::event dupe;
  opportunistic_token->duplicate(ZX_RIGHT_SAME_RIGHTS, &dupe);
  opportunistic_token_ = std::move(dupe);
  return *this;
}

ElementDescBuilder& ElementDescBuilder::SetCurrentLevel(
    fidl::ServerEnd<fuchsia_power_broker::CurrentLevel> current) {
  current_level_ = std::move(current);
  return *this;
}

ElementDescBuilder& ElementDescBuilder::SetRequiredLevel(
    fidl::ServerEnd<fuchsia_power_broker::RequiredLevel> required) {
  required_level_ = std::move(required);
  return *this;
}

ElementDescBuilder& ElementDescBuilder::SetLessor(
    fidl::ServerEnd<fuchsia_power_broker::Lessor> lessor) {
  lessor_ = std::move(lessor);
  return *this;
}

ElementDescBuilder& ElementDescBuilder::SetElementControl(
    fidl::ServerEnd<fuchsia_power_broker::ElementControl> element_control) {
  element_control_ = std::move(element_control);
  return *this;
}

ElementDescBuilder& ElementDescBuilder::SetElementRunner(
    fidl::ClientEnd<fuchsia_power_broker::ElementRunner> element_runner) {
  element_runner_ = std::move(element_runner);
  return *this;
}

}  // namespace fdf_power

#endif
