// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../interconnect-visitor.h"

#include <fidl/fuchsia.hardware.interconnect/cpp/fidl.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devicetree/testing/visitor-test-helper.h>
#include <lib/driver/devicetree/visitors/default/bind-property/bind-property.h>
#include <lib/driver/devicetree/visitors/default/mmio/mmio.h>
#include <lib/driver/devicetree/visitors/registry.h>

#include <cstdint>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/hardware/interconnect/cpp/bind.h>
#include <bind/fuchsia/interconnect/cpp/bind.h>
#include <bind/fuchsia/platform/cpp/bind.h>
#include <gtest/gtest.h>

#include "dts/interconnect.h"

namespace interconnect_dt {

class InterconnectVisitorTester
    : public fdf_devicetree::testing::VisitorTestHelper<InterconnectVisitor> {
 public:
  InterconnectVisitorTester(std::string_view dtb_path)
      : fdf_devicetree::testing::VisitorTestHelper<InterconnectVisitor>(
            dtb_path, "InterconnectVisitorTest") {}
};

TEST(InterconnectVisitorTest, InterconnectsProperty) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  ASSERT_TRUE(visitors.RegisterVisitor(std::make_unique<fdf_devicetree::MmioVisitor>()).is_ok());

  auto tester = std::make_unique<InterconnectVisitorTester>("/pkg/test-data/interconnect.dtb");
  InterconnectVisitorTester* interconnect_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, interconnect_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(interconnect_tester->DoPublish().is_ok());

  auto node_count =
      interconnect_tester->env().SyncCall(&fdf_devicetree::testing::FakeEnvWrapper::pbus_node_size);

