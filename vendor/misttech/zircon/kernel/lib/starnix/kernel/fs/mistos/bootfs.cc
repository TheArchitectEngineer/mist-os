// Copyright 2024 Mist Tecnologia LTDA. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/mistos/starnix/kernel/fs/mistos/bootfs.h"

#include <lib/fit/result.h>
#include <lib/mistos/starnix/kernel/mm/memory.h>
#include <lib/mistos/starnix/kernel/task/current_task.h>
#include <lib/mistos/starnix/kernel/task/task.h>
#include <lib/mistos/starnix/kernel/vfs/fs_node.h>
#include <lib/mistos/starnix/kernel/vfs/fs_node_ops.h>
#include <lib/mistos/starnix/kernel/vfs/memory_file.h>
#include <lib/mistos/util/status.h>
#include <lib/mistos/zx/vmar.h>
#include <lib/mistos/zx/vmo.h>
#include <lib/zbi-format/internal/bootfs.h>
#include <lib/zbitl/error-stdio.h>
#include <lib/zbitl/view.h>
#include <trace.h>
#include <zircon/assert.h>
#include <zircon/errors.h>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <ktl/move.h>
#include <vm/pinned_vm_object.h>
#include <vm/vm_object.h>

#include "../kernel_priv.h"
#include "simple_directory.h"
#include "tree_builder.h"

#define LOCAL_TRACE STARNIX_KERNEL_GLOBAL_TRACE(0)

namespace starnix {

namespace {

using ZbiView = zbitl::View<zx::unowned_vmo>;
using ZbiError = ZbiView::Error;
using ZbiCopyError = ZbiView::CopyError<zx::vmo>;

constexpr const char kBootfsVmoName[] = "uncompressed-bootfs";
constexpr const char kScratchVmoName[] = "bootfs-decompression-scratch";

// This is used as the zbitl::View::CopyStorageItem callback to allocate
// scratch memory used by decompression.
class ScratchAllocator {
 public:
  class Holder {
   public:
    Holder() = delete;
    Holder(const Holder&) = delete;
    Holder& operator=(const Holder&) = delete;

    // Unlike the default move constructor and move assignment operators, these
    // ensure that exactly one destructor cleans up the mapping.

    Holder(Holder&& other) { *this = ktl::move(other); }

    Holder& operator=(Holder&& other) {
      ktl::swap(mapping_, other.mapping_);
      ktl::swap(pinned_vmo_, other.pinned_vmo_);
      return *this;
    }

    Holder(size_t size) {
      fbl::RefPtr<VmObjectPaged> vmo;
      uint64_t aligned_size;
      zx_status_t status = VmObject::RoundSize(size, &aligned_size);
      ZX_ASSERT(status == ZX_OK);
      status = VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0, aligned_size, &vmo);
      ZX_ASSERT(status == ZX_OK);
      status = vmo->set_name(kScratchVmoName, strlen(kScratchVmoName) - 1);
      ZX_ASSERT(status == ZX_OK);

      size = ROUNDUP_PAGE_SIZE(size);
      status = PinnedVmObject::Create(vmo, 0, size, /*write=*/true, &pinned_vmo_);
      ZX_ASSERT_MSG(status == ZX_OK, "Failed to make pin: %d\n", status);

      zx::result<VmAddressRegion::MapResult> map_result =
          VmAspace::kernel_aspace()->RootVmar()->CreateVmMapping(
              0, size, 0, VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE, pinned_vmo_.vmo(), 0,
              ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE, kScratchVmoName);

      if (map_result.is_error()) {
        ZX_PANIC("Failed to map in aspace\n");
      }

      if (status = map_result->mapping->MapRange(0, size, true); status != ZX_OK) {
        ZX_PANIC("Failed to map range\n");
      }

      mapping_ = ktl::move(map_result->mapping);
    }

    // zbitl::View::CopyStorageItem calls this to get the scratch memory.
    void* get() const { return reinterpret_cast<void*>(mapping_->base_locking()); }

    ~Holder() {
      if (mapping_) {
        mapping_->Destroy();
      }
    }

   private:
    PinnedVmObject pinned_vmo_;
    fbl::RefPtr<VmMapping> mapping_;
  };

