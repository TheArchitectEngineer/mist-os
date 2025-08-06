// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_UNIT_LIB_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_UNIT_LIB_H_

#include <fidl/fuchsia.hardware.power/cpp/fidl.h>
#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/test_base.h>
#include <lib/driver/power/cpp/testing/fake_element_control.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/inspect/testing/cpp/inspect.h>

#include <gtest/gtest.h>

#include "mock-device/ufs-mock-device.h"
#include "src/devices/block/drivers/ufs/ufs.h"
#include "src/devices/pci/testing/pci_protocol_fake.h"
#include "src/lib/testing/predicates/status.h"

namespace ufs {

using fdf_power::testing::FakeElementControl;

// Implement all the WireServer handlers of fuchsia_hardware_pci::Device as protocol as required by
// FIDL.
class FakePci : public fidl::WireServer<fuchsia_hardware_pci::Device> {
 public:
  fuchsia_hardware_pci::Service::InstanceHandler GetInstanceHandler() {
    return fuchsia_hardware_pci::Service::InstanceHandler({
        .device = binding_group_.CreateHandler(
            this, fdf::Dispatcher::GetCurrent()->async_dispatcher(), fidl::kIgnoreBindingClosure),
    });
  }

  void GetDeviceInfo(GetDeviceInfoCompleter::Sync& completer) override {
    fuchsia_hardware_pci::wire::DeviceInfo info;
    completer.Reply(info);
  }
  void GetBar(GetBarRequestView request, GetBarCompleter::Sync& completer) override {
    fuchsia_hardware_pci::wire::Bar bar = {
        .bar_id = 0,
        .size = RegisterMap::kRegisterSize,
        .result = fuchsia_hardware_pci::wire::BarResult::WithVmo(mock_device_->GetVmo()),
    };
    completer.ReplySuccess(std::move(bar));
  }
  void SetBusMastering(SetBusMasteringRequestView request,
                       SetBusMasteringCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }
  void ResetDevice(ResetDeviceCompleter::Sync& completer) override { completer.ReplySuccess(); }
  void AckInterrupt(AckInterruptCompleter::Sync& completer) override { completer.ReplySuccess(); }
  void MapInterrupt(MapInterruptRequestView request,
                    MapInterruptCompleter::Sync& completer) override {
    completer.ReplySuccess(mock_device_->GetIrq());
  }
  void GetInterruptModes(GetInterruptModesCompleter::Sync& completer) override {
    fuchsia_hardware_pci::wire::InterruptModes modes;
    modes.has_legacy = true;
    modes.msix_count = 0;
    modes.msi_count = 0;
    completer.Reply(modes);
  }
  void SetInterruptMode(SetInterruptModeRequestView request,
                        SetInterruptModeCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }
  void ReadConfig8(ReadConfig8RequestView request, ReadConfig8Completer::Sync& completer) override {
    completer.ReplySuccess(0);
  }
  void ReadConfig16(ReadConfig16RequestView request,
                    ReadConfig16Completer::Sync& completer) override {
    completer.ReplySuccess(0);
  }
  void ReadConfig32(ReadConfig32RequestView request,
                    ReadConfig32Completer::Sync& completer) override {
    completer.ReplySuccess(0);
  }
  void WriteConfig8(WriteConfig8RequestView request,
                    WriteConfig8Completer::Sync& completer) override {
    completer.ReplySuccess();
  }
  void WriteConfig16(WriteConfig16RequestView request,
                     WriteConfig16Completer::Sync& completer) override {
    completer.ReplySuccess();
  }
  void WriteConfig32(WriteConfig32RequestView request,
                     WriteConfig32Completer::Sync& completer) override {
    completer.ReplySuccess();
  }
  void GetCapabilities(GetCapabilitiesRequestView request,
                       GetCapabilitiesCompleter::Sync& completer) override {
    std::vector<uint8_t> empty_vec;
    auto empty_vec_view = fidl::VectorView<uint8_t>::FromExternal(empty_vec);
    completer.Reply(empty_vec_view);
  }
  void GetExtendedCapabilities(GetExtendedCapabilitiesRequestView request,
                               GetExtendedCapabilitiesCompleter::Sync& completer) override {
    std::vector<uint16_t> empty_vec;
    auto empty_vec_view = fidl::VectorView<uint16_t>::FromExternal(empty_vec);
    completer.Reply(empty_vec_view);
  }
  void GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) override {
    completer.ReplySuccess(mock_device_->GetFakeBti());
  }

