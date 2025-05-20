// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.runner/cpp/fidl.h>
#include <fidl/fuchsia.hardware.power/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/markers.h>
#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/executor.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/driver/power/cpp/element-description-builder.h>
#include <lib/driver/power/cpp/power-support.h>
#include <lib/driver/power/cpp/testing/fake_element_control.h>
#include <lib/driver/power/cpp/testing/fake_topology.h>
#include <lib/driver/power/cpp/testing/fidl_bound_server.h>
#include <lib/driver/power/cpp/testing/scoped_background_loop.h>
#include <lib/driver/power/cpp/wake-lease.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/fidl/cpp/wire/internal/transport_channel.h>
#include <lib/fidl/cpp/wire/status.h>
#include <lib/fidl/cpp/wire/string_view.h>
#include <lib/fidl/cpp/wire_natural_conversions.h>
#include <lib/fpromise/promise.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/inspector.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/inspect/cpp/vmo/types.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/zx/event.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls/object.h>
#include <zircon/time.h>

#include <optional>
#include <vector>

#include <fbl/ref_ptr.h>
#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/real_loop_fixture.h>
#include <src/storage/lib/vfs/cpp/pseudo_dir.h>
#include <src/storage/lib/vfs/cpp/service.h>
#include <src/storage/lib/vfs/cpp/synchronous_vfs.h>

#include "testing-common.h"

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

namespace power_lib_test {

using fdf_power::testing::FidlBoundServer;
using FakeTopology = FidlBoundServer<fdf_power::testing::FakeTopology>;
using FakeElementControl = FidlBoundServer<fdf_power::testing::FakeElementControl>;
using fuchsia_power_broker::ElementControl;

class PowerLibTest : public gtest::RealLoopFixture {};

class FakeTokenServer : public fidl::WireServer<fuchsia_hardware_power::PowerTokenProvider> {
 public:
  explicit FakeTokenServer(zx::event event) : event_(std::move(event)) {}
  void GetToken(GetTokenCompleter::Sync& completer) override {
    zx::event dupe;
    ASSERT_EQ(event_.duplicate(ZX_RIGHT_SAME_RIGHTS, &dupe), ZX_OK);
    completer.ReplySuccess(std::move(dupe));
  }
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_power::PowerTokenProvider> md,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_UNAVAILABLE);
  }
  ~FakeTokenServer() override = default;

 private:
  zx::event event_;
};

class AddInstanceResult {
 public:
  fbl::RefPtr<fs::Service> token_service;
  fbl::RefPtr<fs::PseudoDir> service_instance;
  std::shared_ptr<FakeTokenServer> token_handler;
  std::shared_ptr<zx::event> token;
};

class RequiredLevelServer : public fidl::Server<fuchsia_power_broker::RequiredLevel> {
 public:
  // Return one from the set of values until all are consumed. Then close the
  // channel and call |on_all_values_sent|
  explicit RequiredLevelServer(std::vector<uint8_t> values,
                               fit::function<void()> on_all_values_sent_)
      : values_(std::move(values)), on_all_values_sent_(std::move(on_all_values_sent_)) {}

  void Watch(WatchCompleter::Sync& completer) override {
    if (values_.size() == 0) {
      completer.Close(ZX_ERR_STOP);
      return;
    }

    uint8_t v = values_.front();
    values_.erase(values_.begin());

    completer.Reply(fit::success<fuchsia_power_broker::RequiredLevelWatchResponse>(v));
    if (values_.size() == 0) {
      on_all_values_sent_();
    }
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::RequiredLevel> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  std::vector<uint8_t> values_;
  fit::function<void()> on_all_values_sent_;
};

class CurrentLevelServer : public fidl::Server<fuchsia_power_broker::CurrentLevel> {
 public:
  /// Set the number of requests to serve before call |all_requests_received_|.
  /// Asserts if more requests than expected are received.
  explicit CurrentLevelServer(std::vector<uint8_t> expected_values,
                              fit::function<void()> all_requests_received_)
      : expected_(std::move(expected_values)),
        all_requests_received_(std::move(all_requests_received_)) {}
  void Update(UpdateRequest& request, UpdateCompleter::Sync& completer) override {
    request_count_++;
    ASSERT_LE(request_count_, expected_.size()) << "Received more requests than expected";

    ASSERT_EQ(request.current_level(), expected_.at(request_count_ - 1))
        << "Unexpected reported level";

    completer.Reply(fit::success());

    if (request_count_ == expected_.size()) {
      all_requests_received_();
      return;
    }
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::CurrentLevel> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  uint32_t request_count_ = 0;
  std::vector<uint8_t> expected_;
  fit::function<void()> all_requests_received_;
};

AddInstanceResult AddServiceInstance(
    async_dispatcher_t* dispatcher,
    fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider>* bindings) {
  zx::event raw_event;
  zx::event::create(0, &raw_event);
  std::shared_ptr<zx::event> event = std::make_shared<zx::event>(std::move(raw_event));

  zx::event event_copy;
  event->duplicate(ZX_RIGHT_SAME_RIGHTS, &event_copy);
  std::shared_ptr<FakeTokenServer> token_handler =
      std::make_shared<FakeTokenServer>(std::move(event_copy));
  fbl::RefPtr<fs::PseudoDir> service_instance = fbl::MakeRefCounted<fs::PseudoDir>();
  fbl::RefPtr<fs::Service> token_server = fbl::MakeRefCounted<fs::Service>(
      [token_handler, dispatcher,
       bindings](fidl::ServerEnd<fuchsia_hardware_power::PowerTokenProvider> chan) {
        bindings->AddBinding(dispatcher, std::move(chan), token_handler.get(),
                             fidl::kIgnoreBindingClosure);
        return ZX_OK;
      });

  // Build up the directory structure so that we have an entry for the service
  // and inside that an entry for the instance.
  service_instance->AddEntry(fuchsia_hardware_power::PowerTokenService::TokenProvider::Name,
                             token_server);
  return AddInstanceResult{
      .token_service = std::move(token_server),
      .service_instance = std::move(service_instance),
      .token_handler = std::move(token_handler),
      .token = std::move(event),
  };
}

TEST_F(PowerLibTest, TestLeaseHelper) {
  auto realm_builder = component_testing::RealmBuilder::Create();
  realm_builder.AddChild("power-broker", "#meta/power-broker.cm");

  realm_builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Protocol{"fuchsia.power.broker.Topology"}},
      .source = component_testing::ChildRef{"power-broker"},
      .targets = {component_testing::ParentRef{}},
  });

  auto root = realm_builder.Build(async_get_default_dispatcher());

  fidl::Endpoints<fuchsia_power_broker::Topology> power_broker =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();
  zx_status_t connect_result =
      root.component().Connect("fuchsia.power.broker.Topology", power_broker.server.TakeChannel());
  ASSERT_EQ(ZX_OK, connect_result);

  fdf_power::PowerElementConfiguration config{
      .element{
          .name = "parent_element",
          .levels =
              {
                  {{.level = 0, .name = "zero"}, {.level = 1, .name = "one"}},
              },
      },
  };

  fdf_power::ElementDescBuilder element_builder(config, fdf_power::TokenMap());
  fdf_power::ElementDesc description = element_builder.Build();
  auto result = fdf_power::AddElement(power_broker.client, description);

  ASSERT_FALSE(result.is_error()) << "Error value" << static_cast<uint32_t>(result.error_value());

  bool level_rose = false;
  bool lease_acquired = false;

  // Now run the created element with an element runner
  fdf_power::ElementRunner element_runner(
      "test element", std::move(description.required_level_client.value()),
      std::move(description.current_level_client.value()),
      [level_rose = &level_rose, lease_acquired = &lease_acquired,
       quit = QuitLoopClosure()](uint8_t new_level) {
        if (new_level == 1) {
          *level_rose = true;
        }

        if (*level_rose && *lease_acquired) {
          quit();
        }
        return fit::success(new_level);
      },
      [&](fdf_power::ElementRunnerError e) {
        ASSERT_TRUE(false) << "Unexpected error: " << static_cast<uint32_t>(e);
      },
      async_get_default_dispatcher());
  element_runner.RunPowerElement();

  std::vector<fdf_power::LeaseDependency> deps;
  deps.push_back(fdf_power::LeaseDependency{
      .levels_by_preference = {1},
      .token = std::move(description.assertive_token),
      .type = fuchsia_power_broker::DependencyType::kAssertive,
  });

  // Now, let's create a direct lease on the element we created
  fit::result<std::tuple<fidl::Status, std::optional<fuchsia_power_broker::AddElementError>>,
              std::unique_ptr<fdf_power::LeaseHelper>>
      creation_result = fdf_power::CreateLeaseHelper(
          power_broker.client, std::move(deps), "test_lease", async_get_default_dispatcher(),
          []() { ASSERT_TRUE(false) << "error callback triggered unexpectedly"; }, nullptr);
  ASSERT_FALSE(creation_result.is_error());

  fidl::ClientEnd<fuchsia_power_broker::LeaseControl> lease_ctl;
  std::unique_ptr<fdf_power::LeaseHelper> lease = std::move(creation_result.value());
  lease->AcquireLease(
      [&lease_ctl, level_rose = &level_rose, lease_acquired = &lease_acquired,
       quit = QuitLoopClosure()](fidl::Result<fuchsia_power_broker::Lessor::Lease>& lease) {
        ASSERT_FALSE(lease.is_error());
        *lease_acquired = true;
        if (*level_rose && *lease_acquired) {
          quit();
        }
        lease_ctl = std::move(lease->lease_control());
      });

  RunLoop();
}

