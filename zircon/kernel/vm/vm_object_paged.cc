// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include "vm/vm_object_paged.h"

#include <align.h>
#include <assert.h>
#include <inttypes.h>
#include <lib/console.h>
#include <lib/counters.h>
#include <lib/fit/defer.h>
#include <stdlib.h>
#include <string.h>
#include <trace.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/ops.h>
#include <fbl/alloc_checker.h>
#include <ktl/algorithm.h>
#include <ktl/array.h>
#include <ktl/move.h>
#include <vm/discardable_vmo_tracker.h>
#include <vm/fault.h>
#include <vm/page_source.h>
#include <vm/physical_page_provider.h>
#include <vm/physmap.h>
#include <vm/vm.h>
#include <vm/vm_address_region.h>
#include <vm/vm_cow_pages.h>

#include "vm_priv.h"

#include <ktl/enforce.h>

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

namespace {

KCOUNTER(vmo_attribution_queries, "vm.attributed_memory.object.queries")

}  // namespace

VmObjectPaged::VmObjectPaged(uint32_t options, fbl::RefPtr<VmHierarchyState> hierarchy_state,
                             fbl::RefPtr<VmCowPages> cow_pages, VmCowRange range)
    : VmObject(VMOType::Paged, ktl::move(hierarchy_state)),
      options_(options),
      cow_pages_(ktl::move(cow_pages)),
      cow_range_(range) {
  LTRACEF("%p\n", this);
}

VmObjectPaged::VmObjectPaged(uint32_t options, fbl::RefPtr<VmHierarchyState> hierarchy_state,
                             fbl::RefPtr<VmCowPages> cow_pages)
    : VmObjectPaged(options, ktl::move(hierarchy_state), ktl::move(cow_pages),
                    VmCowRange(0, UINT64_MAX)) {}

VmObjectPaged::~VmObjectPaged() {
  canary_.Assert();

  LTRACEF("%p\n", this);

  // VmObjectPaged initialize must always complete and is not allowed to fail, as such it should
  // always end up in the global list.
  DEBUG_ASSERT(InGlobalList());

  DestructorHelper();
}

void VmObjectPaged::DestructorHelper() {
  RemoveFromGlobalList();

  if (options_ & kAlwaysPinned) {
    Unpin(0, size());
  }

  fbl::RefPtr<VmCowPages> deferred;
  {
    Guard<VmoLockType> guard{lock()};

    // Only clear the backlink if we are not a reference. A reference does not "own" the VmCowPages,
    // so in the typical case, the VmCowPages will not have its backlink set to a reference. There
    // does exist an edge case where the backlink can be a reference, which is handled by the else
    // block below.
    if (!is_reference()) {
      cow_pages_locked()->set_paged_backlink_locked(nullptr);
    } else {
      // If this is a reference, we need to remove it from the original (parent) VMO's reference
      // list.
      VmObjectPaged* root_ref = cow_pages_locked()->get_paged_backlink_locked();
      // The VmCowPages will have a valid backlink, either to the original VmObjectPaged or a
      // reference VmObjectPaged, as long as there is a reference that is alive. We know that this
      // is a reference.
      DEBUG_ASSERT(root_ref);
      if (likely(root_ref != this)) {
        AssertHeld(root_ref->lock_ref());
        VmObjectPaged* removed = root_ref->reference_list_.erase(*this);
        DEBUG_ASSERT(removed == this);
      } else {
        // It is possible for the backlink to point to |this| if the original parent went away at
        // some point and the rest of the reference list had to be re-homed to |this|, and the
        // backlink set to |this|. The VmCowPages was pointing to us, so clear the backlink. The
        // backlink will get reset below if other references remain.
        cow_pages_locked()->set_paged_backlink_locked(nullptr);
      }
    }

    // If this VMO had references, pick one of the references as the paged backlink from the shared
    // VmCowPages. Also, move the remainder of the reference list to the chosen reference. Note that
    // we're only moving the reference list over without adding the references to the children list;
    // we do not want these references to be counted as children of the chosen VMO. We simply want a
    // safe way to propagate mapping updates and VmCowPages changes on hidden node addition.
    if (!reference_list_.is_empty()) {
      // We should only be attempting to reset the backlink if the owner is going away and has reset
      // the backlink above.
      DEBUG_ASSERT(cow_pages_locked()->get_paged_backlink_locked() == nullptr);
      VmObjectPaged* paged_backlink = reference_list_.pop_front();
      cow_pages_locked()->set_paged_backlink_locked(paged_backlink);
      AssertHeld(paged_backlink->lock_ref());
      paged_backlink->reference_list_.splice(paged_backlink->reference_list_.end(),
                                             reference_list_);
    }
    DEBUG_ASSERT(reference_list_.is_empty());
    deferred = cow_pages_;
  }
  while (deferred) {
    deferred = deferred->MaybeDeadTransition();
  }

  fbl::RefPtr<VmObjectPaged> maybe_parent;

  // Re-home all our children with any parent that we have.
  {
    Guard<CriticalMutex> child_guard{ChildListLock::Get()};
    while (!children_list_.is_empty()) {
      VmObject* c = &children_list_.front();
      children_list_.pop_front();
      VmObjectPaged* child = reinterpret_cast<VmObjectPaged*>(c);
      child->parent_ = parent_;
      if (parent_) {
        // Ignore the return since 'this' is a child so we know we are not transitioning from 0->1
        // children.
        [[maybe_unused]] bool notify = parent_->AddChildLocked(child);
        DEBUG_ASSERT(!notify);
      }
    }

    if (parent_) {
      // As parent_ is a raw pointer we must ensure that if we call a method on it that it lives
      // long enough. To do so we attempt to upgrade it to a refptr, which could fail if it's
      // already slated for deletion.
      maybe_parent = fbl::MakeRefPtrUpgradeFromRaw(parent_, child_guard);
      if (maybe_parent) {
        // Holding refptr, can safely pass in the guard to RemoveChild.
        parent_->RemoveChild(this, child_guard.take());
      } else {
        // parent is up for deletion and so there's no need to use RemoveChild since there is no
        // user dispatcher to notify anyway and so just drop ourselves to keep the hierarchy
        // correct.
        parent_->DropChildLocked(this);
      }
    }
  }
  if (maybe_parent) {
    // As we constructed a RefPtr to our parent, and we are in our own destructor, there is now
    // the potential for recursive destruction if we need to delete the parent due to holding the
    // last ref, hit this same path, etc.
    VmDeferredDeleter<VmObjectPaged>::DoDeferredDelete(ktl::move(maybe_parent));
  }
}

