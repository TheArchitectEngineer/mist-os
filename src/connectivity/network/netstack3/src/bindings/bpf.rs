// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bindings::BindingsCtx;
use ebpf::{
    BpfProgramContext, BpfValue, CbpfConfig, DataWidth, EbpfInstruction, EbpfProgram,
    EbpfProgramContext, FieldMapping, HelperSet, MapReference, MapSchema, Packet, ProgramArgument,
    StructMapping, Type, VerifiedEbpfProgram,
};
use ebpf_api::{
    Map, MapError, PinnedMap, __sk_buff, uid_t, MapValueRef, CGROUP_SKB_ARGS,
    CGROUP_SKB_SK_BUF_TYPE, SKF_AD_OFF, SKF_AD_PROTOCOL, SKF_LL_OFF, SKF_NET_OFF, SK_BUF_ID,
    SOCKET_FILTER_CBPF_CONFIG, SOCKET_FILTER_SK_BUF_TYPE,
};
use fidl_table_validation::ValidFidlTable;
use log::{error, warn};
use netstack3_core::device::DeviceId;
use netstack3_core::device_socket::Frame;
use netstack3_core::filter::{
    FilterIpExt, IpPacket, SocketEgressFilterResult, SocketIngressFilterResult, SocketOpsFilter,
};
use netstack3_core::ip::{Mark, MarkDomain, Marks};
use netstack3_core::socket::SocketCookie;
use netstack3_core::sync::{Mutex, RwLock};
use netstack3_core::trace::trace_duration;
use packet::{FragmentedByteSlice, PacketConstraints, PartialSerializer};
use std::collections::{hash_map, HashMap};
use std::mem::offset_of;
use std::sync::{Arc, LazyLock, Weak};
use zerocopy::FromBytes;
use zx::AsHandleRef;
use {fidl_fuchsia_ebpf as febpf, fidl_fuchsia_posix_socket_packet as fppacket};

// Transmutes `Vec<u64>` to `Vec<EbpfInstruction>`.
fn code_from_vec(code: Vec<u64>) -> Vec<EbpfInstruction> {
    // SAFETY:  This is safe because `EbpfInstruction` is 64 bits.
    unsafe {
        let mut code = std::mem::ManuallyDrop::new(code);
        Vec::from_raw_parts(code.as_mut_ptr() as *mut EbpfInstruction, code.len(), code.capacity())
    }
}

// Packet buffer representation used for BPF filters.
#[repr(C)]
struct IpPacketForBpf<'a> {
    // This field must be first. eBPF programs will access it directly.
    sk_buff: __sk_buff,

    kind: fppacket::Kind,
    frame: Frame<&'a [u8]>,
    raw: &'a [u8],
}

impl Packet for &'_ IpPacketForBpf<'_> {
    fn load<'a>(&self, offset: i32, width: DataWidth) -> Option<BpfValue> {
        // cBPF Socket Filters use non-negative offset to access packet content.
        // Negative offsets are handler as follows as follows:
        //   SKF_AD_OFF (-0x1000) - Auxiliary info that may be outside of the packet.
        //      Currently only SKF_AD_PROTOCOL is implemented.
        //   SKF_NET_OFF (-0x100000) - Packet content relative to the IP header.
        //   SKF_LL_OFF (-0x200000) - Packet content relative to the link-level header.
        let (offset, slice) = if offset >= 0 {
            (
                offset,
                match self.kind {
                    fppacket::Kind::Network => self.frame.into_body(),
                    fppacket::Kind::Link => self.raw,
                },
            )
        } else if offset >= SKF_AD_OFF {
            if offset == SKF_AD_OFF + SKF_AD_PROTOCOL {
                return Some(self.frame.protocol().unwrap_or(0).into());
            } else {
                log::info!(
                    "cBPF program tried to access unimplemented SKF_AD_OFF offset: {}",
                    offset - SKF_AD_OFF
                );
                return None;
            }
        } else if offset >= SKF_NET_OFF {
            // Access network level packet.
            (offset - SKF_NET_OFF, self.frame.into_body())
        } else if offset >= SKF_LL_OFF {
            // Access link-level packet.
            (offset - SKF_LL_OFF, self.raw)
        } else {
            return None;
        };

        let offset = offset.try_into().unwrap();

        if offset >= slice.len() {
            return None;
        }

