// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::{IOVecPtr, MemoryAccessor, MemoryAccessorExt};
use crate::security;
use crate::syscalls::time::TimeSpecPtr;
use crate::task::{CurrentTask, IpTables, Task, WaitCallback, Waiter};
use crate::vfs::buffers::{
    AncillaryData, ControlMsg, UserBuffersInputBuffer, UserBuffersOutputBuffer,
};
use crate::vfs::socket::{
    new_socket_file, resolve_unix_socket_address, Socket, SocketAddress, SocketDomain, SocketFile,
    SocketMessageFlags, SocketPeer, SocketProtocol, SocketShutdownFlags, SocketType, UnixSocket,
    SA_FAMILY_SIZE, SA_STORAGE_SIZE,
};
use crate::vfs::{FdFlags, FdNumber, FileHandle, FsString, LookupContext};
use starnix_uapi::user_address::{ArchSpecific, MappingMultiArchUserRef, MultiArchUserRef};
use starnix_uapi::user_value::UserValue;

use starnix_logging::{log_trace, track_stub};
use starnix_sync::{FileOpsCore, LockBefore, Locked, Unlocked};
use starnix_types::time::duration_from_timespec;
use starnix_types::user_buffer::{UserBuffer, UserBuffers};
use starnix_uapi::auth::CAP_NET_BIND_SERVICE;
use starnix_uapi::errors::{Errno, EEXIST, EINPROGRESS};
use starnix_uapi::file_mode::FileMode;
use starnix_uapi::math::round_up_to_increment;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    errno, error, socklen_t, uapi, MSG_CTRUNC, MSG_DONTWAIT, MSG_TRUNC, MSG_WAITFORONE, SHUT_RD,
    SHUT_RDWR, SHUT_WR, SOCK_CLOEXEC, SOCK_NONBLOCK, UIO_MAXIOV,
};

uapi::check_arch_independent_layout! {
    socklen_t {}
}

pub type MsgHdrPtr = MappingMultiArchUserRef<MsgHdr, uapi::msghdr, uapi::arch32::msghdr>;

pub struct MsgHdr {
    name: UserAddress,
    name_len: socklen_t,
    iov: IOVecPtr,
    iovlen: UserValue<usize>,
    control: UserAddress,
    control_len: usize,
    flags: u32,
}

pub type MMsgHdrPtr = MappingMultiArchUserRef<MMsgHdr, uapi::mmsghdr, uapi::arch32::mmsghdr>;

pub struct MMsgHdr {
    hdr: MsgHdr,
    len: usize,
}

uapi::arch_map_data! {
    BidiTryFrom<MsgHdr, msghdr> {
        name = msg_name;
        name_len = msg_namelen;
        iov = msg_iov;
        iovlen = msg_iovlen;
        control = msg_control;
        control_len = msg_controllen;
        flags = msg_flags;
    }

    BidiTryFrom<MMsgHdr, mmsghdr> {
        hdr = msg_hdr;
        len = msg_len;
    }
}

pub type CMsgHdrPtr = MultiArchUserRef<uapi::cmsghdr, uapi::arch32::cmsghdr>;

pub fn sys_socket(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    domain: u32,
    socket_type: u32,
    protocol: u32,
) -> Result<FdNumber, Errno> {
    let flags = socket_type & (SOCK_NONBLOCK | SOCK_CLOEXEC);
    let domain = parse_socket_domain(domain)?;
    let socket_type = parse_socket_type(domain, socket_type)?;
    // Should we use parse_socket_protocol here?
    let protocol = SocketProtocol::from_raw(protocol);
    let open_flags = socket_flags_to_open_flags(flags);
    let socket_file =
        new_socket_file(locked, current_task, domain, socket_type, open_flags, protocol)?;

    let fd_flags = socket_flags_to_fd_flags(flags);
    let fd = current_task.add_file(socket_file, fd_flags)?;
    Ok(fd)
}

fn socket_flags_to_open_flags(flags: u32) -> OpenFlags {
    OpenFlags::RDWR
        | if flags & SOCK_NONBLOCK != 0 { OpenFlags::NONBLOCK } else { OpenFlags::empty() }
}

fn socket_flags_to_fd_flags(flags: u32) -> FdFlags {
    if flags & SOCK_CLOEXEC != 0 {
        FdFlags::CLOEXEC
    } else {
        FdFlags::empty()
    }
}