  void SetMockDevice(ufs_mock_device::UfsMockDevice* mock_device) { mock_device_ = mock_device; }

  fidl::ServerBindingGroup<fuchsia_hardware_pci::Device> binding_group_;

  zx::interrupt irq_;
  ufs_mock_device::UfsMockDevice* mock_device_;
};

class FakeSystemActivityGovernor
    : public fidl::testing::TestBase<fuchsia_power_system::ActivityGovernor> {
 public:
  FakeSystemActivityGovernor(zx::event exec_state_opportunistic, zx::event wake_handling_assertive)
      : exec_state_opportunistic_(std::move(exec_state_opportunistic)),
        wake_handling_assertive_(std::move(wake_handling_assertive)) {}

  fidl::ProtocolHandler<fuchsia_power_system::ActivityGovernor> CreateHandler() {
    return bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   fidl::kIgnoreBindingClosure);
  }

  void GetPowerElements(GetPowerElementsCompleter::Sync& completer) override {
    fuchsia_power_system::PowerElements elements;
    zx::event execution_element;
    exec_state_opportunistic_.duplicate(ZX_RIGHT_SAME_RIGHTS, &execution_element);

    fuchsia_power_system::ExecutionState exec_state = {
        {.opportunistic_dependency_token = std::move(execution_element)}};

    elements = {{.execution_state = std::move(exec_state)}};

    completer.Reply({{std::move(elements)}});
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << "Unexpected SCSI command";
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_system::ActivityGovernor> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  fidl::ServerBindingGroup<fuchsia_power_system::ActivityGovernor> bindings_;

  zx::event exec_state_opportunistic_;
  zx::event wake_handling_assertive_;
};

class FakeLessor : public fidl::Server<fuchsia_power_broker::Lessor> {
 public:
  void AddSideEffect(fit::function<void()> side_effect) { side_effect_ = std::move(side_effect); }

  void Lease(fuchsia_power_broker::LessorLeaseRequest& req,
             LeaseCompleter::Sync& completer) override {
    if (side_effect_) {
      side_effect_();
    }

    auto [lease_control_client_end, lease_control_server_end] =
        fidl::Endpoints<fuchsia_power_broker::LeaseControl>::Create();
    completer.Reply(fit::success(std::move(lease_control_client_end)));
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::Lessor> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  fit::function<void()> side_effect_;
};

class PowerElement {
 public:
  explicit PowerElement(
      fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control,
      fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor)
      : element_control_(std::move(element_control)), lessor_(std::move(lessor)) {}

  fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control_;
  fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor_;
};

class FakePowerBroker : public fidl::Server<fuchsia_power_broker::Topology> {
 public:
  fidl::ProtocolHandler<fuchsia_power_broker::Topology> CreateHandler() {
    return bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   fidl::kIgnoreBindingClosure);
  }

