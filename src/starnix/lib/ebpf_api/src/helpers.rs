// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::maps::{Map, MapKey, MapValueRef, RingBuffer, RingBufferWakeupPolicy};
use ebpf::{BpfValue, EbpfHelperImpl, EbpfProgramContext};
use inspect_stubs::track_stub;
use linux_uapi::{
    bpf_func_id_BPF_FUNC_get_smp_processor_id, bpf_func_id_BPF_FUNC_get_socket_cookie,
    bpf_func_id_BPF_FUNC_get_socket_uid, bpf_func_id_BPF_FUNC_ktime_get_boot_ns,
    bpf_func_id_BPF_FUNC_ktime_get_coarse_ns, bpf_func_id_BPF_FUNC_ktime_get_ns,
    bpf_func_id_BPF_FUNC_map_delete_elem, bpf_func_id_BPF_FUNC_map_lookup_elem,
    bpf_func_id_BPF_FUNC_map_update_elem, bpf_func_id_BPF_FUNC_probe_read_str,
    bpf_func_id_BPF_FUNC_probe_read_user, bpf_func_id_BPF_FUNC_probe_read_user_str,
    bpf_func_id_BPF_FUNC_ringbuf_discard, bpf_func_id_BPF_FUNC_ringbuf_reserve,
    bpf_func_id_BPF_FUNC_ringbuf_submit, bpf_func_id_BPF_FUNC_sk_storage_get,
    bpf_func_id_BPF_FUNC_skb_load_bytes_relative, bpf_func_id_BPF_FUNC_trace_printk, uid_t,
};
use std::slice;

pub trait MapsContext<'a> {
    fn add_value_ref(&mut self, map_ref: MapValueRef<'a>);
}

fn bpf_map_lookup_elem<'a, C: EbpfProgramContext>(
    context: &mut C::RunContext<'a>,
    map: BpfValue,
    key: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue
where
    for<'b> C::RunContext<'b>: MapsContext<'b>,
{
    // SAFETY
    //
    // The safety of the operation is ensured by the bpf verifier. The `map` must be a reference to
    // a `Map` object kept alive by the program itself and the key must be valid for said map.
    let map: &Map = unsafe { &*map.as_ptr::<Map>() };
    let key =
        unsafe { std::slice::from_raw_parts(key.as_ptr::<u8>(), map.schema.key_size as usize) };

    let Some(value_ref) = map.lookup(key) else {
        return BpfValue::default();
    };

    let result: BpfValue = value_ref.ptr().raw_ptr().into();

    // If this is a map with ref-counted elements then save the reference for
    // the lifetime of the program.
    if value_ref.is_ref_counted() {
        context.add_value_ref(value_ref);
    }

    result
}

fn bpf_map_update_elem<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    map: BpfValue,
    key: BpfValue,
    value: BpfValue,
    flags: BpfValue,
    _: BpfValue,
) -> BpfValue {
    // SAFETY
    //
    // The safety of the operation is ensured by the bpf verifier. The `map` must be a reference to
    // a `Map` object kept alive by the program itself.
    let map: &Map = unsafe { &*map.as_ptr::<Map>() };
    let key =
        unsafe { std::slice::from_raw_parts(key.as_ptr::<u8>(), map.schema.key_size as usize) };
    let value =
        unsafe { std::slice::from_raw_parts(value.as_ptr::<u8>(), map.schema.value_size as usize) };
    let flags = flags.as_u64();

    let key = MapKey::from_slice(key);
    map.update(key, value, flags).map(|_| 0).unwrap_or(u64::MAX).into()
}

fn bpf_map_delete_elem<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _map: BpfValue,
    _key: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_map_delete_elem");
    u64::MAX.into()
}

fn bpf_trace_printk<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _fmt: BpfValue,
    _fmt_size: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_trace_printk");
    0.into()
}

fn bpf_ktime_get_ns<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_ktime_get_ns");
    42.into()
}

