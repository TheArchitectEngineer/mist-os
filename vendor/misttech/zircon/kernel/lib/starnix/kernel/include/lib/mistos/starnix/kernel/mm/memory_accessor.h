// Copyright 2024 Mist Tecnologia LTDA. All rights reserved.
// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_KERNEL_LIB_MISTOS_STARNIX_KERNEL_INCLUDE_LIB_MISTOS_STARNIX_KERNEL_MM_MEMORY_ACCESSOR_H_
#define ZIRCON_KERNEL_LIB_MISTOS_STARNIX_KERNEL_INCLUDE_LIB_MISTOS_STARNIX_KERNEL_MM_MEMORY_ACCESSOR_H_

#include <lib/fit/result.h>
#include <lib/mistos/starnix/kernel/vfs/path.h>
#include <lib/mistos/starnix_uapi/errors.h>
#include <lib/mistos/starnix_uapi/user_address.h>
#include <lib/mistos/starnix_uapi/user_buffer.h>
#include <lib/mistos/starnix_uapi/user_value.h>
#include <lib/mistos/util/error_propagation.h>
#include <lib/mistos/util/maybe_uninit.h>

#include <fbl/array.h>
#include <fbl/vector.h>
#include <ktl/array.h>
#include <ktl/byte.h>
#include <ktl/utility.h>
#include <ktl/utility.h>
#include <ktl/span.h>

using namespace starnix_uapi;

namespace starnix {

template <typename T>
inline ktl::span<uint8_t> object_as_mut_bytes(mtl::MaybeUninit<T>& object) {
  return ktl::span<uint8_t>{reinterpret_cast<uint8_t*>(object.as_mut_ptr()), sizeof(T)};
}

template <typename T>
inline ktl::span<uint8_t> array_as_mut_bytes(fbl::Array<mtl::MaybeUninit<T>> array) {
  return ktl::span<uint8_t>{reinterpret_cast<uint8_t*>(array.data()), array.size() * sizeof(T)};
}

/// Holds the number of _elements_ read by the callback to [`read_to_vec`].
///
/// Used to make it clear to callers that the callback should return the number
/// of elements read and not the number of bytes read.
struct NumberOfElementsRead {
  size_t n_elements;
};

/// Performs a read into a `Vec` using the provided read function.
///
/// The read function returns the number of elements of type `T` read.
///
/// # Safety
///
/// The read function must only return `Ok(n)` if at least one element was read and `n` holds
/// the number of elements of type `T` read starting from the beginning of the slice.
template <typename E, typename T, typename ReadFn>
inline fit::result<E, fbl::Vector<T>> read_to_vec(size_t max_len, ReadFn&& read_fn) {
  static_assert(std::is_invocable_r_v<fit::result<E, NumberOfElementsRead>, ReadFn, ktl::span<T>&>);

  fbl::AllocChecker ac;
  fbl::Vector<T> buffer;
  buffer.reserve(max_len, &ac);
  if (!ac.check()) {
    return fit::error(errno(ENOMEM));
  }

  ktl::span<T> capacity(buffer.data(), max_len);
  auto read_result = read_fn(capacity) _EP(read_result);

  NumberOfElementsRead read_elements{read_result.value()};
  DEBUG_ASSERT_MSG(read_elements.n_elements <= max_len, "read_elements=%zu, max_len=%zu",
                   read_elements.n_elements, max_len);
  // SAFETY: The new length is equal to the number of elements successfully
  // initialized (since `read_fn` returned successfully).
  buffer.set_size(read_elements.n_elements);
  return fit::ok(ktl::move(buffer));
}

/// Performs a read into an array using the provided read function.
///
/// The read function returns `Ok(())` if the buffer was fully read to.
///
/// # Safety
///
/// The read function must only return `Ok(())` if all the bytes were read to.
template <typename E, typename T, size_t N, typename ReadFn>
inline fit::result<E, ktl::array<T, N>> read_to_array(ReadFn&& read_fn) {
  static_assert(std::is_invocable_r_v<fit::result<E>, ReadFn, ktl::span<T>&>);

  ktl::array<T, N> buffer;
  ktl::span<T> span{buffer.data(), N};
  auto read_fn_result = read_fn(span) _EP(read_fn_result);
  return fit::ok(ktl::move(buffer));
}

/// Performs a read into an object using the provided read function.
///
/// The read function returns `Ok(())` if the buffer was fully read to.
///
/// # Safety
///
/// THe read function must only return `Ok(())` if all the bytes were read to.
template <typename E, typename T, typename ReadFn>
inline fit::result<E, T> read_to_object_as_bytes(ReadFn&& read_fn) {
  static_assert(std::is_invocable_r_v<fit::result<E>, ReadFn, ktl::span<uint8_t>&>);

  auto object = mtl::MaybeUninit<T>::uninit();
  auto span = object_as_mut_bytes(object);
  auto read_fn_result = read_fn(span) _EP(read_fn_result);
  // SAFETY: The call to `read_fn` succeeded so we know that `object`
  // has been initialized.
  return fit::ok(ktl::move(object).assume_init());
}

class MemoryAccessor {
 public:
  /// Reads exactly `bytes.len()` bytes of memory from `addr` into `bytes`.
  ///
  /// In case of success, the number of bytes read will always be `bytes.len()`.
  ///
  /// Consider using `MemoryAccessorExt::read_memory_to_*` methods if you do not require control
  /// over the allocation.
  virtual fit::result<Errno, ktl::span<uint8_t>> read_memory(UserAddress addr,
                                                             ktl::span<uint8_t>& bytes) const = 0;

