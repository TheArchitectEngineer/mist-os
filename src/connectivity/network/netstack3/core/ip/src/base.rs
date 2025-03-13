// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use alloc::boxed::Box;
use alloc::collections::HashMap;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::convert::Infallible as Never;
use core::fmt::Debug;
use core::hash::Hash;
use core::marker::PhantomData;
use core::num::NonZeroU8;
use core::ops::ControlFlow;
#[cfg(test)]
use core::ops::DerefMut;
use core::sync::atomic::{self, AtomicU16};

use derivative::Derivative;
use explicit::ResultExt as _;
use lock_order::lock::{OrderedLockAccess, OrderedLockRef};
use log::{debug, error, trace};
use net_types::ip::{
    GenericOverIp, Ip, Ipv4, Ipv4Addr, Ipv6, Ipv6Addr, Ipv6SourceAddr, Mtu, Subnet,
};
use net_types::{
    MulticastAddr, MulticastAddress, NonMappedAddr, NonMulticastAddr, SpecifiedAddr,
    SpecifiedAddress as _, UnicastAddr, Witness,
};
use netstack3_base::socket::SocketIpAddrExt as _;
use netstack3_base::sync::{Mutex, PrimaryRc, RwLock, StrongRc, WeakRc};
use netstack3_base::{
    AnyDevice, BroadcastIpExt, CoreTimerContext, Counter, CounterContext, DeviceIdContext,
    DeviceIdentifier as _, DeviceWithName, ErrorAndSerializer, EventContext, FrameDestination,
    HandleableTimer, Inspectable, Inspector, InspectorExt as _, InstantContext, IpAddressId,
    IpDeviceAddr, IpDeviceAddressIdContext, IpExt, Matcher as _, NestedIntoCoreTimerCtx,
    NotFoundError, RngContext, SendFrameErrorReason, StrongDeviceIdentifier, TimerBindingsTypes,
    TimerContext, TimerHandler, TracingContext, TxMetadataBindingsTypes, WeakIpAddressId,
    WrapBroadcastMarker,
};
use netstack3_filter::{
    self as filter, ConnectionDirection, ConntrackConnection, FilterBindingsContext,
    FilterBindingsTypes, FilterHandler as _, FilterIpContext, FilterIpExt, FilterIpMetadata,
    FilterTimerId, ForwardedPacket, IngressVerdict, IpPacket, TransportPacketSerializer, Tuple,
    WeakConnectionError, WeakConntrackConnection,
};
use packet::{
    Buf, BufferAlloc, BufferMut, GrowBuffer, PacketConstraints, ParseBufferMut, ParseMetadata,
    SerializeError, Serializer as _,
};
use packet_formats::error::IpParseError;
use packet_formats::ip::{DscpAndEcn, IpPacket as _, IpPacketBuilder as _};
use packet_formats::ipv4::{Ipv4FragmentType, Ipv4Packet};
use packet_formats::ipv6::Ipv6Packet;
use thiserror::Error;
use zerocopy::SplitByteSlice;

use crate::internal::device::opaque_iid::IidSecret;
use crate::internal::device::slaac::SlaacCounters;
use crate::internal::device::state::{
    IpDeviceStateBindingsTypes, IpDeviceStateIpExt, Ipv6AddressFlags, Ipv6AddressState,
};
use crate::internal::device::{
    self, IpAddressIdExt, IpDeviceBindingsContext, IpDeviceIpExt, IpDeviceSendContext,
};
use crate::internal::fragmentation::{
    FragmentableIpSerializer, FragmentationCounters, FragmentationIpExt, IpFragmenter,
};
use crate::internal::gmp::igmp::IgmpCounters;
use crate::internal::gmp::mld::MldCounters;
use crate::internal::gmp::GmpQueryHandler;
use crate::internal::icmp::{
    IcmpBindingsTypes, IcmpErrorHandler, IcmpHandlerIpExt, Icmpv4Error, Icmpv4ErrorKind,
    Icmpv4State, Icmpv4StateBuilder, Icmpv6ErrorKind, Icmpv6State, Icmpv6StateBuilder,
};
use crate::internal::ipv6::Ipv6PacketAction;
use crate::internal::local_delivery::{
    IpHeaderInfo, Ipv4HeaderInfo, Ipv6HeaderInfo, LocalDeliveryPacketInfo, ReceiveIpPacketMeta,
    TransparentLocalDelivery,
};
use crate::internal::multicast_forwarding::counters::MulticastForwardingCounters;
use crate::internal::multicast_forwarding::route::{
    MulticastRouteIpExt, MulticastRouteTarget, MulticastRouteTargets,
};
use crate::internal::multicast_forwarding::state::{
    MulticastForwardingState, MulticastForwardingStateContext,
};
use crate::internal::multicast_forwarding::{
    MulticastForwardingBindingsTypes, MulticastForwardingDeviceContext, MulticastForwardingEvent,
    MulticastForwardingTimerId,
};
use crate::internal::path_mtu::{PmtuBindingsTypes, PmtuCache, PmtuTimerId};
use crate::internal::raw::counters::RawIpSocketCounters;
use crate::internal::raw::{RawIpSocketHandler, RawIpSocketMap, RawIpSocketsBindingsTypes};
use crate::internal::reassembly::{
    FragmentBindingsTypes, FragmentHandler, FragmentProcessingState, FragmentTimerId,
    FragmentablePacket, IpPacketFragmentCache,
};
use crate::internal::routing::rules::{Marks, Rule, RuleAction, RuleInput, RulesTable};
use crate::internal::routing::{
    IpRoutingDeviceContext, NonLocalSrcAddrPolicy, PacketOrigin, RoutingTable,
};
use crate::internal::socket::{IpSocketBindingsContext, IpSocketContext, IpSocketHandler};
use crate::internal::types::{
    self, Destination, InternalForwarding, NextHop, ResolvedRoute, RoutableIpAddr,
};
use crate::internal::{ipv6, multicast_forwarding};

#[cfg(test)]
mod tests;

/// Default IPv4 TTL.
pub const DEFAULT_TTL: NonZeroU8 = NonZeroU8::new(64).unwrap();

/// Hop limits for packets sent to multicast and unicast destinations.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub struct HopLimits {
    pub unicast: NonZeroU8,
    pub multicast: NonZeroU8,
}

/// Default hop limits for sockets.
pub const DEFAULT_HOP_LIMITS: HopLimits =
    HopLimits { unicast: DEFAULT_TTL, multicast: NonZeroU8::new(1).unwrap() };

/// The IPv6 subnet that contains all addresses; `::/0`.
// Safe because 0 is less than the number of IPv6 address bits.
pub const IPV6_DEFAULT_SUBNET: Subnet<Ipv6Addr> =
    unsafe { Subnet::new_unchecked(Ipv6::UNSPECIFIED_ADDRESS, 0) };

/// An error encountered when receiving a transport-layer packet.
#[derive(Debug)]
#[allow(missing_docs)]
pub enum TransportReceiveError {
    ProtocolUnsupported,
    PortUnreachable,
}

impl TransportReceiveError {
    fn into_icmpv4_error(self, header_len: usize) -> Icmpv4Error {
        let kind = match self {
            TransportReceiveError::ProtocolUnsupported => Icmpv4ErrorKind::ProtocolUnreachable,
            TransportReceiveError::PortUnreachable => Icmpv4ErrorKind::PortUnreachable,
        };
        Icmpv4Error { kind, header_len }
    }

    fn into_icmpv6_error(self, header_len: usize) -> Icmpv6ErrorKind {
        match self {
            TransportReceiveError::ProtocolUnsupported => {
                Icmpv6ErrorKind::ProtocolUnreachable { header_len }
            }
            TransportReceiveError::PortUnreachable => Icmpv6ErrorKind::PortUnreachable,
        }
    }
}

/// Sidecar metadata passed along with the packet.
///
/// Note: This metadata may be regenerated when packet handling requires
/// performing multiple actions (e.g. sending the packet out multiple interfaces
/// as part of multicast forwarding).
#[derive(Derivative)]
#[derivative(Default(bound = ""))]
pub struct IpLayerPacketMetadata<
    I: packet_formats::ip::IpExt,
    A,
    BT: FilterBindingsTypes + TxMetadataBindingsTypes,
> {
    conntrack_connection_and_direction:
        Option<(ConntrackConnection<I, A, BT>, ConnectionDirection)>,
    /// Tx metadata associated with this packet.
    ///
    /// This may be non-default even in the rx path for looped back packets that
    /// are still forcing tx frame ownership for sockets.
    tx_metadata: BT::TxMetadata,
    /// Marks attached to the packet that can be acted upon by routing/filtering.
    marks: Marks,
    #[cfg(debug_assertions)]
    drop_check: IpLayerPacketMetadataDropCheck,
}

/// A type that asserts, on drop, that it was intentionally being dropped.
///
/// NOTE: Unfortunately, debugging this requires backtraces, since track_caller
/// won't do what we want (https://github.com/rust-lang/rust/issues/116942).
/// Since this is only enabled in debug, the assumption is that stacktraces are
/// enabled.
#[cfg(debug_assertions)]
#[derive(Default)]
struct IpLayerPacketMetadataDropCheck {
    okay_to_drop: bool,
}

/// Metadata that is produced and consumed by the IP layer for each packet, but
/// which also traverses the device layer.
#[derive(Derivative)]
#[derivative(Debug(bound = ""), Default(bound = ""))]
pub struct DeviceIpLayerMetadata<BT: TxMetadataBindingsTypes> {
    /// Weak reference to this packet's connection tracking entry, if the packet is
    /// tracked.
    ///
    /// This allows NAT to consistently associate locally-generated, looped-back
    /// packets with the same connection at every filtering hook even when NAT may
    /// have been performed on them, causing them to no longer match the original or
    /// reply tuples of the connection.
    conntrack_entry: Option<(WeakConntrackConnection, ConnectionDirection)>,
    /// Tx metadata associated with this packet.
    ///
    /// This may be non-default even in the rx path for looped back packets that
    /// are still forcing tx frame ownership for sockets.
    tx_metadata: BT::TxMetadata,
    /// Marks attached to this packet. For all the incoming packets, they are None
    /// by default but can be changed by a filtering rule.
    ///
    /// Note: The marks will be preserved if the packet is being looped back, i.e.,
    /// the receiver will be able to observe the marks set by the sender. This is
    /// consistent with Linux behavior.
    marks: Marks,
}

impl<BT: TxMetadataBindingsTypes> DeviceIpLayerMetadata<BT> {
    /// Discards the remaining IP layer information and returns only the tx
    /// metadata used for buffer ownership.
    pub fn into_tx_metadata(self) -> BT::TxMetadata {
        self.tx_metadata
    }
    /// Creates new IP layer metadata with the marks.
    #[cfg(any(test, feature = "testutils"))]
    pub fn with_marks(marks: Marks) -> Self {
        Self { conntrack_entry: None, tx_metadata: Default::default(), marks }
    }
}

impl<
        I: IpLayerIpExt,
        A: WeakIpAddressId<I::Addr>,
        BT: FilterBindingsTypes + TxMetadataBindingsTypes,
    > IpLayerPacketMetadata<I, A, BT>
{
    fn from_device_ip_layer_metadata<CC>(
        core_ctx: &mut CC,
        DeviceIpLayerMetadata { conntrack_entry, tx_metadata, marks }: DeviceIpLayerMetadata<BT>,
    ) -> Self
    where
        CC: CounterContext<IpCounters<I>>,
    {
        let conntrack_connection_and_direction = match conntrack_entry
            .map(|(conn, dir)| conn.into_inner().map(|conn| (conn, dir)))
            .transpose()
        {
            // Either the packet was tracked and we've preserved its conntrack entry across
            // loopback, or it was untracked and we just stash the `None`.
            Ok(conn_and_dir) => conn_and_dir,
            // Conntrack entry was removed from table after packet was enqueued in loopback.
            Err(WeakConnectionError::EntryRemoved) => None,
            // Conntrack entry no longer matches the packet (for example, it could be that
            // this is an IPv6 packet that was modified at the device layer and therefore it
            // no longer matches its IPv4 conntrack entry).
            Err(WeakConnectionError::InvalidEntry) => {
                core_ctx.counters().invalid_cached_conntrack_entry.increment();
                None
            }
        };
        Self {
            conntrack_connection_and_direction,
            tx_metadata,
            marks,
            #[cfg(debug_assertions)]
            drop_check: Default::default(),
        }
    }
}