fn bpf_ringbuf_reserve<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    map: BpfValue,
    size: BpfValue,
    flags: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    // SAFETY
    //
    // The safety of the operation is ensured by the bpf verifier. The `map` must be a reference to
    // a `Map` object kept alive by the program itself.
    let map: &Map = unsafe { &*map.as_ptr::<Map>() };
    let size = u32::from(size);
    let flags = u64::from(flags);
    map.ringbuf_reserve(size, flags).map(BpfValue::from).unwrap_or_else(|_| BpfValue::default())
}

fn bpf_ringbuf_submit<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    data: BpfValue,
    flags: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    let flags = RingBufferWakeupPolicy::from(u32::from(flags));

    // SAFETY
    //
    // The safety of the operation is ensured by the bpf verifier. The data has to come from the
    // result of a reserve call.
    unsafe {
        RingBuffer::submit(u64::from(data), flags);
    }
    0.into()
}

fn bpf_ringbuf_discard<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    data: BpfValue,
    flags: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    let flags = RingBufferWakeupPolicy::from(u32::from(flags));

    // SAFETY
    //
    // The safety of the operation is ensured by the bpf verifier. The data has to come from the
    // result of a reserve call.
    unsafe {
        RingBuffer::discard(u64::from(data), flags);
    }
    0.into()
}

fn bpf_ktime_get_boot_ns<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_ktime_get_boot_ns");
    0.into()
}

fn bpf_probe_read_user<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_probe_read_user");
    0.into()
}

fn bpf_probe_read_user_str<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_probe_read_user_str");
    0.into()
}

fn bpf_ktime_get_coarse_ns<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_ktime_get_coarse_ns");
    0.into()
}

fn bpf_probe_read_str<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_probe_read_str");
    0.into()
}

fn bpf_get_smp_processor_id<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_get_smp_processor_id");
    0.into()
}

pub fn get_common_helpers<C: EbpfProgramContext>() -> Vec<(u32, EbpfHelperImpl<C>)>
where
    for<'a> C::RunContext<'a>: MapsContext<'a>,
{
    vec![
        (bpf_func_id_BPF_FUNC_ktime_get_boot_ns, EbpfHelperImpl(bpf_ktime_get_boot_ns)),
        (bpf_func_id_BPF_FUNC_ktime_get_coarse_ns, EbpfHelperImpl(bpf_ktime_get_coarse_ns)),
        (bpf_func_id_BPF_FUNC_ktime_get_ns, EbpfHelperImpl(bpf_ktime_get_ns)),
        (bpf_func_id_BPF_FUNC_map_delete_elem, EbpfHelperImpl(bpf_map_delete_elem)),
        (bpf_func_id_BPF_FUNC_map_lookup_elem, EbpfHelperImpl(bpf_map_lookup_elem)),
        (bpf_func_id_BPF_FUNC_map_update_elem, EbpfHelperImpl(bpf_map_update_elem)),
        (bpf_func_id_BPF_FUNC_probe_read_str, EbpfHelperImpl(bpf_probe_read_str)),
        (bpf_func_id_BPF_FUNC_probe_read_user, EbpfHelperImpl(bpf_probe_read_user)),
        (bpf_func_id_BPF_FUNC_probe_read_user_str, EbpfHelperImpl(bpf_probe_read_user_str)),
        (bpf_func_id_BPF_FUNC_ringbuf_discard, EbpfHelperImpl(bpf_ringbuf_discard)),
        (bpf_func_id_BPF_FUNC_ringbuf_reserve, EbpfHelperImpl(bpf_ringbuf_reserve)),
        (bpf_func_id_BPF_FUNC_ringbuf_submit, EbpfHelperImpl(bpf_ringbuf_submit)),
        (bpf_func_id_BPF_FUNC_trace_printk, EbpfHelperImpl(bpf_trace_printk)),
        (bpf_func_id_BPF_FUNC_get_smp_processor_id, EbpfHelperImpl(bpf_get_smp_processor_id)),
    ]
}

