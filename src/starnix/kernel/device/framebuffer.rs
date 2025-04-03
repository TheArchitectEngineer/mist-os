// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::framebuffer_server::{init_viewport_scene, start_presentation_loop, FramebufferServer};
use crate::device::kobject::DeviceMetadata;
use crate::device::{DeviceMode, DeviceOps};
use crate::fs::sysfs::DeviceDirectory;
use crate::mm::memory::MemoryObject;
use crate::mm::MemoryAccessorExt;
use crate::task::{CurrentTask, Kernel};
use crate::vfs::{fileops_impl_memory, fileops_impl_noop_sync, FileObject, FileOps, FsNode};
use fuchsia_component::client::connect_to_protocol_sync;
use starnix_logging::{log_info, log_warn};
use starnix_sync::{DeviceOpen, FileOpsCore, LockBefore, Locked, Mutex, RwLock, Unlocked};
use starnix_syscalls::{SyscallArg, SyscallResult, SUCCESS};
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::{
    errno, error, fb_bitfield, fb_fix_screeninfo, fb_var_screeninfo, FBIOGET_FSCREENINFO,
    FBIOGET_VSCREENINFO, FBIOPUT_VSCREENINFO, FB_TYPE_PACKED_PIXELS, FB_VISUAL_TRUECOLOR,
};
use std::sync::Arc;
use zerocopy::IntoBytes;
use {
    fidl_fuchsia_io as fio, fidl_fuchsia_math as fmath,
    fidl_fuchsia_ui_composition as fuicomposition, fidl_fuchsia_ui_display_singleton as fuidisplay,
    fidl_fuchsia_ui_views as fuiviews,
};

