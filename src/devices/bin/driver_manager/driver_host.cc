// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/driver_host.h"

#include <lib/driver/component/cpp/internal/start_args.h>

#include <memory>

#include "lib/vfs/cpp/pseudo_file.h"
#include "src/devices/bin/driver_manager/node_property_conversion.h"
#include "src/devices/bin/driver_manager/pkg_utils.h"
#include "src/devices/lib/log/log.h"
#include "src/lib/fsl/handles/object_info.h"

namespace fdh = fuchsia_driver_host;
namespace frunner = fuchsia_component_runner;
namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf
namespace driver_manager {

namespace {
std::unique_ptr<vfs::PseudoFile> CreateReadonlyFile(
    fit::function<zx::result<std::string>()> content_producer) {
  auto read_fn = [content_producer = std::move(content_producer)](std::vector<uint8_t>* output,
                                                                  size_t max_file_size) {
    zx::result<std::string> contents = content_producer();
    if (contents.is_error()) {
      return contents.status_value();
    }
    output->resize(contents->length());
    std::copy(contents->begin(), contents->end(), output->begin());
    return ZX_OK;
  };

  return std::make_unique<vfs::PseudoFile>(30, std::move(read_fn));
}

std::string_view GetFilename(std::string_view path) {
  size_t index = path.rfind('/');
  return index == std::string_view::npos ? path : path.substr(index + 1);
}

}  // namespace

static constexpr std::string_view kCompatDriverRelativePath = "driver/compat.so";

// static
zx::result<DriverHost::DriverLoadArgs> DriverHost::DriverLoadArgs::Create(
    fuchsia_component_runner::wire::ComponentStartInfo start_info) {
  fuchsia_data::wire::Dictionary wire_program = start_info.program();
  zx::result<std::string> binary = fdf_internal::ProgramValue(wire_program, "binary");
  if (binary.is_error()) {
    LOGF(ERROR, "Failed to start driver, missing 'binary' argument: %s", binary.status_string());
    return binary.take_error();
  }

  auto pkg = fdf_internal::NsValue(start_info.ns(), "/pkg");
  if (pkg.is_error()) {
    LOGF(ERROR, "Failed to start driver, missing '/pkg' directory: %s", pkg.status_string());
    return pkg.take_error();
  }

  auto driver_file = pkg_utils::OpenPkgFile(*pkg, *binary);
  if (driver_file.is_error()) {
    LOGF(ERROR, "Failed to open driver file: %s", driver_file.status_string());
    return driver_file.take_error();
  }

  auto lib_dir = pkg_utils::OpenLibDir(*pkg);
  if (lib_dir.is_error()) {
    LOGF(ERROR, "Failed to open driver libs dir: %s", lib_dir.status_string());
    return lib_dir.take_error();
  }

  std::vector<fuchsia_driver_loader::RootModule> additional_root_modules;
  if (binary == kCompatDriverRelativePath) {
    zx::result<std::string> compat = fdf_internal::ProgramValue(wire_program, "compat");
    if (compat.is_error()) {
      LOGF(ERROR, "Failed to start driver with compat shim, missing 'compat' argument: %s",
           compat.status_string());
      return compat.take_error();
    }
    auto v1_driver_file = pkg_utils::OpenPkgFile(*pkg, *compat);
    if (v1_driver_file.is_error()) {
      LOGF(ERROR, "Failed to open compat driver file: %s", v1_driver_file.status_string());
      return v1_driver_file.take_error();
    }
    additional_root_modules.push_back(fuchsia_driver_loader::RootModule{
        {.name = std::string(GetFilename(*compat)), .binary = std::move(*v1_driver_file)}});
  }

  zx::result modules = fdf_internal::ProgramValueAsObjVector(wire_program, "modules");
  if (modules.is_ok()) {
    for (const auto& module : *modules) {
      zx::result<std::string> module_name = fdf_internal::ProgramValue(module, "module_name");
      if (module_name.is_error()) {
        LOGF(ERROR, "Failed to get module name: %s", module_name.status_string());
        return module_name.take_error();
      }
      if (module_name == "#program.compat") {
        // Handled specially above already.
        continue;
      }
      auto module_vmo = pkg_utils::OpenPkgFile(*pkg, *module_name);
      if (module_vmo.is_error()) {
        LOGF(ERROR, "Failed to open module: %s", module_vmo.status_string());
        return module_vmo.take_error();
      }
      additional_root_modules.push_back(fuchsia_driver_loader::RootModule{
          {.name = std::string(GetFilename(*module_name)), .binary = std::move(*module_vmo)}});
    }
  }

  return zx::ok(DriverHost::DriverLoadArgs(GetFilename(*binary), std::move(*driver_file),
                                           std::move(*lib_dir),
                                           std::move(additional_root_modules)));
}

zx::result<> SetEncodedConfig(fidl::WireTableBuilder<fdf::wire::DriverStartArgs>& args,
                              frunner::wire::ComponentStartInfo& start_info) {
  if (!start_info.has_encoded_config()) {
    return zx::ok();
  }

  if (!start_info.encoded_config().is_buffer() && !start_info.encoded_config().is_bytes()) {
    LOGF(ERROR, "Failed to parse encoded config in start info. Encoding is not buffer or bytes.");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (start_info.encoded_config().is_buffer()) {
    args.config(std::move(start_info.encoded_config().buffer().vmo));
    return zx::ok();
  }

  auto vmo_size = start_info.encoded_config().bytes().count();
  zx::vmo vmo;

  auto status = zx::vmo::create(vmo_size, ZX_RIGHT_TRANSFER | ZX_RIGHT_READ, &vmo);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  status = vmo.write(start_info.encoded_config().bytes().data(), 0, vmo_size);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  args.config(std::move(vmo));
  return zx::ok();
}

DriverHostComponent::DriverHostComponent(
    fidl::ClientEnd<fdh::DriverHost> driver_host, async_dispatcher_t* dispatcher,
    fbl::DoublyLinkedList<std::unique_ptr<DriverHostComponent>>* driver_hosts,
    std::shared_ptr<bool> server_connected,
    fidl::ClientEnd<fuchsia_driver_loader::DriverHost> dynamic_linker_driver_loader)
    : driver_host_(std::move(driver_host), dispatcher,
                   fidl::ObserveTeardown([this, driver_hosts] { driver_hosts->erase(*this); })),
      dispatcher_(dispatcher),
      server_connected_(std::move(server_connected)) {
  InitializeElfDir();

  if (dynamic_linker_driver_loader.is_valid()) {
    dynamic_linker_driver_loader_.Bind(std::move(dynamic_linker_driver_loader), dispatcher_);
  }
}

void DriverHostComponent::InitializeElfDir() {
  // This directory allows zxdb to conn
  auto elf_dir = std::make_unique<vfs::PseudoDir>();

  auto now = std::to_string(zx::clock::get_monotonic().get());
  elf_dir->AddEntry("process_start_time", CreateReadonlyFile([now]() { return zx::ok(now); }));

  elf_dir->AddEntry("job_id", CreateReadonlyFile([this]() -> zx::result<std::string> {
                      zx::result job_id = GetJobKoid();
                      if (job_id.is_error()) {
                        return job_id.take_error();
                      }
                      return zx::ok(std::to_string(*job_id));
                    }));

  elf_dir->AddEntry("process_id", CreateReadonlyFile([this]() -> zx::result<std::string> {
                      zx::result process_id = GetProcessKoid();
                      if (process_id.is_error()) {
                        return process_id.take_error();
                      }
                      return zx::ok(std::to_string(*process_id));
                    }));

  runtime_dir_.AddEntry("elf", std::move(elf_dir));
}

void DriverHostComponent::Start(
    fidl::ClientEnd<fdf::Node> client_end, std::string node_name,
    fuchsia_driver_framework::wire::NodePropertyDictionary2 node_properties,
    fidl::VectorView<fuchsia_driver_framework::wire::NodeSymbol> symbols,
    fidl::VectorView<fuchsia_driver_framework::wire::Offer> offers,
    frunner::wire::ComponentStartInfo start_info, zx::event node_token,
    fidl::ServerEnd<fuchsia_driver_host::Driver> driver, StartCallback cb) {
  auto binary = fdf_internal::ProgramValue(start_info.program(), "binary").value_or("");
  fidl::Arena arena;
  auto args = fdf::wire::DriverStartArgs::Builder(arena);

  // TODO(b/361852885): Remove this once we stop supporting the deprecated dictionary.
  fuchsia_driver_framework::wire::NodePropertyDictionary deprecated_dictionary(
      arena, node_properties.count());
  size_t entry_index = 0;
  for (auto& entry : node_properties) {
    fuchsia_driver_framework::wire::NodePropertyVector deprecated_properties(
        arena, entry.properties.count());
    for (size_t i = 0; i < entry.properties.count(); i++) {
      deprecated_properties[i] = ToDeprecatedProperty(arena, entry.properties[i]);
    }
    deprecated_dictionary[entry_index] = fuchsia_driver_framework::wire::NodePropertyEntry{
        .name = entry.name,
        .properties = deprecated_properties,
    };
    entry_index++;
  }

  args.node(std::move(client_end))
      .node_name(fidl::StringView::FromExternal(node_name))
      .node_offers(offers)
      .node_properties(deprecated_dictionary)
      .node_properties_2(node_properties)
      .node_token(std::move(node_token))
      .url(start_info.resolved_url())
      .program(start_info.program())
      .incoming(start_info.ns())
      .outgoing_dir(std::move(start_info.outgoing_dir()));

  auto status = SetEncodedConfig(args, start_info);
  if (status.is_error()) {
    cb(status.take_error());
    return;
  }

  if (!symbols.empty()) {
    args.symbols(symbols);
  }

  if (start_info.has_runtime_dir()) {
    runtime_dir_.Serve(fuchsia_io::wire::kPermReadable, std::move(start_info.runtime_dir()),
                       dispatcher_);
  }

  driver_host_->Start(args.Build(), std::move(driver))
      .ThenExactlyOnce([cb = std::move(cb), binary = std::move(binary)](auto& result) mutable {
        if (!result.ok()) {
          LOGF(ERROR, "Failed to start driver '%s' in driver host: %s", binary.c_str(),
               result.FormatDescription().c_str());
          cb(zx::error(result.status()));
          return;
        }
        if (result->is_error()) {
          LOGF(ERROR, "Failed to start driver '%s' in driver host: %s", binary.c_str(),
               zx_status_get_string(result->error_value()));
          cb(result->take_error());
          return;
        }
        cb(zx::ok());
      });
}

zx::result<fuchsia_driver_host::ProcessInfo> DriverHostComponent::GetProcessInfo() const {
  if (process_info_) {
    return zx::ok(*process_info_);
  }

  if (!(*server_connected_)) {
    return zx::error(ZX_ERR_SHOULD_WAIT);
  }

  fidl::WireResult result = driver_host_.sync()->GetProcessInfo();
  if (!result.ok()) {
    return zx::error(result.status());
  }
  if (result->is_error()) {
    return zx::error(result->error_value());
  }
  process_info_ = fidl::ToNatural(*result->value());
  return zx::ok(*process_info_);
}

void DriverHostComponent::GetCrashInfo(
    uint64_t thread_koid,
    fit::callback<void(zx::result<fuchsia_driver_host::DriverCrashInfo>)> info_callback) const {
  // Bypass the driver host if the crashing thread is the main thread which means the driver host
  // itself is what crashed.
  if (GetMainThreadKoid() == thread_koid) {
    info_callback(zx::error(ZX_ERR_NOT_FOUND));
    return;
  }

  driver_host_->FindDriverCrashInfoByThreadKoid(thread_koid)
      .Then([callback = std::move(info_callback)](
                fidl::WireUnownedResult<
                    fuchsia_driver_host::DriverHost::FindDriverCrashInfoByThreadKoid>&
                    result) mutable {
        if (!result.ok()) {
          callback(zx::error(result.status()));
          return;
        }
        if (result->is_error()) {
          callback(zx::error(result->error_value()));
          return;
        }

        callback(zx::ok(fidl::ToNatural(*result->value())));
      });
}

zx::result<uint64_t> DriverHostComponent::GetJobKoid() const {
  zx::result result = GetProcessInfo();
  if (result.is_error()) {
    return result.take_error();
  }
  return zx::ok(result->job_koid());
}

zx::result<uint64_t> DriverHostComponent::GetMainThreadKoid() const {
  zx::result result = GetProcessInfo();
  if (result.is_error()) {
    return result.take_error();
  }
  return zx::ok(result->main_thread_koid());
}

zx::result<uint64_t> DriverHostComponent::GetProcessKoid() const {
  zx::result result = GetProcessInfo();
  if (result.is_error()) {
    return result.take_error();
  }
  return zx::ok(result->process_koid());
}

zx::result<> DriverHostComponent::InstallLoader(
    fidl::ClientEnd<fuchsia_ldsvc::Loader> loader_client) const {
  auto result = driver_host_->InstallLoader(std::move(loader_client));
  if (!result.ok()) {
    return zx::error(result.status());
  }
  return zx::ok();
}

void DriverHostComponent::StartWithDynamicLinker(
    fidl::ClientEnd<fuchsia_driver_framework::Node> node, std::string node_name,
    DriverLoadArgs load_args, DriverStartArgs start_args, zx::event node_token,
    fidl::ServerEnd<fuchsia_driver_host::Driver> driver, StartCallback cb) {
  if (!IsDynamicLinkingEnabled()) {
    cb(zx::error(ZX_ERR_NOT_SUPPORTED));
    return;
  }

  fidl::Arena arena;
  auto args = fuchsia_driver_loader::wire::DriverHostLoadDriverRequest::Builder(arena)
                  .driver_soname(fidl::StringView::FromExternal(load_args.driver_soname))
                  .driver_binary(std::move(load_args.driver_file))
                  .driver_libs(std::move(load_args.lib_dir))
                  .additional_root_modules(
                      fidl::ToWire(arena, std::move(load_args.additional_root_modules)))
                  .Build();

  std::string driver_name = std::string(load_args.driver_soname);
  dynamic_linker_driver_loader_->LoadDriver(args).ThenExactlyOnce(
      [this, driver = std::move(driver), node = std::move(node), node_name,
       start_args = std::move(start_args), node_token = std::move(node_token), driver_name,
       cb = std::move(cb)](auto& result) mutable {
        if (!result.ok()) {
          LOGF(ERROR, "Failed to start driver %s in driver host: %s", driver_name.c_str(),
               result.FormatDescription().c_str());
          cb(zx::error(result.status()));
          return;
        }
        if (result->is_error()) {
          LOGF(ERROR, "Failed to start driver %s in driver host: %s", driver_name.c_str(),
               zx_status_get_string(result->error_value()));
          cb(result->take_error());
          return;
        }

        fidl::Arena arena;
        Start(std::move(node), node_name, fidl::ToWire(arena, start_args.node_properties_),
              fidl::ToWire(arena, start_args.symbols_), fidl::ToWire(arena, start_args.offers_),
              fidl::ToWire(arena, std::move(start_args.start_info_)), std::move(node_token),
              std::move(driver), std::move(cb));
      });
}

}  // namespace driver_manager
