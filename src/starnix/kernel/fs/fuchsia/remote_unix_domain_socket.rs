// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::fuchsia::{new_remote_file, OpenFlags};
use crate::task::{
    CurrentTask, EventHandler, SignalHandler, SignalHandlerInner, WaitCanceler, Waiter,
};
use crate::vfs::buffers::{InputBuffer, OutputBuffer};
use crate::vfs::socket::{
    Socket, SocketAddress, SocketDomain, SocketHandle, SocketMessageFlags, SocketOps, SocketPeer,
    SocketProtocol, SocketShutdownFlags, SocketType,
};
use crate::vfs::{AncillaryData, FileHandle, MessageReadInfo, UnixControlData};
use fidl::endpoints::SynchronousProxy;
use linux_uapi::{SOL_SOCKET, SO_LINGER};
use starnix_sync::{FileOpsCore, Locked};
use starnix_types::user_buffer::UserBuffer;
use starnix_uapi::errors::Errno;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{errno, error, from_status_like_fdio, uapi, ucred};
use std::sync::Arc;
use zerocopy::IntoBytes;
use zx::AsHandleRef;
use {fidl_fuchsia_io as fio, fidl_fuchsia_starnix_binder as fbinder};

static READABLE_SIGNAL: zx::Signals =
    zx::Signals::from_bits_retain(fio::FileSignal::READABLE.bits());
static WRITABLE_SIGNAL: zx::Signals =
    zx::Signals::from_bits_retain(fio::FileSignal::WRITABLE.bits());

pub struct RemoteUnixDomainSocket {
    client: fbinder::UnixDomainSocketSynchronousProxy,
    event: Arc<zx::EventPair>,
}

impl RemoteUnixDomainSocket {
    pub fn new(channel: zx::Channel) -> Result<Self, Errno> {
        let client = fbinder::UnixDomainSocketSynchronousProxy::from_channel(channel);
        let response = client
            .get_event(
                &fbinder::UnixDomainSocketGetEventRequest::default(),
                zx::MonotonicInstant::INFINITE,
            )
            .map_err(|_| errno!(ECONNREFUSED))?
            .map_err(|e: i32| from_status_like_fdio!(zx::Status::from_raw(e)))?;
        let event = response.event.ok_or_else(|| errno!(ECONNREFUSED))?;
        Ok(Self { client, event: event.into() })
    }

    fn get_signals_from_events(events: FdEvents) -> zx::Signals {
        let mut signals = zx::Signals::NONE;
        if events.contains(FdEvents::POLLIN) {
            signals |= READABLE_SIGNAL;
        }
        if events.contains(FdEvents::POLLOUT) {
            signals |= WRITABLE_SIGNAL;
        }
        signals
    }

    fn get_events_from_signals(signals: zx::Signals) -> FdEvents {
        let mut events = FdEvents::empty();
        if signals.contains(READABLE_SIGNAL) {
            events |= FdEvents::POLLIN;
        }
        if signals.contains(WRITABLE_SIGNAL) {
            events |= FdEvents::POLLOUT;
        }
        events
    }
}

impl SocketOps for RemoteUnixDomainSocket {
    fn get_socket_info(&self) -> Result<(SocketDomain, SocketType, SocketProtocol), Errno> {
        Ok((SocketDomain::Unix, SocketType::Datagram, SocketProtocol::from_raw(0)))
    }

    fn connect(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &SocketHandle,
        _current_task: &CurrentTask,
        _peer: SocketPeer,
    ) -> Result<(), Errno> {
        error!(EISCONN)
    }

    fn listen(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        _backlog: i32,
        _credentials: ucred,
    ) -> Result<(), Errno> {
        error!(EOPNOTSUPP)
    }

    fn accept(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
    ) -> Result<SocketHandle, Errno> {
        error!(EOPNOTSUPP)
    }

    fn bind(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _socket_address: SocketAddress,
    ) -> Result<(), Errno> {
        error!(EOPNOTSUPP)
    }

