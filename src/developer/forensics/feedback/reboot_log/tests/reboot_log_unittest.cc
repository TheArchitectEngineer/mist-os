// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/reboot_log.h"

#include <fuchsia/hardware/power/statecontrol/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

#include <string>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/reboot_log/graceful_reboot_reason.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/files/file.h"
#include "src/lib/files/scoped_temp_dir.h"
#include "src/lib/timekeeper/test_clock.h"

namespace forensics {
namespace feedback {
namespace {

using fuchsia::hardware::power::statecontrol::RebootReason2;

struct RebootReasonTestParam {
  std::string test_name;
  std::optional<std::string> zircon_reboot_log;
  std::optional<RebootReason2> reboot_reason;
  RebootReason output_reboot_reason;
};

struct RebootMultiReasonTestParam {
  std::string test_name;
  std::vector<RebootReason2> reasons;
  RebootReason output_reboot_reason;
};

struct TimeTestParam {
  std::string test_name;
  std::optional<std::string> zircon_reboot_log;
  std::optional<zx::duration> output_uptime;
  std::optional<zx::duration> output_runtime;
};

struct CriticalProcessTestParam {
  std::string test_name;
  std::optional<std::string> zircon_reboot_log;
  std::optional<std::string> output_critical_process;
};

struct RebootLogStrTestParam {
  std::string test_name;
  std::optional<std::string> zircon_reboot_log;
  std::vector<RebootReason2> reboot_reasons;
  std::optional<std::string> output_reboot_log_str;
};

template <typename TestParam>
class RebootLogTest : public UnitTestFixture, public testing::WithParamInterface<TestParam> {
 protected:
  void WriteZirconRebootLogContents(const std::string& contents) {
    FX_CHECK(tmp_dir_.NewTempFileWithData(contents, &zircon_reboot_log_path_))
        << "Failed to create temporary Zircon reboot log";
  }

  void WriteGracefulRebootLogContents(const std::string& contents) {
    FX_CHECK(tmp_dir_.NewTempFileWithData(contents, &graceful_reboot_log_path_))
        << "Failed to create temporary graceful reboot log";
  }

  void WriteGracefulRebootLogContents(
      fuchsia::hardware::power::statecontrol::RebootOptions options) {
    FX_CHECK(tmp_dir_.NewTempFileWithData("", &graceful_reboot_log_path_))
        << "Failed to create temporary graceful reboot log";

    cobalt::Logger cobalt(dispatcher(), services(), &clock_);

    FX_CHECK(files::WriteFile(graceful_reboot_log_path_,
                              ToFileContent(ToGracefulRebootReasons(std::move(options)))));
  }

  std::string zircon_reboot_log_path_;
  std::string graceful_reboot_log_path_;

