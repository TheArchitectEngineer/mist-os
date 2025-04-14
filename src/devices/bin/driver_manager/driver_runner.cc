// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/driver_runner.h"

#include <fidl/fuchsia.driver.development/cpp/wire.h>
#include <fidl/fuchsia.driver.host/cpp/wire.h>
#include <fidl/fuchsia.driver.index/cpp/wire.h>
#include <fidl/fuchsia.process/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/directory.h>
#include <lib/fit/defer.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/status.h>

#include <forward_list>
#include <queue>
#include <random>
#include <stack>
#include <utility>

#include "src/devices/bin/driver_manager/composite_node_spec_impl.h"
#include "src/devices/bin/driver_manager/node_property_conversion.h"
#include "src/devices/lib/log/log.h"
#include "src/lib/fxl/strings/join_strings.h"

namespace fdf {

using namespace fuchsia_driver_framework;

}
namespace fdh = fuchsia_driver_host;
namespace fdd = fuchsia_driver_development;
namespace fdi = fuchsia_driver_index;
namespace fio = fuchsia_io;
namespace frunner = fuchsia_component_runner;
namespace fcomponent = fuchsia_component;
namespace fdecl = fuchsia_component_decl;

using InspectStack = std::stack<std::pair<inspect::Node*, const driver_manager::Node*>>;