impl<I: IpExt, A, BT: FilterBindingsTypes + TxMetadataBindingsTypes>
    IpLayerPacketMetadata<I, A, BT>
{
    pub(crate) fn from_tx_metadata_and_marks(tx_metadata: BT::TxMetadata, marks: Marks) -> Self {
        Self {
            conntrack_connection_and_direction: None,
            tx_metadata,
            marks,
            #[cfg(debug_assertions)]
            drop_check: Default::default(),
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (Option<(ConntrackConnection<I, A, BT>, ConnectionDirection)>, BT::TxMetadata, Marks) {
        let Self {
            tx_metadata,
            marks,
            conntrack_connection_and_direction,
            #[cfg(debug_assertions)]
            mut drop_check,
        } = self;
        #[cfg(debug_assertions)]
        {
            drop_check.okay_to_drop = true;
        }
        (conntrack_connection_and_direction, tx_metadata, marks)
    }

    /// Acknowledge that it's okay to drop this packet metadata.
    ///
    /// When compiled with debug assertions, dropping [`IplayerPacketMetadata`]
    /// will panic if this method has not previously been called.
    pub(crate) fn acknowledge_drop(self) {
        #[cfg(debug_assertions)]
        {
            let mut this = self;
            this.drop_check.okay_to_drop = true;
        }
    }
}

#[cfg(debug_assertions)]
impl Drop for IpLayerPacketMetadataDropCheck {
    fn drop(&mut self) {
        if !self.okay_to_drop {
            panic!(
                "IpLayerPacketMetadata dropped without acknowledgement.  https://fxbug.dev/334127474"
            );
        }
    }
}

impl<I: packet_formats::ip::IpExt, A, BT: FilterBindingsTypes + TxMetadataBindingsTypes>
    FilterIpMetadata<I, A, BT> for IpLayerPacketMetadata<I, A, BT>
{
    fn take_connection_and_direction(
        &mut self,
    ) -> Option<(ConntrackConnection<I, A, BT>, ConnectionDirection)> {
        self.conntrack_connection_and_direction.take()
    }

    fn replace_connection_and_direction(
        &mut self,
        conn: ConntrackConnection<I, A, BT>,
        direction: ConnectionDirection,
    ) -> Option<ConntrackConnection<I, A, BT>> {
        self.conntrack_connection_and_direction.replace((conn, direction)).map(|(conn, _dir)| conn)
    }
}

/// Send errors observed at or above the IP layer that carry a serializer.
pub type IpSendFrameError<S> = ErrorAndSerializer<IpSendFrameErrorReason, S>;

/// Send error cause for [`IpSendFrameError`].
#[derive(Debug, PartialEq)]
pub enum IpSendFrameErrorReason {
    /// Error comes from the device layer.
    Device(SendFrameErrorReason),
    /// The frame's source or destination address is in the loopback subnet, but
    /// the target device is not the loopback device.
    IllegalLoopbackAddress,
}

impl From<SendFrameErrorReason> for IpSendFrameErrorReason {
    fn from(value: SendFrameErrorReason) -> Self {
        Self::Device(value)
    }
}

/// The execution context provided by a transport layer protocol to the IP
/// layer.
///
/// An implementation for `()` is provided which indicates that a particular
/// transport layer protocol is unsupported.
pub trait IpTransportContext<I: IpExt, BC, CC: DeviceIdContext<AnyDevice> + ?Sized> {
    /// Receive an ICMP error message.
    ///
    /// All arguments beginning with `original_` are fields from the IP packet
    /// that triggered the error. The `original_body` is provided here so that
    /// the error can be associated with a transport-layer socket. `device`
    /// identifies the device that received the ICMP error message packet.
    ///
    /// While ICMPv4 error messages are supposed to contain the first 8 bytes of
    /// the body of the offending packet, and ICMPv6 error messages are supposed
    /// to contain as much of the offending packet as possible without violating
    /// the IPv6 minimum MTU, the caller does NOT guarantee that either of these
    /// hold. It is `receive_icmp_error`'s responsibility to handle any length
    /// of `original_body`, and to perform any necessary validation.
    fn receive_icmp_error(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        device: &CC::DeviceId,
        original_src_ip: Option<SpecifiedAddr<I::Addr>>,
        original_dst_ip: SpecifiedAddr<I::Addr>,
        original_body: &[u8],
        err: I::ErrorCode,
    );

    /// Receive a transport layer packet in an IP packet.
    ///
    /// In the event of an unreachable port, `receive_ip_packet` returns the
    /// buffer in its original state (with the transport packet un-parsed) in
    /// the `Err` variant.
    fn receive_ip_packet<B: BufferMut, H: IpHeaderInfo<I>>(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        device: &CC::DeviceId,
        src_ip: I::RecvSrcAddr,
        dst_ip: SpecifiedAddr<I::Addr>,
        buffer: B,
        info: &LocalDeliveryPacketInfo<I, H>,
    ) -> Result<(), (B, TransportReceiveError)>;
}

impl<I: IpExt, BC, CC: DeviceIdContext<AnyDevice> + ?Sized> IpTransportContext<I, BC, CC> for () {
    fn receive_icmp_error(
        _core_ctx: &mut CC,
        _bindings_ctx: &mut BC,
        _device: &CC::DeviceId,
        _original_src_ip: Option<SpecifiedAddr<I::Addr>>,
        _original_dst_ip: SpecifiedAddr<I::Addr>,
        _original_body: &[u8],
        err: I::ErrorCode,
    ) {
        trace!("IpTransportContext::receive_icmp_error: Received ICMP error message ({:?}) for unsupported IP protocol", err);
    }

    fn receive_ip_packet<B: BufferMut, H: IpHeaderInfo<I>>(
        _core_ctx: &mut CC,
        _bindings_ctx: &mut BC,
        _device: &CC::DeviceId,
        _src_ip: I::RecvSrcAddr,
        _dst_ip: SpecifiedAddr<I::Addr>,
        buffer: B,
        _info: &LocalDeliveryPacketInfo<I, H>,
    ) -> Result<(), (B, TransportReceiveError)> {
        Err((buffer, TransportReceiveError::ProtocolUnsupported))
    }
}

/// The base execution context provided by the IP layer to transport layer
/// protocols.
pub trait BaseTransportIpContext<I: IpExt, BC>: DeviceIdContext<AnyDevice> {
    /// The iterator given to
    /// [`BaseTransportIpContext::with_devices_with_assigned_addr`].
    type DevicesWithAddrIter<'s>: Iterator<Item = Self::DeviceId>;

    /// Is this one of our local addresses, and is it in the assigned state?
    ///
    /// Calls `cb` with an iterator over all the local interfaces for which
    /// `addr` is an associated address, and, for IPv6, for which it is in the
    /// "assigned" state.
    fn with_devices_with_assigned_addr<O, F: FnOnce(Self::DevicesWithAddrIter<'_>) -> O>(
        &mut self,
        addr: SpecifiedAddr<I::Addr>,
        cb: F,
    ) -> O;

    /// Get default hop limits.
    ///
    /// If `device` is not `None` and exists, its hop limits will be returned.
    /// Otherwise the system defaults are returned.
    fn get_default_hop_limits(&mut self, device: Option<&Self::DeviceId>) -> HopLimits;

    /// Gets the original destination for the tracked connection indexed by
    /// `tuple`, which includes the source and destination addresses and
    /// transport-layer ports as well as the transport protocol number.
    fn get_original_destination(&mut self, tuple: &Tuple<I>) -> Option<(I::Addr, u16)>;
}

/// A marker trait for the traits required by the transport layer from the IP
/// layer.
pub trait TransportIpContext<I: IpExt, BC: TxMetadataBindingsTypes>:
    BaseTransportIpContext<I, BC> + IpSocketHandler<I, BC>
{
}

impl<I, CC, BC> TransportIpContext<I, BC> for CC
where
    I: IpExt,
    CC: BaseTransportIpContext<I, BC> + IpSocketHandler<I, BC>,
    BC: TxMetadataBindingsTypes,
{
}

/// Abstraction over the ability to join and leave multicast groups.
pub trait MulticastMembershipHandler<I: Ip, BC>: DeviceIdContext<AnyDevice> {
    /// Requests that the specified device join the given multicast group.
    ///
    /// If this method is called multiple times with the same device and
    /// address, the device will remain joined to the multicast group until
    /// [`MulticastTransportIpContext::leave_multicast_group`] has been called
    /// the same number of times.
    fn join_multicast_group(
        &mut self,
        bindings_ctx: &mut BC,
        device: &Self::DeviceId,
        addr: MulticastAddr<I::Addr>,
    );

    /// Requests that the specified device leave the given multicast group.
    ///
    /// Each call to this method must correspond to an earlier call to
    /// [`MulticastTransportIpContext::join_multicast_group`]. The device
    /// remains a member of the multicast group so long as some call to
    /// `join_multicast_group` has been made without a corresponding call to
    /// `leave_multicast_group`.
    fn leave_multicast_group(
        &mut self,
        bindings_ctx: &mut BC,
        device: &Self::DeviceId,
        addr: MulticastAddr<I::Addr>,
    );

    /// Selects a default device with which to join the given multicast group.
    ///
    /// The selection is made by consulting the routing table; If there is no
    /// route available to the given address, an error is returned.
    fn select_device_for_multicast_group(
        &mut self,
        addr: MulticastAddr<I::Addr>,
        marks: &Marks,
    ) -> Result<Self::DeviceId, ResolveRouteError>;
}

// TODO(joshlf): With all 256 protocol numbers (minus reserved ones) given their
// own associated type in both traits, running `cargo check` on a 2018 MacBook
// Pro takes over a minute. Eventually - and before we formally publish this as
// a library - we should identify the bottleneck in the compiler and optimize
// it. For the time being, however, we only support protocol numbers that we
// actually use (TCP and UDP).

/// Enables a blanket implementation of [`TransportIpContext`].
///
/// Implementing this marker trait for a type enables a blanket implementation
/// of `TransportIpContext` given the other requirements are met.
pub trait UseTransportIpContextBlanket {}

/// An iterator supporting the blanket implementation of
/// [`BaseTransportIpContext::with_devices_with_assigned_addr`].
pub struct AssignedAddressDeviceIterator<Iter, I, D>(Iter, PhantomData<(I, D)>);

impl<Iter, I, D> Iterator for AssignedAddressDeviceIterator<Iter, I, D>
where
    Iter: Iterator<Item = (D, I::AddressStatus)>,
    I: IpLayerIpExt,
{
    type Item = D;
    fn next(&mut self) -> Option<D> {
        let Self(iter, PhantomData) = self;
        iter.by_ref().find_map(|(device, state)| is_unicast_assigned::<I>(&state).then_some(device))
    }
}

impl<
        I: IpLayerIpExt,
        BC: FilterBindingsContext + TxMetadataBindingsTypes,
        CC: IpDeviceContext<I>
            + IpSocketHandler<I, BC>
            + IpStateContext<I>
            + FilterIpContext<I, BC>
            + UseTransportIpContextBlanket,
    > BaseTransportIpContext<I, BC> for CC
{
    type DevicesWithAddrIter<'s> =
        AssignedAddressDeviceIterator<CC::DeviceAndAddressStatusIter<'s>, I, CC::DeviceId>;

    fn with_devices_with_assigned_addr<O, F: FnOnce(Self::DevicesWithAddrIter<'_>) -> O>(
        &mut self,
        addr: SpecifiedAddr<I::Addr>,
        cb: F,
    ) -> O {
        self.with_address_statuses(addr, |it| cb(AssignedAddressDeviceIterator(it, PhantomData)))
    }

    fn get_default_hop_limits(&mut self, device: Option<&Self::DeviceId>) -> HopLimits {
        match device {
            Some(device) => HopLimits {
                unicast: IpDeviceEgressStateContext::<I>::get_hop_limit(self, device),
                ..DEFAULT_HOP_LIMITS
            },
            None => DEFAULT_HOP_LIMITS,
        }
    }

    fn get_original_destination(&mut self, tuple: &Tuple<I>) -> Option<(I::Addr, u16)> {
        self.with_filter_state(|state| {
            let conn = state.conntrack.get_connection(&tuple)?;

            if !conn.destination_nat() {
                return None;
            }

            // The tuple marking the original direction of the connection is
            // never modified by NAT. This means it can be used to recover the
            // destination before NAT was performed.
            let original = conn.original_tuple();
            Some((original.dst_addr, original.dst_port_or_id))
        })
    }
}

/// The status of an IP address on an interface.
#[derive(Debug, PartialEq)]
#[allow(missing_docs)]
pub enum AddressStatus<S> {
    Present(S),
    Unassigned,
}

impl<S> AddressStatus<S> {
    fn into_present(self) -> Option<S> {
        match self {
            Self::Present(s) => Some(s),
            Self::Unassigned => None,
        }
    }
}

impl AddressStatus<Ipv4PresentAddressStatus> {
    /// Creates an IPv4 `AddressStatus` for `addr` on `device`.
    pub fn from_context_addr_v4<
        BC: IpDeviceStateBindingsTypes,
        CC: device::IpDeviceStateContext<Ipv4, BC> + GmpQueryHandler<Ipv4, BC>,
    >(
        core_ctx: &mut CC,
        device: &CC::DeviceId,
        addr: SpecifiedAddr<Ipv4Addr>,
    ) -> AddressStatus<Ipv4PresentAddressStatus> {
        if addr.is_limited_broadcast() {
            return AddressStatus::Present(Ipv4PresentAddressStatus::LimitedBroadcast);
        }

        if MulticastAddr::new(addr.get())
            .is_some_and(|addr| GmpQueryHandler::gmp_is_in_group(core_ctx, device, addr))
        {
            return AddressStatus::Present(Ipv4PresentAddressStatus::Multicast);
        }

        core_ctx.with_address_ids(device, |mut addrs, _core_ctx| {
            addrs
                .find_map(|addr_id| {
                    let dev_addr = addr_id.addr_sub();
                    let (dev_addr, subnet) = dev_addr.addr_subnet();

                    if **dev_addr == addr {
                        Some(AddressStatus::Present(Ipv4PresentAddressStatus::Unicast))
                    } else if addr.get() == subnet.broadcast() {
                        Some(AddressStatus::Present(Ipv4PresentAddressStatus::SubnetBroadcast))
                    } else if device.is_loopback() && subnet.contains(addr.as_ref()) {
                        Some(AddressStatus::Present(Ipv4PresentAddressStatus::LoopbackSubnet))
                    } else {
                        None
                    }
                })
                .unwrap_or(AddressStatus::Unassigned)
        })
    }
}

impl AddressStatus<Ipv6PresentAddressStatus> {
    /// /// Creates an IPv6 `AddressStatus` for `addr` on `device`.
    pub fn from_context_addr_v6<
        BC: IpDeviceBindingsContext<Ipv6, CC::DeviceId>,
        CC: device::Ipv6DeviceContext<BC> + GmpQueryHandler<Ipv6, BC>,
    >(
        core_ctx: &mut CC,
        device: &CC::DeviceId,
        addr: SpecifiedAddr<Ipv6Addr>,
    ) -> AddressStatus<Ipv6PresentAddressStatus> {
        if MulticastAddr::new(addr.get())
            .is_some_and(|addr| GmpQueryHandler::gmp_is_in_group(core_ctx, device, addr))
        {
            return AddressStatus::Present(Ipv6PresentAddressStatus::Multicast);
        }

        let addr_id = match core_ctx.get_address_id(device, addr) {
            Ok(o) => o,
            Err(NotFoundError) => return AddressStatus::Unassigned,
        };

        let assigned = core_ctx.with_ip_address_state(
            device,
            &addr_id,
            |Ipv6AddressState { flags: Ipv6AddressFlags { assigned }, config: _ }| *assigned,
        );

        if assigned {
            AddressStatus::Present(Ipv6PresentAddressStatus::UnicastAssigned)
        } else {
            AddressStatus::Present(Ipv6PresentAddressStatus::UnicastTentative)
        }
    }
}

impl<S: GenericOverIp<I>, I: Ip> GenericOverIp<I> for AddressStatus<S> {
    type Type = AddressStatus<S::Type>;
}

/// The status of an IPv4 address.
#[derive(Debug, PartialEq)]
#[allow(missing_docs)]
pub enum Ipv4PresentAddressStatus {
    LimitedBroadcast,
    SubnetBroadcast,
    Multicast,
    Unicast,
    /// This status indicates that the queried device was Loopback. The address
    /// belongs to a subnet that is assigned to the interface. This status
    /// takes lower precedence than `Unicast` and `SubnetBroadcast``, E.g. if
    /// the loopback device is assigned `127.0.0.1/8`:
    ///   * address `127.0.0.1` -> `Unicast`
    ///   * address `127.0.0.2` -> `LoopbackSubnet`
    ///   * address `127.255.255.255` -> `SubnetBroadcast`
    /// This exists for Linux conformance, which on the Loopback device,
    /// considers an IPv4 address assigned if it belongs to one of the device's
    /// assigned subnets.
    LoopbackSubnet,
}

impl Ipv4PresentAddressStatus {
    fn to_broadcast_marker(&self) -> Option<<Ipv4 as BroadcastIpExt>::BroadcastMarker> {
        match self {
            Self::LimitedBroadcast | Self::SubnetBroadcast => Some(()),
            Self::Multicast | Self::Unicast | Self::LoopbackSubnet => None,
        }
    }
}

/// The status of an IPv6 address.
#[derive(Debug, PartialEq)]
#[allow(missing_docs)]
pub enum Ipv6PresentAddressStatus {
    Multicast,
    UnicastAssigned,
    UnicastTentative,
}

/// An extension trait providing IP layer properties.
pub trait IpLayerIpExt:
    IpExt
    + MulticastRouteIpExt
    + IcmpHandlerIpExt
    + FilterIpExt
    + FragmentationIpExt
    + IpDeviceIpExt
    + IpAddressIdExt
{
    /// IP Address status.
    type AddressStatus: Debug;
    /// IP Address state.
    type State<StrongDeviceId: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>: AsRef<
        IpStateInner<Self, StrongDeviceId, BT>,
    >;
    /// State kept for packet identifiers.
    type PacketIdState;
    /// The type of a single packet identifier.
    type PacketId;
    /// Receive counters.
    type RxCounters: Default + Inspectable;
    /// Produces the next packet ID from the state.
    fn next_packet_id_from_state(state: &Self::PacketIdState) -> Self::PacketId;
}

impl IpLayerIpExt for Ipv4 {
    type AddressStatus = Ipv4PresentAddressStatus;
    type State<StrongDeviceId: StrongDeviceIdentifier, BT: IpLayerBindingsTypes> =
        Ipv4State<StrongDeviceId, BT>;
    type PacketIdState = AtomicU16;
    type PacketId = u16;
    type RxCounters = Ipv4RxCounters;
    fn next_packet_id_from_state(next_packet_id: &Self::PacketIdState) -> Self::PacketId {
        // Relaxed ordering as we only need atomicity without synchronization. See
        // https://en.cppreference.com/w/cpp/atomic/memory_order#Relaxed_ordering
        // for more details.
        next_packet_id.fetch_add(1, atomic::Ordering::Relaxed)
    }
}

impl IpLayerIpExt for Ipv6 {
    type AddressStatus = Ipv6PresentAddressStatus;
    type State<StrongDeviceId: StrongDeviceIdentifier, BT: IpLayerBindingsTypes> =
        Ipv6State<StrongDeviceId, BT>;
    type PacketIdState = ();
    type PacketId = ();
    type RxCounters = Ipv6RxCounters;
    fn next_packet_id_from_state((): &Self::PacketIdState) -> Self::PacketId {
        ()
    }
}

/// The state context provided to the IP layer.
pub trait IpStateContext<I: IpLayerIpExt>:
    IpRouteTablesContext<I, DeviceId: DeviceWithName>
{
    /// The context that provides access to the IP routing tables.
    type IpRouteTablesCtx<'a>: IpRouteTablesContext<I, DeviceId = Self::DeviceId>;

    /// Gets an immutable reference to the rules table.
    fn with_rules_table<
        O,
        F: FnOnce(&mut Self::IpRouteTablesCtx<'_>, &RulesTable<I, Self::DeviceId>) -> O,
    >(
        &mut self,
        cb: F,
    ) -> O;

    /// Gets a mutable reference to the rules table.
    fn with_rules_table_mut<
        O,
        F: FnOnce(&mut Self::IpRouteTablesCtx<'_>, &mut RulesTable<I, Self::DeviceId>) -> O,
    >(
        &mut self,
        cb: F,
    ) -> O;
}

/// The state context that gives access to routing tables provided to the IP layer.
pub trait IpRouteTablesContext<I: IpLayerIpExt>:
    IpRouteTableContext<I> + IpDeviceContext<I>
{
    /// The inner context that can provide access to individual routing tables.
    type Ctx<'a>: IpRouteTableContext<
        I,
        DeviceId = Self::DeviceId,
        WeakDeviceId = Self::WeakDeviceId,
    >;

    /// Gets the main table ID.
    fn main_table_id(&self) -> RoutingTableId<I, Self::DeviceId>;

    /// Gets immutable access to all the routing tables that currently exist.
    fn with_ip_routing_tables<
        O,
        F: FnOnce(
            &mut Self::Ctx<'_>,
            &HashMap<
                RoutingTableId<I, Self::DeviceId>,
                PrimaryRc<RwLock<RoutingTable<I, Self::DeviceId>>>,
            >,
        ) -> O,
    >(
        &mut self,
        cb: F,
    ) -> O;

    /// Gets mutable access to all the routing tables that currently exist.
    fn with_ip_routing_tables_mut<
        O,
        F: FnOnce(
            &mut HashMap<
                RoutingTableId<I, Self::DeviceId>,
                PrimaryRc<RwLock<RoutingTable<I, Self::DeviceId>>>,
            >,
        ) -> O,
    >(
        &mut self,
        cb: F,
    ) -> O;

    // TODO(https://fxbug.dev/354724171): Remove this function when we no longer
    // make routing decisions starting from the main table.
    /// Calls the function with an immutable reference to IP routing table.
    fn with_main_ip_routing_table<
        O,
        F: FnOnce(&mut Self::IpDeviceIdCtx<'_>, &RoutingTable<I, Self::DeviceId>) -> O,
    >(
        &mut self,
        cb: F,
    ) -> O {
        let main_table_id = self.main_table_id();
        self.with_ip_routing_table(&main_table_id, cb)
    }

    // TODO(https://fxbug.dev/341194323): Remove this function when we no longer
    // only update the main routing table by default.
    /// Calls the function with a mutable reference to IP routing table.
    fn with_main_ip_routing_table_mut<
        O,
        F: FnOnce(&mut Self::IpDeviceIdCtx<'_>, &mut RoutingTable<I, Self::DeviceId>) -> O,
    >(
        &mut self,
        cb: F,
    ) -> O {
        let main_table_id = self.main_table_id();
        self.with_ip_routing_table_mut(&main_table_id, cb)
    }
}

/// The state context that gives access to a singular routing table.
pub trait IpRouteTableContext<I: IpLayerIpExt>: IpDeviceContext<I> {
    /// The inner device id context.
    type IpDeviceIdCtx<'a>: DeviceIdContext<AnyDevice, DeviceId = Self::DeviceId, WeakDeviceId = Self::WeakDeviceId>
        + IpRoutingDeviceContext<I>
        + IpDeviceContext<I>;

    /// Calls the function with an immutable reference to IP routing table.
    fn with_ip_routing_table<
        O,
        F: FnOnce(&mut Self::IpDeviceIdCtx<'_>, &RoutingTable<I, Self::DeviceId>) -> O,
    >(
        &mut self,
        table_id: &RoutingTableId<I, Self::DeviceId>,
        cb: F,
    ) -> O;

    /// Calls the function with a mutable reference to IP routing table.
    fn with_ip_routing_table_mut<
        O,
        F: FnOnce(&mut Self::IpDeviceIdCtx<'_>, &mut RoutingTable<I, Self::DeviceId>) -> O,
    >(
        &mut self,
        table_id: &RoutingTableId<I, Self::DeviceId>,
        cb: F,
    ) -> O;
}

/// Provides access to an IP device's state for IP layer egress.
pub trait IpDeviceEgressStateContext<I: IpLayerIpExt>: DeviceIdContext<AnyDevice> {
    /// Calls the callback with the next packet ID.
    fn with_next_packet_id<O, F: FnOnce(&I::PacketIdState) -> O>(&self, cb: F) -> O;

    /// Returns the best local address for communicating with the remote.
    fn get_local_addr_for_remote(
        &mut self,
        device_id: &Self::DeviceId,
        remote: Option<SpecifiedAddr<I::Addr>>,
    ) -> Option<IpDeviceAddr<I::Addr>>;

    /// Returns the hop limit.
    fn get_hop_limit(&mut self, device_id: &Self::DeviceId) -> NonZeroU8;
}

/// Provides access to an IP device's state for IP layer ingress.
pub trait IpDeviceIngressStateContext<I: IpLayerIpExt>: DeviceIdContext<AnyDevice> {
    /// Gets the status of an address.
    ///
    /// Only the specified device will be checked for the address. Returns
    /// [`AddressStatus::Unassigned`] if the address is not assigned to the
    /// device.
    fn address_status_for_device(
        &mut self,
        addr: SpecifiedAddr<I::Addr>,
        device_id: &Self::DeviceId,
    ) -> AddressStatus<I::AddressStatus>;
}

/// The IP device context provided to the IP layer.
pub trait IpDeviceContext<I: IpLayerIpExt>:
    IpDeviceEgressStateContext<I> + IpDeviceIngressStateContext<I>
{
    /// Is the device enabled?
    fn is_ip_device_enabled(&mut self, device_id: &Self::DeviceId) -> bool;

    /// The iterator provided to [`IpDeviceContext::with_address_statuses`].
    type DeviceAndAddressStatusIter<'a>: Iterator<Item = (Self::DeviceId, I::AddressStatus)>;

    /// Provides access to the status of an address.
    ///
    /// Calls the provided callback with an iterator over the devices for which
    /// the address is assigned and the status of the assignment for each
    /// device.
    fn with_address_statuses<F: FnOnce(Self::DeviceAndAddressStatusIter<'_>) -> R, R>(
        &mut self,
        addr: SpecifiedAddr<I::Addr>,
        cb: F,
    ) -> R;

    /// Returns true iff the device has unicast forwarding enabled.
    fn is_device_unicast_forwarding_enabled(&mut self, device_id: &Self::DeviceId) -> bool;
}

/// Provides the ability to check neighbor reachability via a specific device.
pub trait IpDeviceConfirmReachableContext<I: IpLayerIpExt, BC>: DeviceIdContext<AnyDevice> {
    /// Confirm transport-layer forward reachability to the specified neighbor
    /// through the specified device.
    fn confirm_reachable(
        &mut self,
        bindings_ctx: &mut BC,
        device: &Self::DeviceId,
        neighbor: SpecifiedAddr<I::Addr>,
    );
}

/// Provides access to an IP device's MTU for the IP layer.
pub trait IpDeviceMtuContext<I: Ip>: DeviceIdContext<AnyDevice> {
    /// Returns the MTU of the device.
    ///
    /// The MTU is the maximum size of an IP packet.
    fn get_mtu(&mut self, device_id: &Self::DeviceId) -> Mtu;
}

/// Events observed at the IP layer.
#[derive(Debug, Eq, Hash, PartialEq, GenericOverIp)]
#[generic_over_ip(I, Ip)]
pub enum IpLayerEvent<DeviceId, I: IpLayerIpExt> {
    /// A route needs to be added.
    AddRoute(types::AddableEntry<I::Addr, DeviceId>),
    /// Routes matching these specifiers need to be removed.
    RemoveRoutes {
        /// Destination subnet
        subnet: Subnet<I::Addr>,
        /// Outgoing interface
        device: DeviceId,
        /// Gateway/next-hop
        gateway: Option<SpecifiedAddr<I::Addr>>,
    },
    /// The multicast forwarding engine emitted an event.
    MulticastForwarding(MulticastForwardingEvent<I, DeviceId>),
}

impl<DeviceId, I: IpLayerIpExt> From<MulticastForwardingEvent<I, DeviceId>>
    for IpLayerEvent<DeviceId, I>
{
    fn from(event: MulticastForwardingEvent<I, DeviceId>) -> IpLayerEvent<DeviceId, I> {
        IpLayerEvent::MulticastForwarding(event)
    }
}

impl<DeviceId, I: IpLayerIpExt> IpLayerEvent<DeviceId, I> {
    /// Changes the device id type with `map`.
    pub fn map_device<N, F: Fn(DeviceId) -> N>(self, map: F) -> IpLayerEvent<N, I> {
        match self {
            IpLayerEvent::AddRoute(types::AddableEntry { subnet, device, gateway, metric }) => {
                IpLayerEvent::AddRoute(types::AddableEntry {
                    subnet,
                    device: map(device),
                    gateway,
                    metric,
                })
            }
            IpLayerEvent::RemoveRoutes { subnet, device, gateway } => {
                IpLayerEvent::RemoveRoutes { subnet, device: map(device), gateway }
            }
            IpLayerEvent::MulticastForwarding(e) => {
                IpLayerEvent::MulticastForwarding(e.map_device(map))
            }
        }
    }
}

/// An event signifying a router advertisement has been received.
#[derive(Derivative, PartialEq, Eq, Clone, Hash)]
#[derivative(Debug)]
pub struct RouterAdvertisementEvent<D> {
    /// The raw bytes of the router advertisement message's options.
    // NB: avoid deriving Debug for this since it could contain PII.
    #[derivative(Debug = "ignore")]
    pub options_bytes: Box<[u8]>,
    /// The source address of the RA message.
    pub source: net_types::ip::Ipv6Addr,
    /// The device on which the message was received.
    pub device: D,
}

impl<D> RouterAdvertisementEvent<D> {
    /// Maps the contained device ID type.
    pub fn map_device<N, F: Fn(D) -> N>(self, map: F) -> RouterAdvertisementEvent<N> {
        let Self { options_bytes, source, device } = self;
        RouterAdvertisementEvent { options_bytes, source, device: map(device) }
    }
}

/// Ipv6-specific bindings execution context for the IP layer.
pub trait NdpBindingsContext<DeviceId>: EventContext<RouterAdvertisementEvent<DeviceId>> {}
impl<DeviceId, BC: EventContext<RouterAdvertisementEvent<DeviceId>>> NdpBindingsContext<DeviceId>
    for BC
{
}

/// The bindings execution context for the IP layer.
pub trait IpLayerBindingsContext<I: IpLayerIpExt, DeviceId>:
    InstantContext
    + EventContext<IpLayerEvent<DeviceId, I>>
    + TracingContext
    + FilterBindingsContext
    + TxMetadataBindingsTypes
{
}
impl<
        I: IpLayerIpExt,
        DeviceId,
        BC: InstantContext
            + EventContext<IpLayerEvent<DeviceId, I>>
            + TracingContext
            + FilterBindingsContext
            + TxMetadataBindingsTypes,
    > IpLayerBindingsContext<I, DeviceId> for BC
{
}

/// A marker trait for bindings types at the IP layer.
pub trait IpLayerBindingsTypes: IcmpBindingsTypes + IpStateBindingsTypes {}
impl<BT: IcmpBindingsTypes + IpStateBindingsTypes> IpLayerBindingsTypes for BT {}

/// The execution context for the IP layer.
pub trait IpLayerContext<
    I: IpLayerIpExt,
    BC: IpLayerBindingsContext<I, <Self as DeviceIdContext<AnyDevice>>::DeviceId>,
>:
    IpStateContext<I>
    + IpDeviceContext<I>
    + IpDeviceMtuContext<I>
    + IpDeviceSendContext<I, BC>
    + IcmpErrorHandler<I, BC>
    + MulticastForwardingStateContext<I, BC>
    + MulticastForwardingDeviceContext<I>
    + CounterContext<MulticastForwardingCounters<I>>
{
}

impl<
        I: IpLayerIpExt,
        BC: IpLayerBindingsContext<I, <CC as DeviceIdContext<AnyDevice>>::DeviceId>,
        CC: IpStateContext<I>
            + IpDeviceContext<I>
            + IpDeviceMtuContext<I>
            + IpDeviceSendContext<I, BC>
            + IcmpErrorHandler<I, BC>
            + MulticastForwardingStateContext<I, BC>
            + MulticastForwardingDeviceContext<I>
            + CounterContext<MulticastForwardingCounters<I>>,
    > IpLayerContext<I, BC> for CC
{
}

fn is_unicast_assigned<I: IpLayerIpExt>(status: &I::AddressStatus) -> bool {
    #[derive(GenericOverIp)]
    #[generic_over_ip(I, Ip)]
    struct WrapAddressStatus<'a, I: IpLayerIpExt>(&'a I::AddressStatus);

    I::map_ip(
        WrapAddressStatus(status),
        |WrapAddressStatus(status)| match status {
            Ipv4PresentAddressStatus::Unicast | Ipv4PresentAddressStatus::LoopbackSubnet => true,
            Ipv4PresentAddressStatus::LimitedBroadcast
            | Ipv4PresentAddressStatus::SubnetBroadcast
            | Ipv4PresentAddressStatus::Multicast => false,
        },
        |WrapAddressStatus(status)| match status {
            Ipv6PresentAddressStatus::UnicastAssigned => true,
            Ipv6PresentAddressStatus::Multicast | Ipv6PresentAddressStatus::UnicastTentative => {
                false
            }
        },
    )
}

fn is_local_assigned_address<I: Ip + IpLayerIpExt, CC: IpDeviceIngressStateContext<I>>(
    core_ctx: &mut CC,
    device: &CC::DeviceId,
    addr: IpDeviceAddr<I::Addr>,
) -> bool {
    match core_ctx.address_status_for_device(addr.into(), device) {
        AddressStatus::Present(status) => is_unicast_assigned::<I>(&status),
        AddressStatus::Unassigned => false,
    }
}

fn get_device_with_assigned_address<I, CC>(
    core_ctx: &mut CC,
    addr: IpDeviceAddr<I::Addr>,
) -> Option<(CC::DeviceId, I::AddressStatus)>
where
    I: IpLayerIpExt,
    CC: IpDeviceContext<I>,
{
    core_ctx.with_address_statuses(addr.into(), |mut it| {
        it.find_map(|(device, status)| {
            is_unicast_assigned::<I>(&status).then_some((device, status))
        })
    })
}

// Returns the local IP address to use for sending packets from the
// given device to `addr`, restricting to `local_ip` if it is not
// `None`.
fn get_local_addr<I: Ip + IpLayerIpExt, CC: IpDeviceContext<I>>(
    core_ctx: &mut CC,
    local_ip_and_policy: Option<(IpDeviceAddr<I::Addr>, NonLocalSrcAddrPolicy)>,
    device: &CC::DeviceId,
    remote_addr: Option<RoutableIpAddr<I::Addr>>,
) -> Result<IpDeviceAddr<I::Addr>, ResolveRouteError> {
    match local_ip_and_policy {
        Some((local_ip, NonLocalSrcAddrPolicy::Allow)) => Ok(local_ip),
        Some((local_ip, NonLocalSrcAddrPolicy::Deny)) => {
            is_local_assigned_address(core_ctx, device, local_ip)
                .then_some(local_ip)
                .ok_or(ResolveRouteError::NoSrcAddr)
        }
        None => core_ctx
            .get_local_addr_for_remote(device, remote_addr.map(Into::into))
            .ok_or(ResolveRouteError::NoSrcAddr),
    }
}

/// An error occurred while resolving the route to a destination
#[derive(Error, Copy, Clone, Debug, Eq, GenericOverIp, PartialEq)]
#[generic_over_ip()]
pub enum ResolveRouteError {
    /// A source address could not be selected.
    #[error("a source address could not be selected")]
    NoSrcAddr,
    /// The destination in unreachable.
    #[error("no route exists to the destination IP address")]
    Unreachable,
}

/// Like [`get_local_addr`], but willing to forward internally as necessary.
fn get_local_addr_with_internal_forwarding<I, CC>(
    core_ctx: &mut CC,
    local_ip_and_policy: Option<(IpDeviceAddr<I::Addr>, NonLocalSrcAddrPolicy)>,
    device: &CC::DeviceId,
    remote_addr: Option<RoutableIpAddr<I::Addr>>,
) -> Result<(IpDeviceAddr<I::Addr>, InternalForwarding<CC::DeviceId>), ResolveRouteError>
where
    I: IpLayerIpExt,
    CC: IpDeviceContext<I>,
{
    match get_local_addr(core_ctx, local_ip_and_policy, device, remote_addr) {
        Ok(src_addr) => Ok((src_addr, InternalForwarding::NotUsed)),
        Err(e) => {
            // If a local_ip was specified, the local_ip is assigned to a
            // device, and that device has forwarding enabled, use internal
            // forwarding.
            //
            // This enables a weak host model when the Netstack is configured as
            // a router. Conceptually the netstack is forwarding the packet from
            // the local IP's device to the output device of the selected route.
            if let Some((local_ip, _policy)) = local_ip_and_policy {
                if let Some((device, _addr_status)) =
                    get_device_with_assigned_address(core_ctx, local_ip)
                {
                    if core_ctx.is_device_unicast_forwarding_enabled(&device) {
                        return Ok((local_ip, InternalForwarding::Used(device)));
                    }
                }
            }
            Err(e)
        }
    }
}

/// The information about the rule walk in addition to a custom state. This type is introduced so
/// that `walk_rules` can be extended later with more information about the walk if needed.
#[derive(Debug, PartialEq, Eq)]
struct RuleWalkInfo<O> {
    /// Whether there is a rule with a source address matcher during the walk.
    observed_source_address_matcher: bool,
    /// The custom info carried. For example this could be the lookup result from the user provided
    /// function.
    inner: O,
}

/// A helper function that traverses through the rules table.
///
/// To walk through the rules, you need to provide it with an initial value for the loop and a
/// callback function that yieds a [`ControlFlow`] result to indicate whether the traversal should
/// stop.
///
/// # Returns
///
/// - `ControlFlow::Break(RuleAction::Lookup(_))` if we hit a lookup rule and an output is
///   yielded from the route table.
/// - `ControlFlow::Break(RuleAction::Unreachable)` if we hit an unreachable rule.
/// - `ControlFlow::Continue(_)` if we finished walking the rules table without yielding any
///   result.
fn walk_rules<
    I: IpLayerIpExt,
    CC: IpRouteTablesContext<I, DeviceId: DeviceWithName>,
    O,
    State,
    F: FnMut(
        State,
        &mut CC::IpDeviceIdCtx<'_>,
        &RoutingTable<I, CC::DeviceId>,
    ) -> ControlFlow<O, State>,
>(
    core_ctx: &mut CC,
    rules: &RulesTable<I, CC::DeviceId>,
    init: State,
    rule_input: &RuleInput<'_, I, CC::DeviceId>,
    mut lookup_table: F,
) -> ControlFlow<RuleAction<RuleWalkInfo<O>>, RuleWalkInfo<State>> {
    rules.iter().try_fold(
        RuleWalkInfo { inner: init, observed_source_address_matcher: false },
        |RuleWalkInfo { inner: state, observed_source_address_matcher },
         Rule { action, matcher }| {
            let observed_source_address_matcher =
                observed_source_address_matcher || matcher.source_address_matcher.is_some();
            if !matcher.matches(rule_input) {
                return ControlFlow::Continue(RuleWalkInfo {
                    inner: state,
                    observed_source_address_matcher,
                });
            }
            match action {
                RuleAction::Unreachable => return ControlFlow::Break(RuleAction::Unreachable),
                RuleAction::Lookup(table_id) => core_ctx.with_ip_routing_table(
                    &table_id,
                    |core_ctx, table| match lookup_table(state, core_ctx, table) {
                        ControlFlow::Break(out) => {
                            ControlFlow::Break(RuleAction::Lookup(RuleWalkInfo {
                                inner: out,
                                observed_source_address_matcher,
                            }))
                        }
                        ControlFlow::Continue(state) => ControlFlow::Continue(RuleWalkInfo {
                            inner: state,
                            observed_source_address_matcher,
                        }),
                    },
                ),
            }
        },
    )
}

/// Returns the outgoing routing instructions for reaching the given destination.
///
/// If a `device` is specified, the resolved route is limited to those that
/// egress over the device.
///
/// If `src_ip` is specified the resolved route is limited to those that egress
/// over a device with the address assigned.
///
/// This function should only be used for calculating a route for an outgoing packet
/// that is generated by us.
pub fn resolve_output_route_to_destination<
    I: Ip + IpDeviceStateIpExt + IpDeviceIpExt + IpLayerIpExt,
    BC: IpDeviceBindingsContext<I, CC::DeviceId> + IpLayerBindingsContext<I, CC::DeviceId>,
    CC: IpStateContext<I> + IpDeviceContext<I> + device::IpDeviceConfigurationContext<I, BC>,
>(
    core_ctx: &mut CC,
    device: Option<&CC::DeviceId>,
    src_ip_and_policy: Option<(IpDeviceAddr<I::Addr>, NonLocalSrcAddrPolicy)>,
    dst_ip: Option<RoutableIpAddr<I::Addr>>,
    marks: &Marks,
) -> Result<ResolvedRoute<I, CC::DeviceId>, ResolveRouteError> {
    enum LocalDelivery<A, D> {
        WeakLoopback { dst_ip: A, device: D },
        StrongForDevice(D),
    }

    // Check if locally destined. If the destination is an address assigned
    // on an interface, and an egress interface wasn't specifically
    // selected, route via the loopback device. This lets us operate as a
    // strong host when an outgoing interface is explicitly requested while
    // still enabling local delivery via the loopback interface, which is
    // acting as a weak host. Note that if the loopback interface is
    // requested as an outgoing interface, route selection is still
    // performed as a strong host! This makes the loopback interface behave
    // more like the other interfaces on the system.
    //
    // TODO(https://fxbug.dev/42175703): Encode the delivery of locally-
    // destined packets to loopback in the route table.
    //
    // TODO(https://fxbug.dev/322539434): Linux is more permissive about
    // allowing cross-device local delivery even when SO_BINDTODEVICE or
    // link-local addresses are involved, and this behavior may need to be
    // emulated.
    let local_delivery_instructions: Option<LocalDelivery<IpDeviceAddr<I::Addr>, CC::DeviceId>> = {
        let dst_ip = dst_ip.and_then(IpDeviceAddr::new_from_socket_ip_addr);
        match (device, dst_ip) {
            (Some(device), Some(dst_ip)) => is_local_assigned_address(core_ctx, device, dst_ip)
                .then_some(LocalDelivery::StrongForDevice(device.clone())),
            (None, Some(dst_ip)) => {
                get_device_with_assigned_address(core_ctx, dst_ip).map(
                    |(dst_device, _addr_status)| {
                        // If either the source or destination addresses needs
                        // a zone ID, then use strong host to enforce that the
                        // source and destination addresses are assigned to the
                        // same interface.
                        if src_ip_and_policy
                            .is_some_and(|(ip, _policy)| ip.as_ref().must_have_zone())
                            || dst_ip.as_ref().must_have_zone()
                        {
                            LocalDelivery::StrongForDevice(dst_device)
                        } else {
                            LocalDelivery::WeakLoopback { dst_ip, device: dst_device }
                        }
                    },
                )
            }
            (_, None) => None,
        }
    };

    if let Some(local_delivery) = local_delivery_instructions {
        let loopback = core_ctx.loopback_id().ok_or(ResolveRouteError::Unreachable)?;

        let (src_addr, dest_device) = match local_delivery {
            LocalDelivery::WeakLoopback { dst_ip, device } => {
                let src_ip = match src_ip_and_policy {
                    Some((src_ip, NonLocalSrcAddrPolicy::Deny)) => {
                        let _device = get_device_with_assigned_address(core_ctx, src_ip)
                            .ok_or(ResolveRouteError::NoSrcAddr)?;
                        src_ip
                    }
                    Some((src_ip, NonLocalSrcAddrPolicy::Allow)) => src_ip,
                    None => dst_ip,
                };
                (src_ip, device)
            }
            LocalDelivery::StrongForDevice(device) => {
                (get_local_addr(core_ctx, src_ip_and_policy, &device, dst_ip)?, device)
            }
        };
        return Ok(ResolvedRoute {
            src_addr,
            local_delivery_device: Some(dest_device),
            device: loopback,
            next_hop: NextHop::RemoteAsNeighbor,
            internal_forwarding: InternalForwarding::NotUsed,
        });
    }
    let bound_address = src_ip_and_policy.map(|(sock_addr, _policy)| sock_addr.into_inner().get());
    let rule_input = RuleInput {
        packet_origin: PacketOrigin::Local { bound_address, bound_device: device },
        marks,
    };
    core_ctx.with_rules_table(|core_ctx, rules| {
        let mut walk_rules = |rule_input, src_ip_and_policy| {
            walk_rules(
                core_ctx,
                rules,
                None, /* first error encountered */
                rule_input,
                |first_error, core_ctx, table| {
                    let mut matching_with_addr = table.lookup_filter_map(
                        core_ctx,
                        device,
                        dst_ip.map_or(I::UNSPECIFIED_ADDRESS, |a| a.addr()),
                        |core_ctx, d| {
                            Some(get_local_addr_with_internal_forwarding(
                                core_ctx,
                                src_ip_and_policy,
                                d,
                                dst_ip,
                            ))
                        },
                    );

                    let first_error_in_this_table = match matching_with_addr.next() {
                        Some((
                            Destination { device, next_hop },
                            Ok((local_addr, internal_forwarding)),
                        )) => {
                            return ControlFlow::Break(Ok((
                                Destination { device: device.clone(), next_hop },
                                local_addr,
                                internal_forwarding,
                            )));
                        }
                        Some((_, Err(e))) => e,
                        // Note: rule evaluation will continue on to the next rule, if the
                        // previous rule was `Lookup` but the table didn't have the route
                        // inside of it.
                        None => return ControlFlow::Continue(first_error),
                    };

                    matching_with_addr
                        .filter_map(|(destination, local_addr)| {
                            // Select successful routes. We ignore later errors
                            // since we've already saved the first one.
                            local_addr.ok_checked::<ResolveRouteError>().map(
                                |(local_addr, internal_forwarding)| {
                                    (destination, local_addr, internal_forwarding)
                                },
                            )
                        })
                        .next()
                        .map_or(
                            ControlFlow::Continue(first_error.or(Some(first_error_in_this_table))),
                            |(
                                Destination { device, next_hop },
                                local_addr,
                                internal_forwarding,
                            )| {
                                ControlFlow::Break(Ok((
                                    Destination { device: device.clone(), next_hop },
                                    local_addr,
                                    internal_forwarding,
                                )))
                            },
                        )
                },
            )
        };

        let result = match walk_rules(&rule_input, src_ip_and_policy) {
            // Only try to resolve a route again if all of the following are true:
            // 1. The source address is not provided by the caller.
            // 2. A route is successfully resolved so we selected a source address.
            // 3. There is a rule with a source address matcher during the resolution.
            // The rationale is to make sure the route resolution converges to a sensible route
            // after considering the source address we select.
            ControlFlow::Break(RuleAction::Lookup(RuleWalkInfo {
                inner: Ok((_dst, selected_src_addr, _internal_forwarding)),
                observed_source_address_matcher: true,
            })) if src_ip_and_policy.is_none() => walk_rules(
                &RuleInput {
                    packet_origin: PacketOrigin::Local {
                        bound_address: Some(selected_src_addr.into()),
                        bound_device: device,
                    },
                    marks,
                },
                Some((selected_src_addr, NonLocalSrcAddrPolicy::Deny)),
            ),
            result => result,
        };

        match result {
            ControlFlow::Break(RuleAction::Lookup(RuleWalkInfo {
                inner: result,
                observed_source_address_matcher: _,
            })) => {
                result.map(|(Destination { device, next_hop }, src_addr, internal_forwarding)| {
                    ResolvedRoute {
                        src_addr,
                        device,
                        local_delivery_device: None,
                        next_hop,
                        internal_forwarding,
                    }
                })
            }
            ControlFlow::Break(RuleAction::Unreachable) => Err(ResolveRouteError::Unreachable),
            ControlFlow::Continue(RuleWalkInfo {
                inner: first_error,
                observed_source_address_matcher: _,
            }) => Err(first_error.unwrap_or(ResolveRouteError::Unreachable)),
        }
    })
}

/// Enables a blanket implementation of [`IpSocketContext`].
///
/// Implementing this marker trait for a type enables a blanket implementation
/// of `IpSocketContext` given the other requirements are met.
pub trait UseIpSocketContextBlanket {}

impl<
        I: Ip + IpDeviceStateIpExt + IpDeviceIpExt + IpLayerIpExt,
        BC: IpDeviceBindingsContext<I, CC::DeviceId>
            + IpLayerBindingsContext<I, CC::DeviceId>
            + IpSocketBindingsContext,
        CC: IpLayerEgressContext<I, BC>
            + IpStateContext<I>
            + IpDeviceContext<I>
            + IpDeviceConfirmReachableContext<I, BC>
            + IpDeviceMtuContext<I>
            + device::IpDeviceConfigurationContext<I, BC>
            + UseIpSocketContextBlanket,
    > IpSocketContext<I, BC> for CC
{
    fn lookup_route(
        &mut self,
        _bindings_ctx: &mut BC,
        device: Option<&CC::DeviceId>,
        local_ip: Option<IpDeviceAddr<I::Addr>>,
        addr: RoutableIpAddr<I::Addr>,
        transparent: bool,
        marks: &Marks,
    ) -> Result<ResolvedRoute<I, CC::DeviceId>, ResolveRouteError> {
        let src_ip_and_policy = local_ip.map(|local_ip| {
            (
                local_ip,
                if transparent {
                    NonLocalSrcAddrPolicy::Allow
                } else {
                    NonLocalSrcAddrPolicy::Deny
                },
            )
        });
        let res =
            resolve_output_route_to_destination(self, device, src_ip_and_policy, Some(addr), marks);
        trace!(
            "lookup_route(\
                device={device:?}, \
                local_ip={local_ip:?}, \
                addr={addr:?}, \
                transparent={transparent:?}, \
                marks={marks:?}) => {res:?}"
        );
        res
    }

    fn send_ip_packet<S>(
        &mut self,
        bindings_ctx: &mut BC,
        meta: SendIpPacketMeta<
            I,
            &<CC as DeviceIdContext<AnyDevice>>::DeviceId,
            SpecifiedAddr<I::Addr>,
        >,
        body: S,
        packet_metadata: IpLayerPacketMetadata<I, CC::WeakAddressId, BC>,
    ) -> Result<(), IpSendFrameError<S>>
    where
        S: TransportPacketSerializer<I>,
        S::Buffer: BufferMut,
    {
        send_ip_packet_from_device(self, bindings_ctx, meta.into(), body, packet_metadata)
    }

    fn get_loopback_device(&mut self) -> Option<Self::DeviceId> {
        device::IpDeviceConfigurationContext::<I, _>::loopback_id(self)
    }

    fn confirm_reachable(
        &mut self,
        bindings_ctx: &mut BC,
        dst: SpecifiedAddr<I::Addr>,
        input: RuleInput<'_, I, Self::DeviceId>,
    ) {
        match lookup_route_table(self, dst.get(), input) {
            Some(Destination { next_hop, device }) => {
                let neighbor = match next_hop {
                    NextHop::RemoteAsNeighbor => dst,
                    NextHop::Gateway(gateway) => gateway,
                    NextHop::Broadcast(marker) => {
                        I::map_ip::<_, ()>(
                            WrapBroadcastMarker(marker),
                            |WrapBroadcastMarker(())| {
                                debug!(
                                    "can't confirm {dst:?}@{device:?} as reachable: \
                                    dst is a broadcast address"
                                );
                            },
                            |WrapBroadcastMarker(never)| match never {},
                        );
                        return;
                    }
                };
                IpDeviceConfirmReachableContext::confirm_reachable(
                    self,
                    bindings_ctx,
                    &device,
                    neighbor,
                );
            }
            None => {
                debug!("can't confirm {dst:?} as reachable: no route");
            }
        }
    }
}

/// The IP context providing dispatch to the available transport protocols.
///
/// This trait acts like a demux on the transport protocol for ingress IP
/// packets.
pub trait IpTransportDispatchContext<I: IpLayerIpExt, BC>: DeviceIdContext<AnyDevice> {
    /// Dispatches a received incoming IP packet to the appropriate protocol.
    fn dispatch_receive_ip_packet<B: BufferMut, H: IpHeaderInfo<I>>(
        &mut self,
        bindings_ctx: &mut BC,
        device: &Self::DeviceId,
        src_ip: I::RecvSrcAddr,
        dst_ip: SpecifiedAddr<I::Addr>,
        proto: I::Proto,
        body: B,
        info: &LocalDeliveryPacketInfo<I, H>,
    ) -> Result<(), TransportReceiveError>;
}

/// A marker trait for all the contexts required for IP ingress.
pub trait IpLayerIngressContext<I: IpLayerIpExt, BC: IpLayerBindingsContext<I, Self::DeviceId>>:
    IpTransportDispatchContext<I, BC, DeviceId: filter::InterfaceProperties<BC::DeviceClass>>
    + IpDeviceIngressStateContext<I>
    + IpDeviceMtuContext<I>
    + IpDeviceSendContext<I, BC>
    + IcmpErrorHandler<I, BC>
    + IpLayerContext<I, BC>
    + FragmentHandler<I, BC>
    + FilterHandlerProvider<I, BC>
    + RawIpSocketHandler<I, BC>
{
}

impl<
        I: IpLayerIpExt,
        BC: IpLayerBindingsContext<I, CC::DeviceId>,
        CC: IpTransportDispatchContext<
                I,
                BC,
                DeviceId: filter::InterfaceProperties<BC::DeviceClass>,
            > + IpDeviceIngressStateContext<I>
            + IpDeviceMtuContext<I>
            + IpDeviceSendContext<I, BC>
            + IcmpErrorHandler<I, BC>
            + IpLayerContext<I, BC>
            + FragmentHandler<I, BC>
            + FilterHandlerProvider<I, BC>
            + RawIpSocketHandler<I, BC>,
    > IpLayerIngressContext<I, BC> for CC
{
}

/// A marker trait for all the contexts required for IP egress.
pub trait IpLayerEgressContext<I, BC>:
    IpDeviceSendContext<I, BC, DeviceId: filter::InterfaceProperties<BC::DeviceClass>>
    + FilterHandlerProvider<I, BC>
    + CounterContext<IpCounters<I>>
where
    I: IpLayerIpExt,
    BC: FilterBindingsContext + TxMetadataBindingsTypes,
{
}

impl<I, BC, CC> IpLayerEgressContext<I, BC> for CC
where
    I: IpLayerIpExt,
    BC: FilterBindingsContext + TxMetadataBindingsTypes,
    CC: IpDeviceSendContext<I, BC, DeviceId: filter::InterfaceProperties<BC::DeviceClass>>
        + FilterHandlerProvider<I, BC>
        + CounterContext<IpCounters<I>>,
{
}

/// A marker trait for all the contexts required for IP forwarding.
pub trait IpLayerForwardingContext<I: IpLayerIpExt, BC: IpLayerBindingsContext<I, Self::DeviceId>>:
    IpLayerEgressContext<I, BC> + IcmpErrorHandler<I, BC> + IpDeviceMtuContext<I>
{
}

impl<
        I: IpLayerIpExt,
        BC: IpLayerBindingsContext<I, CC::DeviceId>,
        CC: IpLayerEgressContext<I, BC> + IcmpErrorHandler<I, BC> + IpDeviceMtuContext<I>,
    > IpLayerForwardingContext<I, BC> for CC
{
}

/// A builder for IPv4 state.
#[derive(Copy, Clone, Default)]
pub struct Ipv4StateBuilder {
    icmp: Icmpv4StateBuilder,
}

impl Ipv4StateBuilder {
    /// Get the builder for the ICMPv4 state.
    #[cfg(any(test, feature = "testutils"))]
    pub fn icmpv4_builder(&mut self) -> &mut Icmpv4StateBuilder {
        &mut self.icmp
    }

    /// Builds the [`Ipv4State`].
    pub fn build<
        CC: CoreTimerContext<IpLayerTimerId, BC>,
        StrongDeviceId: StrongDeviceIdentifier,
        BC: TimerContext + RngContext + IpLayerBindingsTypes,
    >(
        self,
        bindings_ctx: &mut BC,
    ) -> Ipv4State<StrongDeviceId, BC> {
        let Ipv4StateBuilder { icmp } = self;

        Ipv4State {
            inner: IpStateInner::new::<CC>(bindings_ctx),
            icmp: icmp.build(),
            next_packet_id: Default::default(),
        }
    }
}

/// A builder for IPv6 state.
///
/// By default, opaque IIDs will not be used to generate stable SLAAC addresses.
#[derive(Copy, Clone)]
pub struct Ipv6StateBuilder {
    icmp: Icmpv6StateBuilder,
    slaac_stable_secret_key: Option<IidSecret>,
}

impl Ipv6StateBuilder {
    /// Sets the secret key used to generate stable SLAAC addresses.
    ///
    /// If `slaac_stable_secret_key` is left unset, opaque IIDs will not be used to
    /// generate stable SLAAC addresses.
    pub fn slaac_stable_secret_key(&mut self, secret_key: IidSecret) -> &mut Self {
        self.slaac_stable_secret_key = Some(secret_key);
        self
    }

    /// Builds the [`Ipv6State`].
    ///
    /// # Panics
    ///
    /// Panics if the `slaac_stable_secret_key` has not been set.
    pub fn build<
        CC: CoreTimerContext<IpLayerTimerId, BC>,
        StrongDeviceId: StrongDeviceIdentifier,
        BC: TimerContext + RngContext + IpLayerBindingsTypes,
    >(
        self,
        bindings_ctx: &mut BC,
    ) -> Ipv6State<StrongDeviceId, BC> {
        let Ipv6StateBuilder { icmp, slaac_stable_secret_key } = self;

        let slaac_stable_secret_key = slaac_stable_secret_key
            .expect("stable SLAAC secret key was not provided to `Ipv6StateBuilder`");

        Ipv6State {
            inner: IpStateInner::new::<CC>(bindings_ctx),
            icmp: icmp.build(),
            slaac_counters: Default::default(),
            slaac_temp_secret_key: IidSecret::new_random(&mut bindings_ctx.rng()),
            slaac_stable_secret_key,
        }
    }
}

impl Default for Ipv6StateBuilder {
    fn default() -> Self {
        #[cfg(any(test, feature = "testutils"))]
        let slaac_stable_secret_key = Some(IidSecret::ALL_ONES);

        #[cfg(not(any(test, feature = "testutils")))]
        let slaac_stable_secret_key = None;

        Self { icmp: Icmpv6StateBuilder::default(), slaac_stable_secret_key }
    }
}

/// The stack's IPv4 state.
pub struct Ipv4State<StrongDeviceId: StrongDeviceIdentifier, BT: IpLayerBindingsTypes> {
    /// The common inner IP layer state.
    pub inner: IpStateInner<Ipv4, StrongDeviceId, BT>,
    /// The ICMP state.
    pub icmp: Icmpv4State<BT>,
    /// The atomic counter providing IPv4 packet identifiers.
    pub next_packet_id: AtomicU16,
}

impl<StrongDeviceId: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    AsRef<IpStateInner<Ipv4, StrongDeviceId, BT>> for Ipv4State<StrongDeviceId, BT>
{
    fn as_ref(&self) -> &IpStateInner<Ipv4, StrongDeviceId, BT> {
        &self.inner
    }
}

/// Generates an IP packet ID.
///
/// This is only meaningful for IPv4, see [`IpLayerIpExt`].
pub fn gen_ip_packet_id<I: IpLayerIpExt, CC: IpDeviceEgressStateContext<I>>(
    core_ctx: &mut CC,
) -> I::PacketId {
    core_ctx.with_next_packet_id(|state| I::next_packet_id_from_state(state))
}

/// The stack's IPv6 state.
pub struct Ipv6State<StrongDeviceId: StrongDeviceIdentifier, BT: IpLayerBindingsTypes> {
    /// The common inner IP layer state.
    pub inner: IpStateInner<Ipv6, StrongDeviceId, BT>,
    /// ICMPv6 state.
    pub icmp: Icmpv6State<BT>,
    /// Stateless address autoconfiguration counters.
    pub slaac_counters: SlaacCounters,
    /// Secret key used for generating SLAAC temporary addresses.
    pub slaac_temp_secret_key: IidSecret,
    /// Secret key used for generating SLAAC stable addresses.
    ///
    /// If `None`, opaque IIDs will not be used to generate stable SLAAC
    /// addresses.
    pub slaac_stable_secret_key: IidSecret,
}

impl<StrongDeviceId: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    AsRef<IpStateInner<Ipv6, StrongDeviceId, BT>> for Ipv6State<StrongDeviceId, BT>
{
    fn as_ref(&self) -> &IpStateInner<Ipv6, StrongDeviceId, BT> {
        &self.inner
    }
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    OrderedLockAccess<IpPacketFragmentCache<I, BT>> for IpStateInner<I, D, BT>
{
    type Lock = Mutex<IpPacketFragmentCache<I, BT>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        OrderedLockRef::new(&self.fragment_cache)
    }
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    OrderedLockAccess<PmtuCache<I, BT>> for IpStateInner<I, D, BT>
{
    type Lock = Mutex<PmtuCache<I, BT>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        OrderedLockRef::new(&self.pmtu_cache)
    }
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    OrderedLockAccess<RulesTable<I, D>> for IpStateInner<I, D, BT>
{
    type Lock = RwLock<RulesTable<I, D>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        OrderedLockRef::new(&self.rules_table)
    }
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    OrderedLockAccess<HashMap<RoutingTableId<I, D>, PrimaryRc<RwLock<RoutingTable<I, D>>>>>
    for IpStateInner<I, D, BT>
{
    type Lock = Mutex<HashMap<RoutingTableId<I, D>, PrimaryRc<RwLock<RoutingTable<I, D>>>>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        OrderedLockRef::new(&self.tables)
    }
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier> OrderedLockAccess<RoutingTable<I, D>>
    for RoutingTableId<I, D>
{
    type Lock = RwLock<RoutingTable<I, D>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        let Self(inner) = self;
        OrderedLockRef::new(&*inner)
    }
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    OrderedLockAccess<MulticastForwardingState<I, D, BT>> for IpStateInner<I, D, BT>
{
    type Lock = RwLock<MulticastForwardingState<I, D, BT>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        OrderedLockRef::new(&self.multicast_forwarding)
    }
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    OrderedLockAccess<RawIpSocketMap<I, D::Weak, BT>> for IpStateInner<I, D, BT>
{
    type Lock = RwLock<RawIpSocketMap<I, D::Weak, BT>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        OrderedLockRef::new(&self.raw_sockets)
    }
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpLayerBindingsTypes>
    OrderedLockAccess<filter::State<I, I::Weak<BT>, BT>> for IpStateInner<I, D, BT>
{
    type Lock = RwLock<filter::State<I, I::Weak<BT>, BT>>;
    fn ordered_lock_access(&self) -> OrderedLockRef<'_, Self::Lock> {
        OrderedLockRef::new(&self.filter)
    }
}

/// Ip layer counters.
#[derive(Default, GenericOverIp)]
#[generic_over_ip(I, Ip)]
pub struct IpCounters<I: IpLayerIpExt> {
    /// Count of incoming IP unicast packets delivered.
    pub deliver_unicast: Counter,
    /// Count of incoming IP multicast packets delivered.
    pub deliver_multicast: Counter,
    /// Count of incoming IP packets that are dispatched to the appropriate protocol.
    pub dispatch_receive_ip_packet: Counter,
    /// Count of incoming IP packets destined to another host.
    pub dispatch_receive_ip_packet_other_host: Counter,
    /// Count of incoming IP packets received by the stack.
    pub receive_ip_packet: Counter,
    /// Count of sent outgoing IP packets.
    pub send_ip_packet: Counter,
    /// Count of packets to be forwarded which are instead dropped because
    /// forwarding is disabled.
    pub forwarding_disabled: Counter,
    /// Count of incoming packets forwarded to another host.
    pub forward: Counter,
    /// Count of incoming packets which cannot be forwarded because there is no
    /// route to the destination host.
    pub no_route_to_host: Counter,
    /// Count of incoming packets which cannot be forwarded because the MTU has
    /// been exceeded.
    pub mtu_exceeded: Counter,
    /// Count of incoming packets which cannot be forwarded because the TTL has
    /// expired.
    pub ttl_expired: Counter,
    /// Count of ICMP error messages received.
    pub receive_icmp_error: Counter,
    /// Count of IP fragment reassembly errors.
    pub fragment_reassembly_error: Counter,
    /// Count of IP fragments that could not be reassembled because more
    /// fragments were needed.
    pub need_more_fragments: Counter,
    /// Count of IP fragments that could not be reassembled because the fragment
    /// was invalid.
    pub invalid_fragment: Counter,
    /// Count of IP fragments that could not be reassembled because the stack's
    /// per-IP-protocol fragment cache was full.
    pub fragment_cache_full: Counter,
    /// Count of incoming IP packets not delivered because of a parameter problem.
    pub parameter_problem: Counter,
    /// Count of incoming IP packets with an unspecified destination address.
    pub unspecified_destination: Counter,
    /// Count of incoming IP packets with an unspecified source address.
    pub unspecified_source: Counter,
    /// Count of incoming IP packets dropped.
    pub dropped: Counter,
    /// Number of frames rejected because they'd cause illegal loopback
    /// addresses on the wire.
    pub tx_illegal_loopback_address: Counter,
    /// Version specific rx counters.
    pub version_rx: I::RxCounters,
    /// Count of incoming IP multicast packets that were dropped because
    /// The stack doesn't have any sockets that belong to the multicast group,
    /// and the stack isn't configured to forward the multicast packet.
    pub multicast_no_interest: Counter,
    /// Count of looped-back packets that held a cached conntrack entry that could
    /// not be downcasted to the expected type. This would happen if, for example, a
    /// packet was modified to a different IP version between EGRESS and INGRESS.
    pub invalid_cached_conntrack_entry: Counter,
    /// IP fragmentation counters.
    pub fragmentation: FragmentationCounters,
}

/// IPv4-specific Rx counters.
#[derive(Default)]
pub struct Ipv4RxCounters {
    /// Count of incoming broadcast IPv4 packets delivered.
    pub deliver_broadcast: Counter,
}

impl Inspectable for Ipv4RxCounters {
    fn record<I: Inspector>(&self, inspector: &mut I) {
        let Self { deliver_broadcast } = self;
        inspector.record_counter("DeliveredBroadcast", deliver_broadcast);
    }
}

/// IPv6-specific Rx counters.
#[derive(Default)]
pub struct Ipv6RxCounters {
    /// Count of incoming IPv6 packets dropped because the destination address
    /// is only tentatively assigned to the device.
    pub drop_for_tentative: Counter,
    /// Count of incoming IPv6 packets dropped due to a non-unicast source address.
    pub non_unicast_source: Counter,
    /// Count of incoming IPv6 packets discarded while processing extension
    /// headers.
    pub extension_header_discard: Counter,
    /// Count of incoming neighbor solicitations discarded as looped-back
    /// DAD probes.
    pub drop_looped_back_dad_probe: Counter,
}

impl Inspectable for Ipv6RxCounters {
    fn record<I: Inspector>(&self, inspector: &mut I) {
        let Self {
            drop_for_tentative,
            non_unicast_source,
            extension_header_discard,
            drop_looped_back_dad_probe,
        } = self;
        inspector.record_counter("DroppedTentativeDst", drop_for_tentative);
        inspector.record_counter("DroppedNonUnicastSrc", non_unicast_source);
        inspector.record_counter("DroppedExtensionHeader", extension_header_discard);
        inspector.record_counter("DroppedLoopedBackDadProbe", drop_looped_back_dad_probe);
    }
}

/// Marker trait for the bindings types required by the IP layer's inner state.
pub trait IpStateBindingsTypes:
    PmtuBindingsTypes
    + FragmentBindingsTypes
    + RawIpSocketsBindingsTypes
    + FilterBindingsTypes
    + MulticastForwardingBindingsTypes
    + IpDeviceStateBindingsTypes
{
}
impl<BT> IpStateBindingsTypes for BT where
    BT: PmtuBindingsTypes
        + FragmentBindingsTypes
        + RawIpSocketsBindingsTypes
        + FilterBindingsTypes
        + MulticastForwardingBindingsTypes
        + IpDeviceStateBindingsTypes
{
}

/// Identifier to a routing table.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RoutingTableId<I: Ip, D>(StrongRc<RwLock<RoutingTable<I, D>>>);

impl<I: Ip, D> Debug for RoutingTableId<I, D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self(rc) = self;
        f.debug_tuple("RoutingTableId").field(&StrongRc::debug_id(rc)).finish()
    }
}

impl<I: Ip, D> RoutingTableId<I, D> {
    /// Creates a new table ID.
    pub(crate) fn new(rc: StrongRc<RwLock<RoutingTable<I, D>>>) -> Self {
        Self(rc)
    }

    /// Provides direct access to the forwarding table.
    #[cfg(any(test, feature = "testutils"))]
    pub fn table(&self) -> &RwLock<RoutingTable<I, D>> {
        let Self(inner) = self;
        &*inner
    }

    /// Downgrades the strong ID into a weak one.
    pub fn downgrade(&self) -> WeakRoutingTableId<I, D> {
        let Self(rc) = self;
        WeakRoutingTableId(StrongRc::downgrade(rc))
    }

    #[cfg(test)]
    fn get_mut(&self) -> impl DerefMut<Target = RoutingTable<I, D>> + '_ {
        let Self(rc) = self;
        rc.write()
    }
}

/// Weak Identifier to a routing table.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct WeakRoutingTableId<I: Ip, D>(WeakRc<RwLock<RoutingTable<I, D>>>);

impl<I: Ip, D> Debug for WeakRoutingTableId<I, D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self(rc) = self;
        f.debug_tuple("WeakRoutingTableId").field(&WeakRc::debug_id(rc)).finish()
    }
}

/// The inner state for the IP layer for IP version `I`.
#[derive(GenericOverIp)]
#[generic_over_ip(I, Ip)]
pub struct IpStateInner<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpStateBindingsTypes> {
    rules_table: RwLock<RulesTable<I, D>>,
    // TODO(https://fxbug.dev/355059838): Explore the option to let Bindings create the main table.
    main_table_id: RoutingTableId<I, D>,
    multicast_forwarding: RwLock<MulticastForwardingState<I, D, BT>>,
    multicast_forwarding_counters: MulticastForwardingCounters<I>,
    fragment_cache: Mutex<IpPacketFragmentCache<I, BT>>,
    pmtu_cache: Mutex<PmtuCache<I, BT>>,
    counters: IpCounters<I>,
    raw_sockets: RwLock<RawIpSocketMap<I, D::Weak, BT>>,
    raw_socket_counters: RawIpSocketCounters<I>,
    filter: RwLock<filter::State<I, I::Weak<BT>, BT>>,
    // Make sure the primary IDs are dropped last. Also note that the following hash map also stores
    // the primary ID to the main table, and if the user (Bindings) attempts to remove the main
    // table without dropping `main_table_id` first, it will panic. This serves as an assertion
    // that the main table cannot be removed and Bindings must never attempt to remove the main
    // routing table.
    tables: Mutex<HashMap<RoutingTableId<I, D>, PrimaryRc<RwLock<RoutingTable<I, D>>>>>,
    igmp_counters: IgmpCounters,
    mld_counters: MldCounters,
}

impl<I: IpLayerIpExt, D: StrongDeviceIdentifier, BT: IpStateBindingsTypes> IpStateInner<I, D, BT> {
    /// Gets the IP counters.
    pub fn counters(&self) -> &IpCounters<I> {
        &self.counters
    }

    /// Gets the multicast forwarding counters.
    pub fn multicast_forwarding_counters(&self) -> &MulticastForwardingCounters<I> {
        &self.multicast_forwarding_counters
    }

    /// Gets the aggregate raw IP socket counters.
    pub fn raw_ip_socket_counters(&self) -> &RawIpSocketCounters<I> {
        &self.raw_socket_counters
    }

    /// Gets the main table ID.
    pub fn main_table_id(&self) -> &RoutingTableId<I, D> {
        &self.main_table_id
    }

    /// Provides direct access to the path MTU cache.
    #[cfg(any(test, feature = "testutils"))]
    pub fn pmtu_cache(&self) -> &Mutex<PmtuCache<I, BT>> {
        &self.pmtu_cache
    }

    /// Provides direct access to the filtering state.
    #[cfg(any(test, feature = "testutils"))]
    pub fn filter(&self) -> &RwLock<filter::State<I, I::Weak<BT>, BT>> {
        &self.filter
    }

    /// Gets the stack-wide IGMP counters.
    pub fn igmp_counters(&self) -> &IgmpCounters {
        &self.igmp_counters
    }

    /// Gets the stack-wide MLD counters.
    pub fn mld_counters(&self) -> &MldCounters {
        &self.mld_counters
    }
}

impl<
        I: IpLayerIpExt,
        D: StrongDeviceIdentifier,
        BC: TimerContext + RngContext + IpStateBindingsTypes,
    > IpStateInner<I, D, BC>
{
    /// Creates a new inner IP layer state.
    fn new<CC: CoreTimerContext<IpLayerTimerId, BC>>(bindings_ctx: &mut BC) -> Self {
        let main_table: PrimaryRc<RwLock<RoutingTable<I, D>>> = PrimaryRc::new(Default::default());
        let main_table_id = RoutingTableId(PrimaryRc::clone_strong(&main_table));
        Self {
            rules_table: RwLock::new(RulesTable::new(main_table_id.clone())),
            tables: Mutex::new(HashMap::from_iter(core::iter::once((
                main_table_id.clone(),
                main_table,
            )))),
            main_table_id,
            multicast_forwarding: Default::default(),
            multicast_forwarding_counters: Default::default(),
            fragment_cache: Mutex::new(
                IpPacketFragmentCache::new::<NestedIntoCoreTimerCtx<CC, _>>(bindings_ctx),
            ),
            pmtu_cache: Mutex::new(PmtuCache::new::<NestedIntoCoreTimerCtx<CC, _>>(bindings_ctx)),
            counters: Default::default(),
            raw_sockets: Default::default(),
            raw_socket_counters: Default::default(),
            filter: RwLock::new(filter::State::new::<NestedIntoCoreTimerCtx<CC, _>>(bindings_ctx)),
            igmp_counters: Default::default(),
            mld_counters: Default::default(),
        }
    }
}

/// The identifier for timer events in the IP layer.
#[derive(Debug, Clone, Eq, PartialEq, Hash, GenericOverIp)]
#[generic_over_ip()]
pub enum IpLayerTimerId {
    /// A timer event for IPv4 packet reassembly timers.
    ReassemblyTimeoutv4(FragmentTimerId<Ipv4>),
    /// A timer event for IPv6 packet reassembly timers.
    ReassemblyTimeoutv6(FragmentTimerId<Ipv6>),
    /// A timer event for IPv4 path MTU discovery.
    PmtuTimeoutv4(PmtuTimerId<Ipv4>),
    /// A timer event for IPv6 path MTU discovery.
    PmtuTimeoutv6(PmtuTimerId<Ipv6>),
    /// A timer event for IPv4 filtering timers.
    FilterTimerv4(FilterTimerId<Ipv4>),
    /// A timer event for IPv6 filtering timers.
    FilterTimerv6(FilterTimerId<Ipv6>),
    /// A timer event for IPv4 Multicast forwarding timers.
    MulticastForwardingTimerv4(MulticastForwardingTimerId<Ipv4>),
    /// A timer event for IPv6 Multicast forwarding timers.
    MulticastForwardingTimerv6(MulticastForwardingTimerId<Ipv6>),
}

impl<I: Ip> From<FragmentTimerId<I>> for IpLayerTimerId {
    fn from(timer: FragmentTimerId<I>) -> IpLayerTimerId {
        I::map_ip(timer, IpLayerTimerId::ReassemblyTimeoutv4, IpLayerTimerId::ReassemblyTimeoutv6)
    }
}

impl<I: Ip> From<PmtuTimerId<I>> for IpLayerTimerId {
    fn from(timer: PmtuTimerId<I>) -> IpLayerTimerId {
        I::map_ip(timer, IpLayerTimerId::PmtuTimeoutv4, IpLayerTimerId::PmtuTimeoutv6)
    }
}

impl<I: Ip> From<FilterTimerId<I>> for IpLayerTimerId {
    fn from(timer: FilterTimerId<I>) -> IpLayerTimerId {
        I::map_ip(timer, IpLayerTimerId::FilterTimerv4, IpLayerTimerId::FilterTimerv6)
    }
}

impl<I: Ip> From<MulticastForwardingTimerId<I>> for IpLayerTimerId {
    fn from(timer: MulticastForwardingTimerId<I>) -> IpLayerTimerId {
        I::map_ip(
            timer,
            IpLayerTimerId::MulticastForwardingTimerv4,
            IpLayerTimerId::MulticastForwardingTimerv6,
        )
    }
}

impl<CC, BC> HandleableTimer<CC, BC> for IpLayerTimerId
where
    CC: TimerHandler<BC, FragmentTimerId<Ipv4>>
        + TimerHandler<BC, FragmentTimerId<Ipv6>>
        + TimerHandler<BC, PmtuTimerId<Ipv4>>
        + TimerHandler<BC, PmtuTimerId<Ipv6>>
        + TimerHandler<BC, FilterTimerId<Ipv4>>
        + TimerHandler<BC, FilterTimerId<Ipv6>>
        + TimerHandler<BC, MulticastForwardingTimerId<Ipv4>>
        + TimerHandler<BC, MulticastForwardingTimerId<Ipv6>>,
    BC: TimerBindingsTypes,
{
    fn handle(self, core_ctx: &mut CC, bindings_ctx: &mut BC, timer: BC::UniqueTimerId) {
        match self {
            IpLayerTimerId::ReassemblyTimeoutv4(id) => {
                core_ctx.handle_timer(bindings_ctx, id, timer)
            }
            IpLayerTimerId::ReassemblyTimeoutv6(id) => {
                core_ctx.handle_timer(bindings_ctx, id, timer)
            }
            IpLayerTimerId::PmtuTimeoutv4(id) => core_ctx.handle_timer(bindings_ctx, id, timer),
            IpLayerTimerId::PmtuTimeoutv6(id) => core_ctx.handle_timer(bindings_ctx, id, timer),
            IpLayerTimerId::FilterTimerv4(id) => core_ctx.handle_timer(bindings_ctx, id, timer),
            IpLayerTimerId::FilterTimerv6(id) => core_ctx.handle_timer(bindings_ctx, id, timer),
            IpLayerTimerId::MulticastForwardingTimerv4(id) => {
                core_ctx.handle_timer(bindings_ctx, id, timer)
            }
            IpLayerTimerId::MulticastForwardingTimerv6(id) => {
                core_ctx.handle_timer(bindings_ctx, id, timer)
            }
        }
    }
}

/// An ICMP error, and the metadata required to send it.
///
/// This allows the sending of the ICMP error to be decoupled from the
/// generation of the error, which is advantageous because sending the error
/// requires the underlying packet buffer, which cannot be "moved" in certain
/// contexts.
pub(crate) struct IcmpErrorSender<'a, I: IcmpHandlerIpExt, D> {
    /// The ICMP error that should be sent.
    err: I::IcmpError,
    /// The original source IP address of the packet (before the local-ingress
    /// hook evaluation).
    src_ip: I::SourceAddress,
    /// The original destination IP address of the packet (before the
    /// local-ingress hook evaluation).
    dst_ip: SpecifiedAddr<I::Addr>,
    /// The frame destination of the packet.
    frame_dst: Option<FrameDestination>,
    /// The device out which to send the error.
    device: &'a D,
    /// The metadata from the packet, allowing the packet's backing buffer to be
    /// returned to it's pre-IP-parse state with [`GrowBuffer::undo_parse`].
    meta: ParseMetadata,
    /// The marks used to send the ICMP error.
    marks: Marks,
}

impl<'a, I: IcmpHandlerIpExt, D> IcmpErrorSender<'a, I, D> {
    /// Generate an send an appropriate ICMP error in response to this error.
    ///
    /// The provided `body` must be the original buffer from which the IP
    /// packet responsible for this error was parsed. It is expected to be in a
    /// state that allows undoing the IP packet parse (e.g. unmodified after the
    /// IP packet was parsed).
    fn respond_with_icmp_error<B, BC, CC>(
        self,
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        mut body: B,
    ) where
        B: BufferMut,
        CC: IcmpErrorHandler<I, BC, DeviceId = D>,
    {
        let IcmpErrorSender { err, src_ip, dst_ip, frame_dst, device, meta, marks } = self;
        // Undo the parsing of the IP Packet, moving the buffer's cursor so that
        // it points at the start of the IP header. This way, the sent ICMP
        // error will contain the entire original IP packet.
        body.undo_parse(meta);

        core_ctx.send_icmp_error_message(
            bindings_ctx,
            device,
            frame_dst,
            src_ip,
            dst_ip,
            body,
            err,
            &marks,
        );
    }
}

// TODO(joshlf): Once we support multiple extension headers in IPv6, we will
// need to verify that the callers of this function are still sound. In
// particular, they may accidentally pass a parse_metadata argument which
// corresponds to a single extension header rather than all of the IPv6 headers.

/// Dispatch a received IPv4 packet to the appropriate protocol.
///
/// `device` is the device the packet was received on. `parse_metadata` is the
/// parse metadata associated with parsing the IP headers. It is used to undo
/// that parsing. Both `device` and `parse_metadata` are required in order to
/// send ICMP messages in response to unrecognized protocols or ports. If either
/// of `device` or `parse_metadata` is `None`, the caller promises that the
/// protocol and port are recognized.
///
/// # Panics
///
/// `dispatch_receive_ipv4_packet` panics if the protocol is unrecognized and
/// `parse_metadata` is `None`. If an IGMP message is received but it is not
/// coming from a device, i.e., `device` given is `None`,
/// `dispatch_receive_ip_packet` will also panic.
fn dispatch_receive_ipv4_packet<
    'a,
    'b,
    BC: IpLayerBindingsContext<Ipv4, CC::DeviceId>,
    CC: IpLayerIngressContext<Ipv4, BC> + CounterContext<IpCounters<Ipv4>>,
>(
    core_ctx: &'a mut CC,
    bindings_ctx: &'a mut BC,
    device: &'b CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    mut packet: Ipv4Packet<&'a mut [u8]>,
    mut packet_metadata: IpLayerPacketMetadata<Ipv4, CC::WeakAddressId, BC>,
    receive_meta: ReceiveIpPacketMeta<Ipv4>,
) -> Result<(), IcmpErrorSender<'b, Ipv4, CC::DeviceId>> {
    CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).dispatch_receive_ip_packet.increment();

    match frame_dst {
        Some(FrameDestination::Individual { local: false }) => {
            CounterContext::<IpCounters<Ipv4>>::counters(core_ctx)
                .dispatch_receive_ip_packet_other_host
                .increment();
        }
        Some(FrameDestination::Individual { local: true })
        | Some(FrameDestination::Multicast)
        | Some(FrameDestination::Broadcast)
        | None => (),
    }

    let proto = packet.proto();

    match core_ctx.filter_handler().local_ingress_hook(
        bindings_ctx,
        &mut packet,
        device,
        &mut packet_metadata,
    ) {
        filter::Verdict::Drop => {
            packet_metadata.acknowledge_drop();
            return Ok(());
        }
        filter::Verdict::Accept(()) => {}
    }
    let marks = packet_metadata.marks;
    packet_metadata.acknowledge_drop();

    let src_ip = packet.src_ip();
    // `dst_ip` is validated to be specified before a packet is provided to this
    // function, but it's possible for the LOCAL_INGRESS hook to rewrite the packet,
    // so we have to re-verify this.
    let Some(dst_ip) = SpecifiedAddr::new(packet.dst_ip()) else {
        CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).unspecified_destination.increment();
        debug!(
            "dispatch_receive_ipv4_packet: Received packet with unspecified destination IP address \
            after the LOCAL_INGRESS hook; dropping"
        );
        return Ok(());
    };

    core_ctx.deliver_packet_to_raw_ip_sockets(bindings_ctx, &packet, &device);

    let (prefix, options, body) = packet.parts_with_body_mut();
    let buffer = Buf::new(body, ..);
    let header_info = Ipv4HeaderInfo { prefix, options: options.as_ref() };
    let receive_info = LocalDeliveryPacketInfo { meta: receive_meta, header_info, marks };

    core_ctx
        .dispatch_receive_ip_packet(
            bindings_ctx,
            device,
            src_ip,
            dst_ip,
            proto,
            buffer,
            &receive_info,
        )
        .or_else(|err| {
            if let Some(src_ip) = SpecifiedAddr::new(src_ip) {
                let (_, _, _, meta) = packet.into_metadata();
                Err(IcmpErrorSender {
                    err: err.into_icmpv4_error(meta.header_len()),
                    src_ip,
                    dst_ip,
                    frame_dst,
                    device,
                    meta,
                    marks,
                })
            } else {
                Ok(())
            }
        })
}

