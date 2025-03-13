// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_METADATA_CPP_METADATA_SERVER_H_
#define LIB_DRIVER_METADATA_CPP_METADATA_SERVER_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/logging/cpp/structured_logger.h>
#include <lib/driver/metadata/cpp/metadata.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/platform-device/cpp/pdev.h>

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

namespace fdf_metadata {

// Serves metadata that can be retrieved using `fdf_metadata::GetMetadata<|FidlType|>()`.
// As an example, lets say there exists a FIDL type `fuchsia.hardware.test/Metadata` to be sent from
// a driver to its child driver:
//
//   library fuchsia.hardware.test;
//
//   // Make sure to annotate the type with `@serializable`.
//   @serializable
//   type Metadata = table {
//       1: test_property string:MAX;
//   };
//
// The parent driver can define a `MetadataServer<fuchsia_hardware_test::Metadata>` server
// instance as one its members:
//
//   class ParentDriver : public fdf::DriverBase {
//    private:
//     fdf_metadata::MetadataServer<fuchsia_hardware_test::Metadata> metadata_server_;
//   }
//
// When the parent driver creates a child node, it can offer the metadata server's service to the
// child node by adding the metadata server's offers to the node-add arguments:
//
//   auto args = fuchsia_driver_framework::NodeAddArgs args{{.offers2 =
//     std::vector{metadata_server_.MakeOffer()}}};
//
// The parent driver should also declare the metadata server's capability and offer it in the
// driver's component manifest like so:
//
//   capabilities: [
//     { service: "fuchsia.hardware.test.Metadata" },
//   ],
//   expose: [
//     {
//       service: "fuchsia.hardware.test.Metadata",
//       from: "self",
//     },
//   ],
//
template <typename FidlType>
class MetadataServer final : public fidl::WireServer<fuchsia_driver_metadata::Metadata> {
 public:
  // The caller's component manifest must specify `|FidlType|::kSerializableName` as a service
  // capability and expose it. Otherwise, other components will not be able to retrieve metadata.
  explicit MetadataServer(
      std::string instance_name = component::OutgoingDirectory::kDefaultServiceInstance)
      : instance_name_(std::move(instance_name)) {}

  // Set the metadata to be served to |metadata|. |metadata| must be persistable.
  zx::result<> SetMetadata(const FidlType& metadata) {
    static_assert(fidl::IsFidlType<FidlType>::value, "|FidlType| must be a FIDL domain object.");
    static_assert(!fidl::IsResource<FidlType>::value,
                  "|FidlType| cannot be a resource type. Resources cannot be persisted.");

    fit::result persisted_metadata = fidl::Persist(metadata);
    if (persisted_metadata.is_error()) {
      FDF_SLOG(ERROR, "Failed to persist metadata.",
               KV("status", persisted_metadata.error_value().status_string()));
      return zx::error(persisted_metadata.error_value().status());
    }
    persisted_metadata_.emplace(std::move(persisted_metadata.value()));

    return zx::ok();
  }

  // Retrieves persisted metadata from |pdev| associated with the metadata ID
  // `|FidlType|::kSerializableName`. Assumes that the metadata from the platform device is a
  // persisted |FidlType|. Returns false if the metadata was not found. Returns true if otherwise.
  zx::result<bool> SetMetadataFromPDevIfExists(
      fidl::UnownedClientEnd<fuchsia_hardware_platform_device::Device> pdev) {
    fidl::WireResult result = fidl::WireCall(pdev)->GetMetadata(
        fidl::StringView::FromExternal(FidlType::kSerializableName));
    if (!result.ok()) {
      FDF_LOG(ERROR, "Failed to send GetMetadata request: %s", result.status_string());
      return zx::error(result.status());
    }
    if (result->is_error()) {
      if (result->error_value() == ZX_ERR_NOT_FOUND) {
        return zx::ok(false);
      }
      FDF_LOG(ERROR, "Failed to get metadata: %s", zx_status_get_string(result->error_value()));
      return zx::error(result->error_value());
    }
    const auto persisted_metadata = result.value()->metadata.get();
    persisted_metadata_.emplace();
    persisted_metadata_->assign(persisted_metadata.begin(), persisted_metadata.end());

    return zx::ok(true);
  }

  // See
  // `SetMetadataFromPDevIfExists(fidl::UnownedClientEnd<fuchsia_hardware_platform_device::Device>)`
  // for more details.
  zx::result<bool> SetMetadataFromPDevIfExists(
      fidl::ClientEnd<fuchsia_hardware_platform_device::Device>& pdev) {
    return SetMetadataFromPDevIfExists(pdev.borrow());
  }

  // See
  // `SetMetadataFromPDevIfExists(fidl::UnownedClientEnd<fuchsia_hardware_platform_device::Device>)`
  // for more details.
  zx::result<bool> SetMetadataFromPDevIfExists(fdf::PDev& pdev) {
    return SetMetadataFromPDevIfExists(pdev.borrow());
  }