fn parse_socket_domain(domain: u32) -> Result<SocketDomain, Errno> {
    SocketDomain::from_raw(domain.try_into().map_err(|_| errno!(EAFNOSUPPORT))?).ok_or_else(|| {
        track_stub!(TODO("https://fxbug.dev/322875074"), "parse socket domain", domain);
        errno!(EAFNOSUPPORT)
    })
}

fn parse_socket_type(domain: SocketDomain, socket_type: u32) -> Result<SocketType, Errno> {
    let socket_type = SocketType::from_raw(socket_type & 0xf).ok_or_else(|| {
        track_stub!(TODO("https://fxbug.dev/322875418"), "parse socket type", socket_type);
        errno!(EINVAL)
    })?;
    // For AF_UNIX, SOCK_RAW sockets are treated as if they were SOCK_DGRAM.
    Ok(if domain == SocketDomain::Unix && socket_type == SocketType::Raw {
        SocketType::Datagram
    } else {
        socket_type
    })
}

fn parse_socket_protocol(
    domain: SocketDomain,
    socket_type: SocketType,
    protocol: u32,
) -> Result<SocketProtocol, Errno> {
    let protocol = SocketProtocol::from_raw(protocol);
    if domain == SocketDomain::Inet {
        match (socket_type, protocol) {
            (SocketType::Raw, _) => {
                // Should we have different behavior error when called by root?
                return error!(EPROTONOSUPPORT);
            }
            (SocketType::Datagram, SocketProtocol::UDP) => (),
            (SocketType::Datagram, _) => return error!(EPROTONOSUPPORT),
            (SocketType::Stream, SocketProtocol::TCP) => (),
            (SocketType::Stream, _) => return error!(EPROTONOSUPPORT),
            _ => (),
        }
    }
    Ok(protocol)
}

fn parse_socket_address(
    task: &Task,
    user_socket_address: UserAddress,
    user_address_length: usize,
) -> Result<SocketAddress, Errno> {
    if user_address_length < SA_FAMILY_SIZE || user_address_length > SA_STORAGE_SIZE {
        return error!(EINVAL);
    }

    let address = task.read_memory_to_vec(user_socket_address, user_address_length)?;

    SocketAddress::from_bytes(address)
}

fn maybe_parse_socket_address(
    task: &Task,
    user_socket_address: UserAddress,
    user_address_length: usize,
) -> Result<Option<SocketAddress>, Errno> {
    if user_address_length > i32::MAX as usize {
        return error!(EINVAL);
    }
    Ok(if user_socket_address.is_null() {
        None
    } else {
        Some(parse_socket_address(task, user_socket_address, user_address_length)?)
    })
}

// See "Autobind feature" section of https://man7.org/linux/man-pages/man7/unix.7.html
fn generate_autobind_address() -> FsString {
    let mut bytes = [0u8; 4];
    zx::cprng_draw(&mut bytes);
    let value = u32::from_ne_bytes(bytes) & 0xFFFFF;
    format!("\0{value:05x}").into()
}

pub fn sys_bind(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_socket_address: UserAddress,
    user_address_length: usize,
) -> Result<(), Errno> {
    let file = current_task.files.get(fd)?;
    let socket = Socket::get_from_file(&file)?;
    let address = parse_socket_address(current_task, user_socket_address, user_address_length)?;
    if !address.valid_for_domain(socket.domain) {
        return match socket.domain {
            SocketDomain::Unix
            | SocketDomain::Vsock
            | SocketDomain::Inet6
            | SocketDomain::Netlink
            | SocketDomain::Key
            | SocketDomain::Packet => error!(EINVAL),
            SocketDomain::Inet => error!(EAFNOSUPPORT),
        };
    }
    if let Some(port) = address.maybe_inet_port() {
        // See <https://man7.org/linux/man-pages/man7/ip.7.html>:
        //
        //   The port numbers below 1024 are called privileged ports (or
        //   sometimes: reserved ports).  Only a privileged process (on Linux:
        //   a process that has the CAP_NET_BIND_SERVICE capability in the
        //   user namespace governing its network namespace) may bind(2) to
        //   these sockets.
        if port != 0 && port < 1024 {
            security::check_task_capable(current_task, CAP_NET_BIND_SERVICE)
                .map_err(|_| errno!(EACCES))?;
        }
    }
    security::check_socket_bind_access(current_task, &file.node(), &address)?;
    match address {
        SocketAddress::Unspecified => return error!(EINVAL),
        SocketAddress::Unix(mut name) => {
            if name.is_empty() {
                // If the name is empty, then we're supposed to generate an
                // autobind address, which is always abstract.
                name = generate_autobind_address();
            }
            // If there is a null byte at the start of the sun_path, then the
            // address is abstract.
            if name[0] == b'\0' {
                current_task.abstract_socket_namespace.bind(locked, current_task, name, socket)?;
            } else {
                let mode = file.node().info().mode;
                let mode = current_task.fs().apply_umask(mode).with_type(FileMode::IFSOCK);
                let (parent, basename) = current_task.lookup_parent_at(
                    locked,
                    &mut LookupContext::default(),
                    FdNumber::AT_FDCWD,
                    name.as_ref(),
                )?;

                parent
                    .bind_socket(
                        locked,
                        current_task,
                        basename,
                        socket.clone(),
                        SocketAddress::Unix(name.clone()),
                        mode,
                    )
                    .map_err(|errno| if errno == EEXIST { errno!(EADDRINUSE) } else { errno })?;
            }
        }
        SocketAddress::Vsock(port) => {
            current_task.abstract_vsock_namespace.bind(locked, current_task, port, socket)?;
        }
        SocketAddress::Inet(_)
        | SocketAddress::Inet6(_)
        | SocketAddress::Netlink(_)
        | SocketAddress::Packet(_) => socket.bind(locked, current_task, address)?,
    }

    Ok(())
}