/// Dispatch a received IPv6 packet to the appropriate protocol.
///
/// `dispatch_receive_ipv6_packet` has the same semantics as
/// `dispatch_receive_ipv4_packet`, but for IPv6.
fn dispatch_receive_ipv6_packet<
    'a,
    'b,
    BC: IpLayerBindingsContext<Ipv6, CC::DeviceId>,
    CC: IpLayerIngressContext<Ipv6, BC> + CounterContext<IpCounters<Ipv6>>,
>(
    core_ctx: &'a mut CC,
    bindings_ctx: &'a mut BC,
    device: &'b CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    mut packet: Ipv6Packet<&'a mut [u8]>,
    mut packet_metadata: IpLayerPacketMetadata<Ipv6, CC::WeakAddressId, BC>,
    meta: ReceiveIpPacketMeta<Ipv6>,
) -> Result<(), IcmpErrorSender<'b, Ipv6, CC::DeviceId>> {
    // TODO(https://fxbug.dev/42095067): Once we support multiple extension
    // headers in IPv6, we will need to verify that the callers of this
    // function are still sound. In particular, they may accidentally pass a
    // parse_metadata argument which corresponds to a single extension
    // header rather than all of the IPv6 headers.

    CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).dispatch_receive_ip_packet.increment();

    match frame_dst {
        Some(FrameDestination::Individual { local: false }) => {
            CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                .dispatch_receive_ip_packet_other_host
                .increment();
        }
        Some(FrameDestination::Individual { local: true })
        | Some(FrameDestination::Multicast)
        | Some(FrameDestination::Broadcast)
        | None => (),
    }

    let proto = packet.proto();

    match core_ctx.filter_handler().local_ingress_hook(
        bindings_ctx,
        &mut packet,
        device,
        &mut packet_metadata,
    ) {
        filter::Verdict::Drop => {
            packet_metadata.acknowledge_drop();
            return Ok(());
        }
        filter::Verdict::Accept(()) => {}
    }

    // These invariants are validated by the caller of this function, but it's
    // possible for the LOCAL_INGRESS hook to rewrite the packet, so we have to
    // check them again.
    let Some(src_ip) = packet.src_ipv6() else {
        debug!(
            "dispatch_receive_ipv6_packet: received packet from non-unicast source {} after the \
            LOCAL_INGRESS hook; dropping",
            packet.src_ip()
        );
        CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
            .version_rx
            .non_unicast_source
            .increment();
        return Ok(());
    };
    let Some(dst_ip) = SpecifiedAddr::new(packet.dst_ip()) else {
        CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).unspecified_destination.increment();
        debug!(
            "dispatch_receive_ipv6_packet: Received packet with unspecified destination IP address \
            after the LOCAL_INGRESS hook; dropping"
        );
        return Ok(());
    };

    core_ctx.deliver_packet_to_raw_ip_sockets(bindings_ctx, &packet, &device);

    let (fixed, extension, body) = packet.parts_with_body_mut();
    let buffer = Buf::new(body, ..);
    let header_info = Ipv6HeaderInfo { fixed, extension };
    let receive_info = LocalDeliveryPacketInfo { meta, header_info, marks: packet_metadata.marks };

    let result = core_ctx
        .dispatch_receive_ip_packet(
            bindings_ctx,
            device,
            src_ip,
            dst_ip,
            proto,
            buffer,
            &receive_info,
        )
        .or_else(|err| {
            if let Ipv6SourceAddr::Unicast(src_ip) = src_ip {
                let (_, _, _, meta) = packet.into_metadata();
                Err(IcmpErrorSender {
                    err: err.into_icmpv6_error(meta.header_len()),
                    src_ip: *src_ip,
                    dst_ip,
                    frame_dst,
                    device,
                    meta,
                    marks: receive_info.marks,
                })
            } else {
                Ok(())
            }
        });
    packet_metadata.acknowledge_drop();
    result
}