pub enum LoadBytesBase {
    MacHeader,
    NetworkHeader,
}

pub trait SocketFilterContext {
    type SkBuf<'a>;
    fn get_socket_uid(&self, sk_buf: &Self::SkBuf<'_>) -> Option<uid_t>;
    fn get_socket_cookie(&self, sk_buf: &Self::SkBuf<'_>) -> u64;
    fn load_bytes_relative(
        &self,
        sk_buf: &Self::SkBuf<'_>,
        base: LoadBytesBase,
        offset: usize,
        buf: &mut [u8],
    ) -> i64;
}

fn bpf_get_socket_uid<'a, C: EbpfProgramContext>(
    context: &mut C::RunContext<'a>,
    sk_buf: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue
where
    for<'b> C::RunContext<'b>: SocketFilterContext,
{
    // SAFETY: Verifier checks that the argument points at the `SkBuf`.
    let sk_buf: &<C::RunContext<'a> as SocketFilterContext>::SkBuf<'a> =
        unsafe { &*sk_buf.as_ptr() };
    const OVERFLOW_UID: uid_t = 65534;
    context.get_socket_uid(sk_buf).unwrap_or(OVERFLOW_UID).into()
}

fn bpf_get_socket_cookie<'a, C: EbpfProgramContext>(
    context: &mut C::RunContext<'a>,
    sk_buf: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue
where
    for<'b> C::RunContext<'b>: SocketFilterContext,
{
    // SAFETY: Verifier checks that the argument points at the `SkBuf`.
    let sk_buf: &<C::RunContext<'a> as SocketFilterContext>::SkBuf<'a> =
        unsafe { &*sk_buf.as_ptr() };
    context.get_socket_cookie(sk_buf).into()
}

fn bpf_skb_load_bytes_relative<'a, C: EbpfProgramContext>(
    context: &mut C::RunContext<'a>,
    sk_buf: BpfValue,
    offset: BpfValue,
    to: BpfValue,
    len: BpfValue,
    start_header: BpfValue,
) -> BpfValue
where
    for<'b> C::RunContext<'b>: SocketFilterContext,
{
    // SAFETY: Verifier checks that the argument points at the `SkBuf`.
    let sk_buf: &<C::RunContext<'a> as SocketFilterContext>::SkBuf<'a> =
        unsafe { &*sk_buf.as_ptr() };

    let base = match start_header.as_u32() {
        0 => LoadBytesBase::MacHeader,
        1 => LoadBytesBase::NetworkHeader,
        _ => return u64::MAX.into(),
    };

    let Ok(offset) = offset.as_u64().try_into() else {
        return u64::MAX.into();
    };

    // SAFETY: The verifier ensures that `to` points to a valid buffer of at
    // least `len` bytes that the eBPF program has permission to access.
    let buf = unsafe { slice::from_raw_parts_mut(to.as_ptr::<u8>(), len.as_u64() as usize) };

    context.load_bytes_relative(sk_buf, base, offset, buf).into()
}

fn bpf_sk_storage_get<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_sk_storage_get");
    0.into()
}

// Helpers that are supplied to socket filter programs in addition to the common helpers.
pub fn get_socket_filter_helpers<C: EbpfProgramContext>() -> Vec<(u32, EbpfHelperImpl<C>)>
where
    for<'a> C::RunContext<'a>: SocketFilterContext,
{
    vec![
        (bpf_func_id_BPF_FUNC_get_socket_uid, EbpfHelperImpl(bpf_get_socket_uid)),
        (bpf_func_id_BPF_FUNC_get_socket_cookie, EbpfHelperImpl(bpf_get_socket_cookie)),
        (bpf_func_id_BPF_FUNC_skb_load_bytes_relative, EbpfHelperImpl(bpf_skb_load_bytes_relative)),
        (bpf_func_id_BPF_FUNC_sk_storage_get, EbpfHelperImpl(bpf_sk_storage_get)),
    ]
}
