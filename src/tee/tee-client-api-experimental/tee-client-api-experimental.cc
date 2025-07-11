// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.tee/cpp/fidl.h>
#include <fidl/fuchsia.hardware.tee/cpp/wire.h>
#include <fidl/fuchsia.tee/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <lib/fit/thread_safety.h>
#include <lib/zx/channel.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <tee_client_api.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>

#include <cstdint>
#include <cstring>
#include <mutex>
#include <string_view>
#include <tuple>
#include <unordered_map>
#include <utility>

#include "src/lib/fxl/strings/string_printf.h"
#include "tee-client-api/tee-client-types.h"

// Explicit instantiation to enable std associative containers with a Uuid key
// type.
template <>
struct std::hash<fuchsia_tee::wire::Uuid> {
  size_t operator()(const fuchsia_tee::wire::Uuid& uuid) const {
    size_t hash = 0;
    HashAndCombine(hash, uuid.time_low);
    HashAndCombine(hash, uuid.time_mid);
    HashAndCombine(hash, uuid.time_hi_and_version);
    for (size_t i = 0; i < 8; ++i) {
      HashAndCombine(hash, uuid.clock_seq_and_node[i]);
    }
    return hash;
  }

  // Adapted from: https://www.boost.org/doc/libs/1_64_0/boost/functional/hash/hash.hpp.
  template <class T>
  static void HashAndCombine(size_t& seed, const T& v) {
    std::hash<T> hasher;
    seed ^= hasher(v) + 0x9e3779b9 + (seed << 6) + (seed >> 2);
  }
};

namespace {

struct UuidEqualityComparator {
  bool operator()(const fuchsia_tee::wire::Uuid& lhs, const fuchsia_tee::wire::Uuid& rhs) const {
    if (lhs.time_low != rhs.time_low) {
      return false;
    }
    if (lhs.time_mid != rhs.time_mid) {
      return false;
    }
    if (lhs.time_hi_and_version != rhs.time_hi_and_version) {
      return false;
    }

    if (!std::equal(lhs.clock_seq_and_node.begin(), lhs.clock_seq_and_node.end(),
                    rhs.clock_seq_and_node.begin(), rhs.clock_seq_and_node.end())) {
      return false;
    }

    return true;
  }
};

// A basic thread-safe, UUID-associative container for application endpoints
// that backs the context implementation.
class AppContainer {
 public:
  using Uuid = fuchsia_tee::wire::Uuid;

  static void InitInContext(TEEC_Context& context) {
    context.imp.uuid_to_channel = new AppContainer();
  }

  static AppContainer* FromContext(TEEC_Context& context) {
    return reinterpret_cast<AppContainer*>(context.imp.uuid_to_channel);
  }

  [[nodiscard]] std::lock_guard<std::mutex> lock() FIT_ACQUIRE(m_) { return std::lock_guard{m_}; }

  // Attempts to connect to the associated app.
  zx::result<fidl::UnownedClientEnd<fuchsia_tee::Application>> Connect(const Uuid& uuid)
      FIT_REQUIRES(m_) {
    auto it = apps_.find(uuid);
    if (it == apps_.end()) {
      auto result = component::Connect<fuchsia_tee::Application>(AppConnectionPath(uuid));
      if (result.is_error()) {
        return result.take_error();
      }
      std::tie(it, std::ignore) = apps_.emplace(uuid, std::move(result).value());
    }
    return zx::ok(it->second.borrow());
  }

  void Delete(const Uuid& uuid) {
    std::lock_guard lock(m_);
    auto it = apps_.find(uuid);
    ZX_ASSERT(it != apps_.end());
    apps_.erase(it);
  }

