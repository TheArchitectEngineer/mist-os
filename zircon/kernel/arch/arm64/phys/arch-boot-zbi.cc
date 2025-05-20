// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/cache.h>

#include <phys/boot-zbi.h>

void BootZbi::ZbiBoot(uintptr_t entry, void* data) const {
  arch::DisableLocalCachesAndMmu();
  // Clear the stack and frame pointers and the link register so no misleading
  // breadcrumbs are left.
  __asm__ volatile(
      R"""(
      mov x0, %[zbi]
      mov x29, xzr
      mov x30, xzr
      mov sp, x29
      br %[entry]
      )"""
      :
      : [entry] "r"(entry), [zbi] "r"(data)
      // The compiler gets unhappy if x29 (fp) is a clobber.  It's never going
      // to be the register used for %[entry] anyway.  The memory clobber is
      // probably unnecessary, but it expresses that this constitutes access to
      // the memory kernel and zbi point to.
      : "x0", "x30", "memory");
  __builtin_unreachable();
}