  uint32_t node_tested_count = 0;
  uint32_t mgr_request_idx = 0;
  for (size_t i = 0; i < node_count; i++) {
    auto node = interconnect_tester->env().SyncCall(
        &fdf_devicetree::testing::FakeEnvWrapper::pbus_nodes_at, i);

    if (node.name().value() == "interconnect-ffffa000") {
      auto metadata = interconnect_tester->env()
                          .SyncCall(&fdf_devicetree::testing::FakeEnvWrapper::pbus_nodes_at, i)
                          .metadata();

      // Test metadata properties.
      ASSERT_TRUE(metadata);
      ASSERT_EQ(1lu, metadata->size());

      // Interconnect IDs metadata
      std::vector<uint8_t> metadata_blob_2 = std::move(*(*metadata)[0].data());
      fit::result paths_metadata =
          fidl::Unpersist<fuchsia_hardware_interconnect::Metadata>(cpp20::span(metadata_blob_2));
      ASSERT_TRUE(paths_metadata.is_ok());
      const auto& paths = paths_metadata->paths();
      ASSERT_TRUE(paths.has_value());
      ASSERT_EQ(paths.value().size(), 2lu);
      EXPECT_EQ(paths.value()[0].name(), std::string(PATH1_NAME));
      EXPECT_EQ(paths.value()[0].id(), 1u);
      EXPECT_EQ(paths.value()[0].src_node_id(), uint32_t{ICC_ID1});
      EXPECT_EQ(paths.value()[0].dst_node_id(), uint32_t{ICC_ID2});
      EXPECT_EQ(paths.value()[1].name(), std::string(PATH3_NAME));
      EXPECT_EQ(paths.value()[1].id(), 3u);
      EXPECT_EQ(paths.value()[1].src_node_id(), uint32_t{ICC_ID6});
      EXPECT_EQ(paths.value()[1].dst_node_id(), uint32_t{ICC_ID2});

      node_tested_count++;
    }

    if (node.name().value() == "interconnect-ffffb000") {
      auto metadata = interconnect_tester->env()
                          .SyncCall(&fdf_devicetree::testing::FakeEnvWrapper::pbus_nodes_at, i)
                          .metadata();

      // Test metadata properties.
      ASSERT_TRUE(metadata);
      ASSERT_EQ(1lu, metadata->size());

      // Interconnect IDs metadata
      std::vector<uint8_t> metadata_blob_2 = std::move(*(*metadata)[0].data());
      fit::result paths_metadata =
          fidl::Unpersist<fuchsia_hardware_interconnect::Metadata>(cpp20::span(metadata_blob_2));
      ASSERT_TRUE(paths_metadata.is_ok());
      const auto& paths = paths_metadata->paths();
      ASSERT_TRUE(paths.has_value());
      ASSERT_EQ(paths.value().size(), 1lu);
      EXPECT_EQ(paths.value()[0].name(), std::string(PATH2_NAME));
      EXPECT_EQ(paths.value()[0].id(), 2u);
      EXPECT_EQ(paths.value()[0].src_node_id(), uint32_t{ICC_ID3});
      EXPECT_EQ(paths.value()[0].dst_node_id(), uint32_t{ICC_ID4});

      node_tested_count++;
    }

    if (node.name().value().starts_with("video")) {
      ASSERT_EQ(2lu, interconnect_tester->env().SyncCall(
                         &fdf_devicetree::testing::FakeEnvWrapper::mgr_requests_size));

      auto mgr_request = interconnect_tester->env().SyncCall(
          &fdf_devicetree::testing::FakeEnvWrapper::mgr_requests_at, mgr_request_idx++);
      ASSERT_TRUE(mgr_request.parents2().has_value());
      ASSERT_EQ(3lu, mgr_request.parents2()->size());

      // 1st parent is pdev. Skipping that.
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {{fdf::MakeProperty2(bind_fuchsia_hardware_interconnect::PATHSERVICE,
                               bind_fuchsia_hardware_interconnect::PATHSERVICE_ZIRCONTRANSPORT),
            fdf::MakeProperty2(bind_fuchsia_interconnect::PATH_NAME, std::string(PATH1_NAME))}},
          (*mgr_request.parents2())[1].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{fdf::MakeAcceptBindRule2(
                bind_fuchsia_hardware_interconnect::PATHSERVICE,
                bind_fuchsia_hardware_interconnect::PATHSERVICE_ZIRCONTRANSPORT),
            fdf::MakeAcceptBindRule2(bind_fuchsia::INTERCONNECT_PATH_ID, 1u)}},
          (*mgr_request.parents2())[1].bind_rules(), false));

      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {{fdf::MakeProperty2(bind_fuchsia_hardware_interconnect::PATHSERVICE,
                               bind_fuchsia_hardware_interconnect::PATHSERVICE_ZIRCONTRANSPORT),
            fdf::MakeProperty2(bind_fuchsia_interconnect::PATH_NAME, std::string(PATH2_NAME))}},
          (*mgr_request.parents2())[2].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{fdf::MakeAcceptBindRule2(
                bind_fuchsia_hardware_interconnect::PATHSERVICE,
                bind_fuchsia_hardware_interconnect::PATHSERVICE_ZIRCONTRANSPORT),
            fdf::MakeAcceptBindRule2(bind_fuchsia::INTERCONNECT_PATH_ID, 2u)}},
          (*mgr_request.parents2())[2].bind_rules(), false));

      node_tested_count++;
    }

    if (node.name().value().starts_with("audio")) {
      ASSERT_EQ(2lu, interconnect_tester->env().SyncCall(
                         &fdf_devicetree::testing::FakeEnvWrapper::mgr_requests_size));

      auto mgr_request = interconnect_tester->env().SyncCall(
          &fdf_devicetree::testing::FakeEnvWrapper::mgr_requests_at, mgr_request_idx++);
      ASSERT_TRUE(mgr_request.parents2().has_value());
      ASSERT_EQ(2lu, mgr_request.parents2()->size());

      // 1st parent is pdev. Skipping that.

      // 2nd is the interconnect  parent.
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {{fdf::MakeProperty2(bind_fuchsia_hardware_interconnect::PATHSERVICE,
                               bind_fuchsia_hardware_interconnect::PATHSERVICE_ZIRCONTRANSPORT),
            fdf::MakeProperty2(bind_fuchsia_interconnect::PATH_NAME, std::string(PATH3_NAME))}},
          (*mgr_request.parents2())[1].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{fdf::MakeAcceptBindRule2(
                bind_fuchsia_hardware_interconnect::PATHSERVICE,
                bind_fuchsia_hardware_interconnect::PATHSERVICE_ZIRCONTRANSPORT),
            fdf::MakeAcceptBindRule2(bind_fuchsia::INTERCONNECT_PATH_ID, uint32_t{3})}},
          (*mgr_request.parents2())[1].bind_rules(), false));

      node_tested_count++;
    }
  }

  ASSERT_EQ(node_tested_count, 4u);
}

TEST(InterconnectVisitorTest, IncorrectCellCount) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  ASSERT_TRUE(visitors.RegisterVisitor(std::make_unique<fdf_devicetree::MmioVisitor>()).is_ok());

  auto tester = std::make_unique<InterconnectVisitorTester>(
      "/pkg/test-data/interconnect-incorrect-cell-count.dtb");
  InterconnectVisitorTester* interconnect_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_ERR_INTERNAL, interconnect_tester->manager()->Walk(visitors).status_value());
}

}  // namespace interconnect_dt