/// The metadata required to forward an IP Packet.
///
/// This allows the forwarding of the packet to be decoupled from the
/// determination of how to forward. This is advantageous because forwarding
/// requires the underlying packet buffer, which cannot be "moved" in certain
/// contexts.
pub(crate) struct IpPacketForwarder<
    'a,
    I: IpLayerIpExt,
    D,
    A,
    BT: FilterBindingsTypes + TxMetadataBindingsTypes,
> {
    inbound_device: &'a D,
    outbound_device: &'a D,
    packet_meta: IpLayerPacketMetadata<I, A, BT>,
    src_ip: I::RecvSrcAddr,
    dst_ip: SpecifiedAddr<I::Addr>,
    destination: IpPacketDestination<I, &'a D>,
    proto: I::Proto,
    parse_meta: ParseMetadata,
    frame_dst: Option<FrameDestination>,
}

impl<'a, I, D, A, BC> IpPacketForwarder<'a, I, D, A, BC>
where
    I: IpLayerIpExt,
    BC: IpLayerBindingsContext<I, D>,
{
    // Forward the provided buffer as specified by this [`IpPacketForwarder`].
    fn forward_with_buffer<CC, B>(self, core_ctx: &mut CC, bindings_ctx: &mut BC, buffer: B)
    where
        B: BufferMut,
        CC: IpLayerForwardingContext<I, BC, DeviceId = D, WeakAddressId = A>,
    {
        let Self {
            inbound_device,
            outbound_device,
            packet_meta,
            src_ip,
            dst_ip,
            destination,
            proto,
            parse_meta,
            frame_dst,
        } = self;

        let packet = ForwardedPacket::new(src_ip.into(), dst_ip.get(), proto, parse_meta, buffer);

        trace!("forward_with_buffer: forwarding {} packet", I::NAME);

        let marks = packet_meta.marks;
        match send_ip_frame(
            core_ctx,
            bindings_ctx,
            outbound_device,
            destination,
            packet,
            packet_meta,
            Mtu::no_limit(),
        ) {
            Ok(()) => (),
            Err(IpSendFrameError { serializer, error }) => {
                match error {
                    IpSendFrameErrorReason::Device(
                        SendFrameErrorReason::SizeConstraintsViolation,
                    ) => {
                        debug!("failed to forward {} packet: MTU exceeded", I::NAME);
                        CounterContext::<IpCounters<I>>::counters(core_ctx)
                            .mtu_exceeded
                            .increment();
                        let mtu = core_ctx.get_mtu(inbound_device);
                        // NB: Ipv6 sends a PacketTooBig error. Ipv4 sends nothing.
                        let Some(err) = I::new_mtu_exceeded(proto, parse_meta.header_len(), mtu)
                        else {
                            return;
                        };
                        // NB: Only send an ICMP error if the sender's src
                        // is specified.
                        let Some(src_ip) = I::received_source_as_icmp_source(src_ip) else {
                            return;
                        };
                        // TODO(https://fxbug.dev/362489447): Increment the TTL since we
                        // just decremented it. The fact that we don't do this is
                        // technically a violation of the ICMP spec (we're not
                        // encapsulating the original packet that caused the
                        // issue, but a slightly modified version of it), but
                        // it's not that big of a deal because it won't affect
                        // the sender's ability to figure out the minimum path
                        // MTU. This may break other logic, though, so we should
                        // still fix it eventually.
                        core_ctx.send_icmp_error_message(
                            bindings_ctx,
                            inbound_device,
                            frame_dst,
                            src_ip,
                            dst_ip,
                            serializer.into_buffer(),
                            err,
                            &marks,
                        );
                    }
                    IpSendFrameErrorReason::Device(SendFrameErrorReason::QueueFull)
                    | IpSendFrameErrorReason::Device(SendFrameErrorReason::Alloc)
                    | IpSendFrameErrorReason::IllegalLoopbackAddress => (),
                }
                debug!("failed to forward {} packet: {error:?}", I::NAME);
            }
        }
    }
}

