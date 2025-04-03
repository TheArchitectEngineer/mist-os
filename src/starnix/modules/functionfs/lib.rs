// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::SynchronousProxy;
use fidl_fuchsia_hardware_adb as fadb;
use futures_util::StreamExt;
use starnix_core::power::create_proxy_for_wake_events_counter;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::{
    fileops_impl_noop_sync, fileops_impl_seekless, fs_args, fs_node_impl_dir_readonly,
    fs_node_impl_not_dir, CacheMode, DirectoryEntryType, FileObject, FileOps, FileSystem,
    FileSystemHandle, FileSystemOps, FileSystemOptions, FsNode, FsNodeInfo, FsNodeOps, FsStr,
    InputBuffer, OutputBuffer, VecDirectory, VecDirectoryEntry,
};
use starnix_logging::{log_error, log_warn, track_stub};
use starnix_sync::{FileOpsCore, Locked, Mutex, Unlocked};
use starnix_types::vfs::default_statfs;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    errno, error, gid_t, ino_t, statfs, uid_t, usb_functionfs_event,
    usb_functionfs_event_type_FUNCTIONFS_BIND, usb_functionfs_event_type_FUNCTIONFS_ENABLE,
};
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::mpsc;
use zerocopy::IntoBytes;

// The node identifiers of different nodes in FunctionFS.
const ROOT_NODE_ID: ino_t = 1;

// Control endpoint is always present in a mounted FunctionFS.
const CONTROL_ENDPOINT: &str = "ep0";
const CONTROL_ENDPOINT_NODE_ID: ino_t = 2;

const OUTPUT_ENDPOINT: &str = "ep1";
const OUTPUT_ENDPOINT_NODE_ID: ino_t = 3;

const INPUT_ENDPOINT: &str = "ep2";
const INPUT_ENDPOINT_NODE_ID: ino_t = 4;

// Magic number of the file system, different from the magic used for Descriptors and Strings.
// Set to the same value as Linux.
const FUNCTIONFS_MAGIC: u32 = 0xa647361;

const ADB_DIRECTORY: &str = "/svc/fuchsia.hardware.adb.Service";

struct ReadCommand {
    response_sender: mpsc::Sender<Result<Vec<u8>, Errno>>,
}

struct WriteCommand {
    data: Vec<u8>,
    response_sender: mpsc::Sender<Result<usize, Errno>>,
}