namespace driver_manager {

namespace {

constexpr auto kBootScheme = "fuchsia-boot://";
constexpr std::string_view kRootDeviceName = "dev";

template <typename R, typename F>
std::optional<R> VisitOffer(const NodeOffer& offer, F apply) {
  zx::result get_offer_result = GetInnerOffer(offer);
  if (get_offer_result.is_error()) {
    return {};
  }

  auto [inner_offer, _] = get_offer_result.value();

  // Note, we access each field of the union as mutable, so that `apply` can
  // modify the field if necessary.
  switch (inner_offer.Which()) {
    case fdecl::wire::Offer::Tag::kService:
      return apply(inner_offer.service());
    case fdecl::wire::Offer::Tag::kProtocol:
      return apply(inner_offer.protocol());
    case fdecl::wire::Offer::Tag::kDirectory:
      return apply(inner_offer.directory());
    case fdecl::wire::Offer::Tag::kStorage:
      return apply(inner_offer.storage());
    case fdecl::wire::Offer::Tag::kRunner:
      return apply(inner_offer.runner());
    case fdecl::wire::Offer::Tag::kResolver:
      return apply(inner_offer.resolver());
    case fdecl::wire::Offer::Tag::kEventStream:
      return apply(inner_offer.event_stream());
    default:
      return {};
  }
}

void InspectNode(inspect::Inspector& inspector, InspectStack& stack) {
  const auto inspect_decl = [](auto& decl) -> std::string_view {
    if (decl.has_target_name()) {
      return decl.target_name().get();
    }
    if (decl.has_source_name()) {
      return decl.source_name().get();
    }
    return "<missing>";
  };

  std::forward_list<inspect::Node> roots;
  std::unordered_set<const Node*> unique_nodes;
  while (!stack.empty()) {
    // Pop the current root and node to operate on.
    auto [root, node] = stack.top();
    stack.pop();

    auto [_, inserted] = unique_nodes.insert(node);
    if (!inserted) {
      // Only insert unique nodes from the DAG.
      continue;
    }

    // Populate root with data from node.
    if (const auto& offers = node->offers(); !offers.empty()) {
      std::vector<std::string_view> strings;
      for (const auto& offer : offers) {
        auto string = VisitOffer<std::string_view>(offer, inspect_decl);
        strings.push_back(string.value_or("unknown"));
      }
      root->RecordString("offers", fxl::JoinStrings(strings, ", "));
    }
    if (auto symbols = node->symbols(); !symbols.empty()) {
      std::vector<std::string_view> strings;
      for (auto& symbol : symbols) {
        strings.push_back(symbol.name().get());
      }
      root->RecordString("symbols", fxl::JoinStrings(strings, ", "));
    }
    std::string driver_string = node->driver_url();
    root->RecordString("driver", driver_string);

    // Push children of this node onto the stack. We do this in reverse order to
    // ensure the children are handled in order, from first to last.
    auto& children = node->children();
    for (auto child = children.rbegin(), end = children.rend(); child != end; ++child) {
      auto& name = (*child)->name();
      auto& root_for_child = roots.emplace_front(root->CreateChild(name));
      stack.emplace(&root_for_child, child->get());
    }
  }

  // Store all of the roots in the inspector.
  for (auto& root : roots) {
    inspector.GetRoot().Record(std::move(root));
  }
}

fidl::StringView CollectionName(Collection collection) {
  switch (collection) {
    case Collection::kNone:
      return {};
    case Collection::kBoot:
      return "boot-drivers";
    case Collection::kPackage:
      return "base-drivers";
    case Collection::kFullPackage:
      return "full-drivers";
  }
}

Collection ToCollection(fdf::DriverPackageType package) {
  switch (package) {
    case fdf::DriverPackageType::kBoot:
      return Collection::kBoot;
    case fdf::DriverPackageType::kBase:
      return Collection::kPackage;
    case fdf::DriverPackageType::kCached:
    case fdf::DriverPackageType::kUniverse:
      return Collection::kFullPackage;
    default:
      return Collection::kNone;
  }
}

// Choose the highest ranked collection between `collection` and `node`'s
// parents. If one of `node`'s parent's collection is none then check the
// parent's parents and so on.
Collection GetHighestRankingCollection(const Node& node, Collection collection) {
  std::stack<std::weak_ptr<Node>> ancestors;
  for (const auto& parent : node.parents()) {
    ancestors.emplace(parent);
  }

  // Find the highest ranked collection out of `node`'s parent nodes. If a
  // node's collection is none then check that node's parents and so on.
  while (!ancestors.empty()) {
    auto ancestor = ancestors.top();
    ancestors.pop();
    auto ancestor_ptr = ancestor.lock();
    if (!ancestor_ptr) {
      LOGF(WARNING, "Ancestor node released");
      continue;
    }

    auto ancestor_collection = ancestor_ptr->collection();
    if (ancestor_collection == Collection::kNone) {
      // Check ancestor's parents to see what the collection of the ancestor
      // should be.
      for (const auto& parent : ancestor_ptr->parents()) {
        ancestors.emplace(parent);
      }
    } else if (ancestor_collection > collection) {
      collection = ancestor_collection;
    }
  }

  return collection;
}

// Perform a Breadth-First-Search (BFS) over the node topology, applying the visitor function on
// the node being visited.
// The return value of the visitor function is a boolean for whether the children of the node
// should be visited. If it returns false, the children will be skipped.
void PerformBFS(const std::shared_ptr<Node>& starting_node,
                fit::function<bool(const std::shared_ptr<driver_manager::Node>&)> visitor) {
  std::unordered_set<std::shared_ptr<const Node>> visited;
  std::queue<std::shared_ptr<Node>> node_queue;
  visited.insert(starting_node);
  node_queue.push(starting_node);

  while (!node_queue.empty()) {
    auto current = node_queue.front();
    node_queue.pop();

    bool visit_children = visitor(current);
    if (!visit_children) {
      continue;
    }

    for (const auto& child : current->children()) {
      if (child->GetPrimaryParent() != current.get()) {
        continue;
      }

      if (auto [_, inserted] = visited.insert(child); inserted) {
        node_queue.push(child);
      }
    }
  }
}

void CallStartDriverOnRunner(Runner& runner, Node& node, const std::string& moniker,
                             std::string_view url,
                             std::optional<fuchsia_component_sandbox::DictionaryRef> ref,
                             const std::shared_ptr<BootupTracker>& bootup_tracker) {
  runner.StartDriverComponent(
      moniker, url, CollectionName(node.collection()).get(), node.offers(), std::move(ref),
      [node_weak = node.weak_from_this(), moniker,
       bootup_tracker = std::weak_ptr<BootupTracker>(bootup_tracker)](
          zx::result<driver_manager::Runner::StartedComponent> component) {
        std::shared_ptr node = node_weak.lock();
        if (!node) {
          return;
        }

        if (component.is_error()) {
          node->CompleteBind(component.take_error());
          if (auto tracker_ptr = bootup_tracker.lock(); tracker_ptr) {
            tracker_ptr->NotifyStartComplete(moniker);
          }
          return;
        }

        fidl::Arena arena;
        node->StartDriver(fidl::ToWire(arena, std::move(component->info)),
                          std::move(component->controller),
                          [node_weak, moniker, bootup_tracker](zx::result<> result) {
                            if (std::shared_ptr node = node_weak.lock(); node) {
                              node->CompleteBind(result);
                            }

                            if (auto tracker_ptr = bootup_tracker.lock(); tracker_ptr) {
                              tracker_ptr->NotifyStartComplete(moniker);
                            }
                          });
      });
}

// Helper class to make sending out concurrent async requests and making a callback when they have
// all finished easier.
class AsyncSharder {
 public:
  AsyncSharder(size_t count, fit::callback<void()> complete_callback)
      : remaining_(count), complete_callback_(std::move(complete_callback)) {}

  ~AsyncSharder() { ZX_ASSERT_MSG(remaining_ == 0, "Sharder not complete"); }

  void CompleteShard() {
    if (--remaining_ == 0) {
      complete_callback_();
    }
  }