        // The packet is stored in network byte order, so multi-byte loads need to fix endianness.
        // Potentially this could be handled in the cBPF converter but then it would need to be
        // disabled from seccomp filter, which always runs in the host byte order.
        let slice = &slice[offset..];
        match width {
            DataWidth::U8 => u8::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
            DataWidth::U16 => zerocopy::U16::<zerocopy::NetworkEndian>::read_from_prefix(slice)
                .ok()
                .map(|(v, _)| v.get().into()),
            DataWidth::U32 => zerocopy::U32::<zerocopy::NetworkEndian>::read_from_prefix(slice)
                .ok()
                .map(|(v, _)| v.get().into()),
            DataWidth::U64 => zerocopy::U64::<zerocopy::NetworkEndian>::read_from_prefix(slice)
                .ok()
                .map(|(v, _)| v.get().into()),
        }
    }
}

impl ProgramArgument for &'_ IpPacketForBpf<'_> {
    fn get_type() -> &'static Type {
        &*SOCKET_FILTER_SK_BUF_TYPE
    }
}

struct SocketFilterContext {}

impl BpfProgramContext for SocketFilterContext {
    type RunContext<'a> = ();
    type Packet<'a> = &'a IpPacketForBpf<'a>;
    type Map = PinnedMap;
    const CBPF_CONFIG: &'static CbpfConfig = &SOCKET_FILTER_CBPF_CONFIG;
}

#[derive(Debug)]
pub(crate) struct SocketFilterProgram {
    program: EbpfProgram<SocketFilterContext>,
}

pub(crate) enum SocketFilterResult {
    // If the packet is accepted it may need to trimmed to the specified size.
    Accept(usize),
    Reject,
}

impl SocketFilterProgram {
    pub(crate) fn new(code: Vec<u64>) -> Self {
        // TODO(https://fxbug.dev/370043219): Currently we assume that the code has been verified.
        // This is safe because fuchsia.posix.socket.packet is routed only to Starnix,
        // but that may change in the future. We need a better mechanism for permissions & BPF
        // verification.
        let program = VerifiedEbpfProgram::from_verified_code(
            code_from_vec(code),
            SocketFilterContext::get_arg_types(),
            vec![],
            vec![],
        );
        let program =
            ebpf::link_program::<SocketFilterContext>(&program, &[], vec![], HelperSet::default())
                .expect("Failed to link SocketFilter program");

        Self { program }
    }

    pub(crate) fn run(
        &self,
        kind: fppacket::Kind,
        frame: Frame<&[u8]>,
        raw: &[u8],
    ) -> SocketFilterResult {
        let packet_size = match kind {
            fppacket::Kind::Network => frame.into_body().len(),
            fppacket::Kind::Link => raw.len(),
        };

        let mut packet = IpPacketForBpf {
            sk_buff: __sk_buff { len: packet_size.try_into().unwrap(), ..Default::default() },
            kind,
            frame,
            raw,
        };

        let result = self.program.run(&mut (), &mut packet);
        match result {
            0 => SocketFilterResult::Reject,
            n => SocketFilterResult::Accept(n.try_into().unwrap()),
        }
    }
}

/// `__sk_buff` representation passed to programs of type
/// `BPF_PROG_TYPE_CGROUP_SKB` (attached to `BPF_CGROUP_INET_EGRESS`
/// and `BPF_CGROUP_INET_INGRESS`).
#[repr(C)]
struct IpPacketForCgroupSkb<'a> {
    sk_buff: __sk_buff,

    data_ptr: *const u8,
    data_end_ptr: *const u8,

    marks: &'a Marks,
    socket_cookie: u64,
    data: &'a [u8],
}

static SK_BUF_MAPPING: LazyLock<StructMapping> = LazyLock::new(|| StructMapping {
    memory_id: SK_BUF_ID.clone(),
    fields: vec![
        FieldMapping {
            source_offset: offset_of!(__sk_buff, data),
            target_offset: offset_of!(IpPacketForCgroupSkb<'_>, data_ptr),
        },
        FieldMapping {
            source_offset: offset_of!(__sk_buff, data_end),
            target_offset: offset_of!(IpPacketForCgroupSkb<'_>, data_end_ptr),
        },
    ],
});

impl ProgramArgument for &'_ IpPacketForCgroupSkb<'_> {
    fn get_type() -> &'static Type {
        &*CGROUP_SKB_SK_BUF_TYPE
    }
}

/// Context for programs of type `BPF_PROG_TYPE_CGROUP_SKB` (attached to
/// `BPF_CGROUP_INET_EGRESS` and `BPF_CGROUP_INET_INGRESS`).
struct CgroupSkbContext {}

impl EbpfProgramContext for CgroupSkbContext {
    type RunContext<'a> = CgroupSkbRunContext<'a>;
    type Packet<'a> = ();
    type Map = CachedMapRef;

    type Arg1<'a> = &'a IpPacketForCgroupSkb<'a>;
    type Arg2<'a> = ();
    type Arg3<'a> = ();
    type Arg4<'a> = ();
    type Arg5<'a> = ();
}

#[derive(Default)]
struct CgroupSkbRunContext<'a> {
    map_refs: Vec<MapValueRef<'a>>,
}