pub fn sys_listen(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    backlog: i32,
) -> Result<(), Errno> {
    let file = current_task.files.get(fd)?;
    let socket = Socket::get_from_file(&file)?;
    socket.listen(locked, current_task, backlog)?;
    Ok(())
}

pub fn sys_accept(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_socket_address: UserAddress,
    user_address_length: UserRef<socklen_t>,
) -> Result<FdNumber, Errno> {
    sys_accept4(locked, current_task, fd, user_socket_address, user_address_length, 0)
}

pub fn sys_accept4(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_socket_address: UserAddress,
    user_address_length: UserRef<socklen_t>,
    flags: u32,
) -> Result<FdNumber, Errno> {
    let file = current_task.files.get(fd)?;
    let socket = Socket::get_from_file(&file)?;
    let accepted_socket = file.blocking_op(
        locked,
        current_task,
        FdEvents::POLLIN | FdEvents::POLLHUP,
        None,
        |locked| socket.accept(locked),
    )?;

    if !user_socket_address.is_null() {
        let address_bytes = accepted_socket.getpeername(locked)?.to_bytes();
        write_socket_address(
            current_task,
            user_socket_address,
            user_address_length,
            &address_bytes,
        )?;
    }

    let open_flags = socket_flags_to_open_flags(flags);
    let accepted_socket_file = Socket::new_file(locked, current_task, accepted_socket, open_flags);
    let fd_flags = if flags & SOCK_CLOEXEC != 0 { FdFlags::CLOEXEC } else { FdFlags::empty() };
    let accepted_fd = current_task.add_file(accepted_socket_file, fd_flags)?;
    Ok(accepted_fd)
}

pub fn sys_connect(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_socket_address: UserAddress,
    user_address_length: usize,
) -> Result<(), Errno> {
    let client_file = current_task.files.get(fd)?;
    let client_node = client_file.node();
    let client_socket = Socket::get_from_file(&client_file)?;
    let address = parse_socket_address(current_task, user_socket_address, user_address_length)?;
    let peer = match address {
        SocketAddress::Unspecified => return error!(EAFNOSUPPORT),
        SocketAddress::Unix(ref name) => {
            log_trace!("connect to unix socket named \"{name}\"");
            if name.is_empty() {
                return error!(ECONNREFUSED);
            }
            security::check_socket_connect_access(current_task, client_node, &address)?;
            SocketPeer::Handle(resolve_unix_socket_address(locked, current_task, name.as_ref())?)
        }
        // Connect not available for AF_VSOCK
        SocketAddress::Vsock(_) => return error!(ENOSYS),
        SocketAddress::Inet(ref addr) | SocketAddress::Inet6(ref addr) => {
            log_trace!("connect to inet socket named {:?}", addr);
            security::check_socket_connect_access(current_task, client_node, &address)?;
            SocketPeer::Address(address)
        }
        SocketAddress::Netlink(_) => {
            security::check_socket_connect_access(current_task, client_node, &address)?;
            SocketPeer::Address(address)
        }
        SocketAddress::Packet(ref addr) => {
            log_trace!("connect to packet socket named {:?}", addr);
            security::check_socket_connect_access(current_task, client_node, &address)?;
            SocketPeer::Address(address)
        }
    };
    let result = client_socket.connect(locked, current_task, peer.clone());

    if client_file.is_non_blocking() {
        return result;
    }

    match result {
        // EINPROGRESS may be returned for inet sockets when `connect()` is completed
        // asynchronously.
        Err(errno) if errno.code == EINPROGRESS => {
            let waiter = Waiter::new();
            client_socket.wait_async(
                locked,
                current_task,
                &waiter,
                FdEvents::POLLOUT,
                WaitCallback::none(),
            );
            if !client_socket.query_events(locked, current_task)?.contains(FdEvents::POLLOUT) {
                waiter.wait(locked, current_task)?;
            }
            client_socket.connect(locked, current_task, peer)
        }
        // TODO(tbodt): Support blocking when the UNIX domain socket queue fills up. This one's
        // weird because as far as I can tell, removing a socket from the queue does not actually
        // trigger FdEvents on anything.
        result => result,
    }
}

