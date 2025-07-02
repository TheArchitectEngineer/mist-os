// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_LIB_KTRACE_H_
#define ZIRCON_KERNEL_INCLUDE_LIB_KTRACE_H_

#include <align.h>
#include <lib/fxt/interned_category.h>
#include <lib/fxt/interned_string.h>
#include <lib/fxt/record_types.h>
#include <lib/fxt/serializer.h>
#include <lib/fxt/string_ref.h>
#include <lib/fxt/trace_base.h>
#include <lib/spsc_buffer/spsc_buffer.h>
#include <lib/user_copy/user_ptr.h>
#include <lib/zircon-internal/ktrace.h>
#include <platform.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/mutex.h>
#include <kernel/thread.h>
#include <ktl/atomic.h>
#include <ktl/tuple.h>

//
// # Kernel tracing instrumentation and state management interfaces
//
// The general tracing API is comprised of macros appropriate for most use cases. Direct access to
// the lower level interfaces is available for specialized use cases.
//

//
// # General kernel tracing API
//
// The following macros comprise the general kernel tracing interface. The interface supports most
// FXT record types and provides options for compile-time and runtime predicates to manage the
// presence of instrumentation in different builds.
//

//
// ## Variable arguments
//
// Most FXT records support a variable number of arguments as string/value pairs. The macros in this
// interface expect variable arguments to be parenthesized tuples of the form:
//
//   KTRACE_*(<required args>, ("literal", value, ...), ("literal", value, ...), ...);
//
// These tuples are transformed into instances of fxt::Argument as follows:
//
//   ("key", 10)        -> fxt::MakeArgument("key"_intern, 10)
//   ("key", buf, size) -> fxt::MakeArgument("key"_intern, buf, size)
//
// ## Argument evaluation
//
// Conditions and arguments to KTRACE_* macros are evaluated in the following order and scope. When
// a condition is not exposed by a particular macro it is hard-coded to true unless otherwise
// specified.
//
// 1. constexpr_enabled: Evaluated once with all remaining argument and condition evaluations
//    contained in the true branch.
// 2. runtime_condition: Evaluated once with all remaining argument and condition evaluations
//    contained in the true branch.
// 3. Category predicate: Evaluated once with all remaining argument evaluations contained in the
//    true branch.
// 4. Required and variable arguments: Evaluated as function arguments using standard C++
//    implementation defined evaluation order when all preceding conditions are met. Argument
//    expressions do not need to be globally idempotent (e.g. they can generate ids), however,
//    inter-argument order dependencies must be handled with the same care as in any ordinary
//    function invocation.
//

//
// ## KERNEL_OBJECT
//
// Supports exporting information about kernel objects. For example, the names of threads and
// processes.
//