/// The action to take for a packet that was a candidate for forwarding.
pub(crate) enum ForwardingAction<
    'a,
    I: IpLayerIpExt,
    D,
    A,
    BT: FilterBindingsTypes + TxMetadataBindingsTypes,
> {
    /// Drop the packet without forwarding it or generating an ICMP error.
    SilentlyDrop,
    /// Forward the packet, as specified by the [`IpPacketForwarder`].
    Forward(IpPacketForwarder<'a, I, D, A, BT>),
    /// Drop the packet without forwarding, and generate an ICMP error as
    /// specified by the [`IcmpErrorSender`].
    DropWithIcmpError(IcmpErrorSender<'a, I, D>),
}

impl<'a, I, D, A, BC> ForwardingAction<'a, I, D, A, BC>
where
    I: IpLayerIpExt,
    BC: IpLayerBindingsContext<I, D>,
{
    /// Perform the action prescribed by self, with the provided packet buffer.
    pub(crate) fn perform_action_with_buffer<CC, B>(
        self,
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        buffer: B,
    ) where
        B: BufferMut,
        CC: IpLayerForwardingContext<I, BC, DeviceId = D, WeakAddressId = A>,
    {
        match self {
            ForwardingAction::SilentlyDrop => {}
            ForwardingAction::Forward(forwarder) => {
                forwarder.forward_with_buffer(core_ctx, bindings_ctx, buffer)
            }
            ForwardingAction::DropWithIcmpError(icmp_sender) => {
                icmp_sender.respond_with_icmp_error(core_ctx, bindings_ctx, buffer)
            }
        }
    }
}

/// Determine which [`ForwardingAction`] should be taken for an IP packet.
pub(crate) fn determine_ip_packet_forwarding_action<'a, 'b, I, BC, CC>(
    core_ctx: &'a mut CC,
    mut packet: I::Packet<&'a mut [u8]>,
    mut packet_meta: IpLayerPacketMetadata<I, CC::WeakAddressId, BC>,
    minimum_ttl: Option<u8>,
    inbound_device: &'b CC::DeviceId,
    outbound_device: &'b CC::DeviceId,
    destination: IpPacketDestination<I, &'b CC::DeviceId>,
    frame_dst: Option<FrameDestination>,
    src_ip: I::RecvSrcAddr,
    dst_ip: SpecifiedAddr<I::Addr>,
) -> ForwardingAction<'b, I, CC::DeviceId, CC::WeakAddressId, BC>
where
    I: IpLayerIpExt,
    BC: IpLayerBindingsContext<I, CC::DeviceId>,
    CC: IpLayerForwardingContext<I, BC>,
{
    // When forwarding, if a datagram's TTL is one or zero, discard it, as
    // decrementing the TTL would put it below the allowed minimum value.
    // For IPv4, see "TTL" section, https://tools.ietf.org/html/rfc791#page-14.
    // For IPv6, see "Hop Limit" section, https://datatracker.ietf.org/doc/html/rfc2460#page-5.
    const DEFAULT_MINIMUM_FORWARDING_TTL: u8 = 2;
    let minimum_ttl = minimum_ttl.unwrap_or(DEFAULT_MINIMUM_FORWARDING_TTL);

    let ttl = packet.ttl();
    if ttl < minimum_ttl {
        debug!(
            "{} packet not forwarded due to inadequate TTL: got={ttl} minimum={minimum_ttl}",
            I::NAME
        );
        // As per RFC 792's specification of the Time Exceeded Message:
        //     If the gateway processing a datagram finds the time to live
        //     field is zero it must discard the datagram. The gateway may
        //     also notify the source host via the time exceeded message.
        // And RFC 4443 section 3.3:
        //    If a router receives a packet with a Hop Limit of zero, or if
        //    a router decrements a packet's Hop Limit to zero, it MUST
        //    discard the packet and originate an ICMPv6 Time Exceeded
        //    message with Code 0 to the source of the packet.
        // Don't send a Time Exceeded Message in cases where the netstack is
        // enforcing a higher minimum TTL (e.g. as part of a multicast route).
        if ttl > 1 {
            packet_meta.acknowledge_drop();
            return ForwardingAction::SilentlyDrop;
        }

        CounterContext::<IpCounters<I>>::counters(core_ctx).ttl_expired.increment();

        // Only send an ICMP error if the src_ip is specified.
        let Some(src_ip) = I::received_source_as_icmp_source(src_ip) else {
            CounterContext::<IpCounters<I>>::counters(core_ctx).unspecified_source.increment();
            packet_meta.acknowledge_drop();
            return ForwardingAction::SilentlyDrop;
        };

        // Construct and send the appropriate ICMP error for the IP version.
        let version_specific_meta = packet.version_specific_meta();
        let (_, _, proto, parse_meta): (I::Addr, I::Addr, _, _) = packet.into_metadata();
        let err = I::new_ttl_expired(proto, parse_meta.header_len(), version_specific_meta);
        let action = ForwardingAction::DropWithIcmpError(IcmpErrorSender {
            err,
            src_ip,
            dst_ip,
            frame_dst,
            device: inbound_device,
            meta: parse_meta,
            marks: packet_meta.marks,
        });
        packet_meta.acknowledge_drop();
        return action;
    }

    trace!("determine_ip_packet_forwarding_action: adequate TTL");

    // For IPv6 packets, handle extension headers first.
    //
    // Any previous handling of extension headers was done under the
    // assumption that we are the final destination of the packet. Now that
    // we know we're forwarding, we need to re-examine them.
    let maybe_ipv6_packet_action = I::map_ip_in(
        &packet,
        |_packet| None,
        |packet| {
            Some(ipv6::handle_extension_headers(core_ctx, inbound_device, frame_dst, packet, false))
        },
    );
    match maybe_ipv6_packet_action {
        None => {} // NB: Ipv4 case.
        Some(Ipv6PacketAction::_Discard) => {
            let counters = CounterContext::<IpCounters<I>>::counters(core_ctx);
            #[derive(GenericOverIp)]
            #[generic_over_ip(I, Ip)]
            struct InCounters<'a, I: IpLayerIpExt>(&'a I::RxCounters);
            I::map_ip::<_, ()>(
                InCounters(&counters.version_rx),
                |_counters| {
                    unreachable!("`I` must be `Ipv6` because we're handling IPv6 extension headers")
                },
                |InCounters(counters)| counters.extension_header_discard.increment(),
            );
            trace!(
                "determine_ip_packet_forwarding_action: handled IPv6 extension headers: \
                discarding packet"
            );
            packet_meta.acknowledge_drop();
            return ForwardingAction::SilentlyDrop;
        }
        Some(Ipv6PacketAction::Continue) => {
            trace!(
                "determine_ip_packet_forwarding_action: handled IPv6 extension headers: \
                forwarding packet"
            );
        }
        Some(Ipv6PacketAction::ProcessFragment) => {
            unreachable!(
                "When forwarding packets, we should only ever look at the hop by hop \
                    options extension header (if present)"
            )
        }
    };

    match core_ctx.filter_handler().forwarding_hook(
        I::as_filter_packet(&mut packet),
        inbound_device,
        outbound_device,
        &mut packet_meta,
    ) {
        filter::Verdict::Drop => {
            packet_meta.acknowledge_drop();
            trace!("determine_ip_packet_forwarding_action: filter verdict: Drop");
            return ForwardingAction::SilentlyDrop;
        }
        filter::Verdict::Accept(()) => {}
    }

    packet.set_ttl(ttl - 1);
    let (_, _, proto, parse_meta): (I::Addr, I::Addr, _, _) = packet.into_metadata();
    ForwardingAction::Forward(IpPacketForwarder {
        inbound_device,
        outbound_device,
        packet_meta,
        src_ip,
        dst_ip,
        destination,
        proto,
        parse_meta,
        frame_dst,
    })
}

pub(crate) fn send_ip_frame<I, CC, BC, S>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    destination: IpPacketDestination<I, &CC::DeviceId>,
    mut body: S,
    mut packet_metadata: IpLayerPacketMetadata<I, CC::WeakAddressId, BC>,
    limit_mtu: Mtu,
) -> Result<(), IpSendFrameError<S>>
where
    I: IpLayerIpExt,
    BC: FilterBindingsContext + TxMetadataBindingsTypes,
    CC: IpLayerEgressContext<I, BC> + IpDeviceMtuContext<I> + IpDeviceAddressIdContext<I>,
    S: FragmentableIpSerializer<I, Buffer: BufferMut> + IpPacket<I>,
{
    let (verdict, proof) = core_ctx.filter_handler().egress_hook(
        bindings_ctx,
        &mut body,
        device,
        &mut packet_metadata,
    );
    match verdict {
        filter::Verdict::Drop => {
            packet_metadata.acknowledge_drop();
            return Ok(());
        }
        filter::Verdict::Accept(()) => {}
    }

    // If the packet is leaving through the loopback device, attempt to extract a
    // weak reference to the packet's conntrack entry to plumb that through the
    // device layer so it can be reused on ingress to the IP layer.
    let (conntrack_connection_and_direction, tx_metadata, marks) = packet_metadata.into_parts();
    let conntrack_entry = if device.is_loopback() {
        conntrack_connection_and_direction
            .and_then(|(conn, dir)| WeakConntrackConnection::new(&conn).map(|conn| (conn, dir)))
    } else {
        None
    };
    let device_ip_layer_metadata = DeviceIpLayerMetadata { conntrack_entry, tx_metadata, marks };

    // The filtering layer may have changed our address. Perform a last moment
    // check to protect against sending loopback addresses on the wire for
    // non-loopback devices, which is an RFC violation.
    if !device.is_loopback()
        && (I::LOOPBACK_SUBNET.contains(&body.src_addr())
            || I::LOOPBACK_SUBNET.contains(&body.dst_addr()))
    {
        CounterContext::<IpCounters<I>>::counters(core_ctx).tx_illegal_loopback_address.increment();
        return Err(IpSendFrameError {
            serializer: body,
            error: IpSendFrameErrorReason::IllegalLoopbackAddress,
        });
    }

    // Use the minimum MTU between the target device and the requested mtu.
    let mtu = limit_mtu.min(core_ctx.get_mtu(device));

    let body = body.with_size_limit(mtu.into());

    let fits_mtu =
        match body.serialize_new_buf(PacketConstraints::UNCONSTRAINED, AlwaysFailBufferAlloc) {
            // We hit the allocator that refused to allocate new data, which
            // means the MTU is respected.
            Err(SerializeError::Alloc(())) => true,
            // MTU failure, we should try to fragment.
            Err(SerializeError::SizeLimitExceeded) => false,
        };

    if fits_mtu {
        return core_ctx
            .send_ip_frame(bindings_ctx, device, destination, device_ip_layer_metadata, body, proof)
            .map_err(|ErrorAndSerializer { serializer, error }| IpSendFrameError {
                serializer: serializer.into_inner(),
                error: error.into(),
            });
    }

    // Body doesn't fit MTU, we must fragment this serializer in order to send
    // it out.
    CounterContext::<IpCounters<I>>::counters(core_ctx)
        .fragmentation
        .fragmentation_required
        .increment();

    // Taken on the last frame.
    let mut device_ip_layer_metadata = Some(device_ip_layer_metadata);
    let body = body.into_inner();
    let result = match IpFragmenter::new(bindings_ctx, &body, mtu) {
        Ok(mut fragmenter) => loop {
            let (fragment, has_more) = match fragmenter.next() {
                None => break Ok(()),
                Some(f) => f,
            };

            // TODO(https://fxbug.dev/391953082): We should penalize sockets
            // via the tx metadata when we incur IP fragmentation instead of
            // just attaching the ownership to the last fragment. For now, we
            // attach the tx metadata to the last frame only.
            let device_ip_layer_metadata = if has_more {
                // Unwrap here because only the last frame can take it.
                let device_ip_layer_metadata = device_ip_layer_metadata.as_ref().unwrap();
                DeviceIpLayerMetadata {
                    conntrack_entry: device_ip_layer_metadata.conntrack_entry.clone(),
                    tx_metadata: Default::default(),
                    marks: device_ip_layer_metadata.marks,
                }
            } else {
                // Unwrap here because the last frame can only happen once.
                device_ip_layer_metadata.take().unwrap()
            };

            match core_ctx.send_ip_frame(
                bindings_ctx,
                device,
                destination.clone(),
                device_ip_layer_metadata,
                fragment,
                proof.clone_for_fragmentation(),
            ) {
                Ok(()) => {
                    CounterContext::<IpCounters<I>>::counters(core_ctx)
                        .fragmentation
                        .fragments
                        .increment();
                }
                Err(ErrorAndSerializer { serializer: _, error }) => {
                    CounterContext::<IpCounters<I>>::counters(core_ctx)
                        .fragmentation
                        .error_fragmented_serializer
                        .increment();
                    break Err(error);
                }
            }
        },
        Err(e) => {
            CounterContext::<IpCounters<I>>::counters(core_ctx)
                .fragmentation
                .error_counter(e)
                .increment();
            Err(SendFrameErrorReason::SizeConstraintsViolation)
        }
    };
    result.map_err(|e| IpSendFrameError { serializer: body, error: e.into() })
}

