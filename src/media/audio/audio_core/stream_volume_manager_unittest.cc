// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/audio_core/stream_volume_manager.h"

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/media/audio/lib/processing/gain.h"

namespace media::audio {
namespace {

using fuchsia::media::AudioCaptureUsage2;
using fuchsia::media::AudioRenderUsage2;

class MockStreamVolume : public StreamVolume {
 public:
  fuchsia::media::Usage2 GetStreamUsage() const override { return fidl::Clone(usage_); }
  bool RespectsPolicyAdjustments() const override { return respects_policy_adjustments_; }
  void RealizeVolume(VolumeCommand volume_command) override {
    volume_command_ = volume_command;
    ++realize_volume_calls_;
  }

  bool mute_ = false;
  int realize_volume_calls_ = 0;
  fuchsia::media::Usage2 usage_;
  VolumeCommand volume_command_ = {};
  bool respects_policy_adjustments_ = true;
};

class StreamVolumeManagerTest : public ::gtest::TestLoopFixture {
 protected:
  StreamVolumeManagerTest() : manager_(dispatcher()) {}

  fuchsia::media::audio::VolumeControlPtr AddClientForUsage(fuchsia::media::Usage2 usage) {
    fuchsia::media::audio::VolumeControlPtr volume_control_ptr;
    manager_.BindUsageVolumeClient(std::move(usage), volume_control_ptr.NewRequest(dispatcher()));
    return volume_control_ptr;
  }