/// Tries to create a direct lease, but passes in a closed channel. We then
/// expect CreateLeaseHelper to return an appropriate error.
TEST_F(PowerLibTest, TestCreateLeaseHelperWithClosedChannel) {
  fidl::Endpoints<fuchsia_power_broker::Topology> power_broker =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();
  power_broker.server.TakeChannel().reset();

  zx::event not_registered;
  zx::event::create(0, &not_registered);

  std::vector<fdf_power::LeaseDependency> deps;
  deps.push_back(fdf_power::LeaseDependency{
      .levels_by_preference = {1},
      .token = std::move(not_registered),
      .type = fuchsia_power_broker::DependencyType::kAssertive,
  });

  fit::result<std::tuple<fidl::Status, std::optional<fuchsia_power_broker::AddElementError>>,
              std::unique_ptr<fdf_power::LeaseHelper>>
      creation_result = fdf_power::CreateLeaseHelper(
          power_broker.client, std::move(deps), "test_lease", async_get_default_dispatcher(),
          []() { ASSERT_TRUE(false) << "error callback triggered unexpectedly"; }, nullptr);
  ASSERT_TRUE(creation_result.is_error());
  std::tuple<fidl::Status, std::optional<fuchsia_power_broker::AddElementError>> error_val =
      creation_result.error_value();
  ASSERT_EQ(std::get<0>(error_val).status(), ZX_ERR_PEER_CLOSED);
  ASSERT_EQ(std::get<1>(error_val), std::nullopt);
}

/// Tries to create a direct lease, but does not register the event token that
/// the lease depends on. We expect Power Broker to return an error and then
/// that CreateLeaseHelper returns the expected error.
TEST_F(PowerLibTest, TestCreateLeaseHelperWithInvalidToken) {
  auto realm_builder = component_testing::RealmBuilder::Create();
  realm_builder.AddChild("power-broker", "#meta/power-broker.cm");

  realm_builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Protocol{"fuchsia.power.broker.Topology"}},
      .source = component_testing::ChildRef{"power-broker"},
      .targets = {component_testing::ParentRef{}},
  });

  auto root = realm_builder.Build(async_get_default_dispatcher());

  fidl::Endpoints<fuchsia_power_broker::Topology> power_broker =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();
  zx_status_t connect_result =
      root.component().Connect("fuchsia.power.broker.Topology", power_broker.server.TakeChannel());
  ASSERT_EQ(ZX_OK, connect_result);

  zx::event not_registered;
  zx::event::create(0, &not_registered);

  std::vector<fdf_power::LeaseDependency> deps;
  deps.push_back(fdf_power::LeaseDependency{
      .levels_by_preference = {1},
      .token = std::move(not_registered),
      .type = fuchsia_power_broker::DependencyType::kAssertive,
  });

  // Now, let's create a direct lease on the element we created
  fit::result<std::tuple<fidl::Status, std::optional<fuchsia_power_broker::AddElementError>>,
              std::unique_ptr<fdf_power::LeaseHelper>>
      creation_result = fdf_power::CreateLeaseHelper(
          power_broker.client, std::move(deps), "test_lease", async_get_default_dispatcher(),
          []() { ASSERT_TRUE(false) << "error callback triggered unexpectedly"; }, nullptr);
  ASSERT_TRUE(creation_result.is_error());
  std::tuple<fidl::Status, std::optional<fuchsia_power_broker::AddElementError>> error_val =
      creation_result.error_value();
  ASSERT_EQ(std::get<0>(error_val).status(), fidl::Status::Ok().status());
  ASSERT_EQ(std::get<1>(error_val), fuchsia_power_broker::AddElementError::kNotAuthorized);
}

/// Tests the ElementRunner by sending it two required values and then expects
/// those values to be reported as current levels.
TEST_F(PowerLibTest, TestElementRunner) {
  // Make the current and required levels channels
  fidl::Endpoints<fuchsia_power_broker::RequiredLevel> required_level_channel =
      fidl::Endpoints<fuchsia_power_broker::RequiredLevel>::Create();

  fidl::Endpoints<fuchsia_power_broker::CurrentLevel> current_level_channel =
      fidl::Endpoints<fuchsia_power_broker::CurrentLevel>::Create();

  // Set up flags so we verify that all checks are done before we exit.
  bool cls_done = false;
  bool rls_done = false;
  bool runner_done = false;

  inspect::Inspector general = inspect::Inspector();

  fit::function<void(fdf_power::ElementRunnerError)> completion_handler =
      [run_done = &runner_done, cls_done = &cls_done, rls_done = &rls_done,
       quit_loop = QuitLoopClosure()](fdf_power::ElementRunnerError err) {
        *run_done = true;
        if (*run_done && *cls_done && *rls_done) {
          quit_loop();
        }
      };

  // These are the current level values we expect inspect to have during level
  // change callbacks.
  const std::vector<uint8_t> current_levels{0, 2};
  // These are teh required lievel values we expect to see in inspect during
  // level change callbacks.
  const std::vector<uint8_t> required_levels{2, 3};
  uint8_t request_count = 0;

  std::unique_ptr<fdf_power::ElementRunner> runner = std::make_unique<fdf_power::ElementRunner>(
      "test element", std::move(required_level_channel.client),
      std::move(current_level_channel.client),
      [general = &general, loop = this, &request_count, &required_levels,
       &current_levels](uint8_t new_level) {
        // Check that the level reported in inspect is initially zero
        fpromise::result<inspect::Hierarchy> inspect_tree =
            loop->RunPromise(inspect::ReadFromInspector(*general));
        EXPECT_TRUE(inspect_tree.is_ok());

        auto* req_level =
            inspect_tree.value().node().get_property<inspect::UintPropertyValue>("required_level");
        EXPECT_EQ(req_level->value(), required_levels[request_count]);

        auto* current_level =
            inspect_tree.value().node().get_property<inspect::UintPropertyValue>("current_level");
        EXPECT_EQ(current_level->value(), current_levels[request_count]);

        request_count++;
        return fit::success(new_level);
      },
      std::move(completion_handler), dispatcher(), &general.GetRoot());
  runner->RunPowerElement();

  // Check that the level reported in inspect is initially zero
  fpromise::result<inspect::Hierarchy> inspect_tree =
      RunPromise(inspect::ReadFromInspector(general));
  ASSERT_TRUE(inspect_tree.is_ok());
  auto* req_level =
      inspect_tree.value().node().get_property<inspect::UintPropertyValue>("required_level");
  ASSERT_EQ(req_level->value(), 0u);
  auto* current_level =
      inspect_tree.value().node().get_property<inspect::UintPropertyValue>("current_level");
  ASSERT_EQ(current_level->value(), 0u);
  auto* name =
      inspect_tree.value().node().get_property<inspect::StringPropertyValue>("element_name");
  ASSERT_EQ(name->value(), "test element");

  std::unique_ptr<RequiredLevelServer> rls = std::make_unique<RequiredLevelServer>(
      required_levels, [run_done = &runner_done, cls_done = &cls_done, rls_done = &rls_done,
                        quit_loop = QuitLoopClosure()]() {
        *rls_done = true;
        if (*run_done && *cls_done && *rls_done) {
          quit_loop();
        }
      });
  fidl::ServerBindingRef<fuchsia_power_broker::RequiredLevel> required_level_binding =
      fidl::BindServer<fuchsia_power_broker::RequiredLevel>(
          dispatcher(), std::move(required_level_channel.server), std::move(rls),
          [](RequiredLevelServer* imp, fidl::UnbindInfo info,
             fidl::ServerEnd<fuchsia_power_broker::RequiredLevel> channel) {});

  std::unique_ptr<CurrentLevelServer> cls = std::make_unique<CurrentLevelServer>(
      required_levels, [run_done = &runner_done, cls_done = &cls_done, rls_done = &rls_done,
                        quit_loop = QuitLoopClosure()]() {
        *cls_done = true;
        if (*run_done && *cls_done && *rls_done) {
          quit_loop();
        }
      });
  fidl::ServerBindingRef<fuchsia_power_broker::CurrentLevel> current_level_binding =
      fidl::BindServer<fuchsia_power_broker::CurrentLevel>(
          dispatcher(), std::move(current_level_channel.server), std::move(cls),
          [](CurrentLevelServer* impl, fidl::UnbindInfo info,
             fidl::ServerEnd<fuchsia_power_broker::CurrentLevel> channel) {});

  RunLoop();
  // Check that the level reported in inspect rose
  inspect_tree = RunPromise(inspect::ReadFromInspector(general));
  ASSERT_TRUE(inspect_tree.is_ok());
  req_level =
      inspect_tree.value().node().get_property<inspect::UintPropertyValue>("required_level");
  ASSERT_EQ(req_level->value(), 3u);
  current_level =
      inspect_tree.value().node().get_property<inspect::UintPropertyValue>("current_level");
  ASSERT_EQ(current_level->value(), 3u);
}

/// Test calling |ElementRunner.SetLevel| and expect the value to be received
/// by the faked out |CurrentLevel| server and the result callback called on
/// the client side.
TEST_F(PowerLibTest, TestElementRunnerManualSet) {
  fidl::Endpoints<fuchsia_power_broker::RequiredLevel> required_level_channel =
      fidl::Endpoints<fuchsia_power_broker::RequiredLevel>::Create();

  fidl::Endpoints<fuchsia_power_broker::CurrentLevel> current_level_channel =
      fidl::Endpoints<fuchsia_power_broker::CurrentLevel>::Create();

  // Set up the required level servers
  // For this test we don't want to ask the client to change any levels
  std::vector<uint8_t> requested_levels{};
  std::unique_ptr<RequiredLevelServer> required_level_server =
      std::make_unique<RequiredLevelServer>(std::move(requested_levels), []() {});

  // Create flags to make sure both necessary test checks are run.
  bool current_level_received = false;
  bool set_response_received = false;

  fidl::ServerBindingRef<fuchsia_power_broker::RequiredLevel> required_level_binding =
      fidl::BindServer<fuchsia_power_broker::RequiredLevel>(
          dispatcher(), std::move(required_level_channel.server), std::move(required_level_server),
          [](RequiredLevelServer* impl, fidl::UnbindInfo unbind_info,
             fidl::ServerEnd<fuchsia_power_broker::RequiredLevel> server_chan) {});

  const uint8_t LEVEL_TO_SET = 6;

  std::unique_ptr<CurrentLevelServer> current_level_server = std::make_unique<CurrentLevelServer>(
      std::vector{LEVEL_TO_SET},
      [set_response_received = &set_response_received,
       current_level_received = &current_level_received, quit = QuitLoopClosure()]() {
        *current_level_received = true;
        if (*set_response_received && *current_level_received) {
          quit();
        }
      });
  fidl::ServerBindingRef<fuchsia_power_broker::CurrentLevel> current_level_binding =
      fidl::BindServer<fuchsia_power_broker::CurrentLevel>(
          dispatcher(), std::move(current_level_channel.server), std::move(current_level_server),
          [](CurrentLevelServer* impl, fidl::UnbindInfo info,
             fidl::ServerEnd<fuchsia_power_broker::CurrentLevel> current_level_channel) {});

  std::unique_ptr<fdf_power::ElementRunner> runner = std::make_unique<fdf_power::ElementRunner>(
      "test element", std::move(required_level_channel.client),
      std::move(current_level_channel.client), fdf_power::default_level_changer,
      [](fdf_power::ElementRunnerError err) {}, dispatcher());
  runner->RunPowerElement();

  // Set the level manually
  runner->SetLevel(
      LEVEL_TO_SET,
      [set_response_received = &set_response_received,
       current_level_received = &current_level_received, quit = QuitLoopClosure()](
          fit::result<fidl::ErrorsIn<fuchsia_power_broker::CurrentLevel::Update>, zx_status_t>
              result) {
        fit::result<fidl::ErrorsIn<fuchsia_power_broker::CurrentLevel::Update>, zx_status_t>
            expected = fit::ok(ZX_OK);
        ASSERT_EQ(result, expected) << result.error_value().FormatDescription();
        *set_response_received = true;
        if (*set_response_received && *current_level_received) {
          quit();
        }
      });

  RunLoop();
}