    fn read(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        current_task: &CurrentTask,
        data: &mut dyn OutputBuffer,
        flags: SocketMessageFlags,
    ) -> Result<MessageReadInfo, Errno> {
        if self.client.is_closed().map_err(|_| errno!(ECONNREFUSED))? {
            return error!(ECONNREFUSED);
        }
        let mut read_flags = fbinder::ReadFlags::empty();
        if flags.contains(SocketMessageFlags::PEEK) {
            read_flags |= fbinder::ReadFlags::PEEK;
        }

        let response = self
            .client
            .read(
                &fbinder::UnixDomainSocketReadRequest {
                    count: Some(data.available() as u64),
                    flags: Some(read_flags),
                    ..Default::default()
                },
                zx::MonotonicInstant::INFINITE,
            )
            .map_err(|_| errno!(ECONNREFUSED))?
            .map_err(|e: i32| from_status_like_fdio!(zx::Status::from_raw(e)))?;

        let written =
            if let Some(received_data) = response.data { data.write(&received_data)? } else { 0 };

        let mut file_handles: Vec<FileHandle> = vec![];
        if let Some(handles) = response.handles {
            for handle in handles {
                file_handles.push(new_remote_file(current_task, handle, OpenFlags::RDWR)?);
            }
        }
        let ancillary_data = vec![AncillaryData::Unix(UnixControlData::Rights(file_handles))];

        let message_length = response.data_original_length.unwrap_or(written as u64) as usize;

        Ok(MessageReadInfo { bytes_read: written, message_length, address: None, ancillary_data })
    }

    fn write(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        current_task: &CurrentTask,
        data: &mut dyn InputBuffer,
        _dest_address: &mut Option<SocketAddress>,
        ancillary_data: &mut Vec<AncillaryData>,
    ) -> Result<usize, Errno> {
        if self.client.is_closed().map_err(|_| errno!(ECONNREFUSED))? {
            return error!(ECONNREFUSED);
        }

        let mut handles: Vec<zx::Handle> = vec![];
        for data in ancillary_data {
            match data {
                AncillaryData::Unix(UnixControlData::Rights(file_handles)) => {
                    for file_handle in file_handles {
                        let Some(handle) = file_handle.to_handle(current_task)? else {
                            return error!(EINVAL);
                        };
                        handles.push(handle);
                    }
                }
                _ => return error!(EINVAL),
            }
        }

        let bytes = data.read_all()?;

        let response = self
            .client
            .write(
                fbinder::UnixDomainSocketWriteRequest {
                    data: Some(bytes),
                    handles: Some(handles),
                    ..Default::default()
                },
                zx::MonotonicInstant::INFINITE,
            )
            .map_err(|_| errno!(ECONNREFUSED))?
            .map_err(|e: i32| from_status_like_fdio!(zx::Status::from_raw(e)))?;

        let written = response.actual_count.unwrap_or(0);
        Ok(written as usize)
    }

