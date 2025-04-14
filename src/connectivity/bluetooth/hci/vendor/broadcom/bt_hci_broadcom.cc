// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "bt_hci_broadcom.h"

#include <assert.h>
#include <endian.h>
#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/ddk/binding_driver.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/metadata/cpp/metadata.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <threads.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/threads.h>

#include "src/connectivity/bluetooth/hci/vendor/broadcom/packets.h"

namespace bt_hci_broadcom {
namespace fhbt = fuchsia_hardware_bluetooth;
namespace fhsi = fuchsia_hardware_serialimpl::wire;

constexpr uint32_t kTargetBaudRate = 2000000;
constexpr uint32_t kDefaultBaudRate = 115200;

constexpr zx::duration kFirmwareDownloadDelay = zx::msec(50);

// Hardcoded. Better to parameterize on chipset. Broadcom chips need a few hundred msec delay after
// firmware load.
constexpr zx::duration kBaudRateSwitchDelay = zx::msec(200);

const std::unordered_map<uint16_t, std::string> BtHciBroadcom::kFirmwareMap = {
    {PDEV_PID_BCM43458, "BCM4345C5.hcd"},
    {PDEV_PID_BCM4359, "BCM4359C0.hcd"},
    {PDEV_PID_BCM4381A1, "BCM4381A1.hcd"}};

HciEventHandler::HciEventHandler(fit::function<void(std::vector<uint8_t>&)> on_receive_callback)
    : on_receive_callback_(std::move(on_receive_callback)) {}

void HciEventHandler::OnReceive(fhbt::wire::ReceivedPacket* packet) {
  if (!on_receive_callback_) {
    FDF_LOG(ERROR, "No receive callback has been set.");
    return;
  }
  // Ignore packets if they are not event packets during initialization.
  if (packet->Which() != fhbt::wire::ReceivedPacket::Tag::kEvent) {
    FDF_LOG(ERROR, "Received non event packet: %d", packet->Which());
    return;
  }
  std::vector<uint8_t> buffer(packet->event().begin(), packet->event().end());
  on_receive_callback_(buffer);
}

BtHciBroadcom::BtHciBroadcom(fdf::DriverStartArgs start_args,
                             fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : DriverBase("bt-hci-broadcom", std::move(start_args), std::move(driver_dispatcher)),
      hci_event_handler_([this](std::vector<uint8_t>& packet) { OnReceivePacket(packet); }),
      node_(fidl::WireClient(std::move(node()), dispatcher())),
      devfs_connector_(fit::bind_member<&BtHciBroadcom::Connect>(this)) {}

void BtHciBroadcom::Start(fdf::StartCompleter completer) {
  zx_status_t status = ConnectToHciTransportFidlProtocol();
  if (status != ZX_OK) {
    completer(zx::error(status));
    return;
  }
  status = ConnectToSerialFidlProtocol();
  if (status == ZX_OK) {
    is_uart_ = true;
  }

  fdf::Arena arena('INFO');
  auto result = serial_client_.buffer(arena)->GetInfo();
  if (!result.ok()) {
    FDF_LOG(ERROR, "Read failed FIDL error: %s", result.status_string());
    completer(zx::error(result.status()));
    return;
  }

  if (result->is_error()) {
    FDF_LOG(ERROR, "Read failed : %s", zx_status_get_string(result->error_value()));
    completer(zx::error(result->error_value()));
    return;
  }

  serial_pid_ = result.value()->info.serial_pid;

  if (serial_pid_ == PDEV_PID_BCM4381A1) {
    // BCM4381 board requires flow control by default.
    fdf::Arena config_arena('CONF');
    const uint32_t flags = fhsi::kSerialDataBits8 | fhsi::kSerialStopBits1 |
                           fhsi::kSerialParityNone | fhsi::kSerialFlowCtrlCtsRts;
    fdf::WireUnownedResult<fuchsia_hardware_serialimpl::Device::Config> result =
        serial_client_.buffer(config_arena)->Config(kDefaultBaudRate, flags);
    if (!result.ok()) {
      FDF_LOG(ERROR, "Initial UART configuration failed, FIDL error: %s",
              zx_status_get_string(result.status()));
      completer(zx::error(result.status()));
      return;
    }
    if (result->is_error()) {
      FDF_LOG(ERROR, "Initial UART configuration failed, domain error: %s",
              zx_status_get_string(result->error_value()));
      completer(zx::error(result->error_value()));
      return;
    }
  }

  // Continue initialization through the fpromise executor.
  start_completer_.emplace(std::move(completer));
  executor_.emplace(dispatcher());
  executor_->schedule_task(Initialize().then([this](fpromise::result<void, zx_status_t>& result) {
    if (result.is_ok()) {
      CompleteStart(ZX_OK);
    } else {
      CompleteStart(result.take_error());
    }
  }));
}

void BtHciBroadcom::PrepareStop(fdf::PrepareStopCompleter completer) { completer(zx::ok()); }

void BtHciBroadcom::GetFeatures(GetFeaturesCompleter::Sync& completer) {
  fidl::Arena arena;
  auto builder = fhbt::wire::VendorFeatures::Builder(arena);
  builder.acl_priority_command(true);
  completer.Reply(builder.Build());
}

void BtHciBroadcom::EncodeCommand(EncodeCommandRequestView request,
                                  EncodeCommandCompleter::Sync& completer) {
  uint8_t data_buffer[kBcmSetAclPriorityCmdSize];
  switch (request->Which()) {
    case fhbt::wire::VendorCommand::Tag::kSetAclPriority: {
      EncodeSetAclPriorityCommand(request->set_acl_priority(), data_buffer);
      auto encoded_cmd =
          fidl::VectorView<uint8_t>::FromExternal(data_buffer, kBcmSetAclPriorityCmdSize);
      completer.ReplySuccess(encoded_cmd);
      return;
    }
    default: {
      completer.ReplyError(ZX_ERR_INVALID_ARGS);
      return;
    }
  }
}

void BtHciBroadcom::OpenHci(OpenHciCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void BtHciBroadcom::OpenHciTransport(OpenHciTransportCompleter::Sync& completer) {
  if (hci_transport_client_end_.is_valid()) {
    completer.ReplySuccess(std::move(hci_transport_client_end_));
    return;
  }
  // We need a new client end, because we already gave away the initialization one.
  zx::result<fidl::ClientEnd<fhbt::HciTransport>> client_end =
      incoming()->Connect<fhbt::HciService::HciTransport>();
  if (client_end.is_error()) {
    FDF_LOG(ERROR, "Connect to fhbt::HciTransport protocol failed: %s", client_end.status_string());
    completer.ReplyError(client_end.status_value());
    return;
  }
  completer.ReplySuccess(std::move(*client_end));
}

void BtHciBroadcom::OpenSnoop(OpenSnoopCompleter::Sync& completer) {
  zx::result<fidl::ClientEnd<fhbt::Snoop>> client_end =
      incoming()->Connect<fhbt::HciService::Snoop>();
  if (client_end.is_error()) {
    FDF_LOG(ERROR, "Connect to Snoop protocol failed: %s", client_end.status_string());
    completer.ReplyError(client_end.status_value());
    return;
  }
  completer.ReplySuccess(std::move(*client_end));
}

void BtHciBroadcom::handle_unknown_method(fidl::UnknownMethodMetadata<fhbt::Vendor> metadata,
                                          fidl::UnknownMethodCompleter::Sync& completer) {
  FDF_LOG(ERROR, "Unknown method in Vendor protocol, closing with ZX_ERR_NOT_SUPPORTED");
  completer.Close(ZX_ERR_NOT_SUPPORTED);
}

// driver_devfs::Connector<fhbt::Vendor>
void BtHciBroadcom::Connect(fidl::ServerEnd<fhbt::Vendor> request) {
  vendor_binding_group_.AddBinding(dispatcher(), std::move(request), this,
                                   fidl::kIgnoreBindingClosure);
}

zx_status_t BtHciBroadcom::ConnectToHciTransportFidlProtocol() {
  zx::result<fidl::ClientEnd<fhbt::HciTransport>> client_end =
      incoming()->Connect<fhbt::HciService::HciTransport>();
  if (client_end.is_error()) {
    FDF_LOG(ERROR, "Connect to fhbt::HciTransport protocol failed: %s", client_end.status_string());
    return client_end.status_value();
  }

  hci_transport_client_ = fidl::WireSyncClient(*std::move(client_end));

  return ZX_OK;
}

zx_status_t BtHciBroadcom::ConnectToSerialFidlProtocol() {
  zx::result<fdf::ClientEnd<fuchsia_hardware_serialimpl::Device>> client_end =
      incoming()->Connect<fuchsia_hardware_serialimpl::Service::Device>();
  if (client_end.is_error()) {
    FDF_LOG(ERROR, "Connect to fuchsia_hardware_serialimpl::Device protocol failed: %s",
            client_end.status_string());
    return client_end.status_value();
  }

  serial_client_ = fdf::WireSyncClient(*std::move(client_end));
  return ZX_OK;
}

void BtHciBroadcom::EncodeSetAclPriorityCommand(fhbt::wire::VendorSetAclPriorityParams params,
                                                void* out_buffer) {
  if (!params.has_connection_handle() || !params.has_priority() || !params.has_direction()) {
    FDF_LOG(ERROR,
            "The command cannot be encoded because the following fields are missing: %s %s %s",
            params.has_connection_handle() ? "" : "connection_handle",
            params.has_priority() ? "" : "priority", params.has_direction() ? "" : "direction");
    return;
  }
  BcmSetAclPriorityCmd command = {
      .header =
          {
              .opcode = htole16(kBcmSetAclPriorityCmdOpCode),
              .parameter_total_size = sizeof(BcmSetAclPriorityCmd) - sizeof(HciCommandHeader),
          },
      .connection_handle = htole16(params.connection_handle()),
      .priority = (params.priority() == fhbt::VendorAclPriority::kNormal) ? kBcmAclPriorityNormal
                                                                          : kBcmAclPriorityHigh,
      .direction = (params.direction() == fhbt::VendorAclDirection::kSource)
                       ? kBcmAclDirectionSource
                       : kBcmAclDirectionSink,
  };

  memcpy(out_buffer, &command, sizeof(command));
}

void BtHciBroadcom::OnReceivePacket(std::vector<uint8_t>& packet) {
  event_receive_buffer_ = packet;
  auto result = hci_transport_client_->AckReceive();
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Failed to ack receive: %s", result.status_string());
  }
}

fpromise::promise<std::vector<uint8_t>, zx_status_t> BtHciBroadcom::SendCommand(const void* command,
                                                                                size_t length) {
  // send HCI command
  fidl::Arena arena;
  auto command_vec = std::vector<uint8_t>(static_cast<const uint8_t*>(command),
                                          static_cast<const uint8_t*>(command) + length);
  auto command_view = fidl::VectorView<uint8_t>::FromExternal(command_vec);
  auto result =
      hci_transport_client_->Send(fhbt::wire::SentPacket::WithCommand(arena, command_view));
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Failed to send command: %s", result.status_string());
    return fpromise::make_result_promise<std::vector<uint8_t>, zx_status_t>(
        fpromise::error(result.status()));
  }

  return ReadEvent();
}

fpromise::promise<std::vector<uint8_t>, zx_status_t> BtHciBroadcom::ReadEvent() {
  zx::result<std::vector<uint8_t>> result = ReadEventSync();
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to read event");
    return fpromise::make_result_promise<std::vector<uint8_t>, zx_status_t>(
        fpromise::error(result.status_value()));
  }