 private:
  static std::string AppConnectionPath(const Uuid& uuid) {
    constexpr const char* kPathFormat = "/ta/%08x-%04x-%04x-%02x%02x-%02x%02x%02x%02x%02x%02x/%s";

    return fxl::StringPrintf(kPathFormat, uuid.time_low, uuid.time_mid, uuid.time_hi_and_version,
                             uuid.clock_seq_and_node[0], uuid.clock_seq_and_node[1],
                             uuid.clock_seq_and_node[2], uuid.clock_seq_and_node[3],
                             uuid.clock_seq_and_node[4], uuid.clock_seq_and_node[5],
                             uuid.clock_seq_and_node[6], uuid.clock_seq_and_node[7],
                             fidl::DiscoverableProtocolName<fuchsia_tee::Application>);
  }
  std::mutex m_;
  std::unordered_map<Uuid, fidl::ClientEnd<fuchsia_tee::Application>, std::hash<Uuid>,
                     UuidEqualityComparator>
      apps_ FIT_GUARDED(m_);
};

constexpr uint32_t GetParamTypeForIndex(uint32_t param_types, size_t index) {
  constexpr uint32_t kBitsPerParamType = 4;
  return ((param_types >> (index * kBitsPerParamType)) & 0xF);
}

constexpr bool IsSharedMemFlagInOut(uint32_t flags) {
  constexpr uint32_t kInOutFlags = TEEC_MEM_INPUT | TEEC_MEM_OUTPUT;
  return (flags & kInOutFlags) == kInOutFlags;
}

constexpr bool IsDirectionInput(fuchsia_tee::wire::Direction direction) {
  return ((direction == fuchsia_tee::wire::Direction::kInput) ||
          (direction == fuchsia_tee::wire::Direction::kInout));
}

constexpr bool IsDirectionOutput(fuchsia_tee::wire::Direction direction) {
  return ((direction == fuchsia_tee::wire::Direction::kOutput) ||
          (direction == fuchsia_tee::wire::Direction::kInout));
}

void ConvertTeecUuidToZxUuid(const TEEC_UUID& teec_uuid, fuchsia_tee::wire::Uuid* out_uuid) {
  ZX_DEBUG_ASSERT(out_uuid);

  out_uuid->time_low = teec_uuid.timeLow;
  out_uuid->time_mid = teec_uuid.timeMid;
  out_uuid->time_hi_and_version = teec_uuid.timeHiAndVersion;

  std::memcpy(out_uuid->clock_seq_and_node.data(), teec_uuid.clockSeqAndNode,
              sizeof(out_uuid->clock_seq_and_node));
}

constexpr TEEC_Result ConvertStatusToResult(zx_status_t status) {
  switch (status) {
    case ZX_ERR_PEER_CLOSED:
      return TEEC_ERROR_COMMUNICATION;
    case ZX_ERR_INVALID_ARGS:
      return TEEC_ERROR_BAD_PARAMETERS;
    case ZX_ERR_NOT_SUPPORTED:
      return TEEC_ERROR_NOT_SUPPORTED;
    case ZX_ERR_NO_MEMORY:
      return TEEC_ERROR_OUT_OF_MEMORY;
    case ZX_OK:
      return TEEC_SUCCESS;
  }
  return TEEC_ERROR_GENERIC;
}

constexpr uint32_t ConvertZxToTeecReturnOrigin(fuchsia_tee::wire::ReturnOrigin return_origin) {
  switch (return_origin) {
    case fuchsia_tee::wire::ReturnOrigin::kCommunication:
      return TEEC_ORIGIN_COMMS;
    case fuchsia_tee::wire::ReturnOrigin::kTrustedOs:
      return TEEC_ORIGIN_TEE;
    case fuchsia_tee::wire::ReturnOrigin::kTrustedApplication:
      return TEEC_ORIGIN_TRUSTED_APP;
    default:
      return TEEC_ORIGIN_API;
  }
}

constexpr size_t CountOperationParameters(const TEEC_Operation& operation) {
  // Find the highest-indexed non-none parameter.
  for (size_t param_num = static_cast<size_t>(TEEC_NUM_PARAMS_MAX); param_num != 0; param_num--) {
    uint32_t param_type = GetParamTypeForIndex(operation.paramTypes, param_num - 1);
    if (param_type != TEEC_NONE) {
      return param_num;
    }
  }

  return 0;
}

zx_status_t CreateVmoWithName(uint64_t size, uint32_t options, std::string_view name,
                              zx::vmo* result) {
  ZX_DEBUG_ASSERT(result);

  zx::vmo vmo;
  zx_status_t s = zx::vmo::create(size, options, &vmo);
  if (s != ZX_OK) {
    return s;
  }

  s = vmo.set_property(ZX_PROP_NAME, name.data(), name.size());
  if (s != ZX_OK) {
    return s;
  }
  *result = std::move(vmo);
  return s;
}

void PreprocessValue(fidl::AnyArena& allocator, uint32_t param_type, const TEEC_Value& teec_value,
                     fuchsia_tee::wire::Parameter* out_parameter) {
  ZX_DEBUG_ASSERT(out_parameter);

  fuchsia_tee::wire::Direction direction;
  switch (param_type) {
    case TEEC_VALUE_INPUT:
      direction = fuchsia_tee::wire::Direction::kInput;
      break;
    case TEEC_VALUE_OUTPUT:
      direction = fuchsia_tee::wire::Direction::kOutput;
      break;
    case TEEC_VALUE_INOUT:
      direction = fuchsia_tee::wire::Direction::kInout;
      break;
    default:
      ZX_PANIC("Unknown param type");
  }

  auto value = fuchsia_tee::wire::Value::Builder(allocator);
  value.direction(direction);
  if (IsDirectionInput(direction)) {
    // The TEEC_Value type only includes two generic fields, whereas the Fuchsia TEE interface
    // supports three. The c field cannot be used by the TEE Client API.
    value.a(teec_value.a);
    value.b(teec_value.b);
  }

  *out_parameter = fuchsia_tee::wire::Parameter::WithValue(allocator, value.Build());
}

TEEC_Result PreprocessTemporaryMemref(fidl::AnyArena& allocator, uint32_t param_type,
                                      const TEEC_TempMemoryReference& temp_memory_ref,
                                      fuchsia_tee::wire::Parameter* out_parameter) {
  ZX_DEBUG_ASSERT(out_parameter);

  fuchsia_tee::wire::Direction direction;
  switch (param_type) {
    case TEEC_MEMREF_TEMP_INPUT:
      direction = fuchsia_tee::wire::Direction::kInput;
      break;
    case TEEC_MEMREF_TEMP_OUTPUT:
      direction = fuchsia_tee::wire::Direction::kOutput;
      break;
    case TEEC_MEMREF_TEMP_INOUT:
      direction = fuchsia_tee::wire::Direction::kInout;
      break;
    default:
      ZX_PANIC("TEE Client API Unknown parameter type");
  }

  zx::vmo vmo;

  if (temp_memory_ref.buffer) {
    // We either have data to input or have a buffer to output data to, so create a VMO for it.
    zx_status_t status = CreateVmoWithName(temp_memory_ref.size, 0, "teec_temp_memory", &vmo);
    if (status != ZX_OK) {
      return ConvertStatusToResult(status);
    }

    // If the memory reference is used as an input, then we must copy the data from the user
    // provided buffer into the VMO. There is no need to do this for parameters that are output
    // only.
    if (IsDirectionInput(direction)) {
      status = vmo.write(temp_memory_ref.buffer, 0, temp_memory_ref.size);
      if (status != ZX_OK) {
        return ConvertStatusToResult(status);
      }
    }
  }

  auto buffer = fuchsia_tee::wire::Buffer::Builder(allocator);
  buffer.direction(direction);
  if (vmo.is_valid()) {
    buffer.vmo(std::move(vmo));
  }
  buffer.offset(0);
  buffer.size(temp_memory_ref.size);

  *out_parameter = fuchsia_tee::wire::Parameter::WithBuffer(allocator, buffer.Build());
  return TEEC_SUCCESS;
}

TEEC_Result PreprocessWholeMemref(fidl::AnyArena& allocator,
                                  const TEEC_RegisteredMemoryReference& memory_ref,
                                  fuchsia_tee::wire::Parameter* out_parameter) {
  ZX_DEBUG_ASSERT(out_parameter);

  if (!memory_ref.parent) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  TEEC_SharedMemory* shared_mem = memory_ref.parent;
  fuchsia_tee::wire::Direction direction;
  if (IsSharedMemFlagInOut(shared_mem->flags)) {
    direction = fuchsia_tee::wire::Direction::kInout;
  } else if (shared_mem->flags & TEEC_MEM_INPUT) {
    direction = fuchsia_tee::wire::Direction::kInput;
  } else if (shared_mem->flags & TEEC_MEM_OUTPUT) {
    direction = fuchsia_tee::wire::Direction::kOutput;
  } else {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  zx::vmo vmo;
  zx_status_t status = zx::unowned_vmo(shared_mem->imp.vmo)->duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo);
  if (status != ZX_OK) {
    return ConvertStatusToResult(status);
  }

  auto buffer = fuchsia_tee::wire::Buffer::Builder(allocator);
  buffer.direction(direction);
  buffer.vmo(std::move(vmo));
  buffer.offset(0);
  buffer.size(shared_mem->size);

  *out_parameter = fuchsia_tee::wire::Parameter::WithBuffer(allocator, buffer.Build());
  return TEEC_SUCCESS;
}

TEEC_Result PreprocessPartialMemref(fidl::AnyArena& allocator, uint32_t param_type,
                                    const TEEC_RegisteredMemoryReference& memory_ref,
                                    fuchsia_tee::wire::Parameter* out_parameter) {
  ZX_DEBUG_ASSERT(out_parameter);

  if (!memory_ref.parent) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  uint32_t expected_shm_flags = 0;
  fuchsia_tee::wire::Direction direction;
  switch (param_type) {
    case TEEC_MEMREF_PARTIAL_INPUT:
      expected_shm_flags = TEEC_MEM_INPUT;
      direction = fuchsia_tee::wire::Direction::kInput;
      break;
    case TEEC_MEMREF_PARTIAL_OUTPUT:
      expected_shm_flags = TEEC_MEM_OUTPUT;
      direction = fuchsia_tee::wire::Direction::kOutput;
      break;
    case TEEC_MEMREF_PARTIAL_INOUT:
      expected_shm_flags = TEEC_MEM_INPUT | TEEC_MEM_OUTPUT;
      direction = fuchsia_tee::wire::Direction::kInout;
      break;
    default:
      ZX_DEBUG_ASSERT(param_type == TEEC_MEMREF_PARTIAL_INPUT ||
                      param_type == TEEC_MEMREF_PARTIAL_OUTPUT ||
                      param_type == TEEC_MEMREF_PARTIAL_INOUT);
  }

  TEEC_SharedMemory* shared_mem = memory_ref.parent;

  if ((shared_mem->flags & expected_shm_flags) != expected_shm_flags) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  zx::vmo vmo;
  zx_status_t status = zx::unowned_vmo(shared_mem->imp.vmo)->duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo);
  if (status != ZX_OK) {
    return ConvertStatusToResult(status);
  }

