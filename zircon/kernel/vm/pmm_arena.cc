// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include "vm/pmm_arena.h"

#include <align.h>
#include <inttypes.h>
#include <lib/counters.h>
#include <lib/zx/result.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <kernel/range_check.h>
#include <ktl/limits.h>
#include <pretty/cpp/sizes.h>
#include <vm/physmap.h>
#include <vm/pmm_node.h>

#include "vm_priv.h"

#include <ktl/enforce.h>

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

// A possibly "lossy" estimate of the maximum number of page runs examined while performing a
// contiguous allocation.  See the comment where this counter is updated.
KCOUNTER_DECLARE(counter_max_runs_examined, "vm.pmm.max_runs_examined", Max)

void PmmArena::Init(const PmmArenaSelection& selected, PmmNode* node) {
  DEBUG_ASSERT(IS_PAGE_ALIGNED(selected.arena.base));
  DEBUG_ASSERT(IS_PAGE_ALIGNED(selected.arena.size));
  DEBUG_ASSERT(IS_PAGE_ALIGNED(selected.bookkeeping.base));
  DEBUG_ASSERT(IS_PAGE_ALIGNED(selected.bookkeeping.size));

  size_t page_count = selected.arena.size / PAGE_SIZE;
  DEBUG_ASSERT(selected.bookkeeping.size == ROUNDUP_PAGE_SIZE(page_count * sizeof(vm_page)));
  DEBUG_ASSERT(selected.bookkeeping.size < selected.arena.size);

  dprintf(INFO, "PMM: adding arena [%#" PRIx64 ", %#" PRIx64 ")\n", selected.arena.base,
          selected.arena.end());

  // Intentionally similar to the logging in PmmNode::InitReservedRange().
  dprintf(INFO, "PMM: reserved [%#" PRIx64 ", %#" PRIx64 "): bookkeeping\n",
          selected.bookkeeping.base, selected.bookkeeping.end());

  info_ = pmm_arena_info_t{
      .flags = 0,
      .base = selected.arena.base,
      .size = selected.arena.size,
  };
  snprintf(info_.name, sizeof(info_.name), "%s", "ram");

  // get the kernel pointer
  size_t page_array_size = selected.bookkeeping.size;
  void* raw_page_array = paddr_to_physmap(selected.bookkeeping.base);
  LTRACEF("arena for base 0%#" PRIxPTR " size %#zx page array at %p size %#zx\n", base(), size(),
          raw_page_array, page_array_size);
  memset(raw_page_array, 0, page_array_size);
  page_array_ = (vm_page_t*)raw_page_array;

  // we've just constructed |page_count| pages in the state vm_page_state::FREE
  vm_page::add_to_initial_count(vm_page_state::FREE, page_count);

  // compute the range of the array that backs the array itself
  size_t array_start_index = (selected.bookkeeping.base - info_.base) / PAGE_SIZE;
  size_t array_end_index = array_start_index + page_array_size / PAGE_SIZE;
  LTRACEF("array_start_index %zu, array_end_index %zu, page_count %zu\n", array_start_index,
          array_end_index, page_count);

  DEBUG_ASSERT(array_start_index < page_count && array_end_index <= page_count);

  // add all pages that aren't part of the page array to the free list
  // pages part of the free array go to the WIRED state
  list_node list;
  list_initialize(&list);
  for (size_t i = 0; i < page_count; i++) {
    auto& p = page_array_[i];

    p.paddr_priv = base() + i * PAGE_SIZE;
    if (i >= array_start_index && i < array_end_index) {
      p.set_state(vm_page_state::WIRED);
    } else {
      list_add_tail(&list, &p.queue_node);
    }
  }

  node->AddFreePages(&list);
}

void PmmArena::InitForTest(const pmm_arena_info_t& info, vm_page_t* page_array) {
  info_ = info;
  page_array_ = page_array;
}

vm_page_t* PmmArena::FindSpecific(paddr_t pa) {
  if (!address_in_arena(pa)) {
    return nullptr;
  }

  size_t index = (pa - base()) / PAGE_SIZE;

  DEBUG_ASSERT(index < size() / PAGE_SIZE);

  return get_page(index);
}

// Computes and returns the offset from |page_array_| of the first element at or
// after |offset| whose physical address alignment satisfies |alignment_log2|.
//
// Note, the returned value may exceed the bounds of |page_array_|.
static uint64_t Align(uint64_t offset, uint8_t alignment_log2, uint64_t first_aligned_offset) {
  if (offset < first_aligned_offset) {
    return first_aligned_offset;
  }
  DEBUG_ASSERT(alignment_log2 >= PAGE_SIZE_SHIFT);
  // The "extra" alignment required above and beyond PAGE_SIZE alignment.
  const uint64_t offset_alignment = alignment_log2 - PAGE_SIZE_SHIFT;
  return ROUNDUP(offset - first_aligned_offset, 1UL << (offset_alignment)) + first_aligned_offset;
}