/// Handle all of the ADB messages in an async context.
/// We receive commands from the main thread and then proxy them into the ADB channel.
/// We want to hold the wakelock until we have at least one outstanding read, because we
/// are always woken up on a new message. (If we have no outstanding reads we will not
/// receive any new messages).
///
/// At the same time we still need to handle writes and events. These are handled by always
/// clearing the proxy signal, but only clearing the kernel signal if we have an outstanding read.
async fn handle_adb(
    proxy: fadb::UsbAdbImpl_Proxy,
    message_counter: Option<zx::Counter>,
    read_commands: async_channel::Receiver<ReadCommand>,
    write_commands: async_channel::Receiver<WriteCommand>,
) {
    /// Handle all of the events coming from the ADB device.
    async fn handle_events(
        mut stream: fadb::UsbAdbImpl_EventStream,
        message_counter: &Option<zx::Counter>,
    ) {
        while let Some(_next) = stream.next().await {
            // We can simply clear this after getting a response because we care about
            // reads. Allow new FIDL messages to come through and only go to sleep if
            // we have an outstanding read.
            message_counter.as_ref().map(|c| {
                c.add(-1).expect("Failed message decrement");
            });
        }
    }

    /// Handle the commands coming from the main thread.
    async fn handle_read_commands(
        proxy: &fadb::UsbAdbImpl_Proxy,
        message_counter: &Option<zx::Counter>,
        commands: async_channel::Receiver<ReadCommand>,
    ) {
        let message_counter = message_counter.as_ref();
        commands
            .for_each(|ReadCommand { response_sender }| async move {
                // Queue up our receive future. We want to do this before we decrement the counter,
                // which potentially allows the container to suspend.
                let receive_future = proxy.receive();

                // The message is queued in the channel, so now we can decrement the unhandled
                // message counter to make sure we aren't preventing the container from suspending.
                message_counter.as_ref().map(|c| {
                    c.add(-1).expect("Failed message decrement");
                });

                let response = match receive_future.await {
                    Err(err) => {
                        log_warn!("Failed to call UsbAdbImpl.Receive: {err}");
                        error!(EINVAL)
                    }
                    Ok(Err(err)) => {
                        log_warn!("Failed to receive data from adb driver: {err}");
                        error!(EINVAL)
                    }
                    Ok(Ok(payload)) => Ok(payload),
                };

                response_sender
                    .send(response)
                    .map_err(|e| log_error!("Failed to send to main thread: {:#?}", e))
                    .ok();
            })
            .await;
    }

    /// Handle the commands coming from the main thread.
    async fn handle_write_commands(
        proxy: &fadb::UsbAdbImpl_Proxy,
        message_counter: &Option<zx::Counter>,
        commands: async_channel::Receiver<WriteCommand>,
    ) {
        commands
            .for_each(|WriteCommand { data, response_sender }| async move {
                let response = match proxy.queue_tx(&data).await {
                    Err(err) => {
                        log_warn!("Failed to call UsbAdbImpl.QueueTx: {err}");
                        error!(EINVAL)
                    }
                    Ok(Err(err)) => {
                        log_warn!("Failed to queue data to adb driver: {err}");
                        error!(EINVAL)
                    }
                    Ok(Ok(_)) => Ok(data.len()),
                };

                // We can simply decrement this after getting a response because responses to
                // writes from the container to the host are not expected to wake the container.
                message_counter.as_ref().map(|c| {
                    c.add(-1).expect("Failed message decrement");
                });
                response_sender
                    .send(response)
                    .map_err(|e| log_error!("Failed to send to main thread: {:#?}", e))
                    .ok();
            })
            .await;
    }

    // Run our three futures at the same time.
    let event_future = handle_events(proxy.take_event_stream(), &message_counter);
    let write_commands_future = handle_write_commands(&proxy, &message_counter, write_commands);
    let read_commands_future = handle_read_commands(&proxy, &message_counter, read_commands);
    futures::join!(event_future, write_commands_future, read_commands_future);
}

pub struct FunctionFs;
impl FunctionFs {
    pub fn new_fs(
        _locked: &mut Locked<'_, Unlocked>,
        current_task: &CurrentTask,
        options: FileSystemOptions,
    ) -> Result<FileSystemHandle, Errno> {
        if options.source != "adb" {
            track_stub!(TODO("https://fxbug.dev/329699340"), "FunctionFS supports other uses");
            return error!(ENODEV);
        }

        // ADB daemon assumes that ADB works over USB if FunctionFS is able to mount.
        // Check that the ADB directory capability is provided to the kernel, and fail to mount
        // if it is not.
        if let Err(e) = std::fs::read_dir(ADB_DIRECTORY) {
            log_warn!(
                "Attempted to mount FunctionFS for adb, but could not read {ADB_DIRECTORY}: {e}"
            );
            return error!(ENODEV);
        }

        let uid = if let Some(uid) = options.params.get(b"uid") {
            fs_args::parse::<uid_t>(uid.as_ref())?
        } else {
            0
        };
        let gid = if let Some(gid) = options.params.get(b"gid") {
            fs_args::parse::<gid_t>(gid.as_ref())?
        } else {
            0
        };

        let fs = FileSystem::new(current_task.kernel(), CacheMode::Uncached, FunctionFs, options)?;

        let mut root = FsNode::new_root_with_properties(FunctionFsRootDir::default(), |info| {
            info.ino = ROOT_NODE_ID;
            info.uid = uid;
            info.gid = gid;
        });
        root.node_id = ROOT_NODE_ID;
        fs.set_root_node(root);

        Ok(fs)
    }
}

impl FileSystemOps for FunctionFs {
    fn statfs(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _fs: &FileSystem,
        _current_task: &CurrentTask,
    ) -> Result<statfs, Errno> {
        Ok(default_statfs(FUNCTIONFS_MAGIC))
    }

    fn name(&self) -> &'static FsStr {
        b"functionfs".into()
    }
}

#[derive(Default)]
struct FunctionFsState {
    // Keeps track of the number of FileObject's created for the control endpoint.
    // When all FileObjects are closed, the filesystem resets to its initial state.
    // See https://docs.kernel.org/usb/functionfs.html.
    num_control_file_objects: usize,

