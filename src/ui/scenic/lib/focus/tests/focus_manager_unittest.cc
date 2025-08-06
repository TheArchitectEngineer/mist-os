// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/focus/focus_manager.h"

#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_ref_pair.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/ui/scenic/lib/utils/helpers.h"

namespace focus::test {

enum : zx_koid_t { kNodeA = 1, kNodeB, kNodeC, kNodeD };

using fuchsia::ui::views::ViewRef;
using view_tree::ViewNode;

namespace {

// Creates a snapshot with the following one-node topology:
//     A
std::shared_ptr<const view_tree::Snapshot> OneNodeSnapshot() {
  auto snapshot = std::make_shared<view_tree::Snapshot>();

  snapshot->root = kNodeA;
  auto& view_tree = snapshot->view_tree;
  view_tree[kNodeA] = ViewNode{.parent = ZX_KOID_INVALID};

  return snapshot;
}

// Creates a snapshot with the following two-node topology:
//     A
//     |
//     B
std::shared_ptr<view_tree::Snapshot> TwoNodeSnapshot() {
  auto snapshot = std::make_shared<view_tree::Snapshot>();

  snapshot->root = kNodeA;
  auto& view_tree = snapshot->view_tree;
  view_tree[kNodeA] = ViewNode{.parent = ZX_KOID_INVALID, .children = {kNodeB}};
  view_tree[kNodeB] = ViewNode{.parent = kNodeA};

  return snapshot;
}

// Creates a snapshot with the following three-node topology:
//     A
//     |
//     B
//     |
//     C
std::shared_ptr<const view_tree::Snapshot> ThreeNodeSnapshot() {
  auto snapshot = std::make_shared<view_tree::Snapshot>();

  snapshot->root = kNodeA;
  auto& view_tree = snapshot->view_tree;
  view_tree[kNodeA] = ViewNode{.parent = ZX_KOID_INVALID, .children = {kNodeA}};
  view_tree[kNodeB] = ViewNode{.parent = kNodeA, .children = {kNodeC}};
  view_tree[kNodeC] = ViewNode{.parent = kNodeB};

  return snapshot;
}

// Creates a snapshot with the following four-node topology:
//      A
//    /   \
//   B     C
//   |
//   D
std::shared_ptr<const view_tree::Snapshot> FourNodeSnapshot() {
  auto snapshot = std::make_shared<view_tree::Snapshot>();

  snapshot->root = kNodeA;
  auto& view_tree = snapshot->view_tree;
  view_tree[kNodeA] = ViewNode{.parent = ZX_KOID_INVALID, .children = {kNodeB, kNodeC}};
  view_tree[kNodeB] = ViewNode{.parent = kNodeA, .children = {kNodeD}};
  view_tree[kNodeC] = ViewNode{.parent = kNodeA};
  view_tree[kNodeD] = ViewNode{.parent = kNodeB};

  return snapshot;
}

// Creates a snapshot with the following four-node topology, with valid ViewRefs for each node:
//      A
//    /   \
//   B     C
//   |
//   D
static std::shared_ptr<const view_tree::Snapshot> FourNodeSnapshotWithViewRefs() {
  auto snapshot = std::make_shared<view_tree::Snapshot>();

  snapshot->root = kNodeA;
  auto& view_tree = snapshot->view_tree;
  {
    auto [control_ref, view_ref] = scenic::ViewRefPair::New();
    view_tree[kNodeA] = ViewNode{.parent = ZX_KOID_INVALID,
                                 .children = {kNodeB, kNodeC},
                                 .view_ref = std::make_shared<ViewRef>(std::move(view_ref))};
  }
  {
    auto [control_ref, view_ref] = scenic::ViewRefPair::New();
    view_tree[kNodeB] = ViewNode{.parent = kNodeA,
                                 .children = {kNodeD},
                                 .view_ref = std::make_shared<ViewRef>(std::move(view_ref))};
  }
  {
    auto [control_ref, view_ref] = scenic::ViewRefPair::New();
    view_tree[kNodeC] =
        ViewNode{.parent = kNodeA, .view_ref = std::make_shared<ViewRef>(std::move(view_ref))};
  }
  {
    auto [control_ref, view_ref] = scenic::ViewRefPair::New();
    view_tree[kNodeD] =
        ViewNode{.parent = kNodeB, .view_ref = std::make_shared<ViewRef>(std::move(view_ref))};
  }

  return snapshot;
}

}  // namespace

TEST(FocusManagerTest, EmptyTransitions) {
  FocusManager focus_manager;

  EXPECT_TRUE(focus_manager.focus_chain().empty());

  // Empty snapshot should not affect the empty focus chain.
  focus_manager.OnNewViewTreeSnapshot(std::make_shared<view_tree::Snapshot>());
  EXPECT_TRUE(focus_manager.focus_chain().empty());

  // A non-empty snapshot should affect the focus chain.
  focus_manager.OnNewViewTreeSnapshot(OneNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));

