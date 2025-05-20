// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(not(feature = "starnix_lite"))]
use crate::bpf::fs::get_bpf_object;
#[cfg(not(feature = "starnix_lite"))]
use crate::mm::MemoryAccessorExt;
use crate::security;
use crate::task::{CurrentTask, EventHandler, WaitCanceler, WaitQueue, Waiter};
use crate::vfs::buffers::{
    AncillaryData, InputBuffer, MessageQueue, MessageReadInfo, OutputBuffer, UnixControlData,
};
use crate::vfs::socket::{
    AcceptQueue, Socket, SocketAddress, SocketDomain, SocketFile, SocketHandle, SocketMessageFlags,
    SocketOps, SocketPeer, SocketProtocol, SocketShutdownFlags, SocketType, DEFAULT_LISTEN_BACKLOG,
};
use crate::vfs::{
    default_ioctl, CheckAccessReason, FdNumber, FileHandle, FileObject, FsNodeHandle, FsStr,
    LookupContext, Message, UcredPtr,
};
use ebpf::{
    BpfProgramContext, BpfValue, CbpfConfig, DataWidth, EbpfProgram, Packet, ProgramArgument, Type,
};
use ebpf_api::{
    get_socket_filter_helpers, LoadBytesBase, MapValueRef, MapsContext, PinnedMap, ProgramType,
    SocketFilterContext, SOCKET_FILTER_CBPF_CONFIG, SOCKET_FILTER_SK_BUF_TYPE,
};
use starnix_logging::track_stub;
use starnix_sync::{FileOpsCore, LockBefore, LockEqualOrBefore, Locked, Mutex, Unlocked};
use starnix_syscalls::{SyscallArg, SyscallResult, SUCCESS};
use starnix_types::user_buffer::UserBuffer;
use starnix_uapi::errors::{Errno, EACCES, EINTR, EPERM};
use starnix_uapi::file_mode::Access;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    __sk_buff, errno, error, gid_t, socklen_t, uapi, ucred, uid_t, FIONREAD, SOL_SOCKET,
    SO_ACCEPTCONN, SO_ATTACH_BPF, SO_BROADCAST, SO_ERROR, SO_KEEPALIVE, SO_LINGER, SO_NO_CHECK,
    SO_PASSCRED, SO_PEERCRED, SO_PEERSEC, SO_RCVBUF, SO_REUSEADDR, SO_REUSEPORT, SO_SNDBUF,
};
use std::sync::Arc;
use zerocopy::IntoBytes;

// From unix.go in gVisor.
const SOCKET_MIN_SIZE: usize = 4 << 10;
const SOCKET_DEFAULT_SIZE: usize = 208 << 10;
const SOCKET_MAX_SIZE: usize = 4 << 20;

/// The data of a socket is stored in the "Inner" struct. Because both ends have separate locks,
/// care must be taken to avoid taking both locks since there is no way to tell what order to
/// take them in.
///
/// When writing, data is buffered in the "other" end of the socket's Inner.MessageQueue:
///
///            UnixSocket end #1          UnixSocket end #2
///            +---------------+          +---------------+
///            |               |          |   +-------+   |
///   Writes -------------------------------->| Inner |------> Reads
///            |               |          |   +-------+   |
///            |   +-------+   |          |               |
///   Reads <------| Inner |<-------------------------------- Writes
///            |   +-------+   |          |               |
///            +---------------+          +---------------+
///
pub struct UnixSocket {
    inner: Mutex<UnixSocketInner>,
}

fn downcast_socket_to_unix(socket: &Socket) -> &UnixSocket {
    // It is a programing error if we are downcasting
    // a different type of socket as sockets from different families
    // should not communicate, so unwrapping here
    // will let us know that.
    socket.downcast_socket::<UnixSocket>().unwrap()
}

enum UnixSocketState {
    /// The socket has not been connected.
    Disconnected,

    /// The socket has had `listen` called and can accept incoming connections.
    Listening(AcceptQueue),

    /// The socket is connected to a peer.
    Connected(SocketHandle),

    /// The socket is closed.
    Closed,
}

struct UnixSocketInner {
    /// The `MessageQueue` that contains messages sent to this socket.
    messages: MessageQueue,

    /// This queue will be notified on reads, writes, disconnects etc.
    waiters: WaitQueue,

    /// The address that this socket has been bound to, if it has been bound.
    address: Option<SocketAddress>,

    /// Whether this end of the socket has been shut down and can no longer receive message. It is
    /// still possible to send messages to the peer, if it exists and hasn't also been shut down.
    is_shutdown: bool,

    /// Whether the peer had unread data when it was closed. In this case, reads should return
    /// ECONNRESET instead of 0 (eof).
    peer_closed_with_unread_data: bool,

    /// See SO_LINGER.
    pub linger: uapi::linger,

    /// See SO_PASSCRED.
    pub passcred: bool,

    /// See SO_BROADCAST.
    pub broadcast: bool,

    /// See SO_NO_CHECK.
    pub no_check: bool,

