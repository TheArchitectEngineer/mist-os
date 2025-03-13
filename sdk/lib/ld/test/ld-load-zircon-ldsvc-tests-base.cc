// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ld-load-zircon-ldsvc-tests-base.h"

#include <lib/elfldltl/vmo.h>
#include <lib/fit/defer.h>
#include <lib/ld/abi.h>
#include <lib/ld/testing/test-elf-object.h>

#include <filesystem>

#include "load-tests.h"

namespace ld::testing {

std::string LdLoadZirconLdsvcTestsBase::FindInterp(zx::unowned_vmo vmo) {
  return ld::testing::FindInterp<elfldltl::UnownedVmoFile>(std::move(vmo));
}

std::optional<std::string> LdLoadZirconLdsvcTestsBase::ConfigFromInterp(  //
    std::filesystem::path interp, std::optional<std::string_view> expected_config) {
  EXPECT_EQ(interp.filename(), abi::kInterp) << interp;

  if (!interp.has_parent_path()) {
    EXPECT_EQ(std::nullopt, expected_config) << interp;
    return std::nullopt;
  }

  std::filesystem::path prefix = interp.parent_path();
  std::optional<std::string> config = prefix;
  if (expected_config) {
    EXPECT_EQ(config, expected_config) << interp;
    return std::nullopt;
  }

  mock_.path_prefix_append(prefix);
  return config;
}

zx::vmo LdLoadZirconLdsvcTestsBase::GetExecutableVmoWithInterpConfig(
    std::string_view executable, std::optional<std::string_view> expected_config) {
  zx::vmo vmo = GetExecutableVmo(executable);
  if (vmo) {
    LdsvcExpectConfig(ConfigFromInterp(vmo.borrow(), expected_config));
  }
  return vmo;
}

zx::vmo LdLoadZirconLdsvcTestsBase::GetInterp(std::string_view executable_name,
                                              std::optional<std::string_view> expected_config) {
  if (!expected_config) {
    return GetLibVmo(abi::kInterp);
  }

  const std::string name_str{executable_name};
  elfldltl::Soname<> set_name{name_str};
  const TestElfLoadSet* load_set = TestElfLoadSet::Get(set_name);
  if (!load_set) {
    ADD_FAILURE() << "load set " << set_name << " not found";
    return {};
  }
  const TestElfLoadSet::SonameMap soname_map = load_set->MakeSonameMap();

  auto it = soname_map.find(abi::Abi<>::kSoname);
  if (it == soname_map.end()) {
    ADD_FAILURE() << abi::Abi<>::kSoname << " not in load set " << set_name;
    return {};
  }
  std::optional libprefix = it->second.libprefix;
  if (!libprefix) {
    return GetLibVmo(abi::kInterp);
  }

  std::filesystem::path interp{*libprefix};
  interp /= abi::kInterp;
  return GetLibVmo(interp.native());
}

void LdLoadZirconLdsvcTestsBase::NeededViaLoadSet(  //
    elfldltl::Soname<> set_name, std::initializer_list<std::string_view> names) {
  auto restore_prefix = fit::defer([this, path_prefix = mock_.path_prefix()]() mutable {
    mock_.set_path_prefix(std::move(path_prefix));
  });
  const TestElfLoadSet* load_set = TestElfLoadSet::Get(set_name);
  ASSERT_TRUE(load_set) << set_name;
  const TestElfLoadSet::SonameMap soname_map = load_set->MakeSonameMap();
  for (std::string_view dep : names) {
    auto it = soname_map.find(elfldltl::Soname<>{std::string{dep}});
    ASSERT_NE(it, soname_map.end()) << dep << " for " << set_name;
    LdsvcPathPrefix(set_name.str(), it->second.libprefix);
    ASSERT_NO_FATAL_FAILURE(LdsvcExpectDependency(dep));
  }
}

}  // namespace ld::testing