/// Add an element which has no dependencies
TEST_F(PowerLibTest, AddElementNoDep) {
  // Create the dependency configuration and create a
  // map<parent_name, vec<level_deps> used for call validation later.
  std::string parent_name = "element_first_parent";
  fdf_power::PowerLevel one = {.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two = {.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three = {.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe = {
      .name = "the_element",
      .levels = {one, two, three},
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe, .dependencies = {}};

  std::unordered_map<fdf_power::ParentElement, zx::event> tokens;

  // Make the fake power broker
  fdf_power::testing::ScopedBackgroundLoop loop;
  fidl::Endpoints<fuchsia_power_broker::Topology> endpoints =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();
  FakeTopology fake_power_broker(loop.dispatcher(), std::move(endpoints.server));
  std::unique_ptr<FakeElementControl> fake_element_control(nullptr);
  loop.executor().schedule_task(fake_power_broker.TakeSchemaPromise().then(
      [&loop,
       &fake_element_control](fpromise::result<fuchsia_power_broker::ElementSchema, void>& result) {
        EXPECT_TRUE(result.is_ok());
        EXPECT_TRUE(result.value().dependencies().has_value());
        ASSERT_EQ(result.value().dependencies()->size(), static_cast<size_t>(0));

        std::optional<fidl::ServerEnd<ElementControl>>& ec = result.value().element_control();
        EXPECT_TRUE(ec.has_value());
        fake_element_control =
            std::make_unique<FakeElementControl>(loop.dispatcher(), std::move(ec.value()));
      }));

  // Call add element
  fidl::Endpoints<fuchsia_power_broker::ElementControl> element_control =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  zx::event invalid1, invalid2;
  auto call_result =
      fdf_power::AddElement(endpoints.client, df_config, std::move(tokens), invalid1.borrow(),
                            invalid2.borrow(), std::nullopt, std::nullopt,
                            std::move(element_control.server), element_control.client.borrow());
  ASSERT_TRUE(call_result.is_ok());

  RunLoopUntil([&fake_element_control] { return fake_element_control != nullptr; });
}

/// Add an element which has a has multiple level dependencies on a single
/// parent element. Verifies the dependency token is correct.
TEST_F(PowerLibTest, AddElementSingleDep) {
  // Create the dependency configuration and create a
  // map<parent_name, vec<level_deps> used for call validation later.
  std::string parent_name = "element_first_parent";
  fdf_power::PowerLevel one = {.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two = {.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three = {.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  fdf_power::LevelTuple one_to_one{
      .child_level = 1,
      .parent_level = 1,
  };
  fdf_power::LevelTuple three_to_two{
      .child_level = 3,
      .parent_level = 2,
  };

  fdf_power::PowerDependency power_dep{
      .child = "n/a",
      .parent = fdf_power::ParentElement::WithInstanceName(parent_name),
      .level_deps = {one_to_one, three_to_two},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe, .dependencies = {power_dep}};

  zx::event parent_token;
  ASSERT_EQ(ZX_OK, zx::event::create(0, &parent_token));

  // map of level dependencies we'll use later for validation
  std::unordered_map<uint8_t, uint8_t> child_to_parent_levels{
      {one_to_one.child_level, one_to_one.parent_level},
      {three_to_two.child_level, three_to_two.parent_level}};

  // Create the map of dependency names to zx::event tokens
  // Make a copy of the token
  zx::event token_copy;
  ASSERT_EQ(ZX_OK, parent_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &token_copy));
  std::unordered_map<fdf_power::ParentElement, zx::event> tokens;
  tokens.insert(std::make_pair(fdf_power::ParentElement::WithInstanceName(parent_name),
                               std::move(token_copy)));

  // Make the fake power broker
  fdf_power::testing::ScopedBackgroundLoop loop;
  fidl::Endpoints<fuchsia_power_broker::Topology> endpoints =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();
  std::unique_ptr<FakeElementControl> fake_element_control(nullptr);
  FakeTopology fake_power_broker(loop.dispatcher(), std::move(endpoints.server));
  loop.executor().schedule_task(fake_power_broker.TakeSchemaPromise().then(
      [parent_token = std::move(parent_token),
       child_to_parent_levels = std::move(child_to_parent_levels), &loop, &fake_element_control](
          fpromise::result<fuchsia_power_broker::ElementSchema, void>& result) mutable {
        EXPECT_TRUE(result.is_ok());
        EXPECT_TRUE(result.value().dependencies().has_value());
        ASSERT_EQ(result.value().dependencies()->size(), static_cast<size_t>(2));

        std::optional<fidl::ServerEnd<ElementControl>>& ec = result.value().element_control();
        EXPECT_TRUE(ec.has_value());
        fake_element_control =
            std::make_unique<FakeElementControl>(loop.dispatcher(), std::move(ec.value()));

        // Since both power levels dependended on the same parent power element
        // that both access tokens match the one we made in the test.
        zx_info_handle_basic_t orig_info, copy_info;
        parent_token.get_info(ZX_INFO_HANDLE_BASIC, &orig_info, sizeof(zx_info_handle_basic_t),
                              nullptr, nullptr);
        for (fuchsia_power_broker::LevelDependency& dep : result.value().dependencies().value()) {
          dep.requires_token().get_info(ZX_INFO_HANDLE_BASIC, &copy_info,
                                        sizeof(zx_info_handle_basic_t), nullptr, nullptr);
          auto entry = child_to_parent_levels.extract(dep.dependent_level());
          ASSERT_EQ(entry.mapped(), dep.requires_level_by_preference().front());
          ASSERT_EQ(copy_info.koid, orig_info.koid);
        }
        ASSERT_EQ(child_to_parent_levels.size(), static_cast<size_t>(0));
      }));

  // Call add element
  zx::event invalid1, invalid2;
  fidl::Endpoints<fuchsia_power_broker::ElementControl> element_control =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto call_result =
      fdf_power::AddElement(endpoints.client, df_config, std::move(tokens), invalid1.borrow(),
                            invalid2.borrow(), std::nullopt, std::nullopt,
                            std::move(element_control.server), element_control.client.borrow());
  ASSERT_TRUE(call_result.is_ok());

  RunLoopUntil([&fake_element_control] { return fake_element_control != nullptr; });
}

/// Add an element that has dependencies on two different parent elements.
/// Validates that dependency tokens are the right ones.
TEST_F(PowerLibTest, AddElementDoubleDep) {
  // Create the dependency configuration and create a
  // map<parent_name, vec<level_deps> used for call validation later.
  fdf_power::ParentElement parent_name_first =
      fdf_power::ParentElement::WithInstanceName("element_first_parent");
  fdf_power::ParentElement parent_name_second =
      fdf_power::ParentElement::WithInstanceName("element_second_parent");
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe = {
      .name = "the_element",
      .levels = {one, two, three},
  };

  uint8_t dep_one_level = 1;
  fdf_power::LevelTuple one_to_one{
      .child_level = dep_one_level,
      .parent_level = 1,
  };

  fdf_power::PowerDependency power_dep_one{
      .child = "n/a",
      .parent = parent_name_first,
      .level_deps = {one_to_one},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  uint8_t dep_two_level = 3;
  fdf_power::LevelTuple three_to_two{
      .child_level = dep_two_level,
      .parent_level = 2,
  };

  fdf_power::PowerDependency power_dep_two{
      .child = "n/a",
      .parent = parent_name_second,
      .level_deps = {three_to_two},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config = {.element = pe,
                                                    .dependencies = {power_dep_one, power_dep_two}};

  zx::event parent_token_one;
  ASSERT_EQ(ZX_OK, zx::event::create(0, &parent_token_one));

  zx::event parent_token_two;
  ASSERT_EQ(ZX_OK, zx::event::create(0, &parent_token_two));

  // map of level dependencies we'll use later for validation
  std::unordered_map<uint8_t, uint8_t> child_to_parent_levels{
      {one_to_one.child_level, one_to_one.parent_level},
      {three_to_two.child_level, three_to_two.parent_level}};

  // Create the map of dependency names to zx::event tokens
  // Make a copy of the token
  std::unordered_map<fdf_power::ParentElement, zx::event> tokens;
  {
    zx::event token_one_copy;
    ASSERT_EQ(ZX_OK, parent_token_one.duplicate(ZX_RIGHT_SAME_RIGHTS, &token_one_copy));

    zx::event token_two_copy;
    ASSERT_EQ(ZX_OK, parent_token_two.duplicate(ZX_RIGHT_SAME_RIGHTS, &token_two_copy));

    tokens.insert(std::make_pair(parent_name_first, std::move(token_one_copy)));
    tokens.insert(std::make_pair(parent_name_second, std::move(token_two_copy)));
  }

  // Make the fake power broker
  fdf_power::testing::ScopedBackgroundLoop loop;
  fidl::Endpoints<fuchsia_power_broker::Topology> endpoints =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();
  std::unique_ptr<FakeElementControl> fake_element_control(nullptr);
  FakeTopology fake_power_broker(loop.dispatcher(), std::move(endpoints.server));
  loop.executor().schedule_task(fake_power_broker.TakeSchemaPromise().then(
      [parent_token_one = std::move(parent_token_one),
       parent_token_two = std::move(parent_token_two),
       child_to_parent_levels = std::move(child_to_parent_levels), dep_one_level = dep_one_level,
       &loop, &fake_element_control](
          fpromise::result<fuchsia_power_broker::ElementSchema, void>& result) mutable {
        EXPECT_TRUE(result.is_ok());
        EXPECT_TRUE(result.value().dependencies().has_value());
        ASSERT_EQ(result.value().dependencies()->size(), static_cast<size_t>(2));

        std::optional<fidl::ServerEnd<ElementControl>>& ec = result.value().element_control();
        EXPECT_TRUE(ec.has_value());
        fake_element_control =
            std::make_unique<FakeElementControl>(loop.dispatcher(), std::move(ec.value()));

        // Since both power levels dependended on the same parent power element
        // that both access tokens match the one we made in the test
        zx_info_handle_basic_t parent_one_info, parent_two_info, copy_info;
        parent_token_one.get_info(ZX_INFO_HANDLE_BASIC, &parent_one_info,
                                  sizeof(zx_info_handle_basic_t), nullptr, nullptr);
        parent_token_two.get_info(ZX_INFO_HANDLE_BASIC, &parent_two_info,
                                  sizeof(zx_info_handle_basic_t), nullptr, nullptr);
        for (fuchsia_power_broker::LevelDependency& dep : result.value().dependencies().value()) {
          dep.requires_token().get_info(ZX_INFO_HANDLE_BASIC, &copy_info,
                                        sizeof(zx_info_handle_basic_t), nullptr, nullptr);
          // Since each dependency has a different dependent level, use the dependent
          // level to differentiate which access token to check against. Delightfully
          // basic since we know we only have two dependencies.
          if (dep.dependent_level() == dep_one_level) {
            ASSERT_EQ(copy_info.koid, parent_one_info.koid);
          } else {
            ASSERT_EQ(copy_info.koid, parent_two_info.koid);
          }
          auto entry = child_to_parent_levels.extract(dep.dependent_level());
          ASSERT_EQ(entry.mapped(), dep.requires_level_by_preference().front());
        }

        ASSERT_EQ(child_to_parent_levels.size(), static_cast<size_t>(0));
      }));

  // Call add element
  zx::event invalid1, invalid2;
  fidl::Endpoints<fuchsia_power_broker::ElementControl> element_control =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto call_result =
      fdf_power::AddElement(endpoints.client, df_config, std::move(tokens), invalid1.borrow(),
                            invalid2.borrow(), std::nullopt, std::nullopt,
                            std::move(element_control.server), element_control.client.borrow());
  ASSERT_TRUE(call_result.is_ok());

  RunLoopUntil([&fake_element_control] { return fake_element_control != nullptr; });
}

/// Check that a power element with two levels, dependent on two different
/// parent levels is converted correctly from configuration format to the
/// format used by Power Framework.
TEST_F(PowerLibTest, LevelDependencyWithSingleParent) {
  fdf_power::ParentElement parent =
      fdf_power::ParentElement::WithInstanceName("element_first_parent");
  fdf_power::PowerLevel one = {.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two = {.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three = {.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  fdf_power::LevelTuple one_to_one{
      .child_level = 1,
      .parent_level = 1,
  };
  fdf_power::LevelTuple three_to_two{
      .child_level = 3,
      .parent_level = 2,
  };

  std::unordered_map<uint8_t, uint8_t> child_to_parent_levels{
      {one_to_one.child_level, one_to_one.parent_level},
      {three_to_two.child_level, three_to_two.parent_level}};

  fdf_power::PowerDependency power_dep{
      .child = "n/a",
      .parent = parent,
      .level_deps = {one_to_one, three_to_two},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config = {.element{pe}, .dependencies = {power_dep}};

  auto output = fdf_power::LevelDependencyFromConfig(df_config).value();

  // we expect that "element_first_parent" will have two entries
  // one for each of the level deps we've expressed
  std::vector<fuchsia_power_broker::LevelDependency>& deps = output[parent];
  ASSERT_EQ(static_cast<size_t>(2), deps.size());

  // Check that the translated dependencies match the ones we put in
  for (auto& dep : deps) {
    ASSERT_EQ(dep.dependency_type(), fuchsia_power_broker::DependencyType::kAssertive);
    uint8_t parent_level =
        static_cast<uint8_t>(child_to_parent_levels.extract(dep.dependent_level()).mapped());
    ASSERT_EQ(dep.requires_level_by_preference().front(), parent_level);
  }

  // Check that we took out all the mappings
  ASSERT_EQ(child_to_parent_levels.size(), static_cast<size_t>(0));
}

/// Check that power levels are take out of the driver config format correctly.
TEST_F(PowerLibTest, ExtractPowerLevelsFromConfig) {
  fdf_power::ParentElement parent =
      fdf_power::ParentElement::WithInstanceName("element_first_parent");
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 0, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 0, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  fdf_power::PowerDependency power_dep{
      .child = "n/a",
      .parent = parent,
      .level_deps = {},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe, .dependencies = {power_dep}};

  auto converted = fdf_power::PowerLevelsFromConfig(df_config);

  ASSERT_EQ(static_cast<size_t>(3), converted.size());
}

/// Get the dependency tokens for an element that has no dependencies.
/// This should result in no tokens and no errors.
TEST_F(PowerLibTest, GetTokensNoTokens) {
  fdf_power::testing::ScopedBackgroundLoop loop;

  // create a namespace that has a directory for the PowerTokenService, but
  // no entries
  fbl::RefPtr<fs::PseudoDir> empty_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  fs::SynchronousVfs vfs(loop.dispatcher());
  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  vfs.ServeDirectory(std::move(empty_dir), std::move(dir_endpoints.server));

  fdf_power::PowerLevel one{.level = 1, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 2, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 3, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  // Specify no dependencies
  fdf_power::PowerElementConfiguration df_config{.element = pe, .dependencies = {}};

  fit::result<fdf_power::Error, std::unordered_map<fdf_power::ParentElement, zx::event>> result =
      fdf_power::GetDependencyTokens(df_config, std::move(dir_endpoints.client));

  // Should be no tokens, but also no errors
  ASSERT_EQ(result.value().size(), static_cast<size_t>(0));
}

/// Get the tokens for an element that has one level dependent on one other
/// power element.
TEST_F(PowerLibTest, GetTokensOneDepOneLevel) {
  std::string parent_name = "parentOne";
  fdf_power::ParentElement parent = fdf_power::ParentElement::WithInstanceName(parent_name);
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();
  fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider> bindings;

  fbl::RefPtr<fs::PseudoDir> power_token_service = fbl::MakeRefCounted<fs::PseudoDir>();

  AddInstanceResult instance_data = AddServiceInstance(loop.dispatcher(), &bindings);

  fbl::RefPtr<fs::PseudoDir> svc_instances_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  ASSERT_EQ(svc_instances_dir->AddEntry(parent_name, instance_data.service_instance), ZX_OK);
  ASSERT_EQ(power_token_service->AddEntry(fuchsia_hardware_power::PowerTokenService::Name,
                                          svc_instances_dir),
            ZX_OK);

  fs::SynchronousVfs vfs(loop.dispatcher());
  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  vfs.ServeDirectory(std::move(power_token_service), std::move(dir_endpoints.server));

  // Specify the power element
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  // Create a dependency between a level on this element and a level on the
  // parent
  fdf_power::PowerDependency power_dep{
      .child = "n/a",
      .parent = parent,
      .level_deps =
          {
              {
                  .child_level = 1,
                  .parent_level = 2,
              },
          },
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config = {.element = pe, .dependencies = {power_dep}};

  // With the given configuration, get the dependency tokens. We expect this to
  // call into the PowerTokenProvider we built above which calls into our
  // FakeTokenServer instance.
  fit::result<fdf_power::Error, fdf_power::TokenMap> result =
      fdf_power::GetDependencyTokens(df_config, std::move(dir_endpoints.client));

  ASSERT_TRUE(result.is_ok());
  fdf_power::TokenMap token_map = std::move(result.value());
  ASSERT_EQ(token_map.size(), static_cast<size_t>(1));

  // Check that the zx::event defined in the test and the one return by
  // `GetTokens` are pointing at the same kernel object
  zx_info_handle_basic_t info1, info2;
  instance_data.token->get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t),
                                nullptr, nullptr);
  token_map.begin()->second.get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t),
                                     nullptr, nullptr);
  ASSERT_EQ(info1.koid, info2.koid);

  loop.Shutdown();
  loop.JoinThreads();
}

/// Get tokens for a power element which two levels that have dependencies on
/// the same parent element.
TEST_F(PowerLibTest, GetTokensOneDepTwoLevels) {
  std::string parent_name = "parentOne";
  fdf_power::ParentElement parent = fdf_power::ParentElement::WithInstanceName(parent_name);
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();
  fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider> bindings;

  fbl::RefPtr<fs::PseudoDir> namespace_svc_dir = fbl::MakeRefCounted<fs::PseudoDir>();

  fbl::RefPtr<fs::PseudoDir> svc_instances_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  AddInstanceResult instance_data = AddServiceInstance(loop.dispatcher(), &bindings);
  ASSERT_EQ(svc_instances_dir->AddEntry(parent_name, instance_data.service_instance), ZX_OK);

  ASSERT_EQ(namespace_svc_dir->AddEntry(fuchsia_hardware_power::PowerTokenService::Name,
                                        svc_instances_dir),
            ZX_OK);

  fs::SynchronousVfs vfs(loop.dispatcher());
  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  vfs.ServeDirectory(std::move(namespace_svc_dir), std::move(dir_endpoints.server));

  // Specify the power element
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  // Create a dependency between a level on this element and a level on the
  // parent
  fdf_power::PowerDependency power_dep{
      .child = "n/a",
      .parent = parent,
      .level_deps =
          {
              {
                  .child_level = 1,
                  .parent_level = 2,
              },
              {
                  .child_level = 2,
                  .parent_level = 4,
              },
          },
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe, .dependencies = {power_dep}};

  // With the given configuration, get the dependency tokens. We expect this to
  // call into the PowerTokenProvider we built above which calls into our
  // FakeTokenServer instance.
  fit::result<fdf_power::Error, fdf_power::TokenMap> result =
      fdf_power::GetDependencyTokens(df_config, std::move(dir_endpoints.client));

  ASSERT_TRUE(result.is_ok());
  fdf_power::TokenMap token_map = std::move(result.value());
  ASSERT_EQ(token_map.size(), static_cast<size_t>(1));

  // Check that the zx::event defined in the test and the one return by
  // `GetTokens` are pointing at the same kernel object
  zx_info_handle_basic_t info1, info2;
  instance_data.token->get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t),
                                nullptr, nullptr);
  token_map.begin()->second.get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t),
                                     nullptr, nullptr);
  ASSERT_EQ(info1.koid, info2.koid);

  loop.Shutdown();
  loop.JoinThreads();
}

/// Check GetTokens against an element which has two levels, each of which
/// has a dependency on two different parents.
TEST_F(PowerLibTest, GetTokensTwoDepTwoLevels) {
  std::string parent_name1 = "parentOne";
  fdf_power::ParentElement parent_element1 =
      fdf_power::ParentElement::WithInstanceName(parent_name1);
  std::string parent_name2 = "parentTwo";
  fdf_power::ParentElement parent_element2 =
      fdf_power::ParentElement::WithInstanceName(parent_name2);
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();
  fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider> bindings;

  fbl::RefPtr<fs::PseudoDir> power_token_service = fbl::MakeRefCounted<fs::PseudoDir>();

  AddInstanceResult instance_one_data = AddServiceInstance(loop.dispatcher(), &bindings);
  AddInstanceResult instance_two_data = AddServiceInstance(loop.dispatcher(), &bindings);

  // Create the directory holding the service instances
  fbl::RefPtr<fs::PseudoDir> svc_instances_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  ASSERT_EQ(svc_instances_dir->AddEntry(parent_name1, instance_one_data.service_instance), ZX_OK);
  ASSERT_EQ(svc_instances_dir->AddEntry(parent_name2, instance_two_data.service_instance), ZX_OK);
  ASSERT_EQ(power_token_service->AddEntry(fuchsia_hardware_power::PowerTokenService::Name,
                                          svc_instances_dir),
            ZX_OK);

  fs::SynchronousVfs vfs(loop.dispatcher());
  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  vfs.ServeDirectory(std::move(power_token_service), std::move(dir_endpoints.server));

  // Specify the power element
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  // Create a dependency between a level on this element and a level on the
  // parent
  fdf_power::PowerDependency power_dep_one{
      .child = "n/a",
      .parent = parent_element1,
      .level_deps =
          {
              {
                  .child_level = 1,
                  .parent_level = 2,
              },
          },
      .strength = fdf_power::RequirementType::kAssertive,
  };

  // Create a dependency between a level on this element and a level on the
  // parent
  fdf_power::PowerDependency power_dep_two{
      .child = "n/a",
      .parent = parent_element2,
      .level_deps =
          {
              {
                  .child_level = 2,
                  .parent_level = 4,
              },
          },
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe,
                                                 .dependencies = {power_dep_one, power_dep_two}};

  // With the given configuration, get the dependency tokens. We expect this to
  // call into the PowerTokenProvider we built above which calls into our
  // FakeTokenServer instance.
  fit::result<fdf_power::Error, fdf_power::TokenMap> result =
      fdf_power::GetDependencyTokens(df_config, std::move(dir_endpoints.client));

  ASSERT_TRUE(result.is_ok());
  fdf_power::TokenMap token_map = std::move(result.value());
  ASSERT_EQ(token_map.size(), static_cast<size_t>(2));

  // Check that the zx::event defined in the test and the one return by
  // `GetTokens` are pointing at the same kernel object
  zx_info_handle_basic_t info1, info2;
  instance_one_data.token->get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t),
                                    nullptr, nullptr);
  token_map.at(parent_element1)
      .get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  ASSERT_EQ(info1.koid, info2.koid);

  instance_two_data.token->get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t),
                                    nullptr, nullptr);
  token_map.at(parent_element2)
      .get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  ASSERT_EQ(info1.koid, info2.koid);

  loop.Shutdown();
  loop.JoinThreads();
}

/// This tests the fdf_power::ApplyPowerConfiguration function. The validates
/// that the function calls appropriate token providers and makes a dependency
/// between the retrieved tokens and the element it adds.
///
/// The test creates fake token providers and a fake Topology service.
/// Then the test creates a power element configuration with dependencies on
/// two parent elements. The test gives the configuration and the fakes to the
/// function under test. The test then validates that the topology server was
/// passed an element configuration with the rght number of dependencies and
/// tokens for these dependencies match the ones given to the fake token
/// servers.
TEST_F(PowerLibTest, ApplyPowerConfiguration) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();

  // Create the dependency configuration
  fdf_power::ParentElement parent_name_first =
      fdf_power::ParentElement::WithInstanceName("element_first_parent");
  fdf_power::ParentElement parent_name_second =
      fdf_power::ParentElement::WithInstanceName("element_second_parent");
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  fdf_power::LevelTuple one_to_one{
      .child_level = 1,
      .parent_level = 1,
  };

  fdf_power::PowerDependency power_dep_one{
      .child = "the_element",
      .parent = parent_name_first,
      .level_deps = {one_to_one},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::LevelTuple three_to_two{
      .child_level = 3,
      .parent_level = 2,
  };

  fdf_power::PowerDependency power_dep_two{
      .child = "the_element",
      .parent = parent_name_second,
      .level_deps = {three_to_two},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe,
                                                 .dependencies = {power_dep_one, power_dep_two}};

  // Create service token instances
  fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider> token_bindings;
  AddInstanceResult instance_one_data = AddServiceInstance(loop.dispatcher(), &token_bindings);
  AddInstanceResult instance_two_data = AddServiceInstance(loop.dispatcher(), &token_bindings);

  // Place the services instances in a Service (ie. a directory)
  fbl::RefPtr<fs::PseudoDir> svc_instances_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  ASSERT_EQ(svc_instances_dir->AddEntry("element_first_parent", instance_two_data.service_instance),
            ZX_OK);
  ASSERT_EQ(
      svc_instances_dir->AddEntry("element_second_parent", instance_one_data.service_instance),
      ZX_OK);

  // Make the fake power broker
  std::vector<FakeTopology> fake_power_brokers;
  async::Executor executor(loop.dispatcher());
  std::unique_ptr<FakeElementControl> fake_element_control(nullptr);
  fbl::RefPtr<fs::Service> topology = fbl::MakeRefCounted<fs::Service>(
      [&loop, &executor, &fake_power_brokers, &fake_element_control, &instance_one_data,
       &instance_two_data](fidl::ServerEnd<fuchsia_power_broker::Topology> chan) -> int {
        fake_power_brokers.emplace_back(loop.dispatcher(), std::move(chan));
        executor.schedule_task(fake_power_brokers.back().TakeSchemaPromise().then(
            [&instance_one_data, &instance_two_data, &loop, &fake_element_control](
                fpromise::result<fuchsia_power_broker::ElementSchema, void>& result) {
              EXPECT_TRUE(result.is_ok());
              auto& received_deps = result.value().dependencies();
              EXPECT_TRUE(received_deps.has_value());
              ASSERT_EQ(received_deps->size(), static_cast<size_t>(2));

              std::optional<fidl::ServerEnd<ElementControl>>& ec = result.value().element_control();
              EXPECT_TRUE(ec.has_value());
              fake_element_control =
                  std::make_unique<FakeElementControl>(loop.dispatcher(), std::move(ec.value()));

              // Get handle info for the dependency tokens of the service instances
              zx_info_handle_basic_t dep_token_one_info, dep_token_two_info;
              instance_one_data.token->get_info(ZX_INFO_HANDLE_BASIC, &dep_token_one_info,
                                                sizeof(zx_info_handle_basic_t), nullptr, nullptr);
              instance_two_data.token->get_info(ZX_INFO_HANDLE_BASIC, &dep_token_two_info,
                                                sizeof(zx_info_handle_basic_t), nullptr, nullptr);

              // Get handle info for the dependency tokens sent to the topology server
              zx_info_handle_basic_t received_token_one_info, received_token_two_info;
              received_deps->at(0).requires_token().get_info(
                  ZX_INFO_HANDLE_BASIC, &received_token_one_info, sizeof(zx_info_handle_basic_t),
                  nullptr, nullptr);
              received_deps->at(1).requires_token().get_info(
                  ZX_INFO_HANDLE_BASIC, &received_token_two_info, sizeof(zx_info_handle_basic_t),
                  nullptr, nullptr);

              // Check that the two dep tokens weren't the same token
              EXPECT_NE(dep_token_one_info.koid, dep_token_two_info.koid);

              // Check that the first service instance token is the same as one
              // of the received tokens
              if (dep_token_one_info.koid != received_token_one_info.koid &&
                  dep_token_one_info.koid != received_token_two_info.koid) {
                FAIL() << "created dependency token ONE was not received";
              }

              // Check that the second service instance token is the same as
              // one of the received otkens
              if (dep_token_two_info.koid != received_token_one_info.koid &&
                  dep_token_two_info.koid != received_token_two_info.koid) {
                FAIL() << "created dependency token TWO was not received";
              }
            }));

        return ZX_OK;
      });

  // Add the topology service to services dir
  fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  svcs_dir->AddEntry("fuchsia.power.broker.Topology", topology);
  svcs_dir->AddEntry(fuchsia_hardware_power::PowerTokenService::Name, svc_instances_dir);

  // Create the namespace
  fs::SynchronousVfs vfs(loop.dispatcher());
  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  vfs.ServeDirectory(std::move(svcs_dir), std::move(dir_endpoints.server));
  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> namespace_entries;
  namespace_entries.emplace_back(fuchsia_component_runner::ComponentNamespaceEntry{
      {.path = "/svc", .directory = std::move(dir_endpoints.client)}});
  fdf::Namespace ns = fdf::Namespace::Create(namespace_entries).value();

  std::vector<fdf_power::PowerElementConfiguration> configs{df_config};

  // Now we've done all that, do the call and check that we get one thing back
  ASSERT_EQ(fdf_power::ApplyPowerConfiguration(ns, configs).value().size(), static_cast<size_t>(1));

  RunLoopUntil([&fake_element_control] { return fake_element_control != nullptr; });
  loop.Shutdown();
  loop.JoinThreads();

  // Only one power broker topology instance should exist.
  ASSERT_EQ(fake_power_brokers.size(), static_cast<size_t>(1));
}

/// Check GetTokens with a power elements with a single level which depends
/// on two different parent power elements.
TEST_F(PowerLibTest, GetTokensOneLevelTwoDeps) {
  std::string parent_name1 = "parentOne";
  fdf_power::ParentElement parent_element1 =
      fdf_power::ParentElement::WithInstanceName(parent_name1);
  std::string parent_name2 = "parenttwo";
  fdf_power::ParentElement parent_element2 =
      fdf_power::ParentElement::WithInstanceName(parent_name2);
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();
  fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider> bindings;

  fbl::RefPtr<fs::PseudoDir> power_token_service = fbl::MakeRefCounted<fs::PseudoDir>();

  AddInstanceResult instance_one_data = AddServiceInstance(loop.dispatcher(), &bindings);
  AddInstanceResult instance_two_data = AddServiceInstance(loop.dispatcher(), &bindings);

  // Create the directory holding the service instances
  fbl::RefPtr<fs::PseudoDir> svc_instances_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  ASSERT_EQ(svc_instances_dir->AddEntry(parent_name1, instance_one_data.service_instance), ZX_OK);
  ASSERT_EQ(svc_instances_dir->AddEntry(parent_name2, instance_two_data.service_instance), ZX_OK);
  ASSERT_EQ(power_token_service->AddEntry(fuchsia_hardware_power::PowerTokenService::Name,
                                          svc_instances_dir),
            ZX_OK);

  fs::SynchronousVfs vfs(loop.dispatcher());
  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  vfs.ServeDirectory(std::move(power_token_service), std::move(dir_endpoints.server));

  // Specify the power element
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  // Create a dependency between a level on this element and a level on the
  // parent
  fdf_power::PowerDependency power_dep_one{
      .child = "n/a",
      .parent = parent_element1,
      .level_deps =
          {
              {
                  .child_level = 1,
                  .parent_level = 2,
              },
          },
      .strength = fdf_power::RequirementType::kAssertive,
  };

  // Create a dependency between a level on this element and a level on the
  // parent
  fdf_power::PowerDependency power_dep_two{
      .child = "n/a",
      .parent = parent_element2,
      .level_deps =
          {
              {
                  .child_level = 1,
                  .parent_level = 6,
              },
          },
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe,
                                                 .dependencies = {power_dep_one, power_dep_two}};

  // With the given configuration, get the dependency tokens. We expect this to
  // call into the PowerTokenProvider we built above which calls into our
  // FakeTokenServer instance.
  fit::result<fdf_power::Error, fdf_power::TokenMap> result =
      fdf_power::GetDependencyTokens(df_config, std::move(dir_endpoints.client));

  ASSERT_TRUE(result.is_ok());
  fdf_power::TokenMap token_map = std::move(result.value());
  ASSERT_EQ(token_map.size(), static_cast<size_t>(2));

  // Check that the zx::event defined in the test and the one return by
  // `GetTokens` are pointing at the same kernel object
  zx_info_handle_basic_t info1, info2;
  instance_one_data.token->get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t),
                                    nullptr, nullptr);
  token_map.at(parent_element1)
      .get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  ASSERT_EQ(info1.koid, info2.koid);

  instance_two_data.token->get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t),
                                    nullptr, nullptr);
  token_map.at(parent_element2)
      .get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  ASSERT_EQ(info1.koid, info2.koid);

  loop.Shutdown();
  loop.JoinThreads();
}