    /// See SO_REUSEPORT.
    pub reuseport: bool,

    /// See SO_REUSEADDR.
    pub reuseaddr: bool,

    /// See SO_KEEPALIVE.
    pub keepalive: bool,

    /// See SO_ATTACH_BPF.
    #[cfg(not(feature = "starnix_lite"))]
    bpf_program: Option<UnixSocketFilter>,

    /// Unix credentials of the owner of this socket, for SO_PEERCRED.
    credentials: Option<ucred>,

    /// Socket state: a queue if this is a listening socket, or a peer if this is a connected
    /// socket.
    state: UnixSocketState,
}

impl UnixSocket {
    pub fn new(_socket_type: SocketType) -> UnixSocket {
        UnixSocket {
            inner: Mutex::new(UnixSocketInner {
                messages: MessageQueue::new(SOCKET_DEFAULT_SIZE),
                waiters: WaitQueue::default(),
                address: None,
                is_shutdown: false,
                peer_closed_with_unread_data: false,
                linger: uapi::linger::default(),
                passcred: false,
                broadcast: false,
                no_check: false,
                reuseaddr: false,
                reuseport: false,
                keepalive: false,
                #[cfg(not(feature = "starnix_lite"))]
                bpf_program: None,
                credentials: None,
                state: UnixSocketState::Disconnected,
            }),
        }
    }

    /// Creates a pair of connected sockets.
    ///
    /// # Parameters
    /// - `domain`: The domain of the socket (e.g., `AF_UNIX`).
    /// - `socket_type`: The type of the socket (e.g., `SOCK_STREAM`).
    pub fn new_pair<L>(
        locked: &mut Locked<'_, L>,
        current_task: &CurrentTask,
        domain: SocketDomain,
        socket_type: SocketType,
        open_flags: OpenFlags,
    ) -> Result<(FileHandle, FileHandle), Errno>
    where
        L: LockBefore<FileOpsCore>,
    {
        let credentials = current_task.as_ucred();
        let left = Socket::new(current_task, domain, socket_type, SocketProtocol::default())?;
        let right = Socket::new(current_task, domain, socket_type, SocketProtocol::default())?;
        downcast_socket_to_unix(&left).lock().state = UnixSocketState::Connected(right.clone());
        downcast_socket_to_unix(&left).lock().credentials = Some(credentials.clone());
        downcast_socket_to_unix(&right).lock().state = UnixSocketState::Connected(left.clone());
        downcast_socket_to_unix(&right).lock().credentials = Some(credentials);
        let left = SocketFile::from_socket(
            locked,
            current_task,
            left,
            open_flags,
            /* kernel_private= */ false,
        )?;
        let right = SocketFile::from_socket(
            locked,
            current_task,
            right,
            open_flags,
            /* kernel_private= */ false,
        )?;
        Ok((left, right))
    }

    fn connect_stream(
        &self,
        socket: &SocketHandle,
        current_task: &CurrentTask,
        peer: &SocketHandle,
    ) -> Result<(), Errno> {
        // Only hold one lock at a time until we make sure the lock ordering
        // is right: client before listener
        match downcast_socket_to_unix(peer).lock().state {
            UnixSocketState::Listening(_) => {}
            _ => return error!(ECONNREFUSED),
        }

        let mut client = downcast_socket_to_unix(socket).lock();
        match client.state {
            UnixSocketState::Disconnected => {}
            UnixSocketState::Connected(_) => return error!(EISCONN),
            _ => return error!(EINVAL),
        };

        let mut listener = downcast_socket_to_unix(peer).lock();
        // Must check this again because we released the listener lock for a moment
        let queue = match &listener.state {
            UnixSocketState::Listening(queue) => queue,
            _ => return error!(ECONNREFUSED),
        };

        self.check_type_for_connect(socket, peer, &listener.address)?;

        if queue.sockets.len() > queue.backlog {
            return error!(EAGAIN);
        }

        let server =
            Socket::new(current_task, peer.domain, peer.socket_type, SocketProtocol::default())?;
        security::unix_stream_connect(current_task, socket, peer, &server)?;
        client.state = UnixSocketState::Connected(server.clone());
        client.credentials = Some(current_task.as_ucred());
        {
            let mut server = downcast_socket_to_unix(&server).lock();
            server.state = UnixSocketState::Connected(socket.clone());
            server.address = listener.address.clone();
            server.messages.set_capacity(listener.messages.capacity())?;
            server.credentials = listener.credentials.clone();
            server.passcred = listener.passcred;
        }

        // We already checked that the socket is in Listening state...but the borrow checker cannot
        // be convinced that it's ok to combine these checks
        let queue = match listener.state {
            UnixSocketState::Listening(ref mut queue) => queue,
            _ => panic!("something changed the server socket state while I held a lock on it"),
        };
        queue.sockets.push_back(server);
        listener.waiters.notify_fd_events(FdEvents::POLLIN);
        Ok(())
    }