 private:
  timekeeper::TestClock clock_;
  files::ScopedTempDir tmp_dir_;
};

using RebootLogReasonTest = RebootLogTest<RebootReasonTestParam>;

fuchsia::hardware::power::statecontrol::RebootOptions NewRebootOptions(
    std::vector<RebootReason2> reasons) {
  fuchsia::hardware::power::statecontrol::RebootOptions options;
  options.set_reasons(std::move(reasons));
  return options;
}

INSTANTIATE_TEST_SUITE_P(
    WithVariousRebootLogs, RebootLogReasonTest,
    ::testing::ValuesIn(std::vector<RebootReasonTestParam>({
        {
            "ZirconCleanNoGraceful",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            std::nullopt,
            RebootReason::kGenericGraceful,
        },
        {
            "ZirconCleanGracefulUserRequest",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::USER_REQUEST,
            RebootReason::kUserRequest,
        },
        {
            "ZirconCleanGracefulSystemUpdate",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::SYSTEM_UPDATE,
            RebootReason::kSystemUpdate,
        },
        {
            "ZirconCleanGracefulNetstackMigration",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::NETSTACK_MIGRATION,
            RebootReason::kNetstackMigration,
        },
        {
            "ZirconCleanGracefulHighTemperature",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::HIGH_TEMPERATURE,
            RebootReason::kHighTemperature,
        },
        {
            "ZirconCleanGracefulSessionFailure",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::SESSION_FAILURE,
            RebootReason::kSessionFailure,
        },
        {
            "ZirconCleanGracefulNotSupported",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            static_cast<RebootReason2>(1000u),
            RebootReason::kGenericGraceful,
        },
        {
            "Cold",
            std::nullopt,
            RebootReason2::USER_REQUEST,
            RebootReason::kCold,

        },
        {
            "KernelPanic",
            "ZIRCON REBOOT REASON (KERNEL PANIC)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::USER_REQUEST,
            RebootReason::kKernelPanic,
        },
        {
            "OOM",
            "ZIRCON REBOOT REASON (OOM)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::USER_REQUEST,
            RebootReason::kOOM,
        },
        {
            "SwWatchdog",
            "ZIRCON REBOOT REASON (SW WATCHDOG)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::USER_REQUEST,
            RebootReason::kSoftwareWatchdogTimeout,
        },
        {
            "HwWatchdog",
            "ZIRCON REBOOT REASON (HW WATCHDOG)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::USER_REQUEST,
            RebootReason::kHardwareWatchdogTimeout,
        },
        {
            "Brownout",
            "ZIRCON REBOOT REASON (BROWNOUT)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::USER_REQUEST,
            RebootReason::kBrownout,
        },
        {
            "Spontaneous",
            "ZIRCON REBOOT REASON (UNKNOWN)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::USER_REQUEST,
            RebootReason::kSpontaneous,
        },
        {
            "RootJobTermination",
            "ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            RebootReason2::USER_REQUEST,
            RebootReason::kRootJobTermination,
        },
        {
            "NotParseable",
            "NOT PARSEABLE",
            RebootReason2::USER_REQUEST,
            RebootReason::kNotParseable,

        },
    })),
    [](const testing::TestParamInfo<RebootReasonTestParam>& info) { return info.param.test_name; });

TEST_P(RebootLogReasonTest, Succeed) {
  const auto param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  if (param.reboot_reason.has_value()) {
    WriteGracefulRebootLogContents(NewRebootOptions({param.reboot_reason.value()}));
  }

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.RebootReason(), param.output_reboot_reason);
}

TEST_F(RebootLogReasonTest, Succeed_ZirconCleanGracefulFdr) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  WriteGracefulRebootLogContents(NewRebootOptions({RebootReason2::SYSTEM_UPDATE}));

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/false));

  EXPECT_EQ(reboot_log.RebootReason(), RebootReason::kFdr);
}

TEST_F(RebootLogReasonTest, Succeed_ZirconCleanGracefulNotParseable) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  WriteGracefulRebootLogContents("NOT PARSEABLE");

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.RebootReason(), RebootReason::kGenericGraceful);

  ASSERT_TRUE(reboot_log.Uptime().has_value());
  EXPECT_EQ(*reboot_log.Uptime(), zx::msec(1234));

  ASSERT_TRUE(reboot_log.Runtime().has_value());
  EXPECT_EQ(*reboot_log.Runtime(), zx::msec(1098));
}

TEST_F(RebootLogReasonTest, Succeed_RebootReasonsUnset) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  fuchsia::hardware::power::statecontrol::RebootOptions options;
  WriteGracefulRebootLogContents(std::move(options));
  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.RebootReason(), RebootReason::kGenericGraceful);
}

TEST_F(RebootLogReasonTest, Succeed_RebootReasonsEmpty) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  WriteGracefulRebootLogContents(NewRebootOptions({}));
  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.RebootReason(), RebootReason::kGenericGraceful);
}

using RebootLogMultiReasonTest = RebootLogTest<RebootMultiReasonTestParam>;