  // Submitting the same snapshot again should not change the focus chain.
  focus_manager.OnNewViewTreeSnapshot(OneNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));
}

// Tree topology:
//     A
//     |
//     B
TEST(FocusManagerTest, FocusTransferDownAllowed) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());

  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeB), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

// Tree topology:
//     A
//     |
//     B
TEST(FocusManagerTest, FocusTransferToSameNode_ShouldHaveNoEffect) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());

  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeB), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));

  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeB), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

// Tree topology:
//     A
//     |
//     B
TEST(FocusManagerTest, FocusTransferToSelfAllowed) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());

  // Transfer focus to B.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeB), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));

  // Transfer focus back to A, on the authority of A.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeA), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));
}

// Tree topology:
//     A
//     |
//     B
TEST(FocusManagerTest, FocusTransferUpwardDenied) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());

  // Transfer focus to B.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeB), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));

  // Requesting change to A from B should fail and no change should be observed on the focus chain.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeB, kNodeA),
            FocusChangeStatus::kErrorRequestorNotRequestAncestor);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

// Tree topology:
//     A
//     |
//     B
TEST(FocusManagerTest, FocusTransfer_ToNonFocusableNode_Denied) {
  auto snapshot = TwoNodeSnapshot();
  snapshot->view_tree.at(kNodeB).is_focusable = false;

  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(snapshot);

  // Transfer focus to B.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeB),
            FocusChangeStatus::kErrorRequestCannotReceiveFocus);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));
}

// Tree topology:
//         A
//      /    \
//     B      C
//     |
//     D
TEST(FocusManagerTest, BranchedTree) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(FourNodeSnapshot());

  // Transfer focus from A to C.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeC), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeC));

  // Transfer focus from A to D.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeD), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB, kNodeD));

  // Transfer focus from A to B.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeB), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));

  // Transfer focus from B to D.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeB, kNodeD), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB, kNodeD));
}

// Tree topology:
//         A
//      /    \
//     B      C
//     |
//     D
TEST(FocusManagerTest, FocusTranser_WithRequestorNotInFocusChain_Denied) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(FourNodeSnapshot());

  // Transfer focus from A to C.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeC), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeC));

  // Attempt to transfer focus to D on the authority of B. Should fail since B is not in the focus
  // chain.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeB, kNodeD),
            FocusChangeStatus::kErrorRequestorNotAuthorized);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeC));
}

// Tree topology:
//         A
//      /    \
//     B      C
//     |
//     D
TEST(FocusManagerTest, SiblingTransferRequestsDenied) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(FourNodeSnapshot());

  // Setup: Transfer to "D".
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeD), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB, kNodeD));

  // Transfer request from "B" to "C" denied.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeB, kNodeC),
            FocusChangeStatus::kErrorRequestorNotRequestAncestor);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB, kNodeD));

  // Transfer request from "D" to "C" denied.
  EXPECT_EQ(focus_manager.RequestFocus(kNodeD, kNodeC),
            FocusChangeStatus::kErrorRequestorNotRequestAncestor);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB, kNodeD));
}

// Tree topology:
//     A      A     A
//     |      |
//     B  ->  B  ->    ->
//     |
//     C
TEST(FocusManagerTest, ViewRemoval_ShouldShortenFocusChain) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(ThreeNodeSnapshot());

  // Emulate a focus transfer from "A" to "C".
  EXPECT_EQ(focus_manager.RequestFocus(kNodeA, kNodeC), FocusChangeStatus::kAccept);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB, kNodeC));

  // Client "C" destroys its view.
  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));

  // Client "B" destroys its view.
  focus_manager.OnNewViewTreeSnapshot(OneNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));

  focus_manager.OnNewViewTreeSnapshot(std::make_shared<view_tree::Snapshot>());
  EXPECT_TRUE(focus_manager.focus_chain().empty());
}

// Tree topology:
//  [] -> A (AutoFocus to B)
//        |
//        B
TEST(FocusManagerTest, AutoFocus_BeforeSnapshot) {
  FocusManager focus_manager;
  focus_manager.SetAutoFocus(kNodeA, kNodeB);
  EXPECT_TRUE(focus_manager.focus_chain().empty());

  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

// Tree topology:
//  A  (AutoFocus to B)     A
//                      ->  |
//                          B
// Adding the child after setting it as auto focus should cause focus to move.
TEST(FocusManagerTest, AutoFocus_OnNewValidAutoFocusChild) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(OneNodeSnapshot());
  focus_manager.SetAutoFocus(kNodeA, kNodeB);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));

  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