    fn connect_datagram(&self, socket: &SocketHandle, peer: &SocketHandle) -> Result<(), Errno> {
        {
            let unix_socket = socket.downcast_socket::<UnixSocket>().unwrap();
            let peer_inner = unix_socket.lock();
            self.check_type_for_connect(socket, peer, &peer_inner.address)?;
        }
        let unix_socket = socket.downcast_socket::<UnixSocket>().unwrap();
        unix_socket.lock().state = UnixSocketState::Connected(peer.clone());
        Ok(())
    }

    pub fn check_type_for_connect(
        &self,
        socket: &Socket,
        peer: &Socket,
        peer_address: &Option<SocketAddress>,
    ) -> Result<(), Errno> {
        if socket.domain != peer.domain || socket.socket_type != peer.socket_type {
            // According to ConnectWithWrongType in accept_bind_test, abstract
            // UNIX domain sockets return ECONNREFUSED rather than EPROTOTYPE.
            if let Some(address) = peer_address {
                if address.is_abstract_unix() {
                    return error!(ECONNREFUSED);
                }
            }
            return error!(EPROTOTYPE);
        }
        Ok(())
    }

    /// Locks and returns the inner state of the Socket.
    fn lock(&self) -> starnix_sync::MutexGuard<'_, UnixSocketInner> {
        self.inner.lock()
    }

    fn is_listening(&self, _socket: &Socket) -> bool {
        matches!(self.lock().state, UnixSocketState::Listening(_))
    }

    fn get_receive_capacity(&self) -> usize {
        self.lock().messages.capacity()
    }

    fn set_receive_capacity(&self, requested_capacity: usize) {
        self.lock().set_capacity(requested_capacity);
    }

    fn get_send_capacity(&self) -> usize {
        let peer = {
            if let Some(peer) = self.lock().peer() {
                peer.clone()
            } else {
                return 0;
            }
        };
        let unix_socket = downcast_socket_to_unix(&peer);
        let capacity = unix_socket.lock().messages.capacity();
        capacity
    }

    fn set_send_capacity(&self, requested_capacity: usize) {
        let peer = {
            if let Some(peer) = self.lock().peer() {
                peer.clone()
            } else {
                return;
            }
        };
        let unix_socket = downcast_socket_to_unix(&peer);
        unix_socket.lock().set_capacity(requested_capacity);
    }

    fn get_linger(&self) -> uapi::linger {
        let inner = self.lock();
        inner.linger
    }

    fn set_linger(&self, linger: uapi::linger) {
        let mut inner = self.lock();
        inner.linger = linger;
    }

    fn get_passcred(&self) -> bool {
        let inner = self.lock();
        inner.passcred
    }

    fn set_passcred(&self, passcred: bool) {
        let mut inner = self.lock();
        inner.passcred = passcred;
    }

    fn get_broadcast(&self) -> bool {
        let inner = self.lock();
        inner.broadcast
    }

    fn set_broadcast(&self, broadcast: bool) {
        let mut inner = self.lock();
        inner.broadcast = broadcast;
    }

    fn get_no_check(&self) -> bool {
        let inner = self.lock();
        inner.no_check
    }

    fn set_no_check(&self, no_check: bool) {
        let mut inner = self.lock();
        inner.no_check = no_check;
    }

    fn get_reuseaddr(&self) -> bool {
        let inner = self.lock();
        inner.reuseaddr
    }

    fn set_reuseaddr(&self, reuseaddr: bool) {
        let mut inner = self.lock();
        inner.reuseaddr = reuseaddr;
    }

    fn get_reuseport(&self) -> bool {
        let inner = self.lock();
        inner.reuseport
    }

    fn set_reuseport(&self, reuseport: bool) {
        let mut inner = self.lock();
        inner.reuseport = reuseport;
    }

    fn get_keepalive(&self) -> bool {
        let inner = self.lock();
        inner.keepalive
    }

    fn set_keepalive(&self, keepalive: bool) {
        let mut inner = self.lock();
        inner.keepalive = keepalive;
    }

    #[cfg(not(feature = "starnix_lite"))]
    fn set_bpf_program(&self, program: Option<UnixSocketFilter>) {
        let mut inner = self.lock();
        inner.bpf_program = program;
    }

    fn peer_cred(&self) -> Option<ucred> {
        let peer = {
            let inner = self.lock();
            inner.peer().cloned()
        };
        if let Some(peer) = peer {
            let unix_socket = downcast_socket_to_unix(&peer);
            let unix_socket = unix_socket.lock();
            unix_socket.credentials.clone()
        } else {
            None
        }
    }

    pub fn bind_socket_to_node(
        &self,
        socket: &SocketHandle,
        address: SocketAddress,
        node: &FsNodeHandle,
    ) -> Result<(), Errno> {
        let unix_socket = downcast_socket_to_unix(socket);
        let mut inner = unix_socket.lock();
        inner.bind(address)?;
        node.set_bound_socket(socket.clone());
        Ok(())
    }
}