zx::result<uint64_t> PmmArena::FindLastNonFree(uint64_t offset, size_t count) const {
  uint64_t i = offset + count - 1;
  do {
    if (!page_array_[i].is_free()) {
      return zx::ok(i);
    }
  } while (i-- > offset);

  return zx::error(ZX_ERR_NOT_FOUND);
}

vm_page_t* PmmArena::FindFreeContiguous(size_t count, uint8_t alignment_log2) {
  DEBUG_ASSERT(count > 0);

  if (alignment_log2 < PAGE_SIZE_SHIFT) {
    alignment_log2 = PAGE_SIZE_SHIFT;
  }

  // Number of pages in this arena.
  const uint64_t arena_count = size() / PAGE_SIZE;
  // Offset of the first page that satisfies the required alignment.
  const uint64_t first_aligned_offset =
      (ROUNDUP(base(), 1UL << alignment_log2) - base()) / PAGE_SIZE;
  // Start our search at the hint so that we can skip over regions previously
  // known to be in use.
  const uint64_t initial = search_hint_;
  DEBUG_ASSERT_MSG(initial < arena_count, "initial %lu\n", initial);
  uint64_t candidate = Align(initial, alignment_log2, first_aligned_offset);
  // Keep track of how many runs of pages we examine before finding a
  // sufficiently long contiguous run.
  int64_t num_runs_examined = 0;
  // Indicates whether we have wrapped around back to the start of the arena.
  bool wrapped = false;
  vm_page_t* result = nullptr;

  // Keep searching until we've wrapped and "lapped" our initial starting point.
  while (!wrapped || candidate < initial) {
    LTRACEF(
        "num_runs_examined=%ld candidate=%lu count=%zu alignment_log2=%d arena_count=%lu "
        "initial=%lu\n",
        num_runs_examined, candidate, count, alignment_log2, arena_count, initial);
    num_runs_examined++;
    if (!InRange(candidate, count, arena_count)) {
      if (wrapped) {
        break;
      }
      wrapped = true;
      candidate = first_aligned_offset;
    } else {
      // Is the candidate region free?  Walk the pages of the region back to
      // front, stopping at the first non-free page.

      zx::result<uint64_t> last_non_free = FindLastNonFree(candidate, count);
      if (last_non_free.is_error()) {
        // Candidate region is free.  We're done.
        search_hint_ = (candidate + count) % arena_count;
        result = &page_array_[candidate];
        DEBUG_ASSERT_MSG(candidate < arena_count, "candidate=%lu arena_count=%lu\n", candidate,
                         arena_count);
        break;
      }

      // Candidate region is not completely free.  Skip over the "broken" run,
      // maintaining alignment.
      candidate = Align(last_non_free.value() + 1, alignment_log2, first_aligned_offset);
    }
  }

  // If called with preemption enabled, then the counter may fail to observe the true max.
  int64_t max = counter_max_runs_examined.ValueCurrCpu();
  if (num_runs_examined > max) {
    counter_max_runs_examined.Set(num_runs_examined);
  }

  return result;
}

void PmmArena::CountStates(PmmStateCount* state_count) const {
  for (size_t i = 0; i < size() / PAGE_SIZE; i++) {
    (*state_count)[VmPageStateIndex(page_array_[i].state())]++;
  }
}

void PmmArena::Dump(bool dump_pages, bool dump_free_ranges, PmmStateCount* counts_sum) const {
  printf("  arena %p: name '%s' base %#" PRIxPTR " size %s (0x%zx) flags 0x%x\n", this, name(),
         base(), pretty::FormattedBytes(size()).c_str(), size(), flags());
  printf("\tpage_array %p search_hint %lu\n", page_array_, search_hint_);

  // dump all of the pages
  if (dump_pages) {
    for (size_t i = 0; i < size() / PAGE_SIZE; i++) {
      page_array_[i].dump();
    }
  }

  // count the number of pages in every state
  PmmStateCount state_count = {};
  CountStates(&state_count);

  for (size_t ix = 0; ix < counts_sum->size(); ++ix) {
    (*counts_sum)[ix] += state_count[ix];
  }

  PrintPageStateCounts(state_count);

  // dump the free pages
  if (dump_free_ranges) {
    printf("\tfree ranges:\n");
    ssize_t last = -1;
    for (size_t i = 0; i < size() / PAGE_SIZE; i++) {
      if (page_array_[i].is_free()) {
        if (last == -1) {
          last = i;
        }
      } else {
        if (last != -1) {
          printf("\t\t%#" PRIxPTR " - %#" PRIxPTR "\n", base() + last * PAGE_SIZE,
                 base() + i * PAGE_SIZE);
        }
        last = -1;
      }
    }

    if (last != -1) {
      printf("\t\t%#" PRIxPTR " - %#" PRIxPTR "\n", base() + last * PAGE_SIZE, base() + size());
    }
  }
}

void PrintPageStateCounts(const PmmStateCount& state_count) {
  printf("\tpage states:\n");
  for (unsigned int i = 0; i < VmPageStateIndex(vm_page_state::COUNT_); i++) {
    printf("\t\t%-12s %-16zu (%zu bytes)\n", page_state_to_string(vm_page_state(i)), state_count[i],
           state_count[i] * PAGE_SIZE);
  }
}
