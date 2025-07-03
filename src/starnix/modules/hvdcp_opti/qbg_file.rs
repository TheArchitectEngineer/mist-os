// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::utils::{connect_to_device, connect_to_device_channel};
use fidl_fuchsia_hardware_qcom_hvdcpopti as fhvdcpopti;
use futures_util::StreamExt;
use starnix_core::mm::MemoryAccessorExt;
use starnix_core::power::{create_proxy_for_wake_events_counter, mark_proxy_message_handled};
use starnix_core::task::{CurrentTask, EventHandler, WaitCanceler, WaitQueue, Waiter};
use starnix_core::vfs::{
    fileops_impl_nonseekable, fileops_impl_noop_sync, FileObject, FileOps, FsNode, InputBuffer,
    OutputBuffer, VecInputBuffer,
};
use starnix_logging::{log_error, log_warn, track_stub};
use starnix_sync::{DeviceOpen, FileOpsCore, Locked, Mutex, Unlocked};
use starnix_syscalls::{SyscallArg, SyscallResult, SUCCESS};
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{errno, error};
use std::collections::VecDeque;
use std::sync::Arc;

pub const QBGIOCXCFG: u32 = 0x80684201;
pub const QBGIOCXEPR: u32 = 0x80304202;
pub const QBGIOCXEPW: u32 = 0xC0304203;
pub const QBGIOCXSTEPCHGCFG: u32 = 0xC0F74204;

pub fn create_qbg_device(
    _locked: &mut Locked<DeviceOpen>,
    current_task: &CurrentTask,
    _id: DeviceType,
    _node: &FsNode,
    _flags: OpenFlags,
) -> Result<Box<dyn FileOps>, Errno> {
    Ok(Box::new(QbgDeviceFile::new(current_task)))
}

struct QbgDeviceState {
    waiters: WaitQueue,
    read_queue: Mutex<VecDeque<VecInputBuffer>>,
    message_counter: Mutex<Option<zx::Counter>>,
}

impl QbgDeviceState {
    fn new() -> Self {
        Self {
            waiters: WaitQueue::default(),
            read_queue: Mutex::new(VecDeque::new()),
            message_counter: Mutex::new(None),
        }
    }

    fn handle_event(&self, event: fhvdcpopti::DeviceEvent) {
        let fhvdcpopti::DeviceEvent::OnFifoData { data } = event;
        self.read_queue.lock().push_back(data.into());
        self.waiters.notify_fd_events(FdEvents::POLLIN);
    }
}

async fn run_qbg_device_event_loop(
    device_state: Arc<QbgDeviceState>,
    mut event_stream: fhvdcpopti::DeviceEventStream,
) {
    loop {
        match event_stream.next().await {
            Some(Ok(event)) => {
                device_state.handle_event(event);
            }
            Some(Err(e)) => {
                log_error!("qbg: Received error from device event stream: {}", e);
                break;
            }
            None => {
                log_warn!("qbg: Exhausted device event stream");
                break;
            }
        }
    }

    device_state.waiters.notify_fd_events(FdEvents::POLLHUP);
}

fn spawn_qbg_device_tasks(device_state: Arc<QbgDeviceState>, current_task: &CurrentTask) {
    current_task.kernel().kthreads.spawn_future(async move {
        // Connect to the device on the main thread. The thread from which the task is being
        // spawned does not yet have an executor, so it cannot make an async FIDL connection.
        let channel = connect_to_device_channel().expect("Could not connect to hvdcpopti service");
        // Wake message_counter starts on 1 because set_processed_fifo_data gets called in response
        // to initial data, which is not passed through this event stream.
        let (proxy_channel, message_counter) =
            create_proxy_for_wake_events_counter(channel, "hvdcp_opti".to_string());
        *device_state.message_counter.lock() = Some(message_counter);
        run_qbg_device_event_loop(
            device_state,
            fhvdcpopti::DeviceProxy::new(fidl::AsyncChannel::from_channel(proxy_channel))
                .take_event_stream(),
        )
        .await;
    });
}

struct QbgDeviceFile {
    hvdcpopti: fhvdcpopti::DeviceSynchronousProxy,
    state: Arc<QbgDeviceState>,
}

impl QbgDeviceFile {
    pub fn new(current_task: &CurrentTask) -> Self {
        let state = Arc::new(QbgDeviceState::new());
        spawn_qbg_device_tasks(state.clone(), current_task);
        Self {
            hvdcpopti: connect_to_device().expect("Could not connect to hvdcpopti service"),
            state,
        }
    }
}