class CpuElementManager : public fidl::testing::TestBase<fuchsia_power_system::CpuElementManager> {
 public:
  explicit CpuElementManager(zx::event cpu_dep_token) : cpu_dep_token_(std::move(cpu_dep_token)) {}

  void GetCpuDependencyToken(GetCpuDependencyTokenCompleter::Sync& completer) override {
    zx::event copy;
    cpu_dep_token_.duplicate(ZX_RIGHT_SAME_RIGHTS, &copy);
    completer.Reply({{std::move(copy)}});
  }

  void AddExecutionStateDependency(AddExecutionStateDependencyRequest& request,
                                   AddExecutionStateDependencyCompleter::Sync& completer) override {
    FAIL();
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_system::CpuElementManager> md,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    FAIL();
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override { FAIL(); }

 private:
  zx::event cpu_dep_token_;
};

/// Test getting dependency tokens for an element that depends on SAG's power
/// elements.
TEST_F(PowerLibTest, TestSagElements) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();

  zx::event exec_opportunistic, wake_assertive;
  zx::event::create(0, &exec_opportunistic);
  zx::event::create(0, &wake_assertive);
  zx::event exec_opportunistic_dupe, wake_assertive_dupe;
  exec_opportunistic.duplicate(ZX_RIGHT_SAME_RIGHTS, &exec_opportunistic_dupe);
  wake_assertive.duplicate(ZX_RIGHT_SAME_RIGHTS, &wake_assertive_dupe);

