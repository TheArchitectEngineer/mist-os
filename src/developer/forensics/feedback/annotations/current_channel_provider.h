// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_CURRENT_CHANNEL_PROVIDER_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_CURRENT_CHANNEL_PROVIDER_H_

#include <fuchsia/update/channelcontrol/cpp/fidl.h>

#include "src/developer/forensics/feedback/annotations/fidl_provider.h"
#include "src/developer/forensics/feedback/annotations/types.h"

namespace forensics::feedback {

struct CurrentChannelToAnnotations {
  Annotations operator()(const std::string& current_channel);
};

// Responsible for collecting annotations for
// fuchsia.update.channelcontrol/ChannelControl::GetCurrent.
class CurrentChannelProvider : public StaticSingleFidlMethodAnnotationProvider<
                                   fuchsia::update::channelcontrol::ChannelControl,
                                   &fuchsia::update::channelcontrol::ChannelControl::GetCurrent,
                                   CurrentChannelToAnnotations> {
 public:
  using StaticSingleFidlMethodAnnotationProvider::StaticSingleFidlMethodAnnotationProvider;

  virtual ~CurrentChannelProvider() = default;

  static std::set<std::string> GetAnnotationKeys();
  std::set<std::string> GetKeys() const override;
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_CURRENT_CHANNEL_PROVIDER_H_