  auto buffer = fuchsia_tee::wire::Buffer::Builder(allocator);
  buffer.direction(direction);
  buffer.vmo(std::move(vmo));
  buffer.offset(memory_ref.offset);
  buffer.size(memory_ref.size);

  *out_parameter = fuchsia_tee::wire::Parameter::WithBuffer(allocator, buffer.Build());
  return TEEC_SUCCESS;
}

TEEC_Result PreprocessOperation(fidl::AnyArena& allocator, const TEEC_Operation* operation,
                                fidl::VectorView<fuchsia_tee::wire::Parameter>* out_parameter_set) {
  ZX_DEBUG_ASSERT(out_parameter_set);

  if (!operation) {
    out_parameter_set->Allocate(allocator, 0);
    return TEEC_SUCCESS;
  }

  size_t num_params = CountOperationParameters(*operation);
  out_parameter_set->Allocate(allocator, num_params);

  TEEC_Result rc = TEEC_SUCCESS;
  for (size_t i = 0; i < num_params; i++) {
    uint32_t param_type = GetParamTypeForIndex(operation->paramTypes, i);
    fuchsia_tee::wire::Parameter& parameter = (*out_parameter_set)[i];

    switch (param_type) {
      case TEEC_NONE:
        parameter = fuchsia_tee::wire::Parameter::WithNone({});
        break;
      case TEEC_VALUE_INPUT:
      case TEEC_VALUE_OUTPUT:
      case TEEC_VALUE_INOUT:
        PreprocessValue(allocator, param_type, operation->params[i].value, &parameter);
        break;
      case TEEC_MEMREF_TEMP_INPUT:
      case TEEC_MEMREF_TEMP_OUTPUT:
      case TEEC_MEMREF_TEMP_INOUT:
        rc = PreprocessTemporaryMemref(allocator, param_type, operation->params[i].tmpref,
                                       &parameter);
        break;
      case TEEC_MEMREF_WHOLE:
        rc = PreprocessWholeMemref(allocator, operation->params[i].memref, &parameter);
        break;
      case TEEC_MEMREF_PARTIAL_INPUT:
      case TEEC_MEMREF_PARTIAL_OUTPUT:
      case TEEC_MEMREF_PARTIAL_INOUT:
        rc =
            PreprocessPartialMemref(allocator, param_type, operation->params[i].memref, &parameter);
        break;
      default:
        rc = TEEC_ERROR_BAD_PARAMETERS;
        break;
    }

    if (rc != TEEC_SUCCESS) {
      return rc;
    }
  }

  return rc;
}

