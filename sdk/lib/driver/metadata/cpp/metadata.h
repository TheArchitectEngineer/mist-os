// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_METADATA_CPP_METADATA_H_
#define LIB_DRIVER_METADATA_CPP_METADATA_H_

#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/driver/logging/cpp/structured_logger.h>

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

namespace fdf_metadata {

// Connects to the fuchsia.driver.metadata/Metadata FIDL protocol found within the |incoming|
// incoming namespace at FIDL service |service_name| and instance |instance_name|.
zx::result<fidl::ClientEnd<fuchsia_driver_metadata::Metadata>> ConnectToMetadataProtocol(
    const std::shared_ptr<fdf::Namespace>& incoming, std::string_view service_name,
    std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance);

// Retrieves metadata from the fuchsia.driver.metadata/Metadata FIDL protocol within the |incoming|
// incoming namespace found at FIDL service |service_name| and instance |instance_name|.
//
// Make sure that the component manifest specifies that it uses the `FidlType::kSerializableName`
// FIDL service.
template <typename FidlType>
zx::result<FidlType> GetMetadataFromFidlService(
    const std::shared_ptr<fdf::Namespace>& incoming, std::string_view service_name,
    std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
  static_assert(fidl::IsFidlType<FidlType>::value, "|FidlType| must be a FIDL domain object.");
  static_assert(!fidl::IsResource<FidlType>::value,
                "|FidlType| cannot be a resource type. Resources cannot be persisted.");

  fidl::WireSyncClient<fuchsia_driver_metadata::Metadata> client{};
  {
    zx::result result = ConnectToMetadataProtocol(incoming, service_name, instance_name);
    if (result.is_error()) {
      FDF_SLOG(ERROR, "Failed to connect to metadata server.",
               KV("status", result.status_string()));
      return result.take_error();
    }
    client.Bind(std::move(result.value()));
  }

  fidl::WireResult<fuchsia_driver_metadata::Metadata::GetPersistedMetadata> persisted_metadata =
      client->GetPersistedMetadata();
  if (!persisted_metadata.ok()) {
    FDF_SLOG(ERROR, "Failed to send GetPersistedMetadata request.",
             KV("status", persisted_metadata.status_string()));
    return zx::error(persisted_metadata.status());
  }
  if (persisted_metadata->is_error()) {
    FDF_SLOG(ERROR, "Failed to get persisted metadata.",
             KV("status", zx_status_get_string(persisted_metadata->error_value())));
    return zx::error(persisted_metadata->error_value());
  }

  fit::result metadata =
      fidl::Unpersist<FidlType>(persisted_metadata.value()->persisted_metadata.get());
  if (metadata.is_error()) {
    FDF_SLOG(ERROR, "Failed to unpersist metadata.",
             KV("status", zx_status_get_string(metadata.error_value().status())));
    return zx::error(metadata.error_value().status());
  }

  return zx::ok(metadata.value());
}

// The same as `fdf_metadata::GetMetadataFromFidlService()` except that the service name is assumed
// to be `FidlType::kSerializableName`. Make sure that `FidlType` is annotated with `@serializable`.
template <typename FidlType>
zx::result<FidlType> GetMetadata(
    const std::shared_ptr<fdf::Namespace>& incoming,
    std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
  return GetMetadataFromFidlService<FidlType>(incoming, FidlType::kSerializableName, instance_name);
}

// This function is the same as `fdf_metadata::GetMetadata<FidlType>()` except that it will return a
// `std::nullopt` if there is no metadata FIDL protocol within |device|'s incoming namespace at
// |instance_name| or if the FIDL server does not have metadata to provide.
template <typename FidlType>
zx::result<std::optional<FidlType>> GetMetadataFromFidlServiceIfExists(
    const std::shared_ptr<fdf::Namespace>& incoming, std::string_view service_name,
    std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
  static_assert(fidl::IsFidlType<FidlType>::value, "|FidlType| must be a FIDL domain object.");
  static_assert(!fidl::IsResource<FidlType>::value,
                "|FidlType| cannot be a resource type. Resources cannot be persisted.");

  fidl::WireSyncClient<fuchsia_driver_metadata::Metadata> client{};
  {
    zx::result result = ConnectToMetadataProtocol(incoming, service_name, instance_name);
    if (result.is_error()) {
      FDF_SLOG(DEBUG, "Failed to connect to metadata server.",
               KV("status", result.status_string()));
      return zx::ok(std::nullopt);
    }
    client.Bind(std::move(result.value()));
  }

  fidl::WireResult<fuchsia_driver_metadata::Metadata::GetPersistedMetadata> persisted_metadata =
      client->GetPersistedMetadata();
  if (!persisted_metadata.ok()) {
    if (persisted_metadata.status() == ZX_ERR_PEER_CLOSED) {
      // We assume that the metadata does not exist because we assume that the FIDL server does not
      // exist because we received a peer closed status.
      FDF_SLOG(DEBUG, "Failed to send GetPersistedMetadata request.",
               KV("status", persisted_metadata.status_string()));
      return zx::ok(std::nullopt);
    }
    FDF_SLOG(ERROR, "Failed to send GetPersistedMetadata request.",
             KV("status", persisted_metadata.status_string()));
    return zx::error(persisted_metadata.status());
  }
  if (persisted_metadata->is_error()) {
    if (persisted_metadata->error_value() == ZX_ERR_NOT_FOUND) {
      FDF_SLOG(DEBUG, "Failed to get persisted metadata.",
               KV("status", zx_status_get_string(persisted_metadata->error_value())));
      return zx::ok(std::nullopt);
    }
    FDF_SLOG(ERROR, "Failed to get persisted metadata.",
             KV("status", zx_status_get_string(persisted_metadata->error_value())));
    return zx::error(persisted_metadata->error_value());
  }

  fit::result metadata =
      fidl::Unpersist<FidlType>(persisted_metadata.value()->persisted_metadata.get());
  if (metadata.is_error()) {
    FDF_SLOG(ERROR, "Failed to unpersist metadata.",
             KV("status", zx_status_get_string(metadata.error_value().status())));
    return zx::error(metadata.error_value().status());
  }

  return zx::ok(metadata.value());
}

// The same as `fdf_metadata::GetMetadataFromFidlServiceIfExists()` except that the service name is
// assumed to be `FidlType::kSerializableName`. Make sure that `FidlType` is annotated with
// `@serializable`.
template <typename FidlType>
zx::result<std::optional<FidlType>> GetMetadataIfExists(
    const std::shared_ptr<fdf::Namespace>& incoming,
    std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
  return GetMetadataFromFidlServiceIfExists<FidlType>(incoming, FidlType::kSerializableName,
                                                      instance_name);
}

}  // namespace fdf_metadata

#endif

#endif  // LIB_DRIVER_METADATA_CPP_METADATA_H_
