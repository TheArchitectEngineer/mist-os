// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_

#ifndef __ASSEMBLER__

#include <stdbool.h>
#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/cpu.h>

struct iframe_t;

struct arm64_cache_desc {
  uint8_t ctype;
  uint32_t num_sets;
  uint32_t associativity;
  uint32_t line_size;
};

struct arm64_cache_info {
  // from CLIDR_EL1
  uint8_t inner_boundary;
  uint8_t lou_u;
  uint8_t loc;
  uint8_t lou_is;
  // from CTR_EL0
  uint8_t imin_line;
  uint8_t dmin_line;
  uint8_t cache_writeback_granule;
  uint8_t l1_instruction_cache_policy;
  bool idc;  // requires icache invalidate to pou for instruction to data coherence
  bool dic;  // requires data clean to pou for data to instruction coherence
  // via iterating each cache level
  arm64_cache_desc level_data_type[7];
  arm64_cache_desc level_inst_type[7];
};

struct arch_exception_context {
  struct iframe_t* frame;
  uint64_t far;
  uint32_t esr;
  // The |user_synth_code| and |user_synth_data| fields have different values depending on the
  // exception type.
  //
  // 1) For ZX_EXCP_POLICY_ERROR, |user_synth_code| contains the type of the policy error (a
  // ZX_EXCP_POLICY_CODE_* value), and |user_synth_data| contains additional information relevant to
  // the policy error (e.g. the syscall number for ZX_EXCP_POLICY_CODE_BAD_SYSCALL).
  //
  // 2) For ZX_EXCP_FATAL_PAGE_FAULT, |user_synth_code| contains the |zx_status_t| error code
  // returned by the page fault handler, typecast to |uint32_t|. |user_synth_data| is 0.
  //
  // 3) For all other exception types, |user_synth_code| and |user_synth_data| are both set to 0.
  uint32_t user_synth_code;
  uint32_t user_synth_data;
};

// Register state layout used by arm64_context_switch().
struct arm64_context_switch_frame {
  uint64_t r19;
  uint64_t zero;  // slot where x20 (percpu pointer) would be saved if it were
  uint64_t r21;
  uint64_t r22;
  uint64_t r23;
  uint64_t r24;
  uint64_t r25;
  uint64_t r26;
  uint64_t r27;
  uint64_t r28;
  uint64_t r29;
  uint64_t lr;
};

struct Thread;

// Implemented in or called from assembly.
extern "C" {
#if __has_feature(shadow_call_stack)
void arm64_context_switch(vaddr_t* old_sp, vaddr_t new_sp, vaddr_t new_tpidr, uintptr_t** old_scsp,
                          uintptr_t* new_scsp);
void arm64_uspace_entry(iframe_t* iframe, vaddr_t kstack, vaddr_t scsp) __NO_RETURN;
#else
void arm64_context_switch(vaddr_t* old_sp, vaddr_t new_sp, vaddr_t new_tpidr);
void arm64_uspace_entry(iframe_t* iframe, vaddr_t kstack) __NO_RETURN;
#endif

// These are not actually functions, but special code addresses.
using VbarFunction = void();
VbarFunction arm64_el1_exception;
VbarFunction arm64_el1_exception_smccc11_workaround;
VbarFunction arm64_el1_exception_smccc10_workaround;

void arm64_sync_exception(iframe_t* iframe, uint exception_flags, uint32_t esr);

void platform_irq(iframe_t* frame);
}  // extern C

arm64_context_switch_frame* arm64_get_context_switch_frame(Thread* thread);

/* fpu routines */
void arm64_fpu_exception(iframe_t* iframe, uint exception_flags);
void arm64_fpu_context_switch(Thread* oldthread, Thread* newthread);
void arm64_fpu_save_state(Thread* t);
void arm64_fpu_restore_state(Thread* t);

// TODO(https://fxbug.dev/393619961): Identically 1 today, but should one day
// be dynamic.
constexpr uint64_t arm64_get_boot_el() { return 1; }

// Called during clock selection (if it is called at all) before secondary CPUs
// have started.
void arm64_allow_pct_in_el0();

/*
 * Creates a stack and sets the stack pointer for the specified secondary CPU.
 */
zx_status_t arm64_create_secondary_stack(cpu_num_t cpu_num, uint64_t mpid);

/*
 * Frees a stack created by |arm64_create_secondary_stack|.
 */
zx_status_t arm64_free_secondary_stack(cpu_num_t cpu_num);

#endif  // __ASSEMBLER__

/* used in above exception_flags arguments */
#define ARM64_EXCEPTION_FLAG_LOWER_EL (1 << 0)

/* used in the exceptions_c which argument */
#define ARM64_DISALLOWED_ARM32_SYSCALL (1 << 0)
#define ARM64_DISALLOWED_ARM32_SYNC_EXCEPTION (1 << 1)
#define ARM64_DISALLOWED_ARM32_ASYNC_EXCEPTION (1 << 2)

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_