  return fpromise::make_result_promise<std::vector<uint8_t>, zx_status_t>(
      fpromise::ok(std::move(result.value())));
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::SetBaudRate(uint32_t baud_rate) {
  BcmSetBaudRateCmd command = {
      .header =
          {
              .opcode = kBcmSetBaudRateCmdOpCode,
              .parameter_total_size = sizeof(BcmSetBaudRateCmd) - sizeof(HciCommandHeader),
          },
      .unused = 0,
      .baud_rate = htole32(baud_rate),
  };

  return SendCommand(&command, sizeof(command))
      .and_then(
          [this, baud_rate](const std::vector<uint8_t>&) -> fpromise::result<void, zx_status_t> {
            fdf::Arena arena('CONF');
            fdf::WireUnownedResult<fuchsia_hardware_serialimpl::Device::Config> result =
                serial_client_.buffer(arena)->Config(baud_rate, fhsi::kSerialSetBaudRateOnly);
            if (!result.ok()) {
              return fpromise::error(result.status());
            }
            if (result->is_error()) {
              return fpromise::error(result->error_value());
            }
            return fpromise::ok();
          });
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::SetBdaddr(
    const std::array<uint8_t, kMacAddrLen>& bdaddr) {
  BcmSetBdaddrCmd command = {
      .header =
          {
              .opcode = kBcmSetBdaddrCmdOpCode,
              .parameter_total_size = sizeof(BcmSetBdaddrCmd) - sizeof(HciCommandHeader),
          },
      .bdaddr =
          {// HCI expects little endian. Swap bytes
           bdaddr[5], bdaddr[4], bdaddr[3], bdaddr[2], bdaddr[1], bdaddr[0]},
  };

  return SendCommand(&command.header, sizeof(command)).and_then([](std::vector<uint8_t>&) {});
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::SetDefaultPowerCaps() {
  if (serial_pid_ != PDEV_PID_BCM4381A1) {
    return fpromise::make_promise(
        []() { return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok()); });
  }
  return SendCommand(&kDefaultPowerCapCmd, sizeof(kDefaultPowerCapCmd))
      .and_then([](std::vector<uint8_t>& cmd_complete) {
        if (sizeof(HciCommandComplete) <= cmd_complete.size()) {
          HciCommandComplete event;
          std::memcpy(&event, cmd_complete.data(), sizeof(event));
          if (event.return_code == 0x00) {
            FDF_LOG(INFO, "set default power caps");
          } else {
            FDF_LOG(WARNING, "failed to set default power caps: 0x%02x", event.return_code);
          }
        }
      });
}

fpromise::promise<> BtHciBroadcom::LogControllerFallbackBdaddr() {
  return SendCommand(&kReadBdaddrCmd, sizeof(kReadBdaddrCmd))
      .then([](fpromise::result<std::vector<uint8_t>, zx_status_t>& result) {
        char fallback_addr[18] = "<unknown>";

        if (result.is_ok() && sizeof(ReadBdaddrCommandComplete) == result.value().size()) {
          ReadBdaddrCommandComplete event;
          std::memcpy(&event, result.value().data(), result.value().size());
          // HCI returns data as little endian. Swap bytes
          snprintf(fallback_addr, sizeof(fallback_addr), "%02x:%02x:%02x:%02x:%02x:%02x",
                   event.bdaddr[5], event.bdaddr[4], event.bdaddr[3], event.bdaddr[2],
                   event.bdaddr[1], event.bdaddr[0]);
        }

        FDF_LOG(ERROR, "error getting mac address from bootloader: %s. Fallback address: %s.",
                zx_status_get_string(result.is_ok() ? ZX_OK : result.error()), fallback_addr);
      });
}

constexpr auto kOpenFlags = fuchsia_io::Flags::kPermRead | fuchsia_io::Flags::kProtocolFile;

fpromise::promise<void, zx_status_t> BtHciBroadcom::LoadFirmware() {
  zx::vmo fw_vmo;
  size_t fw_size;

  // If there's no firmware for this PID, we don't expect the bind to happen without a
  // corresponding entry in the firmware table. Please double-check the PID value and add an entry
  // to the firmware table if it's valid.
  ZX_ASSERT_MSG(kFirmwareMap.find(serial_pid_) != kFirmwareMap.end(), "no mapping for PID: %u",
                serial_pid_);

  std::string full_filename = "/pkg/lib/firmware/";
  full_filename.append(kFirmwareMap.at(serial_pid_));

  auto client = incoming()->Open<fuchsia_io::File>(full_filename.c_str(), kOpenFlags);
  if (client.is_error()) {
    FDF_LOG(WARNING, "Open firmware file failed: %s", zx_status_get_string(client.error_value()));
    return fpromise::make_error_promise(client.error_value());
  }

  fidl::WireResult backing_memory_result =
      fidl::WireCall(*client)->GetBackingMemory(fuchsia_io::wire::VmoFlags::kRead);
  if (!backing_memory_result.ok()) {
    if (backing_memory_result.is_peer_closed()) {
      FDF_LOG(WARNING, "Failed to get backing memory: Peer closed");
      return fpromise::make_error_promise(ZX_ERR_NOT_FOUND);
    }
    FDF_LOG(WARNING, "Failed to get backing memory: %s",
            zx_status_get_string(backing_memory_result.status()));
    return fpromise::make_error_promise(backing_memory_result.status());
  }

  const auto* backing_memory = backing_memory_result.Unwrap();
  if (backing_memory->is_error()) {
    FDF_LOG(WARNING, "Failed to get backing memory: %s",
            zx_status_get_string(backing_memory->error_value()));
    return fpromise::make_error_promise(backing_memory->error_value());
  }

  zx::vmo& backing_vmo = backing_memory->value()->vmo;
  if (zx_status_t status = backing_vmo.get_prop_content_size(&fw_size); status != ZX_OK) {
    FDF_LOG(WARNING, "Failed to get vmo size: %s", zx_status_get_string(status));
    return fpromise::make_error_promise(status);
  }
  fw_vmo.reset(backing_vmo.release());

  return SendCommand(&kStartFirmwareDownloadCmd, sizeof(kStartFirmwareDownloadCmd))
      .or_else([](zx_status_t& status) -> fpromise::result<std::vector<uint8_t>, zx_status_t> {
        FDF_LOG(ERROR, "could not load firmware file");
        return fpromise::error(status);
      })
      .and_then([this](std::vector<uint8_t>& /*event*/) mutable {
        // give time for placing firmware in download mode
        return executor_->MakeDelayedPromise(zx::duration(kFirmwareDownloadDelay))
            .then([](fpromise::result<>& /*result*/) {
              return fpromise::result<void, zx_status_t>(fpromise::ok());
            });
      })
      .and_then([this, fw_vmo = std::move(fw_vmo), fw_size]() mutable {
        // The firmware is a sequence of HCI commands containing the firmware data as payloads.
        return SendVmoAsCommands(std::move(fw_vmo), fw_size);
      })
      .and_then([this]() -> fpromise::promise<void, zx_status_t> {
        if (is_uart_) {
          // firmware switched us back to 115200. switch back to kTargetBaudRate.
          fdf::Arena arena('CONF');
          fdf::WireUnownedResult<fuchsia_hardware_serialimpl::Device::Config> result =
              serial_client_.buffer(arena)->Config(kDefaultBaudRate, fhsi::kSerialSetBaudRateOnly);
          if (!result.ok()) {
            return fpromise::make_result_promise(fpromise::error(result.status()));
          }
          if (result->is_error()) {
            return fpromise::make_result_promise(fpromise::error(result->error_value()));
          }

          return executor_->MakeDelayedPromise(kBaudRateSwitchDelay)
              .then(
                  [this](fpromise::result<>& /*result*/) { return SetBaudRate(kTargetBaudRate); });
        }
        return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
      })
      .and_then([]() { FDF_LOG(INFO, "firmware loaded"); });
}

zx_status_t BtHciBroadcom::SendCommandSync(const void* command, size_t length) {
  // send HCI command
  fidl::Arena arena;
  auto command_vec = std::vector<uint8_t>(static_cast<const uint8_t*>(command),
                                          static_cast<const uint8_t*>(command) + length);
  auto command_view = fidl::VectorView<uint8_t>::FromExternal(command_vec);
  auto result =
      hci_transport_client_->Send(fhbt::wire::SentPacket::WithCommand(arena, command_view));
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Failed to send command: %s", result.status_string());
    return result.status();
  }

  return ReadEventSync().status_value();
}

zx::result<std::vector<uint8_t>> BtHciBroadcom::ReadEventSync() {
  fidl::Status result = hci_transport_client_.HandleOneEvent(hci_event_handler_);
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Failed to get event packet: %s", zx_status_get_string(result.status()));
    return zx::error(result.status());
  }