 private:
  size_t remaining_;
  fit::callback<void()> complete_callback_;
};

}  // namespace

Collection ToCollection(const Node& node, fdf::DriverPackageType package_type) {
  Collection collection = ToCollection(package_type);
  return GetHighestRankingCollection(node, collection);
}

DriverRunner::DriverRunner(
    fidl::ClientEnd<fcomponent::Realm> realm,
    fidl::ClientEnd<fuchsia_component_sandbox::CapabilityStore> capability_store,
    fidl::ClientEnd<fdi::DriverIndex> driver_index, InspectManager& inspect,
    LoaderServiceFactory loader_service_factory, async_dispatcher_t* dispatcher,
    bool enable_test_shutdown_delays, OfferInjector offer_injector,
    std::optional<DynamicLinkerArgs> dynamic_linker_args)
    : driver_index_(std::move(driver_index), dispatcher),
      capability_store_(std::move(capability_store), dispatcher),
      loader_service_factory_(std::move(loader_service_factory)),
      dispatcher_(dispatcher),
      root_node_(std::make_shared<Node>(kRootDeviceName, std::vector<std::weak_ptr<Node>>{}, this,
                                        dispatcher,
                                        inspect.CreateDevice(std::string(kRootDeviceName), 0))),
      composite_node_spec_manager_(this),
      bind_manager_(this, this, dispatcher),
      runner_(dispatcher, fidl::WireClient(std::move(realm), dispatcher), offer_injector),
      removal_tracker_(dispatcher),
      enable_test_shutdown_delays_(enable_test_shutdown_delays),
      dynamic_linker_args_(std::move(dynamic_linker_args)) {
  if (enable_test_shutdown_delays_) {
    // TODO(https://fxbug.dev/42084497): Allow the seed to be set from the configuration.
    auto seed = std::chrono::system_clock::now().time_since_epoch().count();
    LOGF(INFO, "Shutdown test delays enabled. Using seed %u", seed);
    shutdown_test_delay_rng_ = std::make_shared<std::mt19937>(static_cast<uint32_t>(seed));
  }

  inspect.root_node().RecordLazyNode("driver_runner", [this] { return Inspect(); });

  // Pick a non-zero starting id so that folks cannot rely on the driver host process names being
  // stable.
  std::random_device rd;
  std::mt19937 gen(rd());
  std::uniform_int_distribution<> distrib(0, 1000);
  next_driver_host_id_ = distrib(gen);

  bootup_tracker_ = std::make_shared<BootupTracker>(&bind_manager_, dispatcher);

  // Setup the driver notifier.
  auto [notifier_client, notifier_server] =
      fidl::Endpoints<fuchsia_driver_index::DriverNotifier>::Create();
  driver_notifier_bindings_.AddBinding(dispatcher_, std::move(notifier_server), this,
                                       fidl::kIgnoreBindingClosure);
  fidl::OneWayStatus status = driver_index_->SetNotifier(std::move(notifier_client));
  if (!status.ok()) {
    LOGF(WARNING, "Failed to set the driver notifier: %s", status.status_string());
  }
}

void DriverRunner::BindNodesForCompositeNodeSpec() { TryBindAllAvailable(); }

void DriverRunner::AddSpec(AddSpecRequestView request, AddSpecCompleter::Sync& completer) {
  if (!request->has_name() || (!request->has_parents() && !request->has_parents2())) {
    completer.Reply(fit::error(fdf::CompositeNodeSpecError::kMissingArgs));
    return;
  }

  if (!request->has_parents() && !request->has_parents2()) {
    completer.Reply(fit::error(fdf::CompositeNodeSpecError::kDuplicateParents));
    return;
  }

  std::vector<fuchsia_driver_framework::ParentSpec2> parents;
  if (request->has_parents()) {
    if (request->parents().empty()) {
      completer.Reply(fit::error(fdf::CompositeNodeSpecError::kEmptyNodes));
      return;
    }
    auto to_parent_spec2 = [](const auto& parent) {
      auto parent_spec = fidl::ToNatural(parent);
      std::vector<fuchsia_driver_framework::BindRule2> bind_rules;
      std::transform(parent_spec.bind_rules().begin(), parent_spec.bind_rules().end(),
                     std::back_inserter(bind_rules), ToBindRule2);

      std::vector<fuchsia_driver_framework::NodeProperty2> properties;
      std::transform(parent_spec.properties().begin(), parent_spec.properties().end(),
                     std::back_inserter(properties),
                     [](const auto& prop) { return ToProperty2(prop); });
      return fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = std::move(bind_rules),
          .properties = std::move(properties),
      }};
    };

    std::transform(request->parents().cbegin(), request->parents().cend(),
                   std::back_inserter(parents), to_parent_spec2);
  }

  if (request->has_parents2()) {
    if (request->parents2().empty()) {
      completer.Reply(fit::error(fdf::CompositeNodeSpecError::kEmptyNodes));
      return;
    }
    parents = fidl::ToNatural(request->parents2()).value();
  }

  auto spec = std::make_unique<CompositeNodeSpecImpl>(
      CompositeNodeSpecCreateInfo{
          .name = std::string(request->name().get()),
          .parents = std::move(parents),
      },
      dispatcher_, this);
  composite_node_spec_manager_.AddSpec(
      *request, std::move(spec),
      [completer = completer.ToAsync()](
          fit::result<fuchsia_driver_framework::CompositeNodeSpecError> result) mutable {
        completer.Reply(result);
      });
}

