// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DL_RUNTIME_DYNAMIC_LINKER_H_
#define LIB_DL_RUNTIME_DYNAMIC_LINKER_H_

#include <dlfcn.h>  // for RTLD_* macros
#include <lib/elfldltl/soname.h>
#include <lib/fit/result.h>

#include <fbl/intrusive_double_list.h>

#include "diagnostics.h"
#include "error.h"
#include "linking-session.h"
#include "runtime-module.h"

namespace dl {

using size_type = Elf::size_type;
using DlIteratePhdrCallback = int(dl_phdr_info*, size_t, void*);

enum OpenSymbolScope : int {
  kLocal = RTLD_LOCAL,
  kGlobal = RTLD_GLOBAL,
};

enum OpenBindingMode : int {
  kNow = RTLD_NOW,
  // RTLD_LAZY functionality is not supported, but keep the flag definition
  // because it's a legitimate flag that can be passed in.
  kLazy = RTLD_LAZY,
};

enum OpenFlags : int {
  kNoload = RTLD_NOLOAD,
  kNodelete = RTLD_NODELETE,
  // TODO(https://fxbug.dev/323425900): support glibc's RTLD_DEEPBIND flag.
  // kDEEPBIND = RTLD_DEEPBIND,
};

// Masks used to validate flag values.
inline constexpr int kOpenSymbolScopeMask = OpenSymbolScope::kLocal | OpenSymbolScope::kGlobal;
inline constexpr int kOpenBindingModeMask = OpenBindingMode::kLazy | OpenBindingMode::kNow;
inline constexpr int kOpenFlagsMask = OpenFlags::kNoload | OpenFlags::kNodelete;

class RuntimeDynamicLinker {
 public:
  using Soname = elfldltl::Soname<>;

  // Create a RuntimeDynamicLinker with the passed in passive `abi`. The caller
  // is required to pass an AllocChecker and check it to verify the
  // RuntimeDynamicLinker was created and initialized successfully.
  static std::unique_ptr<RuntimeDynamicLinker> Create(const ld::abi::Abi<>& abi,
                                                      fbl::AllocChecker& ac);

  constexpr const ModuleList& modules() const { return modules_; }

  size_t max_static_tls_modid() const { return max_static_tls_modid_; }

  // Lookup a symbol from the given module, returning a pointer to it in memory,
  // or an error if not found (ie undefined symbol).
  fit::result<Error, void*> LookupSymbol(const RuntimeModule& root, const char* ref);

  // - TODO(https://fxbug.dev/339037138): Add a test exercising the system error
  // case and include it as an example for the fit::error{Error} description.

  // Open `file` with the given `mode`, returning a pointer to the loaded module
  // for the file. The `retrieve_file` argument is to the LinkingSession and
  // is called as a `fit::result<std::optional<Error>, File>(Diagnostics&, std::string_view)`
  // with the following semantics:
  //   - fit::error{std::nullopt} is a not found error
  //   - fit::error{Error} is an error type that can be passed to
  //     Diagnostics::SystemError (see <lib/elfldltl/diagnostics.h>) to give
  //     more context to the error message.
  //   - fit::ok{File} is the found elfldltl File API type for the module
  //     (see <lib/elfldltl/memory.h>).
  // The Diagnostics reference passed to `retrieve_file` is not used by the
  // function itself to report its errors, but is plumbed into the created File
  // API object that will use it for reporting file read errors.
  template <class Loader, typename RetrieveFile>
  fit::result<Error, void*> Open(const char* file, int mode, RetrieveFile&& retrieve_file) {
    // `mode` must be a valid value.
    if (mode & ~(kOpenSymbolScopeMask | kOpenBindingModeMask | kOpenFlagsMask)) {
      return fit::error{Error{"invalid mode parameter"}};
    }

    if (!file || !strlen(file)) {
      return fit::error{
          Error{"TODO(https://fxbug.dev/361674544): nullptr for file is unsupported."}};
    }

    // Use a non-scoped diagnostics object for the root module. Because errors
    // are generated on this module directly, its name does not need to be
    // prefixed to the error, as is the case using ld::ScopedModuleDiagnostics.
    dl::Diagnostics diag;

    Soname name{file};
    // If a module for this file is already loaded, return a reference to it.
    // Update its global visibility if dlopen(...RTLD_GLOBAL) was passed.
    if (RuntimeModule* found = FindModule(name)) {
      if (!found->ReifyModuleTree(diag)) {
        return diag.take_error();
      }
      if (mode & OpenSymbolScope::kGlobal) {
        MakeGlobal(found->module_tree());
      }
      return diag.ok(found);
    }

    if (mode & OpenFlags::kNoload) {
      return diag.ok(nullptr);
    }

    // A Module for `file` does not yet exist; create a new LinkingSession
    // to perform the loading and linking of the file and all its dependencies.
    LinkingSession<Loader> linking_session{modules_, max_static_tls_modid_, max_tls_modid_};

    if (!linking_session.Link(diag, name, std::forward<RetrieveFile>(retrieve_file))) {
      return diag.take_error();
    }

    // Commit the linking session and its mapped modules.
    LinkingResult result = std::move(linking_session).Commit();

    // The max_tls_modid from the LinkingResult should be an updated counter
    // of any new TLS modules that were loaded.
    assert(result.max_tls_modid >= max_tls_modid_);
    assert(result.max_tls_modid >= max_static_tls_modid_);
    max_tls_modid_ = result.max_tls_modid;

    // Obtain a reference to the root module for the dlopen-ed file to return
    // back to the caller.
    RuntimeModule& root_module = result.loaded_modules.front();

    // After successful loading and relocation, append the new permanent modules
    // created by the linking session to the dynamic linker's module list.
    AddNewModules(std::move(result.loaded_modules));

    // If RTLD_GLOBAL was passed, make the module and all of its dependencies
    // global. This is done after modules from the linking session have been
    // added to the modules_ list, because this operation may change the
    // ordering of all loaded modules.
    if (mode & OpenSymbolScope::kGlobal) {
      MakeGlobal(root_module.module_tree());
    }

    return diag.ok(&root_module);
  }