INSTANTIATE_TEST_SUITE_P(WithVariousRebootLogs, RebootLogMultiReasonTest,
                         ::testing::ValuesIn(std::vector<RebootMultiReasonTestParam>(
                             {{
                                  "SystemUpdateThenNetstackMigration",
                                  {
                                      RebootReason2::SYSTEM_UPDATE,
                                      RebootReason2::NETSTACK_MIGRATION,
                                  },
                                  RebootReason::kSystemUpdate,
                              },
                              {
                                  "NetstackMigrationThenSystemUpdate",
                                  {
                                      RebootReason2::NETSTACK_MIGRATION,
                                      RebootReason2::SYSTEM_UPDATE,
                                  },
                                  RebootReason::kSystemUpdate,
                              },
                              {
                                  "UnexpectedCombination",
                                  {
                                      RebootReason2::OUT_OF_MEMORY,
                                      RebootReason2::SYSTEM_UPDATE,
                                  },
                                  RebootReason::kUnexpectedReasonGraceful,
                              }})),
                         [](const testing::TestParamInfo<RebootMultiReasonTestParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(RebootLogMultiReasonTest, Succeed) {
  const auto param = GetParam();

  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");

  WriteGracefulRebootLogContents(NewRebootOptions(param.reasons));

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.RebootReason(), param.output_reboot_reason);
}

using RebootLogTimeTest = RebootLogTest<TimeTestParam>;

INSTANTIATE_TEST_SUITE_P(
    WithVariousRebootLogs, RebootLogTimeTest,
    ::testing::ValuesIn(std::vector<TimeTestParam>({
        {
            "WellFormedLog",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            zx::msec(1234),
            zx::msec(1098),
        },
        {
            "NoZirconRebootLog",
            std::nullopt,
            std::nullopt,
            std::nullopt,
        },
        {
            "EmptyZirconRebootLog",
            "",
            std::nullopt,
            std::nullopt,
        },
        {
            "TooFewLinesForUptime",
            "BAD REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n",
            std::nullopt,
            std::nullopt,
        },
        {
            "BadUptimeString",
            "BAD REBOOT REASON (NO CRASH)\n\nDOWNTIME (ms)\n1234",
            std::nullopt,
            std::nullopt,
        },
        {
            "TooFewLinesForRuntime",
            "BAD REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n",
            zx::msec(1234),
            std::nullopt,
        },
        {
            "BadRuntimeString",
            "BAD REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nWALKTIME (ms)\n1098",
            zx::msec(1234),
            std::nullopt,
        },
    })),
    [](const testing::TestParamInfo<TimeTestParam>& info) { return info.param.test_name; });

TEST_P(RebootLogTimeTest, Succeed) {
  const auto param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));

  if (param.output_uptime.has_value()) {
    ASSERT_TRUE(reboot_log.Uptime().has_value());
    EXPECT_EQ(*reboot_log.Uptime(), param.output_uptime.value());
  } else {
    EXPECT_FALSE(reboot_log.Uptime().has_value());
  }

  if (param.output_runtime.has_value()) {
    ASSERT_TRUE(reboot_log.Runtime().has_value());
    EXPECT_EQ(*reboot_log.Runtime(), param.output_runtime.value());
  } else {
    EXPECT_FALSE(reboot_log.Runtime().has_value());
  }
}

using RebootLogCriticalProcessTest = RebootLogTest<CriticalProcessTestParam>;

INSTANTIATE_TEST_SUITE_P(
    WithVariousRebootLogs, RebootLogCriticalProcessTest,
    ::testing::ValuesIn(std::vector<CriticalProcessTestParam>({
        {
            "WellFormedLog",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\n"
            "ROOT JOB TERMINATED BY CRITICAL PROCESS DEATH: foo (1)",
            "foo",
        },
        {
            "NoZirconRebootLog",
            std::nullopt,
            std::nullopt,
        },
        {
            "EmptyZirconRebootLog",
            "",
            std::nullopt,
        },
        {
            "TooFewLines",
            "BAD REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n",
            std::nullopt,
        },
        {
            "BadCriticalProcessString",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\n"
            "ROOT JOB TERMINATED BY CRITICAL PROCESS ALIVE: foo (1)",
            std::nullopt,
        },
    })),
    [](const testing::TestParamInfo<CriticalProcessTestParam>& info) {
      return info.param.test_name;
    });

TEST_P(RebootLogCriticalProcessTest, Succeed) {
  const auto param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));

  if (param.output_critical_process.has_value()) {
    ASSERT_TRUE(reboot_log.CriticalProcess().has_value());
    EXPECT_EQ(*reboot_log.CriticalProcess(), param.output_critical_process.value());
  } else {
    EXPECT_FALSE(reboot_log.CriticalProcess().has_value());
  }
}

using RebootLogStrTest = RebootLogTest<RebootLogStrTestParam>;