impl SocketOps for UnixSocket {
    fn connect(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        socket: &SocketHandle,
        current_task: &CurrentTask,
        peer: SocketPeer,
    ) -> Result<(), Errno> {
        let peer = match peer {
            SocketPeer::Handle(handle) => handle,
            SocketPeer::Address(_) => return error!(EINVAL),
        };
        match socket.socket_type {
            SocketType::Stream | SocketType::SeqPacket => {
                self.connect_stream(socket, current_task, &peer)
            }
            SocketType::Datagram | SocketType::Raw => self.connect_datagram(socket, &peer),
            _ => error!(EINVAL),
        }
    }

    fn listen(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        socket: &Socket,
        backlog: i32,
        credentials: ucred,
    ) -> Result<(), Errno> {
        match socket.socket_type {
            SocketType::Stream | SocketType::SeqPacket => {}
            _ => return error!(EOPNOTSUPP),
        }
        let mut inner = self.lock();
        inner.credentials = Some(credentials);
        let is_bound = inner.address.is_some();
        let backlog = if backlog < 0 { DEFAULT_LISTEN_BACKLOG } else { backlog as usize };
        match &mut inner.state {
            UnixSocketState::Disconnected if is_bound => {
                inner.state = UnixSocketState::Listening(AcceptQueue::new(backlog));
                Ok(())
            }
            UnixSocketState::Listening(queue) => {
                queue.set_backlog(backlog)?;
                Ok(())
            }
            _ => error!(EINVAL),
        }
    }

    fn accept(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        socket: &Socket,
    ) -> Result<SocketHandle, Errno> {
        match socket.socket_type {
            SocketType::Stream | SocketType::SeqPacket => {}
            _ => return error!(EOPNOTSUPP),
        }
        let mut inner = self.lock();
        let queue = match &mut inner.state {
            UnixSocketState::Listening(queue) => queue,
            _ => return error!(EINVAL),
        };
        queue.sockets.pop_front().ok_or_else(|| errno!(EAGAIN))
    }

    fn bind(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
        socket_address: SocketAddress,
    ) -> Result<(), Errno> {
        match socket_address {
            SocketAddress::Unix(_) => {}
            _ => return error!(EINVAL),
        }
        self.lock().bind(socket_address)
    }

    fn read(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        socket: &Socket,
        _current_task: &CurrentTask,
        data: &mut dyn OutputBuffer,
        flags: SocketMessageFlags,
    ) -> Result<MessageReadInfo, Errno> {
        let info = self.lock().read(data, socket.socket_type, flags)?;
        if info.bytes_read > 0 {
            let peer = {
                let inner = self.lock();
                inner.peer().cloned()
            };
            if let Some(socket) = peer {
                let unix_socket_peer = socket.downcast_socket::<UnixSocket>();
                if let Some(socket) = unix_socket_peer {
                    socket.lock().waiters.notify_fd_events(FdEvents::POLLOUT);
                }
            }
        }
        Ok(info)
    }

    fn write(
        &self,
        locked: &mut Locked<'_, FileOpsCore>,
        socket: &Socket,
        current_task: &CurrentTask,
        data: &mut dyn InputBuffer,
        dest_address: &mut Option<SocketAddress>,
        ancillary_data: &mut Vec<AncillaryData>,
    ) -> Result<usize, Errno> {
        let (connected_peer, local_address, creds) = {
            let inner = self.lock();
            (inner.peer().map(|p| p.clone()), inner.address.clone(), inner.credentials.clone())
        };

        let peer = match (connected_peer, dest_address, socket.socket_type) {
            (Some(peer), None, _) => peer,
            (None, Some(_), SocketType::Stream) => return error!(EOPNOTSUPP),
            (None, Some(_), SocketType::SeqPacket) => return error!(ENOTCONN),
            (Some(_), Some(_), _) => return error!(EISCONN),
            (_, Some(SocketAddress::Unix(ref name)), _) => {
                resolve_unix_socket_address(locked, current_task, name.as_ref())?
            }
            (_, Some(_), _) => return error!(EINVAL),
            (None, None, _) => return error!(ENOTCONN),
        };

        if socket.socket_type == SocketType::Datagram {
            security::unix_may_send(current_task, socket, &peer)?;
        }

        let unix_socket = downcast_socket_to_unix(&peer);
        let mut peer = unix_socket.lock();
        if peer.passcred {
            let creds = creds.unwrap_or_else(|| current_task.as_ucred());
            ancillary_data.push(AncillaryData::Unix(UnixControlData::Credentials(creds)));
        }
        peer.write(locked, current_task, data, local_address, ancillary_data, socket.socket_type)
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
        self.lock().waiters.wait_async_fd_events(waiter, events, handler)
    }