TEEC_Result PostprocessValue(uint32_t param_type, const fuchsia_tee::wire::Parameter& zx_param,
                             TEEC_Value* out_teec_value) {
  ZX_DEBUG_ASSERT(out_teec_value);
  // Input parameters are expected to be ignored after a TA operation.
  ZX_DEBUG_ASSERT(param_type == TEEC_VALUE_OUTPUT || param_type == TEEC_VALUE_INOUT);

  if (zx_param.Which() != fuchsia_tee::wire::Parameter::Tag::kValue) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  const fuchsia_tee::wire::Value& zx_value = zx_param.value();
  if (!zx_value.has_direction()) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  // Validate that the direction of the returned parameter matches the expected.
  if ((param_type == TEEC_VALUE_OUTPUT) &&
      (zx_value.direction() != fuchsia_tee::wire::Direction::kOutput)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_VALUE_INOUT) &&
      (zx_value.direction() != fuchsia_tee::wire::Direction::kInout)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (IsDirectionOutput(zx_value.direction())) {
    if (!zx_value.has_a() || !zx_value.has_b()) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }

    // The TEEC_Value type only includes two generic fields, whereas the Fuchsia TEE interface
    // supports three. The c field cannot be used by the TEE Client API.
    out_teec_value->a = static_cast<uint32_t>(zx_value.a());
    out_teec_value->b = static_cast<uint32_t>(zx_value.b());
  }
  return TEEC_SUCCESS;
}