fn write_socket_address(
    current_task: &CurrentTask,
    user_socket_address: UserAddress,
    user_address_length: UserRef<socklen_t>,
    address_bytes: &[u8],
) -> Result<(), Errno> {
    let capacity = current_task.read_object(user_address_length)?;
    if capacity > i32::MAX as socklen_t {
        return error!(EINVAL);
    }
    let length = address_bytes.len() as socklen_t;
    if length > 0 {
        let actual = std::cmp::min(length, capacity) as usize;
        current_task.write_memory(user_socket_address, &address_bytes[..actual])?;
    }
    current_task.write_object(user_address_length, &length)?;
    Ok(())
}

pub fn sys_getsockname(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_socket_address: UserAddress,
    user_address_length: UserRef<socklen_t>,
) -> Result<(), Errno> {
    let file = current_task.files.get(fd)?;
    let socket = Socket::get_from_file(&file)?;
    let address_bytes = socket.getsockname(locked)?.to_bytes();

    write_socket_address(current_task, user_socket_address, user_address_length, &address_bytes)?;

    Ok(())
}

pub fn sys_getpeername(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_socket_address: UserAddress,
    user_address_length: UserRef<socklen_t>,
) -> Result<(), Errno> {
    let file = current_task.files.get(fd)?;
    let socket = Socket::get_from_file(&file)?;
    let address_bytes = socket.getpeername(locked)?.to_bytes();

    write_socket_address(current_task, user_socket_address, user_address_length, &address_bytes)?;

    Ok(())
}

pub fn sys_socketpair(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    domain: u32,
    socket_type: u32,
    protocol: u32,
    user_sockets: UserRef<[FdNumber; 2]>,
) -> Result<(), Errno> {
    let flags = socket_type & (SOCK_NONBLOCK | SOCK_CLOEXEC);
    let domain = parse_socket_domain(domain)?;
    if !matches!(domain, SocketDomain::Unix | SocketDomain::Inet) {
        return error!(EAFNOSUPPORT);
    }
    let socket_type = parse_socket_type(domain, socket_type)?;
    let _protocol = parse_socket_protocol(domain, socket_type, protocol)?;
    if domain != SocketDomain::Unix {
        return error!(EOPNOTSUPP);
    }
    let open_flags = socket_flags_to_open_flags(flags);

    let (left, right) =
        UnixSocket::new_pair(locked, current_task, domain, socket_type, open_flags)?;

    let fd_flags = socket_flags_to_fd_flags(flags);
    // TODO: Eventually this will need to allocate two fd numbers (each of which could
    // potentially fail), and only populate the fd numbers (which can't fail) if both allocations
    // succeed.
    let left_fd = current_task.add_file(left, fd_flags)?;
    let right_fd = current_task.add_file(right, fd_flags)?;

    let fds = [left_fd, right_fd];
    log_trace!("socketpair -> [{:#x}, {:#x}]", fds[0].raw(), fds[1].raw());
    current_task.write_object(user_sockets, &fds)?;

    Ok(())
}

fn read_iovec_from_msghdr(
    current_task: &CurrentTask,
    message_header: &MsgHdr,
) -> Result<UserBuffers, Errno> {
    let iovec_count = message_header.iovlen;

    // In `CurrentTask::read_iovec()` the same check fails with `EINVAL`. This works for all
    // syscalls that use `iovec`, except `sendmsg()` and `recvmsg()`, which need to fail with
    // EMSGSIZE.
    if iovec_count.raw() > UIO_MAXIOV as usize {
        return error!(EMSGSIZE);
    }

    current_task.read_iovec(message_header.iov, iovec_count)
}