  // Sets the metadata to be served to the metadata found in |incoming|.
  //
  // If the metadata found in |incoming| changes after this function has been called then those
  // changes will not be reflected in the metadata to be served.
  //
  // Make sure that the component manifest specifies that is uses the `FidlType::kSerializableName`
  // FIDL service.
  zx::result<> ForwardMetadata(
      const std::shared_ptr<fdf::Namespace>& incoming,
      std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
    fidl::WireSyncClient<fuchsia_driver_metadata::Metadata> client{};
    {
      zx::result result =
          ConnectToMetadataProtocol(incoming, FidlType::kSerializableName, instance_name);
      if (result.is_error()) {
        FDF_SLOG(ERROR, "Failed to connect to metadata server.",
                 KV("status", result.status_string()));
        return result.take_error();
      }
      client.Bind(std::move(result.value()));
    }

    fidl::WireResult<fuchsia_driver_metadata::Metadata::GetPersistedMetadata> result =
        client->GetPersistedMetadata();
    if (!result.ok()) {
      FDF_SLOG(ERROR, "Failed to send GetPersistedMetadata request.",
               KV("status", result.status_string()));
      return zx::error(result.status());
    }
    if (result->is_error()) {
      FDF_SLOG(ERROR, "Failed to get persisted metadata.",
               KV("status", zx_status_get_string(result->error_value())));
      return result->take_error();
    }
    cpp20::span<uint8_t> persisted_metadata = result.value()->persisted_metadata.get();
    std::vector<uint8_t> copy;
    copy.insert(copy.begin(), persisted_metadata.begin(), persisted_metadata.end());
    persisted_metadata_.emplace(std::move(copy));

    return zx::ok();
  }

  // Similar to `ForwardMetadata()` except that it will return false if it fails to connect to the
  // incoming metadata server or if the incoming metadata server does not have metadata to provide.
  // Returns true otherwise.
  zx::result<bool> ForwardMetadataIfExists(
      const std::shared_ptr<fdf::Namespace>& incoming,
      std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
    fidl::WireSyncClient<fuchsia_driver_metadata::Metadata> client{};
    {
      zx::result result =
          ConnectToMetadataProtocol(incoming, FidlType::kSerializableName, instance_name);
      if (result.is_error()) {
        FDF_SLOG(DEBUG, "Failed to connect to metadata server.",
                 KV("status", result.status_string()));
        return zx::ok(false);
      }
      client.Bind(std::move(result.value()));
    }

    fidl::WireResult<fuchsia_driver_metadata::Metadata::GetPersistedMetadata> result =
        client->GetPersistedMetadata();
    if (!result.ok()) {
      if (result.status() == ZX_ERR_PEER_CLOSED) {
        // We assume that the metadata does not exist because we assume that the FIDL server does
        // not exist because we received a peer closed status.
        FDF_SLOG(DEBUG, "Failed to send GetPersistedMetadata request.",
                 KV("status", result.status_string()));
        return zx::ok(false);
      }
      FDF_SLOG(ERROR, "Failed to send GetPersistedMetadata request.",
               KV("status", result.status_string()));
      return zx::error(result.status());
    }
    if (result->is_error()) {
      if (result->error_value() == ZX_ERR_NOT_FOUND) {
        FDF_SLOG(DEBUG, "Failed to get persisted metadata.",
                 KV("status", zx_status_get_string(result->error_value())));
        return zx::ok(false);
      }
      FDF_SLOG(ERROR, "Failed to get persisted metadata.",
               KV("status", zx_status_get_string(result->error_value())));
      return result->take_error();
    }
    cpp20::span<uint8_t> persisted_metadata = result.value()->persisted_metadata.get();
    std::vector<uint8_t> copy;
    copy.insert(copy.begin(), persisted_metadata.begin(), persisted_metadata.end());
    persisted_metadata_.emplace(std::move(copy));

    return zx::ok(true);
  }

  zx::result<> Serve(fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher) {
    return Serve(outgoing.component(), dispatcher);
  }

  // Serves the fuchsia.driver.metadata/Service service to |outgoing| under the service name
  // `|FidlType|::kSerializableName` and instance name `MetadataServer::instance_name_`.
  zx::result<> Serve(component::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher) {
    fuchsia_driver_metadata::Service::InstanceHandler handler{
        {.metadata = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure)}};
    zx::result result =
        outgoing.AddService(std::move(handler), FidlType::kSerializableName, instance_name_);
    if (result.is_error()) {
      FDF_SLOG(ERROR, "Failed to add service.", KV("status", result.status_string()));
      return result.take_error();
    }
    return zx::ok();
  }

  // Creates an offer for this `MetadataServer` instance's fuchsia.driver.metadata/Service
  // service.
  fuchsia_driver_framework::Offer MakeOffer() {
    return fuchsia_driver_framework::Offer::WithZirconTransport(
        fdf::MakeOffer(FidlType::kSerializableName, instance_name_));
  }

  fuchsia_driver_framework::wire::Offer MakeOffer(fidl::AnyArena& arena) {
    return fuchsia_driver_framework::wire::Offer::WithZirconTransport(
        arena, fdf::MakeOffer(arena, FidlType::kSerializableName, instance_name_));
  }

 private:
  // fuchsia.driver.metadata/Metadata protocol implementation.
  void GetPersistedMetadata(GetPersistedMetadataCompleter::Sync& completer) override {
    if (!persisted_metadata_.has_value()) {
      FDF_LOG(WARNING, "Metadata not set");
      completer.ReplyError(ZX_ERR_NOT_FOUND);
      return;
    }
    completer.ReplySuccess(fidl::VectorView<uint8_t>::FromExternal(persisted_metadata_.value()));
  }

  fidl::ServerBindingGroup<fuchsia_driver_metadata::Metadata> bindings_;

  // Persisted metadata that will be served in this instance's fuchsia.driver.metadata/Metadata
  // protocol.
  std::optional<std::vector<uint8_t>> persisted_metadata_;

  // Name of the instance directory that will serve this instance's fuchsia.driver.metadata/Service
  // service.
  std::string instance_name_;
};

}  // namespace fdf_metadata

#endif

#endif  // LIB_DRIVER_METADATA_CPP_METADATA_SERVER_H_