  /// Reads bytes starting at `addr`, continuing until either a null byte is read, `bytes.len()`
  /// bytes have been read or no more bytes can be read from the target.
  ///
  /// This is used, for example, to read null-terminated strings where the exact length is not
  /// known, only the maximum length is.
  ///
  /// Returns the bytes that have been read to on success.
  virtual fit::result<Errno, ktl::span<uint8_t>> read_memory_partial_until_null_byte(
      UserAddress addr, ktl::span<uint8_t>& bytes) const = 0;

  /// Reads bytes starting at `addr`, continuing until either `bytes.len()` bytes have been read
  /// or no more bytes can be read from the target.
  ///
  /// This is used, for example, to read null-terminated strings where the exact length is not
  /// known, only the maximum length is.
  ///
  /// Consider using `MemoryAccessorExt::read_memory_partial_to_*` methods if you do not require
  /// control over the allocation.
  virtual fit::result<Errno, ktl::span<uint8_t>> read_memory_partial(
      UserAddress addr, ktl::span<uint8_t>& bytes) const = 0;

  /// Writes the provided bytes to `addr`.
  ///
  /// In case of success, the number of bytes written will always be `bytes.len()`.
  ///
  /// # Parameters
  /// - `addr`: The address to write to.
  /// - `bytes`: The bytes to write from.
  virtual fit::result<Errno, size_t> write_memory(UserAddress addr,
                                                  const ktl::span<const uint8_t>& bytes) const = 0;

  /// Writes bytes starting at `addr`, continuing until either `bytes.len()` bytes have been
  /// written or no more bytes can be written.
  ///
  /// # Parameters
  /// - `addr`: The address to write to.
  /// - `bytes`: The bytes to write from.
  virtual fit::result<Errno, size_t> write_memory_partial(
      UserAddress addr, const ktl::span<const uint8_t>& bytes) const = 0;

  /// Writes zeros starting at `addr` and continuing for `length` bytes.
  ///
  /// Returns the number of bytes that were zeroed.
  virtual fit::result<Errno, size_t> zero(UserAddress addr, size_t length) const = 0;

  virtual ~MemoryAccessor() = default;
};

class MemoryAccessorExt : public MemoryAccessor {
 public:
  /// Read exactly `len` bytes of memory, returning them as a a fbl::Vector.
  fit::result<Errno, fbl::Vector<uint8_t>> read_memory_to_vec(UserAddress addr, size_t len) const;

  /// Read up to `max_len` bytes from `addr`, returning them as a Vec.
  fit::result<Errno, fbl::Vector<uint8_t>> read_memory_partial_to_vec(UserAddress addr,
                                                                      size_t max_len) const;

  // Read exactly `N` bytes from `addr`, returning them as an array.
  template <size_t N>
  fit::result<Errno, ktl::array<uint8_t, N>> read_memory_to_array(UserAddress addr) const {
    return read_to_array<Errno, uint8_t, N>([&](ktl::span<uint8_t>& buf) -> fit::result<Errno> {
      auto read_result = this->read_memory(addr, buf) _EP(read_result);
      DEBUG_ASSERT(N == read_result->size());
      return fit::ok();
    });
  }

  /// Read an instance of T from `user`.
  template <typename T>
  fit::result<Errno, T> read_object(UserRef<T> user) const {
    return read_to_object_as_bytes<Errno, T>([&](ktl::span<uint8_t>& buf) -> fit::result<Errno> {
      auto read_result = this->read_memory(user.addr(), buf) _EP(read_result);
      // SAFETY: `read_memory` only returns `Ok` if all bytes were read to.
      DEBUG_ASSERT(sizeof(T) == read_result->size());
      return fit::ok();
    });
  }