void DriverRunner::FindDriverCrash(FindDriverCrashRequestView request,
                                   FindDriverCrashCompleter::Sync& completer) {
  for (const DriverHostComponent& host : driver_hosts_) {
    zx::result process_koid = host.GetProcessKoid();
    if (process_koid.is_ok() && process_koid.value() == request->process_koid) {
      host.GetCrashInfo(
          request->thread_koid,
          [this, async_completer = completer.ToAsync()](
              zx::result<fuchsia_driver_host::DriverCrashInfo> info_result) mutable {
            if (info_result.is_error()) {
              async_completer.ReplyError(info_result.error_value());
              return;
            }
            fuchsia_driver_host::DriverCrashInfo& found = info_result.value();
            zx_info_handle_basic_t info;
            zx_status_t status = found.node_token()->get_info(ZX_INFO_HANDLE_BASIC, &info,
                                                              sizeof(info), nullptr, nullptr);
            if (status != ZX_OK) {
              async_completer.ReplyError(ZX_ERR_INTERNAL);
              return;
            }

            const Node* node = nullptr;
            PerformBFS(root_node_, [&node, token_koid = info.koid](
                                       const std::shared_ptr<driver_manager::Node>& current) {
              if (node != nullptr) {
                // Already found it.
                return false;
              }
              std::optional current_koid = current->token_koid();
              if (current_koid && current_koid.value() == token_koid) {
                node = current.get();
                return false;
              }
              return true;
            });
            if (node == nullptr) {
              async_completer.ReplyError(ZX_ERR_NOT_FOUND);
              return;
            }

            fidl::Arena arena;
            async_completer.ReplySuccess(fuchsia_driver_crash::wire::DriverCrashInfo::Builder(arena)
                                             .node_moniker(arena, node->MakeComponentMoniker())
                                             .url(arena, found.url().value())
                                             .Build());
          });
      return;
    }
  }
  completer.ReplyError(ZX_ERR_NOT_FOUND);
}

void DriverRunner::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_framework::CompositeNodeManager> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  std::string method_type;
  switch (metadata.unknown_method_type) {
    case fidl::UnknownMethodType::kOneWay:
      method_type = "one-way";
      break;
    case fidl::UnknownMethodType::kTwoWay:
      method_type = "two-way";
      break;
  };

  LOGF(WARNING, "CompositeNodeManager received unknown %s method. Ordinal: %lu",
       method_type.c_str(), metadata.method_ordinal);
}

void DriverRunner::Get(GetRequest& request, GetCompleter::Sync& completer) {
  zx_info_handle_basic_t info;
  zx_status_t status =
      request.token().get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    completer.Reply(zx::error(status));
    return;
  }
  const Node* node = nullptr;
  PerformBFS(root_node_,
             [&node, token_koid = info.koid](const std::shared_ptr<driver_manager::Node>& current) {
               if (node != nullptr) {
                 // Already found it.
                 return false;
               }
               std::optional current_koid = current->token_koid();
               if (current_koid && current_koid.value() == token_koid) {
                 node = current.get();
                 return false;
               }
               return true;
             });
  if (node == nullptr) {
    completer.Reply(zx::error(ZX_ERR_NOT_FOUND));
    return;
  }

  completer.Reply(zx::ok(node->GetBusTopology()));
}

void DriverRunner::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_token::NodeBusTopology> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  std::string method_type;
  switch (metadata.unknown_method_type) {
    case fidl::UnknownMethodType::kOneWay:
      method_type = "one-way";
      break;
    case fidl::UnknownMethodType::kTwoWay:
      method_type = "two-way";
      break;
  };

  LOGF(WARNING, "NodeBusTopology received unknown %s method. Ordinal: %lu", method_type.c_str(),
       metadata.method_ordinal);
}

void DriverRunner::AddSpecToDriverIndex(fuchsia_driver_framework::wire::CompositeNodeSpec group,
                                        AddToIndexCallback callback) {
  driver_index_->AddCompositeNodeSpec(group).Then(
      [callback = std::move(callback)](
          fidl::WireUnownedResult<fdi::DriverIndex::AddCompositeNodeSpec>& result) mutable {
        if (!result.ok()) {
          LOGF(ERROR, "DriverIndex::AddCompositeNodeSpec failed %d", result.status());
          callback(zx::error(result.status()));
          return;
        }

        if (result->is_error()) {
          callback(result->take_error());
          return;
        }

        callback(zx::ok());
      });
}

// TODO(https://fxbug.dev/42072971): Add information for composite node specs.
fpromise::promise<inspect::Inspector> DriverRunner::Inspect() const {
  // Create our inspector.
  // The default maximum size was too small, and so this is double the default size.
  // If a device loads too much inspect data, this can be increased in the future.
  inspect::Inspector inspector(inspect::InspectSettings{.maximum_size = 2 * 256 * 1024});

  // Make the device tree inspect nodes.
  auto device_tree = inspector.GetRoot().CreateChild("node_topology");
  auto root = device_tree.CreateChild(root_node_->name());
  InspectStack stack{{std::make_pair(&root, root_node_.get())}};
  InspectNode(inspector, stack);
  device_tree.Record(std::move(root));
  inspector.GetRoot().Record(std::move(device_tree));

  bind_manager_.RecordInspect(inspector);

  return fpromise::make_ok_promise(inspector);
}

std::vector<fdd::wire::CompositeNodeInfo> DriverRunner::GetCompositeListInfo(
    fidl::AnyArena& arena) const {
  auto spec_composite_list = composite_node_spec_manager_.GetCompositeInfo(arena);
  auto list = bind_manager_.GetCompositeListInfo(arena);
  list.reserve(list.size() + spec_composite_list.size());
  list.insert(list.end(), std::make_move_iterator(spec_composite_list.begin()),
              std::make_move_iterator(spec_composite_list.end()));
  return list;
}

void DriverRunner::WaitForBootup(fit::callback<void()> callback) {
  bootup_tracker_->WaitForBootup(std::move(callback));
}