  // Read result will be stored in |event_receive_buffer_|.
  std::vector<uint8_t> packet_bytes = std::move(event_receive_buffer_);
  // Copy out the data from buffer and clear the buffer.
  event_receive_buffer_.clear();

  if (packet_bytes.size() < sizeof(HciCommandComplete)) {
    FDF_LOG(ERROR, "command channel read too short: %zu < %lu", packet_bytes.size(),
            sizeof(HciCommandComplete));
    return zx::error(ZX_ERR_INTERNAL);
  }

  HciCommandComplete event;
  std::memcpy(&event, packet_bytes.data(), sizeof(HciCommandComplete));
  if (event.header.event_code != kHciEvtCommandCompleteEventCode ||
      event.header.parameter_total_size < kMinEvtParamSize) {
    FDF_LOG(ERROR, "did not receive command complete or params too small");
    return zx::error(ZX_ERR_INTERNAL);
  }

  if (event.return_code != 0) {
    FDF_LOG(ERROR, "got command complete error %u", event.return_code);
    return zx::error(ZX_ERR_INTERNAL);
  }

  return zx::ok(std::move(packet_bytes));
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::SendVmoAsCommands(zx::vmo vmo, size_t size) {
  size_t offset = 0;

  while (offset < size) {
    uint8_t buffer[kMaxHciCommandSize];

    size_t remaining = size - offset;
    size_t read_amount = (remaining > sizeof(buffer) ? sizeof(buffer) : remaining);

    if (read_amount < sizeof(HciCommandHeader)) {
      FDF_LOG(ERROR, "short HCI command in firmware download");
      return fpromise::make_error_promise(ZX_ERR_INTERNAL);
    }

    zx_status_t status = vmo.read(buffer, offset, read_amount);
    if (status != ZX_OK) {
      return fpromise::make_error_promise(status);
    }

    HciCommandHeader header;
    std::memcpy(&header, buffer, sizeof(HciCommandHeader));
    size_t length = header.parameter_total_size + sizeof(header);
    if (read_amount < length) {
      FDF_LOG(ERROR, "short HCI command in firmware download");
      return fpromise::make_error_promise(ZX_ERR_INTERNAL);
    }

    offset += length;
    if (zx_status_t status = SendCommandSync(buffer, length); status != ZX_OK) {
      FDF_LOG(ERROR, "SendCommand failed in firmware download: %s", zx_status_get_string(status));
      return fpromise::make_error_promise(status);
    }
  }

  return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::Initialize() {
  FDF_LOG(DEBUG, "sending initial reset command");
  return SendCommand(&kResetCmd, sizeof(kResetCmd))
      .and_then([this](std::vector<uint8_t>&) -> fpromise::promise<void, zx_status_t> {
        if (is_uart_) {
          FDF_LOG(DEBUG, "setting baud rate to %u", kTargetBaudRate);
          // switch baud rate to TARGET_BAUD_RATE
          return SetBaudRate(kTargetBaudRate);
        }
        return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
      })
      .and_then([this]() {
        FDF_LOG(DEBUG, "loading firmware");
        return LoadFirmware();
      })
      .and_then([this]() {
        FDF_LOG(DEBUG, "sending reset command");
        return SendCommand(&kResetCmd, sizeof(kResetCmd));
      })
      .and_then([this](std::vector<uint8_t>&) -> fpromise::promise<void, zx_status_t> {
        FDF_LOG(DEBUG, "Getting mac address");
        zx::result metadata =
            fdf_metadata::GetMetadata<fuchsia_boot_metadata::MacAddressMetadata>(*incoming());
        if (metadata.is_error()) {
          return LogControllerFallbackBdaddr().then(
              [](fpromise::result<>&) -> fpromise::result<void, zx_status_t> {
                return fpromise::ok();
              });
        }

        if (!metadata.value().mac_address().has_value()) {
          FDF_LOG(ERROR, "Mac address metadata missing mac address");
          return fpromise::make_error_promise(ZX_ERR_INTERNAL);
        }
        const auto& octets = metadata.value().mac_address().value().octets();
        FDF_LOG(INFO, "Got mac address %02x:%02x:%02x:%02x:%02x:%02x", octets[0], octets[1],
                octets[2], octets[3], octets[4], octets[5]);

        // send Set BDADDR command
        return SetBdaddr(octets);
      })
      .and_then([this]() { return SetDefaultPowerCaps(); })
      .and_then([this]() { return AddNode(); })
      .then([this](fpromise::result<void, zx_status_t>& result) {
        zx_status_t status = result.is_ok() ? ZX_OK : result.error();
        return OnInitializeComplete(status);
      });
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::OnInitializeComplete(zx_status_t status) {
  // We're done with the HciTransport client end. Unbind the |ClientEnd| from the |Wireclient| and
  // keep it until the host calls |OpenHci| to get it.
  hci_transport_client_end_ = hci_transport_client_.TakeClientEnd();

  if (status != ZX_OK) {
    FDF_LOG(ERROR, "device initialization failed: %s", zx_status_get_string(status));
    return fpromise::make_error_promise(status);
  }

  FDF_LOG(INFO, "initialization completed successfully.");
  return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::AddNode() {
  zx::result connector = devfs_connector_.Bind(dispatcher());
  if (connector.is_error()) {
    FDF_LOG(ERROR, "Failed to bind devfs connecter to dispatcher: %s", connector.status_string());
    return fpromise::make_error_promise(connector.error_value());
  }

  fidl::Arena args_arena;
  auto devfs = fuchsia_driver_framework::wire::DevfsAddArgs::Builder(args_arena)
                   .connector(std::move(connector.value()))
                   .class_name("bt-hci")
                   .Build();

  auto args = fuchsia_driver_framework::wire::NodeAddArgs::Builder(args_arena)
                  .name("bt-hci-broadcom")
                  .devfs_args(devfs)
                  .Build();

  auto controller_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  if (controller_endpoints.is_error()) {
    FDF_LOG(ERROR, "Create node controller end points failed: %s",
            zx_status_get_string(controller_endpoints.error_value()));
    return fpromise::make_error_promise(controller_endpoints.error_value());
  }

  // Create the endpoints of fuchsia_driver_framework::Node protocol for the child node, and hold
  // the client end of it, because no driver will bind to the child node.
  auto child_node_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
  if (child_node_endpoints.is_error()) {
    FDF_LOG(ERROR, "Create child node end points failed: %s",
            zx_status_get_string(child_node_endpoints.error_value()));
    return fpromise::make_error_promise(child_node_endpoints.error_value());
  }

  // Add bt-hci-broadcom child node.
  fpromise::bridge<void, zx_status_t> bridge;
  node_
      ->AddChild(args, std::move(controller_endpoints->server),
                 std::move(child_node_endpoints->server))
      .Then([this, completer = std::move(bridge.completer),
             child_node_client = std::move(child_node_endpoints->client),
             child_controller_client = std::move(controller_endpoints->client)](
                fidl::WireUnownedResult<fuchsia_driver_framework::Node::AddChild>&
                    child_result) mutable {
        if (!child_result.ok()) {
          FDF_LOG(ERROR, "Failed to add bt-hci-broadcom node, FIDL error: %s",
                  child_result.status_string());
          completer.complete_error(child_result.status());
          return;
        }

        if (child_result->is_error()) {
          FDF_LOG(ERROR, "Failed to add bt-hci-broadcom node: %u",
                  static_cast<uint32_t>(child_result->error_value()));
          completer.complete_error(ZX_ERR_INTERNAL);
          return;
        }

        child_node_.Bind(std::move(child_node_client), dispatcher(), this);
        node_controller_.Bind(std::move(child_controller_client), dispatcher(), this);
        completer.complete_ok();
      });

  return bridge.consumer.promise();
}

void BtHciBroadcom::CompleteStart(zx_status_t status) {
  if (start_completer_.has_value()) {
    start_completer_.value()(zx::make_result(status));
    start_completer_.reset();
  } else {
    FDF_LOG(ERROR, "CompleteStart called without start_completer_.");
  }
}

}  // namespace bt_hci_broadcom

FUCHSIA_DRIVER_EXPORT(bt_hci_broadcom::BtHciBroadcom);