/// A buffer allocator that always fails to allocate a new buffer.
///
/// Can be used to check for packet size constraints in serializer without in
/// fact serializing the buffer.
struct AlwaysFailBufferAlloc;

impl BufferAlloc<Never> for AlwaysFailBufferAlloc {
    type Error = ();
    fn alloc(self, _len: usize) -> Result<Never, Self::Error> {
        Err(())
    }
}

/// Drop a packet and undo the effects of parsing it.
///
/// `drop_packet_and_undo_parse!` takes a `$packet` and a `$buffer` which the
/// packet was parsed from. It saves the results of the `src_ip()`, `dst_ip()`,
/// `proto()`, and `parse_metadata()` methods. It drops `$packet` and uses the
/// result of `parse_metadata()` to undo the effects of parsing the packet.
/// Finally, it returns the source IP, destination IP, protocol, and parse
/// metadata.
macro_rules! drop_packet_and_undo_parse {
    ($packet:expr, $buffer:expr) => {{
        let (src_ip, dst_ip, proto, meta) = $packet.into_metadata();
        $buffer.undo_parse(meta);
        (src_ip, dst_ip, proto, meta)
    }};
}

/// The result of calling [`process_fragment`], depending on what action needs
/// to be taken by the caller.
enum ProcessFragmentResult<'a, I: IpLayerIpExt> {
    /// Processing of the packet is complete and no more action should be
    /// taken.
    Done,

    /// Reassembly is not needed. The returned packet is the same one that was
    /// passed in the call to [`process_fragment`].
    NotNeeded(I::Packet<&'a mut [u8]>),

    /// A packet was successfully reassembled into the provided buffer. If a
    /// parsed packet is needed, then the caller must perform that parsing.
    Reassembled(Vec<u8>),
}

/// Process a fragment and reassemble if required.
///
/// Attempts to process a potential fragment packet and reassemble if we are
/// ready to do so. Returns an enum to the caller with the result of processing
/// the potential fragment.
fn process_fragment<'a, I, CC, BC>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    packet: I::Packet<&'a mut [u8]>,
) -> ProcessFragmentResult<'a, I>
where
    I: IpLayerIpExt,
    for<'b> I::Packet<&'b mut [u8]>: FragmentablePacket,
    CC: IpLayerIngressContext<I, BC> + CounterContext<IpCounters<I>>,
    BC: IpLayerBindingsContext<I, CC::DeviceId>,
{
    match FragmentHandler::<I, _>::process_fragment::<&mut [u8]>(core_ctx, bindings_ctx, packet) {
        // Handle the packet right away since reassembly is not needed.
        FragmentProcessingState::NotNeeded(packet) => {
            trace!("receive_ip_packet: not fragmented");
            ProcessFragmentResult::NotNeeded(packet)
        }
        // Ready to reassemble a packet.
        FragmentProcessingState::Ready { key, packet_len } => {
            trace!("receive_ip_packet: fragmented, ready for reassembly");
            // Allocate a buffer of `packet_len` bytes.
            let mut buffer = Buf::new(alloc::vec![0; packet_len], ..);

            // Attempt to reassemble the packet.
            let reassemble_result = match FragmentHandler::<I, _>::reassemble_packet(
                core_ctx,
                bindings_ctx,
                &key,
                buffer.buffer_view_mut(),
            ) {
                // Successfully reassembled the packet, handle it.
                Ok(()) => ProcessFragmentResult::Reassembled(buffer.into_inner()),
                Err(e) => {
                    CounterContext::<IpCounters<I>>::counters(core_ctx)
                        .fragment_reassembly_error
                        .increment();
                    debug!("receive_ip_packet: fragmented, failed to reassemble: {:?}", e);
                    ProcessFragmentResult::Done
                }
            };
            reassemble_result
        }
        // Cannot proceed since we need more fragments before we
        // can reassemble a packet.
        FragmentProcessingState::NeedMoreFragments => {
            CounterContext::<IpCounters<I>>::counters(core_ctx).need_more_fragments.increment();
            trace!("receive_ip_packet: fragmented, need more before reassembly");
            ProcessFragmentResult::Done
        }
        // TODO(ghanan): Handle invalid fragments.
        FragmentProcessingState::InvalidFragment => {
            CounterContext::<IpCounters<I>>::counters(core_ctx).invalid_fragment.increment();
            trace!("receive_ip_packet: fragmented, invalid");
            ProcessFragmentResult::Done
        }
        FragmentProcessingState::OutOfMemory => {
            CounterContext::<IpCounters<I>>::counters(core_ctx).fragment_cache_full.increment();
            trace!("receive_ip_packet: fragmented, dropped because OOM");
            ProcessFragmentResult::Done
        }
    }
}

// TODO(joshlf): Can we turn `try_parse_ip_packet` into a function? So far, I've
// been unable to get the borrow checker to accept it.

/// Try to parse an IP packet from a buffer.
///
/// If parsing fails, return the buffer to its original state so that its
/// contents can be used to send an ICMP error message. When invoked, the macro
/// expands to an expression whose type is `Result<P, P::Error>`, where `P` is
/// the parsed packet type.
macro_rules! try_parse_ip_packet {
    ($buffer:expr) => {{
        let p_len = $buffer.prefix_len();
        let s_len = $buffer.suffix_len();

        let result = $buffer.parse_mut();

        if let Err(err) = result {
            // Revert `buffer` to it's original state.
            let n_p_len = $buffer.prefix_len();
            let n_s_len = $buffer.suffix_len();

            if p_len > n_p_len {
                $buffer.grow_front(p_len - n_p_len);
            }

            if s_len > n_s_len {
                $buffer.grow_back(s_len - n_s_len);
            }

            Err(err)
        } else {
            result
        }
    }};
}

/// Clone an IP packet so that it may be delivered to a multicast route target.
///
/// Note: We must copy the underlying data here, as the filtering
/// engine may uniquely modify each instance as part of
/// performing forwarding.
///
/// In the future there are potential optimizations we could
/// pursue, including:
///   * Copy-on-write semantics for the buffer/packet so that
///     copies of the underlying data are done on an as-needed
///     basis.
///   * Avoid reparsing the IP packet. Because we're parsing an
///     exact copy of a known good packet, it would be safe to
///     adopt the data as an IP packet without performing any
///     validation.
// NB: This is a macro, not a function, because Rust's "move" semantics prevent
// us from returning both a buffer and a packet referencing that buffer.
macro_rules! clone_packet_for_mcast_forwarding {
    {let ($new_data:ident, $new_buffer:ident, $new_packet:ident) = $packet:ident} => {
        let mut $new_data = $packet.to_vec();
        let mut $new_buffer: Buf<&mut [u8]> = Buf::new($new_data.as_mut(), ..);
        let $new_packet = try_parse_ip_packet!($new_buffer).unwrap();
    };
}

/// Receive an IPv4 packet from a device.
///
/// `frame_dst` specifies how this packet was received; see [`FrameDestination`]
/// for options.
pub fn receive_ipv4_packet<
    BC: IpLayerBindingsContext<Ipv4, CC::DeviceId>,
    B: BufferMut,
    CC: IpLayerIngressContext<Ipv4, BC> + CounterContext<IpCounters<Ipv4>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    device_ip_layer_metadata: DeviceIpLayerMetadata<BC>,
    buffer: B,
) {
    if !core_ctx.is_ip_device_enabled(&device) {
        return;
    }

    // This is required because we may need to process the buffer that was
    // passed in or a reassembled one, which have different types.
    let mut buffer: packet::Either<B, Buf<Vec<u8>>> = packet::Either::A(buffer);

    CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).receive_ip_packet.increment();
    trace!("receive_ip_packet({device:?})");

    let packet: Ipv4Packet<_> = match try_parse_ip_packet!(buffer) {
        Ok(packet) => packet,
        // Conditionally send an ICMP response if we encountered a parameter
        // problem error when parsing an IPv4 packet. Note, we do not always
        // send back an ICMP response as it can be used as an attack vector for
        // DDoS attacks. We only send back an ICMP response if the RFC requires
        // that we MUST send one, as noted by `must_send_icmp` and `action`.
        // TODO(https://fxbug.dev/42157630): test this code path once
        // `Ipv4Packet::parse` can return an `IpParseError::ParameterProblem`
        // error.
        Err(IpParseError::ParameterProblem {
            src_ip,
            dst_ip,
            code,
            pointer,
            must_send_icmp,
            header_len,
            action,
        }) if must_send_icmp && action.should_send_icmp(&dst_ip) => {
            CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).parameter_problem.increment();
            // `should_send_icmp_to_multicast` should never return `true` for IPv4.
            assert!(!action.should_send_icmp_to_multicast());
            let dst_ip = match SpecifiedAddr::new(dst_ip) {
                Some(ip) => ip,
                None => {
                    CounterContext::<IpCounters<Ipv4>>::counters(core_ctx)
                        .unspecified_destination
                        .increment();
                    debug!("receive_ipv4_packet: Received packet with unspecified destination IP address; dropping");
                    return;
                }
            };
            let src_ip = match SpecifiedAddr::new(src_ip) {
                Some(ip) => ip,
                None => {
                    CounterContext::<IpCounters<Ipv4>>::counters(core_ctx)
                        .unspecified_source
                        .increment();
                    trace!("receive_ipv4_packet: Cannot send ICMP error in response to packet with unspecified source IP address");
                    return;
                }
            };
            IcmpErrorHandler::<Ipv4, _>::send_icmp_error_message(
                core_ctx,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                buffer,
                Icmpv4Error {
                    kind: Icmpv4ErrorKind::ParameterProblem {
                        code,
                        pointer,
                        // When the call to `action.should_send_icmp` returns true, it always means that
                        // the IPv4 packet that failed parsing is an initial fragment.
                        fragment_type: Ipv4FragmentType::InitialFragment,
                    },
                    header_len,
                },
                &device_ip_layer_metadata.marks,
            );
            return;
        }
        _ => return, // TODO(joshlf): Do something with ICMP here?
    };

    // We verify this later by actually creating the `SpecifiedAddr` witness
    // type after the INGRESS filtering hook, but we keep this check here as an
    // optimization to return early if the packet has an unspecified
    // destination.
    if !packet.dst_ip().is_specified() {
        CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).unspecified_destination.increment();
        debug!("receive_ipv4_packet: Received packet with unspecified destination IP; dropping");
        return;
    };

    // Reassemble all packets before local delivery or forwarding. Reassembly
    // before forwarding is not RFC-compliant, but it's the easiest way to
    // ensure that fragments are filtered properly. Linux does this and it
    // doesn't seem to create major problems.
    //
    // TODO(https://fxbug.dev/345814518): Forward fragments without reassembly.
    //
    // Note, the `process_fragment` function could panic if the packet does not
    // have fragment data. However, we are guaranteed that it will not panic
    // because the fragment data is in the fixed header so it is always present
    // (even if the fragment data has values that implies that the packet is not
    // fragmented).
    let mut packet = match process_fragment(core_ctx, bindings_ctx, packet) {
        ProcessFragmentResult::Done => return,
        ProcessFragmentResult::NotNeeded(packet) => packet,
        ProcessFragmentResult::Reassembled(buf) => {
            let buf = Buf::new(buf, ..);
            buffer = packet::Either::B(buf);

            match buffer.parse_mut() {
                Ok(packet) => packet,
                Err(err) => {
                    CounterContext::<IpCounters<Ipv4>>::counters(core_ctx)
                        .fragment_reassembly_error
                        .increment();
                    debug!("receive_ip_packet: fragmented, failed to reassemble: {:?}", err);
                    return;
                }
            }
        }
    };

    // TODO(ghanan): Act upon options.

    let mut packet_metadata =
        IpLayerPacketMetadata::from_device_ip_layer_metadata(core_ctx, device_ip_layer_metadata);
    let mut filter = core_ctx.filter_handler();
    match filter.ingress_hook(bindings_ctx, &mut packet, device, &mut packet_metadata) {
        IngressVerdict::Verdict(filter::Verdict::Accept(())) => {}
        IngressVerdict::Verdict(filter::Verdict::Drop) => {
            packet_metadata.acknowledge_drop();
            return;
        }
        IngressVerdict::TransparentLocalDelivery { addr, port } => {
            // Drop the filter handler since it holds a mutable borrow of `core_ctx`, which
            // we need to provide to the packet dispatch function.
            drop(filter);

            let Some(addr) = SpecifiedAddr::new(addr) else {
                CounterContext::<IpCounters<Ipv4>>::counters(core_ctx)
                    .unspecified_destination
                    .increment();
                debug!("cannot perform transparent delivery to unspecified destination; dropping");
                return;
            };

            let receive_meta = ReceiveIpPacketMeta {
                // It's possible that the packet was actually sent to a
                // broadcast address, but it doesn't matter here since it's
                // being delivered to a transparent proxy.
                broadcast: None,
                transparent_override: Some(TransparentLocalDelivery { addr, port }),
            };

            // Short-circuit the routing process and override local demux, providing a local
            // address and port to which the packet should be transparently delivered at the
            // transport layer.
            dispatch_receive_ipv4_packet(
                core_ctx,
                bindings_ctx,
                device,
                frame_dst,
                packet,
                packet_metadata,
                receive_meta,
            )
            .unwrap_or_else(|err| err.respond_with_icmp_error(core_ctx, bindings_ctx, buffer));
            return;
        }
    }
    // Drop the filter handler since it holds a mutable borrow of `core_ctx`, which
    // we need below.
    drop(filter);

    let action = receive_ipv4_packet_action(
        core_ctx,
        bindings_ctx,
        device,
        &packet,
        frame_dst,
        &packet_metadata.marks,
    );
    match action {
        ReceivePacketAction::MulticastForward { targets, address_status, dst_ip } => {
            let src_ip = packet.src_ip();
            // TOOD(https://fxbug.dev/364242513): Support connection tracking of
            // the multiplexed flows created by multicast forwarding. Here, we
            // use the existing metadata for the first action taken, and then
            // a default instance for each subsequent action. The first action
            // will populate the conntrack table with an entry, which will then
            // be used by all subsequent forwards.
            let mut packet_metadata = Some(packet_metadata);
            for MulticastRouteTarget { output_interface, min_ttl } in targets.as_ref() {
                clone_packet_for_mcast_forwarding! {
                    let (copy_of_data, copy_of_buffer, copy_of_packet) = packet
                };
                determine_ip_packet_forwarding_action::<Ipv4, _, _>(
                    core_ctx,
                    copy_of_packet,
                    packet_metadata.take().unwrap_or_default(),
                    Some(*min_ttl),
                    device,
                    &output_interface,
                    IpPacketDestination::from_addr(dst_ip),
                    frame_dst,
                    src_ip,
                    dst_ip,
                )
                .perform_action_with_buffer(core_ctx, bindings_ctx, copy_of_buffer);
            }

            // If we also have an interest in the packet, deliver it locally.
            if let Some(address_status) = address_status {
                let receive_meta = ReceiveIpPacketMeta {
                    broadcast: address_status.to_broadcast_marker(),
                    transparent_override: None,
                };
                dispatch_receive_ipv4_packet(
                    core_ctx,
                    bindings_ctx,
                    device,
                    frame_dst,
                    packet,
                    packet_metadata.take().unwrap_or_default(),
                    receive_meta,
                )
                .unwrap_or_else(|err| err.respond_with_icmp_error(core_ctx, bindings_ctx, buffer));
            }
        }
        ReceivePacketAction::Deliver { address_status, internal_forwarding } => {
            // NB: when performing internal forwarding, hit the
            // forwarding hook.
            match internal_forwarding {
                InternalForwarding::Used(outbound_device) => {
                    CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).forward.increment();
                    match core_ctx.filter_handler().forwarding_hook(
                        &mut packet,
                        device,
                        &outbound_device,
                        &mut packet_metadata,
                    ) {
                        filter::Verdict::Drop => {
                            packet_metadata.acknowledge_drop();
                            return;
                        }
                        filter::Verdict::Accept(()) => {}
                    }
                }
                InternalForwarding::NotUsed => {}
            }

            let receive_meta = ReceiveIpPacketMeta {
                broadcast: address_status.to_broadcast_marker(),
                transparent_override: None,
            };
            dispatch_receive_ipv4_packet(
                core_ctx,
                bindings_ctx,
                device,
                frame_dst,
                packet,
                packet_metadata,
                receive_meta,
            )
            .unwrap_or_else(|err| err.respond_with_icmp_error(core_ctx, bindings_ctx, buffer));
        }
        ReceivePacketAction::Forward {
            original_dst,
            dst: Destination { device: dst_device, next_hop },
        } => {
            let src_ip = packet.src_ip();
            determine_ip_packet_forwarding_action::<Ipv4, _, _>(
                core_ctx,
                packet,
                packet_metadata,
                None,
                device,
                &dst_device,
                IpPacketDestination::from_next_hop(next_hop, original_dst),
                frame_dst,
                src_ip,
                original_dst,
            )
            .perform_action_with_buffer(core_ctx, bindings_ctx, buffer);
        }
        ReceivePacketAction::SendNoRouteToDest { dst: dst_ip } => {
            use packet_formats::ipv4::Ipv4Header as _;
            CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).no_route_to_host.increment();
            debug!("received IPv4 packet with no known route to destination {}", dst_ip);
            let fragment_type = packet.fragment_type();
            let (src_ip, _, proto, meta): (_, Ipv4Addr, _, _) =
                drop_packet_and_undo_parse!(packet, buffer);
            let marks = packet_metadata.marks;
            packet_metadata.acknowledge_drop();
            let src_ip = match SpecifiedAddr::new(src_ip) {
                Some(ip) => ip,
                None => {
                    CounterContext::<IpCounters<Ipv4>>::counters(core_ctx)
                        .unspecified_source
                        .increment();
                    trace!("receive_ipv4_packet: Cannot send ICMP error in response to packet with unspecified source IP address");
                    return;
                }
            };
            IcmpErrorHandler::<Ipv4, _>::send_icmp_error_message(
                core_ctx,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                buffer,
                Icmpv4Error {
                    kind: Icmpv4ErrorKind::NetUnreachable { proto, fragment_type },
                    header_len: meta.header_len(),
                },
                &marks,
            );
        }
        ReceivePacketAction::Drop { reason } => {
            let src_ip = packet.src_ip();
            let dst_ip = packet.dst_ip();
            packet_metadata.acknowledge_drop();
            CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).dropped.increment();
            debug!(
                "receive_ipv4_packet: dropping packet from {src_ip} to {dst_ip} received on \
                {device:?}: {reason:?}",
            );
        }
    }
}