  SystemActivityGovernor sag_server(std::move(exec_opportunistic_dupe),
                                    std::move(wake_assertive_dupe), loop.dispatcher());

  fidl::ServerBindingGroup<fuchsia_power_system::ActivityGovernor> bindings;
  fbl::RefPtr<fs::Service> sag = fbl::MakeRefCounted<fs::Service>(
      [&](fidl::ServerEnd<fuchsia_power_system::ActivityGovernor> chan) {
        bindings.AddBinding(loop.dispatcher(), std::move(chan), &sag_server,
                            fidl::kIgnoreBindingClosure);
        return ZX_OK;
      });

  fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  svcs_dir->AddEntry("fuchsia.power.system.ActivityGovernor", sag);
  fs::SynchronousVfs vfs(loop.dispatcher());

  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  vfs.ServeDirectory(std::move(svcs_dir), std::move(dir_endpoints.server));
  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> namespace_entries;
  namespace_entries.emplace_back(fuchsia_component_runner::ComponentNamespaceEntry{
      {.path = "/svc", .directory = std::move(dir_endpoints.client)}});
  fdf::Namespace ns = fdf::Namespace::Create(namespace_entries).value();

  fdf_power::ParentElement parent =
      fdf_power::ParentElement::WithSag(fdf_power::SagElement::kExecutionState);
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  fdf_power::LevelTuple one_to_one{
      .child_level = 1,
      .parent_level = 1,
  };
  fdf_power::LevelTuple three_to_two{
      .child_level = 3,
      .parent_level = 2,
  };

