// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/fence.h"

#include <lib/async-testing/test_loop.h>
#include <lib/async/default.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>

#include <fbl/vector.h>
#include <gtest/gtest.h>

namespace display_coordinator {

class TestCallback : public FenceCallback {
 public:
  void OnFenceFired(FenceReference* f) override { fired_.push_back(f); }

  void OnRefForFenceDead(Fence* fence) override {
    // TODO(https://fxbug.dev/394422104): it is not ideal to require implementors of `FenceCallback`
    // to call `OnRefDead()` in order to maintain the fence's ref-count. This should be handled
    // between `Fence`/`FenceReference` without muddying the `FenceCallback` contract.
    fence->OnRefDead();
  }

  fbl::Vector<FenceReference*> fired_;
};

class FenceTest : public testing::Test {
 public:
  void SetUp() override {
    zx::event ev;
    zx::event::create(0, &ev);
    constexpr display::EventId kEventId(1);
    fence_ = fbl::AdoptRef(new Fence(&cb_, loop_.dispatcher(), kEventId, std::move(ev)));
  }

  void TearDown() override { fence_->ClearRef(); }

  async::TestLoop& loop() { return loop_; }
  fbl::RefPtr<Fence> fence() { return fence_; }
  TestCallback& cb() { return cb_; }

 protected:
  // `logger_` must outlive `driver_runtime_` to allow for any
  // logging in driver de-initialization code.
  fdf_testing::ScopedGlobalLogger logger_;
  fdf_testing::DriverRuntime runtime_;
  async::TestLoop loop_;
  fbl::RefPtr<Fence> fence_;
  TestCallback cb_;
};

TEST_F(FenceTest, MultipleRefs_OnePurpose) {
  fence()->CreateRef();
  auto one = fence()->GetReference();
  auto two = fence()->GetReference();
}

TEST_F(FenceTest, MultipleRefs_MultiplePurposes) {
  fence()->CreateRef();
  auto one = fence()->GetReference();
  fence()->CreateRef();
  auto two = fence()->GetReference();
  fence()->CreateRef();
  auto three = fence()->GetReference();
  two->StartReadyWait();
  one->StartReadyWait();

  three->Signal();
  loop().RunUntilIdle();

  three->Signal();
  loop().RunUntilIdle();

  ASSERT_EQ(cb().fired_.size(), 2u);
  EXPECT_EQ(cb().fired_[0], two.get());
  EXPECT_EQ(cb().fired_[1], one.get());
}

}  // namespace display_coordinator