    fn query_events(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        // Note that self.lock() must be dropped before acquiring peer.inner.lock() to avoid
        // potential deadlocks.
        let (mut events, peer) = {
            let inner = self.lock();

            let mut events = FdEvents::empty();
            let local_events = inner.messages.query_events();
            // From our end's message queue we only care about POLLIN (whether we have data stored
            // that's readable). POLLOUT is based on whether the peer end has room in its buffer.
            if local_events.contains(FdEvents::POLLIN) {
                events = FdEvents::POLLIN;
            }

            if inner.is_shutdown {
                events |= FdEvents::POLLIN | FdEvents::POLLOUT | FdEvents::POLLHUP;
            }

            match &inner.state {
                UnixSocketState::Listening(queue) => {
                    if !queue.sockets.is_empty() {
                        events |= FdEvents::POLLIN;
                    }
                }
                UnixSocketState::Closed => {
                    events |= FdEvents::POLLHUP;
                }
                _ => {}
            }

            (events, inner.peer().cloned())
        };

        // Check the peer (outside of our lock) to see if it can accept data written from our end.
        if let Some(peer) = peer {
            let unix_socket = downcast_socket_to_unix(&peer);
            let peer_inner = unix_socket.lock();
            let peer_events = peer_inner.messages.query_events();
            if peer_events.contains(FdEvents::POLLOUT) {
                events |= FdEvents::POLLOUT;
            }
        }

        Ok(events)
    }

    /// Shuts down this socket according to how, preventing any future reads and/or writes.
    ///
    /// Used by the shutdown syscalls.
    fn shutdown(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        how: SocketShutdownFlags,
    ) -> Result<(), Errno> {
        let peer = {
            let mut inner = self.lock();
            let peer = inner.peer().ok_or_else(|| errno!(ENOTCONN))?.clone();
            if how.contains(SocketShutdownFlags::READ) {
                inner.shutdown_one_end();
            }
            peer
        };
        if how.contains(SocketShutdownFlags::WRITE) {
            let unix_socket = downcast_socket_to_unix(&peer);
            unix_socket.lock().shutdown_one_end();
        }
        Ok(())
    }

    /// Close this socket.
    ///
    /// Called by SocketFile when the file descriptor that is holding this
    /// socket is closed.
    ///
    /// Close differs from shutdown in two ways. First, close will call
    /// mark_peer_closed_with_unread_data if this socket has unread data,
    /// which changes how read() behaves on that socket. Second, close
    /// transitions the internal state of this socket to Closed, which breaks
    /// the reference cycle that exists in the connected state.
    fn close(&self, _locked: &mut Locked<'_, FileOpsCore>, socket: &Socket) {
        let (maybe_peer, has_unread) = {
            let mut inner = self.lock();
            let maybe_peer = inner.peer().map(Arc::clone);
            inner.shutdown_one_end();
            (maybe_peer, !inner.messages.is_empty())
        };
        // If this is a connected socket type, also shut down the connected peer.
        if socket.socket_type == SocketType::Stream || socket.socket_type == SocketType::SeqPacket {
            if let Some(peer) = maybe_peer {
                let unix_socket = downcast_socket_to_unix(&peer);

                let mut peer_inner = unix_socket.lock();
                if has_unread {
                    peer_inner.peer_closed_with_unread_data = true;
                }
                peer_inner.shutdown_one_end();
            }
        }
        self.lock().state = UnixSocketState::Closed;
    }

    /// Returns the name of this socket.
    ///
    /// The name is derived from the address and domain. A socket
    /// will always have a name, even if it is not bound to an address.
    fn getsockname(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        socket: &Socket,
    ) -> Result<SocketAddress, Errno> {
        let inner = self.lock();
        if let Some(address) = &inner.address {
            Ok(address.clone())
        } else {
            Ok(SocketAddress::default_for_domain(socket.domain))
        }
    }

    /// Returns the name of the peer of this socket, if such a peer exists.
    ///
    /// Returns an error if the socket is not connected.
    fn getpeername(
        &self,
        locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
    ) -> Result<SocketAddress, Errno> {
        let peer = self.lock().peer().ok_or_else(|| errno!(ENOTCONN))?.clone();
        peer.getsockname(locked)
    }