fn recvmsg_internal<L>(
    locked: &mut Locked<'_, L>,
    current_task: &CurrentTask,
    file: &FileHandle,
    user_message_header: MsgHdrPtr,
    flags: u32,
    deadline: Option<zx::MonotonicInstant>,
) -> Result<usize, Errno>
where
    L: LockBefore<FileOpsCore>,
{
    let mut message_header = current_task.read_multi_arch_object(user_message_header)?;
    let result = recvmsg_internal_with_header(
        locked,
        current_task,
        file,
        &mut message_header,
        flags,
        deadline,
    )?;
    current_task.write_multi_arch_object(user_message_header, message_header)?;
    Ok(result)
}

fn recvmsg_internal_with_header<L>(
    locked: &mut Locked<'_, L>,
    current_task: &CurrentTask,
    file: &FileHandle,
    message_header: &mut MsgHdr,
    flags: u32,
    deadline: Option<zx::MonotonicInstant>,
) -> Result<usize, Errno>
where
    L: LockBefore<FileOpsCore>,
{
    let iovec = read_iovec_from_msghdr(current_task, &message_header)?;

    let flags = SocketMessageFlags::from_bits(flags).ok_or_else(|| errno!(EINVAL))?;
    let socket_ops = file.downcast_file::<SocketFile>().unwrap();
    let info = socket_ops.recvmsg(
        locked,
        current_task,
        file,
        &mut UserBuffersOutputBuffer::unified_new(current_task, iovec)?,
        flags,
        deadline,
    )?;

    message_header.flags = 0;

    let cmsg_buffer_size = message_header.control_len;
    let mut cmsg_bytes_written = 0;
    let header_size = CMsgHdrPtr::size_of_object_for(current_task);

    for ancillary_data in info.ancillary_data {
        if ancillary_data.total_size(current_task) == 0 {
            // Skip zero-byte ancillary data on the receiving end. Not doing this trips this
            // assert:
            // https://cs.android.com/android/platform/superproject/+/master:system/libbase/cmsg.cpp;l=144;drc=15ec2c7a23cda814351a064a345a8270ed8c83ab
            continue;
        }

        let expected_size = header_size + ancillary_data.total_size(current_task);
        let message_bytes = ancillary_data.into_bytes(
            current_task,
            flags,
            cmsg_buffer_size - cmsg_bytes_written,
        )?;

        // If the message is smaller than expected, set the MSG_CTRUNC flag, so the caller can tell
        // some of the message is missing.
        let truncated = message_bytes.len() < expected_size;
        if truncated {
            message_header.flags |= MSG_CTRUNC;
        }

        if message_bytes.len() < header_size {
            // Can't fit the header, so stop trying to write.
            break;
        }

        if !message_bytes.is_empty() {
            current_task
                .write_memory(message_header.control + cmsg_bytes_written, &message_bytes)?;
            cmsg_bytes_written += message_bytes.len();
            if !truncated {
                cmsg_bytes_written = cmsg_align(current_task, cmsg_bytes_written)?;
            }
        }
    }

    message_header.control_len = cmsg_bytes_written;

    let msg_name = message_header.name;
    if !msg_name.is_null() {
        if message_header.name_len > i32::MAX as u32 {
            return error!(EINVAL);
        }
        let bytes = info.address.map(|a| a.to_bytes()).unwrap_or_else(|| vec![]);
        let num_bytes = std::cmp::min(message_header.name_len as usize, bytes.len());
        message_header.name_len = bytes.len() as u32;
        if num_bytes > 0 {
            current_task.write_memory(msg_name, &bytes[..num_bytes])?;
        }
    }

    if info.bytes_read != info.message_length {
        message_header.flags |= MSG_TRUNC;
    }

    if flags.contains(SocketMessageFlags::TRUNC) {
        Ok(info.message_length)
    } else {
        Ok(info.bytes_read)
    }
}

pub fn sys_recvmsg(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_message_header: MsgHdrPtr,
    flags: u32,
) -> Result<usize, Errno> {
    let file = current_task.files.get(fd)?;
    if !file.node().is_sock() {
        return error!(ENOTSOCK);
    }
    recvmsg_internal(locked, current_task, &file, user_message_header, flags, None)
}