TEEC_Result PostprocessTemporaryMemref(uint32_t param_type,
                                       const fuchsia_tee::wire::Parameter& zx_param,
                                       TEEC_TempMemoryReference* out_temp_memory_ref) {
  ZX_DEBUG_ASSERT(out_temp_memory_ref);
  // Input parameters are expected to be ignored after a TA operation.
  ZX_DEBUG_ASSERT(param_type == TEEC_MEMREF_TEMP_OUTPUT || param_type == TEEC_MEMREF_TEMP_INOUT);

  if (zx_param.Which() != fuchsia_tee::wire::Parameter::Tag::kBuffer) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  const fuchsia_tee::wire::Buffer& zx_buffer = zx_param.buffer();
  if (!zx_buffer.has_direction()) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if ((param_type == TEEC_MEMREF_TEMP_OUTPUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::kOutput)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_MEMREF_TEMP_INOUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::kInout)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  TEEC_Result rc = TEEC_SUCCESS;
  if (IsDirectionOutput(zx_buffer.direction())) {
    // For output buffers, if we don't have enough space in the temporary memory reference to
    // copy the data out, we still need to update the size to indicate to the user how large of
    // a buffer they need to perform the requested operation.
    if (!zx_buffer.has_size()) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }

    if (out_temp_memory_ref->buffer && out_temp_memory_ref->size >= zx_buffer.size()) {
      if (!zx_buffer.has_offset() || !zx_buffer.has_vmo()) {
        return TEEC_ERROR_BAD_PARAMETERS;
      }

      zx_status_t status =
          zx_buffer.vmo().read(out_temp_memory_ref->buffer, zx_buffer.offset(), zx_buffer.size());
      rc = ConvertStatusToResult(status);
    }
    out_temp_memory_ref->size = zx_buffer.size();
  }

  return rc;
}

TEEC_Result PostprocessWholeMemref(const fuchsia_tee::wire::Parameter& zx_param,
                                   TEEC_RegisteredMemoryReference* out_memory_ref) {
  ZX_DEBUG_ASSERT(out_memory_ref);
  ZX_DEBUG_ASSERT(out_memory_ref->parent);

  if (zx_param.Which() != fuchsia_tee::wire::Parameter::Tag::kBuffer) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  const fuchsia_tee::wire::Buffer& zx_buffer = zx_param.buffer();
  if (!zx_buffer.has_direction()) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (IsDirectionOutput(zx_buffer.direction())) {
    if (!zx_buffer.has_size()) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }
    out_memory_ref->size = zx_buffer.size();
  }

  return TEEC_SUCCESS;
}