INSTANTIATE_TEST_SUITE_P(
    WithVariousRebootLogs, RebootLogStrTest,
    ::testing::ValuesIn(std::vector<RebootLogStrTestParam>({
        {
            "ConcatenatesZirconAndGraceful",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            {RebootReason2::USER_REQUEST},
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\nGRACEFUL REBOOT REASONS: (USER "
            "REQUEST)\n\nFINAL REBOOT REASON (USER REQUEST)",
        },
        {
            // This test is the same as the above test, but is used to show that there may be an
            // ungraceful zircon reboot reason and a graceful reboot reason.
            "ConcatenatesZirconUngracefulAndGraceful",
            "ZIRCON REBOOT REASON (KERNEL PANIC)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            {RebootReason2::USER_REQUEST},
            "ZIRCON REBOOT REASON (KERNEL PANIC)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\nGRACEFUL REBOOT REASONS: "
            "(USER REQUEST)\n\nFINAL REBOOT REASON (KERNEL PANIC)",
        },
        {
            "NoGracefulRebootLog",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            {},
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\nGRACEFUL REBOOT REASONS: "
            "(NONE)\n\nFINAL REBOOT REASON (GENERIC GRACEFUL)",
        },
        {
            "MultipleGracefulRebootLog",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            {RebootReason2::NETSTACK_MIGRATION, RebootReason2::SYSTEM_UPDATE},
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\nGRACEFUL REBOOT REASONS: "
            "(NETSTACK MIGRATION,SYSTEM UPDATE)\n\nFINAL REBOOT REASON (SYSTEM UPDATE)",
        },
        {
            "NoZirconRebootLog",
            std::nullopt,
            {RebootReason2::USER_REQUEST},
            "GRACEFUL REBOOT REASONS: (USER REQUEST)\n\nFINAL REBOOT REASON (COLD)",
        },
    })),
    [](const testing::TestParamInfo<RebootLogStrTestParam>& info) { return info.param.test_name; });

TEST_P(RebootLogStrTest, Succeed) {
  const auto param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  if (!param.reboot_reasons.empty()) {
    WriteGracefulRebootLogContents(NewRebootOptions(param.reboot_reasons));
  }

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));

  if (param.output_reboot_log_str.has_value()) {
    EXPECT_EQ(reboot_log.RebootLogStr(), param.output_reboot_log_str.value());
  } else {
  }
}

TEST_F(RebootLogStrTest, Succeed_SetGracefulFDR) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  WriteGracefulRebootLogContents(NewRebootOptions({RebootReason2::FACTORY_DATA_RESET}));

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));
  EXPECT_EQ(reboot_log.RebootLogStr(),
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\n"
            "GRACEFUL REBOOT REASONS: (FACTORY DATA RESET)\n\n"
            "FINAL REBOOT REASON (FACTORY DATA RESET)");
}

TEST_F(RebootLogStrTest, Succeed_InferFDR) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/false));
  EXPECT_EQ(reboot_log.RebootReason(), RebootReason::kFdr);
  EXPECT_EQ(reboot_log.RebootLogStr(),
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\n"
            "GRACEFUL REBOOT REASONS: (NONE)\n\nFINAL REBOOT REASON (FACTORY DATA RESET)");
}

TEST_F(RebootLogStrTest, Succeed_SetDlog) {
  constexpr std::string_view kContents =
      R"(ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)

UPTIME (ms)
1234
RUNTIME (ms)
1098

--- BEGIN DLOG DUMP ---
test dlog dump line1
test dlog dump line2

--- END DLOG DUMP ---

GRACEFUL REBOOT REASONS: (NONE)

FINAL REBOOT REASON (ROOT JOB TERMINATION))";

  WriteZirconRebootLogContents(std::string(kContents));
  WriteGracefulRebootLogContents(NewRebootOptions({RebootReason2::CRITICAL_COMPONENT_FAILURE}));

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));
  EXPECT_EQ(reboot_log.Dlog(), "test dlog dump line1\ntest dlog dump line2");
}

TEST_F(RebootLogStrTest, Succeed_EmptyDlog) {
  constexpr std::string_view kContents =
      R"(ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)

  UPTIME (ms)
  1234
  RUNTIME (ms)
  1098

  --- BEGIN DLOG DUMP ---
  --- END DLOG DUMP ---

  GRACEFUL REBOOT REASONS: (NONE)

  FINAL REBOOT REASON (ROOT JOB TERMINATION))";

  WriteZirconRebootLogContents(std::string(kContents));
  WriteGracefulRebootLogContents(NewRebootOptions({RebootReason2::CRITICAL_COMPONENT_FAILURE}));

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));
  EXPECT_EQ(reboot_log.Dlog(), "");
}

TEST_F(RebootLogStrTest, Succeed_NoDlog) {
  constexpr std::string_view kContents =
      R"(ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)

  UPTIME (ms)
  1234
  RUNTIME (ms)
  1098

  GRACEFUL REBOOT REASONS: (NONE)

  FINAL REBOOT REASON (ROOT JOB TERMINATION))";

  WriteZirconRebootLogContents(std::string(kContents));
  WriteGracefulRebootLogContents(NewRebootOptions({RebootReason2::CRITICAL_COMPONENT_FAILURE}));

  const RebootLog reboot_log(RebootLog::ParseRebootLog(
      zircon_reboot_log_path_, graceful_reboot_log_path_, /*not_a_fdr=*/true));
  EXPECT_EQ(reboot_log.Dlog(), std::nullopt);
}

}  // namespace
}  // namespace feedback
}  // namespace forensics