pub fn sys_recvmmsg(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_mmsgvec: MMsgHdrPtr,
    vlen: u32,
    mut flags: u32,
    user_timeout: TimeSpecPtr,
) -> Result<usize, Errno> {
    let file = current_task.files.get(fd)?;
    if !file.node().is_sock() {
        return error!(ENOTSOCK);
    }

    if vlen > UIO_MAXIOV {
        return error!(EINVAL);
    }

    let deadline = if user_timeout.is_null() {
        None
    } else {
        let ts = current_task.read_multi_arch_object(user_timeout)?;
        Some(zx::MonotonicInstant::after(duration_from_timespec(ts)?))
    };

    let mut index = 0usize;
    while index < vlen as usize {
        let current_ptr = user_mmsgvec.at(index);
        let mut current_mmsghdr = current_task.read_multi_arch_object(current_ptr)?;
        match recvmsg_internal_with_header(
            locked,
            current_task,
            &file,
            &mut current_mmsghdr.hdr,
            flags,
            deadline,
        ) {
            Err(error) => {
                if index == 0 {
                    return Err(error);
                }
                break;
            }
            Ok(bytes_read) => {
                current_mmsghdr.len = bytes_read;
                current_task.write_multi_arch_object(current_ptr, current_mmsghdr)?;
            }
        }
        index += 1;
        if flags & MSG_WAITFORONE != 0 {
            flags |= MSG_DONTWAIT;
        }
    }
    Ok(index)
}

pub fn sys_recvfrom(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_buffer: UserAddress,
    buffer_length: usize,
    flags: u32,
    user_src_address: UserAddress,
    user_src_address_length: UserRef<socklen_t>,
) -> Result<usize, Errno> {
    let file = current_task.files.get(fd)?;
    if !file.node().is_sock() {
        return error!(ENOTSOCK);
    }

    let flags = SocketMessageFlags::from_bits(flags).ok_or_else(|| errno!(EINVAL))?;
    let socket_ops = file.downcast_file::<SocketFile>().unwrap();
    let info = socket_ops.recvmsg(
        locked,
        current_task,
        &file,
        &mut UserBuffersOutputBuffer::unified_new_at(current_task, user_buffer, buffer_length)?,
        flags,
        None,
    )?;

    if !user_src_address.is_null() {
        let bytes = info.address.map(|a| a.to_bytes()).unwrap_or_else(|| vec![]);
        write_socket_address(current_task, user_src_address, user_src_address_length, &bytes)?;
    }

    if flags.contains(SocketMessageFlags::TRUNC) {
        Ok(info.message_length)
    } else {
        Ok(info.bytes_read)
    }
}

fn sendmsg_internal<L>(
    locked: &mut Locked<'_, L>,
    current_task: &CurrentTask,
    file: &FileHandle,
    user_message_header: MsgHdrPtr,
    flags: u32,
) -> Result<usize, Errno>
where
    L: LockBefore<FileOpsCore>,
{
    let message_header = current_task.read_multi_arch_object(user_message_header)?;
    sendmsg_internal_with_header(locked, current_task, file, &message_header, flags)
}

fn sendmsg_internal_with_header<L>(
    locked: &mut Locked<'_, L>,
    current_task: &CurrentTask,
    file: &FileHandle,
    message_header: &MsgHdr,
    flags: u32,
) -> Result<usize, Errno>
where
    L: LockBefore<FileOpsCore>,
{
    if message_header.name_len > i32::MAX as u32 {
        return error!(EINVAL);
    }
    let dest_address = maybe_parse_socket_address(
        current_task,
        message_header.name,
        message_header.name_len as usize,
    )?;
    let iovec = read_iovec_from_msghdr(current_task, &message_header)?;

    let mut next_message_offset: usize = 0;
    let mut ancillary_data = Vec::new();
    let header_size = CMsgHdrPtr::size_of_object_for(current_task);
    loop {
        let space = message_header.control_len.saturating_sub(next_message_offset);
        if space < header_size {
            break;
        }
        let cmsg_ref = CMsgHdrPtr::new(current_task, message_header.control + next_message_offset);
        let cmsg = current_task.read_multi_arch_object(cmsg_ref)?;
        // If the message header is not long enough to fit the required fields of the
        // control data, return EINVAL.
        if (cmsg.cmsg_len as usize) < header_size {
            return error!(EINVAL);
        }

        let data_size = std::cmp::min(cmsg.cmsg_len as usize - header_size, space);
        let data = current_task.read_memory_to_vec(
            message_header.control + next_message_offset + header_size,
            data_size,
        )?;
        next_message_offset += cmsg_align(current_task, header_size + data.len())?;
        let data = AncillaryData::from_cmsg(
            current_task,
            ControlMsg::new(cmsg.cmsg_level, cmsg.cmsg_type, data),
        )?;
        if data.total_size(current_task) == 0 {
            continue;
        }
        ancillary_data.push(data);
    }

    let flags = SocketMessageFlags::from_bits(flags).ok_or_else(|| errno!(EOPNOTSUPP))?;
    let socket_ops = file.downcast_file::<SocketFile>().unwrap();
    socket_ops.sendmsg(
        locked,
        current_task,
        file,
        &mut UserBuffersInputBuffer::unified_new(current_task, iovec)?,
        dest_address,
        ancillary_data,
        flags,
    )
}