    // Whether the FunctionFS has input/output endpoints, which are /ep2 and /ep1
    // respectively. /ep0 is the control endpoint and is always available.
    has_input_output_endpoints: bool,

    adb_read_channel: Option<async_channel::Sender<ReadCommand>>,
    adb_write_channel: Option<async_channel::Sender<WriteCommand>>,

    // FIDL binding to the adb driver, for start and stop calls.
    device_proxy: Option<fadb::DeviceSynchronousProxy>,

    // FunctionFs events that indicate the connection state, to be read through
    // the control endpoint.
    event_queue: VecDeque<usb_functionfs_event>,
}

pub enum AdbProxyMode {
    /// Don't proxy events at all.
    None,

    /// Have the Starnix runner proxy events such that the container
    /// will wake up if events are received while the container is
    /// suspended.
    WakeContainer,
}

fn connect_to_device(
    proxy: AdbProxyMode,
) -> Result<
    (fadb::DeviceSynchronousProxy, fadb::UsbAdbImpl_SynchronousProxy, Option<zx::Counter>),
    Errno,
> {
    let mut dir = std::fs::read_dir(ADB_DIRECTORY).map_err(|_| errno!(EINVAL))?;

    let Some(Ok(entry)) = dir.next() else {
        return error!(EBUSY);
    };
    let path =
        entry.path().join("adb").into_os_string().into_string().map_err(|_| errno!(EINVAL))?;

    let (client_channel, server_channel) = zx::Channel::create();
    fdio::service_connect(&path, server_channel).map_err(|_| errno!(EINVAL))?;
    let device_proxy = fadb::DeviceSynchronousProxy::new(client_channel);

    let (adb_proxy, server_end) = fidl::endpoints::create_sync_proxy::<fadb::UsbAdbImpl_Marker>();
    let (adb_proxy, message_counter) = match proxy {
        AdbProxyMode::None => (adb_proxy, None),
        AdbProxyMode::WakeContainer => {
            let (adb_proxy, message_counter) =
                create_proxy_for_wake_events_counter(adb_proxy.into_channel(), "adb".to_string());
            let adb_proxy = fadb::UsbAdbImpl_SynchronousProxy::from_channel(adb_proxy);
            (adb_proxy, Some(message_counter))
        }
    };

    device_proxy
        .start_adb(server_end, zx::MonotonicInstant::INFINITE)
        .map_err(|_| errno!(EINVAL))?
        .map_err(|_| errno!(EINVAL))?;

    loop {
        let fadb::UsbAdbImpl_Event::OnStatusChanged { status } = adb_proxy
            .wait_for_event(zx::MonotonicInstant::INFINITE)
            .expect("failed to wait for event");

        // Decrement the counter after we receive a response, since we don't need to schedule
        // another message before allowing the container to suspend.
        message_counter.as_ref().map(|c| {
            c.add(-1).expect("Failed to decrement");
        });

        if status == fadb::StatusFlags::ONLINE {
            break;
        }
    }
    return Ok((device_proxy, adb_proxy, message_counter));
}

#[derive(Default)]
struct FunctionFsRootDir {
    state: Mutex<FunctionFsState>,
}

impl FunctionFsRootDir {
    fn create_endpoints(&self, kernel: &Kernel) -> Result<(), Errno> {
        let mut state = self.state.lock();

        // create_endpoints can be called multiple times as descriptors are written
        // to the control endpoint.
        if state.has_input_output_endpoints {
            return Ok(());
        }
        let (device_proxy, adb_proxy, message_counter) =
            connect_to_device(AdbProxyMode::WakeContainer)?;
        state.device_proxy = Some(device_proxy);

        let (read_command_sender, read_command_receiver) = async_channel::unbounded();
        state.adb_read_channel = Some(read_command_sender);

        let (write_command_sender, write_command_receiver) = async_channel::unbounded();
        state.adb_write_channel = Some(write_command_sender);

        // Spawn our future that will handle all of the ADB messages.
        kernel.kthreads.spawn_future(async move {
            let adb_proxy = fadb::UsbAdbImpl_Proxy::new(fidl::AsyncChannel::from_channel(
                adb_proxy.into_channel(),
            ));
            handle_adb(adb_proxy, message_counter, read_command_receiver, write_command_receiver)
                .await
        });

        state.has_input_output_endpoints = true;

        // Currently FunctionFS assumes the device is always online.
        track_stub!(TODO("https://fxbug.dev/329699340"), "FunctionFS correctly handles USB events");

        state.event_queue.push_back(usb_functionfs_event {
            type_: usb_functionfs_event_type_FUNCTIONFS_BIND as u8,
            ..Default::default()
        });
        state.event_queue.push_back(usb_functionfs_event {
            type_: usb_functionfs_event_type_FUNCTIONFS_ENABLE as u8,
            ..Default::default()
        });
        Ok(())
    }