  // zbitl::View::CopyStorageItem calls this to allocate scratch space.
  fit::result<ktl::string_view, Holder> operator()(size_t size) const {
    return fit::ok(Holder{size});
  }

  ScratchAllocator() = default;
};

[[noreturn]] void FailFromZbiCopyError(const ZbiCopyError& error) {
  zbitl::PrintViewCopyError(error, [&](const char* fmt, ...) {
    va_list args;
    va_start(args, fmt);
    vprintf(fmt, args);
    va_end(args);
  });
  ZX_PANIC("");
}

bool zbi_bootfs_is_aligned(uint32_t size) { return (size % ZBI_BOOTFS_PAGE_SIZE == 0); }

// Transferring data from Bootfs can only be done with page-aligned offsets
// and sizes. It is expected for the VMO offset to be aligned by BootfsParser,
// but the size alignment is not guaranteed.
fit::result<zx_status_t, util::Range<uint64_t>> aligned_range(uint32_t offset, uint32_t size) {
  if (!zbi_bootfs_is_aligned(offset)) {
    return fit::error(ZX_ERR_INTERNAL);
  }
  uint64_t aligned_offset = static_cast<uint64_t>(offset);
  uint64_t aligned_size = static_cast<uint64_t>(ZBI_BOOTFS_PAGE_ALIGN(size));
  return fit::ok(
      util::Range<uint64_t>{.start = aligned_offset, .end = (aligned_offset + aligned_size)});
}

fbl::Vector<ktl::string_view> split_and_filter(const ktl::string_view& str, char delimiter) {
  fbl::Vector<ktl::string_view> result;
  ktl::string_view::size_type start = 0, end;

  while ((end = str.find(delimiter, start)) != std::string_view::npos) {
    auto token = str.substr(start, end - start);
    if (!token.empty()) {
      fbl::AllocChecker ac;
      result.push_back(token, &ac);
      ASSERT(ac.check());
    }
    start = end + 1;
  }

  // Add the last token
  auto last_token = str.substr(start);
  if (!last_token.empty()) {
    fbl::AllocChecker ac;
    result.push_back(last_token, &ac);
    ASSERT(ac.check());
  }

  return result;
}

}  // namespace

FileSystemHandle BootFs::new_fs(const fbl::RefPtr<Kernel>& kernel, zx::unowned_vmo vmo) {
  if (auto result = BootFs::new_fs_with_options(kernel, ktl::move(vmo), {}); result.is_error()) {
    ZX_PANIC("empty options cannot fail");
  } else {
    return result.value();
  }
}

fit::result<Errno, FileSystemHandle> BootFs::new_fs_with_options(const fbl::RefPtr<Kernel>& kernel,
                                                                 zx::unowned_vmo vmo,
                                                                 FileSystemOptions options) {
  fbl::AllocChecker ac;
  auto bootfs = new (&ac) BootFs(zx::vmar::kernel_vmar(), ktl::move(vmo));
  if (!ac.check()) {
    return fit::error(errno(ENOMEM));
  }

  auto fs = FileSystem::New(kernel, {.type = CacheMode::Type::Permanent}, bootfs,
                            ktl::move(options)) _EP(fs);
  TreeBuilder tree = TreeBuilder::empty_dir();
  auto mode = FILE_MODE(IFDIR, 0755);

  BootfsView view = bootfs->bootfs_reader_.root();
  for (auto item : view) {
    LTRACEF("name=[%.*s]\n", static_cast<int>(item.name.length()), item.name.data());
    auto vmo_range = aligned_range(item.offset, item.size);
    if (vmo_range.is_error()) {
      return fit::error(errno(EIO));
    }
    auto file_vmo = bootfs->create_vmo_from_bootfs(vmo_range.value(), item.size);
    if (file_vmo.is_error()) {
      return fit::error(errno(EIO));
    }

    auto node = MemoryFileNode::from_memory(MemoryObject::From(ktl::move(file_vmo.value())));
    auto result =
        tree.add_entry(split_and_filter(item.name, '/'), ktl::unique_ptr<FsNodeOps>(node));
    ZX_ASSERT(result.is_ok());
  }

  auto root = tree.build(fs.value());

  auto root_node =
      FsNode::new_root_with_properties(root, [&mode /*, &uid, &gid*/](FsNodeInfo& info) -> void {
        info.chmod(mode);
        info.uid_ = 0;
        info.gid_ = 0;
      });
  fs->set_root_node(root_node);

  return fit::ok(ktl::move(fs.value()));
}