pub fn sys_sendmsg(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_message_header: MsgHdrPtr,
    flags: u32,
) -> Result<usize, Errno> {
    let file = current_task.files.get(fd)?;
    if !file.node().is_sock() {
        return error!(ENOTSOCK);
    }
    sendmsg_internal(locked, current_task, &file, user_message_header, flags)
}

pub fn sys_sendmmsg(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_mmsgvec: MMsgHdrPtr,
    mut vlen: u32,
    flags: u32,
) -> Result<usize, Errno> {
    let file = current_task.files.get(fd)?;
    if !file.node().is_sock() {
        return error!(ENOTSOCK);
    }

    // vlen is capped at UIO_MAXIOV.
    if vlen > UIO_MAXIOV {
        vlen = UIO_MAXIOV;
    }

    let mut index = 0usize;
    while index < vlen as usize {
        let current_ptr = user_mmsgvec.at(index);
        let mut current_mmsghdr = current_task.read_multi_arch_object(current_ptr)?;
        match sendmsg_internal_with_header(locked, current_task, &file, &current_mmsghdr.hdr, flags)
        {
            Err(error) => {
                if index == 0 {
                    return Err(error);
                }
                break;
            }
            Ok(bytes_read) => {
                current_mmsghdr.len = bytes_read;
                current_task.write_multi_arch_object(current_ptr, current_mmsghdr)?;
            }
        }
        index += 1;
    }
    Ok(index)
}

pub fn sys_sendto(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    user_buffer: UserAddress,
    user_buffer_length: usize,
    flags: u32,
    user_dest_address: UserAddress,
    user_dest_address_length: socklen_t,
) -> Result<usize, Errno> {
    let file = current_task.files.get(fd)?;
    if !file.node().is_sock() {
        return error!(ENOTSOCK);
    }

    let dest_address = maybe_parse_socket_address(
        current_task,
        user_dest_address,
        user_dest_address_length as usize,
    )?;
    let mut data =
        UserBuffersInputBuffer::unified_new_at(current_task, user_buffer, user_buffer_length)?;

    let flags = SocketMessageFlags::from_bits(flags).ok_or_else(|| errno!(EOPNOTSUPP))?;
    let socket_file = file.downcast_file::<SocketFile>().unwrap();
    socket_file.sendmsg(locked, current_task, &file, &mut data, dest_address, vec![], flags)
}

pub fn sys_getsockopt(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    level: u32,
    optname: u32,
    user_optval: UserAddress,
    user_optlen: UserRef<socklen_t>,
) -> Result<(), Errno> {
    let file = current_task.files.get(fd)?;
    let socket = Socket::get_from_file(&file)?;

    let optlen = current_task.read_object(user_optlen)?;
    let optval = current_task.read_memory_to_vec(user_optval, optlen as usize)?;

    let opt_value = if socket.domain.is_inet() && IpTables::can_handle_getsockopt(level, optname) {
        current_task.kernel().iptables.read(locked).getsockopt(socket, optname, optval)?
    } else {
        socket.getsockopt(locked, current_task, level, optname, optlen)?
    };

    let actual_optlen = opt_value.len() as socklen_t;
    if optlen < actual_optlen {
        return error!(EINVAL);
    }
    current_task.write_memory(user_optval, &opt_value)?;
    current_task.write_object(user_optlen, &actual_optlen)?;

    Ok(())
}

pub fn sys_setsockopt(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    level: u32,
    optname: u32,
    user_optval: UserAddress,
    optlen: socklen_t,
) -> Result<(), Errno> {
    let file = current_task.files.get(fd)?;
    let socket = Socket::get_from_file(&file)?;

    let user_opt = UserBuffer { address: user_optval, length: optlen as usize };
    if socket.domain.is_inet() && IpTables::can_handle_setsockopt(level, optname) {
        current_task.kernel().iptables.write(locked).setsockopt(
            current_task,
            socket,
            optname,
            user_opt,
        )
    } else {
        socket.setsockopt(locked, current_task, level, optname, user_opt)
    }
}