    fn from_file(file: &FileObject) -> &Self {
        file.fs
            .root()
            .node
            .downcast_ops::<FunctionFsRootDir>()
            .expect("failed to downcast functionfs root dir")
    }

    fn on_control_opened(&self) {
        let mut state = self.state.lock();
        state.num_control_file_objects += 1;
    }

    fn on_control_closed(&self) {
        let mut state = self.state.lock();
        state.num_control_file_objects -= 1;
        if state.num_control_file_objects == 0 {
            // When all control endpoints are closed, the filesystem resets to its initial state.
            state.has_input_output_endpoints = false;
            state.adb_read_channel = None;
            state.adb_write_channel = None;
            state.event_queue.clear();

            if let Some(device_proxy) = state.device_proxy.as_ref() {
                let _ = device_proxy
                    .stop_adb(zx::MonotonicInstant::INFINITE)
                    .map_err(|_| errno!(EINVAL));
            }
        }
    }

    fn available(&self) -> usize {
        let state = self.state.lock();
        state.event_queue.len()
    }

    fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
        let data = Vec::from(bytes);
        let (response_sender, receiver) = std::sync::mpsc::channel();
        if let Some(channel) = self.state.lock().adb_write_channel.as_ref() {
            channel
                .send_blocking(WriteCommand { data, response_sender })
                .map_err(|_| errno!(EINVAL))?;
        } else {
            return error!(ENODEV);
        }
        receiver.recv().map_err(|_| errno!(EINVAL))?
    }

    fn read(&self) -> Result<Vec<u8>, Errno> {
        let (response_sender, receiver) = std::sync::mpsc::channel();
        if let Some(channel) = self.state.lock().adb_read_channel.as_ref() {
            channel.send_blocking(ReadCommand { response_sender }).map_err(|_| errno!(EINVAL))?;
        } else {
            return error!(ENODEV);
        }
        receiver.recv().map_err(|_| errno!(EINVAL))?
    }
}

impl FsNodeOps for FunctionFsRootDir {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let mut entries = vec![];
        entries.push(VecDirectoryEntry {
            entry_type: DirectoryEntryType::REG,
            name: CONTROL_ENDPOINT.into(),
            inode: Some(CONTROL_ENDPOINT_NODE_ID),
        });

        let state = self.state.lock();
        if state.has_input_output_endpoints {
            entries.push(VecDirectoryEntry {
                entry_type: DirectoryEntryType::REG,
                name: INPUT_ENDPOINT.into(),
                inode: Some(INPUT_ENDPOINT_NODE_ID),
            });
            entries.push(VecDirectoryEntry {
                entry_type: DirectoryEntryType::REG,
                name: OUTPUT_ENDPOINT.into(),
                inode: Some(OUTPUT_ENDPOINT_NODE_ID),
            });
        }

        Ok(VecDirectory::new_file(entries))
    }

    fn lookup(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<starnix_core::vfs::FsNodeHandle, Errno> {
        let name = std::str::from_utf8(name).map_err(|_| errno!(ENOENT))?;
        match name {
            CONTROL_ENDPOINT => Ok(node.fs().create_node_with_id(
                current_task,
                FunctionFsControlEndpoint,
                CONTROL_ENDPOINT_NODE_ID,
                FsNodeInfo::new(CONTROL_ENDPOINT_NODE_ID, mode!(IFREG, 0o600), node.info().cred()),
            )),
            OUTPUT_ENDPOINT => Ok(node.fs().create_node_with_id(
                current_task,
                FunctionFsOutputEndpoint,
                OUTPUT_ENDPOINT_NODE_ID,
                FsNodeInfo::new(OUTPUT_ENDPOINT_NODE_ID, mode!(IFREG, 0o600), node.info().cred()),
            )),
            INPUT_ENDPOINT => Ok(node.fs().create_node_with_id(
                current_task,
                FunctionFsInputEndpoint,
                INPUT_ENDPOINT_NODE_ID,
                FsNodeInfo::new(INPUT_ENDPOINT_NODE_ID, mode!(IFREG, 0o600), node.info().cred()),
            )),
            _ => error!(ENOENT),
        }
    }
}