TEEC_Result PostprocessPartialMemref(uint32_t param_type,
                                     const fuchsia_tee::wire::Parameter& zx_param,
                                     TEEC_RegisteredMemoryReference* out_memory_ref) {
  ZX_DEBUG_ASSERT(out_memory_ref);
  // Input parameters are expected to be ignored after a TA operation.
  ZX_DEBUG_ASSERT(param_type == TEEC_MEMREF_PARTIAL_OUTPUT ||
                  param_type == TEEC_MEMREF_PARTIAL_INOUT);

  if (zx_param.Which() != fuchsia_tee::wire::Parameter::Tag::kBuffer) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  const fuchsia_tee::wire::Buffer& zx_buffer = zx_param.buffer();
  if (!zx_buffer.has_direction()) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if ((param_type == TEEC_MEMREF_PARTIAL_OUTPUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::kOutput)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  if ((param_type == TEEC_MEMREF_PARTIAL_INOUT) &&
      (zx_buffer.direction() != fuchsia_tee::wire::Direction::kInout)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (IsDirectionOutput(zx_buffer.direction())) {
    if (!zx_buffer.has_size()) {
      return TEEC_ERROR_BAD_PARAMETERS;
    }
    out_memory_ref->size = zx_buffer.size();
  }

  return TEEC_SUCCESS;
}

TEEC_Result PostprocessOperation(
    const fidl::VectorView<fuchsia_tee::wire::Parameter>& parameter_set,
    TEEC_Operation* out_operation) {
  if (!out_operation) {
    return TEEC_SUCCESS;
  }

  // The runtime is supposed to ignore returned input parameters, so the
  // returned list of parameter structures may be less than those originally
  // be provided to the operation (e.g., in stripping trailing input
  // parameters). At least check that this number isn't somehow now greater.
  if (parameter_set.size() > CountOperationParameters(*out_operation)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  TEEC_Result rc = TEEC_SUCCESS;
  for (size_t i = 0; i < parameter_set.size(); i++) {
    uint32_t param_type = GetParamTypeForIndex(out_operation->paramTypes, i);

    switch (param_type) {
      // Input parameters are expected to be ignored after a TA operation.
      case TEEC_VALUE_INPUT:
      case TEEC_MEMREF_TEMP_INPUT:
      case TEEC_MEMREF_PARTIAL_INPUT:
        break;
      case TEEC_NONE:
        if (parameter_set[i].Which() != fuchsia_tee::wire::Parameter::Tag::kNone) {
          rc = TEEC_ERROR_BAD_PARAMETERS;
        }
        break;
      case TEEC_VALUE_OUTPUT:
      case TEEC_VALUE_INOUT:
        rc = PostprocessValue(param_type, parameter_set[i], &out_operation->params[i].value);
        break;
      case TEEC_MEMREF_TEMP_OUTPUT:
      case TEEC_MEMREF_TEMP_INOUT:
        rc = PostprocessTemporaryMemref(param_type, parameter_set[i],
                                        &out_operation->params[i].tmpref);
        break;
      case TEEC_MEMREF_WHOLE:
        rc = PostprocessWholeMemref(parameter_set[i], &out_operation->params[i].memref);
        break;
      case TEEC_MEMREF_PARTIAL_OUTPUT:
      case TEEC_MEMREF_PARTIAL_INOUT:
        rc = PostprocessPartialMemref(param_type, parameter_set[i],
                                      &out_operation->params[i].memref);
        break;
      default:
        rc = TEEC_ERROR_BAD_PARAMETERS;
    }

    if (rc != TEEC_SUCCESS) {
      break;
    }
  }

  return rc;
}

fidl::UnownedClientEnd<fuchsia_tee::Application> GetApplicationFromSession(TEEC_Session* session) {
  ZX_DEBUG_ASSERT(session);
  return fidl::UnownedClientEnd<fuchsia_tee::Application>(session->imp.application_channel);
}

TEEC_Result ConnectApplication(const fuchsia_tee::wire::Uuid& uuid, TEEC_Context* context,
                               fidl::UnownedClientEnd<fuchsia_tee::Application>* out_app) {
  ZX_DEBUG_ASSERT(context);
  ZX_DEBUG_ASSERT(out_app);

  AppContainer* apps = AppContainer::FromContext(*context);
  if (apps == nullptr) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  std::lock_guard lock = apps->lock();
  zx::result result = apps->Connect(uuid);
  if (result.is_error()) {
    return TEEC_ERROR_COMMUNICATION;
  }
  *out_app = result.value();
  return TEEC_SUCCESS;
}

}  // namespace

__EXPORT
TEEC_Result TEEC_InitializeContext(const char* name, TEEC_Context* context) {
  if (!context) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }
  fidl::ClientEnd<fuchsia_hardware_tee::DeviceConnector> device_connector;
  context->imp.device_connector_channel = device_connector.TakeChannel().release();
  AppContainer::InitInContext(*context);
  return TEEC_SUCCESS;
}

__EXPORT
void TEEC_FinalizeContext(TEEC_Context* context) {
  if (context) {
    zx_handle_close(context->imp.device_connector_channel);
    context->imp.device_connector_channel = ZX_HANDLE_INVALID;

    delete AppContainer::FromContext(*context);
    context->imp.uuid_to_channel = nullptr;
  }
}

__EXPORT
TEEC_Result TEEC_RegisterSharedMemory(TEEC_Context* context, TEEC_SharedMemory* sharedMem) {
  /* This function is supposed to register an existing buffer for use as shared memory. We don't
   * have a way of discovering the VMO handle for an arbitrary address, so implementing this would
   * require an extra VMO that would be copied into at invocation. Since we currently don't have
   * any use cases for this function and TEEC_AllocateSharedMemory should be the preferred method
   * of acquiring shared memory, we're going to leave this unimplemented for now. */
  return TEEC_ERROR_NOT_IMPLEMENTED;
}

__EXPORT
TEEC_Result TEEC_AllocateSharedMemory(TEEC_Context* context, TEEC_SharedMemory* sharedMem) {
  if (!context || !sharedMem) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (sharedMem->flags & ~(TEEC_MEM_INPUT | TEEC_MEM_OUTPUT)) {
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  std::memset(&sharedMem->imp, 0, sizeof(sharedMem->imp));

  size_t size = sharedMem->size;

  zx::vmo vmo;
  zx_status_t status = CreateVmoWithName(size, 0, "teec_shared_memory", &vmo);
  if (status != ZX_OK) {
    return ConvertStatusToResult(status);
  }

  uintptr_t mapped_addr;
  status =
      zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0, size, &mapped_addr);
  if (status != ZX_OK) {
    return ConvertStatusToResult(status);
  }

  sharedMem->buffer = reinterpret_cast<void*>(mapped_addr);
  sharedMem->imp.vmo = vmo.release();
  sharedMem->imp.mapped_addr = mapped_addr;
  sharedMem->imp.mapped_size = size;

  return TEEC_SUCCESS;
}

__EXPORT
void TEEC_ReleaseSharedMemory(TEEC_SharedMemory* sharedMem) {
  if (!sharedMem) {
    return;
  }
  zx::vmar::root_self()->unmap(sharedMem->imp.mapped_addr, sharedMem->imp.mapped_size);
  zx_handle_close(sharedMem->imp.vmo);
  sharedMem->imp.vmo = ZX_HANDLE_INVALID;
}

__EXPORT
TEEC_Result TEEC_OpenSession(TEEC_Context* context, TEEC_Session* session,
                             const TEEC_UUID* destination, uint32_t connectionMethod,
                             const void* connectionData, TEEC_Operation* operation,
                             uint32_t* returnOrigin) {
  if (!context || !session || !destination) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_API;
    }
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  if (connectionMethod != TEEC_LOGIN_PUBLIC) {
    // TODO(rjascani): Investigate whether non public login is needed.
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_API;
    }
    return TEEC_ERROR_NOT_IMPLEMENTED;
  }

  fuchsia_tee::wire::Uuid app_uuid_fidl;
  ConvertTeecUuidToZxUuid(*destination, &app_uuid_fidl);

  fidl::Arena allocator;
  fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
  TEEC_Result processing_rc = PreprocessOperation(allocator, operation, &parameter_set);
  if (processing_rc != TEEC_SUCCESS) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return processing_rc;
  }

  fidl::UnownedClientEnd<fuchsia_tee::Application> app_client_end(ZX_HANDLE_INVALID);
  if (TEEC_Result result = ConnectApplication(app_uuid_fidl, context, &app_client_end);
      result != TEEC_SUCCESS) {
    return result;
  }

  auto result = fidl::WireCall(app_client_end)->OpenSession2(std::move(parameter_set));
  zx_status_t status = result.status();

  if (status != ZX_OK) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }

    if (status == ZX_ERR_PEER_CLOSED) {
      // If the channel has closed, drop the entry from the map, closing the client end.
      AppContainer::FromContext(*context)->Delete(app_uuid_fidl);
    }

    return ConvertStatusToResult(status);
  }

  uint32_t out_session_id = result.value().session_id;
  fuchsia_tee::wire::OpResult& out_result = result.value().op_result;

  if (!out_result.has_return_code() || !out_result.has_return_origin()) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return TEEC_ERROR_COMMUNICATION;
  }

  // Try and run post-processing regardless of TEE operation status. Even if an error occurred,
  // the parameter set may have been updated.
  processing_rc = out_result.has_parameter_set()
                      ? PostprocessOperation(out_result.parameter_set(), operation)
                      : TEEC_ERROR_COMMUNICATION;

  if (out_result.return_code() != TEEC_SUCCESS) {
    // If the TEE operation failed, use that return code above any processing failure codes.
    if (returnOrigin) {
      *returnOrigin = ConvertZxToTeecReturnOrigin(out_result.return_origin());
    }
    return static_cast<uint32_t>(out_result.return_code());
  }
  if (processing_rc != TEEC_SUCCESS) {
    // The TEE operation succeeded but the processing operation failed.
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return processing_rc;
  }

  session->imp.session_id = out_session_id;
  session->imp.application_channel = app_client_end.channel()->get();

  return static_cast<uint32_t>(out_result.return_code());
}