  fdf_power::PowerDependency power_dep{
      .child = "n/a",
      .parent = parent,
      .level_deps = {one_to_one, three_to_two},
      .strength = fdf_power::RequirementType::kOpportunistic,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe, .dependencies = {power_dep}};

  fit::result<fdf_power::Error, fdf_power::TokenMap> call_result =
      fdf_power::GetDependencyTokens(ns, df_config);

  EXPECT_TRUE(call_result.is_ok());
  loop.Shutdown();
  loop.JoinThreads();

  fdf_power::TokenMap map = std::move(call_result.value());
  EXPECT_EQ(size_t(1), map.size());
  zx_info_handle_basic_t info1, info2;
  exec_opportunistic.get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t), nullptr,
                              nullptr);
  map.at(parent).get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t), nullptr,
                          nullptr);
  EXPECT_EQ(info1.koid, info2.koid);
}

TEST_F(PowerLibTest, TestCpuElementParent) {
  // Configure our power dependency
  fdf_power::ParentElement parent = fdf_power::ParentElement::WithCpu(fdf_power::CpuElement::kCpu);
  fdf_power::PowerLevel zero({.level = 0, .name = "zero", .transitions{}});
  fdf_power::PowerLevel one({.level = 1, .name = "one", .transitions{}});

  fdf_power::PowerElement child_element{
      .name = "child",
      .levels = {zero, one},
  };

  // set up the element dependencies
  fdf_power::LevelTuple one_on_three{
      .child_level = 1,
      .parent_level = 3,
  };

  fdf_power::PowerDependency child_to_parent_dep{
      .child = "little_one",
      .parent = parent,
      .level_deps = {one_on_three},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration pe_config{.element = child_element,
                                                 .dependencies = {child_to_parent_dep}};

  // Set up fake CPU element server
  zx::event token;
  zx::event::create(0, &token);
  zx::event token_copy;
  token.duplicate(ZX_RIGHT_SAME_RIGHTS, &token_copy);
  CpuElementManager cpu_element_mgr(std::move(token_copy));

  // Make the glue for FIDL calls to go to the fake
  // we'll need a separate dispatcher so we can run sync code in the test
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();

  fidl::ServerBindingGroup<fuchsia_power_system::CpuElementManager> bindings;
  fbl::RefPtr<fs::Service> cpu_element_mgr_server = fbl::MakeRefCounted<fs::Service>(
      [&](fidl::ServerEnd<fuchsia_power_system::CpuElementManager> chan) {
        bindings.AddBinding(loop.dispatcher(), std::move(chan), &cpu_element_mgr,
                            fidl::kIgnoreBindingClosure);
        return ZX_OK;
      });

  // Add entry for the service to a directory and serve the directory
  fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  svcs_dir->AddEntry("fuchsia.power.system.CpuElementManager", cpu_element_mgr_server);
  fidl::Endpoints<fuchsia_io::Directory> svcs_dir_channel =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  fs::SynchronousVfs vfs(loop.dispatcher());
  vfs.ServeDirectory(std::move(svcs_dir), std::move(svcs_dir_channel.server));

  // Wire the services directory into the namespace
  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> namespace_entries;
  namespace_entries.emplace_back(fuchsia_component_runner::ComponentNamespaceEntry{
      {.path = "/svc", .directory = std::move(svcs_dir_channel.client)}});
  fdf::Namespace ns = fdf::Namespace::Create(namespace_entries).value();

  // call the function under test
  fit::result<fdf_power::Error, fdf_power::TokenMap> token_result =
      fdf_power::GetDependencyTokens(ns, pe_config);

  loop.Shutdown();
  loop.JoinThreads();

  EXPECT_TRUE(token_result.is_ok());

  fdf_power::TokenMap tokens = std::move(token_result.value());

  // Check that we have just one token in the map
  EXPECT_EQ(tokens.size(), size_t{1});
  const zx::event& retrieved_token = tokens.at(parent);

  // Check that the token we got matches what we expect
  zx_info_handle_basic_t retreived_info, original_info;
  retrieved_token.get_info(ZX_INFO_HANDLE_BASIC, &retreived_info, sizeof(zx_info_handle_basic_t),
                           nullptr, nullptr);
  token.get_info(ZX_INFO_HANDLE_BASIC, &original_info, sizeof(zx_info_handle_basic_t), nullptr,
                 nullptr);
  EXPECT_EQ(retreived_info.koid, original_info.koid);
}

TEST_F(PowerLibTest, TestCpuElementManagerCloseOnRequest) {
  class ErrorCpuElementManager
      : public fidl::testing::TestBase<fuchsia_power_system::CpuElementManager> {
   public:
    ErrorCpuElementManager() = default;

    void GetCpuDependencyToken(GetCpuDependencyTokenCompleter::Sync& completer) override {
      completer.Close(ZX_OK);
    }

    void AddExecutionStateDependency(
        AddExecutionStateDependencyRequest& request,
        AddExecutionStateDependencyCompleter::Sync& completer) override {}

    void handle_unknown_method(
        fidl::UnknownMethodMetadata<fuchsia_power_system::CpuElementManager> md,
        fidl::UnknownMethodCompleter::Sync& completer) override {}

    void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {}
  };

  ErrorCpuElementManager mgr = ErrorCpuElementManager();

  // Configure out power dependency
  fdf_power::ParentElement parent = fdf_power::ParentElement::WithCpu(fdf_power::CpuElement::kCpu);
  fdf_power::PowerLevel zero({.level = 0, .name = "zero", .transitions{}});
  fdf_power::PowerLevel one({.level = 1, .name = "one", .transitions{}});

  fdf_power::PowerElement child_element{
      .name = "child",
      .levels = {zero, one},
  };

  // set up the element dependencies
  fdf_power::LevelTuple one_on_three{
      .child_level = 1,
      .parent_level = 3,
  };

  fdf_power::PowerDependency child_to_parent_dep{
      .child = "little_one",
      .parent = parent,
      .level_deps = {one_on_three},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration pe_config{.element = child_element,
                                                 .dependencies = {child_to_parent_dep}};

  // Make the glue for FIDL calls to go to the fake
  // we'll need a separate dispatcher so we can run sync code in the test
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();

  fidl::ServerBindingGroup<fuchsia_power_system::CpuElementManager> bindings;
  fbl::RefPtr<fs::Service> cpu_element_mgr_server = fbl::MakeRefCounted<fs::Service>(
      [&](fidl::ServerEnd<fuchsia_power_system::CpuElementManager> chan) {
        bindings.AddBinding(loop.dispatcher(), std::move(chan), &mgr, fidl::kIgnoreBindingClosure);
        return ZX_OK;
      });

  // Add entry for the service to a directory and serve the directory
  fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  svcs_dir->AddEntry("fuchsia.power.system.CpuElementManager", cpu_element_mgr_server);
  fidl::Endpoints<fuchsia_io::Directory> svcs_dir_channel =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  fs::SynchronousVfs vfs(loop.dispatcher());
  vfs.ServeDirectory(std::move(svcs_dir), std::move(svcs_dir_channel.server));

  // Wire the services directory into the namespace
  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> namespace_entries;
  namespace_entries.emplace_back(fuchsia_component_runner::ComponentNamespaceEntry{
      {.path = "/svc", .directory = std::move(svcs_dir_channel.client)}});
  fdf::Namespace ns = fdf::Namespace::Create(namespace_entries).value();

  // call the function under test
  fit::result<fdf_power::Error, fdf_power::TokenMap> token_result =
      fdf_power::GetDependencyTokens(ns, pe_config);

  loop.Shutdown();
  loop.JoinThreads();

  ASSERT_TRUE(token_result.is_error());
  ASSERT_EQ(token_result.error_value(), fdf_power::Error::CPU_ELEMENT_MANAGER_UNAVAILABLE);
}

TEST_F(PowerLibTest, TestCpuElementParentNotAvailable) {
  // Configure out power dependency
  fdf_power::ParentElement parent = fdf_power::ParentElement::WithCpu(fdf_power::CpuElement::kCpu);
  fdf_power::PowerLevel zero({.level = 0, .name = "zero", .transitions{}});
  fdf_power::PowerLevel one({.level = 1, .name = "one", .transitions{}});

  fdf_power::PowerElement child_element{
      .name = "child",
      .levels = {zero, one},
  };

  // set up the element dependencies
  fdf_power::LevelTuple one_on_three{
      .child_level = 1,
      .parent_level = 3,
  };

  fdf_power::PowerDependency child_to_parent_dep{
      .child = "little_one",
      .parent = parent,
      .level_deps = {one_on_three},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration pe_config{.element = child_element,
                                                 .dependencies = {child_to_parent_dep}};

  // Make the glue for FIDL calls to go to the fake
  // we'll need a separate dispatcher so we can run sync code in the test
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();

  // Add directory for the service, but do NOT add a service instance
  fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  fidl::Endpoints<fuchsia_io::Directory> svcs_dir_channel =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  fs::SynchronousVfs vfs(loop.dispatcher());
  vfs.ServeDirectory(std::move(svcs_dir), std::move(svcs_dir_channel.server));

  // Wire the services directory into the namespace
  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> namespace_entries;
  namespace_entries.emplace_back(fuchsia_component_runner::ComponentNamespaceEntry{
      {.path = "/svc", .directory = std::move(svcs_dir_channel.client)}});
  fdf::Namespace ns = fdf::Namespace::Create(namespace_entries).value();

  // call the function under test
  fit::result<fdf_power::Error, fdf_power::TokenMap> token_result =
      fdf_power::GetDependencyTokens(ns, pe_config);

  loop.Shutdown();
  loop.JoinThreads();

  ASSERT_TRUE(token_result.is_error());
  ASSERT_EQ(token_result.error_value(), fdf_power::Error::CPU_ELEMENT_MANAGER_UNAVAILABLE);
}

// Test that we can get tokens from all three types of dependencies if we
// configure them
TEST_F(PowerLibTest, TestAllParentElementTypes) {
  // Configure out power dependency
  fdf_power::PowerLevel zero({.level = 0, .name = "zero", .transitions{}});
  fdf_power::PowerLevel one({.level = 1, .name = "one", .transitions{}});
  fdf_power::PowerLevel two({.level = 2, .name = "two", .transitions{}});

  fdf_power::PowerElement child_element{
      .name = "child",
      .levels = {zero, one},
  };

  fdf_power::ParentElement cpu_parent =
      fdf_power::ParentElement::WithCpu(fdf_power::CpuElement::kCpu);
  fdf_power::ParentElement sag_parent =
      fdf_power::ParentElement::WithSag(fdf_power::SagElement::kExecutionState);
  std::string driver_parent_name = "driver_parent";
  fdf_power::ParentElement driver_parent =
      fdf_power::ParentElement::WithInstanceName(driver_parent_name);

  // set up the element dependencies
  fdf_power::LevelTuple one_on_cpu_three{
      .child_level = 1,
      .parent_level = 3,
  };

  fdf_power::LevelTuple two_on_cpu_four{
      .child_level = 2,
      .parent_level = 4,
  };

  fdf_power::PowerDependency cpu_dep{
      .child = "little_one",
      .parent = cpu_parent,
      .level_deps = {one_on_cpu_three, two_on_cpu_four},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::LevelTuple on_exec_state_active{
      .child_level = 1,
      .parent_level = 2,
  };

  fdf_power::PowerDependency sag_dep{
      .child = "little_one",
      .parent = sag_parent,
      .level_deps = {on_exec_state_active},
      .strength = fdf_power::RequirementType::kOpportunistic,
  };

  fdf_power::LevelTuple two_on_parent{
      .child_level = 2,
      .parent_level = 1,
  };

  fdf_power::PowerDependency driver_dep{
      .child = "little_one",
      .parent = driver_parent,
      .level_deps = {two_on_parent},
      .strength = fdf_power::RequirementType::kAssertive,
  };

  fdf_power::PowerElementConfiguration pe_config{.element = child_element,
                                                 .dependencies = {cpu_dep, sag_dep, driver_dep}};

  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();

  // Set up fake CPU element server
  zx::event cpu_token;
  zx::event::create(0, &cpu_token);
  zx::event token_copy;
  cpu_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &token_copy);
  CpuElementManager cpu_element_mgr(std::move(token_copy));

  // Make the glue for FIDL calls to go to the fake
  // we'll need a separate dispatcher so we can run sync code in the test
  fidl::ServerBindingGroup<fuchsia_power_system::CpuElementManager> cpu_bindings;
  fbl::RefPtr<fs::Service> cpu_element_mgr_server = fbl::MakeRefCounted<fs::Service>(
      [&](fidl::ServerEnd<fuchsia_power_system::CpuElementManager> chan) {
        cpu_bindings.AddBinding(loop.dispatcher(), std::move(chan), &cpu_element_mgr,
                                fidl::kIgnoreBindingClosure);
        return ZX_OK;
      });

  // Set up the fake SAG server
  // Note that `wake_assertive` we don't dupe because we aren't going to use
  // it for validation.
  zx::event exec_opportunistic, wake_assertive;
  zx::event::create(0, &exec_opportunistic);
  zx::event::create(0, &wake_assertive);
  zx::event exec_opportunistic_dupe;
  exec_opportunistic.duplicate(ZX_RIGHT_SAME_RIGHTS, &exec_opportunistic_dupe);
  SystemActivityGovernor sag(std::move(exec_opportunistic_dupe), std::move(wake_assertive),
                             loop.dispatcher());

  fidl::ServerBindingGroup<fuchsia_power_system::ActivityGovernor> sag_bindings;
  fbl::RefPtr<fs::Service> sag_server = fbl::MakeRefCounted<fs::Service>(
      [&](fidl::ServerEnd<fuchsia_power_system::ActivityGovernor> chan) {
        sag_bindings.AddBinding(loop.dispatcher(), std::move(chan), &sag,
                                fidl::kIgnoreBindingClosure);
        return ZX_OK;
      });

  // Set up the fake parent token provider
  fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider> token_service_bindings;
  // Service dir which contains the service instances
  fbl::RefPtr<fs::PseudoDir> power_token_service = fbl::MakeRefCounted<fs::PseudoDir>();
  AddInstanceResult driver_parent_instance_data =
      AddServiceInstance(loop.dispatcher(), &token_service_bindings);
  power_token_service->AddEntry(driver_parent_name, driver_parent_instance_data.service_instance);

  // Add entry for the service to a directory and serve the directory
  fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  svcs_dir->AddEntry("fuchsia.power.system.CpuElementManager", cpu_element_mgr_server);
  svcs_dir->AddEntry("fuchsia.power.system.ActivityGovernor", sag_server);
  svcs_dir->AddEntry(fuchsia_hardware_power::PowerTokenService::Name, power_token_service);

  fidl::Endpoints<fuchsia_io::Directory> svcs_dir_channel =
      fidl::Endpoints<fuchsia_io::Directory>::Create();
  fs::SynchronousVfs vfs(loop.dispatcher());
  vfs.ServeDirectory(std::move(svcs_dir), std::move(svcs_dir_channel.server));

  // Wire the services directory into the namespace
  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> namespace_entries;
  namespace_entries.emplace_back(fuchsia_component_runner::ComponentNamespaceEntry{
      {.path = "/svc", .directory = std::move(svcs_dir_channel.client)}});
  fdf::Namespace ns = fdf::Namespace::Create(namespace_entries).value();

  // call the function under test
  fit::result<fdf_power::Error, fdf_power::TokenMap> token_result =
      fdf_power::GetDependencyTokens(ns, pe_config);

  loop.Shutdown();
  loop.JoinThreads();

  EXPECT_TRUE(token_result.is_ok())
      << std::to_string(static_cast<uint8_t>(token_result.error_value()));

  fdf_power::TokenMap tokens = std::move(token_result.value());

  // Check that we have just one token in the map
  EXPECT_EQ(tokens.size(), size_t(3));
  const zx::event& retrieved_token = tokens.at(cpu_parent);

  // Check that the cpu token we got matches what we expect
  zx_info_handle_basic_t retreived_info, original_info;
  retrieved_token.get_info(ZX_INFO_HANDLE_BASIC, &retreived_info, sizeof(zx_info_handle_basic_t),
                           nullptr, nullptr);
  cpu_token.get_info(ZX_INFO_HANDLE_BASIC, &original_info, sizeof(zx_info_handle_basic_t), nullptr,
                     nullptr);
  EXPECT_EQ(retreived_info.koid, original_info.koid);

  // Check that the SAG otken matches what we expect
  const zx::event& retrieved_token_sag = tokens.at(sag_parent);
  retrieved_token_sag.get_info(ZX_INFO_HANDLE_BASIC, &retreived_info,
                               sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  exec_opportunistic.get_info(ZX_INFO_HANDLE_BASIC, &original_info, sizeof(zx_info_handle_basic_t),
                              nullptr, nullptr);
  EXPECT_EQ(retreived_info.koid, original_info.koid);

  const zx::event& retrieved_token_driver_parent = tokens.at(driver_parent);
  retrieved_token_driver_parent.get_info(ZX_INFO_HANDLE_BASIC, &retreived_info,
                                         sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  driver_parent_instance_data.token->get_info(ZX_INFO_HANDLE_BASIC, &original_info,
                                              sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  EXPECT_EQ(retreived_info.koid, original_info.koid);
}

/// Test GetTokens when a power element has dependencies on driver/named power
/// elements and SAG power elements.
TEST_F(PowerLibTest, TestDriverAndSagElements) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();

  zx::event exec_opportunistic, wake_assertive;
  zx::event::create(0, &exec_opportunistic);
  zx::event::create(0, &wake_assertive);
  zx::event exec_opportunistic_dupe, wake_assertive_dupe;
  exec_opportunistic.duplicate(ZX_RIGHT_SAME_RIGHTS, &exec_opportunistic_dupe);
  wake_assertive.duplicate(ZX_RIGHT_SAME_RIGHTS, &wake_assertive_dupe);

  SystemActivityGovernor sag_server(std::move(exec_opportunistic_dupe),
                                    std::move(wake_assertive_dupe), loop.dispatcher());

  fidl::ServerBindingGroup<fuchsia_power_system::ActivityGovernor> bindings;
  fbl::RefPtr<fs::Service> sag = fbl::MakeRefCounted<fs::Service>(
      [&](fidl::ServerEnd<fuchsia_power_system::ActivityGovernor> chan) {
        bindings.AddBinding(loop.dispatcher(), std::move(chan), &sag_server,
                            fidl::kIgnoreBindingClosure);
        return ZX_OK;
      });

  fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  svcs_dir->AddEntry("fuchsia.power.system.ActivityGovernor", sag);
  fs::SynchronousVfs vfs(loop.dispatcher());

  // Now let's add dependencies on non-SAG elements
  std::string driver_parent_name = "driver_parent";
  fdf_power::ParentElement driver_parent =
      fdf_power::ParentElement::WithInstanceName(driver_parent_name);

  fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider> token_service_bindings;
  // Service dir which contains the service instances
  fbl::RefPtr<fs::PseudoDir> power_token_service = fbl::MakeRefCounted<fs::PseudoDir>();
  AddInstanceResult instance_data = AddServiceInstance(loop.dispatcher(), &token_service_bindings);
  ASSERT_EQ(ZX_OK,
            power_token_service->AddEntry(driver_parent_name, instance_data.service_instance));
  svcs_dir->AddEntry(fuchsia_hardware_power::PowerTokenService::Name, power_token_service);

  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();

  vfs.ServeDirectory(std::move(svcs_dir), std::move(dir_endpoints.server));

  fdf_power::ParentElement parent =
      fdf_power::ParentElement::WithSag(fdf_power::SagElement::kExecutionState);
  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe = {
      .name = "the_element",
      .levels = {one, two, three},
  };

  fdf_power::LevelTuple one_to_one{
      .child_level = 1,
      .parent_level = 1,
  };
  fdf_power::LevelTuple three_to_two{
      .child_level = 3,
      .parent_level = 2,
  };

  fdf_power::PowerDependency power_dep{
      .child = "n/a",
      .parent = parent,
      .level_deps = {one_to_one, three_to_two},
      .strength = fdf_power::RequirementType::kOpportunistic,
  };

  fdf_power::LevelTuple two_to_two{
      .child_level = 2,
      .parent_level = 2,
  };

  fdf_power::PowerDependency driver_power_dep{
      .child = "n/a",
      .parent = driver_parent,
      .level_deps = {two_to_two},
      .strength = fdf_power::RequirementType::kOpportunistic,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe,
                                                 .dependencies = {power_dep, driver_power_dep}};

  fit::result<fdf_power::Error, fdf_power::TokenMap> call_result =
      fdf_power::GetDependencyTokens(df_config, std::move(dir_endpoints.client));

  EXPECT_TRUE(call_result.is_ok());

  loop.Shutdown();
  loop.JoinThreads();

  fdf_power::TokenMap map = std::move(call_result.value());
  EXPECT_EQ(size_t(2), map.size());
  zx_info_handle_basic_t info1, info2;
  exec_opportunistic.get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t), nullptr,
                              nullptr);
  map.at(parent).get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t), nullptr,
                          nullptr);
  EXPECT_EQ(info1.koid, info2.koid);

  instance_data.token->get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t),
                                nullptr, nullptr);
  map.at(driver_parent)
      .get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  EXPECT_EQ(info1.koid, info2.koid);
}