void DriverRunner::PublishComponentRunner(component::OutgoingDirectory& outgoing) {
  zx::result result = runner_.Publish(outgoing);
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());

  result = outgoing.AddUnmanagedProtocol<fdf::CompositeNodeManager>(
      manager_bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure));
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());

  result = outgoing.AddUnmanagedProtocol<fuchsia_driver_token::NodeBusTopology>(
      bus_topo_bindings_.CreateHandler(this, dispatcher_, [](fidl::UnbindInfo info) {
        if (info.is_user_initiated() || info.is_peer_closed()) {
          return;
        }
        LOGF(WARNING, "Unexpected closure of NodeBusTopology: %s",
             info.FormatDescription().c_str());
      }));
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());

  result = outgoing.AddUnmanagedProtocol<fuchsia_driver_crash::CrashIntrospect>(
      crash_introspect_bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure));
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());
}

zx::result<> DriverRunner::StartRootDriver(std::string_view url) {
  fdf::DriverPackageType package = cpp20::starts_with(url, kBootScheme)
                                       ? fdf::DriverPackageType::kBoot
                                       : fdf::DriverPackageType::kBase;
  bootup_tracker_->Start();
  return StartDriver(*root_node_, url, package);
}

void DriverRunner::StartDevfsDriver(driver_manager::Devfs& devfs) {
  std::vector<NodeOffer> offers;
  runner_.StartDriverComponent(
      "devfs_driver", "fuchsia-boot:///devfs-driver#meta/devfs-driver.cm",
      CollectionName(Collection::kBoot).get(), offers, std::nullopt,
      [&devfs](zx::result<driver_manager::Runner::StartedComponent> component) {
        if (component.is_error()) {
          LOGF(ERROR, "Starting the devfs component failed %s", component.status_string());
          return;
        }
        devfs.AttachComponent(std::move(component->info), std::move(component->controller));
      });
}

void DriverRunner::NewDriverAvailable(NewDriverAvailableCompleter::Sync& completer) {
  TryBindAllAvailable();
}

void DriverRunner::TryBindAllAvailable(NodeBindingInfoResultCallback result_callback) {
  bind_manager_.TryBindAllAvailable(std::move(result_callback));
}

zx::result<> DriverRunner::StartDriver(Node& node, std::string_view url,
                                       fdf::DriverPackageType package_type) {
  // Ensure `node`'s collection is equal to or higher ranked than its ancestor
  // nodes' collections. This is to avoid node components having a dependency
  // cycle with each other. For example, node components in the boot driver
  // collection depend on the devfs component which ultimately depends on all
  // components within the package driver collection. If a package driver
  // component depended on a component in the boot driver collection (a lower
  // ranked collection than the package driver collection) then a cyclic
  // dependency would occur.
  node.set_collection(ToCollection(node, package_type));
  node.set_driver_package_type(package_type);

  std::weak_ptr node_weak = node.shared_from_this();
  std::string url_string(url.data(), url.size());
  auto moniker = node.MakeComponentMoniker();
  bootup_tracker_->NotifyNewStartRequest(moniker, url_string);

  if (node.dictionary_ref().has_value()) {
    uint64_t dest = cap_id_++;
    capability_store_->DictionaryCopy(node.dictionary_ref().value(), dest)
        .Then(
            [this, dest, node_weak, moniker, url_string,
             bootup_tracker = std::weak_ptr<BootupTracker>(bootup_tracker_)](
                fidl::WireUnownedResult<fuchsia_component_sandbox::CapabilityStore::DictionaryCopy>&
                    result) {
              if (!result.ok() || result->is_error()) {
                LOGF(ERROR, "Failed to copy dictionary.");
                return;
              }

              capability_store_->Export(dest).Then(
                  [this, node_weak, moniker, url_string](
                      fidl::WireUnownedResult<fuchsia_component_sandbox::CapabilityStore::Export>&
                          result) {
                    if (!result.ok() || result->is_error()) {
                      LOGF(ERROR, "Failed to export dictionary.");
                      return;
                    }

                    std::shared_ptr node = node_weak.lock();
                    if (!node) {
                      return;
                    }

                    CallStartDriverOnRunner(
                        runner_, *node, moniker, url_string,
                        fidl::ToNatural(std::move(result->value()->capability.dictionary())),
                        bootup_tracker_);
                  });
            });
    return zx::ok();
  }

  CallStartDriverOnRunner(runner_, node, moniker, url, std::nullopt, bootup_tracker_);
  return zx::ok();
}

void DriverRunner::Bind(Node& node, std::shared_ptr<BindResultTracker> result_tracker) {
  BindToUrl(node, {}, std::move(result_tracker));
}

void DriverRunner::BindToUrl(Node& node, std::string_view driver_url_suffix,
                             std::shared_ptr<BindResultTracker> result_tracker) {
  bind_manager_.Bind(node, driver_url_suffix, std::move(result_tracker));
}

void DriverRunner::RebindComposite(std::string spec, std::optional<std::string> driver_url,
                                   fit::callback<void(zx::result<>)> callback) {
  composite_node_spec_manager_.Rebind(spec, driver_url, std::move(callback));
}