    fn setsockopt(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _socket: &Socket,
        current_task: &CurrentTask,
        level: u32,
        optname: u32,
        user_opt: UserBuffer,
    ) -> Result<(), Errno> {
        match level {
            SOL_SOCKET => match optname {
                SO_SNDBUF => {
                    let requested_capacity: socklen_t =
                        current_task.read_object(user_opt.try_into()?)?;
                    // See StreamUnixSocketPairTest.SetSocketSendBuf for why we multiply by 2 here.
                    self.set_send_capacity(requested_capacity as usize * 2);
                }
                SO_RCVBUF => {
                    let requested_capacity: socklen_t =
                        current_task.read_object(user_opt.try_into()?)?;
                    self.set_receive_capacity(requested_capacity as usize);
                }
                SO_LINGER => {
                    let mut linger: uapi::linger =
                        current_task.read_object(user_opt.try_into()?)?;
                    if linger.l_onoff != 0 {
                        linger.l_onoff = 1;
                    }
                    self.set_linger(linger);
                }
                SO_PASSCRED => {
                    let passcred: u32 = current_task.read_object(user_opt.try_into()?)?;
                    self.set_passcred(passcred != 0);
                }
                SO_BROADCAST => {
                    let broadcast: u32 = current_task.read_object(user_opt.try_into()?)?;
                    self.set_broadcast(broadcast != 0);
                }
                SO_NO_CHECK => {
                    let no_check: u32 = current_task.read_object(user_opt.try_into()?)?;
                    self.set_no_check(no_check != 0);
                }
                SO_REUSEADDR => {
                    let reuseaddr: u32 = current_task.read_object(user_opt.try_into()?)?;
                    self.set_reuseaddr(reuseaddr != 0);
                }
                SO_REUSEPORT => {
                    let reuseport: u32 = current_task.read_object(user_opt.try_into()?)?;
                    self.set_reuseport(reuseport != 0);
                }
                SO_KEEPALIVE => {
                    let keepalive: u32 = current_task.read_object(user_opt.try_into()?)?;
                    self.set_keepalive(keepalive != 0);
                }

                SO_ATTACH_BPF => {
                    #[cfg(not(feature = "starnix_lite"))]
                    {
                        let fd: FdNumber = current_task.read_object(user_opt.try_into()?)?;
                        let object = get_bpf_object(current_task, fd)?;
                        let program = object.as_program()?;

                    let linked_program = program.link(
                        ProgramType::SocketFilter,
                        &[],
                        &get_socket_filter_helpers::<UnixSocketEbpfContext>()[..],
                    )?;

                        self.set_bpf_program(Some(linked_program));
                    }

                    #[cfg(feature = "starnix_lite")]
                    return error!(ENOPROTOOPT);
                }
                _ => return error!(ENOPROTOOPT),
            },
            _ => return error!(ENOPROTOOPT),
        }
        Ok(())
    }

    fn getsockopt(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        socket: &Socket,
        current_task: &CurrentTask,
        level: u32,
        optname: u32,
        _optlen: u32,
    ) -> Result<Vec<u8>, Errno> {
        match level {
            SOL_SOCKET => match optname {
                SO_PEERCRED => Ok(UcredPtr::into_bytes(
                    current_task,
                    self.peer_cred().unwrap_or(ucred { pid: 0, uid: uid_t::MAX, gid: gid_t::MAX }),
                )
                .map_err(|_| errno!(EINVAL))?),
                SO_PEERSEC => match socket.socket_type {
                    SocketType::Stream => security::socket_getpeersec_stream(current_task, socket),
                    _ => error!(ENOPROTOOPT),
                },
                SO_ACCEPTCONN =>
                {
                    #[allow(clippy::bool_to_int_with_if)]
                    Ok(if self.is_listening(socket) { 1u32 } else { 0u32 }.to_ne_bytes().to_vec())
                }
                SO_SNDBUF => Ok((self.get_send_capacity() as socklen_t).to_ne_bytes().to_vec()),
                SO_RCVBUF => Ok((self.get_receive_capacity() as socklen_t).to_ne_bytes().to_vec()),
                SO_LINGER => Ok(self.get_linger().as_bytes().to_vec()),
                SO_PASSCRED => Ok((self.get_passcred() as u32).as_bytes().to_vec()),
                SO_BROADCAST => Ok((self.get_broadcast() as u32).as_bytes().to_vec()),
                SO_NO_CHECK => Ok((self.get_no_check() as u32).as_bytes().to_vec()),
                SO_REUSEADDR => Ok((self.get_reuseaddr() as u32).as_bytes().to_vec()),
                SO_REUSEPORT => Ok((self.get_reuseport() as u32).as_bytes().to_vec()),
                SO_KEEPALIVE => Ok((self.get_keepalive() as u32).as_bytes().to_vec()),
                SO_ERROR => Ok((0u32).as_bytes().to_vec()),
                _ => error!(ENOPROTOOPT),
            },
            _ => error!(ENOPROTOOPT),
        }
    }

    fn ioctl(
        &self,
        locked: &mut Locked<'_, Unlocked>,
        socket: &Socket,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        match request {
            FIONREAD if socket.socket_type == SocketType::Stream => {
                let length: i32 =
                    self.lock().messages.len().try_into().map_err(|_| errno!(EINVAL))?;
                current_task.write_object(UserRef::<i32>::new(user_addr), &length)?;
                Ok(SUCCESS)
            }
            _ => default_ioctl(file, locked, current_task, request, arg),
        }
    }
}

impl UnixSocketInner {
    pub fn bind(&mut self, socket_address: SocketAddress) -> Result<(), Errno> {
        if self.address.is_some() {
            return error!(EINVAL);
        }
        self.address = Some(socket_address);
        Ok(())
    }

    fn set_capacity(&mut self, requested_capacity: usize) {
        let capacity = requested_capacity.clamp(SOCKET_MIN_SIZE, SOCKET_MAX_SIZE);
        let capacity = std::cmp::max(capacity, self.messages.len());
        // We have validated capacity sufficiently that set_capacity should always succeed.
        self.messages.set_capacity(capacity).unwrap();
    }