namespace {

uint32_t from_be_bytes(const std::array<uint8_t, 4>& bytes) {
  return (static_cast<uint32_t>(bytes[0]) << 24) | (static_cast<uint32_t>(bytes[1]) << 16) |
         (static_cast<uint32_t>(bytes[2]) << 8) | static_cast<uint32_t>(bytes[3]);
}

}  // namespace

fit::result<Errno, struct statfs> BootFs::statfs(const FileSystem& fs,
                                                 const CurrentTask& current_task) const {
  struct statfs stat = default_statfs(from_be_bytes(ktl::array<uint8_t, 4>{'m', 'b', 'f', 's'}));
  return fit::ok(stat);
}

BootFs::BootFs(zx::unowned<zx::vmar> vmar, zx::unowned_vmo boot_vmo) {
  ZbiView zbi(ktl::move(boot_vmo));

  zx::vmo bootfs_vmo;
  for (auto it = zbi.begin(); it != zbi.end(); ++it) {
    if (it->header->type == ZBI_TYPE_STORAGE_BOOTFS) {
      auto result = zbi.CopyStorageItem(it, ScratchAllocator());
      if (result.is_error()) {
        printf("cannot extract BOOTFS from ZBI: ");
        FailFromZbiCopyError(result.error_value());
      }

      bootfs_vmo = zx::vmo(ktl::move(result).value().release());
      zx_status_t status =
          bootfs_vmo.set_property(ZX_PROP_NAME, kBootfsVmoName, sizeof(kBootfsVmoName) - 1);
      ZX_ASSERT_MSG(status == ZX_OK, "cannot set name of uncompressed BOOTFS VMO");

#if 0
      // Signal that we've already processed this one.
      // GCC's -Wmissing-field-initializers is buggy: it should allow
      // designated initializers without all fields, but doesn't (in C++?).
      zbi_header_t discard{};
      discard.type = ZBI_TYPE_DISCARD;
      if (auto ok = zbi.EditHeader(it, discard); ok.is_error()) {
        ZX_PANIC("vmo write failed on ZBI VMO\n");
      }
#endif

      // Cancel error-checking since we're ending the iteration on purpose.
      zbi.ignore_error();
      break;
    }
  }
  if (bootfs_vmo.is_valid()) {
    if (auto result = BootfsReader::Create(ktl::move(bootfs_vmo)); result.is_error()) {
      zbitl::PrintBootfsError(result.error_value(), [&](const char* fmt, ...) {
        va_list args;
        va_start(args, fmt);
        vprintf(fmt, args);
        va_end(args);
      });
      ZX_PANIC("Failed to create bootfs");
    } else {
      bootfs_reader_ = ktl::move(result.value());
    }
  } else {
    if (auto check = zbi.take_error(); check.is_error()) {
      printf("invalid ZBI: ");
      zbitl::PrintViewError(check.error_value(), [&](const char* fmt, ...) {
        va_list args;
        va_start(args, fmt);
        vprintf(fmt, args);
        va_end(args);
      });
      ZX_PANIC("Invalid ZBI");
    }
  }
}

fit::result<BootfsError, zx::vmo> BootFs::create_vmo_from_bootfs(const util::Range<uint64_t>& range,
                                                                 uint64_t original_size) {
  auto aligned_size = range.end - range.start;
  zx::vmo vmo;
  zx_status_t status = zx::vmo::create(aligned_size, ZX_VMO_RESIZABLE, &vmo);
  if (status != ZX_OK) {
    return fit::error{BootfsError(BootfsError::Code::kVmo, status)};
  }

  status = vmo.transfer_data(0, 0, aligned_size, &bootfs_reader_.storage(), range.start);
  if (status != ZX_OK) {
    return fit::error{BootfsError(BootfsError::Code::kVmo, status)};
  }

  // Set the VMO content size back to the original size.
  status = vmo.set_size(original_size);
  if (status != ZX_OK) {
    return fit::error{BootfsError(BootfsError::Code::kVmo, status)};
  }

  return fit::ok(ktl::move(vmo));
}

BootFs::~BootFs() { LTRACE_ENTRY_OBJ; }

}  // namespace starnix