  void AddElement(fuchsia_power_broker::ElementSchema& req,
                  AddElementCompleter::Sync& completer) override {
    // Get channels from request.
    ASSERT_TRUE(req.element_runner().has_value());
    fidl::ServerEnd<fuchsia_power_broker::Lessor>& lessor_server_end = req.lessor_channel().value();

    // Instantiate (fake) element control implementation.
    ASSERT_TRUE(req.element_control().has_value());
    auto element_control_impl = std::make_unique<FakeElementControl>();
    fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control_binding =
        fidl::BindServer<fuchsia_power_broker::ElementControl>(
            fdf::Dispatcher::GetCurrent()->async_dispatcher(), std::move(*req.element_control()),
            std::move(element_control_impl));

    // Instantiate (fake) lessor implementation.
    auto lessor_impl = std::make_unique<FakeLessor>();
    if (req.element_name() == Ufs::kHardwarePowerElementName) {
      hardware_power_lessor_ = lessor_impl.get();
    } else if (req.element_name() == Ufs::kSystemWakeOnRequestPowerElementName) {
      wake_on_request_lessor_ = lessor_impl.get();
    } else {
      ZX_ASSERT_MSG(0, "Unexpected power element.");
    }
    fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor_binding =
        fidl::BindServer<fuchsia_power_broker::Lessor>(
            fdf::Dispatcher::GetCurrent()->async_dispatcher(), std::move(lessor_server_end),
            std::move(lessor_impl),
            [](FakeLessor* impl, fidl::UnbindInfo info,
               fidl::ServerEnd<fuchsia_power_broker::Lessor> server_end) mutable {});

    // Make (fake) call to ElementRunner::SetLevel
    if (req.element_name() == Ufs::kHardwarePowerElementName) {
      fidl::Client<fuchsia_power_broker::ElementRunner> element_runner_client(
          std::move(*req.element_runner()), fdf::Dispatcher::GetCurrent()->async_dispatcher());
      element_runner_client->SetLevel(Ufs::kPowerLevelOff)
          .ThenExactlyOnce(
              [&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel>& result) mutable {
                if (!result.is_ok()) {
                  ADD_FAILURE() << "SetLevel failed: " << result.error_value();
                }
              });
      hardware_power_element_runner_client_ = std::move(element_runner_client);
    }
    if (wake_on_request_lessor_) {
      wake_on_request_lessor_->AddSideEffect([&]() {
        hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOn})
            .ThenExactlyOnce(
                [&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
                  EXPECT_TRUE(result.is_ok());
                });
      });
    }
    servers_.emplace_back(std::move(element_control_binding), std::move(lessor_binding));

    completer.Reply(fit::success());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::Topology> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  FakeLessor* hardware_power_lessor_ = nullptr;
  FakeLessor* wake_on_request_lessor_ = nullptr;
  fidl::Client<fuchsia_power_broker::ElementRunner> hardware_power_element_runner_client_;

 private:
  fidl::ServerBindingGroup<fuchsia_power_broker::Topology> bindings_;

  std::vector<PowerElement> servers_;
};

class TestUfs : public Ufs {
 public:
  TestUfs(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : Ufs(std::move(start_args), std::move(dispatcher)) {}
  ~TestUfs() override = default;

  inspect::ComponentInspector& GetInspector() { return inspector(); }

  static void SetMockDevice(ufs_mock_device::UfsMockDevice* mock_device) {
    TestUfs::mock_device_ = mock_device;
  }

 protected:
  zx::result<fdf::MmioBuffer> CreateMmioBuffer(zx_off_t offset, size_t size, zx::vmo vmo) override;

 private:
  // TODO(https://fxbug.dev/42075643): We can avoid the static pointer by moving the
  // RegisterMmioProcessor to the TestUfs class.
  static ufs_mock_device::UfsMockDevice* mock_device_;
};

class Environment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    // Serve device_server_
    device_server_.Init(component::kDefaultInstance, "root");
    device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs);

    // Serve (fake) pci_server_
    auto result = to_driver_vfs.AddService<fuchsia_hardware_pci::Service>(
        pci_server_.GetInstanceHandler(), "pci");
    EXPECT_EQ(ZX_OK, result.status_value());

    // Serve (fake) system_activity_governor.
    zx::event::create(0, &exec_opportunistic_);
    zx::event::create(0, &wake_assertive_);
    zx::event exec_opportunistic_dupe, wake_assertive_dupe;
    EXPECT_EQ(exec_opportunistic_.duplicate(ZX_RIGHT_SAME_RIGHTS, &exec_opportunistic_dupe), ZX_OK);
    EXPECT_EQ(wake_assertive_.duplicate(ZX_RIGHT_SAME_RIGHTS, &wake_assertive_dupe), ZX_OK);
    system_activity_governor_.emplace(std::move(exec_opportunistic_dupe),
                                      std::move(wake_assertive_dupe));
    auto result_sag =
        to_driver_vfs.component().AddUnmanagedProtocol<fuchsia_power_system::ActivityGovernor>(
            system_activity_governor_->CreateHandler());
    EXPECT_EQ(ZX_OK, result_sag.status_value());

    // Serve (fake) power_broker.
    auto result_pb = to_driver_vfs.component().AddUnmanagedProtocol<fuchsia_power_broker::Topology>(
        power_broker_.CreateHandler());
    EXPECT_EQ(ZX_OK, result_pb.status_value());

    return zx::ok();
  }

  FakePci& pci_server() { return pci_server_; }
  FakePowerBroker& power_broker() { return power_broker_; }
  FakeSystemActivityGovernor& system_activity_governor() { return *system_activity_governor_; }

 private:
  FakePci pci_server_;
  compat::DeviceServer device_server_;
  zx::event exec_opportunistic_, wake_assertive_;
  std::optional<FakeSystemActivityGovernor> system_activity_governor_;
  FakePowerBroker power_broker_;
};