    /// Returns the socket that is connected to this socket, if such a peer exists. Returns
    /// ENOTCONN otherwise.
    fn peer(&self) -> Option<&SocketHandle> {
        match &self.state {
            UnixSocketState::Connected(peer) => Some(peer),
            _ => None,
        }
    }

    /// Reads the the contents of this socket into `InputBuffer`.
    ///
    /// Will stop reading if a message with ancillary data is encountered (after the message with
    /// ancillary data has been read).
    ///
    /// # Parameters
    /// - `data`: The `OutputBuffer` to write the data to.
    ///
    /// Returns the number of bytes that were read into the buffer, and any ancillary data that was
    /// read from the socket.
    fn read(
        &mut self,
        data: &mut dyn OutputBuffer,
        socket_type: SocketType,
        flags: SocketMessageFlags,
    ) -> Result<MessageReadInfo, Errno> {
        if self.peer_closed_with_unread_data {
            return error!(ECONNRESET);
        }
        let mut info = if socket_type == SocketType::Stream {
            if data.available() == 0 {
                return Ok(MessageReadInfo::default());
            }

            if flags.contains(SocketMessageFlags::PEEK) {
                self.messages.peek_stream(data)?
            } else {
                self.messages.read_stream(data)?
            }
        } else if flags.contains(SocketMessageFlags::PEEK) {
            self.messages.peek_datagram(data)?
        } else {
            self.messages.read_datagram(data)?
        };
        if info.message_length == 0 && !self.is_shutdown {
            return error!(EAGAIN);
        }

        // Remove any credentials message, so that it can be moved to the front if passcred is
        // enabled, or simply be removed if passcred is not enabled.
        let creds_message;
        if let Some(index) = info
            .ancillary_data
            .iter()
            .position(|m| matches!(m, AncillaryData::Unix(UnixControlData::Credentials { .. })))
        {
            creds_message = info.ancillary_data.remove(index)
        } else {
            // If passcred is enabled credentials are returned even if they were not sent.
            creds_message = AncillaryData::Unix(UnixControlData::unknown_creds());
        }
        if self.passcred {
            // Allow credentials to take priority if they are enabled, so insert at 0.
            info.ancillary_data.insert(0, creds_message);
        }

        Ok(info)
    }

    /// Writes the the contents of `InputBuffer` into this socket.
    ///
    /// # Parameters
    /// - `data`: The `InputBuffer` to read the data from.
    /// - `ancillary_data`: Any ancillary data to write to the socket. Note that the ancillary data
    ///                     will only be written if the entirety of the requested write completes.
    ///
    /// Returns the number of bytes that were written to the socket.
    fn write(
        &mut self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _current_task: &CurrentTask,
        data: &mut dyn InputBuffer,
        address: Option<SocketAddress>,
        ancillary_data: &mut Vec<AncillaryData>,
        socket_type: SocketType,
    ) -> Result<usize, Errno> {
        if self.is_shutdown {
            return error!(EPIPE);
        }

        #[cfg(not(feature = "starnix_lite"))]
        let filter = |mut message: Message| {
            let Some(bpf_program) = self.bpf_program.as_ref() else {
                return Some(message);
            };

            // TODO(https://fxbug.dev/385015056): Fill in SkBuf.
            let mut sk_buf = SkBuf::default();

            let mut context = UnixSocketEbpfHelpersContext::<'_>::default();
            let s = bpf_program.run(&mut context, &mut sk_buf);
            if s == 0 {
                None
            } else {
                message.truncate(s as usize);
                Some(message)
            }
        };

        #[cfg(feature = "starnix_lite")]
        let filter = |_message: Message| None;

        let bytes_written = if socket_type == SocketType::Stream {
            self.messages.write_stream_with_filter(data, address, ancillary_data, filter)?
        } else {
            self.messages.write_datagram_with_filter(data, address, ancillary_data, filter)?
        };
        if bytes_written > 0 {
            self.waiters.notify_fd_events(FdEvents::POLLIN);
        }
        Ok(bytes_written)
    }

    fn shutdown_one_end(&mut self) {
        self.is_shutdown = true;
        self.waiters.notify_fd_events(FdEvents::POLLIN | FdEvents::POLLOUT | FdEvents::POLLHUP);
    }
}

pub fn resolve_unix_socket_address<L>(
    locked: &mut Locked<'_, L>,
    current_task: &CurrentTask,
    name: &FsStr,
) -> Result<SocketHandle, Errno>
where
    L: LockEqualOrBefore<FileOpsCore>,
{
    if name[0] == b'\0' {
        current_task.abstract_socket_namespace.lookup(name)
    } else {
        let mut context = LookupContext::default();
        let (parent, basename) =
            current_task.lookup_parent_at(locked, &mut context, FdNumber::AT_FDCWD, name)?;
        let name =
            parent.lookup_child(locked, current_task, &mut context, basename).map_err(|errno| {
                if matches!(errno.code, EACCES | EPERM | EINTR) {
                    errno
                } else {
                    errno!(ECONNREFUSED)
                }
            })?;
        name.check_access(
            locked,
            current_task,
            Access::WRITE,
            CheckAccessReason::InternalPermissionChecks,
        )?;
        name.entry.node.bound_socket().map(|s| s.clone()).ok_or_else(|| errno!(ECONNREFUSED))
    }
}