// Tree topology:
//  A      A (AutoFocus to B)
//  |  ->  |
//  B      B
// Setting auto focus after adding the child should cause focus to move.
TEST(FocusManagerTest, AutoFocus_WithValidTarget) {
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));

  focus_manager.SetAutoFocus(kNodeA, kNodeB);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

// Tree topology:
//  A
//  |
//  B (AutoFocus to C)
//  |
//  C
// Focus moved due to RequestFocus() should trigger auto focus deferment.
TEST(FocusManagerTest, AutoFocus_OnRequestFocus) {
  FocusManager focus_manager;
  focus_manager.SetAutoFocus(kNodeB, kNodeC);
  focus_manager.OnNewViewTreeSnapshot(ThreeNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));

  focus_manager.RequestFocus(kNodeA, kNodeB);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB, kNodeC));
}

// Tree topology:
//  A                      A
//  |                      |
//  B (AutoFocus to C) ->  B (no AutoFocus)
//  |                      |
//  C                      C
// Same as AutoFocus_OnRequestFocus above, except we unset the auto focus target
// before requesting focus and observe no auto focus deferment happening.
TEST(FocusManagerTest, UnsetAutoFocus) {
  FocusManager focus_manager;
  focus_manager.SetAutoFocus(kNodeB, kNodeC);
  focus_manager.OnNewViewTreeSnapshot(ThreeNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));

  focus_manager.SetAutoFocus(kNodeB, ZX_KOID_INVALID);  // Unset.
  focus_manager.RequestFocus(kNodeA, kNodeB);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

// Tree topology:
//      A (AutoFocus to B)    A
//    /   \                   |
//   B     C        ->        B
//   |
//   D
// Focus moved to ViewTree changes should trigger auto focus deferment.
TEST(FocusManagerTest, AutoFocus_FocusMovedDueToViewTreeChange) {
  // Transfer focus from A to C, then set auto focus from A to B.
  FocusManager focus_manager;
  focus_manager.OnNewViewTreeSnapshot(FourNodeSnapshot());
  focus_manager.RequestFocus(kNodeA, kNodeC);
  focus_manager.SetAutoFocus(kNodeA, kNodeB);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeC));

  // When C disappears the focus should transfer to B.
  focus_manager.OnNewViewTreeSnapshot(TwoNodeSnapshot());
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

// Tree topology:
//   A (AutoFocus to D)
//   |
//   B (AutoFocus to D)
//   |
//   C (unfocusable)
//   |
//   D (unfocusable)
// Focus loops should resolve themselves to the highest node in the loop:
// A should auto transfer focus to D, which is unfocusable so focus goes to C which is unfocusable
// so focus goes to B which tries to transfer focus back to D, which would create a loop. The
// highest node in the loop is B so focus should remain there.
TEST(FocusManagerTest, AutoFocus_LoopShouldLandOnTopMostNode) {
  FocusManager focus_manager;
  auto snapshot = std::make_shared<view_tree::Snapshot>();
  {
    snapshot->root = kNodeA;
    auto& view_tree = snapshot->view_tree;
    view_tree[kNodeA] = ViewNode{.parent = ZX_KOID_INVALID, .children = {kNodeB}};
    view_tree[kNodeB] = ViewNode{.parent = kNodeA, .children = {kNodeC}};
    view_tree[kNodeC] = ViewNode{.parent = kNodeB, .children = {kNodeD}, .is_focusable = false};
    view_tree[kNodeD] = ViewNode{.parent = kNodeC, .is_focusable = false};
  }
  focus_manager.OnNewViewTreeSnapshot(snapshot);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA));

  focus_manager.SetAutoFocus(kNodeA, kNodeD);
  focus_manager.SetAutoFocus(kNodeB, kNodeD);
  EXPECT_THAT(focus_manager.focus_chain(), testing::ElementsAre(kNodeA, kNodeB));
}

