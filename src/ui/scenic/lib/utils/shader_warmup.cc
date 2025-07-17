// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/utils/shader_warmup.h"

#include "src/ui/lib/escher/escher.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/renderer/sampler_cache.h"

namespace utils {

namespace {

// This list includes some exotic formats based on product needs - for example, to prevent nasty
// gralloc errors in system logs. At this time there is sufficient test coverage to ensure these
// formats are supported on all target platforms; however it's unclear how we would handle a
// platform that does not support one or more formats.
const std::vector<vk::Format> kSupportedClientImageFormats = {
    vk::Format::eR8G8B8A8Srgb,           vk::Format::eB8G8R8A8Srgb,
    vk::Format::eA2B10G10R10UnormPack32, vk::Format::eR8Unorm,
    vk::Format::eG8B8R83Plane420Unorm,   vk::Format::eR5G6B5UnormPack16,
    vk::Format::eG8B8R82Plane420Unorm};

const std::vector<vk::Format> kSupportedClientYuvImageFormats = {vk::Format::eG8B8R83Plane420Unorm,
                                                                 vk::Format::eG8B8R82Plane420Unorm};

}  // namespace

const std::vector<vk::Format>& SupportedClientImageFormats() {
  return kSupportedClientImageFormats;
}

const std::vector<vk::Format>& SupportedClientYuvImageFormats() {
  return kSupportedClientYuvImageFormats;
}

// Helper for ImmutableSamplersForShaderWarmup().
static bool FilterSupportsOptimalTilingForFormat(vk::PhysicalDevice physical_device,
                                                 vk::Filter filter, vk::Format format) {
  vk::FormatFeatureFlagBits feature_flag;
  switch (filter) {
    case vk::Filter::eNearest:
      // eNearest filtering doesn't require a specific feature flag.
      return true;
    case vk::Filter::eLinear:
      feature_flag = vk::FormatFeatureFlagBits::eSampledImageFilterLinear;
      break;
    case vk::Filter::eCubicEXT:
      // eCubicEXT and eCubicIMG are the same (and have the same value).
      static_assert(vk::Filter::eCubicEXT == vk::Filter::eCubicIMG, "");
      feature_flag = vk::FormatFeatureFlagBits::eSampledImageFilterCubicEXT;
      break;
  }
  bool has_support = static_cast<bool>(
      physical_device.getFormatProperties(format).optimalTilingFeatures & feature_flag);

  if (!has_support) {
    FX_LOGS(WARNING) << "Optimal tiling not supported for format=" << vk::to_string(format)
                     << " filter=" << vk::to_string(filter)
                     << ".  Skipping creating immutable sampler.";
  }
  return has_support;
}

std::vector<escher::SamplerPtr> ImmutableSamplersForShaderWarmup(escher::EscherWeakPtr escher,
                                                                 vk::Filter filter) {
  if (!escher->allow_ycbcr()) {
    return {};
  }

  // Generate the list of immutable samples for all of the YUV types that we expect to see.
  std::vector<escher::SamplerPtr> immutable_samplers;
  const std::vector<escher::ColorSpace> color_spaces{
      escher::ColorSpace::kRec709,
      escher::ColorSpace::kRec601Ntsc,
  };
  const auto vk_physical_device = escher->vk_physical_device();
  for (auto fmt : SupportedClientYuvImageFormats()) {
    for (auto color_space : color_spaces) {
      if (escher::impl::IsYuvConversionSupported(vk_physical_device, fmt)) {
        if (FilterSupportsOptimalTilingForFormat(vk_physical_device, filter, fmt)) {
          immutable_samplers.push_back(
              escher->sampler_cache()->ObtainYuvSampler(fmt, filter, color_space));
        }
      } else {
        FX_LOGS(WARNING) << "YUV conversion not supported for format=" << vk::to_string(fmt)
                         << ".  Skipping creating immutable sampler.";
      }
    }
  }

  return immutable_samplers;
}

}  // namespace utils
