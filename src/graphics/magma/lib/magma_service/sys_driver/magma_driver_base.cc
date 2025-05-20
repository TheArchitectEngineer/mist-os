// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "magma_driver_base.h"

namespace msd {

zx::result<> MagmaDriverBase::Start() {
  teardown_logger_callback_ =
      magma::InitializePlatformLoggerForDFv2(&logger(), std::string(name()));

  if (zx::result result = MagmaStart(); result.is_error()) {
    node().reset();
    return result.take_error();
  }

  InitializeInspector();

  node_client_.Bind(std::move(node()));

  auto defer_teardown = fit::defer([this]() { node_client_ = {}; });

  if (zx::result result = perf_counter_.Create(node_client_); result.is_error()) {
    return result.take_error();
  }
  {
    std::lock_guard lock(magma_->magma_mutex);
    magma_->magma_system_device->set_perf_count_access_token_id(perf_counter_.GetEventKoid());
  }

  if (zx::result result = dependency_injection_.Create(node_client_); result.is_error()) {
    return result.take_error();
  }

  if (zx::result result = CreateDevfsNode(); result.is_error()) {
    return result.take_error();
  }
  if (zx::result result = CreateAdditionalDevNodes(); result.is_error()) {
    return result.take_error();
  }
  MAGMA_LOG(INFO, "MagmaDriverBase::Start completed for MSD %s", std::string(name()).c_str());
  defer_teardown.cancel();
  return zx::ok();
}

void MagmaDriverBase::Stop() {
  std::lock_guard lock(magma_->magma_mutex);
  if (magma_->magma_system_device) {
    magma_->magma_system_device->Shutdown();
  }
  magma_->magma_system_device.reset();
  magma_->magma_driver.reset();
  teardown_logger_callback_.call();
}

void MagmaDriverBase::GetClockSpeedLevel(
    ::fuchsia_gpu_magma::wire::PowerElementProviderGetClockSpeedLevelRequest* request,
    GetClockSpeedLevelCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void MagmaDriverBase::SetClockLimit(
    ::fuchsia_gpu_magma::wire::PowerElementProviderSetClockLimitRequest* request,
    SetClockLimitCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}
void MagmaDriverBase::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_gpu_magma::PowerElementProvider> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {}

zx::result<zx::resource> MagmaDriverBase::GetInfoResource() {
  auto info_resource = incoming()->template Connect<fuchsia_kernel::InfoResource>();

  if (info_resource.is_error()) {
    MAGMA_LOG(INFO, "Error requesting info resource: %s", info_resource.status_string());
    return info_resource.take_error();
  }
  auto info_resource_client = fidl::WireSyncClient(std::move(*info_resource));
  auto result = info_resource_client->Get();
  if (!result.ok()) {
    MAGMA_LOG(INFO, "Protocol error calling InfoResource.Get(): %s", result.status_string());
    return zx::error(result.error().status());
  }
  return zx::ok(std::move(result->resource));
}

void MagmaDriverBase::set_magma_driver(std::unique_ptr<msd::Driver> magma_driver)
    FIT_REQUIRES(magma_->magma_mutex) {
  ZX_DEBUG_ASSERT(!magma_->magma_driver);
  magma_->magma_driver = std::move(magma_driver);
}

void MagmaDriverBase::set_magma_system_device(
    std::unique_ptr<MagmaSystemDevice> magma_system_device) FIT_REQUIRES(magma_->magma_mutex) {
  ZX_DEBUG_ASSERT(!magma_->magma_system_device);
  magma_->magma_system_device = std::move(magma_system_device);
}

MagmaSystemDevice* MagmaDriverBase::magma_system_device() FIT_REQUIRES(magma_->magma_mutex) {
  return magma_->magma_system_device.get();
}

zx::result<> MagmaDriverBase::CreateTestService(MagmaTestServer& test_server) {
  auto power_protocol =
      [this](fidl::ServerEnd<fuchsia_gpu_magma::PowerElementProvider> server_end) mutable {
        fidl::BindServer(dispatcher(), std::move(server_end), this);
      };
  auto device_protocol =
      [this](fidl::ServerEnd<fuchsia_gpu_magma::CombinedDevice> server_end) mutable {
        fidl::BindServer(dispatcher(), std::move(server_end), &combined_device_server_);
      };
  auto test_protocol =
      [this, &test_server](fidl::ServerEnd<fuchsia_gpu_magma::TestDevice2> server_end) mutable {
        fidl::BindServer(dispatcher(), std::move(server_end), &test_server);
      };

  fuchsia_gpu_magma::TestService::InstanceHandler handler({
      .device = std::move(device_protocol),
      .power_element_provider = std::move(power_protocol),
      .test_device = std::move(test_protocol),
  });
  {
    auto status =
        outgoing()->template AddService<fuchsia_gpu_magma::TestService>(std::move(handler));
    if (status.is_error()) {
      FDF_LOG(ERROR, "%s(): Failed to add service to outgoing directory: %s\n", __func__,
              status.status_string());
      return status.take_error();
    }
  }
  return zx::ok();
}

zx::result<> MagmaDriverBase::CreateDevfsNode() {
  fidl::Arena arena;
  zx::result connector = magma_devfs_connector_.Bind(dispatcher());
  if (connector.is_error()) {
    node_client_ = {};
    return connector.take_error();
  }

  auto devfs = fuchsia_driver_framework::wire::DevfsAddArgs::Builder(arena)
                   .connector(std::move(connector.value()))
                   .class_name("gpu");

  auto args = fuchsia_driver_framework::wire::NodeAddArgs::Builder(arena)
                  .name(arena, "magma_gpu")
                  .devfs_args(devfs.Build())
                  .Build();

  auto controller_endpoints = fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  zx::result node_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
  ZX_ASSERT_MSG(node_endpoints.is_ok(), "Failed: %s", node_endpoints.status_string());

  fidl::WireResult result = node_client_->AddChild(args, std::move(controller_endpoints.server),
                                                   std::move(node_endpoints->server));
  gpu_node_controller_.Bind(std::move(controller_endpoints.client));
  gpu_node_.Bind(std::move(node_endpoints->client));
  // Add the gpu service.
  {
    auto power_protocol =
        [this](fidl::ServerEnd<fuchsia_gpu_magma::PowerElementProvider> server_end) mutable {
          fidl::BindServer(dispatcher(), std::move(server_end), this);
        };
    auto device_protocol =
        [this](fidl::ServerEnd<fuchsia_gpu_magma::CombinedDevice> server_end) mutable {
          fidl::BindServer(dispatcher(), std::move(server_end), &combined_device_server_);
        };

    fuchsia_gpu_magma::Service::InstanceHandler handler(
        {.device = std::move(device_protocol),
         .power_element_provider = std::move(power_protocol)});
    {
      auto status = outgoing()->template AddService<fuchsia_gpu_magma::Service>(std::move(handler));
      if (status.is_error()) {
        FDF_LOG(ERROR, "%s(): Failed to add service to outgoing directory: %s\n", __func__,
                status.status_string());
        return status.take_error();
      }
    }
  }

  // Add the trusted gpu service.
  {
    auto power_protocol =
        [this](fidl::ServerEnd<fuchsia_gpu_magma::PowerElementProvider> server_end) mutable {
          fidl::BindServer(dispatcher(), std::move(server_end), this);
        };
    auto debug_utils_protocol =
        [this](fidl::ServerEnd<fuchsia_gpu_magma::DebugUtils> server_end) mutable {
          fidl::BindServer(dispatcher(), std::move(server_end), this);
        };
    auto device_protocol =
        [this](fidl::ServerEnd<fuchsia_gpu_magma::CombinedDevice> server_end) mutable {
          fidl::BindServer(dispatcher(), std::move(server_end), &trusted_combined_device_server_);
        };

    fuchsia_gpu_magma::TrustedService::InstanceHandler handler(
        {.device = std::move(device_protocol),
         .power_element_provider = std::move(power_protocol),
         .debug_utils = std::move(debug_utils_protocol)});
    {
      auto status =
          outgoing()->template AddService<fuchsia_gpu_magma::TrustedService>(std::move(handler));
      if (status.is_error()) {
        FDF_LOG(ERROR, "%s(): Failed to add service to outgoing directory: %s\n", __func__,
                status.status_string());
        return status.take_error();
      }
    }
  }
  return zx::ok();
}

void MagmaDriverBase::InitializeInspector() {
  std::lock_guard lock(magma_->magma_mutex);
  auto inspector = magma_driver()->DuplicateInspector();
  if (inspector) {
    InitInspectorExactlyOnce(inspector.value());
  }
}

void MagmaDriverBase::SetMemoryPressureLevel(MagmaMemoryPressureLevel level) {
  std::lock_guard lock(magma_->magma_mutex);
  MAGMA_DASSERT(magma_->magma_system_device);
  magma_->magma_system_device->SetMemoryPressureLevel(level);
}

void MagmaCombinedDeviceServer::Query(QueryRequestView request, QueryCompleter::Sync& _completer) {
  MAGMA_DLOG("MagmaDriverBase::Query");
  std::lock_guard lock(magma_->magma_mutex);
  if (!CheckSystemDevice(_completer))
    return;

  zx_handle_t result_buffer = ZX_HANDLE_INVALID;
  uint64_t result = 0;

  magma::Status status = magma_->magma_system_device->Query(fidl::ToUnderlying(request->query_id),
                                                            &result_buffer, &result);
  if (!status.ok()) {
    _completer.ReplyError(magma::ToZxStatus(status.get()));
    return;
  }

  if (result_buffer != ZX_HANDLE_INVALID) {
    _completer.ReplySuccess(
        fuchsia_gpu_magma::wire::DeviceQueryResponse::WithBufferResult(zx::vmo(result_buffer)));
  } else {
    _completer.ReplySuccess(fuchsia_gpu_magma::wire::DeviceQueryResponse::WithSimpleResult(
        fidl::ObjectView<uint64_t>::FromExternal(&result)));
  }
}

void MagmaCombinedDeviceServer::Connect2(Connect2RequestView request,
                                         Connect2Completer::Sync& _completer) {
  MAGMA_DLOG("MagmaDriverBase::Connect2");
  std::lock_guard lock(magma_->magma_mutex);
  if (!CheckSystemDevice(_completer))
    return;

  auto connection =
      magma_->magma_system_device->Open(request->client_id, std::move(request->primary_channel),
                                        std::move(request->notification_channel), client_type_);

  if (!connection) {
    MAGMA_DLOG("MagmaSystemDevice::Open failed");
    _completer.Close(ZX_ERR_INTERNAL);
    return;
  }

  magma_->magma_system_device->StartConnectionThread(
      std::move(connection), [](const char* role_name) {
        zx_status_t status = fuchsia_scheduler::SetRoleForThisThread(role_name);
        if (status != ZX_OK) {
          MAGMA_DMESSAGE("Failed to set role for this thread; status: %s",
                         zx_status_get_string(status));
          return;
        }
      });
}

void MagmaCombinedDeviceServer::DumpState(DumpStateRequestView request,
                                          DumpStateCompleter::Sync& _completer) {
  MAGMA_DLOG("MagmaDriverBase::DumpState");
  std::lock_guard lock(magma_->magma_mutex);
  if (!CheckSystemDevice(_completer))
    return;
  if (request->dump_type & ~MAGMA_DUMP_TYPE_NORMAL) {
    MAGMA_DLOG("Invalid dump type %d", request->dump_type);
    return;
  }

  if (magma_->magma_system_device) {
    magma_->magma_system_device->DumpStatus(request->dump_type);
  }
}

void MagmaCombinedDeviceServer::GetIcdList(GetIcdListCompleter::Sync& completer) {
  std::lock_guard lock(magma_->magma_mutex);
  if (!CheckSystemDevice(completer))
    return;
  fidl::Arena allocator;
  std::vector<msd::MsdIcdInfo> msd_icd_infos;
  magma_->magma_system_device->GetIcdList(&msd_icd_infos);
  std::vector<fuchsia_gpu_magma::wire::IcdInfo> icd_infos;
  for (auto& item : msd_icd_infos) {
    auto icd_info = fuchsia_gpu_magma::wire::IcdInfo::Builder(allocator);
    icd_info.component_url(fidl::StringView::FromExternal(item.component_url));
    fuchsia_gpu_magma::wire::IcdFlags flags;
    if (item.support_flags & ICD_SUPPORT_FLAG_VULKAN)
      flags |= fuchsia_gpu_magma::wire::IcdFlags::kSupportsVulkan;
    if (item.support_flags & ICD_SUPPORT_FLAG_OPENCL)
      flags |= fuchsia_gpu_magma::wire::IcdFlags::kSupportsOpencl;
    if (item.support_flags & ICD_SUPPORT_FLAG_MEDIA_CODEC_FACTORY)
      flags |= fuchsia_gpu_magma::wire::IcdFlags::kSupportsMediaCodecFactory;
    icd_info.flags(flags);
    icd_infos.push_back(icd_info.Build());
  }

  completer.Reply(fidl::VectorView<fuchsia_gpu_magma::wire::IcdInfo>::FromExternal(icd_infos));
}

void MagmaDriverBase::SetPowerState(
    fuchsia_gpu_magma::wire::DebugUtilsSetPowerStateRequest* request,
    fidl::WireServer<::fuchsia_gpu_magma::DebugUtils>::SetPowerStateCompleter::Sync& completer) {
  std::lock_guard lock(magma_mutex());

  magma_system_device()->SetPowerState(
      request->power_state, [completer = completer.ToAsync()](magma_status_t status) mutable {
        if (status == MAGMA_STATUS_OK) {
          completer.ReplySuccess();
        } else {
          completer.ReplyError(ZX_ERR_INTERNAL);
        }
      });
}

}  // namespace msd
