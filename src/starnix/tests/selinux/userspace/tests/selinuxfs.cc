// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/result.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/fs.h>

#include "src/lib/fxl/strings/string_number_conversions.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/selinux/userspace/util.h"

extern std::string DoPrePolicyLoadWork() { return "selinuxfs_policy.pp"; }

namespace {

using SeLinuxApiResult = fit::result<int, std::string>;

/// Calls the specified `api` with the supplied `request` and returns the resulting output.
SeLinuxApiResult CallSeLinuxApi(std::string_view api, std::string_view request) {
  constexpr char kSeLinuxFsApiFmt[] = "/sys/fs/selinux/%s";

  auto api_path = fxl::StringPrintf(kSeLinuxFsApiFmt, std::string(api).data());
  fbl::unique_fd api_fd(open(api_path.data(), O_RDWR));
  if (!api_fd.is_valid()) {
    return fit::error(errno);
  }

  if (!request.empty() && write(api_fd.get(), request.data(), request.length()) < 0) {
    return fit::error(errno);
  }

  std::string result;
  char read_buf[10];
  while (true) {
    ssize_t read_result = read(api_fd.get(), read_buf, sizeof(read_buf));
    if (read_result == 0) {
      // Some APIs include a trailing NUL under Linux, where Starnix does not include it.
      return fit::ok(RemoveTrailingNul(result));
    }
    if (read_result < 0) {
      return fit::error(errno);
    }
    result.append(read_buf, read_result);
  }
}

/// Reads a `uint32_t` value from the SELinuxFS `path` and returns it, if it is well-formed.
uint32_t ReadSeLinuxFsU32(std::string path) {
  auto result = CallSeLinuxApi(path, std::string_view());
  uint32_t class_id{};
  if (result.is_error() || !fxl::StringToNumberWithError(result.value(), &class_id)) {
    ADD_FAILURE() << "Failed to read SELinuxFS U32: " << path;
    return 0u;
  }
  return class_id;
}

uint32_t GetClassId(std::string_view class_name) {
  constexpr char kSeLinuxFsClassIndexApiNameFmt[] = "class/%s/index";

  auto api_name = fxl::StringPrintf(kSeLinuxFsClassIndexApiNameFmt, std::string(class_name).data());
  return ReadSeLinuxFsU32(api_name);
}

uint32_t GetPermissionId(std::string_view class_name, std::string_view perm_name) {
  constexpr char kSeLinuxFsPermissionIndexApiNameFmt[] = "class/%s/perms/%s";

  auto api_name = fxl::StringPrintf(kSeLinuxFsPermissionIndexApiNameFmt,
                                    std::string(class_name).data(), std::string(perm_name).data());
  return ReadSeLinuxFsU32(api_name);
}

uint32_t GetAccessVector(std::string_view class_name, std::string_view perm_name) {
  auto perm_index = GetPermissionId(class_name, perm_name);
  if (perm_index < 1 || perm_index > 32) {
    ADD_FAILURE() << "Read invalid permission Id: " << perm_index;
    return 0u;
  }
  uint32_t perm_mask = 1 << (perm_index - 1);
  return perm_mask;
}

SeLinuxApiResult ValidateContext(std::string_view context) {
  return CallSeLinuxApi("context", context);
}

SeLinuxApiResult ComputeCreateContext(std::string_view source_context,
                                      std::string_view target_context,
                                      std::string_view target_class) {
  auto class_id = GetClassId(target_class);
  constexpr char kSeLinuxCreateRequestFmt[] = "%s %s %hu";
  auto request = fxl::StringPrintf(kSeLinuxCreateRequestFmt, std::string(source_context).data(),
                                   std::string(target_context).data(), class_id);
  return CallSeLinuxApi("create", request);
}

SeLinuxApiResult ComputeAccess(std::string_view source_context, std::string_view target_context,
                               std::string_view target_class, uint32_t requested) {
  auto class_id = GetClassId(target_class);
  constexpr char kSeLinuxCreateRequestFmt[] = "%s %s %hu %x";
  auto request = fxl::StringPrintf(kSeLinuxCreateRequestFmt, std::string(source_context).data(),
                                   std::string(target_context).data(), class_id, requested);
  return CallSeLinuxApi("access", request);
}

TEST(SeLinuxFsNull, HasPolicyDevnullContext) {
  constexpr char kSeLinuxFsNull[] = "/sys/fs/selinux/null";

  EXPECT_EQ(GetLabel(kSeLinuxFsNull), fit::ok("system_u:object_r:devnull_t:s0"));
}

TEST(SeLinuxFsContext, ValidatesRequiredFieldsPresent) {
  // Contexts that have too few colons to provide user, role, type & sensitivity are rejected.
  EXPECT_EQ(ValidateContext("test_selinuxfs_u"), fit::failed());
  EXPECT_EQ(ValidateContext("test_selinuxfs_u:test_selinuxfs_r"), fit::failed());
  EXPECT_EQ(ValidateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t"), fit::failed());

  // The minimum valid context has at least user, role, type and low/default sensitivity.
  constexpr std::string_view kMinimumValidContext =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0";
  EXPECT_EQ(ValidateContext(kMinimumValidContext), fit::ok(kMinimumValidContext));
}

TEST(SeLinuxFsContext, ValidatesFieldValues) {
  // Valid contexts are successfully written, and can be read-back.
  constexpr std::string_view kValidContext =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0:c0-s2:c0.c2";
  EXPECT_EQ(ValidateContext(kValidContext), fit::ok(kValidContext));

  // Context user must be defined by the policy.
  EXPECT_EQ(ValidateContext("bad_value:test_selinuxfs_r:test_selinuxfs_t:s0:c0-s2:c0.c2"),
            fit::failed());

  // Context role must be defined by the policy.
  EXPECT_EQ(ValidateContext("test_selinuxfs_u:bad_value:test_selinuxfs_t:s0:c0-s2:c0.c2"),
            fit::failed());

  // Context type/domain must be defined by the policy.
  EXPECT_EQ(ValidateContext("test_selinuxfs_u:test_selinuxfs_r:bad_value:s0:c0-s2:c0.c2"),
            fit::failed());

  // Context low & high sensitivities must be defined by the policy.
  EXPECT_EQ(
      ValidateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:bad_value:c0-s2:c0.c2"),
      fit::failed());
  EXPECT_EQ(
      ValidateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0:c0-bad_value:c0.c2"),
      fit::failed());

  // Context low & high categories must be defined by the policy.
  EXPECT_EQ(
      ValidateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0:bad_value-s2:c0.c2"),
      fit::failed());
  EXPECT_EQ(
      ValidateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0:c0-s2:c0.bad_value"),
      fit::failed());
}

TEST(SeLinuxFsContext, ValidatesAllowedUserFieldValues) {
  // The "test_selinuxfs_u" user is granted the full range of categories.
  constexpr std::string_view kValidContext =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0:c0-s2:c0.c2";
  EXPECT_EQ(ValidateContext(kValidContext), fit::ok(kValidContext));

  // The "test_selinuxfs_limited_u" user is granted only "s0" sensitivity, must have "c0" category
  // and may have "c1" category.
  constexpr std::string_view kLimitedContext_Valid =
      "test_selinuxfs_limited_level_u:test_selinuxfs_r:test_selinuxfs_t:s0:c0";
  EXPECT_EQ(ValidateContext(kLimitedContext_Valid), fit::ok(kLimitedContext_Valid));

  constexpr std::string_view kLimitedContext_MissingCategory =
      "test_selinuxfs_limited_level_u:test_selinuxfs_r:test_selinuxfs_t:s0";
  EXPECT_EQ(ValidateContext(kLimitedContext_MissingCategory), fit::failed());

  constexpr std::string_view kLimitedContext_BadSensitivity =
      "test_selinuxfs_limited_level_u:test_selinuxfs_r:test_selinuxfs_t:s1:c0";
  EXPECT_EQ(ValidateContext(kLimitedContext_BadSensitivity), fit::failed());
}

TEST(SeLinuxFsContext, NormalizeCategories) {
  // Expansion of a three-element category span results in the same Security Context as using the
  // range syntax directly.
  constexpr std::string_view kThreeCategoryContextFormA =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s2:c0.c2";
  constexpr std::string_view kThreeCategoryContextFormB =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s2:c0,c1,c2";
  EXPECT_EQ(ValidateContext(kThreeCategoryContextFormA),
            ValidateContext(kThreeCategoryContextFormB));

  // Using a pair of categories results in the same Security Context as a two-element range.
  constexpr std::string_view kTwoCategoryContextFormA =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s2:c0.c1";
  constexpr std::string_view kTwoCategoryContextFormB =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s2:c0,c1";
  EXPECT_EQ(ValidateContext(kTwoCategoryContextFormA), ValidateContext(kTwoCategoryContextFormB));
}

// Validate that Security Contexts for new "process" class instances inherit the source role & type.
TEST(SeLinuxFsCreate, DefaultComputeCreateForProcess) {
  constexpr std::string_view kSourceContext =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0";
  constexpr std::string_view kTargetContext =
      "test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0";

  EXPECT_EQ(ComputeCreateContext(kSourceContext, kTargetContext, "process"),
            fit::ok("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0"));
}

// Validate that Security Contexts for new socket-like class instances behave the same as "process".
TEST(SeLinuxFsCreate, DefaultComputeCreateForSocket) {
  constexpr std::string_view kSourceContext =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0";
  constexpr std::string_view kTargetContext =
      "test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0";

  EXPECT_EQ(ComputeCreateContext(kSourceContext, kTargetContext, "tcp_socket"),
            fit::ok("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0"));
}

// Validate that Security Contexts for non-process/socket-like class instances receive "object_r"
// role, and the target/parent type.
TEST(SeLinuxFsCreate, DefaultComputeCreateForFile) {
  constexpr std::string_view kSourceContext =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0";
  constexpr std::string_view kTargetContext =
      "test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0";

  EXPECT_EQ(ComputeCreateContext(kSourceContext, kTargetContext, "file"),
            fit::ok("test_selinuxfs_u:object_r:test_selinuxfs_create_target_t:s0"));
}

TEST(SeLinuxFsCreate, ComputeCreateForFifoFile) {
  constexpr std::string_view kSourceContext =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0";
  constexpr std::string_view kTargetContext =
      "test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0";

  EXPECT_EQ(ComputeCreateContext(kSourceContext, kTargetContext, "fifo_file"),
            fit::ok("test_selinuxfs_u:object_r:test_selinuxfs_create_target_t:s0"));
}

// Validate handling of whitespace between request elements.
TEST(SeLinuxFsCreate, ExtraWhitespace) {
  constexpr std::string_view kExpectedContext =
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0";

  // Additional whitespace between elements is ignored.
  EXPECT_EQ(
      ComputeCreateContext(" test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                           "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process"),
      fit::ok(kExpectedContext));
  EXPECT_EQ(
      ComputeCreateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 ",
                           "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process"),
      fit::ok(kExpectedContext));
  EXPECT_EQ(
      ComputeCreateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                           "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 ", "process"),
      fit::ok(kExpectedContext));

  // Fields are separated by spaces, with leading & trailing whitespace around each element ignored.
  EXPECT_EQ(
      ComputeCreateContext("\ttest_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                           "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process"),
      fit::ok(kExpectedContext));
  EXPECT_EQ(
      ComputeCreateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\t",
                           "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process"),
      fit::ok(kExpectedContext));
  EXPECT_EQ(
      ComputeCreateContext("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                           "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\t", "process"),
      fit::ok(kExpectedContext));
  EXPECT_EQ(
      ComputeCreateContext("\ntest_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                           "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\n", "process"),
      fit::ok(kExpectedContext));

  // Although whitespace around elements is ignored, only spaces are valid separators.
  EXPECT_EQ(
      CallSeLinuxApi(
          "create",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\ttest_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\tprocess"),
      fit::error(EINVAL));
  EXPECT_EQ(
      CallSeLinuxApi(
          "create",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\ntest_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\nprocess"),
      fit::error(EINVAL));
}

// "create" API requests are appropriately validated.
TEST(SeLinuxFsCreate, ComputeCreateRequestValidation) {
  // Fewer than three arguments is an invalid request.
  EXPECT_EQ(CallSeLinuxApi("create", "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0"),
            fit::error(EINVAL));
  EXPECT_EQ(
      CallSeLinuxApi(
          "create",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0"),
      fit::error(EINVAL));

  // More than four arguments is an invalid request.
  EXPECT_EQ(
      CallSeLinuxApi(
          "create",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 process test bad"),
      fit::error(EINVAL));

  // If either security context is malformed, it is an invalid request.
  EXPECT_EQ(
      ComputeCreateContext("test_selinuxfs_non_existent_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                           "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process"),
      fit::error(EINVAL));
  EXPECT_EQ(ComputeCreateContext(
                "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                "test_selinuxfs_non_existent_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process"),
            fit::error(EINVAL));
}

// Validate "create" handling of class Id values not defined in the loaded policy.
// It appears that invalid class Ids may be "clamped" to the class Id range, resulting in their
// being labeled according to whether the first/last class in the policy is process/socket-like, or
// not.
TEST(SeLinuxFsCreate, InvalidComputeCreateClassId) {
  // Zero is not a valid class Id, but is apparently treated as process/socket-like.
  EXPECT_EQ(
      CallSeLinuxApi(
          "create",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0 0"),
      fit::ok("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0"));

  // 65535 is not a valid class Id in the test policy, but is apparently treated as
  // non-process/socket-like.
  EXPECT_EQ(
      CallSeLinuxApi(
          "create",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0 65535"),
      fit::ok("test_selinuxfs_u:object_r:test_selinuxfs_create_target_t:s0"));
}

// Validate handling of whitespace between request elements.
TEST(SeLinuxFsAccess, ExtraWhitespace) {
  const auto kRequested = GetAccessVector("process", "fork");
  const auto kExpectedResult =
      ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                    "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process", kRequested)
          .value();

  // Additional whitespace between elements is ignored.
  EXPECT_EQ(
      ComputeAccess(" test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                    "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process", kRequested),
      fit::ok(kExpectedResult));
  EXPECT_EQ(
      ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 ",
                    "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process", kRequested),
      fit::ok(kExpectedResult));
  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 ", "process",
                          kRequested),
            fit::ok(kExpectedResult));

  // Fields are separated by spaces, with leading & trailing whitespace around each element ignored.
  EXPECT_EQ(
      ComputeAccess("\ttest_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                    "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process", kRequested),
      fit::ok(kExpectedResult));
  EXPECT_EQ(
      ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\t",
                    "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process", kRequested),
      fit::ok(kExpectedResult));
  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\t", "process",
                          kRequested),
            fit::ok(kExpectedResult));
  EXPECT_EQ(ComputeAccess("\ntest_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\n", "process",
                          kRequested),
            fit::ok(kExpectedResult));

  // Although whitespace around elements is ignored, only spaces are valid separators.
  EXPECT_EQ(
      CallSeLinuxApi(
          "access",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\ttest_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\tprocess\t1"),
      fit::error(EINVAL));
  EXPECT_EQ(
      CallSeLinuxApi(
          "access",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\ntest_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0\nprocess\t1"),
      fit::error(EINVAL));
}

// "access" API requests with too few arguments are invalid.
TEST(SeLinuxFsAccess, TooFewArgumentsIsInvalid) {
  // Fewer than three arguments is an invalid request.
  EXPECT_EQ(CallSeLinuxApi("access", "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0"),
            fit::error(EINVAL));
  EXPECT_EQ(
      CallSeLinuxApi(
          "access",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0"),
      fit::error(EINVAL));
}

// "access" API requests with three arguments are valid, even with trailing data.
TEST(SeLinuxFsAccess, ClassIdMayHaveTrailingNonNumericData) {
  const uint32_t kTargetClassId = GetClassId("test_selinuxfs_target_class");
  const auto kRequestBase = fxl::StringPrintf(
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_u:object_r:test_selinuxfs_access_myperm1234_target_t:s0 %u",
      kTargetClassId);
  const std::string_view kExpectedAccess = "f ffffffff 0 ffffffff 1 0";

  // Three arguments (just the `kRequestBase` with no "requested" set) is valid.
  ASSERT_EQ(CallSeLinuxApi("access", kRequestBase), fit::ok(kExpectedAccess));

  // Trailing non-numeric characters in the target class argument appear to be ignored. This is
  // consistent with the way that `atoi()` handles trailing non-numeric characters.
  EXPECT_EQ(CallSeLinuxApi("access", kRequestBase + "third_arg_trailing_junk"),
            fit::ok(kExpectedAccess));
}

// The class Id argument must be a valid decimal.
TEST(SeLinuxFsAccess, ClassIdMustBeDecimal) {
  // Non-numeric class Ids are rejected.
  EXPECT_EQ(
      CallSeLinuxApi(
          "access",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0 bad"),
      fit::error(EINVAL));
}

// "access" API requests with a fourth argument containing arbitrary data is accepted.
TEST(SeLinuxFsAccess, FourthArgumentIsNotValidated) {
  const uint32_t kTargetClassId = GetClassId("test_selinuxfs_target_class");
  const auto kRequestBase = fxl::StringPrintf(
      "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_u:object_r:test_selinuxfs_access_myperm1234_target_t:s0 %u",
      kTargetClassId);
  const std::string_view kExpectedAccess = "f ffffffff 0 ffffffff 1 0";

  // Four arguments (i.e. including the "requested" set) is valid, though the requested set has no
  // effect on the result.
  EXPECT_EQ(CallSeLinuxApi("access", kRequestBase + " 0"), fit::ok(kExpectedAccess));
  EXPECT_EQ(CallSeLinuxApi("access", kRequestBase + " ffffffff"), fit::ok(kExpectedAccess));
  EXPECT_EQ(CallSeLinuxApi("access", kRequestBase + " -1"), fit::ok(kExpectedAccess));

  // There is no validation that the requested set argument is a valid hex value.
  EXPECT_EQ(CallSeLinuxApi("access", kRequestBase + " non_numeric"), fit::ok(kExpectedAccess));
}

// If either security context is malformed, it is an invalid request.
TEST(SeLinuxFsAccess, SecurityContextsMustBeValid) {
  const auto kRequested = GetAccessVector("process", "fork");
  EXPECT_EQ(
      ComputeAccess("test_selinuxfs_non_existent_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                    "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0", "process", kRequested),
      fit::error(EINVAL));
  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_non_existent_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "process", kRequested),
            fit::error(EINVAL));
}

// Validate "access" handling of class Id values not defined in the loaded policy.
// These queries appear to fall-back on the "handle unknown" behaviour, which is set to "deny" in
// our test policy.
TEST(SeLinuxFsAccess, UnknownClassIdIsAccepted) {
  constexpr char kAllAccessDenied[] = "0 ffffffff 0 ffffffff 1 0";

  // Zero is not a valid class Id.
  EXPECT_EQ(
      CallSeLinuxApi(
          "access",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0 0 1"),
      fit::ok(kAllAccessDenied));

  // 65535 is not a valid class Id in the test policy.
  EXPECT_EQ(
      CallSeLinuxApi(
          "access",
          "test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0 test_selinuxfs_create_target_u:test_selinuxfs_create_target_r:test_selinuxfs_create_target_t:s0 65535 1"),
      fit::ok(kAllAccessDenied));
}

// Validate that a subject domain that is marked `permissive` in the policy is reported as having
// no permissions allowed, but the permissive bit set in the "flags" argument of the result.
TEST(SeLinuxFsAccess, ComputeAccessPermissiveSubject) {
  constexpr uint32_t kAllPerms = UINT32_MAX;

  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_access_permissive_t:s0",
                          "test_selinuxfs_u:object_r:test_selinuxfs_access_no_perms_target_t:s0",
                          "test_selinuxfs_target_class", kAllPerms),
            fit::ok("0 ffffffff 0 ffffffff 1 1"));
}

// Validate that the allowed permissions are reported correctly in the result.
TEST(SeLinuxFsAccess, ComputeAccessPermissions) {
  constexpr uint32_t kAllPerms = UINT32_MAX;

  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:object_r:test_selinuxfs_access_no_perms_target_t:s0",
                          "test_selinuxfs_target_class", kAllPerms),
            fit::ok("0 ffffffff 0 ffffffff 1 0"));
  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:object_r:test_selinuxfs_access_myperm1_target_t:s0",
                          "test_selinuxfs_target_class", kAllPerms),
            fit::ok("1 ffffffff 0 ffffffff 1 0"));
  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:object_r:test_selinuxfs_access_myperm1234_target_t:s0",
                          "test_selinuxfs_target_class", kAllPerms),
            fit::ok("f ffffffff 0 ffffffff 1 0"));
  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:object_r:test_selinuxfs_access_all_target_t:s0",
                          "test_selinuxfs_target_class", kAllPerms),
            fit::ok("f ffffffff 0 ffffffff 1 0"));
}

// Validate that the `auditallow` and `dontaudit` statements are taken into account in the reported
// "audit allow" and "audit deny" sets.
TEST(SeLinuxFsAccess, ComputeAccessAudit) {
  constexpr uint32_t kAllPerms = UINT32_MAX;

  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:object_r:test_selinuxfs_access_audit_all_target_t:s0",
                          "test_selinuxfs_target_class", kAllPerms),
            fit::ok("0 ffffffff f ffffffff 1 0"));
  EXPECT_EQ(ComputeAccess("test_selinuxfs_u:test_selinuxfs_r:test_selinuxfs_t:s0",
                          "test_selinuxfs_u:object_r:test_selinuxfs_access_audit_none_target_t:s0",
                          "test_selinuxfs_target_class", kAllPerms),
            fit::ok("0 ffffffff 0 fffffff0 1 0"));
}

}  // namespace