class TestConfig final {
 public:
  using DriverType = TestUfs;
  using EnvironmentType = Environment;
};

class UfsTest : public ::testing::Test {
 public:
  void SetUp() override;
  void TearDown() override;

  void InitMockDevice();
  void StartDriver(bool supply_power_framework = false);

  fdf_testing::ForegroundDriverTest<TestConfig>& driver_test() { return driver_test_; }

  zx::result<fdf::MmioBuffer> GetMmioBuffer(zx::vmo vmo) {
    return zx::ok(mock_device_.GetMmioBuffer(std::move(vmo)));
  }

  zx_status_t DisableController();
  zx_status_t EnableController();

  // Helper functions for accessing private functions.
  zx::result<> TransferFillDescriptorAndSendRequest(uint8_t slot, DataDirection ddir,
                                                    uint16_t resp_offset, uint16_t resp_len,
                                                    uint16_t prdt_offset,
                                                    uint16_t prdt_entry_count);
  zx::result<> TaskManagementFillDescriptorAndSendRequest(uint8_t slot,
                                                          TaskManagementRequestUpiu& request);

  // Map the data vmo to the address space and assign physical addresses. Currently, it only
  // supports 8KB vmo. So, we get two physical addresses. The return value is the physical address
  // of the pinned memory.
  zx::result<> MapVmo(zx::unowned_vmo& vmo, fzl::VmoMapper& mapper, uint64_t offset_vmo,
                      uint64_t length);

  uint8_t GetSlotStateCount(SlotState slot_state);

  zx::result<uint32_t> ReadAttribute(Attributes attribute, uint8_t index = 0);
  zx::result<> WriteAttribute(Attributes attribute, uint32_t value, uint8_t index = 0);

  zx::result<> DisableBackgroundOp() { return dut_->GetDeviceManager().DisableBackgroundOp(); }

  // This function is a wrapper to avoid the thread annotation of ReserveAdminSlot().
  zx::result<uint8_t> ReserveAdminSlot() {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().admin_slot_lock_);
    return dut_->GetTransferRequestProcessor().ReserveAdminSlot();
  }

  ufs_mock_device::UfsMockDevice mock_device_;

  template <class T>
  zx::result<uint8_t> ReserveSlot() {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  template <>
  zx::result<uint8_t> ReserveSlot<TransferRequestProcessor>() {
    return dut_->GetTransferRequestProcessor().ReserveSlot();
  }
  template <>
  zx::result<uint8_t> ReserveSlot<TaskManagementRequestProcessor>() {
    return dut_->GetTaskManagementRequestProcessor().ReserveSlot();
  }

  template <class T>
  zx::result<> RingRequestDoorbell(uint8_t slot_num) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  template <>
  zx::result<> RingRequestDoorbell<TransferRequestProcessor>(uint8_t slot_num) {
    return dut_->GetTransferRequestProcessor().RingRequestDoorbell(slot_num);
  }
  template <>
  zx::result<> RingRequestDoorbell<TaskManagementRequestProcessor>(uint8_t slot_num) {
    return dut_->GetTaskManagementRequestProcessor().RingRequestDoorbell(slot_num);
  }

 protected:
  fdf_testing::ForegroundDriverTest<TestConfig> driver_test_;
  TestConfig::DriverType* dut_;
};

}  // namespace ufs

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_UNIT_LIB_H_