/// Add an element which uses the instance name to get the dependency token.
TEST_F(PowerLibTest, TestDriverInstanceDep) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop.StartThread();

  fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();

  // Now let's add dependencies on non-SAG elements
  std::string driver_instance_name = "instance_name";
  fdf_power::ParentElement driver_parent =
      fdf_power::ParentElement::WithInstanceName(driver_instance_name);

  fidl::ServerBindingGroup<fuchsia_hardware_power::PowerTokenProvider> token_service_bindings;
  // Service dir which contains the service instances
  fbl::RefPtr<fs::PseudoDir> power_token_service = fbl::MakeRefCounted<fs::PseudoDir>();
  AddInstanceResult instance_data = AddServiceInstance(loop.dispatcher(), &token_service_bindings);
  ASSERT_EQ(ZX_OK,
            power_token_service->AddEntry(driver_instance_name, instance_data.service_instance));
  svcs_dir->AddEntry(fuchsia_hardware_power::PowerTokenService::Name, power_token_service);

  fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
      fidl::Endpoints<fuchsia_io::Directory>::Create();

  fs::SynchronousVfs vfs(loop.dispatcher());
  vfs.ServeDirectory(std::move(svcs_dir), std::move(dir_endpoints.server));

  fdf_power::PowerLevel one{.level = 0, .name = "one", .transitions{}};
  fdf_power::PowerLevel two{.level = 1, .name = "two", .transitions{}};
  fdf_power::PowerLevel three{.level = 2, .name = "three", .transitions{}};

  fdf_power::PowerElement pe{
      .name = "the_element",
      .levels = {one, two, three},
  };

  fdf_power::LevelTuple two_to_two{
      .child_level = 2,
      .parent_level = 2,
  };

  fdf_power::PowerDependency driver_power_dep{
      .child = "n/a",
      .parent = driver_parent,
      .level_deps = {two_to_two},
      .strength = fdf_power::RequirementType::kOpportunistic,
  };

  fdf_power::PowerElementConfiguration df_config{.element = pe, .dependencies = {driver_power_dep}};

  fit::result<fdf_power::Error, fdf_power::TokenMap> call_result =
      fdf_power::GetDependencyTokens(df_config, std::move(dir_endpoints.client));

  EXPECT_TRUE(call_result.is_ok());

  loop.Shutdown();
  loop.JoinThreads();

  fdf_power::TokenMap map = std::move(call_result.value());
  EXPECT_EQ(size_t(1), map.size());
  zx_info_handle_basic_t info1, info2;

  instance_data.token->get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(zx_info_handle_basic_t),
                                nullptr, nullptr);
  map.at(driver_parent)
      .get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(zx_info_handle_basic_t), nullptr, nullptr);
  EXPECT_EQ(info1.koid, info2.koid);
}
// TODO(https://fxbug.dev/328527466) This dependency is invalid because it has
// no level deps add a test that checks we return a proper error

}  // namespace power_lib_test

#endif
