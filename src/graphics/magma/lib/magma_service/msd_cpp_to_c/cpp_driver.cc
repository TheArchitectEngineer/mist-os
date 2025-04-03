// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "cpp_driver.h"

#include <lib/magma/platform/platform_logger.h>
#include <lib/magma/platform/platform_trace.h>
#include <lib/magma/util/macros.h>

#include <mutex>

#include "cpp_buffer.h"
#include "cpp_device.h"
#include "cpp_semaphore.h"

namespace msd {
namespace {

static void log(int32_t level, const char* file, int32_t line, const char* str) {
  magma::PlatformLogger::Log(static_cast<magma::PlatformLogger::LogLevel>(level), file, line, "%s",
                             str);
}

static void trace_vthread_duration(magma_bool_t begin, const char* category, const char* name,
                                   const char* vthread, uint64_t vthread_id, uint64_t timestamp) {
  if (begin) {
    TRACE_VTHREAD_DURATION_BEGIN(category, name, vthread, vthread_id, timestamp);
  } else {
    TRACE_VTHREAD_DURATION_END(category, name, vthread, vthread_id, timestamp);
  }
}

}  // namespace

CppDriver::CppDriver() {
  static std::once_flag flag;

  std::call_once(flag, [] {
    MsdDriverCallbacks callbacks = {
        .log = log,
        .trace_vthread_duration = trace_vthread_duration,
    };
    msd_driver_register_callbacks(&callbacks);
  });
}

std::unique_ptr<msd::Device> CppDriver::CreateDevice(msd::DeviceHandle* device_handle) {
  return std::make_unique<CppDevice>(msd_driver_create_device(device_handle->platform_device));
}

std::unique_ptr<msd::Buffer> CppDriver::ImportBuffer(zx::vmo vmo, uint64_t client_id) {
  struct MsdBuffer* buffer = msd_driver_import_buffer(vmo.release(), client_id);
  if (!buffer)
    return MAGMA_DRETP(nullptr, "msd_driver_import_buffer failed");

  return std::make_unique<CppBuffer>(buffer);
}

magma_status_t CppDriver::ImportSemaphore(zx::handle handle, uint64_t client_id, uint64_t flags,
                                          std::unique_ptr<msd::Semaphore>* out) {
  struct MsdSemaphore* msd_semaphore = nullptr;

  magma_status_t status =
      msd_driver_import_semaphore(handle.release(), client_id, flags, &msd_semaphore);
  if (status != MAGMA_STATUS_OK) {
    return MAGMA_DRET_MSG(status, "msd_driver_import_semaphore failed");
  }

  *out = std::make_unique<CppSemaphore>(msd_semaphore);

  return MAGMA_STATUS_OK;
}
}  // namespace msd

// static
std::unique_ptr<msd::Driver> msd::Driver::Create() { return std::make_unique<CppDriver>(); }
