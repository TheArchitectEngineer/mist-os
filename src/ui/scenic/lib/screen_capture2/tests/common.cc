// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/screen_capture2/tests/common.h"

#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.composition/cpp/hlcpp_conversion.h>
#include <lib/ui/scenic/cpp/buffer_collection_import_export_tokens.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/allocation/allocator.h"
#include "src/ui/scenic/lib/flatland/engine/engine.h"
#include "src/ui/scenic/lib/screen_capture/screen_capture_buffer_collection_importer.h"
#include "src/ui/scenic/lib/utils/helpers.h"

using testing::_;

using allocation::Allocator;
using allocation::BufferCollectionImporter;
using screen_capture::ScreenCaptureBufferCollectionImporter;

namespace screen_capture2 {
namespace test {

std::shared_ptr<Allocator> CreateAllocator(
    std::shared_ptr<screen_capture::ScreenCaptureBufferCollectionImporter> importer,
    sys::ComponentContext* app_context) {
  std::vector<std::shared_ptr<BufferCollectionImporter>> extra_importers;
  std::vector<std::shared_ptr<BufferCollectionImporter>> screenshot_importers;
  screenshot_importers.push_back(importer);
  return std::make_shared<Allocator>(app_context, extra_importers, screenshot_importers,
                                     utils::CreateSysmemAllocatorSyncPtr("-allocator"));
}

void CreateBufferCollectionInfoWithConstraints(
    fuchsia::sysmem2::BufferCollectionConstraints constraints,
    fuchsia::ui::composition::BufferCollectionExportToken export_token,
    std::shared_ptr<Allocator> flatland_allocator,
    fuchsia::sysmem2::Allocator_Sync* sysmem_allocator) {
  zx_status_t status;
  // Create Sysmem tokens.
  auto [local_token, dup_token] = utils::CreateSysmemTokens(sysmem_allocator);

  fuchsia_ui_composition::RegisterBufferCollectionArgs rbc_args;
  rbc_args.export_token(fidl::HLCPPToNatural(std::move(export_token)));
  rbc_args.buffer_collection_token2(fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>(
      std::move(dup_token).Unbind().TakeChannel()));
  rbc_args.usages(fuchsia_ui_composition::RegisterBufferCollectionUsages::kScreenshot);

  fuchsia::sysmem2::BufferCollectionSyncPtr buffer_collection;
  fuchsia::sysmem2::AllocatorBindSharedCollectionRequest bind_shared_request;
  bind_shared_request.set_token(std::move(local_token));
  bind_shared_request.set_buffer_collection_request(buffer_collection.NewRequest());
  status = sysmem_allocator->BindSharedCollection(std::move(bind_shared_request));
  FX_DCHECK(status == ZX_OK);

  fuchsia::sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.set_constraints(std::move(constraints));
  status = buffer_collection->SetConstraints(std::move(set_constraints_request));
  EXPECT_EQ(status, ZX_OK);

  bool processed_callback = false;
  flatland_allocator->RegisterBufferCollection(std::move(rbc_args),
                                               [&processed_callback](auto result) {
                                                 EXPECT_TRUE(result.is_ok());
                                                 processed_callback = true;
                                               });

  // Wait for allocation.
  fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result wait_result;
  status = buffer_collection->WaitForAllBuffersAllocated(&wait_result);
  ASSERT_EQ(ZX_OK, status);
  ASSERT_TRUE(!wait_result.is_framework_err());
  ASSERT_TRUE(!wait_result.is_err());
  ASSERT_TRUE(wait_result.is_response());
  auto buffer_collection_info = std::move(*wait_result.response().mutable_buffer_collection_info());
  ASSERT_EQ(constraints.min_buffer_count(), buffer_collection_info.buffers().size());

  buffer_collection->Release();
}

}  // namespace test
}  // namespace screen_capture2