__EXPORT
void TEEC_CloseSession(TEEC_Session* session) {
  if (!session || session->imp.application_channel == ZX_HANDLE_INVALID) {
    return;
  }

  // TEEC_CloseSession simply swallows errors, so no need to check here.
  // TODO(https://fxbug.dev/42180237) Consider handling the error instead of ignoring it.
  (void)fidl::WireCall(GetApplicationFromSession(session))->CloseSession(session->imp.session_id);
  session->imp.application_channel = ZX_HANDLE_INVALID;
}

__EXPORT
TEEC_Result TEEC_InvokeCommand(TEEC_Session* session, uint32_t commandID, TEEC_Operation* operation,
                               uint32_t* returnOrigin) {
  if (!session || session->imp.application_channel == ZX_HANDLE_INVALID) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_API;
    }
    return TEEC_ERROR_BAD_PARAMETERS;
  }

  fidl::Arena allocator;
  fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
  TEEC_Result processing_rc = PreprocessOperation(allocator, operation, &parameter_set);
  if (processing_rc != TEEC_SUCCESS) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return processing_rc;
  }

  auto result = fidl::WireCall(GetApplicationFromSession(session))
                    ->InvokeCommand(session->imp.session_id, commandID, std::move(parameter_set));
  zx_status_t status = result.status();
  if (status != ZX_OK) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return ConvertStatusToResult(status);
  }

  fuchsia_tee::wire::OpResult& out_result = result.value().op_result;

  if (!out_result.has_return_code() || !out_result.has_return_origin()) {
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return TEEC_ERROR_COMMUNICATION;
  }

  // Try and run post-processing regardless of TEE operation status. Even if an error occurred,
  // the parameter set may have been updated.
  processing_rc = out_result.has_parameter_set()
                      ? PostprocessOperation(out_result.parameter_set(), operation)
                      : TEEC_ERROR_COMMUNICATION;

  if (out_result.return_code() != TEEC_SUCCESS) {
    // If the TEE operation failed, use that return code above any processing failure codes.
    if (returnOrigin) {
      *returnOrigin = ConvertZxToTeecReturnOrigin(out_result.return_origin());
    }
    return static_cast<uint32_t>(out_result.return_code());
  }
  if (processing_rc != TEEC_SUCCESS) {
    // The TEE operation succeeded but the processing operation failed.
    if (returnOrigin) {
      *returnOrigin = TEEC_ORIGIN_COMMS;
    }
    return processing_rc;
  }

  return static_cast<uint32_t>(out_result.return_code());
}

__EXPORT
void TEEC_RequestCancellation(TEEC_Operation* operation) {}