void DriverRunner::RebindCompositesWithDriver(const std::string& url,
                                              fit::callback<void(size_t)> complete_callback) {
  std::unordered_set<std::string> names;
  PerformBFS(root_node_, [&names, url](const std::shared_ptr<driver_manager::Node>& current) {
    if (current->type() == driver_manager::NodeType::kComposite && current->driver_url() == url) {
      LOGF(DEBUG, "RebindCompositesWithDriver rebinding composite %s",
           current->MakeComponentMoniker().c_str());
      names.insert(current->name());
      return false;
    }

    return true;
  });

  if (names.empty()) {
    complete_callback(0);
    return;
  }

  auto complete_wrapper = [complete_callback = std::move(complete_callback),
                           count = names.size()]() mutable { complete_callback(count); };

  std::shared_ptr<AsyncSharder> sharder =
      std::make_shared<AsyncSharder>(names.size(), std::move(complete_wrapper));

  for (const auto& name : names) {
    RebindComposite(name, std::nullopt,
                    [sharder](zx::result<>) mutable { sharder->CompleteShard(); });
  }
}

void DriverRunner::DestroyDriverComponent(driver_manager::Node& node,
                                          DestroyDriverComponentCallback callback) {
  auto name = node.MakeComponentMoniker();
  fdecl::wire::ChildRef child_ref{
      .name = fidl::StringView::FromExternal(name),
      .collection = CollectionName(node.collection()),
  };
  runner_.realm()->DestroyChild(child_ref).Then(std::move(callback));
}

zx::result<DriverHost*> DriverRunner::CreateDriverHost(bool use_next_vdso) {
  auto endpoints = fidl::Endpoints<fio::Directory>::Create();
  std::string name = "driver-host-" + std::to_string(next_driver_host_id_++);

  std::shared_ptr<bool> connected = std::make_shared<bool>(false);
  auto create =
      CreateDriverHostComponent(name, std::move(endpoints.server), connected, use_next_vdso);
  if (create.is_error()) {
    return create.take_error();
  }

  auto client_end = component::ConnectAt<fdh::DriverHost>(endpoints.client);
  if (client_end.is_error()) {
    LOGF(ERROR, "Failed to connect to service '%s': %s",
         fidl::DiscoverableProtocolName<fdh::DriverHost>, client_end.status_string());
    return client_end.take_error();
  }

  auto loader_service_client = loader_service_factory_();
  if (loader_service_client.is_error()) {
    LOGF(ERROR, "Failed to connect to service fuchsia.ldsvc/Loader: %s",
         loader_service_client.status_string());
    return loader_service_client.take_error();
  }

  auto driver_host = std::make_unique<DriverHostComponent>(std::move(*client_end), dispatcher_,
                                                           &driver_hosts_, connected);
  auto result = driver_host->InstallLoader(std::move(*loader_service_client));
  if (result.is_error()) {
    LOGF(ERROR, "Failed to install loader service: %s", result.status_string());
    return result.take_error();
  }

  auto driver_host_ptr = driver_host.get();
  driver_hosts_.push_back(std::move(driver_host));

  return zx::ok(driver_host_ptr);
}

void DriverRunner::CreateDriverHostDynamicLinker(
    fit::callback<void(zx::result<DriverHost*>)> completion_cb) {
  if (!dynamic_linker_args_.has_value()) {
    LOGF(ERROR, "Dynamic linker was not available");
    completion_cb(zx::error(ZX_ERR_NOT_SUPPORTED));
    return;
  }

  auto endpoints = fidl::Endpoints<fio::Directory>::Create();

  auto client_end = component::ConnectAt<fdh::DriverHost>(endpoints.client);
  if (client_end.is_error()) {
    LOGF(ERROR, "Failed to connect to service '%s': %s",
         fidl::DiscoverableProtocolName<fdh::DriverHost>, client_end.status_string());
    completion_cb(client_end.take_error());
    return;
  }

  // TODO(https://fxbug.dev/349831408): for now we use the same driver host launcher client
  // channel for each driver host.
  if (!driver_host_launcher_.has_value()) {
    auto client = dynamic_linker_args_->linker_service_factory();
    if (client.is_error()) {
      LOGF(ERROR, "Failed to create driver host launcher client");
      completion_cb(client.take_error());
      return;
    }
    driver_host_launcher_ = fidl::WireSharedClient<fuchsia_driver_loader::DriverHostLauncher>(
        std::move(*client), dispatcher_);
  }
  std::shared_ptr<bool> connected = std::make_shared<bool>(false);
  dynamic_linker_args_->driver_host_runner->StartDriverHost(
      driver_host_launcher_->Clone(), std::move(endpoints.server), connected,
      [this, completion_cb = std::move(completion_cb), client_end = std::move(client_end),
       connected = std::move(connected)](
          zx::result<fidl::ClientEnd<fuchsia_driver_loader::DriverHost>> result) mutable {
        if (result.is_error()) {
          completion_cb(result.take_error());
          return;
        }

        auto driver_host = std::make_unique<DriverHostComponent>(
            std::move(*client_end), dispatcher_, &driver_hosts_, connected, std::move(*result));

        auto driver_host_ptr = driver_host.get();
        driver_hosts_.push_back(std::move(driver_host));
        completion_cb(zx::ok(driver_host_ptr));
      });
}