/// Receive an IPv6 packet from a device.
///
/// `frame_dst` specifies how this packet was received; see [`FrameDestination`]
/// for options.
pub fn receive_ipv6_packet<
    BC: IpLayerBindingsContext<Ipv6, CC::DeviceId>,
    B: BufferMut,
    CC: IpLayerIngressContext<Ipv6, BC> + CounterContext<IpCounters<Ipv6>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    frame_dst: Option<FrameDestination>,
    device_ip_layer_metadata: DeviceIpLayerMetadata<BC>,
    buffer: B,
) {
    if !core_ctx.is_ip_device_enabled(&device) {
        return;
    }

    // This is required because we may need to process the buffer that was
    // passed in or a reassembled one, which have different types.
    let mut buffer: packet::Either<B, Buf<Vec<u8>>> = packet::Either::A(buffer);

    CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).receive_ip_packet.increment();
    trace!("receive_ipv6_packet({:?})", device);

    let packet: Ipv6Packet<_> = match try_parse_ip_packet!(buffer) {
        Ok(packet) => packet,
        // Conditionally send an ICMP response if we encountered a parameter
        // problem error when parsing an IPv4 packet. Note, we do not always
        // send back an ICMP response as it can be used as an attack vector for
        // DDoS attacks. We only send back an ICMP response if the RFC requires
        // that we MUST send one, as noted by `must_send_icmp` and `action`.
        Err(IpParseError::ParameterProblem {
            src_ip,
            dst_ip,
            code,
            pointer,
            must_send_icmp,
            header_len: _,
            action,
        }) if must_send_icmp && action.should_send_icmp(&dst_ip) => {
            CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).parameter_problem.increment();
            let dst_ip = match SpecifiedAddr::new(dst_ip) {
                Some(ip) => ip,
                None => {
                    CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                        .unspecified_destination
                        .increment();
                    debug!("receive_ipv6_packet: Received packet with unspecified destination IP address; dropping");
                    return;
                }
            };
            let src_ip = match UnicastAddr::new(src_ip) {
                Some(ip) => ip,
                None => {
                    CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                        .version_rx
                        .non_unicast_source
                        .increment();
                    trace!("receive_ipv6_packet: Cannot send ICMP error in response to packet with non unicast source IP address");
                    return;
                }
            };
            IcmpErrorHandler::<Ipv6, _>::send_icmp_error_message(
                core_ctx,
                bindings_ctx,
                device,
                frame_dst,
                src_ip,
                dst_ip,
                buffer,
                Icmpv6ErrorKind::ParameterProblem {
                    code,
                    pointer,
                    allow_dst_multicast: action.should_send_icmp_to_multicast(),
                },
                &device_ip_layer_metadata.marks,
            );
            return;
        }
        _ => return, // TODO(joshlf): Do something with ICMP here?
    };

    trace!("receive_ipv6_packet: parsed packet: {:?}", packet);

    // TODO(ghanan): Act upon extension headers.

    // We verify these properties later by actually creating the corresponding
    // witness types after the INGRESS filtering hook, but we keep these checks
    // here as an optimization to return early and save some work.
    if packet.src_ipv6().is_none() {
        debug!(
            "receive_ipv6_packet: received packet from non-unicast source {}; dropping",
            packet.src_ip()
        );
        CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
            .version_rx
            .non_unicast_source
            .increment();
        return;
    };
    if !packet.dst_ip().is_specified() {
        CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).unspecified_destination.increment();
        debug!("receive_ipv6_packet: Received packet with unspecified destination IP; dropping");
        return;
    };

    // Reassemble all packets before local delivery or forwarding. Reassembly
    // before forwarding is not RFC-compliant, but it's the easiest way to
    // ensure that fragments are filtered properly. Linux does this and it
    // doesn't seem to create major problems.
    //
    // TODO(https://fxbug.dev/345814518): Forward fragments without reassembly.
    //
    // delivery_extension_header_action is used to prevent looking at the
    // extension headers twice when a non-fragmented packet is delivered
    // locally.
    let (mut packet, delivery_extension_header_action) =
        match ipv6::handle_extension_headers(core_ctx, device, frame_dst, &packet, true) {
            Ipv6PacketAction::_Discard => {
                CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                    .version_rx
                    .extension_header_discard
                    .increment();
                trace!("receive_ipv6_packet: handled IPv6 extension headers: discarding packet");
                return;
            }
            Ipv6PacketAction::Continue => {
                trace!("receive_ipv6_packet: handled IPv6 extension headers: dispatching packet");
                (packet, Some(Ipv6PacketAction::Continue))
            }
            Ipv6PacketAction::ProcessFragment => {
                trace!(
                    "receive_ipv6_packet: handled IPv6 extension headers: handling \
                    fragmented packet"
                );

                // Note, `IpPacketFragmentCache::process_fragment`
                // could panic if the packet does not have fragment data.
                // However, we are guaranteed that it will not panic for an
                // IPv6 packet because the fragment data is in an (optional)
                // fragment extension header which we attempt to handle by
                // calling `ipv6::handle_extension_headers`. We will only
                // end up here if its return value is
                // `Ipv6PacketAction::ProcessFragment` which is only
                // possible when the packet has the fragment extension
                // header (even if the fragment data has values that implies
                // that the packet is not fragmented).
                match process_fragment(core_ctx, bindings_ctx, packet) {
                    ProcessFragmentResult::Done => return,
                    ProcessFragmentResult::NotNeeded(packet) => {
                        // While strange, it's possible for there to be a Fragment
                        // header that says the packet doesn't need defragmentation.
                        // As per RFC 8200 4.5:
                        //
                        //   If the fragment is a whole datagram (that is, both the
                        //   Fragment Offset field and the M flag are zero), then it
                        //   does not need any further reassembly and should be
                        //   processed as a fully reassembled packet (i.e., updating
                        //   Next Header, adjust Payload Length, removing the
                        //   Fragment header, etc.).
                        //
                        // In this case, we're not technically reassembling the
                        // packet, since, per the RFC, that would mean removing the
                        // Fragment header.
                        (packet, Some(Ipv6PacketAction::Continue))
                    }
                    ProcessFragmentResult::Reassembled(buf) => {
                        let buf = Buf::new(buf, ..);
                        buffer = packet::Either::B(buf);

                        match buffer.parse_mut() {
                            Ok(packet) => (packet, None),
                            Err(err) => {
                                CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                                    .fragment_reassembly_error
                                    .increment();
                                debug!(
                                    "receive_ip_packet: fragmented, failed to reassemble: {:?}",
                                    err
                                );
                                return;
                            }
                        }
                    }
                }
            }
        };

    let mut packet_metadata =
        IpLayerPacketMetadata::from_device_ip_layer_metadata(core_ctx, device_ip_layer_metadata);
    let mut filter = core_ctx.filter_handler();

    match filter.ingress_hook(bindings_ctx, &mut packet, device, &mut packet_metadata) {
        IngressVerdict::Verdict(filter::Verdict::Accept(())) => {}
        IngressVerdict::Verdict(filter::Verdict::Drop) => {
            packet_metadata.acknowledge_drop();
            return;
        }
        IngressVerdict::TransparentLocalDelivery { addr, port } => {
            // Drop the filter handler since it holds a mutable borrow of `core_ctx`, which
            // we need to provide to the packet dispatch function.
            drop(filter);

            let Some(addr) = SpecifiedAddr::new(addr) else {
                CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                    .unspecified_destination
                    .increment();
                debug!("cannot perform transparent delivery to unspecified destination; dropping");
                return;
            };

            let receive_meta = ReceiveIpPacketMeta {
                broadcast: None,
                transparent_override: Some(TransparentLocalDelivery { addr, port }),
            };

            // Short-circuit the routing process and override local demux, providing a local
            // address and port to which the packet should be transparently delivered at the
            // transport layer.
            dispatch_receive_ipv6_packet(
                core_ctx,
                bindings_ctx,
                device,
                frame_dst,
                packet,
                packet_metadata,
                receive_meta,
            )
            .unwrap_or_else(|err| err.respond_with_icmp_error(core_ctx, bindings_ctx, buffer));
            return;
        }
    }
    // Drop the filter handler since it holds a mutable borrow of `core_ctx`, which
    // we need below.
    drop(filter);

    let Some(src_ip) = packet.src_ipv6() else {
        debug!(
            "receive_ipv6_packet: received packet from non-unicast source {}; dropping",
            packet.src_ip()
        );
        CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
            .version_rx
            .non_unicast_source
            .increment();
        return;
    };

    match receive_ipv6_packet_action(
        core_ctx,
        bindings_ctx,
        device,
        &packet,
        frame_dst,
        &packet_metadata.marks,
    ) {
        ReceivePacketAction::MulticastForward { targets, address_status, dst_ip } => {
            // TOOD(https://fxbug.dev/364242513): Support connection tracking of
            // the multiplexed flows created by multicast forwarding. Here, we
            // use the existing metadata for the first action taken, and then
            // a default instance for each subsequent action. The first action
            // will populate the conntrack table with an entry, which will then
            // be used by all subsequent forwards.
            let mut packet_metadata = Some(packet_metadata);
            for MulticastRouteTarget { output_interface, min_ttl } in targets.as_ref() {
                clone_packet_for_mcast_forwarding! {
                    let (copy_of_data, copy_of_buffer, copy_of_packet) = packet
                };
                determine_ip_packet_forwarding_action::<Ipv6, _, _>(
                    core_ctx,
                    copy_of_packet,
                    packet_metadata.take().unwrap_or_default(),
                    Some(*min_ttl),
                    device,
                    &output_interface,
                    IpPacketDestination::from_addr(dst_ip),
                    frame_dst,
                    src_ip,
                    dst_ip,
                )
                .perform_action_with_buffer(core_ctx, bindings_ctx, copy_of_buffer);
            }

            // If we also have an interest in the packet, deliver it locally.
            if let Some(_) = address_status {
                let receive_meta =
                    ReceiveIpPacketMeta { broadcast: None, transparent_override: None };

                dispatch_receive_ipv6_packet(
                    core_ctx,
                    bindings_ctx,
                    device,
                    frame_dst,
                    packet,
                    packet_metadata.take().unwrap_or_default(),
                    receive_meta,
                )
                .unwrap_or_else(|err| err.respond_with_icmp_error(core_ctx, bindings_ctx, buffer));
            }
        }
        ReceivePacketAction::Deliver { address_status: _, internal_forwarding } => {
            trace!("receive_ipv6_packet: delivering locally");

            let action = if let Some(action) = delivery_extension_header_action {
                action
            } else {
                ipv6::handle_extension_headers(core_ctx, device, frame_dst, &packet, true)
            };
            match action {
                Ipv6PacketAction::_Discard => {
                    CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                        .version_rx
                        .extension_header_discard
                        .increment();
                    trace!(
                        "receive_ipv6_packet: handled IPv6 extension headers: discarding packet"
                    );
                    packet_metadata.acknowledge_drop();
                }
                Ipv6PacketAction::Continue => {
                    trace!(
                        "receive_ipv6_packet: handled IPv6 extension headers: dispatching packet"
                    );

                    // NB: when performing internal forwarding, hit the
                    // forwarding hook.
                    match internal_forwarding {
                        InternalForwarding::Used(outbound_device) => {
                            CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                                .forward
                                .increment();
                            match core_ctx.filter_handler().forwarding_hook(
                                &mut packet,
                                device,
                                &outbound_device,
                                &mut packet_metadata,
                            ) {
                                filter::Verdict::Drop => {
                                    packet_metadata.acknowledge_drop();
                                    return;
                                }
                                filter::Verdict::Accept(()) => {}
                            }
                        }
                        InternalForwarding::NotUsed => {}
                    }

                    let meta = ReceiveIpPacketMeta { broadcast: None, transparent_override: None };

                    // TODO(joshlf):
                    // - Do something with ICMP if we don't have a handler for
                    //   that protocol?
                    // - Check for already-expired TTL?
                    dispatch_receive_ipv6_packet(
                        core_ctx,
                        bindings_ctx,
                        device,
                        frame_dst,
                        packet,
                        packet_metadata,
                        meta,
                    )
                    .unwrap_or_else(|err| {
                        err.respond_with_icmp_error(core_ctx, bindings_ctx, buffer)
                    });
                }
                Ipv6PacketAction::ProcessFragment => {
                    debug!("receive_ipv6_packet: found fragment header after reassembly; dropping");
                    packet_metadata.acknowledge_drop();
                }
            }
        }
        ReceivePacketAction::Forward {
            original_dst,
            dst: Destination { device: dst_device, next_hop },
        } => {
            determine_ip_packet_forwarding_action::<Ipv6, _, _>(
                core_ctx,
                packet,
                packet_metadata,
                None,
                device,
                &dst_device,
                IpPacketDestination::from_next_hop(next_hop, original_dst),
                frame_dst,
                src_ip,
                original_dst,
            )
            .perform_action_with_buffer(core_ctx, bindings_ctx, buffer);
        }
        ReceivePacketAction::SendNoRouteToDest { dst: dst_ip } => {
            CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).no_route_to_host.increment();
            let (_, _, proto, meta): (Ipv6Addr, Ipv6Addr, _, _) =
                drop_packet_and_undo_parse!(packet, buffer);
            debug!("received IPv6 packet with no known route to destination {}", dst_ip);
            let marks = packet_metadata.marks;
            packet_metadata.acknowledge_drop();

            if let Ipv6SourceAddr::Unicast(src_ip) = src_ip {
                IcmpErrorHandler::<Ipv6, _>::send_icmp_error_message(
                    core_ctx,
                    bindings_ctx,
                    device,
                    frame_dst,
                    *src_ip,
                    dst_ip,
                    buffer,
                    Icmpv6ErrorKind::NetUnreachable { proto, header_len: meta.header_len() },
                    &marks,
                );
            }
        }
        ReceivePacketAction::Drop { reason } => {
            CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).dropped.increment();
            let src_ip = packet.src_ip();
            let dst_ip = packet.dst_ip();
            packet_metadata.acknowledge_drop();
            debug!(
                "receive_ipv6_packet: dropping packet from {src_ip} to {dst_ip} received on \
                {device:?}: {reason:?}",
            );
        }
    }
}

/// The action to take in order to process a received IP packet.
#[derive(Debug, PartialEq)]
pub enum ReceivePacketAction<I: BroadcastIpExt + IpLayerIpExt, DeviceId: StrongDeviceIdentifier> {
    /// Deliver the packet locally.
    Deliver {
        /// Status of the receiving IP address.
        address_status: I::AddressStatus,
        /// `InternalForwarding::Used(d)` if we're delivering the packet as a
        /// Weak Host performing internal forwarding via output device `d`.
        internal_forwarding: InternalForwarding<DeviceId>,
    },

    /// Forward the packet to the given destination.
    Forward {
        /// The original destination IP address of the packet.
        original_dst: SpecifiedAddr<I::Addr>,
        /// The destination that the packet should be forwarded to.
        dst: Destination<I::Addr, DeviceId>,
    },

    /// A multicast packet that should be forwarded (& optional local delivery).
    ///
    /// The packet should be forwarded to each of the given targets. This case
    /// is only returned when the packet is eligible for multicast forwarding;
    /// `Self::Deliver` is used for packets that are ineligible (either because
    /// multicast forwarding is disabled, or because there are no applicable
    /// multicast routes with which to forward the packet).
    MulticastForward {
        /// The multicast targets to forward the packet via.
        targets: MulticastRouteTargets<DeviceId>,
        /// Some if the host is a member of the multicast group and the packet
        /// should be delivered locally (in addition to forwarding).
        address_status: Option<I::AddressStatus>,
        /// The multicast address the packet should be forwarded to.
        dst_ip: SpecifiedAddr<I::Addr>,
    },

    /// Send a Destination Unreachable ICMP error message to the packet's sender
    /// and drop the packet.
    ///
    /// For ICMPv4, use the code "net unreachable". For ICMPv6, use the code "no
    /// route to destination".
    SendNoRouteToDest {
        /// The destination IP Address to which there was no route.
        dst: SpecifiedAddr<I::Addr>,
    },

    /// Silently drop the packet.
    ///
    /// `reason` describes why the packet was dropped.
    #[allow(missing_docs)]
    Drop { reason: DropReason },
}

/// The reason a received IP packet is dropped.
#[derive(Debug, PartialEq)]
pub enum DropReason {
    /// Remote packet destined to tentative address.
    Tentative,
    /// Remote packet destined to the unspecified address.
    UnspecifiedDestination,
    /// Cannot forward a packet with unspecified source address.
    ForwardUnspecifiedSource,
    /// Packet should be forwarded but packet's inbound interface has forwarding
    /// disabled.
    ForwardingDisabledInboundIface,
    /// Remote packet destined to a multicast address that could not be:
    /// * delivered locally (because we are not a member of the multicast
    ///   group), or
    /// * forwarded (either because multicast forwarding is disabled, or no
    ///   applicable multicast route has been installed).
    MulticastNoInterest,
}

/// Computes the action to take in order to process a received IPv4 packet.
pub fn receive_ipv4_packet_action<BC, CC, B>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    packet: &Ipv4Packet<B>,
    frame_dst: Option<FrameDestination>,
    marks: &Marks,
) -> ReceivePacketAction<Ipv4, CC::DeviceId>
where
    BC: IpLayerBindingsContext<Ipv4, CC::DeviceId>,
    CC: IpLayerContext<Ipv4, BC> + CounterContext<IpCounters<Ipv4>>,
    B: SplitByteSlice,
{
    let Some(dst_ip) = SpecifiedAddr::new(packet.dst_ip()) else {
        CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).unspecified_destination.increment();
        return ReceivePacketAction::Drop { reason: DropReason::UnspecifiedDestination };
    };

    // If the packet arrived at the loopback interface, check if any local
    // interface has the destination address assigned. This effectively lets
    // the loopback interface operate as a weak host for incoming packets.
    //
    // Note that (as of writing) the stack sends all locally destined traffic to
    // the loopback interface so we need this hack to allow the stack to accept
    // packets that arrive at the loopback interface (after being looped back)
    // but destined to an address that is assigned to another local interface.
    //
    // TODO(https://fxbug.dev/42175703): This should instead be controlled by the
    // routing table.

    // Since we treat all addresses identically, it doesn't matter whether one
    // or more than one device has the address assigned. That means we can just
    // take the first status and ignore the rest.
    let first_status = if device.is_loopback() {
        core_ctx.with_address_statuses(dst_ip, |it| it.map(|(_device, status)| status).next())
    } else {
        core_ctx.address_status_for_device(dst_ip, device).into_present()
    };
    match first_status {
        Some(
            address_status @ (Ipv4PresentAddressStatus::Unicast
            | Ipv4PresentAddressStatus::LoopbackSubnet),
        ) => {
            CounterContext::<IpCounters<Ipv4>>::counters(core_ctx).deliver_unicast.increment();
            ReceivePacketAction::Deliver {
                address_status,
                internal_forwarding: InternalForwarding::NotUsed,
            }
        }
        Some(address_status @ Ipv4PresentAddressStatus::Multicast) => {
            receive_ip_multicast_packet_action(
                core_ctx,
                bindings_ctx,
                device,
                packet,
                Some(address_status),
                dst_ip,
                frame_dst,
            )
        }
        Some(
            address_status @ (Ipv4PresentAddressStatus::LimitedBroadcast
            | Ipv4PresentAddressStatus::SubnetBroadcast),
        ) => {
            CounterContext::<IpCounters<Ipv4>>::counters(core_ctx)
                .version_rx
                .deliver_broadcast
                .increment();
            ReceivePacketAction::Deliver {
                address_status,
                internal_forwarding: InternalForwarding::NotUsed,
            }
        }
        None => receive_ip_packet_action_common::<Ipv4, _, _, _>(
            core_ctx,
            bindings_ctx,
            dst_ip,
            device,
            packet,
            frame_dst,
            marks,
        ),
    }
}

/// Computes the action to take in order to process a received IPv6 packet.
pub fn receive_ipv6_packet_action<BC, CC, B>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    packet: &Ipv6Packet<B>,
    frame_dst: Option<FrameDestination>,
    marks: &Marks,
) -> ReceivePacketAction<Ipv6, CC::DeviceId>
where
    BC: IpLayerBindingsContext<Ipv6, CC::DeviceId>,
    CC: IpLayerContext<Ipv6, BC> + CounterContext<IpCounters<Ipv6>>,
    B: SplitByteSlice,
{
    let Some(dst_ip) = SpecifiedAddr::new(packet.dst_ip()) else {
        CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).unspecified_destination.increment();
        return ReceivePacketAction::Drop { reason: DropReason::UnspecifiedDestination };
    };

    // If the packet arrived at the loopback interface, check if any local
    // interface has the destination address assigned. This effectively lets
    // the loopback interface operate as a weak host for incoming packets.
    //
    // Note that (as of writing) the stack sends all locally destined traffic to
    // the loopback interface so we need this hack to allow the stack to accept
    // packets that arrive at the loopback interface (after being looped back)
    // but destined to an address that is assigned to another local interface.
    //
    // TODO(https://fxbug.dev/42175703): This should instead be controlled by the
    // routing table.

    // It's possible that there is more than one device with the address
    // assigned. Since IPv6 addresses are either multicast or unicast, we
    // don't expect to see one device with `UnicastAssigned` or
    // `UnicastTentative` and another with `Multicast`. We might see one
    // assigned and one tentative status, though, in which case we should
    // prefer the former.
    fn choose_highest_priority(
        address_statuses: impl Iterator<Item = Ipv6PresentAddressStatus>,
        dst_ip: SpecifiedAddr<Ipv6Addr>,
    ) -> Option<Ipv6PresentAddressStatus> {
        address_statuses.max_by(|lhs, rhs| {
            use Ipv6PresentAddressStatus::*;
            match (lhs, rhs) {
                (UnicastAssigned | UnicastTentative, Multicast)
                | (Multicast, UnicastAssigned | UnicastTentative) => {
                    unreachable!("the IPv6 address {:?} is not both unicast and multicast", dst_ip)
                }
                (UnicastAssigned, UnicastTentative) => Ordering::Greater,
                (UnicastTentative, UnicastAssigned) => Ordering::Less,
                (UnicastTentative, UnicastTentative)
                | (UnicastAssigned, UnicastAssigned)
                | (Multicast, Multicast) => Ordering::Equal,
            }
        })
    }

    let highest_priority = if device.is_loopback() {
        core_ctx.with_address_statuses(dst_ip, |it| {
            let it = it.map(|(_device, status)| status);
            choose_highest_priority(it, dst_ip)
        })
    } else {
        core_ctx.address_status_for_device(dst_ip, device).into_present()
    };
    match highest_priority {
        Some(address_status @ Ipv6PresentAddressStatus::Multicast) => {
            receive_ip_multicast_packet_action(
                core_ctx,
                bindings_ctx,
                device,
                packet,
                Some(address_status),
                dst_ip,
                frame_dst,
            )
        }
        Some(address_status @ Ipv6PresentAddressStatus::UnicastAssigned) => {
            CounterContext::<IpCounters<Ipv6>>::counters(core_ctx).deliver_unicast.increment();
            ReceivePacketAction::Deliver {
                address_status,
                internal_forwarding: InternalForwarding::NotUsed,
            }
        }
        Some(Ipv6PresentAddressStatus::UnicastTentative) => {
            // If the destination address is tentative (which implies that
            // we are still performing NDP's Duplicate Address Detection on
            // it), then we don't consider the address "assigned to an
            // interface", and so we drop packets instead of delivering them
            // locally.
            //
            // As per RFC 4862 section 5.4:
            //
            //   An address on which the Duplicate Address Detection
            //   procedure is applied is said to be tentative until the
            //   procedure has completed successfully. A tentative address
            //   is not considered "assigned to an interface" in the
            //   traditional sense.  That is, the interface must accept
            //   Neighbor Solicitation and Advertisement messages containing
            //   the tentative address in the Target Address field, but
            //   processes such packets differently from those whose Target
            //   Address matches an address assigned to the interface. Other
            //   packets addressed to the tentative address should be
            //   silently discarded. Note that the "other packets" include
            //   Neighbor Solicitation and Advertisement messages that have
            //   the tentative (i.e., unicast) address as the IP destination
            //   address and contain the tentative address in the Target
            //   Address field.  Such a case should not happen in normal
            //   operation, though, since these messages are multicasted in
            //   the Duplicate Address Detection procedure.
            //
            // That is, we accept no packets destined to a tentative
            // address. NS and NA packets should be addressed to a multicast
            // address that we would have joined during DAD so that we can
            // receive those packets.
            CounterContext::<IpCounters<Ipv6>>::counters(core_ctx)
                .version_rx
                .drop_for_tentative
                .increment();
            ReceivePacketAction::Drop { reason: DropReason::Tentative }
        }
        None => receive_ip_packet_action_common::<Ipv6, _, _, _>(
            core_ctx,
            bindings_ctx,
            dst_ip,
            device,
            packet,
            frame_dst,
            marks,
        ),
    }
}