class FocusChainTest : public gtest::TestLoopFixture,
                       public fuchsia::ui::focus::FocusChainListener {
 public:
  FocusChainTest() : focus_listener_(this) {}

  void OnFocusChange(
      fuchsia::ui::focus::FocusChain new_focus_chain,
      fuchsia::ui::focus::FocusChainListener::OnFocusChangeCallback callback) override {
    num_focus_chains_received_++;
    last_received_chain_.clear();
    if (new_focus_chain.has_focus_chain()) {
      for (const auto& view_ref : new_focus_chain.focus_chain()) {
        last_received_chain_.push_back(utils::ExtractKoid(view_ref));
      }
    }

    callback();
  }

  void RegisterFocusListener(FocusManager& focus_manager) {
    fidl::InterfaceHandle<FocusChainListener> listener_handle;
    focus_listener_.Bind(listener_handle.NewRequest());
    focus_manager.Register(std::move(listener_handle));
  }

  std::vector<zx_koid_t> last_received_chain_;
  uint32_t num_focus_chains_received_ = 0;

 private:
  fidl::Binding<fuchsia::ui::focus::FocusChainListener> focus_listener_;
};

TEST_F(FocusChainTest, RegisterBeforeSceneSetup_ShouldReturnEmptyFocusChain) {
  FocusManager focus_manager;

  RegisterFocusListener(focus_manager);
  RunLoopUntilIdle();
  EXPECT_TRUE(last_received_chain_.empty());
}

// A (AutoFocus B)  A (AutoFocus B)
// |                |
// B       ->       C
//                  |
//                  B
// In this case a View is inserted between A and B, where B is auto focused by A. Normally this
// would cause focus to revert to A as its place in the ViewTree is disturbed, but since A has its
// auto focus set to B focus get returned to B. We now have a situation where the focus chain has
// changed, but focus has not. Observe listeners being updated/not updated accordingly.
TEST_F(FocusChainTest, FocusChainChangedButNotFocus) {
  // Create ViewRefs.
  std::shared_ptr<const fuchsia::ui::views::ViewRef> view_ref_A;
  std::shared_ptr<const fuchsia::ui::views::ViewRef> view_ref_B;
  std::shared_ptr<const fuchsia::ui::views::ViewRef> view_ref_C;
  {
    auto [_, view_ref] = scenic::ViewRefPair::New();
    view_ref_A = std::make_shared<fuchsia::ui::views::ViewRef>(std::move(view_ref));
  }
  {
    auto [_, view_ref] = scenic::ViewRefPair::New();
    view_ref_B = std::make_shared<fuchsia::ui::views::ViewRef>(std::move(view_ref));
  }
  {
    auto [_, view_ref] = scenic::ViewRefPair::New();
    view_ref_C = std::make_shared<fuchsia::ui::views::ViewRef>(std::move(view_ref));
  }

  const zx_koid_t koid_A = utils::ExtractKoid(*view_ref_A);
  const zx_koid_t koid_B = utils::ExtractKoid(*view_ref_B);
  const zx_koid_t koid_C = utils::ExtractKoid(*view_ref_C);

  // Initialize focus manager.
  FocusManager focus_manager;
  RegisterFocusListener(focus_manager);
  fuchsia::ui::views::ViewRefFocusedPtr vrf;
  focus_manager.RegisterViewRefFocused(koid_B, vrf.NewRequest());
  int view_ref_focused_count = 0;
  vrf->Watch([&view_ref_focused_count](auto) { view_ref_focused_count++; });
  focus_manager.SetAutoFocus(koid_A, koid_B);
  RunLoopUntilIdle();
  EXPECT_EQ(num_focus_chains_received_, 1u);

  // Scene 1.
  auto snapshot = std::make_shared<view_tree::Snapshot>();
  {
    snapshot->root = koid_A;
    auto& view_tree = snapshot->view_tree;
    view_tree[koid_A] =
        ViewNode{.parent = ZX_KOID_INVALID, .children = {koid_B}, .view_ref = view_ref_A};
    view_tree[koid_B] = ViewNode{.parent = koid_A, .view_ref = view_ref_B};
  }
  focus_manager.OnNewViewTreeSnapshot(snapshot);
  RunLoopUntilIdle();

  EXPECT_EQ(num_focus_chains_received_, 2u);
  EXPECT_THAT(last_received_chain_, testing::ElementsAre(koid_A, koid_B));
  EXPECT_EQ(view_ref_focused_count, 1);

  // Scene 2.
  snapshot = std::make_shared<view_tree::Snapshot>();
  {
    snapshot->root = koid_A;
    auto& view_tree = snapshot->view_tree;
    view_tree[koid_A] =
        ViewNode{.parent = ZX_KOID_INVALID, .children = {koid_C}, .view_ref = view_ref_A};
    view_tree[koid_C] = ViewNode{.parent = koid_A, .children = {koid_B}, .view_ref = view_ref_C};
    view_tree[koid_B] = ViewNode{.parent = koid_C, .view_ref = view_ref_B};
  }
  focus_manager.OnNewViewTreeSnapshot(snapshot);
  RunLoopUntilIdle();

  EXPECT_EQ(num_focus_chains_received_, 3u);
  EXPECT_THAT(last_received_chain_, testing::ElementsAre(koid_A, koid_C, koid_B));
  EXPECT_EQ(view_ref_focused_count, 1);
}