pub fn sys_shutdown(
    locked: &mut Locked<'_, Unlocked>,
    current_task: &CurrentTask,
    fd: FdNumber,
    how: u32,
) -> Result<(), Errno> {
    let file = current_task.files.get(fd)?;
    let socket = Socket::get_from_file(&file)?;
    let how = match how {
        SHUT_RD => SocketShutdownFlags::READ,
        SHUT_WR => SocketShutdownFlags::WRITE,
        SHUT_RDWR => SocketShutdownFlags::READ | SocketShutdownFlags::WRITE,
        _ => return error!(EINVAL),
    };
    socket.shutdown(locked, how)?;
    Ok(())
}

pub fn cmsg_align(current_task: &CurrentTask, value: usize) -> Result<usize, Errno> {
    let alignment = if current_task.is_arch32() { 4 } else { 8 };
    round_up_to_increment(value, alignment)
}

// Syscalls for arch32 usage
#[cfg(feature = "arch32")]
mod arch32 {
    use crate::vfs::{CurrentTask, FdNumber};
    use starnix_sync::{Locked, Unlocked};
    use starnix_uapi::errors::Errno;
    use starnix_uapi::user_address::UserAddress;

    pub use super::{
        sys_accept as sys_arch32_accept, sys_bind as sys_arch32_bind,
        sys_getsockname as sys_arch32_getsockname, sys_getsockopt as sys_arch32_getsockopt,
        sys_listen as sys_arch32_listen, sys_recvfrom as sys_arch32_recvfrom,
        sys_recvmmsg as sys_arch32_recvmmsg, sys_recvmsg as sys_arch32_recvmsg,
        sys_sendmsg as sys_arch32_sendmsg, sys_sendto as sys_arch32_sendto,
        sys_setsockopt as sys_arch32_setsockopt, sys_shutdown as sys_arch32_shutdown,
        sys_socketpair as sys_arch32_socketpair,
    };

    pub fn sys_arch32_send(
        locked: &mut Locked<'_, Unlocked>,
        current_task: &CurrentTask,
        fd: FdNumber,
        user_buffer: UserAddress,
        user_buffer_length: usize,
        flags: u32,
    ) -> Result<usize, Errno> {
        super::sys_sendto(
            locked,
            current_task,
            fd,
            user_buffer,
            user_buffer_length,
            flags,
            Default::default(),
            Default::default(),
        )
    }

    pub fn sys_arch32_recv(
        locked: &mut Locked<'_, Unlocked>,
        current_task: &CurrentTask,
        fd: FdNumber,
        user_buffer: UserAddress,
        buffer_length: usize,
        flags: u32,
    ) -> Result<usize, Errno> {
        super::sys_recvfrom(
            locked,
            current_task,
            fd,
            user_buffer,
            buffer_length,
            flags,
            Default::default(),
            Default::default(),
        )
    }
}

#[cfg(feature = "arch32")]
pub use arch32::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use starnix_uapi::{AF_INET, AF_UNIX, SOCK_STREAM};

    #[::fuchsia::test]
    async fn test_socketpair_invalid_arguments() {
        let (_kernel, current_task, mut locked) = create_kernel_task_and_unlocked();
        assert_eq!(
            sys_socketpair(
                &mut locked,
                &current_task,
                AF_INET as u32,
                SOCK_STREAM,
                0,
                UserRef::new(UserAddress::default())
            ),
            error!(EPROTONOSUPPORT)
        );
        assert_eq!(
            sys_socketpair(
                &mut locked,
                &current_task,
                AF_UNIX as u32,
                7,
                0,
                UserRef::new(UserAddress::default())
            ),
            error!(EINVAL)
        );
        assert_eq!(
            sys_socketpair(
                &mut locked,
                &current_task,
                AF_UNIX as u32,
                SOCK_STREAM,
                0,
                UserRef::new(UserAddress::default())
            ),
            error!(EFAULT)
        );
    }

    #[::fuchsia::test]
    fn test_generate_autobind_address() {
        let address = generate_autobind_address();
        assert_eq!(address.len(), 6);
        assert_eq!(address[0], 0);
        for byte in address[1..].iter() {
            match byte {
                b'0'..=b'9' | b'a'..=b'f' => {
                    // Ok.
                }
                bad => {
                    panic!("bad byte: {bad}");
                }
            }
        }
    }
}