  // Create a `dl_phdr_info` for each module in `modules_` and pass it
  // to the caller-supplied `callback`. Iteration ceases when `callback` returns
  // a non-zero value. The result of the last callback function to run is
  // returned to the caller.
  int IteratePhdrInfo(DlIteratePhdrCallback* callback, void* data) const;

  // Allocate and initialize the thread's dynamic TLS blocks. This will iterate
  // through all the currently loaded modules with dynamic TLS and populate this
  // thread's _dl_tlsdesc_runtime_dynamic_blocks variable with their TLS data.
  // This function will fail if allocation fails.
  [[nodiscard]] fit::result<Error> PrepareTlsBlocksForThread(void* tp) const;

  // The number of dynamic TLS modules that are loaded.
  size_t DynamicTlsCount() const { return max_tls_modid_ - max_static_tls_modid_; }

 private:
  // A The RuntimeDynamicLinker can only be created with RuntimeDynamicLinker::Create...).
  RuntimeDynamicLinker() = default;

  // Append new modules to the end of the `modules_`.
  void AddNewModules(ModuleList modules);

  // Attempt to find the loaded module with the given name, returning a nullptr
  // if the module was not found.
  RuntimeModule* FindModule(Soname name);

  // Apply RTLD_GLOBAL to any module that is not already global in the provided
  // `module_tree`. When a module is promoted to global, its load order in the
  // dynamic linker's modules_ list changes: it is moved to the back of the
  // list, as if it was just loaded with RTLD_GLOBAL.
  void MakeGlobal(const ModuleTree& module_tree);

  // Create RuntimeModule data structures from the passive ABI and add them to
  // the dynamic linker's modules_ list. The caller is required to pass an
  // AllocChecker and check it to verify the success/failure of loading the
  // passive ABI into the RuntimeDynamicLinker.
  void PopulateStartupModules(fbl::AllocChecker& ac, const ld::abi::Abi<>& abi);

  ld::DlPhdrInfoCounts dl_phdr_info_counts() const {
    return {.adds = loaded_, .subs = loaded_ - modules_.size()};
  }

  // Return a pointer to the beginning of a module's static or dynamic TLS block.
  void* TlsBlock(const RuntimeModule& module) const;

  // The RuntimeDynamicLinker owns the list of all 'live' modules that have been
  // loaded into the system image.
  ModuleList modules_;

  // The maximum static TLS module id is taken from the ld::abi::Abi<> at
  // creation and passed to LinkinSessions to be able to detect TLS modules
  // during relocation.
  size_type max_static_tls_modid_ = 0;

  // The maximum TLS modid assigned to a module in modules_. This value
  // describes the number of static and dynamic TLS modules that are currently
  // loaded. This gets set to max_static_tls_modid_ when startup TLS modules are
  // loaded and gets incremented when a new dynamic TLS module is dlopen-ed.
  size_type max_tls_modid_ = 0;

  // This is incremented every time a module is loaded into the system. This
  // number only ever increases and includes startup modules.
  size_t loaded_ = 0;
};

}  // namespace dl

#endif  // LIB_DL_RUNTIME_DYNAMIC_LINKER_H_