// Topology:
//      A
//    /   \
//   B     C
//   |
//   D
TEST_F(FocusChainTest, RegisterAfterSceneSetup_ShouldReturnNonEmptyFocusChain) {
  FocusManager focus_manager;

  // New view tree should set the focus to root.
  focus_manager.OnNewViewTreeSnapshot(FourNodeSnapshotWithViewRefs());
  RegisterFocusListener(focus_manager);
  RunLoopUntilIdle();
  EXPECT_EQ(num_focus_chains_received_, 1u);
  EXPECT_EQ(last_received_chain_.size(), 1u);
}

// Topology:
//          A
//        /   \
//    -> B     C
//       |
//       D
TEST_F(FocusChainTest, NewSnapshotAfterRegister_ShouldReturnNewFocusChain) {
  FocusManager focus_manager;

  RegisterFocusListener(focus_manager);
  RunLoopUntilIdle();
  EXPECT_EQ(num_focus_chains_received_, 1u);
  EXPECT_TRUE(last_received_chain_.empty());

  focus_manager.OnNewViewTreeSnapshot(FourNodeSnapshotWithViewRefs());
  RunLoopUntilIdle();
  EXPECT_EQ(num_focus_chains_received_, 2u);
  EXPECT_EQ(last_received_chain_.size(), 1u);
}

// Topology:
//     A           A
//   /   \       /   \
//  B     C  -> B     C
//  |           |
//  D           D
TEST_F(FocusChainTest, SameSnapshotTopologyTwice_ShouldNotSendNewFocusChain) {
  FocusManager focus_manager;

  focus_manager.OnNewViewTreeSnapshot(FourNodeSnapshotWithViewRefs());
  RegisterFocusListener(focus_manager);
  RunLoopUntilIdle();
  EXPECT_EQ(num_focus_chains_received_, 1u);

  focus_manager.OnNewViewTreeSnapshot(FourNodeSnapshotWithViewRefs());
  RunLoopUntilIdle();
  EXPECT_EQ(num_focus_chains_received_, 1u);
}

class FocusManagerInspectTest : public gtest::TestLoopFixture {
 public:
  FocusManagerInspectTest()
      : inspector_(), focus_manager_(inspector_.GetRoot().CreateChild("focus_manager")) {}

  std::vector<uint64_t> GetInspectFocusChain() {
    auto hierarchy = ReadHierarchyFromInspector();
    FX_CHECK(hierarchy);
    auto focus_manager = hierarchy.value().GetByPath({"focus_manager"});
    FX_CHECK(focus_manager);
    auto focus_chain = focus_manager->node().get_property<inspect::UintArrayValue>("focus_chain");
    FX_CHECK(focus_chain);
    return focus_chain->value();
  }

  fpromise::result<inspect::Hierarchy> ReadHierarchyFromInspector() {
    fpromise::result<inspect::Hierarchy> result;
    fpromise::single_threaded_executor exec;
    exec.schedule_task(
        inspect::ReadFromInspector(inspector_).then([&](fpromise::result<inspect::Hierarchy>& res) {
          result = std::move(res);
        }));
    exec.run();

    return result;
  }

  inspect::Inspector inspector_;
  FocusManager focus_manager_;
};

// Tree topology:
//     A
//     |
//     B
//     |
//     C
TEST_F(FocusManagerInspectTest, InspectTest) {
  focus_manager_.OnNewViewTreeSnapshot(ThreeNodeSnapshot());

  // Move focus to "C".
  EXPECT_EQ(focus_manager_.RequestFocus(kNodeA, kNodeC), FocusChangeStatus::kAccept);
  EXPECT_THAT(GetInspectFocusChain(), testing::ElementsAre(kNodeA, kNodeB, kNodeC));

  // Move focus to "B".
  EXPECT_EQ(focus_manager_.RequestFocus(kNodeA, kNodeB), FocusChangeStatus::kAccept);
  EXPECT_THAT(GetInspectFocusChain(), testing::ElementsAre(kNodeA, kNodeB));

  // Move focus to "A"
  EXPECT_EQ(focus_manager_.RequestFocus(kNodeA, kNodeA), FocusChangeStatus::kAccept);
  EXPECT_THAT(GetInspectFocusChain(), testing::ElementsAre(kNodeA));
}

}  // namespace focus::test