bool DriverRunner::IsDriverHostValid(DriverHost* driver_host) const {
  return driver_hosts_.find_if([driver_host](const DriverHostComponent& host) {
    return &host == driver_host;
  }) != driver_hosts_.end();
}

zx::result<std::string> DriverRunner::StartDriver(
    Node& node, fuchsia_driver_framework::wire::DriverInfo driver_info) {
  if (!driver_info.has_url()) {
    LOGF(ERROR, "Failed to start driver for node '%s', the driver URL is missing",
         node.name().c_str());
    return zx::error(ZX_ERR_INTERNAL);
  }

  auto pkg_type =
      driver_info.has_package_type() ? driver_info.package_type() : fdf::DriverPackageType::kBase;
  auto result = StartDriver(node, driver_info.url().get(), pkg_type);
  if (result.is_error()) {
    return result.take_error();
  }
  return zx::ok(std::string(driver_info.url().get()));
}

zx::result<BindSpecResult> DriverRunner::BindToParentSpec(fidl::AnyArena& arena,
                                                          CompositeParents composite_parents,
                                                          std::weak_ptr<Node> node,
                                                          bool enable_multibind) {
  return this->composite_node_spec_manager_.BindParentSpec(arena, composite_parents, node,
                                                           enable_multibind);
}

void DriverRunner::RequestMatchFromDriverIndex(
    fuchsia_driver_index::wire::MatchDriverArgs args,
    fit::callback<void(fidl::WireUnownedResult<fdi::DriverIndex::MatchDriver>&)> match_callback) {
  driver_index()->MatchDriver(args).Then(std::move(match_callback));
}

void DriverRunner::RequestRebindFromDriverIndex(std::string spec,
                                                std::optional<std::string> driver_url_suffix,
                                                fit::callback<void(zx::result<>)> callback) {
  fidl::Arena allocator;
  fidl::StringView fidl_driver_url = driver_url_suffix == std::nullopt
                                         ? fidl::StringView()
                                         : fidl::StringView(allocator, driver_url_suffix.value());
  driver_index_->RebindCompositeNodeSpec(fidl::StringView(allocator, spec), fidl_driver_url)
      .Then(
          [callback = std::move(callback)](
              fidl::WireUnownedResult<fdi::DriverIndex::RebindCompositeNodeSpec>& result) mutable {
            if (!result.ok()) {
              LOGF(ERROR, "Failed to send a composite rebind request to the Driver Index failed %s",
                   result.error().FormatDescription().c_str());
              callback(zx::error(result.status()));
              return;
            }

            if (result->is_error()) {
              callback(result->take_error());
              return;
            }
            callback(zx::ok());
          });
}

zx::result<> DriverRunner::CreateDriverHostComponent(
    std::string moniker, fidl::ServerEnd<fuchsia_io::Directory> exposed_dir,
    std::shared_ptr<bool> exposed_dir_connected, bool use_next_vdso) {
#ifdef __mist_os__
  constexpr std::string_view kUrl = "fuchsia-boot:///#meta/driver_host.cm";
  constexpr std::string_view kNextUrl = "fuchsia-boot:///#meta/driver_host_next.cm";
#else
  constexpr std::string_view kUrl = "fuchsia-boot:///driver_host#meta/driver_host.cm";
  constexpr std::string_view kNextUrl = "fuchsia-boot:///driver_host#meta/driver_host_next.cm";
#endif
  fidl::Arena arena;
  auto child_decl_builder = fdecl::wire::Child::Builder(arena)
                                .name(moniker)
                                .url(use_next_vdso ? kNextUrl : kUrl)
                                .startup(fdecl::wire::StartupMode::kLazy);
  auto child_args_builder = fcomponent::wire::CreateChildArgs::Builder(arena);
  auto open_callback =
      [moniker](fidl::WireUnownedResult<fcomponent::Realm::OpenExposedDir>& result) {
        if (!result.ok()) {
          LOGF(ERROR, "Failed to open exposed directory for driver host: '%s': %s", moniker.c_str(),
               result.FormatDescription().data());
          return;
        }
        if (result->is_error()) {
          LOGF(ERROR, "Failed to open exposed directory for driver host: '%s': %u", moniker.c_str(),
               result->error_value());
        }
      };
  auto create_callback =
      [this, moniker, exposed_dir = std::move(exposed_dir),
       exposed_dir_connected = std::move(exposed_dir_connected),
       open_callback = std::move(open_callback)](
          fidl::WireUnownedResult<fcomponent::Realm::CreateChild>& result) mutable {
        if (!result.ok()) {
          LOGF(ERROR, "Failed to create driver host '%s': %s", moniker.c_str(),
               result.error().FormatDescription().data());
          return;
        }
        if (result->is_error()) {
          LOGF(ERROR, "Failed to create driver host '%s': %u", moniker.c_str(),
               result->error_value());
          return;
        }
        fdecl::wire::ChildRef child_ref{
            .name = fidl::StringView::FromExternal(moniker),
            .collection = "driver-hosts",
        };
        runner_.realm()
            ->OpenExposedDir(child_ref, std::move(exposed_dir))
            .ThenExactlyOnce(std::move(open_callback));
        *exposed_dir_connected = true;
      };
  runner_.realm()
      ->CreateChild(
          fdecl::wire::CollectionRef{
              .name = "driver-hosts",
          },
          child_decl_builder.Build(), child_args_builder.Build())
      .Then(std::move(create_callback));
  return zx::ok();
}