  /// Reads the first `partial` bytes of an object, leaving any remainder 0-filled.
  ///
  /// This is used for reading size-versioned structures where the user can specify an older
  /// version of the structure with a smaller size.
  ///
  /// Returns EINVAL if the input size is larger than the object (assuming the input size is from
  /// the user who has specified something we don't support).
  template <typename T>
  fit::result<Errno, T> read_object_partial(UserRef<T> user, size_t partial_size) const {
    auto full_size = sizeof(T);
    if (partial_size > full_size) {
      return fit::error(errno(EINVAL));
    }

    // This implementation involves an extra memcpy compared to read_object but avoids unsafe
    // code. This isn't currently called very often.
    auto object = mtl::MaybeUninit<T>::uninit();
    auto span = object_as_mut_bytes(object);
    ktl::span<uint8_t> to_read{span.data(), partial_size};
    ktl::span<uint8_t> to_zero = span.subspan(partial_size);

    auto read_result = read_memory(user.addr(), to_read);
    if (read_result.is_error()) {
      return read_result.take_error();
    }

    // Zero pad out to the correct size.
    memset(to_zero.data(), 0x00, to_zero.size());

    return fit::ok(ktl::move(object).assume_init());
  }

  /// Read exactly `objects.len()` objects into `objects` from `user`.
  template <typename T>
  fit::result<Errno, ktl::span<T>> read_objects(UserRef<T> user, ktl::span<T> objects) const {
    auto objects_len = objects.size();

    ktl::span<uint8_t> as_bytes{reinterpret_cast<uint8_t*>(objects.data()), objects.size_bytes()};
    auto read_result = read_memory(user.addr(), as_bytes) _EP(read_result);
    DEBUG_ASSERT(objects_len * sizeof(T) == read_result->size());

    return fit::ok(ktl::span{reinterpret_cast<T*>(read_result->data()), objects_len});
  }

  // Read exactly `len` objects from `user`, returning them as a Vec.
  template <typename T>
  fit::result<Errno, fbl::Vector<T>> read_objects_to_vec(UserRef<T> user, size_t len) const {
    return read_to_vec<Errno, T>(len,
                                 [&](ktl::span<T>& b) -> fit::result<Errno, NumberOfElementsRead> {
                                   auto read_result = this->read_objects(user, b) _EP(read_result);
                                   DEBUG_ASSERT(len == read_result->size());
                                   return fit::ok(NumberOfElementsRead{len});
                                 });
  }

  /// Read exactly `len` objects from `user`, returning them as a SmallVector.
  template <typename T, size_t N>
  fit::result<Errno, util::SmallVector<T, N>> read_objects_to_smallvec(UserRef<T> user,
                                                                       size_t len) const {
    if (len > N) {
      auto read_result = read_objects_to_vec<T>(user, len) _EP(read_result);
      return fit::ok(util::SmallVector<T, N>::from_vec(ktl::move(*read_result)));
    }
    ktl::array<T, N> buffer;
    auto read_result = read_objects(user, {buffer.data(), buffer.size()}) _EP(read_result);
    return fit::ok(util::SmallVector<T, N>::from_buf_and_len_unchecked(read_result->data(), len));
  }

  /// Read exactly `iovec_count` `UserBuffer`s from `iovec_addr`.
  ///
  /// Fails if `iovec_count` is greater than `UIO_MAXIOV`.
  fit::result<Errno, UserBuffers> read_iovec(UserAddress iovec_addr,
                                             UserValue<uint32_t> iovec_count) const;

  /// Read up to `max_size` bytes from `string`, stopping at the first discovered null byte and
  /// returning the results as a Vec.
  fit::result<Errno, FsString> read_c_string_to_vec(UserCString string, size_t max_size) const;

  /// Read up to `buffer.len()` bytes from `string`, stopping at the first discovered null byte
  /// and returning the result as a slice that ends before that null.
  ///
  /// Consider using `read_c_string_to_vec` if you do not require control over the allocation.
  fit::result<Errno, FsString> read_c_string(UserCString string, ktl::span<uint8_t>& buffer) const;

  template <typename T>
  fit::result<Errno, size_t> write_object(UserRef<T> user, const T& object) const {
    ktl::span<const uint8_t> data{reinterpret_cast<const uint8_t*>(&object), sizeof(T)};
    return this->write_memory(user.addr(), data);
  }

  template <typename T>
  fit::result<Errno, size_t> write_objects(UserRef<T> user, const T* objects, size_t count) const {
    ktl::span<const uint8_t> data{reinterpret_cast<const uint8_t*>(objects), count * sizeof(T)};
    return this->write_memory(user.addr(), data);
  }

  ~MemoryAccessorExt() override = default;
};

class TaskMemoryAccessor : public MemoryAccessorExt {
 public:
  /// Returns the maximum valid address for this memory accessor.
  virtual UserAddress maximum_valid_address() const = 0;
};

}  // namespace starnix

#endif  // ZIRCON_KERNEL_LIB_MISTOS_STARNIX_KERNEL_INCLUDE_LIB_MISTOS_STARNIX_KERNEL_MM_MEMORY_ACCESSOR_H_