/// Computes the action to take for multicast packets on behalf of
/// [`receive_ipv4_packet_action`] and [`receive_ipv6_packet_action`].
fn receive_ip_multicast_packet_action<
    I: IpLayerIpExt,
    B: SplitByteSlice,
    BC: IpLayerBindingsContext<I, CC::DeviceId>,
    CC: IpLayerContext<I, BC> + CounterContext<IpCounters<I>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    device: &CC::DeviceId,
    packet: &I::Packet<B>,
    address_status: Option<I::AddressStatus>,
    dst_ip: SpecifiedAddr<I::Addr>,
    frame_dst: Option<FrameDestination>,
) -> ReceivePacketAction<I, CC::DeviceId> {
    let targets = multicast_forwarding::lookup_multicast_route_or_stash_packet(
        core_ctx,
        bindings_ctx,
        packet,
        device,
        frame_dst,
    );
    match (targets, address_status) {
        (Some(targets), address_status) => {
            if address_status.is_some() {
                CounterContext::<IpCounters<I>>::counters(core_ctx).deliver_multicast.increment();
            }
            ReceivePacketAction::MulticastForward { targets, address_status, dst_ip }
        }
        (None, Some(address_status)) => {
            // If the address was present on the device (e.g. the host is a
            // member of the multicast group), fallback to local delivery.
            CounterContext::<IpCounters<I>>::counters(core_ctx).deliver_multicast.increment();
            ReceivePacketAction::Deliver {
                address_status,
                internal_forwarding: InternalForwarding::NotUsed,
            }
        }
        (None, None) => {
            // As per RFC 1122 Section 3.2.2
            //   An ICMP error message MUST NOT be sent as the result of
            //   receiving:
            //   ...
            //   * a datagram destined to an IP broadcast or IP multicast
            //     address
            //
            // As such, drop the packet
            CounterContext::<IpCounters<I>>::counters(core_ctx).multicast_no_interest.increment();
            ReceivePacketAction::Drop { reason: DropReason::MulticastNoInterest }
        }
    }
}

/// Computes the remaining protocol-agnostic actions on behalf of
/// [`receive_ipv4_packet_action`] and [`receive_ipv6_packet_action`].
fn receive_ip_packet_action_common<
    I: IpLayerIpExt,
    B: SplitByteSlice,
    BC: IpLayerBindingsContext<I, CC::DeviceId>,
    CC: IpLayerContext<I, BC> + CounterContext<IpCounters<I>>,
>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    dst_ip: SpecifiedAddr<I::Addr>,
    device_id: &CC::DeviceId,
    packet: &I::Packet<B>,
    frame_dst: Option<FrameDestination>,
    marks: &Marks,
) -> ReceivePacketAction<I, CC::DeviceId> {
    if dst_ip.is_multicast() {
        return receive_ip_multicast_packet_action(
            core_ctx,
            bindings_ctx,
            device_id,
            packet,
            None,
            dst_ip,
            frame_dst,
        );
    }

    // The packet is not destined locally, so we attempt to forward it.
    if !core_ctx.is_device_unicast_forwarding_enabled(device_id) {
        // Forwarding is disabled; we are operating only as a host.
        //
        // For IPv4, per RFC 1122 Section 3.2.1.3, "A host MUST silently discard
        // an incoming datagram that is not destined for the host."
        //
        // For IPv6, per RFC 4443 Section 3.1, the only instance in which a host
        // sends an ICMPv6 Destination Unreachable message is when a packet is
        // destined to that host but on an unreachable port (Code 4 - "Port
        // unreachable"). Since the only sensible error message to send in this
        // case is a Destination Unreachable message, we interpret the RFC text
        // to mean that, consistent with IPv4's behavior, we should silently
        // discard the packet in this case.
        CounterContext::<IpCounters<I>>::counters(core_ctx).forwarding_disabled.increment();
        return ReceivePacketAction::Drop { reason: DropReason::ForwardingDisabledInboundIface };
    }
    // Per https://www.rfc-editor.org/rfc/rfc4291.html#section-2.5.2:
    //   An IPv6 packet with a source address of unspecified must never be forwarded by an IPv6
    //   router.
    // Per https://datatracker.ietf.org/doc/html/rfc1812#section-5.3.7:
    //   A router SHOULD NOT forward any packet that has an invalid IP source address or a source
    //   address on network 0
    let Some(source_address) = SpecifiedAddr::new(packet.src_ip()) else {
        return ReceivePacketAction::Drop { reason: DropReason::ForwardUnspecifiedSource };
    };

    // If forwarding is enabled, allow local delivery if the packet is destined
    // for an IP assigned to a different interface.
    //
    // This enables a weak host model when the Netstack is configured as a
    // router. Conceptually, the netstack is forwarding the packet from the
    // input device, to the destination IP's device.
    if let Some(dst_ip) = NonMappedAddr::new(dst_ip).and_then(NonMulticastAddr::new) {
        if let Some((outbound_device, address_status)) =
            get_device_with_assigned_address(core_ctx, IpDeviceAddr::new_from_witness(dst_ip))
        {
            return ReceivePacketAction::Deliver {
                address_status,
                internal_forwarding: InternalForwarding::Used(outbound_device),
            };
        }
    }

    match lookup_route_table(
        core_ctx,
        *dst_ip,
        RuleInput {
            packet_origin: PacketOrigin::NonLocal { source_address, incoming_device: device_id },
            marks,
        },
    ) {
        Some(dst) => {
            CounterContext::<IpCounters<I>>::counters(core_ctx).forward.increment();
            ReceivePacketAction::Forward { original_dst: dst_ip, dst }
        }
        None => {
            CounterContext::<IpCounters<I>>::counters(core_ctx).no_route_to_host.increment();
            ReceivePacketAction::SendNoRouteToDest { dst: dst_ip }
        }
    }
}

// Look up the route to a host.
fn lookup_route_table<I: IpLayerIpExt, CC: IpStateContext<I>>(
    core_ctx: &mut CC,
    dst_ip: I::Addr,
    rule_input: RuleInput<'_, I, CC::DeviceId>,
) -> Option<Destination<I::Addr, CC::DeviceId>> {
    let bound_device = match rule_input.packet_origin {
        PacketOrigin::Local { bound_address: _, bound_device } => bound_device,
        PacketOrigin::NonLocal { source_address: _, incoming_device: _ } => None,
    };
    core_ctx.with_rules_table(|core_ctx, rules| {
        match walk_rules(core_ctx, rules, (), &rule_input, |(), core_ctx, table| {
            match table.lookup(core_ctx, bound_device, dst_ip) {
                Some(dst) => ControlFlow::Break(Some(dst)),
                None => ControlFlow::Continue(()),
            }
        }) {
            ControlFlow::Break(RuleAction::Lookup(RuleWalkInfo {
                inner: dst,
                observed_source_address_matcher: _,
            })) => dst,
            ControlFlow::Break(RuleAction::Unreachable) => None,
            ControlFlow::Continue(RuleWalkInfo {
                inner: (),
                observed_source_address_matcher: _,
            }) => None,
        }
    })
}

/// Packed destination passed to [`IpDeviceSendContext::send_ip_frame`].
#[derive(Debug, Derivative, Clone)]
#[derivative(Eq(bound = "D: Eq"), PartialEq(bound = "D: PartialEq"))]
pub enum IpPacketDestination<I: BroadcastIpExt, D> {
    /// Broadcast packet.
    Broadcast(I::BroadcastMarker),

    /// Multicast packet to the specified IP.
    Multicast(MulticastAddr<I::Addr>),

    /// Send packet to the neighbor with the specified IP (the receiving
    /// node is either a router or the final recipient of the packet).
    Neighbor(SpecifiedAddr<I::Addr>),

    /// Loopback the packet to the specified device. Can be used only when
    /// sending to the loopback device.
    Loopback(D),
}

impl<I: BroadcastIpExt, D> IpPacketDestination<I, D> {
    /// Creates `IpPacketDestination` for IP address.
    pub fn from_addr(addr: SpecifiedAddr<I::Addr>) -> Self {
        match MulticastAddr::new(addr.into_addr()) {
            Some(mc_addr) => Self::Multicast(mc_addr),
            None => Self::Neighbor(addr),
        }
    }

    /// Create `IpPacketDestination` from `NextHop`.
    pub fn from_next_hop(next_hop: NextHop<I::Addr>, dst_ip: SpecifiedAddr<I::Addr>) -> Self {
        match next_hop {
            NextHop::RemoteAsNeighbor => Self::from_addr(dst_ip),
            NextHop::Gateway(gateway) => Self::Neighbor(gateway),
            NextHop::Broadcast(marker) => Self::Broadcast(marker),
        }
    }
}

/// The metadata associated with an outgoing IP packet.
#[derive(Debug, Clone)]
pub struct SendIpPacketMeta<I: IpExt, D, Src> {
    /// The outgoing device.
    pub device: D,

    /// The source address of the packet.
    pub src_ip: Src,

    /// The destination address of the packet.
    pub dst_ip: SpecifiedAddr<I::Addr>,

    /// The destination for the send operation.
    pub destination: IpPacketDestination<I, D>,

    /// The upper-layer protocol held in the packet's payload.
    pub proto: I::Proto,

    /// The time-to-live (IPv4) or hop limit (IPv6) for the packet.
    ///
    /// If not set, a default TTL may be used.
    pub ttl: Option<NonZeroU8>,

    /// An MTU to artificially impose on the whole IP packet.
    ///
    /// Note that the device's and discovered path MTU may still be imposed on
    /// the packet.
    pub mtu: Mtu,

    /// Traffic Class (IPv6) or Type of Service (IPv4) field for the packet.
    pub dscp_and_ecn: DscpAndEcn,
}

impl<I: IpExt, D> From<SendIpPacketMeta<I, D, SpecifiedAddr<I::Addr>>>
    for SendIpPacketMeta<I, D, Option<SpecifiedAddr<I::Addr>>>
{
    fn from(
        SendIpPacketMeta { device, src_ip, dst_ip, destination, proto, ttl, mtu, dscp_and_ecn }: SendIpPacketMeta<
            I,
            D,
            SpecifiedAddr<I::Addr>,
        >,
    ) -> SendIpPacketMeta<I, D, Option<SpecifiedAddr<I::Addr>>> {
        SendIpPacketMeta {
            device,
            src_ip: Some(src_ip),
            dst_ip,
            destination,
            proto,
            ttl,
            mtu,
            dscp_and_ecn,
        }
    }
}

/// Trait for abstracting the IP layer for locally-generated traffic.  That is,
/// traffic generated by the netstack itself (e.g. ICMP, IGMP, or MLD).
///
/// NOTE: Due to filtering rules, it is possible that the device provided in
/// `meta` will not be the device that final IP packet is actually sent from.
pub trait IpLayerHandler<I: IpExt + FragmentationIpExt, BC>: DeviceIdContext<AnyDevice> {
    /// Encapsulate and send the provided transport packet and from the device
    /// provided in `meta`.
    fn send_ip_packet_from_device<S>(
        &mut self,
        bindings_ctx: &mut BC,
        meta: SendIpPacketMeta<I, &Self::DeviceId, Option<SpecifiedAddr<I::Addr>>>,
        body: S,
    ) -> Result<(), IpSendFrameError<S>>
    where
        S: TransportPacketSerializer<I>,
        S::Buffer: BufferMut;

    /// Send an IP packet that doesn't require the encapsulation and other
    /// processing of [`send_ip_packet_from_device`] from the device specified
    /// in `meta`.
    // TODO(https://fxbug.dev/333908066): The packets going through this
    // function only hit the EGRESS filter hook, bypassing LOCAL_EGRESS.
    // Refactor callers and other functions to prevent this.
    fn send_ip_frame<S>(
        &mut self,
        bindings_ctx: &mut BC,
        device: &Self::DeviceId,
        destination: IpPacketDestination<I, &Self::DeviceId>,
        body: S,
    ) -> Result<(), IpSendFrameError<S>>
    where
        S: FragmentableIpSerializer<I, Buffer: BufferMut> + IpPacket<I>;
}

impl<
        I: IpLayerIpExt,
        BC: IpLayerBindingsContext<I, <CC as DeviceIdContext<AnyDevice>>::DeviceId>,
        CC: IpLayerEgressContext<I, BC> + IpDeviceEgressStateContext<I> + IpDeviceMtuContext<I>,
    > IpLayerHandler<I, BC> for CC
{
    fn send_ip_packet_from_device<S>(
        &mut self,
        bindings_ctx: &mut BC,
        meta: SendIpPacketMeta<I, &CC::DeviceId, Option<SpecifiedAddr<I::Addr>>>,
        body: S,
    ) -> Result<(), IpSendFrameError<S>>
    where
        S: TransportPacketSerializer<I>,
        S::Buffer: BufferMut,
    {
        send_ip_packet_from_device(self, bindings_ctx, meta, body, IpLayerPacketMetadata::default())
    }

    fn send_ip_frame<S>(
        &mut self,
        bindings_ctx: &mut BC,
        device: &Self::DeviceId,
        destination: IpPacketDestination<I, &Self::DeviceId>,
        body: S,
    ) -> Result<(), IpSendFrameError<S>>
    where
        S: FragmentableIpSerializer<I, Buffer: BufferMut> + IpPacket<I>,
    {
        send_ip_frame(
            self,
            bindings_ctx,
            device,
            destination,
            body,
            IpLayerPacketMetadata::default(),
            Mtu::no_limit(),
        )
    }
}

/// Sends an Ip packet with the specified metadata.
///
/// # Panics
///
/// Panics if either the source or destination address is the loopback address
/// and the device is a non-loopback device.
pub(crate) fn send_ip_packet_from_device<I, BC, CC, S>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    meta: SendIpPacketMeta<
        I,
        &<CC as DeviceIdContext<AnyDevice>>::DeviceId,
        Option<SpecifiedAddr<I::Addr>>,
    >,
    body: S,
    packet_metadata: IpLayerPacketMetadata<I, CC::WeakAddressId, BC>,
) -> Result<(), IpSendFrameError<S>>
where
    I: IpLayerIpExt,
    BC: FilterBindingsContext + TxMetadataBindingsTypes,
    CC: IpLayerEgressContext<I, BC> + IpDeviceEgressStateContext<I> + IpDeviceMtuContext<I>,
    S: TransportPacketSerializer<I>,
    S::Buffer: BufferMut,
{
    let SendIpPacketMeta { device, src_ip, dst_ip, destination, proto, ttl, mtu, dscp_and_ecn } =
        meta;
    let next_packet_id = gen_ip_packet_id(core_ctx);
    let ttl = ttl.unwrap_or_else(|| core_ctx.get_hop_limit(device)).get();
    let src_ip = src_ip.map_or(I::UNSPECIFIED_ADDRESS, |a| a.get());
    let mut builder = I::PacketBuilder::new(src_ip, dst_ip.get(), ttl, proto);

    #[derive(GenericOverIp)]
    #[generic_over_ip(I, Ip)]
    struct Wrap<'a, I: IpLayerIpExt> {
        builder: &'a mut I::PacketBuilder,
        next_packet_id: I::PacketId,
    }

    I::map_ip::<_, ()>(
        Wrap { builder: &mut builder, next_packet_id },
        |Wrap { builder, next_packet_id }| {
            builder.id(next_packet_id);
        },
        |Wrap { builder: _, next_packet_id: () }| {
            // IPv6 doesn't have packet IDs.
        },
    );

    builder.set_dscp_and_ecn(dscp_and_ecn);

    let ip_frame = body.encapsulate(builder);
    send_ip_frame(core_ctx, bindings_ctx, device, destination, ip_frame, packet_metadata, mtu)
        .map_err(|ser| ser.map_serializer(|s| s.into_inner()))
}

/// Abstracts access to a [`filter::FilterHandler`] for core contexts.
pub trait FilterHandlerProvider<I: packet_formats::ip::IpExt, BT: FilterBindingsTypes>:
    IpDeviceAddressIdContext<I, DeviceId: filter::InterfaceProperties<BT::DeviceClass>>
{
    /// The filter handler.
    type Handler<'a>: filter::FilterHandler<
        I,
        BT,
        DeviceId = Self::DeviceId,
        WeakAddressId = Self::WeakAddressId,
    >
    where
        Self: 'a;

    /// Gets the filter handler for this context.
    fn filter_handler(&mut self) -> Self::Handler<'_>;
}

#[cfg(any(test, feature = "testutils"))]
pub(crate) mod testutil {
    use super::*;

    use netstack3_base::testutil::{FakeCoreCtx, FakeStrongDeviceId};
    use netstack3_base::{AssignedAddrIpExt, SendFrameContext, SendFrameError, SendableFrameMeta};
    use packet::Serializer;

    /// A [`SendIpPacketMeta`] for dual stack contextx.
    #[derive(Debug, GenericOverIp)]
    #[generic_over_ip()]
    #[allow(missing_docs)]
    pub enum DualStackSendIpPacketMeta<D> {
        V4(SendIpPacketMeta<Ipv4, D, SpecifiedAddr<Ipv4Addr>>),
        V6(SendIpPacketMeta<Ipv6, D, SpecifiedAddr<Ipv6Addr>>),
    }

    impl<I: IpExt, D> From<SendIpPacketMeta<I, D, SpecifiedAddr<I::Addr>>>
        for DualStackSendIpPacketMeta<D>
    {
        fn from(value: SendIpPacketMeta<I, D, SpecifiedAddr<I::Addr>>) -> Self {
            #[derive(GenericOverIp)]
            #[generic_over_ip(I, Ip)]
            struct Wrap<I: IpExt, D>(SendIpPacketMeta<I, D, SpecifiedAddr<I::Addr>>);
            use DualStackSendIpPacketMeta::*;
            I::map_ip_in(Wrap(value), |Wrap(value)| V4(value), |Wrap(value)| V6(value))
        }
    }

    impl<I: IpExt, S, DeviceId, BC>
        SendableFrameMeta<FakeCoreCtx<S, DualStackSendIpPacketMeta<DeviceId>, DeviceId>, BC>
        for SendIpPacketMeta<I, DeviceId, SpecifiedAddr<I::Addr>>
    {
        fn send_meta<SS>(
            self,
            core_ctx: &mut FakeCoreCtx<S, DualStackSendIpPacketMeta<DeviceId>, DeviceId>,
            bindings_ctx: &mut BC,
            frame: SS,
        ) -> Result<(), SendFrameError<SS>>
        where
            SS: Serializer,
            SS::Buffer: BufferMut,
        {
            SendFrameContext::send_frame(
                &mut core_ctx.frames,
                bindings_ctx,
                DualStackSendIpPacketMeta::from(self),
                frame,
            )
        }
    }

    /// Error returned when the IP version doesn't match.
    #[derive(Debug)]
    pub struct WrongIpVersion;

    impl<D> DualStackSendIpPacketMeta<D> {
        /// Returns the internal [`SendIpPacketMeta`] if this is carrying the
        /// version matching `I`.
        pub fn try_as<I: IpExt>(
            &self,
        ) -> Result<&SendIpPacketMeta<I, D, SpecifiedAddr<I::Addr>>, WrongIpVersion> {
            #[derive(GenericOverIp)]
            #[generic_over_ip(I, Ip)]
            struct Wrap<'a, I: IpExt, D>(
                Option<&'a SendIpPacketMeta<I, D, SpecifiedAddr<I::Addr>>>,
            );
            use DualStackSendIpPacketMeta::*;
            let Wrap(dual_stack) = I::map_ip(
                self,
                |value| {
                    Wrap(match value {
                        V4(meta) => Some(meta),
                        V6(_) => None,
                    })
                },
                |value| {
                    Wrap(match value {
                        V4(_) => None,
                        V6(meta) => Some(meta),
                    })
                },
            );
            dual_stack.ok_or(WrongIpVersion)
        }
    }

    impl<I, BC, S, Meta, DeviceId> FilterHandlerProvider<I, BC> for FakeCoreCtx<S, Meta, DeviceId>
    where
        I: packet_formats::ip::IpExt + AssignedAddrIpExt,
        BC: FilterBindingsContext,
        DeviceId: FakeStrongDeviceId + filter::InterfaceProperties<BC::DeviceClass>,
    {
        type Handler<'a>
            = filter::testutil::NoopImpl<DeviceId>
        where
            Self: 'a;

        fn filter_handler(&mut self) -> Self::Handler<'_> {
            filter::testutil::NoopImpl::default()
        }
    }
}