  MockStreamVolume mock_;
  StreamVolumeManager manager_;
};

TEST_F(StreamVolumeManagerTest, StreamCanUpdateSelf) {
  mock_.usage_ = ToFidlUsage2(RenderUsage::INTERRUPTION);

  manager_.NotifyStreamChanged(&mock_);
  EXPECT_FLOAT_EQ(mock_.volume_command_.volume, 1.0);
  EXPECT_FLOAT_EQ(mock_.volume_command_.gain_db_adjustment, media_audio::kUnityGainDb);
  EXPECT_EQ(mock_.volume_command_.ramp, std::nullopt);
}

TEST_F(StreamVolumeManagerTest, StreamUpdatedOnAdd) {
  mock_.usage_ = ToFidlUsage2(RenderUsage::INTERRUPTION);

  manager_.AddStream(&mock_);
  EXPECT_FLOAT_EQ(mock_.volume_command_.volume, 1.0);
  EXPECT_FLOAT_EQ(mock_.volume_command_.gain_db_adjustment, media_audio::kUnityGainDb);
  EXPECT_EQ(mock_.volume_command_.ramp, std::nullopt);
}

TEST_F(StreamVolumeManagerTest, StreamCanIgnorePolicy) {
  const auto usage = ToFidlUsage2(RenderUsage::INTERRUPTION);
  mock_.usage_ = fidl::Clone(usage);

  manager_.SetUsageGainAdjustment(fidl::Clone(usage), media_audio::kMinGainDb);

  manager_.NotifyStreamChanged(&mock_);
  EXPECT_FLOAT_EQ(mock_.volume_command_.gain_db_adjustment, media_audio::kMinGainDb);

  mock_.respects_policy_adjustments_ = false;
  manager_.NotifyStreamChanged(&mock_);
  EXPECT_FLOAT_EQ(mock_.volume_command_.gain_db_adjustment, 0.0);
}

TEST_F(StreamVolumeManagerTest, UsageChangesUpdateRegisteredStreams) {
  mock_.usage_ = ToFidlUsage2(RenderUsage::SYSTEM_AGENT);

  manager_.AddStream(&mock_);
  manager_.SetUsageGain(ToFidlUsage2(RenderUsage::SYSTEM_AGENT), -10.0);

  EXPECT_FLOAT_EQ(mock_.volume_command_.gain_db_adjustment, -10.0);
}

TEST_F(StreamVolumeManagerTest, StreamsCanBeRemoved) {
  mock_.usage_ = ToFidlUsage2(RenderUsage::SYSTEM_AGENT);

  manager_.AddStream(&mock_);
  manager_.RemoveStream(&mock_);
  manager_.SetUsageGain(ToFidlUsage2(RenderUsage::SYSTEM_AGENT), 10.0);

  EXPECT_FLOAT_EQ(mock_.volume_command_.volume, 1.0);
  EXPECT_FLOAT_EQ(mock_.volume_command_.gain_db_adjustment, media_audio::kUnityGainDb);
  EXPECT_EQ(mock_.volume_command_.ramp, std::nullopt);
}

TEST_F(StreamVolumeManagerTest, StreamsCanRamp) {
  mock_.usage_ = ToFidlUsage2(RenderUsage::INTERRUPTION);

  manager_.NotifyStreamChanged(&mock_,
                               Ramp{zx::nsec(100), fuchsia::media::audio::RampType::SCALE_LINEAR});

  EXPECT_EQ(mock_.volume_command_.ramp->duration, zx::nsec(100));
  EXPECT_EQ(mock_.volume_command_.ramp->ramp_type, fuchsia::media::audio::RampType::SCALE_LINEAR);
}

TEST_F(StreamVolumeManagerTest, UsageVolumeChangeUpdatesStream) {
  MockStreamVolume media_stream;
  media_stream.usage_ = ToFidlUsage2(RenderUsage::MEDIA);

  MockStreamVolume system_agent_stream;
  system_agent_stream.usage_ = ToFidlUsage2(CaptureUsage::SYSTEM_AGENT);

  manager_.AddStream(&media_stream);
  manager_.AddStream(&system_agent_stream);

  auto media_client = AddClientForUsage(ToFidlUsage2(RenderUsage::MEDIA));
  media_client->SetVolume(0.8f);
  RunLoopUntilIdle();

  EXPECT_FLOAT_EQ(media_stream.volume_command_.volume, 0.8f);
  ASSERT_TRUE(media_stream.volume_command_.ramp.has_value());
  EXPECT_EQ(media_stream.volume_command_.ramp->duration, zx::msec(5));

  EXPECT_FLOAT_EQ(system_agent_stream.volume_command_.volume, 1.0f);
  EXPECT_FALSE(system_agent_stream.volume_command_.ramp.has_value());

  auto system_client = AddClientForUsage(ToFidlUsage2(CaptureUsage::SYSTEM_AGENT));
  system_client->SetVolume(0.9f);
  RunLoopUntilIdle();

  EXPECT_FLOAT_EQ(media_stream.volume_command_.volume, 0.8f);
  ASSERT_TRUE(media_stream.volume_command_.ramp.has_value());
  EXPECT_EQ(media_stream.volume_command_.ramp->duration, zx::msec(5));

  EXPECT_FLOAT_EQ(system_agent_stream.volume_command_.volume, 0.9f);
  ASSERT_FALSE(system_agent_stream.volume_command_.ramp.has_value());
}

TEST_F(StreamVolumeManagerTest, DuplicateUsageGainSettingsIgnored) {
  auto render_usage = ToFidlUsage2(RenderUsage::MEDIA);
  auto capture_usage = ToFidlUsage2(CaptureUsage::SYSTEM_AGENT);

  MockStreamVolume render_stream;
  render_stream.usage_ = fidl::Clone(render_usage);

  MockStreamVolume capture_stream;
  capture_stream.usage_ = fidl::Clone(capture_usage);

  manager_.AddStream(&render_stream);
  manager_.AddStream(&capture_stream);
  RunLoopUntilIdle();
  EXPECT_EQ(1, render_stream.realize_volume_calls_);
  EXPECT_EQ(1, capture_stream.realize_volume_calls_);

  manager_.SetUsageGain(fidl::Clone(render_usage), -10);
  RunLoopUntilIdle();
  EXPECT_EQ(2, render_stream.realize_volume_calls_);

  // No realize volume call if gain is unchanged.
  manager_.SetUsageGain(fidl::Clone(render_usage), -10);
  RunLoopUntilIdle();
  EXPECT_EQ(2, render_stream.realize_volume_calls_);

  manager_.SetUsageGainAdjustment(fidl::Clone(capture_usage), -10);
  RunLoopUntilIdle();
  EXPECT_EQ(2, capture_stream.realize_volume_calls_);

  // No realize volume call if gain adjustment is unchanged.
  manager_.SetUsageGainAdjustment(fidl::Clone(capture_usage), -10);
  RunLoopUntilIdle();
  EXPECT_EQ(2, capture_stream.realize_volume_calls_);
}

}  // namespace
}  // namespace media::audio