impl<'a> ebpf_api::SocketFilterContext for CgroupSkbRunContext<'a> {
    type SkBuf<'b> = IpPacketForCgroupSkb<'b>;
    fn get_socket_uid(&self, sk_buf: &Self::SkBuf<'_>) -> Option<uid_t> {
        let Mark(mark_value) = sk_buf.marks.get(MarkDomain::Mark2);
        *mark_value
    }

    fn get_socket_cookie(&self, sk_buf: &Self::SkBuf<'_>) -> u64 {
        sk_buf.socket_cookie
    }

    fn load_bytes_relative(
        &self,
        sk_buf: &Self::SkBuf<'_>,
        base: ebpf_api::LoadBytesBase,
        offset: usize,
        buf: &mut [u8],
    ) -> i64 {
        if base != ebpf_api::LoadBytesBase::NetworkHeader {
            warn!("bpf_skb_load_bytes_relative(BPF_HDR_START_MAC) not supported");
            return -1;
        }

        let Some(data) = sk_buf.data.get(offset..(offset + buf.len())) else {
            return -1;
        };

        buf.copy_from_slice(data);
        0
    }
}

impl<'a> ebpf_api::MapsContext<'a> for CgroupSkbRunContext<'a> {
    fn add_value_ref(&mut self, map_ref: MapValueRef<'a>) {
        self.map_refs.push(map_ref)
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum EbpfError {
    NotSupported,
    LinkFailed,
    MapFailed,
    DuplicateAttachment,
}

#[derive(ValidFidlTable)]
#[fidl_table_src(febpf::VerifiedProgram)]
#[fidl_table_strict]
pub(crate) struct ValidVerifiedProgram {
    code: Vec<u64>,
    struct_access_instructions: Vec<febpf::StructAccess>,
    maps: Vec<febpf::Map>,
}

/// Translate FIDL representation of a verified eBPF program to
/// `VerifiedEbpfProgram`. Initializes all included eBPF maps and adds them
/// to the `map_cache`.
fn parse_verified_program_fidl(
    program: ValidVerifiedProgram,
    map_cache: &Arc<EbpfMapCache>,
    args: Vec<Type>,
) -> Result<(VerifiedEbpfProgram, Vec<CachedMapRef>), EbpfError> {
    let ValidVerifiedProgram { code, struct_access_instructions, maps } = program;

    let maps = map_cache.init_maps(maps).map_err(|e| {
        error!("Failed to initialize eBPF map: {:?}", e);
        EbpfError::MapFailed
    })?;
    let map_schemas = maps.iter().map(|m| m.schema().clone()).collect();

    let struct_access_instructions = struct_access_instructions
        .iter()
        .map(|value| ebpf::StructAccess {
            pc: value.pc.try_into().unwrap(),
            memory_id: value.struct_memory_id.into(),
            field_offset: value.field_offset.try_into().unwrap(),
            is_32_bit_ptr_load: value.is_32_bit_ptr_load,
        })
        .collect();

    let program = VerifiedEbpfProgram::from_verified_code(
        code_from_vec(code),
        args,
        struct_access_instructions,
        map_schemas,
    );

    Ok((program, maps))
}

/// An eBPF programs of type `BPF_PROG_TYPE_CGROUP_SKB`, attachment type ether
/// `CGROUP_EGRESS` or `CGROUP_INGRESS`.
#[derive(Debug)]
pub(crate) struct CgroupSkbProgram {
    program: EbpfProgram<CgroupSkbContext>,
}

impl CgroupSkbProgram {
    // Both `CGROUP_EGRESS` and `CGROUP_INGRESS` returns result where the first bit indicates if
    // the packet should be passed or dropped.
    const RESULT_PASS_BIT: u64 = 1;

    // `CGROUP_EGRESS` uses second bit of the result to signal congestion.
    const RESULT_CONGESTION_BIT: u64 = 2;

    // Max value that can be returned from a `CGROUP_EGRESS` program.
    const EGRESS_MAX_RESULT: u64 = Self::RESULT_PASS_BIT | Self::RESULT_CONGESTION_BIT;

    pub fn new(
        program: ValidVerifiedProgram,
        map_cache: &Arc<EbpfMapCache>,
    ) -> Result<Self, EbpfError> {
        // TODO(https://fxbug.dev/370043219): Currently we assume that the code has been verified.
        // This is safe because `fuchsia.posix.filter.SocketControl` is routed only to Starnix,
        // but that may change in the future. We need a better mechanism for permissions & BPF
        // verification.
        let (program, maps) =
            parse_verified_program_fidl(program, map_cache, CGROUP_SKB_ARGS.clone())?;

        let helpers = ebpf_api::get_common_helpers()
            .into_iter()
            .chain(ebpf_api::get_socket_filter_helpers())
            .collect();

        let program =
            ebpf::link_program(&program, std::slice::from_ref(&SK_BUF_MAPPING), maps, helpers)
                .map_err(|e| {
                    error!("Failed to link eBPF program: {:?}", e);
                    EbpfError::LinkFailed
                })?;

        Ok(Self { program })
    }

    fn run<I: FilterIpExt, P: IpPacket<I> + PartialSerializer>(
        &self,
        packet: &P,
        ifindex: u32,
        socket_cookie: SocketCookie,
        marks: &Marks,
    ) -> u64 {
        let mut data = [0u8; 128];
        let serialize_result = packet
            .partial_serialize(PacketConstraints::UNCONSTRAINED, &mut data)
            .expect("Packet serialization failed");

        let mut skb = IpPacketForCgroupSkb {
            sk_buff: __sk_buff {
                len: serialize_result.total_size.try_into().unwrap_or(0),
                protocol: u16::from(I::ETHER_TYPE).to_be().into(),
                ifindex,
                ..__sk_buff::default()
            },
            data_ptr: data.as_ptr(),
            // SAFETY: `data_end_ptr` points at the end of the data buffer, but it's never
            // dereferenced directly.
            data_end_ptr: unsafe { data.as_ptr().add(serialize_result.bytes_written) },
            marks,
            socket_cookie: socket_cookie.export_value(),
            data: &data,
        };

        trace_duration!(
            c"ebpf::cgroup_skb::run",
            "len" => serialize_result.total_size,
            "protocol" => skb.sk_buff.protocol
        );

        let mut run_context = CgroupSkbRunContext::default();
        self.program.run_with_1_argument(&mut run_context, &mut skb)
    }
}

type MapId = zx::Koid;

#[derive(Clone)]
pub(crate) struct CachedMapRefInner {
    map: PinnedMap,
    id: MapId,
    cache: Weak<EbpfMapCache>,
}

impl Drop for CachedMapRefInner {
    fn drop(&mut self) {
        // If this is the last reference to the map beside the reference owned
        // by the cache itself, then remove it from the cache.
        if let Some(cache) = self.cache.upgrade() {
            cache.last_reference_dropped(self.id)
        }
    }
}

/// A reference to a map stored in `EbpfMapCache`. The referenced map is
/// deleted from the cache when the last reference to that map is dropped.
struct CachedMapRef {
    inner: Arc<CachedMapRefInner>,
}

impl MapReference for CachedMapRef {
    fn schema(&self) -> &MapSchema {
        self.inner.map.schema()
    }

    fn as_bpf_value(&self) -> BpfValue {
        self.inner.map.as_bpf_value()
    }
}

/// `EbpfMapCache` maintains list of all eBPF maps programs loaded in this
/// process. This allows to initialize to memory-map the corresponding VMOs
/// only once per process.
#[derive(Default)]
pub(crate) struct EbpfMapCache {
    maps: Mutex<HashMap<MapId, Weak<CachedMapRefInner>>>,
}

impl EbpfMapCache {
    fn init_map(self: &Arc<Self>, fidl_map: febpf::Map) -> Result<CachedMapRef, MapError> {
        // Maps are identified by the KOID of the underlying VMO.
        let id = fidl_map
            .vmo
            .as_ref()
            .ok_or(MapError::InvalidParam)?
            .get_koid()
            .map_err(|_: zx::Status| MapError::InvalidVmo)?;

        let mut maps = self.maps.lock();
        let entry = maps.entry(id);
        let inner = match &entry {
            hash_map::Entry::Occupied(occupied) => {
                // The upgraded `Arc` may be `None` if the map is being
                // dropped concurrently by another thread. In that case it
                // will be initialized again below.
                occupied.get().upgrade()
            }

            hash_map::Entry::Vacant(_) => None,
        };

        let inner = match inner {
            Some(inner) => inner,
            None => {
                let map = Map::new_shared(fidl_map)?;
                let inner = Arc::new(CachedMapRefInner { map, id, cache: Arc::downgrade(self) });
                let _: hash_map::OccupiedEntry<'_, _, _> =
                    entry.insert_entry(Arc::downgrade(&inner));
                inner
            }
        };

        Ok(CachedMapRef { inner })
    }

    fn init_maps(
        self: &Arc<Self>,
        fidl_maps: Vec<febpf::Map>,
    ) -> Result<Vec<CachedMapRef>, MapError> {
        let mut result = Vec::with_capacity(fidl_maps.len());
        for map in fidl_maps {
            result.push(self.init_map(map)?);
        }
        Ok(result)
    }

    fn last_reference_dropped(&self, id: MapId) {
        match self.maps.lock().entry(id) {
            hash_map::Entry::Occupied(occupied) => {
                // Remove the entry only if it's no longer valid since the
                // entry might have been replaced in `init_map()`.
                if occupied.get().upgrade().is_none() {
                    let _: Weak<CachedMapRefInner> = occupied.remove();
                }
            }
            hash_map::Entry::Vacant(_) => {
                // Nothing to do since the entry is not in the cache. This case
                // may be reached when the map is inserted and deleted
                // concurrently by another thread.
            }
        }
    }
}

#[derive(Default)]
struct EbpfManagerState {
    root_cgroup_egress: Option<CgroupSkbProgram>,
    root_cgroup_ingress: Option<CgroupSkbProgram>,
}

/// Holds state of eBPF programs attached in this process.
#[derive(Default)]
pub(crate) struct EbpfManager {
    state: RwLock<EbpfManagerState>,
    maps_cache: Arc<EbpfMapCache>,
}

impl EbpfManager {
    pub fn maps_cache(&self) -> &Arc<EbpfMapCache> {
        &self.maps_cache
    }

    pub fn set_egress_hook(
        &self,
        program: Option<CgroupSkbProgram>,
        allow_replace: bool,
    ) -> Result<(), EbpfError> {
        let mut state = self.state.write();
        if !allow_replace && state.root_cgroup_egress.is_some() && program.is_some() {
            return Err(EbpfError::DuplicateAttachment);
        }
        state.root_cgroup_egress = program;
        Ok(())
    }

    pub fn set_ingress_hook(
        &self,
        program: Option<CgroupSkbProgram>,
        allow_replace: bool,
    ) -> Result<(), EbpfError> {
        let mut state = self.state.write();
        if !allow_replace && state.root_cgroup_ingress.is_some() && program.is_some() {
            return Err(EbpfError::DuplicateAttachment);
        }
        state.root_cgroup_ingress = program;
        Ok(())
    }
}

impl SocketOpsFilter<DeviceId<BindingsCtx>> for &EbpfManager {
    fn on_egress<I: FilterIpExt, P: IpPacket<I> + PartialSerializer>(
        &self,
        packet: &P,
        device: &DeviceId<BindingsCtx>,
        socket_cookie: SocketCookie,
        marks: &Marks,
    ) -> SocketEgressFilterResult {
        let state = self.state.read();
        let Some(prog) = state.root_cgroup_egress.as_ref() else {
            return SocketEgressFilterResult::Pass { congestion: false };
        };

        let ifindex = device.bindings_id().id.get().try_into().unwrap_or(0);

        trace_duration!(c"ebpf::egress");

        let result = prog.run(packet, ifindex, socket_cookie, marks);
        if result > CgroupSkbProgram::EGRESS_MAX_RESULT {
            // TODO(https://fxbug.dev/413490751): Change this to panic once
            // result validation is implemented in the verifier.
            error!("eBPF program returned invalid result: {}", result);
            return SocketEgressFilterResult::Pass { congestion: false };
        }

        let congestion = result & CgroupSkbProgram::RESULT_CONGESTION_BIT > 0;
        if result & CgroupSkbProgram::RESULT_PASS_BIT > 0 {
            SocketEgressFilterResult::Pass { congestion }
        } else {
            SocketEgressFilterResult::Drop { congestion }
        }
    }

    fn on_ingress(
        &self,
        _packet: &FragmentedByteSlice<'_, &[u8]>,
        _device: &DeviceId<BindingsCtx>,
        _cookie: SocketCookie,
        _marks: &Marks,
    ) -> SocketIngressFilterResult {
        // TODO(https://fxbug.dev/407809292): Execute attached program.
        SocketIngressFilterResult::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ebpf::Packet;
    use ebpf_api::{AttachType, ProgramType, SKF_AD_MAX};
    use ip_test_macro::ip_test;
    use net_types::Witness;
    use netstack3_core::device_socket::SentFrame;
    use netstack3_core::ip::Mark;
    use netstack3_core::sync::ResourceTokenValue;
    use netstack3_core::testutil::{self, TestIpExt};
    use packet::{InnerPacketBuilder, PacketBuilder, ParsablePacket};
    use packet_formats::ethernet::EthernetFrameLengthCheck;
    use packet_formats::ip::IpProto;
    use packet_formats::udp::UdpPacketBuilder;
    use std::num::NonZeroU16;
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    struct TestData;
    impl TestData {
        const PROTO: u16 = 0x08AB;
        const BUFFER: &'static [u8] = &[
            0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, // Dest MAC
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, // Source MAC
            0x08, 0xAB, // EtherType
            0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x3A, 0x4B, // Packet body
        ];
        const BODY_POSITION: i32 = 14;

        /// Creates an EthernetFrame with the values specified above.
        fn frame() -> Frame<&'static [u8]> {
            let mut buffer_view = Self::BUFFER;
            Frame::Sent(SentFrame::Ethernet(
                packet_formats::ethernet::EthernetFrame::parse(
                    &mut buffer_view,
                    EthernetFrameLengthCheck::NoCheck,
                )
                .unwrap()
                .into(),
            ))
        }
    }

    fn packet_load(packet: &IpPacketForBpf<'_>, offset: i32, width: DataWidth) -> Option<u64> {
        packet.load(offset, width).map(|v| v.as_u64())
    }

    // Test loading Ethernet header at the specified base offset.
    fn test_ll_header_load(packet: &IpPacketForBpf<'_>, base: i32) {
        assert_eq!(packet_load(packet, base, DataWidth::U8), Some(0x06));
        assert_eq!(packet_load(packet, base, DataWidth::U16), Some(0x0607));
        assert_eq!(packet_load(packet, base, DataWidth::U32), Some(0x06070809));
        assert_eq!(packet_load(packet, base, DataWidth::U64), Some(0x060708090A0B0001));

        // Loads past the Ethernet header load the packet body.
        assert_eq!(packet_load(packet, base + 8, DataWidth::U8), Some(0x02));
        assert_eq!(packet_load(packet, base + 8, DataWidth::U16), Some(0x0203));
        assert_eq!(packet_load(packet, base + 8, DataWidth::U32), Some(0x02030405));
        assert_eq!(packet_load(packet, base + 8, DataWidth::U64), Some(0x0203040508AB2122));
    }

    // Test loading packet body at the specified base offset.
    fn test_packet_body_load(packet: &IpPacketForBpf<'_>, base: i32) {
        assert_eq!(packet_load(packet, base, DataWidth::U64), Some(0x212223242526273A));
        assert_eq!(packet_load(packet, base, DataWidth::U8), Some(0x21));
        assert_eq!(packet_load(packet, base, DataWidth::U16), Some(0x2122));
        assert_eq!(packet_load(packet, base, DataWidth::U32), Some(0x21222324));
        assert_eq!(packet_load(packet, base, DataWidth::U64), Some(0x212223242526273A));

        assert_eq!(packet_load(packet, base + 6, DataWidth::U8), Some(0x27));
        assert_eq!(packet_load(packet, base + 6, DataWidth::U16), Some(0x273A));
        assert_eq!(packet_load(packet, base + 6, DataWidth::U32), None);
        assert_eq!(packet_load(packet, base + 6, DataWidth::U64), None);

        assert_eq!(packet_load(packet, base + 9, DataWidth::U8), None);
        assert_eq!(packet_load(packet, base + 9, DataWidth::U16), None);
        assert_eq!(packet_load(packet, base + 9, DataWidth::U32), None);
        assert_eq!(packet_load(packet, base + 9, DataWidth::U64), None);
    }

    #[test]
    fn network_level_packet() {
        let packet = IpPacketForBpf {
            sk_buff: Default::default(),
            kind: fppacket::Kind::Network,
            frame: TestData::frame(),
            raw: TestData::BUFFER,
        };

        test_packet_body_load(&packet, 0);

        assert_eq!(packet_load(&packet, i32::MAX, DataWidth::U8), None);
        assert_eq!(packet_load(&packet, i32::MAX, DataWidth::U16), None);
        assert_eq!(packet_load(&packet, i32::MAX, DataWidth::U32), None);
        assert_eq!(packet_load(&packet, i32::MAX, DataWidth::U64), None);
    }

    #[test]
    fn link_level_packet() {
        let packet = IpPacketForBpf {
            sk_buff: Default::default(),
            kind: fppacket::Kind::Link,
            frame: TestData::frame(),
            raw: TestData::BUFFER,
        };

        test_ll_header_load(&packet, 0);
        test_packet_body_load(&packet, TestData::BODY_POSITION);
    }

    #[test]
    fn negative_offsets() {
        let packet = IpPacketForBpf {
            sk_buff: Default::default(),
            kind: fppacket::Kind::Link,
            frame: TestData::frame(),
            raw: TestData::BUFFER,
        };

        // Loads from SKF_AD_OFF + SKF_AD_PROTOCOL load EtherType, ignoring data width.
        assert_eq!(
            packet_load(&packet, SKF_AD_OFF + SKF_AD_PROTOCOL, DataWidth::U8),
            Some(TestData::PROTO as u64)
        );
        assert_eq!(
            packet_load(&packet, SKF_AD_OFF + SKF_AD_PROTOCOL, DataWidth::U16),
            Some(TestData::PROTO as u64)
        );
        assert_eq!(
            packet_load(&packet, SKF_AD_OFF + SKF_AD_PROTOCOL, DataWidth::U32),
            Some(TestData::PROTO as u64)
        );

        // SKF_AD_MAX is the max offset that can be used with SKF_AD_OFF.
        assert_eq!(packet_load(&packet, SKF_AD_OFF + SKF_AD_MAX, DataWidth::U16), None);
        assert_eq!(packet_load(&packet, SKF_AD_OFF + SKF_AD_MAX + 1, DataWidth::U16), None);

        // SKF_LL_OFF can be used to load the packet starting from the LL header.
        test_ll_header_load(&packet, SKF_LL_OFF);
        test_packet_body_load(&packet, SKF_LL_OFF + TestData::BODY_POSITION);

        // Loads with `offset = SKF_NET_OFF+n` load the packet starting from the
        // packet body (Network-level header).
        test_packet_body_load(&packet, SKF_NET_OFF);

        // Loads below `SKF_LL_OFF` should always fail.
        assert_eq!(packet_load(&packet, SKF_LL_OFF - 1, DataWidth::U16), None);
        assert_eq!(packet_load(&packet, SKF_LL_OFF - 8, DataWidth::U16), None);
        assert_eq!(packet_load(&packet, i32::MIN, DataWidth::U16), None);
    }

    #[test]
    fn maps_cache() {
        let schema = MapSchema {
            map_type: ebpf_api::BPF_MAP_TYPE_HASH,
            key_size: 1,
            value_size: 2,
            max_entries: 10,
        };

        let cache = Arc::new(EbpfMapCache::default());

        let num_cached = || cache.maps.lock().len();
        assert_eq!(num_cached(), 0);

        // Create a map and insert it to the cache.
        let map1 = Map::new(schema, 0).unwrap();
        let fidl_map = map1.share().unwrap();
        let cache_ref1 = cache.init_map(fidl_map).unwrap();

        let num_cached = || cache.maps.lock().len();
        assert_eq!(num_cached(), 1);

        // Import second map.
        let map2 = Map::new(schema, 0).unwrap();
        let fidl_map = map2.share().unwrap();
        let cache_ref2 = cache.init_map(fidl_map).unwrap();

        let num_cached = || cache.maps.lock().len();
        assert_eq!(num_cached(), 2);

        // Import the first map again. The cached entry should be reused.
        let fidl_map = map1.share().unwrap();
        let cache_ref1_dup = cache.init_map(fidl_map).unwrap();
        assert_eq!(num_cached(), 2);

        // Map should be imported only once, so `as_bpf_value()` will return
        // the same value for both refs.
        assert_eq!(cache_ref1.as_bpf_value().as_u64(), cache_ref1_dup.as_bpf_value().as_u64());

        // But the `BpfValue` is different when the maps are different.
        assert_ne!(cache_ref1.as_bpf_value().as_u64(), cache_ref2.as_bpf_value().as_u64());

        // Maps are removed from the cache when the references are dropped.
        std::mem::drop(cache_ref2);
        assert_eq!(num_cached(), 1);

        std::mem::drop(cache_ref1);
        assert_eq!(num_cached(), 1);

        std::mem::drop(cache_ref1_dup);
        assert_eq!(num_cached(), 0);
    }

    #[ip_test(I)]
    fn run_egress_prog<I: TestIpExt + FilterIpExt>() {
        let prog =
            ebpf_loader::load_ebpf_program("/pkg/data/ebpf_test_progs.o", ".text", "egress_prog")
                .expect("Failed to load test prog");
        let maps_schema = prog.maps.iter().map(|m| m.schema).collect();
        let calling_context = ProgramType::CgroupSkb
            .create_calling_context(AttachType::CgroupInetEgress, maps_schema)
            .expect("Failed to create CallingContext");
        let verified =
            ebpf::verify_program(prog.code, calling_context, &mut ebpf::NullVerifierLogger)
                .expect("Failed to verify loaded program");

        let maps: Vec<_> = prog
            .maps
            .iter()
            .map(|def| ebpf_api::Map::new(def.schema, def.flags).expect("Failed to create a map"))
            .collect();
        let shared_maps =
            maps.iter().map(|map| map.share().expect("Failed to share a map")).collect();

        let manager = EbpfManager::default();

        let code: Vec<u64> =
            <[u64]>::ref_from_bytes(verified.code().as_bytes()).unwrap().to_owned();
        let struct_access_instructions = verified
            .struct_access_instructions()
            .iter()
            .map(|s| febpf::StructAccess {
                pc: s.pc.try_into().unwrap(),
                struct_memory_id: s.memory_id.id(),
                field_offset: s.field_offset.try_into().unwrap(),
                is_32_bit_ptr_load: s.is_32_bit_ptr_load,
            })
            .collect();

        let program = ValidVerifiedProgram { code, struct_access_instructions, maps: shared_maps };
        let program = CgroupSkbProgram::new(program, manager.maps_cache())
            .expect("Failed to initialize a program");

        const SRC_PORT: NonZeroU16 = NonZeroU16::new(1234).unwrap();
        const DST_PORT: NonZeroU16 = NonZeroU16::new(5678).unwrap();

        let data = b"PACKET";
        let mut udp_packet = UdpPacketBuilder::new(
            I::TEST_ADDRS.local_ip.get(),
            I::TEST_ADDRS.remote_ip.get(),
            Some(SRC_PORT),
            DST_PORT,
        )
        .wrap_body(data.into_serializer());
        let packet = testutil::new_filter_egress_ip_packet::<I, _>(
            I::TEST_ADDRS.local_ip.get(),
            I::TEST_ADDRS.remote_ip.get(),
            IpProto::Udp.into(),
            &mut udp_packet,
        );

        const IFINDEX: u32 = 2;
        const UID: u32 = 231;

        let socket_resource_token = ResourceTokenValue::default();
        let socket_cookie = SocketCookie::new(socket_resource_token.token());

        let mut marks = Marks::default();
        *marks.get_mut(MarkDomain::Mark2) = Mark(Some(UID));

        let r = program.run(&packet, IFINDEX, socket_cookie, &marks);
        assert_eq!(r, 1);

        // Results struct. Must match the `struct test_result` in
        // `ebpf_test_progs.c`.
        #[derive(FromBytes, Immutable, KnownLayout)]
        #[repr(C)]
        // LINT.IfChange
        struct TestResult {
            cookie: u64,
            uid: u32,
            ifindex: u32,
            proto: u32,
            ip_proto: u8,
        }
        // LINT.ThenChange(//src/connectivity/network/netstack3/tests/ebpf/ebpf_test_progs.c)

        // Check the result.
        let result = maps[0].load(&[0; 4]).expect("Failed to retrieve test result");
        let result =
            TestResult::ref_from_bytes(&result).expect("Failed to convert test results struct");

        assert_eq!(result.cookie, socket_resource_token.token().export_value());
        assert_eq!(result.uid, UID);
        assert_eq!(result.ifindex, IFINDEX);
        assert_eq!(result.proto, u32::from(u16::from(I::ETHER_TYPE)));
        assert_eq!(result.ip_proto, u8::from(IpProto::Udp));
    }
}