impl FileOps for QbgDeviceFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn ioctl(
        &self,
        _locked: &mut Locked<Unlocked>,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);

        match request {
            QBGIOCXCFG => {
                let config =
                    self.hvdcpopti.get_config(zx::MonotonicInstant::INFINITE).map_err(|e| {
                        log_error!("GetConfig failed: {:?}", e);
                        errno!(EINVAL)
                    })?;
                current_task.write_object(UserRef::new(user_addr), &config).map_err(|e| {
                    log_error!("GetConfig write_object failed: {:?}", e);
                    e
                })?;
                Ok(SUCCESS)
            }
            QBGIOCXEPR => {
                let params = self
                    .hvdcpopti
                    .get_essential_params(zx::MonotonicInstant::INFINITE)
                    .map_err(|e| {
                        log_error!("Failed to GetEssentialParams: {:?}", e);
                        errno!(EINVAL)
                    })?
                    .map_err(|e| {
                        log_error!("GetEssentialParams failed: {:?}", e);
                        errno!(EINVAL)
                    })?;
                current_task.write_object(UserRef::new(user_addr), &params).map_err(|e| {
                    log_error!("GetEssentialParams write_object failed: {:?}", e);
                    e
                })?;
                Ok(SUCCESS)
            }
            QBGIOCXEPW => {
                let params: [u8; fhvdcpopti::ESSENTIAL_PARAMS_LENGTH as usize] =
                    current_task.read_object(UserRef::new(user_addr)).map_err(|e| {
                        log_error!("SetEssentialParams read_object failed: {:?}", e);
                        e
                    })?;
                self.hvdcpopti
                    .set_essential_params(&params, zx::MonotonicInstant::INFINITE)
                    .map_err(|e| {
                        log_error!("Failed to SetEssentialParams: {:?}", e);
                        errno!(EINVAL)
                    })?
                    .map_err(|e| {
                        log_error!("SetEssentialParams failed: {:?}", e);
                        errno!(EINVAL)
                    })?;
                Ok(SUCCESS)
            }
            QBGIOCXSTEPCHGCFG => {
                let params = self
                    .hvdcpopti
                    .get_step_and_jeita_params(zx::MonotonicInstant::INFINITE)
                    .map_err(|e| {
                        log_error!("GetStepAndJeitaParams failed: {:?}", e);
                        errno!(EINVAL)
                    })?;
                current_task.write_object(UserRef::new(user_addr), &params).map_err(|e| {
                    log_error!("GetStepAndJeitaParams write_object failed: {:?}", e);
                    e
                })?;
                Ok(SUCCESS)
            }
            unknown_ioctl => {
                track_stub!(TODO("https://fxbug.dev/322874368"), "qbg ioctl", unknown_ioctl);
                error!(ENOSYS)
            }
        }?;

        Ok(SUCCESS)
    }

    fn read(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        buffer: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        file.blocking_op(locked, current_task, FdEvents::POLLIN | FdEvents::POLLHUP, None, |_| {
            let mut queue = self.state.read_queue.lock();
            if queue.is_empty() {
                return error!(EAGAIN);
            }

            // Try and pull as much data from the queue as possible to fill the buffer.
            while buffer.available() > 0 {
                let Some(data) = queue.front_mut() else {
                    break;
                };

                buffer.write_buffer(data)?;
                if data.available() == 0 {
                    queue.pop_front();
                }
            }

            Ok(buffer.bytes_written())
        })
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        buffer: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let data = buffer.read_all()?;
        let data_len = data.len();

        self.hvdcpopti
            .set_processed_fifo_data(&data.try_into().unwrap(), zx::MonotonicInstant::INFINITE)
            .map_err(|e| {
                log_error!("SetProcessedFifoData failed: {:?}", e);
                errno!(EINVAL)
            })?;
        self.state.message_counter.lock().as_ref().map(mark_proxy_message_handled);
        Ok(data_len)
    }

    fn wait_async(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        Some(self.state.waiters.wait_async_fd_events(waiter, events, handler))
    }

    fn query_events(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let mut events = FdEvents::POLLOUT;
        if !self.state.read_queue.lock().is_empty() {
            events |= FdEvents::POLLIN;
        }
        Ok(events)
    }
}