zx::result<uint32_t> DriverRunner::RestartNodesColocatedWithDriverUrl(
    std::string_view url, fdd::RestartRematchFlags rematch_flags) {
  auto driver_hosts = DriverHostsWithDriverUrl(url);

  // Perform a BFS over the node topology, if the current node's host is one of the driver_hosts
  // we collected, then restart that node and skip its children since they will go away
  // as part of it's restart.
  //
  // The BFS ensures that we always find the topmost node of a driver host.
  // This node will by definition have colocated set to false, so when we call StartDriver
  // on this node we will always create a new driver host. The old driver host will go away
  // on its own asynchronously since it is drained from all of its drivers.
  PerformBFS(root_node_, [this, &driver_hosts, rematch_flags,
                          url](const std::shared_ptr<driver_manager::Node>& current) {
    if (driver_hosts.find(current->driver_host()) == driver_hosts.end()) {
      // Not colocated with one of the restarting hosts. Continue to visit the children.
      return true;
    }

    if (current->EvaluateRematchFlags(rematch_flags, url)) {
      if (current->type() == driver_manager::NodeType::kComposite) {
        // Composites need to go through a different flow that will fully remove the
        // node and empty out the composite spec management layer.
        LOGF(DEBUG, "RestartNodesColocatedWithDriverUrl rebinding composite %s",
             current->MakeComponentMoniker().c_str());
        RebindComposite(current->name(), std::nullopt, [](zx::result<>) {});
        return false;
      }

      // Non-composite nodes use the restart with rematch flow.
      LOGF(DEBUG, "RestartNodesColocatedWithDriverUrl restarting node with rematch %s",
           current->MakeComponentMoniker().c_str());
      current->RestartNodeWithRematch();
      return false;
    }

    // Not rematching, plain node restart.
    LOGF(DEBUG, "RestartNodesColocatedWithDriverUrl restarting node %s",
         current->MakeComponentMoniker().c_str());
    current->RestartNode();
    return false;
  });

  return zx::ok(static_cast<uint32_t>(driver_hosts.size()));
}

void DriverRunner::RestartWithDictionary(fidl::StringView moniker,
                                         fuchsia_component_sandbox::wire::DictionaryRef dictionary,
                                         zx::eventpair reset_eventpair) {
  uint64_t imported = cap_id_++;
  capability_store_
      ->Import(imported,
               fuchsia_component_sandbox::wire::Capability::WithDictionary(std::move(dictionary)))
      .Then([this, moniker = std::string(moniker.get()),
             reset_eventpair = std::move(reset_eventpair),
             imported](fidl::WireUnownedResult<fuchsia_component_sandbox::CapabilityStore::Import>&
                           result) mutable {
        if (!result.ok() || result->is_error()) {
          LOGF(ERROR, "RestartWithDictionary failed to import the dictionary.");
          return;
        }

        std::shared_ptr<driver_manager::Node> restarted_node = nullptr;
        PerformBFS(root_node_, [&](const std::shared_ptr<driver_manager::Node>& current) {
          if (current->MakeComponentMoniker() == moniker) {
            if (current->dictionary_ref()) {
              LOGF(
                  ERROR,
                  "RestartWithDictionary requested node id already contains a dictionary_ref from another RestartWithDictionary operation.");
              return false;
            }
            ZX_ASSERT_MSG(restarted_node == nullptr,
                          "Multiple nodes with same moniker not possible.");
            restarted_node = current;
            current->SetDictionaryRef(imported);
            current->RestartNode();
            return false;
          }

          return true;
        });

        if (restarted_node != nullptr) {
          std::unique_ptr<async::WaitOnce> wait = std::make_unique<async::WaitOnce>(
              reset_eventpair.release(), ZX_EVENTPAIR_PEER_CLOSED | ZX_EVENTPAIR_SIGNALED);
          async::WaitOnce* wait_ptr = wait.get();
          zx_status_t status = wait_ptr->Begin(
              dispatcher_,
              [restarted_node = std::move(restarted_node), moved_wait = std::move(wait)](
                  async_dispatcher_t* dispatcher, async::WaitOnce* wait, zx_status_t status,
                  const zx_packet_signal_t* signal) {
                LOGF(INFO, "RestartWithDictionary operation released.");
                restarted_node->SetDictionaryRef(std::nullopt);
                restarted_node->RestartNode();
              });

          if (status != ZX_OK) {
            LOGF(ERROR, "Failed to Begin async::Wait for RestartWithDictionary.");
          }
        }
      });
}

std::unordered_set<const DriverHost*> DriverRunner::DriverHostsWithDriverUrl(std::string_view url) {
  std::unordered_set<const DriverHost*> result_hosts;

  // Perform a BFS over the node topology, if the current node's driver url is the url we are
  // interested in, add the driver host it is in to the result set.
  PerformBFS(root_node_,
             [&result_hosts, url](const std::shared_ptr<driver_manager::Node>& current) {
               if (current->driver_url() == url) {
                 result_hosts.insert(current->driver_host());
               }
               return true;
             });

  return result_hosts;
}

}  // namespace driver_manager