zx_status_t VmObjectPaged::HintRange(uint64_t offset, uint64_t len, EvictionHint hint) {
  canary_.Assert();

  if (can_block_on_page_requests() && hint == EvictionHint::AlwaysNeed) {
    lockdep::AssertNoLocksHeld();
  }

  Guard<VmoLockType> guard{lock()};

  // Ignore hints for non user-pager-backed VMOs. We choose to silently ignore hints for
  // incompatible combinations instead of failing. This is because the kernel does not make any
  // explicit guarantees on hints; since they are just hints, the kernel is always free to ignore
  // them.
  if (!cow_pages_locked()->can_root_source_evict()) {
    return ZX_OK;
  }

  auto cow_range = GetCowRangeSizeCheckLocked(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  switch (hint) {
    case EvictionHint::DontNeed: {
      cow_pages_locked()->PromoteRangeForReclamationLocked(*cow_range);
      break;
    }
    case EvictionHint::AlwaysNeed: {
      // Hints are best effort, so ignore any errors in the paging in process.
      cow_pages_locked()->ProtectRangeFromReclamationLocked(*cow_range, /*set_always_need=*/true,
                                                            /*ignore_errors=*/true, &guard);
      break;
    }
  }

  return ZX_OK;
}

zx_status_t VmObjectPaged::PrefetchRangeLocked(uint64_t offset, uint64_t len,
                                               Guard<VmoLockType>* guard) {
  auto cow_range = GetCowRangeSizeCheckLocked(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  // Cannot overflow otherwise IsBoundedBy would have failed.
  DEBUG_ASSERT(cow_range->is_page_aligned());
  if (cow_range->is_empty()) {
    return ZX_OK;
  }
  if (cow_pages_locked()->is_root_source_user_pager_backed()) {
    return cow_pages_locked()->ProtectRangeFromReclamationLocked(*cow_range,
                                                                 /*set_always_need=*/false,
                                                                 /*ignore_errors=*/false, guard);
  } else {
    // Committing high priority pages is best effort, so ignore any errors from decompressing.
    return cow_pages_locked()->DecompressInRangeLocked(*cow_range, guard);
  }
}

zx_status_t VmObjectPaged::PrefetchRange(uint64_t offset, uint64_t len) {
  canary_.Assert();
  if (can_block_on_page_requests()) {
    lockdep::AssertNoLocksHeld();
  }
  Guard<VmoLockType> guard{lock()};

  // Round offset and len to be page aligned. Use a sub-scope to validate that temporary end
  // calculations cannot be accidentally used later on.
  {
    uint64_t end;
    if (add_overflow(offset, len, &end)) {
      return ZX_ERR_OUT_OF_RANGE;
    }
    const uint64_t end_page = ROUNDUP_PAGE_SIZE(end);
    if (end_page < end) {
      return ZX_ERR_OUT_OF_RANGE;
    }
    DEBUG_ASSERT(end_page >= offset);
    offset = ROUNDDOWN(offset, PAGE_SIZE);
    len = end_page - offset;
  }

  return PrefetchRangeLocked(offset, len, &guard);
}

void VmObjectPaged::CommitHighPriorityPages(uint64_t offset, uint64_t len) {
  Guard<VmoLockType> guard{lock()};
  if (!cow_pages_locked()->is_high_memory_priority_locked()) {
    return;
  }
  // Ignore the result of the prefetch, high priority commit is best effort.
  PrefetchRangeLocked(offset, len, &guard);
}

bool VmObjectPaged::CanDedupZeroPagesLocked() {
  canary_.Assert();

  // Skip uncached VMOs as we cannot efficiently scan them.
  if ((cache_policy_ & ZX_CACHE_POLICY_MASK) != ZX_CACHE_POLICY_CACHED) {
    return false;
  }

  // Okay to dedup from this VMO.
  return true;
}

zx_status_t VmObjectPaged::CreateCommon(uint32_t pmm_alloc_flags, uint32_t options, uint64_t size,
                                        fbl::RefPtr<VmObjectPaged>* obj) {
  DEBUG_ASSERT(!(options & (kContiguous | kCanBlockOnPageRequests)));

  // Cannot be resizable and pinned, otherwise we will lose track of the pinned range.
  if ((options & kResizable) && (options & kAlwaysPinned)) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (pmm_alloc_flags & PMM_ALLOC_FLAG_CAN_WAIT) {
    options |= kCanBlockOnPageRequests;
  }

  // make sure size is page aligned
  if (!IS_PAGE_ALIGNED(size)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (size > MAX_SIZE) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  fbl::AllocChecker ac;
  fbl::RefPtr<VmHierarchyState> state;
  if constexpr (VMO_USE_SHARED_LOCK) {
    state = fbl::MakeRefCountedChecked<VmHierarchyState>(&ac);
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
  }

  ktl::unique_ptr<DiscardableVmoTracker> discardable = nullptr;
  if (options & kDiscardable) {
    discardable = ktl::make_unique<DiscardableVmoTracker>(&ac);
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
  }

  // This function isn't used to create slices or pager-backed VMOs, so VmCowPageOptions can be
  // kNone.
  fbl::RefPtr<VmCowPages> cow_pages;
  zx_status_t status = VmCowPages::Create(state, VmCowPagesOptions::kNone, pmm_alloc_flags, size,
                                          ktl::move(discardable), &cow_pages);
  if (status != ZX_OK) {
    return status;
  }

  // If this VMO will always be pinned, allocate and pin the pages in the VmCowPages prior to
  // creating the VmObjectPaged. This ensures the VmObjectPaged destructor can assume that the pages
  // are committed and pinned.
  if (options & kAlwaysPinned) {
    list_node_t prealloc_pages;
    list_initialize(&prealloc_pages);
    status = pmm_alloc_pages(size / PAGE_SIZE, pmm_alloc_flags, &prealloc_pages);
    if (status != ZX_OK) {
      return status;
    }
    Guard<VmoLockType> guard{cow_pages->lock()};
    // Add all the preallocated pages to the object, this takes ownership of all pages regardless
    // of the outcome. This is a new VMO, but this call could fail due to OOM.
    status = cow_pages->AddNewPagesLocked(0, &prealloc_pages, VmCowPages::CanOverwriteContent::Zero,
                                          true, nullptr);
    if (status != ZX_OK) {
      return status;
    }
    // With all the pages in place, pin them.
    status = cow_pages->PinRangeLocked(VmCowRange(0, size));
    ASSERT(status == ZX_OK);
  }

  auto vmo = fbl::AdoptRef<VmObjectPaged>(
      new (&ac) VmObjectPaged(options, ktl::move(state), ktl::move(cow_pages)));
  if (!ac.check()) {
    if (options & kAlwaysPinned) {
      Guard<VmoLockType> guard{cow_pages->lock()};
      cow_pages->UnpinLocked(VmCowRange(0, size));
    }
    return ZX_ERR_NO_MEMORY;
  }

  // This creation has succeeded. Must wire up the cow pages and *then* place in the globals list.
  {
    Guard<VmoLockType> guard{vmo->lock()};
    vmo->cow_pages_locked()->set_paged_backlink_locked(vmo.get());
    vmo->cow_pages_locked()->TransitionToAliveLocked();
  }
  vmo->AddToGlobalList();

  *obj = ktl::move(vmo);

  return ZX_OK;
}

zx_status_t VmObjectPaged::Create(uint32_t pmm_alloc_flags, uint32_t options, uint64_t size,
                                  fbl::RefPtr<VmObjectPaged>* obj) {
  if (options & (kContiguous | kCanBlockOnPageRequests)) {
    // Force callers to use CreateContiguous() instead.
    return ZX_ERR_INVALID_ARGS;
  }

  return CreateCommon(pmm_alloc_flags, options, size, obj);
}

zx_status_t VmObjectPaged::CreateContiguous(uint32_t pmm_alloc_flags, uint64_t size,
                                            uint8_t alignment_log2,
                                            fbl::RefPtr<VmObjectPaged>* obj) {
  DEBUG_ASSERT(alignment_log2 < sizeof(uint64_t) * 8);
  // make sure size is page aligned
  if (!IS_PAGE_ALIGNED(size)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (size > MAX_SIZE) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  fbl::AllocChecker ac;
  // For contiguous VMOs, we need a PhysicalPageProvider to reclaim specific loaned physical pages
  // on commit.
  auto page_provider = fbl::AdoptRef(new (&ac) PhysicalPageProvider(size));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  PhysicalPageProvider* physical_page_provider_ptr = page_provider.get();
  fbl::RefPtr<PageSource> page_source =
      fbl::AdoptRef(new (&ac) PageSource(ktl::move(page_provider)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  auto* page_source_ptr = page_source.get();

  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status =
      CreateWithSourceCommon(page_source, pmm_alloc_flags, kContiguous, size, &vmo);
  if (status != ZX_OK) {
    // Ensure to close the page source we created, as it will not get closed by the VmCowPages since
    // that creation failed.
    page_source->Close();
    return status;
  }

  if (size == 0) {
    *obj = ktl::move(vmo);
    return ZX_OK;
  }

  // allocate the pages
  list_node page_list;
  list_initialize(&page_list);

  size_t num_pages = size / PAGE_SIZE;
  paddr_t pa;
  status = pmm_alloc_contiguous(num_pages, pmm_alloc_flags, alignment_log2, &pa, &page_list);
  if (status != ZX_OK) {
    LTRACEF("failed to allocate enough pages (asked for %zu)\n", num_pages);
    return ZX_ERR_NO_MEMORY;
  }
  Guard<VmoLockType> guard{vmo->lock()};
  // Add them to the appropriate range of the object, this takes ownership of all the pages
  // regardless of outcome.
  // This is a newly created VMO with a page source, so we don't expect to be overwriting anything
  // in its page list.
  status = vmo->cow_pages_locked()->AddNewPagesLocked(
      0, &page_list, VmCowPages::CanOverwriteContent::None, true, nullptr);
  if (status != ZX_OK) {
    return status;
  }

  physical_page_provider_ptr->Init(vmo->cow_pages_locked(), page_source_ptr, pa);

  *obj = ktl::move(vmo);
  return ZX_OK;
}

zx_status_t VmObjectPaged::CreateFromWiredPages(const void* data, size_t size, bool exclusive,
                                                fbl::RefPtr<VmObjectPaged>* obj) {
  LTRACEF("data %p, size %zu\n", data, size);

  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = CreateCommon(PMM_ALLOC_FLAG_ANY, 0, size, &vmo);
  if (status != ZX_OK) {
    return status;
  }

  if (size > 0) {
    ASSERT(IS_PAGE_ALIGNED(size));
    ASSERT(IS_PAGE_ALIGNED(reinterpret_cast<uintptr_t>(data)));

    // Do a direct lookup of the physical pages backing the range of
    // the kernel that these addresses belong to and jam them directly
    // into the VMO.
    //
    // NOTE: This relies on the kernel not otherwise owning the pages.
    // If the setup of the kernel's address space changes so that the
    // pages are attached to a kernel VMO, this will need to change.

    paddr_t start_paddr = vaddr_to_paddr(data);
    ASSERT(start_paddr != 0);

    Guard<VmoLockType> guard{vmo->lock()};

    for (size_t count = 0; count < size / PAGE_SIZE; count++) {
      paddr_t pa = start_paddr + count * PAGE_SIZE;
      vm_page_t* page = paddr_to_vm_page(pa);
      ASSERT(page);

      if (page->state() == vm_page_state::WIRED) {
        pmm_unwire_page(page);
      } else {
        // This function is only valid for memory in the boot image,
        // which should all be wired.
        panic("page used to back static vmo in unusable state: paddr %#" PRIxPTR " state %zu\n", pa,
              VmPageStateIndex(page->state()));
      }
      // This is a newly created anonymous VMO, so we expect to be overwriting zeros. A newly
      // created anonymous VMO with no committed pages has all its content implicitly zero.
      status = vmo->cow_pages_locked()->AddNewPageLocked(
          count * PAGE_SIZE, page, VmCowPages::CanOverwriteContent::Zero, nullptr, false, nullptr);
      ASSERT_MSG(status == ZX_OK,
                 "AddNewPageLocked failed on page %zu of %zu at %#" PRIx64 " from [%#" PRIx64
                 ", %#" PRIx64 ")",
                 count, size / PAGE_SIZE, pa, start_paddr, start_paddr + size);
      DEBUG_ASSERT(!page->is_loaned());
    }

    if (exclusive && !is_physmap_addr(data)) {
      // unmap it from the kernel
      // NOTE: this means the image can no longer be referenced from original pointer
      status = VmAspace::kernel_aspace()->arch_aspace().Unmap(
          reinterpret_cast<vaddr_t>(data), size / PAGE_SIZE, ArchVmAspace::EnlargeOperation::No,
          nullptr);
      ASSERT(status == ZX_OK);
    }
    if (!exclusive) {
      // Pin all the pages as we must never decommit any of them since they are shared elsewhere.
      ASSERT(vmo->cow_range_.offset == 0);
      status = vmo->cow_pages_locked()->PinRangeLocked(VmCowRange(0, size));
      ASSERT(status == ZX_OK);
    }
  }

  *obj = ktl::move(vmo);

  return ZX_OK;
}

zx_status_t VmObjectPaged::CreateExternal(fbl::RefPtr<PageSource> src, uint32_t options,
                                          uint64_t size, fbl::RefPtr<VmObjectPaged>* obj) {
  if (options & (kDiscardable | kCanBlockOnPageRequests | kAlwaysPinned)) {
    return ZX_ERR_INVALID_ARGS;
  }

  // make sure size is page aligned
  if (!IS_PAGE_ALIGNED(size)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (size > MAX_SIZE) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // External VMOs always support delayed PMM allocations, since they already have to tolerate
  // arbitrary waits for pages due to the PageSource.
  return CreateWithSourceCommon(ktl::move(src), PMM_ALLOC_FLAG_ANY | PMM_ALLOC_FLAG_CAN_WAIT,
                                options | kCanBlockOnPageRequests, size, obj);
}

zx_status_t VmObjectPaged::CreateWithSourceCommon(fbl::RefPtr<PageSource> src,
                                                  uint32_t pmm_alloc_flags, uint32_t options,
                                                  uint64_t size, fbl::RefPtr<VmObjectPaged>* obj) {
  // Caller must check that size is page aligned.
  DEBUG_ASSERT(IS_PAGE_ALIGNED(size));
  DEBUG_ASSERT(!(options & kAlwaysPinned));

  fbl::AllocChecker ac;
  fbl::RefPtr<VmHierarchyState> state;
  if constexpr (VMO_USE_SHARED_LOCK) {
    state = fbl::MakeRefCountedChecked<VmHierarchyState>(&ac);
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
  }

  // The cow pages will have a page source, so blocking is always possible.
  options |= kCanBlockOnPageRequests;

  VmCowPagesOptions cow_options = VmCowPagesOptions::kNone;
  cow_options |= VmCowPagesOptions::kPageSourceRoot;

  if (options & kContiguous) {
    cow_options |= VmCowPagesOptions::kCannotDecommitZeroPages;
  }

  if (src->properties().is_user_pager) {
    cow_options |= VmCowPagesOptions::kUserPagerBackedRoot;
  }

  if (src->properties().is_preserving_page_content) {
    cow_options |= VmCowPagesOptions::kPreservingPageContentRoot;
  }

  fbl::RefPtr<VmCowPages> cow_pages;
  zx_status_t status =
      VmCowPages::CreateExternal(ktl::move(src), cow_options, state, size, &cow_pages);
  if (status != ZX_OK) {
    return status;
  }

  auto vmo = fbl::AdoptRef<VmObjectPaged>(
      new (&ac) VmObjectPaged(options, ktl::move(state), ktl::move(cow_pages)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  // This creation has succeeded. Must wire up the cow pages and *then* place in the globals list.
  {
    Guard<VmoLockType> guard{vmo->lock()};
    vmo->cow_pages_locked()->set_paged_backlink_locked(vmo.get());
    vmo->cow_pages_locked()->TransitionToAliveLocked();
  }
  vmo->AddToGlobalList();

  *obj = ktl::move(vmo);

  return ZX_OK;
}

zx_status_t VmObjectPaged::CreateChildSlice(uint64_t offset, uint64_t size, bool copy_name,
                                            fbl::RefPtr<VmObject>* child_vmo) {
  LTRACEF("vmo %p offset %#" PRIx64 " size %#" PRIx64 "\n", this, offset, size);

  canary_.Assert();

  // Offset must be page aligned.
  if (!IS_PAGE_ALIGNED(offset)) {
    return ZX_ERR_INVALID_ARGS;
  }

  // Make sure size is page aligned.
  if (!IS_PAGE_ALIGNED(size)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (size > MAX_SIZE) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // Slice must be wholly contained. |size()| will read the size holding the lock. This extra
  // acquisition is correct as we must drop the lock in order to perform the allocations.
  VmCowRange range;
  {
    Guard<VmoLockType> guard{lock()};
    auto cow_range = GetCowRangeSizeCheckLocked(offset, size);
    if (!cow_range) {
      return ZX_ERR_INVALID_ARGS;
    }
    range = *cow_range;
  }

  // Forbid creating children of resizable VMOs. This restriction may be lifted in the future.
  if (is_resizable()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  uint32_t options = kSlice;
  if (is_contiguous()) {
    options |= kContiguous;
  }

  // If this VMO is contiguous then we allow creating an uncached slice.  When zeroing pages that
  // are reclaimed from having been loaned from a contiguous VMO, we will zero the pages and flush
  // the zeroes to RAM.
  const bool allow_uncached = is_contiguous();
  return CreateChildReferenceCommon(options, range, allow_uncached, copy_name, nullptr, child_vmo);
}

zx_status_t VmObjectPaged::CreateChildReference(Resizability resizable, uint64_t offset,
                                                uint64_t size, bool copy_name, bool* first_child,
                                                fbl::RefPtr<VmObject>* child_vmo) {
  LTRACEF("vmo %p offset %#" PRIx64 " size %#" PRIx64 "\n", this, offset, size);

  canary_.Assert();

  // A reference spans the entirety of the parent. The specified range has no meaning, require it
  // to be zero.
  if (offset != 0 || size != 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (is_slice()) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  ASSERT(cow_range_.offset == 0);

  // Not supported for contiguous VMOs. Can use slices instead as contiguous VMOs are non-resizable
  // and support slices.
  if (is_contiguous()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (resizable == Resizability::Resizable) {
    // Cannot create a resizable reference from a non-resizable VMO.
    if (!is_resizable()) {
      return ZX_ERR_NOT_SUPPORTED;
    }
  }

  uint32_t options = 0;

  // Reference inherits resizability from parent.
  if (is_resizable()) {
    options |= kResizable;
  }

  return CreateChildReferenceCommon(options, VmCowRange(0, UINT64_MAX), false, copy_name,
                                    first_child, child_vmo);
}

zx_status_t VmObjectPaged::CreateChildReferenceCommon(uint32_t options, VmCowRange range,
                                                      bool allow_uncached, bool copy_name,
                                                      bool* first_child,
                                                      fbl::RefPtr<VmObject>* child_vmo) {
  canary_.Assert();

  options |= kReference;

  if (can_block_on_page_requests()) {
    options |= kCanBlockOnPageRequests;
  }

  // Reference shares the same VmCowPages as the parent.
  fbl::RefPtr<VmObjectPaged> vmo;
  {
    Guard<VmoLockType> guard{lock()};

    // We know that we are not contiguous so we should not be uncached either.
    if (cache_policy_ != ARCH_MMU_FLAG_CACHED && !allow_uncached) {
      return ZX_ERR_BAD_STATE;
    }

    // Once all fallible checks are performed, construct the VmObjectPaged.
    fbl::AllocChecker ac;
    fbl::RefPtr<VmHierarchyState> state;
#if VMO_USE_SHARED_LOCK
    state = hierarchy_state_ptr_;
#endif
    vmo = fbl::AdoptRef<VmObjectPaged>(
        new (&ac) VmObjectPaged(options, ktl::move(state), cow_pages_, range));
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
    AssertHeld(vmo->lock_ref());

    vmo->cache_policy_ = cache_policy_;
    {
      Guard<CriticalMutex> child_guard{ChildListLock::Get()};
      vmo->parent_ = this;
      const bool first = AddChildLocked(vmo.get());
      if (first_child) {
        *first_child = first;
      }
    }

    // Also insert into the reference list. The reference should only be inserted in the list of the
    // object that the cow_pages_locked() has the backlink to, i.e. the notional "owner" of the
    // VmCowPages.
    // As a consequence of this, in the case of nested references, the reference relationship can
    // look different from the parent->child relationship, which instead mirrors the child creation
    // calls as specified by the user (this is true for all child types).
    VmObjectPaged* paged_owner = cow_pages_locked()->get_paged_backlink_locked();
    // The VmCowPages we point to should have a valid backlink, either to us or to our parent (if we
    // are a reference).
    DEBUG_ASSERT(paged_owner);
    // If this object is not a reference, the |paged_owner| we computed should be the same as
    // |this|.
    DEBUG_ASSERT(is_reference() || paged_owner == this);
    AssertHeld(paged_owner->lock_ref());
    paged_owner->reference_list_.push_back(vmo.get());

    if (copy_name) {
      vmo->name_ = name_;
    }
  }

  // Add to the global list now that fully initialized.
  vmo->AddToGlobalList();

  *child_vmo = ktl::move(vmo);

  return ZX_OK;
}

zx_status_t VmObjectPaged::CreateClone(Resizability resizable, SnapshotType type, uint64_t offset,
                                       uint64_t size, bool copy_name,
                                       fbl::RefPtr<VmObject>* child_vmo) {
  LTRACEF("vmo %p offset %#" PRIx64 " size %#" PRIx64 "\n", this, offset, size);

  canary_.Assert();

  // Copy-on-write clones of contiguous VMOs do not have meaningful semantics, so forbid them.
  if (is_contiguous()) {
    return ZX_ERR_INVALID_ARGS;
  }

  // offset must be page aligned
  if (!IS_PAGE_ALIGNED(offset)) {
    return ZX_ERR_INVALID_ARGS;
  }

  // size must be page aligned and not too large.
  if (!IS_PAGE_ALIGNED(size)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (size > MAX_SIZE) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  auto cow_range = GetCowRange(offset, size);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  fbl::RefPtr<VmObjectPaged> vmo;

  {
    Guard<VmoLockType> guard{lock()};
    // check that we're not uncached in some way
    if (cache_policy_ != ARCH_MMU_FLAG_CACHED) {
      return ZX_ERR_BAD_STATE;
    }

    // If we are a slice we require a unidirection clone, as performing a bi-directional clone
    // through a slice does not yet have defined semantics.
    const bool require_unidirection = is_slice();
    auto result = cow_pages_locked()->CreateCloneLocked(type, require_unidirection, *cow_range);
    if (result.is_error()) {
      return result.error_value();
    }

    uint32_t options = 0;
    if (resizable == Resizability::Resizable) {
      options |= kResizable;
    }
    if (can_block_on_page_requests()) {
      options |= kCanBlockOnPageRequests;
    }
    fbl::AllocChecker ac;
    fbl::RefPtr<VmHierarchyState> state;
#if VMO_USE_SHARED_LOCK
    state = hierarchy_state_ptr_;
#endif
    auto [child, child_lock] = (*result).take();
    vmo = fbl::AdoptRef<VmObjectPaged>(
        new (&ac) VmObjectPaged(options, ktl::move(state), ktl::move(child)));
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
    Guard<VmoLockType> child_guard{AdoptLock, vmo->lock(), ktl::move(child_lock)};
    DEBUG_ASSERT(vmo->cache_policy_ == ARCH_MMU_FLAG_CACHED);

    // Now that everything has succeeded we can wire up cow pages references. VMO will be placed in
    // the global list later once lock has been dropped.
    vmo->cow_pages_locked()->set_paged_backlink_locked(vmo.get());
    vmo->cow_pages_locked()->TransitionToAliveLocked();

    // Install the parent.
    {
      Guard<CriticalMutex> list_guard{ChildListLock::Get()};
      vmo->parent_ = this;

      // add the new vmo as a child before we do anything, since its
      // dtor expects to find it in its parent's child list
      AddChildLocked(vmo.get());
    }

    if (copy_name) {
      vmo->name_ = name_;
    }
  }

  // Add to the global list now that fully initialized.
  vmo->AddToGlobalList();

  *child_vmo = ktl::move(vmo);

  return ZX_OK;
}

void VmObjectPaged::DumpLocked(uint depth, bool verbose) const {
  canary_.Assert();

  uint64_t parent_id = 0;
  // Cache the parent value as a void* as it's not safe to dereference once the ChildListLock is
  // dropped, but we can still print out its value.
  void* parent;
  {
    Guard<CriticalMutex> guard{ChildListLock::Get()};
    parent = parent_;
    if (parent_) {
      parent_id = parent_->user_id();
    }
  }

  for (uint i = 0; i < depth; ++i) {
    printf("  ");
  }
  printf("vmo %p/k%" PRIu64 " ref %d parent %p/k%" PRIu64 "\n", this, user_id_.load(),
         ref_count_debug(), parent, parent_id);

  char name[ZX_MAX_NAME_LEN];
  get_name(name, sizeof(name));
  if (strlen(name) > 0) {
    for (uint i = 0; i < depth + 1; ++i) {
      printf("  ");
    }
    printf("name %s\n", name);
  }

  cow_pages_locked()->DumpLocked(depth, verbose);
}

VmObject::AttributionCounts VmObjectPaged::GetAttributedMemoryInRangeLocked(
    uint64_t offset_bytes, uint64_t len_bytes) const {
  vmo_attribution_queries.Add(1);

  // A reference never has memory attributed to it. It points to the parent's VmCowPages, and we
  // need to hold the invariant that we don't double-count attributed memory.
  //
  // TODO(https://fxbug.dev/42069078): Consider attributing memory to the current VmCowPages
  // backlink for the case where the parent has gone away.
  if (is_reference()) {
    return AttributionCounts{};
  }
  ASSERT(cow_range_.offset == 0);
  uint64_t new_len_bytes;
  if (!TrimRange(offset_bytes, len_bytes, size_locked(), &new_len_bytes)) {
    return AttributionCounts{};
  }

  auto cow_range = GetCowRange(offset_bytes, new_len_bytes);
  return cow_pages_locked()->GetAttributedMemoryInRangeLocked(*cow_range);
}

zx_status_t VmObjectPaged::CommitRangeInternal(uint64_t offset, uint64_t len, bool pin,
                                               bool write) {
  canary_.Assert();
  LTRACEF("offset %#" PRIx64 ", len %#" PRIx64 "\n", offset, len);

  if (can_block_on_page_requests()) {
    lockdep::AssertNoLocksHeld();
  }

  // We only expect write to be set if this a pin. All non-pin commits are reads.
  DEBUG_ASSERT(!write || pin);

  Guard<VmoLockType> guard{lock()};

  // Child slices of VMOs are currently not resizable, nor can they be made
  // from resizable parents.  If this ever changes, the logic surrounding what
  // to do if a VMO gets resized during a Commit or Pin operation will need to
  // be revisited.  Right now, we can just rely on the fact that the initial
  // vetting/trimming of the offset and length of the operation will never
  // change if the operation is being executed against a child slice.
  DEBUG_ASSERT(!is_resizable() || !is_slice());

  // Round offset and len to be page aligned. Use a sub-scope to validate that temporary end
  // calculations cannot be accidentally used later on.
  {
    uint64_t end;
    if (add_overflow(offset, len, &end)) {
      return ZX_ERR_OUT_OF_RANGE;
    }
    const uint64_t end_page = ROUNDUP_PAGE_SIZE(end);
    if (end_page < end) {
      return ZX_ERR_OUT_OF_RANGE;
    }
    DEBUG_ASSERT(end_page >= offset);
    offset = ROUNDDOWN(offset, PAGE_SIZE);
    len = end_page - offset;
  }

  // If a pin is requested the entire range must exist and be valid.
  if (pin) {
    // If pinning we explicitly forbid zero length pins as we cannot guarantee consistent semantics.
    // For example pinning a zero length range outside the range of the VMO is an error, and so
    // pinning a zero length range inside the vmo and then resizing the VMO smaller than the pin
    // region should also be an error. To enforce this without having to have new metadata to track
    // zero length pin regions is to just forbid them. Note that the user entry points for pinning
    // already forbid zero length ranges.
    if (unlikely(len == 0)) {
      return ZX_ERR_INVALID_ARGS;
    }
    // verify that the range is within the object
    if (unlikely(!InRange(offset, len, size_locked()))) {
      return ZX_ERR_OUT_OF_RANGE;
    }
  } else {
    // verify that the range is within the object
    if (!InRange(offset, len, size_locked())) {
      return ZX_ERR_OUT_OF_RANGE;
    }
    // was in range, just zero length
    if (len == 0) {
      return ZX_OK;
    }
  }

  // Tracks the end of the pinned range to unpin in case of failure. The |offset| might lag behind
  // the pinned range, as it tracks the range that has been completely processed, which would
  // also include dirtying the page after pinning in case of a write.
  uint64_t pinned_end_offset = offset;
  // Should any errors occur we need to unpin everything. If we were asked to write, we need to mark
  // the VMO modified if any pages were committed.
  auto deferred_cleanup =
      fit::defer([this, original_offset = offset, &offset, &len, &pinned_end_offset, pin, write]() {
        AssertHeld(lock_ref());
        // If we were not able to pin the entire range, i.e. len is not 0, we need to unpin
        // everything. Regardless of any resizes or other things that may have happened any pinned
        // pages *must* still be within a valid range, and so we know Unpin should succeed. The edge
        // case is if we had failed to pin *any* pages and so our original offset may be outside the
        // current range of the vmo. Additionally, as pinning a zero length range is invalid, so is
        // unpinning, and so we must avoid.
        if (pin && len > 0 && pinned_end_offset > original_offset) {
          auto cow_range = GetCowRange(original_offset, pinned_end_offset - original_offset);
          cow_pages_locked()->UnpinLocked(*cow_range);
        } else if (write && offset > original_offset) {
          // Mark modified as we successfully committed pages for writing *and* we did not end up
          // undoing a partial pin (the if-block above).
          mark_modified_locked();
        }
      });

  __UNINITIALIZED MultiPageRequest page_request;
  // Convenience lambda to advance offset by processed_len, indicating that all pages in the range
  // [offset, offset + processed_len) have been processed, then potentially wait on the page_request
  // (if wait_on_page_request is set to true), and revalidate range checks after waiting.
  auto advance_processed_range = [&](uint64_t processed_len,
                                     bool wait_on_page_request) -> zx_status_t {
    offset += processed_len;
    len -= processed_len;

    if (wait_on_page_request) {
      // If the length is now zero we should not be waiting on a page request. This is both
      // nonsensical, as we have already done all we needed, but also an error since if the wait
      // were to fail we would error the commit, but not undo any potential pinning.
      DEBUG_ASSERT(len > 0);
      DEBUG_ASSERT(can_block_on_page_requests());
      zx_status_t wait_status = ZX_OK;
      AssertHeld(lock_ref());
      guard.CallUnlocked(
          [&page_request, &wait_status]() mutable { wait_status = page_request.Wait(); });
      if (wait_status != ZX_OK) {
        if (wait_status == ZX_ERR_TIMED_OUT) {
          DumpLocked(0, false);
        }
        return wait_status;
      }

      // Re-run the range checks, since size_ could have changed while we were blocked. This
      // is not a failure, since the arguments were valid when the syscall was made. It's as
      // if the commit was successful but then the pages were thrown away. Unless we are pinning,
      // in which case pages being thrown away is explicitly an error.
      if (pin) {
        // verify that the range is within the object
        if (unlikely(!InRange(offset, len, size_locked()))) {
          return ZX_ERR_OUT_OF_RANGE;
        }
      } else {
        uint64_t new_len = len;
        if (!TrimRange(offset, len, size_locked(), &new_len)) {
          // No remaining range to process. Set len to 0 so that the top level loop can exit.
          len = 0;
          return ZX_OK;
        }
        len = new_len;
      }
    }
    return ZX_OK;
  };

  // As we may need to wait on arbitrary page requests we just keep running this as long as there is
  // a non-zero range to process.
  while (len > 0) {
    uint64_t committed_len = 0;
    zx_status_t commit_status = cow_pages_locked()->CommitRangeLocked(
        *GetCowRange(offset, len), &committed_len, &page_request);
    DEBUG_ASSERT(committed_len <= len);

    // Now we can exit if we received any error states.
    if (commit_status != ZX_OK && commit_status != ZX_ERR_SHOULD_WAIT) {
      return commit_status;
    }

    // If we're required to pin, try to pin the committed range before waiting on the page_request,
    // which has been populated to request pages beyond the committed range.
    // Even though the page_request has already been initialized, we choose to first completely
    // process the committed range, which could end up canceling the already initialized page
    // request. This allows us to keep making forward progress as we will potentially pin a few
    // pages before trying to fault in further pages, thereby preventing the already committed (and
    // pinned) pages from being evicted while we wait with the lock dropped.
    if (pin && committed_len > 0) {
      uint64_t non_loaned_len = 0;
      zx_status_t replace_status = ZX_OK;
      if (cow_pages_locked()->can_borrow_locked()) {
        // We need to replace any loaned pages in the committed range with non-loaned pages first,
        // since pinning expects all pages to be non-loaned. Replacing loaned pages requires a page
        // request too. At any time we'll only be able to wait on a single page request, and after
        // the wait the conditions that resulted in the previous request might have changed, so we
        // can just cancel and reuse the existing page_request.
        // TODO: consider not canceling this and the other request below. The issue with not
        // canceling is that without early wake support, i.e. being able to reinitialize an existing
        // initialized request, I think this code will not work without canceling.
        page_request.CancelRequests();
        replace_status = cow_pages_locked()->ReplacePagesWithNonLoanedLocked(
            *GetCowRange(offset, committed_len), page_request.GetAnonymous(), &non_loaned_len);
        DEBUG_ASSERT(non_loaned_len <= committed_len);
        if (replace_status == ZX_OK) {
          DEBUG_ASSERT(non_loaned_len == committed_len);
        } else if (replace_status != ZX_ERR_SHOULD_WAIT) {
          return replace_status;
        }
      } else {
        // Borrowing not available so we know there are no loaned pages.
        non_loaned_len = committed_len;
        // As we have not canceled the page_request in this branch, duplicate the commit_status into
        // the replace_status so that later code knows whether there is still a page_request to wait
        // on or not.
        replace_status = commit_status;
      }

      // We can safely pin the non-loaned range before waiting on the page request.
      if (non_loaned_len > 0) {
        // Verify that we are starting the pin after the previously pinned range, as we do not want
        // to repeatedly pin the same pages.
        ASSERT(pinned_end_offset == offset);
        zx_status_t pin_status =
            cow_pages_locked()->PinRangeLocked(*GetCowRange(offset, non_loaned_len));
        if (pin_status != ZX_OK) {
          return pin_status;
        }
      }
      // At this point we have successfully committed and pinned non_loaned_len.
      uint64_t pinned_len = non_loaned_len;
      pinned_end_offset = offset + pinned_len;

      // If this is a write and the VMO supports dirty tracking, we also need to mark the pinned
      // pages Dirty.
      // We pin the pages first before marking them dirty in order to guarantee forward progress.
      // Pinning the pages will prevent them from getting decommitted while we are waiting on the
      // dirty page request without the lock held.
      if (write && pinned_len > 0 && is_dirty_tracked()) {
        // Prepare the committed range for writing. We need a page request for this too, so cancel
        // any existing one and reuse it.
        page_request.CancelRequests();

        // We want to dirty the entire pinned range.
        uint64_t to_dirty_len = pinned_len;
        while (to_dirty_len > 0) {
          uint64_t dirty_len = 0;
          zx_status_t write_status = cow_pages_locked()->PrepareForWriteLocked(
              *GetCowRange(offset, to_dirty_len), page_request.GetLazyDirtyRequest(), &dirty_len);
          DEBUG_ASSERT(dirty_len <= to_dirty_len);
          if (write_status != ZX_OK && write_status != ZX_ERR_SHOULD_WAIT) {
            return write_status;
          }
          if (write_status == ZX_ERR_SHOULD_WAIT) {
            page_request.MadeDirtyRequest();
          }
          // Account for the pages that were dirtied during this attempt.
          to_dirty_len -= dirty_len;

          // At this point we have successfully committed, pinned, and dirtied dirty_len. This is
          // where we need to restart the next call to PrepareForWriteLocked. Advance the offset to
          // reflect that, and then wait on the page request beyond dirty_len (if any).
          zx_status_t wait_status = advance_processed_range(
              dirty_len, /*wait_on_page_request=*/write_status == ZX_ERR_SHOULD_WAIT);
          if (wait_status != ZX_OK) {
            return wait_status;
          }
          // Retry dirtying pages beyond dirty_len. Note that it is fine to resume the inner loop
          // here and directly call PrepareForWriteLocked after advancing the offset because the
          // pages were pinned previously, and so they could not have gotten decommitted while we
          // waited on the page request.
          if (write_status == ZX_ERR_SHOULD_WAIT) {
            // Resume the loop that repeatedly calls PrepareForWriteLocked until all the pinned
            // pages have been marked dirty.
            continue;
          }
        }
      } else {
        // We did not need to perform any dirty tracking. So we can advance the offset over the
        // pinned length. Now that we've dealt with all the pages in the non-loaned range, wait on
        // the page request for offsets beyond (if any).
        zx_status_t wait_status = advance_processed_range(
            pinned_len, /*wait_on_page_request=*/replace_status == ZX_ERR_SHOULD_WAIT);
        if (wait_status != ZX_OK) {
          return wait_status;
        }
      }
      // If we dropped the lock while waiting, things might have changed, so can reattempt
      // committing beyond the length we had successfully pinned before waiting. Alternatively if we
      // canceled that page request in favor of potentially making a dirty request we still have
      // unfinished work and need to go around the loop again.
      if (replace_status == ZX_ERR_SHOULD_WAIT) {
        continue;
      }
    } else {
      // We were either not required to pin, or committed_len was 0. We need to update how much was
      // committed, and then wait on the page request (if any).
      zx_status_t wait_status = advance_processed_range(
          committed_len, /*wait_on_page_request=*/commit_status == ZX_ERR_SHOULD_WAIT);
      if (wait_status != ZX_OK) {
        return wait_status;
      }
      // After we're done waiting on the page request, we loop around with the same |offset| and
      // |len|, so that we can reprocess the range populated by the page request, with another
      // call to VmCowPages::CommitRangeLocked(). This is required to make any COW copies of pages
      // that were just supplied.
      // - The first call to VmCowPages::CommitRangeLocked() returns early from
      // LookupCursor::RequireOwnedPage with ZX_ERR_SHOULD_WAIT after queueing a page request
      // for the absent page.
      // - The second call to VmCowPages::CommitRangeLocked() calls LookupCursor::RequireOwnedPage
      // which copies out the now present page (if required).
      if (commit_status == ZX_ERR_SHOULD_WAIT) {
        continue;
      }
    }

    // If commit was successful we should have no more to process.
    DEBUG_ASSERT(commit_status != ZX_OK || len == 0);
  }
  return ZX_OK;
}

zx_status_t VmObjectPaged::DecommitRange(uint64_t offset, uint64_t len) {
  canary_.Assert();
  LTRACEF("offset %#" PRIx64 ", len %#" PRIx64 "\n", offset, len);
  Guard<VmoLockType> guard{lock()};
  if (is_contiguous() && !pmm_physical_page_borrowing_config()->is_loaning_enabled()) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  return DecommitRangeLocked(offset, len);
}

zx_status_t VmObjectPaged::DecommitRangeLocked(uint64_t offset, uint64_t len) {
  canary_.Assert();

  auto cow_range = GetCowRangeSizeCheckLocked(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // Decommit of pages from a contiguous VMO relies on contiguous VMOs not being resizable.
  DEBUG_ASSERT(!is_resizable() || !is_contiguous());

  return cow_pages_locked()->DecommitRangeLocked(*cow_range);
}

zx_status_t VmObjectPaged::ZeroPartialPageLocked(uint64_t page_base_offset,
                                                 uint64_t zero_start_offset,
                                                 uint64_t zero_end_offset,
                                                 Guard<VmoLockType>* guard) {
  DEBUG_ASSERT(zero_start_offset <= zero_end_offset);
  DEBUG_ASSERT(zero_end_offset <= PAGE_SIZE);
  DEBUG_ASSERT(IS_PAGE_ALIGNED(page_base_offset));
  DEBUG_ASSERT(page_base_offset < size_locked());

  // TODO: Consider replacing this with a more appropriate generic API when one is available.
  if (cow_pages_locked()->PageWouldReadZeroLocked(page_base_offset)) {
    // This is already considered zero so no need to redundantly zero again.
    return ZX_OK;
  }

  // Need to actually zero out bytes in the page.
  return ReadWriteInternalLocked(
      page_base_offset + zero_start_offset, zero_end_offset - zero_start_offset, true,
      VmObjectReadWriteOptions::None,
      [](void* dst, size_t offset, size_t len) -> UserCopyCaptureFaultsResult {
        // We're memsetting the *kernel* address of an allocated page, so we know that this
        // cannot fault. memset may not be the most efficient, but we don't expect to be doing
        // this very often.
        memset(dst, 0, len);
        return UserCopyCaptureFaultsResult{ZX_OK};
      },
      guard);
}

zx_status_t VmObjectPaged::ZeroRangeInternal(uint64_t offset, uint64_t len, bool dirty_track) {
  canary_.Assert();
  if (can_block_on_page_requests()) {
    lockdep::AssertNoLocksHeld();
  }
  // May need to zero in chunks across multiple different lock acquisitions so loop until nothing
  // left to do.
  while (len > 0) {
    // We might need a page request if the VMO is backed by a page source.
    __UNINITIALIZED MultiPageRequest page_request;
    uint64_t zeroed_len = 0;
    zx_status_t status;
    {
      Guard<VmoLockType> guard{lock()};

      // Zeroing a range behaves as if it were an efficient zx_vmo_write. As we cannot write to
      // uncached vmo, we also cannot zero an uncahced vmo.
      if (cache_policy_ != ARCH_MMU_FLAG_CACHED) {
        return ZX_ERR_BAD_STATE;
      }

      // Validate the range.
      ktl::optional<VmCowRange> cow_range = GetCowRangeSizeCheckLocked(offset, len);
      if (!cow_range) {
        return ZX_ERR_OUT_OF_RANGE;
      }

      // Check for any non-page aligned start and handle separately.
      if (!IS_PAGE_ALIGNED(offset)) {
        // We're doing partial page writes, so we should be dirty tracking.
        DEBUG_ASSERT(dirty_track);
        const uint64_t page_base = ROUNDDOWN(offset, PAGE_SIZE);
        const uint64_t zero_start_offset = offset - page_base;
        const uint64_t zero_len = ktl::min(PAGE_SIZE - zero_start_offset, len);
        status = ZeroPartialPageLocked(page_base, zero_start_offset, zero_start_offset + zero_len,
                                       &guard);
        if (status != ZX_OK) {
          return status;
        }
        // Advance over the length we zeroed and then, since the lock might have been dropped, go
        // around the loop to redo the checks.
        offset += zero_len;
        len -= zero_len;
        continue;
      }
      // The start is page aligned, so if the remaining length is not a page size then perform the
      // final sub-page zero.
      if (len < PAGE_SIZE) {
        DEBUG_ASSERT(dirty_track);
        return ZeroPartialPageLocked(offset, 0, len, &guard);
      }
      // Offset is page aligned, and we have at least one full page to process, so find the page
      // aligned length to hand over to the cow pages zero method.
      VmCowRange zero_range = cow_range->WithLength(ROUNDDOWN(cow_range->len, PAGE_SIZE));

#if DEBUG_ASSERT_IMPLEMENTED
      // Currently we want ZeroPagesLocked() to not decommit any pages from a contiguous VMO.  In
      // debug we can assert that (not a super fast assert, but seems worthwhile; it's debug only).
      uint64_t page_count_before =
          is_contiguous() ? cow_pages_locked()->DebugGetPageCountLocked() : 0;
#endif
      // Now that we have a page aligned range we can try hand over to the cow pages zero method.
      status =
          cow_pages_locked()->ZeroPagesLocked(zero_range, dirty_track, &page_request, &zeroed_len);
      if (zeroed_len != 0) {
        // Mark modified since we wrote zeros.
        mark_modified_locked();
      }

#if DEBUG_ASSERT_IMPLEMENTED
      if (is_contiguous()) {
        uint64_t page_count_after = cow_pages_locked()->DebugGetPageCountLocked();
        DEBUG_ASSERT(page_count_after == page_count_before);
      }
#endif
    }

    // Wait on any page request, which is the only non-fatal error case.
    if (status == ZX_ERR_SHOULD_WAIT) {
      status = page_request.Wait();
      if (status == ZX_ERR_TIMED_OUT) {
        Dump(0, false);
      }
    }
    if (status != ZX_OK) {
      return status;
    }
    // Advance over pages that had already been zeroed.
    offset += zeroed_len;
    len -= zeroed_len;
  }
  return ZX_OK;
}

zx_status_t VmObjectPaged::Resize(uint64_t s) {
  canary_.Assert();

  LTRACEF("vmo %p, size %" PRIu64 "\n", this, s);

  DEBUG_ASSERT(!is_contiguous() || !is_resizable());
  // Also rejects contiguous VMOs.
  if (!is_resizable()) {
    return ZX_ERR_UNAVAILABLE;
  }

  // ensure the size is valid and that we will not wrap.
  if (!IS_PAGE_ALIGNED(s)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (s > MAX_SIZE) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  return cow_pages_->Resize(s);
}

// perform some sort of copy in/out on a range of the object using a passed in lambda for the copy
// routine. The copy routine has the expected type signature of: (void *ptr, uint64_t offset,
//  uint64_t len) -> UserCopyCaptureFaultsResult.
template <typename T>
zx_status_t VmObjectPaged::ReadWriteInternalLocked(uint64_t offset, size_t len, bool write,
                                                   VmObjectReadWriteOptions options, T copyfunc,
                                                   Guard<VmoLockType>* guard) {
  canary_.Assert();

  uint64_t end_offset;
  if (add_overflow(offset, len, &end_offset)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // Declare a lambda that will check any object properties we require to be true and, if can_trim
  // is set, reduce the requested length if it exceeds the the VMO size. We place these in a lambda
  // so that we can perform them any time the lock is dropped.
  const bool can_trim = !!(options & VmObjectReadWriteOptions::TrimLength);
  auto check_and_trim = [this, can_trim, &end_offset]() TA_REQ(lock()) -> zx_status_t {
    if (cache_policy_ != ARCH_MMU_FLAG_CACHED) {
      return ZX_ERR_BAD_STATE;
    }
    const uint64_t size = size_locked();
    if (end_offset > size) {
      if (can_trim) {
        end_offset = size;
      } else {
        return ZX_ERR_OUT_OF_RANGE;
      }
    }
    return ZX_OK;
  };

  // Perform initial check.
  if (zx_status_t status = check_and_trim(); status != ZX_OK) {
    return status;
  }

  // Track our two offsets.
  uint64_t src_offset = offset;
  size_t dest_offset = 0;

  auto mark_modified = fit::defer([this, &dest_offset, write]() {
    if (write && dest_offset > 0) {
      // We wrote something, so mark as modified.
      AssertHeld(lock_ref());
      mark_modified_locked();
    }
  });

  // The PageRequest is a non-trivial object so we declare it outside the loop to avoid having to
  // construct and deconstruct it each iteration. It is tolerant of being reused and will
  // reinitialize itself if needed.
  // Ideally we can wake up early from the page request to begin processing any partially supplied
  // ranges. However, if performing a write to a dirty tracked VMO this is not presently possible as
  // we need to first read in the range and then dirty it, and we cannot have both a read and dirty
  // request outstanding at one time.
  __UNINITIALIZED MultiPageRequest page_request(!write);
  // Copy loop uses a custom status variant to track its state so that it easily create an
  // unambiguous distinction between no error and no error but the lock has been dropped.
  // Overloading one of the zx_status_t values (such as ZX_ERR_NEXT or ZX_ERR_SHOULD_WAIT) to mean
  // this is confusing and error prone. The downside to this approach is the StatusType is 8 bytes
  // versus the 4 bytes of a zx_status_t.
  struct LockDroppedTag {
    bool operator==(const LockDroppedTag& other) const { return true; }
    bool operator!=(const LockDroppedTag& other) const { return false; }
  };
  using StatusType = ktl::variant<zx_status_t, LockDroppedTag>;
  while (src_offset < end_offset) {
    const size_t first_page_offset = ROUNDDOWN(src_offset, PAGE_SIZE);
    const size_t last_page_offset = ROUNDDOWN(end_offset - 1, PAGE_SIZE);
    size_t remaining_pages = (last_page_offset - first_page_offset) / PAGE_SIZE + 1;
    size_t pages_since_last_unlock = 0;
    __UNINITIALIZED zx::result<VmCowPages::LookupCursor> cursor =
        GetLookupCursorLocked(first_page_offset, remaining_pages * PAGE_SIZE);
    if (cursor.is_error()) {
      return cursor.status_value();
    }
    // Performing explicit accesses by request of the user, so disable zero forking.
    cursor->DisableZeroFork();
    AssertHeld(cursor->lock_ref());

    StatusType status = ZX_OK;
    while (remaining_pages > 0) {
      const size_t page_offset = src_offset % PAGE_SIZE;
      const size_t tocopy = ktl::min(PAGE_SIZE - page_offset, end_offset - src_offset);

      // If we need to wait on pages then we would like to wait on as many as possible, up to the
      // actual limit of the read/write operation. For a read we can wake up once some pages are
      // received, minimizing the latency before we start making progress, but as this is not true
      // for writes we cap the maximum number requested.
      constexpr uint64_t kMaxWriteWaitPages = 16;
      const uint64_t max_wait_pages = write ? kMaxWriteWaitPages : UINT64_MAX;
      const uint64_t max_waitable_pages = ktl::min(remaining_pages, max_wait_pages);

      // Attempt to lookup a page
      __UNINITIALIZED zx::result<VmCowPages::LookupCursor::RequireResult> result =
          cursor->RequirePage(write, static_cast<uint>(max_waitable_pages), &page_request);

      status = result.status_value();
      if (status == StatusType(ZX_OK)) {
        // Compute the kernel mapping of this page.
        const paddr_t pa = result->page->paddr();
        char* page_ptr = reinterpret_cast<char*>(paddr_to_physmap(pa));

        // Call the copy routine. If the copy was successful then ZX_OK is returned, otherwise
        // ZX_ERR_SHOULD_WAIT may be returned to indicate the copy failed but we can retry it.
        UserCopyCaptureFaultsResult copy_result =
            copyfunc(page_ptr + page_offset, dest_offset, tocopy);

        // If a fault has actually occurred, then we will have captured fault info that we can use
        // to handle the fault.
        if (copy_result.fault_info.has_value()) {
          guard->CallUnlocked(
              [&info = *copy_result.fault_info, to_fault = len - dest_offset, &status] {
                // If status is not ZX_OK, there is no guarantee that any of the data has been
                // copied.
                status = Thread::Current::SoftFaultInRange(info.pf_va, info.pf_flags, to_fault);
              });
          if (status == StatusType(ZX_OK)) {
            status = LockDroppedTag{};
          }
        } else {
          // If we encounter _any_ unrecoverable error from the copy operation which
          // produced no fault address, squash the error down to just "NOT_FOUND".
          // This is what the SoftFault error would have told us if we did try to
          // handle the fault and could not.
          if (copy_result.status != ZX_OK) {
            status = ZX_ERR_NOT_FOUND;
          }
        }
      } else if (status == StatusType(ZX_ERR_SHOULD_WAIT)) {
        // RequirePage 'failed', but told us that it had filled out the page request, so we should
        // wait on it. Waiting on the page request must be done with the lock dropped.
        DEBUG_ASSERT(can_block_on_page_requests());
        guard->CallUnlocked([&status, &page_request]() { status = page_request.Wait(); });
        if (likely(status == StatusType(ZX_OK))) {
          // page request waiting succeeded, but indicate that the lock has been dropped.
          status = LockDroppedTag{};
        } else if (status == StatusType(ZX_ERR_TIMED_OUT)) {
          DumpLocked(0, false);
        }
      }
      // If any 'errors', including having dropped the lock, exit back to the outer loop to handle
      // and/or retry.
      if (status != StatusType(ZX_OK)) {
        break;
      }

      // Advance the copy location.
      src_offset += tocopy;
      dest_offset += tocopy;
      remaining_pages--;

      // Periodically yield the lock in order to allow other read or write
      // operations to advance sooner than they otherwise would.
      constexpr size_t kPagesBetweenUnlocks = 16;
      if (unlikely(++pages_since_last_unlock == kPagesBetweenUnlocks)) {
        pages_since_last_unlock = 0;
        if (guard->lock()->IsContested()) {
          // Just drop the lock and re-acquire it. There is no need to yield.
          //
          // Since the lock is contested, the empty |CallUnlocked| will:
          // 1. Immediately grant the lock to another thread. This thread may
          //   continue running until #3, or it may be descheduled.
          // 2. Run the empty lambda.
          // 3. Attempt to re-acquire the lock. There are 3 possibilities:
          //   3a. Mutex is owned by the other thread, and is contested (there
          //       are more waiters besides the other thread). This thread will
          //       immediately block on the Mutex.
          //   3b. Mutex is owned by the other thread, and uncontested. This
          //       thread will spin on the Mutex, and block after some time.
          //   3c. Mutex is un-owned.  This thread will immediately own the
          //       Mutex again and continue running.
          //
          // Thus, there is no danger of thrashing here. The other thread will
          // always get the Mutex, even without an explicit yield.
          guard->CallUnlocked([]() {});
          status = LockDroppedTag{};
          break;
        }
      }
    }
    // Whenever the lock is dropped we need to re-check the properties before going back around
    // for a new cursor.
    if (status == StatusType(LockDroppedTag{})) {
      status = check_and_trim();
    }
    if (status != StatusType(ZX_OK)) {
      return ktl::get<zx_status_t>(status);
    }
  }

  return ZX_OK;
}

zx_status_t VmObjectPaged::Read(void* _ptr, uint64_t offset, size_t len) {
  canary_.Assert();
  // test to make sure this is a kernel pointer
  if (!is_kernel_address(reinterpret_cast<vaddr_t>(_ptr))) {
    DEBUG_ASSERT_MSG(0, "non kernel pointer passed\n");
    return ZX_ERR_INVALID_ARGS;
  }

  // read routine that just uses a memcpy
  char* ptr = reinterpret_cast<char*>(_ptr);
  auto read_routine = [ptr](const void* src, size_t offset,
                            size_t len) -> UserCopyCaptureFaultsResult {
    memcpy(ptr + offset, src, len);
    return UserCopyCaptureFaultsResult{ZX_OK};
  };

  if (can_block_on_page_requests()) {
    lockdep::AssertNoLocksHeld();
  }

  Guard<VmoLockType> guard{lock()};

  return ReadWriteInternalLocked(offset, len, false, VmObjectReadWriteOptions::None, read_routine,
                                 &guard);
}

zx_status_t VmObjectPaged::Write(const void* _ptr, uint64_t offset, size_t len) {
  canary_.Assert();
  // test to make sure this is a kernel pointer
  if (!is_kernel_address(reinterpret_cast<vaddr_t>(_ptr))) {
    DEBUG_ASSERT_MSG(0, "non kernel pointer passed\n");
    return ZX_ERR_INVALID_ARGS;
  }

  // write routine that just uses a memcpy
  const char* ptr = reinterpret_cast<const char*>(_ptr);
  auto write_routine = [ptr](void* dst, size_t offset, size_t len) -> UserCopyCaptureFaultsResult {
    memcpy(dst, ptr + offset, len);
    return UserCopyCaptureFaultsResult{ZX_OK};
  };

  if (can_block_on_page_requests()) {
    lockdep::AssertNoLocksHeld();
  }

  Guard<VmoLockType> guard{lock()};

  return ReadWriteInternalLocked(offset, len, true, VmObjectReadWriteOptions::None, write_routine,
                                 &guard);
}

zx_status_t VmObjectPaged::CacheOp(uint64_t offset, uint64_t len, CacheOpType type) {
  canary_.Assert();
  if (unlikely(len == 0)) {
    return ZX_ERR_INVALID_ARGS;
  }

  Guard<VmoLockType> guard{lock()};

  // verify that the range is within the object
  auto cow_range = GetCowRangeSizeCheckLocked(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // This cannot overflow as we already checked the range.
  const uint64_t cow_end = cow_range->end();

  // For syncing instruction caches there may be work that is more efficient to batch together, and
  // so we use an abstract consistency manager to optimize it for the given architecture.
  ArchVmICacheConsistencyManager sync_cm;

  return cow_pages_locked()->LookupReadableLocked(
      *cow_range,
      [&sync_cm, cow_offset = cow_range->offset, cow_end, type](uint64_t page_offset, paddr_t pa) {
        // This cannot overflow due to the maximum possible size of a VMO.
        const uint64_t page_end = page_offset + PAGE_SIZE;

        // Determine our start and end in terms of vmo offset
        const uint64_t start = ktl::max(page_offset, cow_offset);
        const uint64_t end = ktl::min(cow_end, page_end);

        // Translate to inter-page offset
        DEBUG_ASSERT(start >= page_offset);
        const uint64_t op_start_offset = start - page_offset;
        DEBUG_ASSERT(op_start_offset < PAGE_SIZE);

        DEBUG_ASSERT(end > start);
        const uint64_t op_len = end - start;

        CacheOpPhys(pa + op_start_offset, op_len, type, sync_cm);
        return ZX_ERR_NEXT;
      });
}

zx_status_t VmObjectPaged::Lookup(uint64_t offset, uint64_t len,
                                  VmObject::LookupFunction lookup_fn) {
  canary_.Assert();
  VmCowRange range(offset, len);
  auto cow_range = GetCowRange(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  Guard<VmoLockType> guard{lock()};

  return cow_pages_locked()->LookupLocked(
      *cow_range, [&lookup_fn, undo_offset = cow_range_.offset](uint64_t offset, paddr_t pa) {
        // Need to undo the parent_offset before forwarding to the lookup_fn, who is ignorant of
        // slices.
        return lookup_fn(offset - undo_offset, pa);
      });
}

zx_status_t VmObjectPaged::LookupContiguous(uint64_t offset, uint64_t len, paddr_t* out_paddr) {
  canary_.Assert();

  // We should consider having the callers round up to page boundaries and then check whether the
  // length is page-aligned.
  if (unlikely(len == 0 || !IS_PAGE_ALIGNED(offset))) {
    return ZX_ERR_INVALID_ARGS;
  }

  Guard<VmoLockType> guard{lock()};

  auto cow_range = GetCowRangeSizeCheckLocked(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  if (unlikely(!is_contiguous() && (cow_range->len != PAGE_SIZE))) {
    // Multi-page lookup only supported for contiguous VMOs.
    return ZX_ERR_BAD_STATE;
  }

  // Verify that all pages are present, and assert that the present pages are contiguous since we
  // only support len > PAGE_SIZE for contiguous VMOs.
  bool page_seen = false;
  uint64_t first_offset = 0;
  paddr_t first_paddr = 0;
  uint64_t count = 0;
  // This has to work for child slices with non-zero cow_range_.offset also, which means even if all
  // pages are present, the first cur_offset can be offset + cow_range_.offset.
  zx_status_t status = cow_pages_locked()->LookupLocked(
      *cow_range,
      [&page_seen, &first_offset, &first_paddr, &count](uint64_t cur_offset, paddr_t pa) mutable {
        ++count;
        if (!page_seen) {
          first_offset = cur_offset;
          first_paddr = pa;
          page_seen = true;
        }
        ASSERT(first_paddr + (cur_offset - first_offset) == pa);
        return ZX_ERR_NEXT;
      });
  ASSERT(status == ZX_OK);
  if (count != cow_range->len / PAGE_SIZE) {
    return ZX_ERR_NOT_FOUND;
  }
  if (out_paddr) {
    *out_paddr = first_paddr;
  }
  return ZX_OK;
}

zx_status_t VmObjectPaged::ReadUser(user_out_ptr<char> ptr, uint64_t offset, size_t len,
                                    VmObjectReadWriteOptions options, size_t* out_actual) {
  canary_.Assert();

  if (out_actual != nullptr) {
    *out_actual = 0;
  }

  // read routine that uses copy_to_user
  auto read_routine = [ptr, out_actual](const char* src, size_t offset,
                                        size_t len) -> UserCopyCaptureFaultsResult {
    __UNINITIALIZED auto copy_result =
        ptr.byte_offset(offset).copy_array_to_user_capture_faults(src, len);

    if (copy_result.status == ZX_OK && out_actual) {
      *out_actual += len;
    }
    return copy_result;
  };

  if (can_block_on_page_requests()) {
    lockdep::AssertNoLocksHeld();
  }

  Guard<VmoLockType> guard{lock()};

  return ReadWriteInternalLocked(offset, len, false, options, read_routine, &guard);
}

zx_status_t VmObjectPaged::WriteUser(user_in_ptr<const char> ptr, uint64_t offset, size_t len,
                                     VmObjectReadWriteOptions options, size_t* out_actual,
                                     const OnWriteBytesTransferredCallback& on_bytes_transferred) {
  canary_.Assert();

  if (out_actual != nullptr) {
    *out_actual = 0;
  }

  // write routine that uses copy_from_user
  auto write_routine = [ptr, base_vmo_offset = offset, out_actual, &on_bytes_transferred](
                           char* dst, size_t offset, size_t len) -> UserCopyCaptureFaultsResult {
    __UNINITIALIZED auto copy_result =
        ptr.byte_offset(offset).copy_array_from_user_capture_faults(dst, len);

    if (copy_result.status == ZX_OK) {
      if (out_actual != nullptr) {
        *out_actual += len;
      }

      if (on_bytes_transferred) {
        on_bytes_transferred(base_vmo_offset + offset, len);
      }
    }
    return copy_result;
  };

  if (can_block_on_page_requests()) {
    lockdep::AssertNoLocksHeld();
  }

  Guard<VmoLockType> guard{lock()};

  return ReadWriteInternalLocked(offset, len, true, options, write_routine, &guard);
}

zx_status_t VmObjectPaged::TakePages(uint64_t offset, uint64_t len, VmPageSpliceList* pages) {
  canary_.Assert();

  // TODO: Check that the region is locked once locking is implemented
  if (is_contiguous()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  auto cow_range = GetCowRange(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  auto range = *cow_range;

  // Initialize the splice list to the right size.
  *pages = VmPageSpliceList(range.offset, range.len, 0);

  __UNINITIALIZED MultiPageRequest page_request;
  while (!range.is_empty()) {
    uint64_t taken_len = 0;
    zx_status_t status = cow_pages_->TakePages(range, pages, &taken_len, &page_request);
    if (status != ZX_ERR_SHOULD_WAIT && status != ZX_OK) {
      return status;
    }
    // We would only have failed to take anything if status was not ZX_OK, which in this case
    // would be ZX_ERR_SHOULD_WAIT as that is the only non-OK status we can reach here with.
    DEBUG_ASSERT(taken_len > 0 || status == ZX_ERR_SHOULD_WAIT);
    // We should have taken the entire range requested if the status was ZX_OK.
    DEBUG_ASSERT(status != ZX_OK || taken_len == range.len);
    // We should not have taken any more than the requested range.
    DEBUG_ASSERT(taken_len <= range.len);

    // Record the completed portion.
    range = range.TrimedFromStart(taken_len);

    if (status == ZX_ERR_SHOULD_WAIT) {
      status = page_request.Wait();
      if (status != ZX_OK) {
        return status;
      }
    }
  }
  return ZX_OK;
}

zx_status_t VmObjectPaged::SupplyPages(uint64_t offset, uint64_t len, VmPageSpliceList* pages,
                                       SupplyOptions options) {
  canary_.Assert();

  // We need this check here instead of in SupplyPagesLocked, as we do use that
  // function to provide pages to contiguous VMOs as well.
  if (is_contiguous()) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  auto cow_range = GetCowRange(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  auto range = *cow_range;

  __UNINITIALIZED MultiPageRequest page_request;
  while (!range.is_empty()) {
    uint64_t supply_len = 0;
    zx_status_t status;
    {
      __UNINITIALIZED VmCowPages::DeferredOps deferred(cow_pages_.get());
      Guard<VmoLockType> guard{lock()};
      status = cow_pages_locked()->SupplyPagesLocked(range, pages, options, &supply_len, deferred,
                                                     &page_request);
    }
    if (status != ZX_ERR_SHOULD_WAIT && status != ZX_OK) {
      return status;
    }
    // We would only have failed to supply anything if status was not ZX_OK, which in this case
    // would be ZX_ERR_SHOULD_WAIT as that is the only non-OK status we can reach here with.
    DEBUG_ASSERT(supply_len > 0 || status == ZX_ERR_SHOULD_WAIT);
    // We should have supplied the entire range requested if the status was ZX_OK.
    DEBUG_ASSERT(status != ZX_OK || supply_len == range.len);
    // We should not have supplied any more than the requested range.
    DEBUG_ASSERT(supply_len <= range.len);

    // Record the completed portion.
    range = range.TrimedFromStart(supply_len);

    if (status == ZX_ERR_SHOULD_WAIT) {
      status = page_request.Wait();
      if (status != ZX_OK) {
        return status;
      }
    }
  }
  return ZX_OK;
}

zx_status_t VmObjectPaged::DirtyPages(uint64_t offset, uint64_t len) {
  // It is possible to encounter delayed PMM allocations, which requires waiting on the
  // page_request.
  __UNINITIALIZED AnonymousPageRequest page_request;

  auto cow_range = GetCowRange(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // Initialize a list of allocated pages that DirtyPages will allocate any new pages into
  // before inserting them in the VMO. Allocated pages can therefore be shared across multiple calls
  // to DirtyPages. Instead of having to allocate and free pages in case DirtyPages
  // cannot successfully dirty the entire range atomically, we can just hold on to the allocated
  // pages and use them for the next call. This ensures that we are making forward progress with
  // each successive call to DirtyPages.
  list_node alloc_list;
  list_initialize(&alloc_list);
  auto alloc_list_cleanup = fit::defer([&alloc_list, this]() -> void {
    if (!list_is_empty(&alloc_list)) {
      cow_pages_->FreePages(&alloc_list);
    }
  });
  while (true) {
    zx_status_t status = cow_pages_->DirtyPages(*cow_range, &alloc_list, &page_request);
    if (status == ZX_OK) {
      return ZX_OK;
    }
    if (status == ZX_ERR_SHOULD_WAIT) {
      status = page_request.Wait();
    }
    if (status != ZX_OK) {
      return status;
    }
    // If the wait was successful, loop around and try the call again, which will re-validate any
    // state that might have changed when the lock was dropped.
  }
}

zx_status_t VmObjectPaged::EnumerateDirtyRanges(uint64_t offset, uint64_t len,
                                                DirtyRangeEnumerateFunction&& dirty_range_fn) {
  Guard<VmoLockType> guard{lock()};
  if (auto cow_range = GetCowRange(offset, len)) {
    // Need to wrap the callback to translate the cow pages offsets back into offsets as seen by
    // this object.
    return cow_pages_locked()->EnumerateDirtyRangesLocked(
        *cow_range, [&dirty_range_fn, undo_offset = cow_range_.offset](
                        uint64_t range_offset, uint64_t range_len, bool range_is_zero) {
          return dirty_range_fn(range_offset - undo_offset, range_len, range_is_zero);
        });
  }
  return ZX_ERR_OUT_OF_RANGE;
}

zx_status_t VmObjectPaged::SetMappingCachePolicy(const uint32_t cache_policy) {
  // Is it a valid cache flag?
  if (cache_policy & ~ZX_CACHE_POLICY_MASK) {
    return ZX_ERR_INVALID_ARGS;
  }

  Guard<VmoLockType> guard{lock()};

  // conditions for allowing the cache policy to be set:
  // 1) vmo either has no pages committed currently or is transitioning from being cached
  // 2) vmo has no pinned pages
  // 3) vmo has no mappings
  // 4) vmo has no children
  // 5) vmo is not a child
  // Counting attributed memory does a sufficient job of checking for committed pages since we also
  // require no children and no parent, so attribution == precisely our pages.
  if (cow_pages_locked()->GetAttributedMemoryInRangeLocked(VmCowRange(0, size_locked())) !=
          AttributionCounts{} &&
      cache_policy_ != ARCH_MMU_FLAG_CACHED) {
    // We forbid to transitioning committed pages from any kind of uncached->cached policy as we do
    // not currently have a story for dealing with the speculative loads that may have happened
    // against the cached physmap. That is, whilst a page was uncached the cached physmap version
    // may have been loaded and sitting in cache. If we switch to cached mappings we may then use
    // stale data out of the cache.
    // This isn't a problem if going *from* an cached state, as we can safely clean+invalidate.
    // Similarly it's not a problem if there aren't actually any committed pages.
    return ZX_ERR_BAD_STATE;
  }
  if (cow_pages_locked()->pinned_page_count_locked() > 0) {
    return ZX_ERR_BAD_STATE;
  }

  if (self_locked()->num_mappings_locked() != 0) {
    return ZX_ERR_BAD_STATE;
  }

  // The ChildListLock needs to be held to inspect the children/parent pointers, however we do not
  // need to hold it over the remainder of this method as the main VMO lock is held, and creating a
  // new child happens under that lock as well since the creation path must, in a single lock
  // acquisition, be checking the cache_policy_ and creating the child.
  {
    Guard<CriticalMutex> child_guard{ChildListLock::Get()};

    if (!children_list_.is_empty()) {
      return ZX_ERR_BAD_STATE;
    }
    if (parent_) {
      return ZX_ERR_BAD_STATE;
    }
  }

  // Forbid if there are references, or if this object is a reference itself. We do not want cache
  // policies to diverge across references. Note that this check is required in addition to the
  // children_list_ and parent_ check, because it is possible for a non-reference parent to go away,
  // which will trigger the election of a reference as the new owner for the remaining
  // reference_list_, and also reset the parent_.
  if (!reference_list_.is_empty()) {
    return ZX_ERR_BAD_STATE;
  }
  if (is_reference()) {
    return ZX_ERR_BAD_STATE;
  }

  // If transitioning from a cached policy we must clean/invalidate all the pages as the kernel may
  // have written to them on behalf of the user.
  if (cache_policy_ == ARCH_MMU_FLAG_CACHED && cache_policy != ARCH_MMU_FLAG_CACHED) {
    // No need to perform clean/invalidate if size is zero because there can be no pages.
    if (size_locked() > 0) {
      VmCowRange range(0, size_locked());
      zx_status_t status =
          cow_pages_locked()->LookupLocked(range, [](uint64_t offset, paddr_t pa) mutable {
            arch_clean_invalidate_cache_range((vaddr_t)paddr_to_physmap(pa), PAGE_SIZE);
            return ZX_ERR_NEXT;
          });
      if (status != ZX_OK) {
        return status;
      }
    }
  }

  cache_policy_ = cache_policy;

  return ZX_OK;
}

void VmObjectPaged::RangeChangeUpdateLocked(VmCowRange range, RangeChangeOp op) {
  canary_.Assert();

  // offsets for vmos needn't be aligned, but vmars use aligned offsets
  uint64_t aligned_offset = ROUNDDOWN(range.offset, PAGE_SIZE);
  uint64_t aligned_len = ROUNDUP(range.end(), PAGE_SIZE) - aligned_offset;
  if (GetIntersect(cow_range_.offset, cow_range_.len, aligned_offset, aligned_len, &aligned_offset,
                   &aligned_len)) {
    // Found the intersection in cow space, convert back to object space.
    aligned_offset -= cow_range_.offset;
    self_locked()->RangeChangeUpdateMappingsLocked(aligned_offset, aligned_len, op);
  }

  // Propagate the change to reference children as well. This is done regardless of intersection as
  // we may have become the holder of the reference list even if they were not originally references
  // made against us, and so their cow views might be different.
  for (auto& ref : reference_list_) {
    AssertHeld(ref.lock_ref());
    // Use the same offset and len. References span the entirety of the parent VMO and hence share
    // all offsets.
    ref.RangeChangeUpdateLocked(range, op);
  }
}

void VmObjectPaged::ForwardRangeChangeUpdateLocked(uint64_t offset, uint64_t len,
                                                   RangeChangeOp op) {
  canary_.Assert();

  // Call RangeChangeUpdateLocked on the owner of the CowPages.
  AssertHeld(cow_pages_locked()->get_paged_backlink_locked()->lock_ref());
  if (auto cow_range = GetCowRange(offset, len)) {
    cow_pages_locked()->get_paged_backlink_locked()->RangeChangeUpdateLocked(*cow_range, op);
  }
}

zx_status_t VmObjectPaged::LockRange(uint64_t offset, uint64_t len,
                                     zx_vmo_lock_state_t* lock_state_out) {
  if (!is_discardable()) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  auto cow_range = GetCowRange(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  Guard<VmoLockType> guard{lock()};
  return cow_pages_locked()->LockRangeLocked(*cow_range, lock_state_out);
}

zx_status_t VmObjectPaged::TryLockRange(uint64_t offset, uint64_t len) {
  if (!is_discardable()) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  auto cow_range = GetCowRange(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  Guard<VmoLockType> guard{lock()};
  return cow_pages_locked()->TryLockRangeLocked(*cow_range);
}

zx_status_t VmObjectPaged::UnlockRange(uint64_t offset, uint64_t len) {
  if (!is_discardable()) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  auto cow_range = GetCowRange(offset, len);
  if (!cow_range) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  Guard<VmoLockType> guard{lock()};
  return cow_pages_locked()->UnlockRangeLocked(*cow_range);
}

zx_status_t VmObjectPaged::GetPage(uint64_t offset, uint pf_flags, list_node* alloc_list,
                                   MultiPageRequest* page_request, vm_page_t** page, paddr_t* pa) {
  Guard<VmoLockType> guard{lock()};
  const bool write = pf_flags & VMM_PF_FLAG_WRITE;
  zx::result<VmCowPages::LookupCursor> cursor = GetLookupCursorLocked(offset, PAGE_SIZE);
  if (cursor.is_error()) {
    return cursor.error_value();
  }
  AssertHeld(cursor->lock_ref());
  // Hardware faults are considered to update access times separately, all other lookup reasons
  // should do the default update of access time.
  if (pf_flags & VMM_PF_FLAG_HW_FAULT) {
    cursor->DisableMarkAccessed();
  }
  if (!(pf_flags & VMM_PF_FLAG_FAULT_MASK)) {
    vm_page_t* p = cursor->MaybePage(write);
    if (!p) {
      return ZX_ERR_NOT_FOUND;
    }
    if (page) {
      *page = p;
    }
    if (pa) {
      *pa = p->paddr();
    }
    return ZX_OK;
  }
  auto result = cursor->RequirePage(write, PAGE_SIZE, page_request);
  if (result.is_error()) {
    return result.error_value();
  }
  if (page) {
    *page = result->page;
  }
  if (pa) {
    *pa = result->page->paddr();
  }
  return ZX_OK;
}