// Writes a kernel object record when the given trace category is enabled.
//
// Arguments:
// - category: String literal filter category for the object record.
// - koid: Kernel object id of the object. Expects type zx_koid_t.
// - obj_type: The type the object. Expects type zx_obj_type_t.
// - name: The name of the object. Must be convertible to fxt::StringRef.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_KERNEL_OBJECT(category, koid, obj_type, name, ...)                            \
  FXT_KERNEL_OBJECT(true, KTrace::CategoryEnabled, KTrace::EmitKernelObject, category, koid, \
                    obj_type, name, ##__VA_ARGS__)

// Writes a kernel object record unconditionally. Useful for generating the initial set of object
// info records before tracing is enabled.
//
// Arguments:
// - koid: Kernel object id of the object. Expects type zx_koid_t.
// - obj_type: The type the object. Expects type zx_obj_type_t.
// - name: The name of the object. Must be convertible to fxt::StringRef.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_KERNEL_OBJECT_ALWAYS(koid, obj_type, name, ...) \
  FXT_KERNEL_OBJECT_ALWAYS(true, KTrace::EmitKernelObject, koid, obj_type, name, ##__VA_ARGS__)

//
// ## SCOPE
//
// Supports tracing the duration of lexical scopes, such as functions, conditional bodies, and
// nested scopes using duration complete events.
//
// The duration of a scope is determined by the lifetime of the RAII helper type ktrace::Scope. The
// category, label, and arguments for the complete event are captured by delegates created by
// KTRACE_BEGIN_SCOPE* and KTRACE_END_SCOPE* macros.
//
// Examples:
//
//  // Basic scope with arguments.
//  void Function(int value) {
//    ktrace::Scope trace = KTRACE_BEGIN_SCOPE("category", ("value", value));
//    // ...
//  }
//
//  // Early completion of the scope.
//  void Function(int value) {
//    ktrace::Scope trace = KTRACE_BEGIN_SCOPE("category", ("value", value));
//    // ...
//    scope.End();
//    // ...
//  }
//
//  // Completion of the scope with additional arguments.
//  void Function(int value) {
//    ktrace::Scope trace = KTRACE_BEGIN_SCOPE("category", ("value", value));
//    const int result = GetResult();
//    scope = KTRACE_END_SCOPE(("result", result));
//  }
//
//  // Runtime predicate to enable the scope only in certain conditions.
//  void Function(int value) {
//    ktrace::Scope trace = KTRACE_BEGIN_SCOPE_COND(Predicate(value), "category", ("value", value));
//    const int result = GetResult();
//    scope = KTRACE_END_SCOPE(("result", result));
//  }
//

// Creates a delegate to capture the given arguments at the beginning of a scope when the given
// category is enabled. The returned value should be used to construct a ktrace::Scope to track the
// lifetime of the scope and emit the complete trace event. The complete event is associated with
// the current thread.
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_BEGIN_SCOPE(category, label, ...)                                                \
  FXT_BEGIN_SCOPE(true, true, KTrace::CategoryEnabled, KTrace::Timestamp, KTrace::EmitComplete, \
                  category, FXT_INTERN_STRING(label), KTrace::Context::Thread, ##__VA_ARGS__)

// Similar to KTRACE_BEGIN_SCOPE, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_BEGIN_SCOPE(category, label, ...)                                            \
  FXT_BEGIN_SCOPE(true, true, KTrace::CategoryEnabled, KTrace::Timestamp, KTrace::EmitComplete, \
                  category, FXT_INTERN_STRING(label), KTrace::Context::Cpu, ##__VA_ARGS__)

// Similar to KTRACE_BEGIN_SCOPE, but checks the given runtime_condition, in addition to the given
// category, to determine whether to emit the event.
#define KTRACE_BEGIN_SCOPE_COND(runtime_condition, category, label, ...)               \
  FXT_BEGIN_SCOPE(true, runtime_condition, KTrace::CategoryEnabled, KTrace::Timestamp, \
                  KTrace::EmitComplete, category, FXT_INTERN_STRING(label),            \
                  KTrace::Context::Thread, ##__VA_ARGS__)

// Similar to KTRACE_BEGIN_SCOPE_COND, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_BEGIN_SCOPE_COND(runtime_condition, category, label, ...)                      \
  FXT_BEGIN_SCOPE(true, runtime_condition, KTrace::CategoryEnabled, KTrace::Timestamp,            \
                  KTrace::EmitComplete, category, FXT_INTERN_STRING(label), KTrace::Context::Cpu, \
                  ##__VA_ARGS__)

// Similar to KTRACE_BEGIN_SCOPE, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time.
#define KTRACE_BEGIN_SCOPE_ENABLE(constexpr_enabled, category, label, ...)             \
  FXT_BEGIN_SCOPE(constexpr_enabled, true, KTrace::CategoryEnabled, KTrace::Timestamp, \
                  KTrace::EmitComplete, category, FXT_INTERN_STRING(label),            \
                  KTrace::Context::Thread, ##__VA_ARGS__)

// Similar to KTRACE_BEGIN_SCOPE_ENABLE, but associates the event with the current CPU instead of
// the current thread.
#define KTRACE_CPU_BEGIN_SCOPE_ENABLE(constexpr_enabled, category, label, ...)                    \
  FXT_BEGIN_SCOPE(constexpr_enabled, true, KTrace::CategoryEnabled, KTrace::Timestamp,            \
                  KTrace::EmitComplete, category, FXT_INTERN_STRING(label), KTrace::Context::Cpu, \
                  ##__VA_ARGS__)

// Similar to KTRACE_BEGIN_SCOPE, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time, and checks the given runtime_condition, in addition to the
// given category, to determine whether to emit the event.
#define KTRACE_BEGIN_SCOPE_ENABLE_COND(constexpr_enabled, runtime_enabled, category, label, ...)  \
  FXT_BEGIN_SCOPE(constexpr_enabled, runtime_enabled, KTrace::CategoryEnabled, KTrace::Timestamp, \
                  KTrace::EmitComplete, category, FXT_INTERN_STRING(label),                       \
                  KTrace::Context::Thread, ##__VA_ARGS__)

// Similar to KTRACE_BEGIN_SCOPE_ENABLE_COND, but associates the event with the current CPU instead
// of the current thread.
#define KTRACE_CPU_BEGIN_SCOPE_ENABLE_COND(constexpr_enabled, runtime_enabled, category, label,   \
                                           ...)                                                   \
  FXT_BEGIN_SCOPE(constexpr_enabled, runtime_enabled, KTrace::CategoryEnabled, KTrace::Timestamp, \
                  KTrace::EmitComplete, category, FXT_INTERN_STRING(label), KTrace::Context::Cpu, \
                  ##__VA_ARGS__)

// Creates a delegate to capture the given arguments at the end of an active scope. The returned
// value should be assigned to the scope to complete with additional arguments.
#define KTRACE_END_SCOPE(...) FXT_END_SCOPE(__VA_ARGS__)

// Utility to access values with Clang static analysis annotations in a scoped argument list.

// Due to the way arguments are captured by lambdas when using KTRACE_BEGIN_SCOPE and
// KTRACE_END_SCOPE, it is necessary to hoist static assertions or lock guards into the argument
// capture expressions using KTRACE_ANNOTATED_VALUE.
//
// Examples:
//
// - Lock is already held and it is safe to read the guarded value:
//
//  ktrace::Scope scope =
//      KTRACE_BEGIN_SCOPE("category", "label",
//          ("guarded_value", KTRACE_ANNOTATED_VALUE(AssertHeld(lock), guarded_value)));
//
// - Lock is not held and must be acquired to read the guarded value:
//
//  ktrace::Scope scope =
//      KTRACE_BEGIN_SCOPE("category", "label",
//          ("guarded_value", KTRACE_ANNOTATED_VALUE(Guard guard{lock}, guarded_value)));
//
#define KTRACE_ANNOTATED_VALUE(acquire_or_assert_expression, value_expression) \
  FXT_ANNOTATED_VALUE(acquire_or_assert_expression, value_expression)

//
// ## INSTANT
//

// Writes an instant event associated with the current thread when the given category is enabled.
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_INSTANT(category, label, ...)                                               \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitInstant, category,           \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread, \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_INSTANT, but associates the event with the current CPU instead of the current
// thread.
#define KTRACE_CPU_INSTANT(category, label, ...)                                        \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitInstant, category,        \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_INSTANT, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time.
#define KTRACE_INSTANT_ENABLE(constexpr_enabled, category, label, ...)                        \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitInstant, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread,    \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_INSTANT_ENABLE, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_INSTANT_ENABLE(constexpr_enabled, category, label, ...)                    \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitInstant, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu,       \
                   KTrace::Unused{}, ##__VA_ARGS__)

//
// ## DURATION_BEGIN
//

// Writes a duration begin event associated with the current thread when the given category is
// enabled.
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_DURATION_BEGIN(category, label, ...)                                        \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitDurationBegin, category,     \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread, \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_BEGIN, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_DURATION_BEGIN(category, label, ...)                                 \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitDurationBegin, category,  \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_BEGIN, but checks the given constexpr_condition to determine whether
// the event is enabled at compile time.
#define KTRACE_DURATION_BEGIN_ENABLE(constexpr_enabled, category, label, ...)             \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitDurationBegin, \
                   category, FXT_INTERN_STRING(label), KTrace::Timestamp(),               \
                   KTrace::Context::Thread, KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_BEGIN_ENABLE, but associates the event with the current CPU instead of
// the current thread.
#define KTRACE_CPU_DURATION_BEGIN_ENABLE(constexpr_enabled, category, label, ...)                 \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitDurationBegin,         \
                   category, FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_BEGIN, but accepts a value convertible to fxt::StringRef for the
// label. Useful to tracing durations where the label comes from a table (e.g. syscall, vmm).
#define KTRACE_DURATION_BEGIN_LABEL_REF(category, label_ref, ...)                                 \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitDurationBegin, category, label_ref, \
                   KTrace::Timestamp(), KTrace::Context::Thread, KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_BEGIN, but accepts an expression to use for the event timestamp.
#define KTRACE_DURATION_BEGIN_TIMESTAMP(category, label, timestamp, ...)                           \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitDurationBegin, category,             \
                   FXT_INTERN_STRING(label), timestamp, KTrace::Context::Thread, KTrace::Unused{}, \
                   ##__VA_ARGS__)

//
// ## DURATION_END
//

// Writes a duration end event associated with the current thread when the given category is
// enabled.
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_DURATION_END(category, label, ...)                                          \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitDurationEnd, category,       \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread, \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_END, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_DURATION_END(category, label, ...)                                   \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitDurationEnd, category,    \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_END, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time.
#define KTRACE_DURATION_END_ENABLE(constexpr_enabled, category, label, ...)                       \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitDurationEnd, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread,        \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_END_ENABLE, but associates the event with the current CPU instead of
// the current thread.
#define KTRACE_CPU_DURATION_END_ENABLE(constexpr_enabled, category, label, ...)                   \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitDurationEnd, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu,           \
                   KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_END, but accepts a value convertible to fxt::StringRef for the label.
// Useful to tracing durations where the label comes from a table (e.g. syscall, vmm).
#define KTRACE_DURATION_END_LABEL_REF(category, label, ...)                                 \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitDurationEnd, category, label, \
                   KTrace::Timestamp(), KTrace::Context::Thread, KTrace::Unused{}, ##__VA_ARGS__)

// Similar to KTRACE_DURATION_END, but accepts an expression to use for the event timestamp.
#define KTRACE_DURATION_END_TIMESTAMP(category, label, timestamp, ...)                             \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitDurationEnd, category,               \
                   FXT_INTERN_STRING(label), timestamp, KTrace::Context::Thread, KTrace::Unused{}, \
                   ##__VA_ARGS__)

//
// ## COMPLETE
//

// Writes a duration complete event associated with the current thread when the given category is
// enabled.
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - start_timestamp: The starting timestamp for the event. Must be convertible to uint64_t.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_COMPLETE(category, label, start_timestamp, ...)                     \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitComplete, category,  \
                   FXT_INTERN_STRING(label), start_timestamp, KTrace::Timestamp(), \
                   KTrace::Context::Thread, ##__VA_ARGS__)

// Similar to KTRACE_COMPLETE, but associates the event with the current CPU instead of the current
// thread.
#define KTRACE_CPU_COMPLETE(category, label, start_timestamp, ...)                 \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitComplete, category,  \
                   FXT_INTERN_STRING(label), start_timestamp, KTrace::Timestamp(), \
                   KTrace::Context::Cpu, ##__VA_ARGS__)

// Similar to KTRACE_COMPLETE, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time.
#define KTRACE_COMPLETE_ENABLE(constexpr_enabled, category, label, start_timestamp, ...)       \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitComplete, category, \
                   FXT_INTERN_STRING(label), start_timestamp, KTrace::Timestamp(),             \
                   KTrace::Context::Thread, ##__VA_ARGS__)

// Similar to KTRACE_COMPLETE_ENABLE, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_COMPLETE_ENABLE(constexpr_enabled, category, label, start_timestamp, ...)   \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitComplete, category, \
                   FXT_INTERN_STRING(label), start_timestamp, KTrace::Timestamp(),             \
                   KTrace::Context::Cpu, ##__VA_ARGS__)

//
// ## COUNTER
//

// Writes a counter event associated with the current thread when the given category is enabled.
//
// Each argument is rendered as a separate value series named "<label>:<arg name>:<counter_id>".
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - counter_id: Correlation id for the event. Must be convertible to uint64_t.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_COUNTER(category, label, counter_id, ...)                                   \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitCounter, category,           \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread, \
                   counter_id, ##__VA_ARGS__)

// Similar to KTRACE_COUNTER, but associates the event with the current CPU instead of the current
// thread.
#define KTRACE_CPU_COUNTER(category, label, counter_id, ...)                            \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitCounter, category,        \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, \
                   counter_id, ##__VA_ARGS__)

// Similar to KTRACE_COUNTER, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time.
#define KTRACE_COUNTER_ENABLE(constexpr_enabled, category, label, counter_id, ...)            \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitCounter, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread,    \
                   counter_id, ##__VA_ARGS__)

// Similar to KTRACE_COUNTER_ENABLE, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_COUNTER_ENABLE(constexpr_enabled, category, label, counter_id, ...)        \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitCounter, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu,       \
                   counter_id, ##__VA_ARGS__)

//
// ## FLOW_BEGIN
//

// Writes a flow begin event associated with the current thread when the given category is enabled.
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - flow_id: Flow id for the event. Must be convertible to uint64_t.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_FLOW_BEGIN(category, label, flow_id, ...)                                   \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowBegin, category,         \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread, \
                   flow_id, ##__VA_ARGS__)

// Similar to KTRACE_FLOW_BEGIN, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_FLOW_BEGIN(category, label, flow_id, ...)                                     \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowBegin, category,               \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, flow_id, \
                   ##__VA_ARGS__)

// Similar to KTRACE_FLOW_BEGIN, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time.
#define KTRACE_FLOW_BEGIN_ENABLE(constexpr_enabled, category, label, flow_id, ...)              \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitFlowBegin, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread,      \
                   flow_id, ##__VA_ARGS__)

// Similar to KTRACE_FLOW_BEGIN_ENABLE, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_FLOW_BEGIN_ENABLE(constexpr_enabled, category, label, flow_id, ...)           \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitFlowBegin, category,  \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, flow_id, \
                   ##__VA_ARGS__)

// Similar to KTRACE_FLOW_BEGIN, but accepts an expression to use for the event timestamp.
#define KTRACE_FLOW_BEGIN_TIMESTAMP(category, label, timestamp, flow_id, ...)             \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowBegin, category,        \
                   FXT_INTERN_STRING(label), timestamp, KTrace::Context::Thread, flow_id, \
                   ##__VA_ARGS__)

//
// ## FLOW_STEP
//

// Writes a flow step event associated with the current thread when the given category is enabled.
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - flow_id: Flow id for the event. Must be convertible to uint64_t.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_FLOW_STEP(category, label, flow_id, ...)                                    \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowStep, category,          \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread, \
                   flow_id, ##__VA_ARGS__)

// Similar to KTRACE_FLOW_STEP, but associates the event with the current CPU instead of the current
// thread.
#define KTRACE_CPU_FLOW_STEP(category, label, flow_id, ...)                                      \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowStep, category,                \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, flow_id, \
                   ##__VA_ARGS__)

// Similar to KTRACE_FLOW_STEP, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time.
#define KTRACE_FLOW_STEP_ENABLE(constexpr_enabled, category, label, flow_id, ...)              \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitFlowStep, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread,     \
                   flow_id, ##__VA_ARGS__)

// Similar to KTRACE_FLOW_STEP_ENABLE, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_FLOW_STEP_ENABLE(constexpr_enabled, category, label, flow_id, ...)            \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitFlowStep, category,   \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, flow_id, \
                   ##__VA_ARGS__)

// Similar to KTRACE_FLOW_STEP, but accepts an expression to use for the event timestamp.
#define KTRACE_FLOW_STEP_TIMESTAMP(category, label, timestamp, flow_id, ...)              \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowStep, category,         \
                   FXT_INTERN_STRING(label), timestamp, KTrace::Context::Thread, flow_id, \
                   ##__VA_ARGS__)

//
// ## FLOW_END
//

// Writes a flow end event associated with the current thread when the given category is enabled.
//
// Arguments:
// - category: Filter category for the event. Expects a string literal.
// - label: Label for the event. Expects a string literal.
// - flow_id: Flow id for the event. Must be convertible to uint64_t.
// - ...: List of parenthesized argument tuples.
//
#define KTRACE_FLOW_END(category, label, flow_id, ...)                                     \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowEnd, category,           \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread, \
                   flow_id, ##__VA_ARGS__)

// Similar to KTRACE_FLOW_END, but associates the event with the current CPU instead of the current
// thread.
#define KTRACE_CPU_FLOW_END(category, label, flow_id, ...)                                       \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowEnd, category,                 \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, flow_id, \
                   ##__VA_ARGS__)

// Similar to KTRACE_FLOW_END, but checks the given constexpr_condition to determine whether the
// event is enabled at compile time.
#define KTRACE_FLOW_END_ENABLE(constexpr_enabled, category, label, flow_id, ...)              \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitFlowEnd, category, \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Thread,    \
                   flow_id, ##__VA_ARGS__)

// Similar to KTRACE_FLOW_END_ENABLE, but associates the event with the current CPU instead of the
// current thread.
#define KTRACE_CPU_FLOW_END_ENABLE(constexpr_enabled, category, label, flow_id, ...)             \
  FXT_EVENT_COMMON(constexpr_enabled, KTrace::CategoryEnabled, KTrace::EmitFlowEnd, category,    \
                   FXT_INTERN_STRING(label), KTrace::Timestamp(), KTrace::Context::Cpu, flow_id, \
                   ##__VA_ARGS__)

// Similar to KTRACE_FLOW_END, but accepts an expression to use for the event timestamp.
#define KTRACE_FLOW_END_TIMESTAMP(category, label, timestamp, flow_id, ...)               \
  FXT_EVENT_COMMON(true, KTrace::CategoryEnabled, KTrace::EmitFlowEnd, category,          \
                   FXT_INTERN_STRING(label), timestamp, KTrace::Context::Thread, flow_id, \
                   ##__VA_ARGS__)

//
// ## CONTEXT_SWITCH
//

// Writes a context switch record for the given threads.
#define KTRACE_CONTEXT_SWITCH(category, cpu, outgoing_state, outgoing_thread_ref, \
                              incoming_thread_ref, ...)                           \
  do {                                                                            \
    if (unlikely(KTrace::CategoryEnabled(FXT_INTERN_CATEGORY(category)))) {       \
      KTrace::EmitContextSwitch(                                                  \
          cpu, outgoing_state, outgoing_thread_ref, incoming_thread_ref,          \
          ktl::make_tuple(FXT_MAP_LIST_ARGS(FXT_MAKE_ARGUMENT, ##__VA_ARGS__)));  \
    }                                                                             \
  } while (false)

//
// ## THREAD_WAKEUP
//

// Writes a thread wakeup record for the given thread.
#define KTRACE_THREAD_WAKEUP(category, cpu, thread_ref, ...)                                      \
  do {                                                                                            \
    if (unlikely(KTrace::CategoryEnabled(FXT_INTERN_CATEGORY(category)))) {                       \
      KTrace::EmitThreadWakeup(                                                                   \
          cpu, thread_ref, ktl::make_tuple(FXT_MAP_LIST_ARGS(FXT_MAKE_ARGUMENT, ##__VA_ARGS__))); \
    }                                                                                             \
  } while (false)

//
// # Kernel tracing state and low level API
//

namespace ktrace {

using fxt::Koid;
using fxt::Pointer;
using fxt::Scope;

}  // namespace ktrace

// Bring the fxt literal operators into the global scope.
using fxt::operator""_category;
using fxt::operator""_intern;

void ktrace_report_live_threads();
void ktrace_report_live_processes();

class KTrace {
 private:
  // Allocator used by SpscBuffer to allocate and free its underlying storage.
  class KernelAspaceAllocator {
   public:
    static ktl::byte* Allocate(uint32_t size);
    static void Free(ktl::byte* ptr);
  };
  // PerCpuBuffer wraps an SpscBuffer and adds functionality to track dropped trace records.
  class PerCpuBuffer {
   public:
    using Reservation = SpscBuffer<KernelAspaceAllocator>::Reservation;

    // Initializes the underlying SpscBuffer.
    zx_status_t Init(uint32_t size) { return buffer_.Init(size); }

    // Drains the underlying SpscBuffer.
    void Drain() { buffer_.Drain(); }

    // Reads from the underlying SpscBuffer.
    template <CopyOutFunction CopyFunc>
    zx::result<uint32_t> Read(CopyFunc copy_fn, uint32_t len) {
      return buffer_.Read(copy_fn, len);
    }

    // We interpose ourselves in the Reserve path to ensure that we can emit a record containing
    // the dropped records statistics if we need to.
    zx::result<Reservation> Reserve(uint32_t size) {
      // If first_dropped_ is set to a value, then we are currently tracking a run of dropped trace
      // records, so we need to emit a duration record containing that information. We could emit
      // this record independently (with its own call to SpscBuffer::Reserve), but this could lead
      // to situations in which we thrash and emit multiple DroppedRecordDurationEvents in a row.
      // To avoid this, we attempt to reserve the desired size plus the size needed to store the
      // DroppedRecordDurationEvent record, and then write the statistics into the first part of
      // the reservation before returning it.
      uint32_t total_size = size;
      if (first_dropped_.has_value()) {
        DEBUG_ASSERT(last_dropped_.has_value());
        total_size += sizeof(DroppedRecordDurationEvent);
      }

      // Pass the Reserve call on to the SpscBuffer.
      zx::result<Reservation> res = buffer_.Reserve(total_size);
      if (res.is_error()) {
        // If the reservation failed, then we did not have enough space in this buffer, and the
        // record we were attempting to write will be dropped. Add the "size" to the dropped record
        // statistics. Notably, we do not add the "total_size," because that may include the size
        // of the DroppedRecordDurationEvent.
        TrackDroppedRecord(size);
        return res.take_error();
      }

      // If we need to write a dropped record duration event, do that here.
      DEBUG_ASSERT(first_dropped_.has_value() || total_size == size);
      if (first_dropped_.has_value()) {
        DroppedRecordDurationEvent record = SerializeDropStats();
        res->Write(ktl::span<ktl::byte>(reinterpret_cast<ktl::byte*>(&record), sizeof(record)));
        // We've successfully written out the dropped record stats, so reset them for the next run.
        ResetDropStats();
      }
      return res;
    }

    // Emit the dropped record stats to the trace buffer.
    // If we're not tracking a run of dropped records, this is a no-op.
    zx_status_t EmitDropStats() {
      if (!first_dropped_.has_value()) {
        DEBUG_ASSERT(!last_dropped_.has_value());
        return ZX_OK;
      }

      // Try to reserve a slot for the duration record. This will fail if there still isn't enough
      // space in buffer to store the statistics.
      zx::result<Reservation> res = buffer_.Reserve(sizeof(DroppedRecordDurationEvent));
      if (res.is_error()) {
        return res.status_value();
      }
      DroppedRecordDurationEvent record = SerializeDropStats();

      ktl::span bytes =
          ktl::span<const ktl::byte>(reinterpret_cast<const ktl::byte*>(&record), sizeof(record));
      res->Write(bytes);
      res->Commit();

      // We've successfully emitted a record containing statistics on the last run of dropped
      // records. To prepare for the next run, we must reset the stats.
      ResetDropStats();
      return ZX_OK;
    }

    // Resets the dropped records statistics to their initial values.
    // This is used to clear the stats after they've been emitted to a trace buffer.
    void ResetDropStats() {
      first_dropped_ = ktl::nullopt;
      last_dropped_ = ktl::nullopt;
      num_dropped_ = 0;
      bytes_dropped_ = 0;
    }

   private:
    friend class KTraceTests;

    // This is the structure of the FXT duration event that will store dropped record metadata in
    // the trace buffer. Normally, we would use the FXT serialization functions to build this record
    // dynamically, but we cannot do this in the PerCpuBuffer because those functions invoke
    // writer->Reserve, which would lead to recursion. To avoid this, we serialize the record
    // manually using this struct, which in turn is set up to match the structure outlined in the
    // FXT spec: https://fuchsia.dev/fuchsia-src/reference/tracing/trace-format#event-record
    //
    // Eventually, it would be nice to have the FXT serialization library support in-place
    // serialization, as that would allow us to remove this bespoke functionality.
    struct DroppedRecordDurationEvent {
      uint64_t header;
      zx_instant_boot_ticks_t start;
      uint64_t process_id;
      uint64_t thread_id;
      uint64_t num_dropped_arg;
      uint64_t bytes_dropped_arg;
      zx_instant_boot_ticks_t end;
    };
    static_assert(std::is_standard_layout_v<DroppedRecordDurationEvent>);

    // Serializes the dropped record statistics into a DroppedRecordDurationEvent.
    DroppedRecordDurationEvent SerializeDropStats() {
      // This method should only be called if we are currently tracking a run of dropped records.
      DEBUG_ASSERT(first_dropped_.has_value());
      DEBUG_ASSERT(last_dropped_.has_value());

      constexpr fxt::WordSize record_size =
          fxt::WordSize::FromBytes(sizeof(DroppedRecordDurationEvent));
      const fxt::ThreadRef thread_ref = ThreadRefFromContext(Context::Cpu);
      const fxt::StringRef<fxt::RefType::kId> name_ref = fxt::StringRef{"ktrace_drop_stats"_intern};
      const fxt::StringRef<fxt::RefType::kId> category_ref = fxt::StringRef{"kernel:meta"_intern};
      constexpr uint64_t num_args = 2;
      const fxt::Argument num_dropped_arg = fxt::Argument{"num_records"_intern, num_dropped_};
      const fxt::Argument bytes_dropped_arg = fxt::Argument{"num_bytes"_intern, bytes_dropped_};
      const uint64_t header =
          fxt::MakeHeader(fxt::RecordType::kEvent, record_size) |
          fxt::EventRecordFields::EventType::Make(
              ToUnderlyingType(fxt::EventType::kDurationComplete)) |
          fxt::EventRecordFields::ArgumentCount::Make(num_args) |
          fxt::EventRecordFields::ThreadRef::Make(thread_ref.HeaderEntry()) |
          fxt::EventRecordFields::CategoryStringRef::Make(category_ref.HeaderEntry()) |
          fxt::EventRecordFields::NameStringRef::Make(name_ref.HeaderEntry());

      return {
          .header = header,
          .start = first_dropped_.value(),
          .process_id = thread_ref.process().koid,
          .thread_id = thread_ref.thread().koid,
          .num_dropped_arg = num_dropped_arg.Header(),
          .bytes_dropped_arg = bytes_dropped_arg.Header(),
          .end = last_dropped_.value(),
      };
    }

    // Adds a dropped record of the given size to the tracked statistics.
    void TrackDroppedRecord(uint32_t size) {
      if (!first_dropped_.has_value()) {
        first_dropped_ = ktl::optional(Timestamp());
      }
      last_dropped_ = ktl::optional(Timestamp());
      num_dropped_++;
      bytes_dropped_ += size;
    }

    // The underlying SpscBuffer.
    SpscBuffer<KernelAspaceAllocator> buffer_;

    // This class keeps track of the duration, number, and size of trace records dropped when the
    // buffer is full. These statistics are emitted to the trace buffer as a duration as soon as
    // space is available to do so, at which point the values are reset to ktl::nullopt, in the
    // case of first_dropped_ and last_dropped_, or zero in the case of num_dropped_
    // and bytes_dropped_.
    ktl::optional<zx_instant_boot_ticks_t> first_dropped_;
    ktl::optional<zx_instant_boot_ticks_t> last_dropped_;
    // By storing num_dropped_ and bytes_dropped_ in 32-bit values, we ensure that they can each
    // be stored in a single 64-bit word in the FXT record we emit when space is available.
    uint32_t num_dropped_{0};
    uint32_t bytes_dropped_{0};
  };

 public:
  // Reservation encapsulates a pending write to the KTrace buffer.
  //
  // This class implements the fxt::Writer::Reservation trait, which is required by the FXT
  // serializer.
  //
  // It is absolutely imperative that interrupts remain disabled for the lifetime of this class.
  // Enabling interrupts at any point during the lifetime of this class will break the
  // single-writer invariant of each per-CPU buffer and lead to subtle concurrency bugs that may
  // manifest as corrupt trace data. Unfortunately, there is no way for us to programmatically
  // ensure this, so we do our best by asserting that interrupts are disabled in every method of
  // this class. It is therefore up to the caller to ensure that interrupts are disabled for the
  // lifetime of this object.
  class Reservation {
   public:
    ~Reservation() { DEBUG_ASSERT(arch_ints_disabled()); }

    // Disallow copies and move assignment, but allow moves.
    // Disallowing move assignment allows the saved interrupt state to be const.
    Reservation(const Reservation&) = delete;
    Reservation& operator=(const Reservation&) = delete;
    Reservation& operator=(Reservation&&) = delete;
    Reservation(Reservation&& other) : reservation_(ktl::move(other.reservation_)) {
      DEBUG_ASSERT(arch_ints_disabled());
    }

    void WriteWord(uint64_t word) {
      DEBUG_ASSERT(arch_ints_disabled());
      reservation_.Write(ktl::span<ktl::byte>(reinterpret_cast<ktl::byte*>(&word), sizeof(word)));
    }

    void WriteBytes(const void* bytes, size_t num_bytes) {
      DEBUG_ASSERT(arch_ints_disabled());
      // Write the data provided.
      reservation_.Write(
          ktl::span<const ktl::byte>(static_cast<const ktl::byte*>(bytes), num_bytes));

      // Write any padding bytes necessary.
      constexpr uint8_t kZero[8]{};
      const size_t aligned_bytes = ALIGN(num_bytes, 8);
      const size_t num_zeros_to_write = aligned_bytes - num_bytes;
      if (num_zeros_to_write != 0) {
        reservation_.Write(ktl::span<const ktl::byte>(reinterpret_cast<const ktl::byte*>(kZero),
                                                      num_zeros_to_write));
      }
    }

    void Commit() {
      DEBUG_ASSERT(arch_ints_disabled());
      reservation_.Commit();
    }

   private:
    friend class KTrace;
    Reservation(PerCpuBuffer::Reservation reservation, uint64_t header)
        : reservation_(ktl::move(reservation)) {
      DEBUG_ASSERT(arch_ints_disabled());
      WriteWord(header);
    }

    PerCpuBuffer::Reservation reservation_;
  };

  // Control is responsible for starting, stopping, or rewinding the ktrace buffer.
  //
  // The meaning of the options changes based on the action. If the action is to start tracing,
  // then the options field functions as the group mask.
  zx_status_t Control(uint32_t action, uint32_t options) TA_EXCL(lock_) {
    Guard<Mutex> guard{&lock_};
    switch (action) {
      case KTRACE_ACTION_START:
      case KTRACE_ACTION_START_CIRCULAR:
        return Start(action, options ? options : KTRACE_GRP_ALL);
      case KTRACE_ACTION_STOP:
        return Stop();
      case KTRACE_ACTION_REWIND:
        return Rewind();
      default:
        return ZX_ERR_INVALID_ARGS;
    }
  }

  // ReadUser reads len bytes from the ktrace buffer starting at offset off into the given user
  // buffer.
  //
  // The return value is one of the following:
  // * If ptr is nullptr, the number of bytes needed to read all of the available data is returned.
  // * Otherwise:
  //    * On success, this function returns the number of bytes that were read into the buffer.
  //    * On failure, a zx_status_t error code is returned.
  zx::result<size_t> ReadUser(user_out_ptr<void> ptr, uint32_t off, size_t len);

  // Reserve reserves a slot of memory to write a record into.
  //
  // This is likely not the method you want to use. In fact, it is exposed as a public method only
  // because the FXT serializer library requires it. If you're trying to write a record to the
  // global KTrace buffer, prefer using the Emit* methods below instead.
  //
  // This method MUST be invoked with interrupts disabled to enforce the single-writer invariant of
  // the per-CPU buffers.
  zx::result<Reservation> Reserve(uint64_t header);

  // Sentinel type for unused arguments.
  struct Unused {};

  // Specifies whether a trace record applies to the current thread or CPU.
  enum class Context {
    Thread,
    Cpu,
    // TODO(eieio): Support process?
  };

  // Initializes all of the state needed to support kernel tracing. This includes, but is not
  // limited to, calling Init on the KTrace instance. This is a no-op if tracing has been disabled.
  static void InitHook(unsigned);

  // Returns the timestamp that should be used to annotate ktrace records.
  static zx_instant_boot_ticks_t Timestamp() { return current_boot_ticks(); }

  static bool CategoryEnabled(const fxt::InternedCategory& category) {
    return GetInstance().IsCategoryEnabled(category);
  }

  // Generates an instant event record that contains the given arguments if the kernel:probe
  // category is enabled.
  static void Probe(Context context, const fxt::InternedString& label, uint64_t a, uint64_t b) {
    if (CategoryEnabled("kernel:probe"_category)) {
      InterruptDisableGuard guard;
      const fxt::StringRef name_ref = fxt::StringRef{label};
      fxt::WriteInstantEventRecord(
          &GetInstance(), KTrace::Timestamp(), ThreadRefFromContext(context),
          fxt::StringRef{"kernel:probe"_category.label()}, name_ref,
          fxt::Argument{"arg0"_intern, a}, fxt::Argument{"arg1"_intern, b});
    }
  }

  // The Emit* functions provide an API by which the ktrace macros above can write a trace record
  // to the ktrace buffer.
  template <fxt::RefType name_type, typename... Ts>
  static void EmitKernelObject(zx_koid_t koid, zx_obj_type_t obj_type,
                               const fxt::StringRef<name_type>& name,
                               const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteKernelObjectRecord(&GetInstance(), fxt::Koid(koid), obj_type, name,
                                       unpacked_args...);
        },
        args);
  }

  template <fxt::RefType name_type, typename... Ts>
  static void EmitComplete(const fxt::InternedCategory& category,
                           const fxt::StringRef<name_type>& label, uint64_t start_time,
                           uint64_t end_time, Context context, const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteDurationCompleteEventRecord(
              &GetInstance(), start_time, ThreadRefFromContext(context),
              fxt::StringRef{category.label()}, label, end_time, unpacked_args...);
        },
        args);
  }

  template <fxt::RefType name_type, typename... Ts>
  static void EmitInstant(const fxt::InternedCategory& category,
                          const fxt::StringRef<name_type>& label, uint64_t timestamp,
                          Context context, Unused, const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteInstantEventRecord(&GetInstance(), timestamp, ThreadRefFromContext(context),
                                       fxt::StringRef{category.label()}, label, unpacked_args...);
        },
        args);
  }

  template <fxt::RefType name_type, typename... Ts>
  static void EmitDurationBegin(const fxt::InternedCategory& category,
                                const fxt::StringRef<name_type>& label, uint64_t timestamp,
                                Context context, Unused, const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteDurationBeginEventRecord(
              &GetInstance(), timestamp, ThreadRefFromContext(context),
              fxt::StringRef{category.label()}, label, unpacked_args...);
        },
        args);
  }

  template <fxt::RefType name_type, typename... Ts>
  static void EmitDurationEnd(const fxt::InternedCategory& category,
                              const fxt::StringRef<name_type>& label, uint64_t timestamp,
                              Context context, Unused, const ktl::tuple<Ts...> args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteDurationEndEventRecord(&GetInstance(), timestamp, ThreadRefFromContext(context),
                                           fxt::StringRef{category.label()}, label,
                                           unpacked_args...);
        },
        args);
  }

  template <fxt::RefType name_type, typename... Ts>
  static void EmitCounter(const fxt::InternedCategory& category,
                          const fxt::StringRef<name_type>& label, uint64_t timestamp,
                          Context context, uint64_t counter_id, const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteCounterEventRecord(&GetInstance(), timestamp, ThreadRefFromContext(context),
                                       fxt::StringRef{category.label()}, label, counter_id,
                                       unpacked_args...);
        },
        args);
  }

  template <fxt::RefType name_type, typename... Ts>
  static void EmitFlowBegin(const fxt::InternedCategory& category,
                            const fxt::StringRef<name_type>& label, uint64_t timestamp,
                            Context context, uint64_t flow_id, const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteFlowBeginEventRecord(&GetInstance(), timestamp, ThreadRefFromContext(context),
                                         fxt::StringRef{category.label()}, label, flow_id,
                                         unpacked_args...);
        },
        args);
  }

  template <fxt::RefType name_type, typename... Ts>
  static void EmitFlowStep(const fxt::InternedCategory& category,
                           const fxt::StringRef<name_type>& label, uint64_t timestamp,
                           Context context, uint64_t flow_id, const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteFlowStepEventRecord(&GetInstance(), timestamp, ThreadRefFromContext(context),
                                        fxt::StringRef{category.label()}, label, flow_id,
                                        unpacked_args...);
        },
        args);
  }

  template <fxt::RefType name_type, typename... Ts>
  static void EmitFlowEnd(const fxt::InternedCategory& category,
                          const fxt::StringRef<name_type>& label, uint64_t timestamp,
                          Context context, uint64_t flow_id, const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteFlowEndEventRecord(&GetInstance(), timestamp, ThreadRefFromContext(context),
                                       fxt::StringRef{category.label()}, label, flow_id,
                                       unpacked_args...);
        },
        args);
  }

  // The EmitThreadWakeup and EmitContextSwitch functions have slightly different signatures from
  // the other Emit functions because they do not utilize the FXT_* macros to invoke them.
  template <typename... Ts>
  static void EmitThreadWakeup(const cpu_num_t cpu,
                               fxt::ThreadRef<fxt::RefType::kInline> thread_ref,
                               const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteThreadWakeupRecord(&GetInstance(), Timestamp(), static_cast<uint16_t>(cpu),
                                       thread_ref, unpacked_args...);
        },
        args);
  }

  template <typename... Ts>
  static void EmitContextSwitch(const cpu_num_t cpu, zx_thread_state_t outgoing_thread_state,
                                const fxt::ThreadRef<fxt::RefType::kInline>& outgoing_thread,
                                const fxt::ThreadRef<fxt::RefType::kInline>& incoming_thread,
                                const ktl::tuple<Ts...>& args) {
    InterruptDisableGuard guard;
    ktl::apply(
        [&](const Ts&... unpacked_args) {
          fxt::WriteContextSwitchRecord(&GetInstance(), Timestamp(), static_cast<uint16_t>(cpu),
                                        outgoing_thread_state, outgoing_thread, incoming_thread,
                                        unpacked_args...);
        },
        args);
  }

  // Retrieves a reference to the global KTrace instance.
  static KTrace& GetInstance() { return instance_; }

 private:
  friend class KTraceTests;
  friend class TestKTrace;

  // A special KOID used to signify the lack of an associated process.
  constexpr static fxt::Koid kNoProcess{0u};

  // Set this class up as a singleton by:
  // * Making the constructor and destructor private
  // * Preventing copies and moves
  constexpr explicit KTrace(bool disable_diagnostic_logs = false)
      : disable_diagnostic_logs_(disable_diagnostic_logs) {}
  virtual ~KTrace() = default;
  KTrace(const KTrace&) = delete;
  KTrace& operator=(const KTrace&) = delete;
  KTrace(KTrace&&) = delete;
  KTrace& operator=(KTrace&&) = delete;

  // Maintains the mapping from CPU numbers to pre-allocated KOIDs.
  class CpuContextMap {
   public:
    // Returns the pre-allocated KOID for the given CPU.
    zx_koid_t GetCpuKoid(cpu_num_t cpu_num) const { return cpu_koid_base_ + cpu_num; }

    // Returns a ThreadRef for the current CPU.
    fxt::ThreadRef<fxt::RefType::kInline> GetCurrentCpuRef() const {
      return GetCpuRef(arch_curr_cpu_num());
    }

    // Initializes the CPU KOID base value.
    void Init() {
      if (cpu_koid_base_ == ZX_KOID_INVALID) {
        cpu_koid_base_ = KernelObjectId::GenerateRange(arch_max_num_cpus());
      }
    }

   private:
    // Returns a ThreadRef for the given CPU.
    fxt::ThreadRef<fxt::RefType::kInline> GetCpuRef(cpu_num_t cpu_num) const {
      return {kNoProcess, fxt::Koid{GetCpuKoid(cpu_num)}};
    }

    // The KOID of CPU 0. Valid CPU KOIDs are in the range [base, base + max_cpus).
    zx_koid_t cpu_koid_base_;
  };

  // Retrieves a CPU or Thread reference depending on the Context that was passed in.
  static fxt::ThreadRef<fxt::RefType::kInline> ThreadRefFromContext(Context context) {
    switch (context) {
      case Context::Thread:
        return Thread::Current::Get()->fxt_ref();
      case Context::Cpu:
        return GetInstance().cpu_context_map_.GetCurrentCpuRef();
      default:
        return {kNoProcess, fxt::Koid{0}};
    }
  }

  // The global KTrace singleton.
  static KTrace instance_;

  // A small printf stand-in which gives tests the ability to disable diagnostic
  // printing during testing.
  int DiagsPrintf(int level, const char* fmt, ...) const __PRINTFLIKE(3, 4) {
    if (!disable_diagnostic_logs_ && DPRINTF_ENABLED_FOR_LEVEL(level)) {
      va_list args;
      va_start(args, fmt);
      int result = vprintf(fmt, args);
      va_end(args);
      return result;
    }
    return 0;
  }

  // Initializes the KTrace instance. Calling any other function on KTrace before calling Init
  // should be a no-op. This should only be called from the InitHook.
  void Init(uint32_t bufsize, uint32_t initial_grpmask) TA_EXCL(lock_);

  // Allocate our per-CPU buffers.
  zx_status_t Allocate() TA_REQ(lock_);

  // Start collecting trace data.
  // `action` must be one of KTRACE_ACTION_START or KTRACE_ACTION_START_CIRCULAR.
  // `categories` is the set of categories to trace. Cannot be zero.
  // TODO(https://fxbug.dev/404539312): The `action` argument is unnecessary once we switch to
  // the per-CPU streaming implementation by default.
  zx_status_t Start(uint32_t action, uint32_t categories) TA_REQ(lock_);
  // Stop collecting trace data.
  zx_status_t Stop() TA_REQ(lock_);
  // Rewinds the buffer, meaning that all contained trace data is dropped and the buffer is reset
  // to its initial state.
  zx_status_t Rewind() TA_REQ(lock_);

  // Returns true if the given category is enabled for tracing.
  bool IsCategoryEnabled(const fxt::InternedCategory& category) const {
    const uint32_t bit_number = category.index();
    if (bit_number == fxt::InternedCategory::kInvalidIndex) {
      return false;
    }
    const uint32_t bitmask = 1u << bit_number;
    return (bitmask & categories_bitmask()) != 0;
  }

  // Emits metadata records into the trace buffer.
  // This method is declared virtual to facilitate testing.
  virtual void ReportMetadata();

  // Getter and setter for the categories bitmask.
  // These use acquire-release semantics because the order in which we set the bitmask matters.
  // Specifically, when starting a trace, we want to ensure that all of the trace metadata that we
  // emit (such as thread and process names, among other things) happens-before we allow trace
  // records from arbitrary categories to enter the trace buffer. Without this affordance, it is
  // possible for trace records from other categories to fill up the buffer and cause the metadata
  // to be dropped, which would make the trace hard to understand.
  uint32_t categories_bitmask() const {
    return categories_bitmask_.load(ktl::memory_order_acquire);
  }
  void set_categories_bitmask(uint32_t new_mask) {
    categories_bitmask_.store(new_mask, ktl::memory_order_release);
  }

  // Enables writes to the ktrace buffer.
  // This uses release semantics to synchronize with writers when they check if writes are enabled,
  // which in turn ensures that operations to initialize the buffer during Init are not reordered
  // after the call to EnableWrites. Without this synchronization, it would be possible for a
  // writer to begin a write before the backing buffer has been allocated.
  void EnableWrites() { writes_enabled_.store(true, ktl::memory_order_release); }

  // Disables writes to the ktrace buffer.
  // Note: This function does not wait for any ongoing writes to complete. The caller is
  // responsible for doing this, likely using an IPI to all other cores.
  // It may be possible to perform this store with relaxed semantics, but we do it with release
  // semantics out of an abundance of caution.
  void DisableWrites() { writes_enabled_.store(false, ktl::memory_order_release); }

  // Returns true if writes are currently enabled.
  // This uses acquire semantics to ensure that it synchronizes with EnableWrites as described in
  // that method comment.
  bool WritesEnabled() const { return writes_enabled_.load(ktl::memory_order_acquire); }

  // A mapping of KOIDs to CPUs, used to annotate trace records.
  CpuContextMap cpu_context_map_;
  // A bitmask of categories to trace. Bits set to 1 indicate a category that should be traced.
  ktl::atomic<uint32_t> categories_bitmask_{0};
  // True if diagnostic log messages should not be printed. Set to true in tests to avoid logspam.
  const bool disable_diagnostic_logs_{false};

  // Stores whether writes are currently enabled.
  ktl::atomic<bool> writes_enabled_{false};

  // The buffers used to store data when using per-CPU mode.
  ktl::unique_ptr<PerCpuBuffer[]> percpu_buffers_{nullptr};
  // The number of buffers in percpu_buffers_. This is logically equivalent to the number of cores.
  // This should be set during Init and never modified.
  uint32_t num_buffers_ TA_GUARDED(lock_){0};
  // The size of each buffer in the percpu_buffers_.
  // This should be set during Init and never modified.
  uint32_t buffer_size_ TA_GUARDED(lock_){0};

  // Lock used to serialize non-write operations.
  DECLARE_MUTEX(KTrace) lock_;
};

#endif  // ZIRCON_KERNEL_INCLUDE_LIB_KTRACE_H_