// FunctionFS Control Endpoint is both readable and writable.
// Clients should write USB descriptors to the endpoint to setup the USB connection.
// Clients can read `usb_functionfs_event`s to know about the USB connection state.
struct FunctionFsControlEndpoint;
impl FsNodeOps for FunctionFsControlEndpoint {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let fs = node.fs();
        let rootdir = fs
            .root()
            .node
            .downcast_ops::<FunctionFsRootDir>()
            .expect("failed to downcast functionfs root dir");
        rootdir.on_control_opened();
        Ok(Box::new(FunctionFsControlEndpoint))
    }
}

impl FileOps for FunctionFsControlEndpoint {
    fileops_impl_seekless!();
    fileops_impl_noop_sync!();

    fn close(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
    ) {
        let rootdir = FunctionFsRootDir::from_file(file);
        rootdir.on_control_closed();
    }

    fn read(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        // The control endpoint does not currently implement blocking read.
        // ADB would only read from this endpoint after polling it.
        track_stub!(
            TODO("https://fxbug.dev/329699340"),
            "FunctionFS blocking read on control endpoint"
        );

        let rootdir = FunctionFsRootDir::from_file(file);

        let mut state = rootdir.state.lock();
        if !state.event_queue.is_empty() {
            if data.available() < std::mem::size_of::<usb_functionfs_event>() {
                return error!(EINVAL);
            }
        } else {
            return error!(EAGAIN);
        }
        let front = state.event_queue.pop_front().expect("pop from non-empty event queue");
        data.write(front.as_bytes())
    }

    fn write(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        // The ADB driver creates and passes its own descriptors to the host system over the wire,
        // and so, Starnix does not need to parse the descriptors that Android sends.
        // Here we directly attempt to connect to the driver via FIDL, and create endpoints for data transfer.
        track_stub!(TODO("https://fxbug.dev/329699340"), "FunctionFS should parse descriptors");

        let rootdir = FunctionFsRootDir::from_file(file);
        rootdir.create_endpoints(current_task.kernel().deref())?;

        Ok(data.drain())
    }

    fn query_events(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let rootdir = FunctionFsRootDir::from_file(file);
        if rootdir.available() > 0 {
            Ok(FdEvents::POLLIN)
        } else {
            Ok(FdEvents::empty())
        }
    }
}

// FunctionFSInputEndpoint is device to host communication, a.k.a. the "IN" USB direction.
// This endpoint is writable, and not readable.
struct FunctionFsInputEndpoint;
impl FsNodeOps for FunctionFsInputEndpoint {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(FunctionFsInputEndpoint))
    }
}

impl FileOps for FunctionFsInputEndpoint {
    fileops_impl_seekless!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        error!(EINVAL)
    }

    fn write(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let bytes = data.read_all()?;
        let rootdir = FunctionFsRootDir::from_file(file);
        rootdir.write(&bytes)
    }
}

// FunctionFSOutputEndpoint is host to device communication, a.k.a. the "OUT" USB direction.
// This endpoint is readable, and not writable.
struct FunctionFsOutputEndpoint;
impl FsNodeOps for FunctionFsOutputEndpoint {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(FunctionFsOutputFileObject))
    }
}

struct FunctionFsOutputFileObject;

impl FileOps for FunctionFsOutputFileObject {
    fileops_impl_seekless!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let rootdir = FunctionFsRootDir::from_file(file);
        let payload = rootdir.read()?;
        if payload.len() > data.available() {
            // This means the data will only be partially written, with the rest discarded.
            // Instead of attempting this, we'll instead return error to the client.
            return error!(EINVAL);
        }

        data.write(&payload)
    }

    fn write(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        error!(EINVAL)
    }
}