    fn wait_async(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> WaitCanceler {
        let signal_handler = SignalHandler {
            inner: SignalHandlerInner::ZxHandle(Self::get_events_from_signals),
            event_handler: handler,
            err_code: None,
        };
        let canceler = waiter
            .wake_on_zircon_signals(
                self.event.as_ref(),
                Self::get_signals_from_events(events),
                signal_handler,
            )
            .unwrap();
        WaitCanceler::new_event_pair(Arc::downgrade(&self.event), canceler)
    }

    fn query_events(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let signals = self
            .event
            .as_handle_ref()
            .wait(zx::Signals::NONE, zx::MonotonicInstant::INFINITE_PAST)
            .map_err(|e| from_status_like_fdio!(e))?;
        Ok(Self::get_events_from_signals(signals))
    }

    fn shutdown(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        _how: SocketShutdownFlags,
    ) -> Result<(), Errno> {
        Ok(())
    }

    fn close(&self, _locked: &mut Locked<'_, FileOpsCore>, _socket: &Socket) {
        let _ = self.client.close(zx::MonotonicInstant::ZERO);
    }

    fn getsockname(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
    ) -> Result<SocketAddress, Errno> {
        Ok(SocketAddress::default_for_domain(SocketDomain::Unix))
    }

    fn getpeername(
        &self,
        locked: &mut Locked<'_, FileOpsCore>,
        socket: &Socket,
    ) -> Result<SocketAddress, Errno> {
        self.getsockname(locked, socket)
    }

    fn setsockopt(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _level: u32,
        _optname: u32,
        _user_opt: UserBuffer,
    ) -> Result<(), Errno> {
        error!(EOPNOTSUPP)
    }

    fn getsockopt(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        level: u32,
        optname: u32,
        _optlen: u32,
    ) -> Result<Vec<u8>, Errno> {
        if level != SOL_SOCKET {
            return error!(EINVAL);
        }
        let data = match optname {
            SO_LINGER => uapi::linger::default().as_bytes().to_vec(),
            _ => return error!(EINVAL),
        };

        Ok(data)
    }

    fn to_handle(
        &self,
        _socket: &Socket,
        _current_task: &CurrentTask,
    ) -> Result<Option<zx::Handle>, Errno> {
        let (proxy, server) = zx::Channel::create();
        self.client.clone(server.into()).map_err(|_| errno!(ECONNREFUSED))?;
        Ok(Some(proxy.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use crate::vfs::socket::SocketFile;
    use crate::vfs::{VecInputBuffer, VecOutputBuffer};
    use fidl::endpoints::RequestStream;
    use futures::StreamExt;
    use starnix_sync::Mutex;
    use zx::HandleBased;
    use {fidl_fuchsia_unknown as funknown, fuchsia_async as fasync};

    #[derive(Debug)]
    struct Data {
        bytes: Vec<u8>,
        handles: Vec<zx::Handle>,
    }

    impl Data {
        fn try_clone(&mut self) -> Result<Self, zx::Status> {
            let mut new_handles = vec![];
            for handle in std::mem::take(&mut self.handles) {
                let (new_handle, old_handle) = {
                    let handle_type = handle.basic_info()?.object_type;
                    match handle_type {
                        zx::ObjectType::CHANNEL => {
                            let channel = zx::Channel::from(handle);
                            let client = funknown::CloneableSynchronousProxy::new(channel);
                            let (proxy, server) = zx::Channel::create();
                            let new_handle = client
                                .clone(server.into())
                                .map(|_| proxy.into())
                                .map_err(|_| zx::Status::NOT_SUPPORTED);
                            (new_handle, client.into_channel().into_handle())
                        }
                        _ => {
                            let new_handle = handle.duplicate_handle(zx::Rights::SAME_RIGHTS);
                            (new_handle, handle)
                        }
                    }
                };
                self.handles.push(old_handle);
                new_handles.push(new_handle);
            }
            let new_handles =
                new_handles.into_iter().collect::<Result<Vec<zx::Handle>, zx::Status>>()?;
            Ok(Self { bytes: self.bytes.clone(), handles: new_handles })
        }
    }

    #[derive(Debug)]
    struct UnixDomainSocketImplState {
        _local_event: zx::EventPair,
        remote_event: zx::EventPair,
        buffer: Vec<Data>,
    }

    impl Default for UnixDomainSocketImplState {
        fn default() -> Self {
            let (_local_event, remote_event) = zx::EventPair::create();
            Self { _local_event, remote_event, buffer: vec![] }
        }
    }

    #[derive(Debug, Default)]
    struct UnixDomainSocketImpl {
        state: Mutex<UnixDomainSocketImplState>,
    }

    impl UnixDomainSocketImpl {
        fn read(
            &self,
            payload: fbinder::UnixDomainSocketReadRequest,
        ) -> Result<fbinder::UnixDomainSocketReadResponse, zx::Status> {
            let Some(count) = payload.count else {
                return Err(zx::Status::INVALID_ARGS);
            };
            let Some(flags) = payload.flags else {
                return Err(zx::Status::INVALID_ARGS);
            };
            let mut state = self.state.lock();
            if state.buffer.is_empty() {
                return Err(zx::Status::SHOULD_WAIT);
            }
            let mut data = if flags.contains(fbinder::ReadFlags::PEEK) {
                state.buffer[0].try_clone()?
            } else {
                state.buffer.remove(0)
            };

            if state.buffer.is_empty() {
                state.remote_event.as_handle_ref().signal(READABLE_SIGNAL, WRITABLE_SIGNAL)?;
            }

            let actual_count = data.bytes.len() as u64;
            data.bytes.truncate(count as usize);

            Ok(fbinder::UnixDomainSocketReadResponse {
                data: Some(data.bytes),
                data_original_length: Some(actual_count),
                handles: Some(data.handles),
                ..Default::default()
            })
        }

        fn write(
            &self,
            payload: fbinder::UnixDomainSocketWriteRequest,
        ) -> Result<fbinder::UnixDomainSocketWriteResponse, zx::Status> {
            let Some(bytes) = payload.data else {
                return Err(zx::Status::INVALID_ARGS);
            };
            let actual_count = bytes.len() as u64;
            let Some(handles) = payload.handles else {
                return Err(zx::Status::INVALID_ARGS);
            };
            let mut state = self.state.lock();
            state.buffer.push(Data { bytes, handles });
            state
                .remote_event
                .as_handle_ref()
                .signal(zx::Signals::NONE, READABLE_SIGNAL | WRITABLE_SIGNAL)?;
            Ok(fbinder::UnixDomainSocketWriteResponse {
                actual_count: Some(actual_count),
                ..Default::default()
            })
        }

        async fn serve(self: &Arc<Self>, channel: zx::Channel) {
            let stream = fbinder::UnixDomainSocketRequestStream::from_channel(
                fasync::Channel::from_channel(channel),
            );
            stream
                .for_each_concurrent(None, |message| async {
                    match message {
                        Ok(fbinder::UnixDomainSocketRequest::GetEvent { responder, .. }) => {
                            let state = self.state.lock();
                            let event = state
                                .remote_event
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("duplicate event");
                            responder
                                .send(Ok(fbinder::UnixDomainSocketGetEventResponse {
                                    event: Some(event),
                                    ..Default::default()
                                }))
                                .expect("respond");
                        }
                        Ok(fbinder::UnixDomainSocketRequest::Read {
                            payload, responder, ..
                        }) => {
                            assert!(responder
                                .send(self.read(payload).map_err(|e| e.into_raw()))
                                .is_ok());
                        }
                        Ok(fbinder::UnixDomainSocketRequest::Write {
                            payload, responder, ..
                        }) => {
                            assert!(responder
                                .send(self.write(payload).as_ref().map_err(|e| e.into_raw()))
                                .is_ok());
                        }
                        Ok(fbinder::UnixDomainSocketRequest::Query { responder }) => {
                            assert!(responder
                                .send(fbinder::UNIX_DOMAIN_SOCKET_PROTOCOL_NAME.as_bytes())
                                .is_ok());
                        }
                        Ok(fbinder::UnixDomainSocketRequest::Clone { request, .. }) => {
                            self.serve(request.into()).await;
                        }
                        Ok(fbinder::UnixDomainSocketRequest::Close { responder }) => {
                            assert!(responder.send(Ok(())).is_ok());
                        }
                        _ => {
                            return;
                        }
                    }
                })
                .await;
        }
    }

    #[::fuchsia::test]
    async fn test_remote_uds() {
        let (client, server) = zx::Channel::create();
        let handle = std::thread::spawn(|| {
            let mut executor = fasync::LocalExecutor::new();
            executor.run_singlethreaded(async move {
                let uds_impl = UnixDomainSocketImpl::default();
                Arc::new(uds_impl).serve(server).await;
            });
        });
        spawn_kernel_and_run(move |locked, current_task| {
            let original_file = new_remote_file(current_task, client.into(), OpenFlags::RDWR)
                .expect("new_remote_file");
            assert!(original_file.node().is_sock());
            let file = new_remote_file(
                current_task,
                original_file.to_handle(current_task).expect("to_handle").expect("has_handle"),
                OpenFlags::RDWR,
            )
            .expect("new_remote_file");
            let ancillary_data =
                vec![AncillaryData::Unix(UnixControlData::Rights(vec![original_file]))];
            let socket_ops = file.downcast_file::<SocketFile>().unwrap();
            let data = "HelloWorld";
            let mut input_buffer = VecInputBuffer::new(data.as_bytes());
            assert_eq!(
                socket_ops.sendmsg(
                    locked,
                    current_task,
                    &file,
                    &mut input_buffer,
                    None,
                    ancillary_data,
                    SocketMessageFlags::empty()
                ),
                Ok(data.len())
            );

            let flags = SocketMessageFlags::CTRUNC
                | SocketMessageFlags::TRUNC
                | SocketMessageFlags::NOSIGNAL
                | SocketMessageFlags::CMSG_CLOEXEC;

            let mut buffer = VecOutputBuffer::new(1024);
            let info = socket_ops
                .recvmsg(
                    locked,
                    &current_task,
                    &file,
                    &mut buffer,
                    flags | SocketMessageFlags::PEEK,
                    None,
                )
                .expect("recvmsg");

            assert_eq!(info.ancillary_data.len(), 1);
            assert_eq!(info.message_length, data.len());

            let mut buffer = VecOutputBuffer::new(1024);
            let info = socket_ops
                .recvmsg(locked, &current_task, &file, &mut buffer, flags, None)
                .expect("recvmsg");

            assert_eq!(info.ancillary_data.len(), 1);
            assert_eq!(info.message_length, data.len());

            let mut buffer = VecOutputBuffer::new(1024);
            let err = socket_ops
                .recvmsg(
                    locked,
                    &current_task,
                    &file,
                    &mut buffer,
                    flags | SocketMessageFlags::DONTWAIT,
                    None,
                )
                .unwrap_err();
            assert_eq!(err, errno!(EAGAIN));
        });
        handle.join().expect("join");
    }
}