fn get_display_size() -> Result<fmath::SizeU, Errno> {
    let singleton_display_info =
        connect_to_protocol_sync::<fuidisplay::InfoMarker>().map_err(|_| errno!(ENOENT))?;
    let metrics = singleton_display_info
        .get_metrics(zx::MonotonicInstant::INFINITE)
        .map_err(|_| errno!(EINVAL))?;
    let extent_in_px =
        metrics.extent_in_px.ok_or("Failed to get extent_in_px").map_err(|_| errno!(EINVAL))?;
    Ok(extent_in_px)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AspectRatio {
    pub width: u32,
    pub height: u32,
}

pub struct Framebuffer {
    server: Option<Arc<FramebufferServer>>,
    memory: Mutex<Option<Arc<MemoryObject>>>,
    pub info: RwLock<fb_var_screeninfo>,
    pub view_identity: Mutex<Option<fuiviews::ViewIdentityOnCreation>>,
    pub view_bound_protocols: Mutex<Option<fuicomposition::ViewBoundProtocols>>,
}

impl Framebuffer {
    /// Returns the current fraembuffer if one was created for this kernel.
    pub fn get(kernel: &Kernel) -> Result<Arc<Self>, Errno> {
        kernel.expando.get_or_try_init(|| error!(EINVAL))
    }

    /// Initialize the framebuffer device. Should only be called once per kernel.
    pub fn device_init<L>(
        locked: &mut Locked<'_, L>,
        system_task: &CurrentTask,
        aspect_ratio: Option<AspectRatio>,
        enable_visual_debugging: bool,
    ) -> Result<Arc<Framebuffer>, Errno>
    where
        L: LockBefore<FileOpsCore>,
    {
        let kernel = system_task.kernel();
        let registry = &kernel.device_registry;

        let framebuffer = kernel
            .expando
            .get_or_try_init(|| Framebuffer::new(aspect_ratio, enable_visual_debugging))?;

        let graphics_class = registry.objects.graphics_class();
        registry.register_device(
            locked,
            system_task,
            "fb0".into(),
            DeviceMetadata::new("fb0".into(), DeviceType::FB0, DeviceMode::Char),
            graphics_class,
            DeviceDirectory::new,
            framebuffer.clone(),
        );

        Ok(framebuffer)
    }

    /// Creates a new `Framebuffer` fit to the screen, while maintaining the provided aspect ratio.
    ///
    /// If the `aspect_ratio` is `None`, the framebuffer will be scaled to the display.
    fn new(
        aspect_ratio: Option<AspectRatio>,
        enable_visual_debugging: bool,
    ) -> Result<Self, Errno> {
        let mut info = fb_var_screeninfo::default();

        let display_size = get_display_size().unwrap_or(fmath::SizeU { width: 700, height: 1200 });

        // If the container has a specific aspect ratio set, use that to fit the framebuffer
        // inside of the display.
        let (feature_width, feature_height) = aspect_ratio
            .map(|ar| (ar.width, ar.height))
            .unwrap_or((display_size.width, display_size.height));

        // Scale to framebuffer to fit the display, while maintaining the expected aspect ratio.
        let ratio =
            std::cmp::min(display_size.width / feature_width, display_size.height / feature_height);
        let (width, height) = (feature_width * ratio, feature_height * ratio);

        info.xres = width;
        info.yres = height;
        info.xres_virtual = info.xres;
        info.yres_virtual = info.yres;
        info.bits_per_pixel = 32;
        info.red = fb_bitfield { offset: 0, length: 8, msb_right: 0 };
        info.green = fb_bitfield { offset: 8, length: 8, msb_right: 0 };
        info.blue = fb_bitfield { offset: 16, length: 8, msb_right: 0 };
        info.transp = fb_bitfield { offset: 24, length: 8, msb_right: 0 };

        if let Ok((server, memory)) = FramebufferServer::new(width, height) {
            let server = Arc::new(server);
            let memory_len = memory.info()?.size_bytes as u32;

            // Fill the buffer with black pixels as a placeholder, if visual debug is off.
            // Fill the buffer with purple, if visual debug is on.
            let background = if enable_visual_debugging {
                [0xff, 0x00, 0xff, 0xff].repeat((memory_len / 4) as usize)
            } else {
                vec![0x00; memory_len as usize]
            };

            if let Err(err) = memory.write(&background, 0) {
                log_warn!("could not write initial framebuffer: {:?}", err);
            }

            Ok(Self {
                server: Some(server),
                memory: Mutex::new(Some(memory)),
                info: RwLock::new(info),
                view_identity: Default::default(),
                view_bound_protocols: Default::default(),
            })
        } else {
            Ok(Self {
                server: None,
                memory: Default::default(),
                info: RwLock::new(info),
                view_identity: Default::default(),
                view_bound_protocols: Default::default(),
            })
        }
    }

    /// Starts presenting a view based on this framebuffer.
    ///
    /// # Parameters
    /// * `incoming_dir`: the incoming service directory under which the
    ///   `fuchsia.element.GraphicalPresenter` protocol can be retrieved.
    pub fn start_server(&self, kernel: &Arc<Kernel>, incoming_dir: Option<fio::DirectoryProxy>) {
        if let Some(server) = &self.server {
            let view_bound_protocols = self.view_bound_protocols.lock().take().unwrap();
            let view_identity = self.view_identity.lock().take().unwrap();
            log_info!("Presenting view using GraphicalPresenter");
            start_presentation_loop(
                kernel,
                server.clone(),
                view_bound_protocols,
                view_identity,
                incoming_dir,
            );
        }
    }

    /// Starts presenting a child view instead of the framebuffer.
    ///
    /// # Parameters
    /// * `viewport_token`: handles to the child view
    pub fn present_view(&self, viewport_token: fuiviews::ViewportCreationToken) {
        if let Some(server) = &self.server {
            init_viewport_scene(server.clone(), viewport_token);

            // Release the memory associated with the framebuffer.
            let mut memory = self.memory.lock();
            if let Some(memory_ref) = memory.as_ref() {
                let bytes = memory_ref.get_size();
                let refs = Arc::strong_count(memory_ref);
                *memory = None;
                log_info!("Released framebuffer memory ({} bytes, {} refs)", bytes, refs);
            }
        }
    }

    /// Returns the framebuffer's memory.
    fn get_memory(&self) -> Result<Arc<MemoryObject>, Errno> {
        self.memory.lock().clone().ok_or_else(|| errno!(EIO))
    }

    /// Returns the logical size of the framebuffer's memory.
    fn memory_len(&self) -> usize {
        self.memory
            .lock()
            .as_ref()
            .map_or(0, |memory| memory.info().map_or(0, |info| info.size_bytes)) as usize
    }

    /// Returns the allocated size of the framebuffer's memory.
    fn memory_size(&self) -> usize {
        self.memory.lock().as_ref().map_or(0, |memory| memory.get_size()) as usize
    }
}

impl DeviceOps for Arc<Framebuffer> {
    fn open(
        &self,
        _locked: &mut Locked<'_, DeviceOpen>,
        _current_task: &CurrentTask,
        dev: DeviceType,
        node: &FsNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        if dev.minor() != 0 {
            return error!(ENODEV);
        }
        node.update_info(|info| {
            info.size = self.memory_len();
            info.blocks = self.memory_size() / info.blksize;
            Ok(())
        })?;
        Ok(Box::new(Arc::clone(self)))
    }
}

impl FileOps for Framebuffer {
    fileops_impl_memory!(self, &self.get_memory()?);
    fileops_impl_noop_sync!();

    fn ioctl(
        &self,
        _locked: &mut Locked<'_, Unlocked>,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        match request {
            FBIOGET_FSCREENINFO => {
                let info = self.info.read();
                let finfo = fb_fix_screeninfo {
                    id: zerocopy::FromBytes::read_from_bytes(&b"Starnix\0\0\0\0\0\0\0\0\0"[..])
                        .unwrap(),
                    smem_start: 0,
                    smem_len: self.memory_len() as u32,
                    type_: FB_TYPE_PACKED_PIXELS,
                    visual: FB_VISUAL_TRUECOLOR,
                    line_length: info.bits_per_pixel / 8 * info.xres,
                    ..fb_fix_screeninfo::default()
                };
                current_task.write_object(UserRef::new(user_addr), &finfo)?;
                Ok(SUCCESS)
            }

            FBIOGET_VSCREENINFO => {
                let info = self.info.read();
                current_task.write_object(UserRef::new(user_addr), &*info)?;
                Ok(SUCCESS)
            }

            FBIOPUT_VSCREENINFO => {
                let new_info: fb_var_screeninfo =
                    current_task.read_object(UserRef::new(user_addr))?;
                let old_info = self.info.read();
                // We don't yet support actually changing anything
                if new_info.as_bytes() != old_info.as_bytes() {
                    return error!(EINVAL);
                }
                Ok(SUCCESS)
            }

            _ => {
                error!(EINVAL)
            }
        }
    }
}