// Packet buffer representation used for eBPF filters.
#[repr(C)]
#[derive(Default)]
struct SkBuf {
    sk_buff: __sk_buff,
}

impl Packet for &mut SkBuf {
    fn load(&self, _offset: i32, _width: DataWidth) -> Option<BpfValue> {
        // TODO(https://fxbug.dev/385015056): Implement packet access.
        None
    }
}

#[derive(Default)]
struct UnixSocketEbpfHelpersContext<'a> {
    map_refs: Vec<MapValueRef<'a>>,
}

impl<'a> MapsContext<'a> for UnixSocketEbpfHelpersContext<'a> {
    fn add_value_ref(&mut self, map_ref: MapValueRef<'a>) {
        self.map_refs.push(map_ref)
    }
}

impl<'a> SocketFilterContext for UnixSocketEbpfHelpersContext<'a> {
    type SkBuf<'b> = SkBuf;
    fn get_socket_uid(&self, _sk_buf: &Self::SkBuf<'_>) -> Option<uid_t> {
        track_stub!(TODO("https://fxbug.dev/385015056"), "bpf_get_socket_uid");
        None
    }

    fn get_socket_cookie(&self, _sk_buf: &Self::SkBuf<'_>) -> u64 {
        track_stub!(TODO("https://fxbug.dev/385015056"), "bpf_get_socket_cookie");
        0
    }

    fn load_bytes_relative(
        &self,
        _sk_buf: &Self::SkBuf<'_>,
        _base: LoadBytesBase,
        _offset: usize,
        _buf: &mut [u8],
    ) -> i64 {
        track_stub!(TODO("https://fxbug.dev/385015056"), "bpf_load_bytes_relative");
        -1
    }
}

impl ProgramArgument for &'_ mut SkBuf {
    fn get_type() -> &'static Type {
        &*SOCKET_FILTER_SK_BUF_TYPE
    }
}

struct UnixSocketEbpfContext {}
impl BpfProgramContext for UnixSocketEbpfContext {
    type RunContext<'a> = UnixSocketEbpfHelpersContext<'a>;
    type Packet<'a> = &'a mut SkBuf;
    type Map = PinnedMap;
    const CBPF_CONFIG: &'static CbpfConfig = &SOCKET_FILTER_CBPF_CONFIG;
}

type UnixSocketFilter = EbpfProgram<UnixSocketEbpfContext>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::MemoryAccessor;
    use crate::testing::*;

    #[::fuchsia::test]
    async fn test_socket_send_capacity() {
        let (_kernel, current_task, mut locked) = create_kernel_task_and_unlocked();
        let socket = Socket::new(
            &current_task,
            SocketDomain::Unix,
            SocketType::Stream,
            SocketProtocol::default(),
        )
        .expect("Failed to create socket.");
        socket
            .bind(&mut locked, &current_task, SocketAddress::Unix(b"\0".into()))
            .expect("Failed to bind socket.");
        socket.listen(&mut locked, &current_task, 10).expect("Failed to listen.");
        let connecting_socket = Socket::new(
            &current_task,
            SocketDomain::Unix,
            SocketType::Stream,
            SocketProtocol::default(),
        )
        .expect("Failed to connect socket.");
        connecting_socket
            .connect(&mut locked, &current_task, SocketPeer::Handle(socket.clone()))
            .expect("Failed to connect socket.");
        assert_eq!(Ok(FdEvents::POLLIN), socket.query_events(&mut locked, &current_task));
        let server_socket = socket.accept(&mut locked).unwrap();

        let opt_size = std::mem::size_of::<socklen_t>();
        let user_address =
            map_memory(&mut locked, &current_task, UserAddress::default(), opt_size as u64);
        let send_capacity: socklen_t = 4 * 4096;
        current_task.write_memory(user_address, &send_capacity.to_ne_bytes()).unwrap();
        let user_buffer = UserBuffer { address: user_address, length: opt_size };
        server_socket
            .setsockopt(&mut locked, &current_task, SOL_SOCKET, SO_SNDBUF, user_buffer)
            .unwrap();

        let opt_bytes =
            server_socket.getsockopt(&mut locked, &current_task, SOL_SOCKET, SO_SNDBUF, 0).unwrap();
        let retrieved_capacity = socklen_t::from_ne_bytes(opt_bytes.try_into().unwrap());
        // Setting SO_SNDBUF actually sets it to double the size
        assert_eq!(2 * send_capacity, retrieved_capacity);
    }
}
