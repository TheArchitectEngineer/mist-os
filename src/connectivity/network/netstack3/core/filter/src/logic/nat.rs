// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Network Address Translation.

use core::fmt::Debug;
use core::num::NonZeroU16;
use core::ops::{ControlFlow, RangeInclusive};

use derivative::Derivative;
use log::{error, warn};
use net_types::ip::IpVersionMarker;
use net_types::SpecifiedAddr;
use netstack3_base::sync::Mutex;
use netstack3_base::{
    Inspectable, InspectableValue, Inspector as _, IpAddressId as _, IpDeviceAddr, WeakIpAddressId,
};
use once_cell::sync::OnceCell;
use packet_formats::ip::IpExt;
use rand::Rng as _;

use crate::conntrack::{
    CompatibleWith, Connection, ConnectionDirection, ConnectionExclusive, Table, TransportProtocol,
};
use crate::context::{FilterBindingsContext, FilterBindingsTypes, NatContext};
use crate::logic::{IngressVerdict, Interfaces, RoutineResult, Verdict};
use crate::packets::{IpPacket, MaybeTransportPacketMut as _, TransportPacketMut as _};
use crate::state::Hook;

/// The NAT configuration for a given conntrack connection.
///
/// Each type of NAT (source and destination) is configured exactly once for a
/// given connection, for the first packet encountered on that connection. This
/// is not to say that all connections are NATed: the configuration can be
/// either `ShouldNat::Yes` or `ShouldNat::No`, but the `OnceCell` containing
/// the configuration should, in most cases, be initialized by the time a
/// connection is inserted in the conntrack table.
#[derive(Derivative)]
#[derivative(Default(bound = ""), Debug(bound = "A: Debug"), PartialEq(bound = ""))]
pub struct NatConfig<I: IpExt, A> {
    destination: OnceCell<ShouldNat<I, A>>,
    source: OnceCell<ShouldNat<I, A>>,
}

/// The NAT configuration for a given type of NAT (either source or
/// destination).
#[derive(Derivative)]
#[derivative(Debug(bound = "A: Debug"), PartialEq(bound = ""))]
pub(crate) enum ShouldNat<I: IpExt, A> {
    /// NAT should be performed on this connection.
    ///
    /// Optionally contains a `CachedAddr`, which is a weak reference to the address
    /// used for NAT, if it is a dynamically assigned IP address owned by the stack.
    Yes(#[derivative(PartialEq = "ignore")] Option<CachedAddr<I, A>>),
    /// NAT should not be performed on this connection.
    No,
}

impl<I: IpExt, A: PartialEq> CompatibleWith for NatConfig<I, A> {
    fn compatible_with(&self, other: &Self) -> bool {
        // Check for both SNAT and DNAT that either NAT is configured with the same
        // value, or that it is unconfigured. When determining whether two connections
        // are compatible, if NAT is configured differently for both, they are not
        // compatible, but if NAT is either unconfigured or matches, they are considered
        // to be compatible.
        self.source.get().unwrap_or(&ShouldNat::No) == other.source.get().unwrap_or(&ShouldNat::No)
            && self.destination.get().unwrap_or(&ShouldNat::No)
                == other.destination.get().unwrap_or(&ShouldNat::No)
    }
}

impl<I: IpExt, A, BT: FilterBindingsTypes> Connection<I, NatConfig<I, A>, BT> {
    pub fn destination_nat(&self) -> bool {
        match self.external_data().destination.get() {
            Some(ShouldNat::Yes(_)) => true,
            Some(ShouldNat::No) | None => false,
        }
    }
}

impl<I: IpExt, A: InspectableValue> Inspectable for NatConfig<I, A> {
    fn record<Inspector: netstack3_base::Inspector>(&self, inspector: &mut Inspector) {
        fn record_nat_status<
            I: IpExt,
            A: InspectableValue,
            Inspector: netstack3_base::Inspector,
        >(
            inspector: &mut Inspector,
            config: &OnceCell<ShouldNat<I, A>>,
        ) {
            let status = match config.get() {
                None => "Unconfigured",
                Some(ShouldNat::No) => "No-op",
                Some(ShouldNat::Yes(cached_addr)) => {
                    if let Some(CachedAddr { id, _marker }) = cached_addr {
                        match &*id.lock() {
                            Some(id) => inspector.record_inspectable_value("To", id),
                            None => inspector.record_str("To", "InvalidAddress"),
                        }
                    }
                    "NAT"
                }
            };
            inspector.record_str("Status", status);
        }

        let Self { source, destination } = self;
        inspector.record_child("NAT", |inspector| {
            inspector
                .record_child("Destination", |inspector| record_nat_status(inspector, destination));
            inspector.record_child("Source", |inspector| record_nat_status(inspector, source));
        });
    }
}

/// A weak handle to the address assigned to a device. Allows checking for
/// validity when using the address for NAT.
#[derive(Derivative)]
#[derivative(Debug(bound = "A: Debug"))]
pub(crate) struct CachedAddr<I: IpExt, A> {
    id: Mutex<Option<A>>,
    _marker: IpVersionMarker<I>,
}

impl<I: IpExt, A> CachedAddr<I, A> {
    fn new(id: A) -> Self {
        Self { id: Mutex::new(Some(id)), _marker: IpVersionMarker::new() }
    }
}

impl<I: IpExt, A: WeakIpAddressId<I::Addr>> CachedAddr<I, A> {
    /// Check whether the provided IP address is still valid to be used for NAT.
    ///
    /// If the address is no longer valid, attempts to acquire a new handle to the
    /// same address.
    fn validate_or_replace<CC, BT>(
        &self,
        core_ctx: &mut CC,
        device: &CC::DeviceId,
        addr: I::Addr,
    ) -> Verdict
    where
        CC: NatContext<I, BT, WeakAddressId = A>,
        BT: FilterBindingsContext,
    {
        let Self { id, _marker } = self;

        // If the address ID is still valid, use the address for NAT.
        {
            if id.lock().as_ref().map(|id| id.is_assigned()).unwrap_or(false) {
                return Verdict::Accept(());
            }
        }

        // Otherwise, try to re-acquire a new handle to the same address in case it has
        // since been re-added to the device.
        //
        // NB: we release the lock around the `Option<WeakIpAddressId>` while we request
        // a new address ID and then reacquire it to assign it in order to avoid any
        // possible lock ordering issues. This does mean that there is a potential race
        // here between two packets in the same conntrack flow updating `id`, but `id`
        // should be eventually consistent with the state of the device addresses given
        // each subsequent packet will check for its validity and try to update it if
        // necessary.
        match IpDeviceAddr::new(addr).and_then(|addr| core_ctx.get_address_id(device, addr)) {
            Some(new) => {
                *id.lock() = Some(new.downgrade());
                Verdict::Accept(())
            }
            // If either the address we are using for NAT is not a valid `IpDeviceAddr` or
            // the address is no longer assigned to the device, drop the ID, both to avoid
            // further futile attempts to upgrade the weak reference and to allow the
            // backing memory to be deallocated.
            None => {
                *id.lock() = None;
                Verdict::Drop
            }
        }
    }
}

/// A type of NAT that is performed on a given conntrack connection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NatType {
    /// Destination NAT is performed.
    Destination,
    /// Source NAT is performed.
    Source,
}

pub(crate) trait NatHook<I: IpExt> {
    type Verdict<R: Debug>: FilterVerdict<R> + Debug;

    fn verdict_behavior<R: Debug>(v: Self::Verdict<R>) -> ControlFlow<Self::Verdict<()>, R>;

    const NAT_TYPE: NatType;

    /// Evaluate the result of a given routine and returning the resulting control
    /// flow.
    fn evaluate_result<P, CC, BC>(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
        conn: &mut ConnectionExclusive<I, NatConfig<I, CC::WeakAddressId>, BC>,
        packet: &P,
        interfaces: &Interfaces<'_, CC::DeviceId>,
        result: RoutineResult<I>,
    ) -> ControlFlow<Self::Verdict<NatConfigurationResult<I, CC::WeakAddressId, BC>>>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BC>,
        BC: FilterBindingsContext;

    /// Return the IP address that should be used for Redirect NAT in this NAT hook
    /// and, if applicable, a weakly-held reference to the address if it is assigned
    /// to an interface owned by the stack.
    ///
    /// # Panics
    ///
    /// Panics if called on a hook that does not support DNAT.
    fn redirect_addr<P, CC, BT>(
        core_ctx: &mut CC,
        packet: &P,
        ingress: Option<&CC::DeviceId>,
    ) -> Option<(I::Addr, Option<CachedAddr<I, CC::WeakAddressId>>)>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BT>,
        BT: FilterBindingsTypes;

    /// Extract the relevant interface for this NAT hook from `interfaces`.
    ///
    /// # Panics
    ///
    /// Panics if the required interface (ingress for ingress hooks and egress for
    /// egress hooks) is not provided to the NAT hook.
    fn interface<DeviceId>(interfaces: Interfaces<'_, DeviceId>) -> &DeviceId;
}

pub(crate) trait FilterVerdict<R>: From<Verdict<R>> {
    fn accept(&self) -> Option<&R>;
}

impl<R> FilterVerdict<R> for Verdict<R> {
    fn accept(&self) -> Option<&R> {
        match self {
            Self::Accept(result) => Some(result),
            Self::Drop => None,
        }
    }
}

impl<R> Verdict<R> {
    fn into_behavior(self) -> ControlFlow<Verdict<()>, R> {
        match self {
            Self::Accept(result) => ControlFlow::Continue(result),
            Self::Drop => ControlFlow::Break(Verdict::Drop.into()),
        }
    }
}

#[derive(Derivative)]
#[derivative(Debug(bound = "A: Debug"))]
pub(crate) enum NatConfigurationResult<I: IpExt, A, BT: FilterBindingsTypes> {
    Result(ShouldNat<I, A>),
    AdoptExisting(Connection<I, NatConfig<I, A>, BT>),
}

pub(crate) enum IngressHook {}

impl<I: IpExt> NatHook<I> for IngressHook {
    type Verdict<R: Debug> = IngressVerdict<I, R>;

    fn verdict_behavior<R: Debug>(v: Self::Verdict<R>) -> ControlFlow<Self::Verdict<()>, R> {
        v.into_behavior()
    }

    const NAT_TYPE: NatType = NatType::Destination;

    fn evaluate_result<P, CC, BC>(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
        conn: &mut ConnectionExclusive<I, NatConfig<I, CC::WeakAddressId>, BC>,
        packet: &P,
        interfaces: &Interfaces<'_, CC::DeviceId>,
        result: RoutineResult<I>,
    ) -> ControlFlow<Self::Verdict<NatConfigurationResult<I, CC::WeakAddressId, BC>>>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BC>,
        BC: FilterBindingsContext,
    {
        match result {
            RoutineResult::Accept | RoutineResult::Return => ControlFlow::Continue(()),
            RoutineResult::Drop => ControlFlow::Break(Verdict::Drop.into()),
            RoutineResult::TransparentLocalDelivery { addr, port } => {
                ControlFlow::Break(IngressVerdict::TransparentLocalDelivery { addr, port })
            }
            RoutineResult::Redirect { dst_port } => {
                ControlFlow::Break(configure_redirect_nat::<Self, _, _, _, _>(
                    core_ctx,
                    bindings_ctx,
                    table,
                    conn,
                    packet,
                    interfaces,
                    dst_port,
                ))
            }
            result @ RoutineResult::Masquerade { .. } => {
                unreachable!("SNAT not supported in INGRESS; got {result:?}")
            }
        }
    }

    fn redirect_addr<P, CC, BT>(
        core_ctx: &mut CC,
        packet: &P,
        ingress: Option<&CC::DeviceId>,
    ) -> Option<(I::Addr, Option<CachedAddr<I, CC::WeakAddressId>>)>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BT>,
        BT: FilterBindingsTypes,
    {
        let interface = ingress.expect("must have ingress interface in ingress hook");
        let addr_id = core_ctx
            .get_local_addr_for_remote(interface, SpecifiedAddr::new(packet.src_addr()))
            .or_else(|| {
                warn!(
                    "cannot redirect because there is no address assigned to the incoming \
                    interface {interface:?}; dropping packet",
                );
                None
            })?;
        let addr = addr_id.addr();
        Some((addr.addr(), Some(CachedAddr::new(addr_id.downgrade()))))
    }

    fn interface<DeviceId>(interfaces: Interfaces<'_, DeviceId>) -> &DeviceId {
        let Interfaces { ingress, egress: _ } = interfaces;
        ingress.expect("ingress interface must be provided to INGRESS hook")
    }
}

impl<I: IpExt, R> FilterVerdict<R> for IngressVerdict<I, R> {
    fn accept(&self) -> Option<&R> {
        match self {
            Self::Verdict(Verdict::Accept(result)) => Some(result),
            Self::Verdict(Verdict::Drop) | Self::TransparentLocalDelivery { .. } => None,
        }
    }
}

impl<I: IpExt, R> IngressVerdict<I, R> {
    fn into_behavior(self) -> ControlFlow<IngressVerdict<I, ()>, R> {
        match self {
            Self::Verdict(v) => match v.into_behavior() {
                ControlFlow::Continue(r) => ControlFlow::Continue(r),
                ControlFlow::Break(v) => ControlFlow::Break(v.into()),
            },
            Self::TransparentLocalDelivery { addr, port } => {
                ControlFlow::Break(IngressVerdict::TransparentLocalDelivery { addr, port })
            }
        }
    }
}

pub(crate) enum LocalEgressHook {}

impl<I: IpExt> NatHook<I> for LocalEgressHook {
    type Verdict<R: Debug> = Verdict<R>;

    fn verdict_behavior<R: Debug>(v: Self::Verdict<R>) -> ControlFlow<Self::Verdict<()>, R> {
        v.into_behavior()
    }

    const NAT_TYPE: NatType = NatType::Destination;

    fn evaluate_result<P, CC, BC>(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
        conn: &mut ConnectionExclusive<I, NatConfig<I, CC::WeakAddressId>, BC>,
        packet: &P,
        interfaces: &Interfaces<'_, CC::DeviceId>,
        result: RoutineResult<I>,
    ) -> ControlFlow<Self::Verdict<NatConfigurationResult<I, CC::WeakAddressId, BC>>>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BC>,
        BC: FilterBindingsContext,
    {
        match result {
            RoutineResult::Accept | RoutineResult::Return => ControlFlow::Continue(()),
            RoutineResult::Drop => ControlFlow::Break(Verdict::Drop.into()),
            result @ RoutineResult::TransparentLocalDelivery { .. } => {
                unreachable!(
                    "transparent local delivery is only valid in INGRESS hook; got {result:?}"
                )
            }
            result @ RoutineResult::Masquerade { .. } => {
                unreachable!("SNAT not supported in LOCAL_EGRESS; got {result:?}")
            }
            RoutineResult::Redirect { dst_port } => {
                ControlFlow::Break(configure_redirect_nat::<Self, _, _, _, _>(
                    core_ctx,
                    bindings_ctx,
                    table,
                    conn,
                    packet,
                    interfaces,
                    dst_port,
                ))
            }
        }
    }

    fn redirect_addr<P, CC, BT>(
        _: &mut CC,
        _: &P,
        _: Option<&CC::DeviceId>,
    ) -> Option<(I::Addr, Option<CachedAddr<I, CC::WeakAddressId>>)>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BT>,
        BT: FilterBindingsTypes,
    {
        Some((*I::LOOPBACK_ADDRESS, None))
    }

    fn interface<DeviceId>(interfaces: Interfaces<'_, DeviceId>) -> &DeviceId {
        let Interfaces { ingress: _, egress } = interfaces;
        egress.expect("egress interface must be provided to LOCAL_EGRESS hook")
    }
}

pub(crate) enum LocalIngressHook {}

impl<I: IpExt> NatHook<I> for LocalIngressHook {
    type Verdict<R: Debug> = Verdict<R>;

    fn verdict_behavior<R: Debug>(v: Self::Verdict<R>) -> ControlFlow<Self::Verdict<()>, R> {
        v.into_behavior()
    }

    const NAT_TYPE: NatType = NatType::Source;

    fn evaluate_result<P, CC, BC>(
        _core_ctx: &mut CC,
        _bindings_ctx: &mut BC,
        _table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
        _conn: &mut ConnectionExclusive<I, NatConfig<I, CC::WeakAddressId>, BC>,
        _packet: &P,
        _interfaces: &Interfaces<'_, CC::DeviceId>,
        result: RoutineResult<I>,
    ) -> ControlFlow<Self::Verdict<NatConfigurationResult<I, CC::WeakAddressId, BC>>>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BC>,
        BC: FilterBindingsContext,
    {
        match result {
            RoutineResult::Accept | RoutineResult::Return => ControlFlow::Continue(()),
            RoutineResult::Drop => ControlFlow::Break(Verdict::Drop.into()),
            result @ RoutineResult::Masquerade { .. } => {
                unreachable!("Masquerade not supported in LOCAL_INGRESS; got {result:?}")
            }
            result @ RoutineResult::TransparentLocalDelivery { .. } => {
                unreachable!(
                    "transparent local delivery is only valid in INGRESS hook; got {result:?}"
                )
            }
            result @ RoutineResult::Redirect { .. } => {
                unreachable!("DNAT not supported in LOCAL_INGRESS; got {result:?}")
            }
        }
    }

    fn redirect_addr<P, CC, BT>(
        _: &mut CC,
        _: &P,
        _: Option<&CC::DeviceId>,
    ) -> Option<(I::Addr, Option<CachedAddr<I, CC::WeakAddressId>>)>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BT>,
        BT: FilterBindingsTypes,
    {
        unreachable!("DNAT not supported in LOCAL_INGRESS; cannot perform redirect action")
    }

    fn interface<DeviceId>(interfaces: Interfaces<'_, DeviceId>) -> &DeviceId {
        let Interfaces { ingress, egress: _ } = interfaces;
        ingress.expect("ingress interface must be provided to LOCAL_INGRESS hook")
    }
}

pub(crate) enum EgressHook {}

impl<I: IpExt> NatHook<I> for EgressHook {
    type Verdict<R: Debug> = Verdict<R>;

    fn verdict_behavior<R: Debug>(v: Self::Verdict<R>) -> ControlFlow<Self::Verdict<()>, R> {
        v.into_behavior()
    }

    const NAT_TYPE: NatType = NatType::Source;

    fn evaluate_result<P, CC, BC>(
        core_ctx: &mut CC,
        bindings_ctx: &mut BC,
        table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
        conn: &mut ConnectionExclusive<I, NatConfig<I, CC::WeakAddressId>, BC>,
        packet: &P,
        interfaces: &Interfaces<'_, CC::DeviceId>,
        result: RoutineResult<I>,
    ) -> ControlFlow<Self::Verdict<NatConfigurationResult<I, CC::WeakAddressId, BC>>>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BC>,
        BC: FilterBindingsContext,
    {
        match result {
            RoutineResult::Accept | RoutineResult::Return => ControlFlow::Continue(()),
            RoutineResult::Drop => ControlFlow::Break(Verdict::Drop.into()),
            RoutineResult::Masquerade { src_port } => {
                ControlFlow::Break(configure_masquerade_nat::<_, _, _, _>(
                    core_ctx,
                    bindings_ctx,
                    table,
                    conn,
                    packet,
                    interfaces,
                    src_port,
                ))
            }
            result @ RoutineResult::TransparentLocalDelivery { .. } => {
                unreachable!(
                    "transparent local delivery is only valid in INGRESS hook; got {result:?}"
                )
            }
            result @ RoutineResult::Redirect { .. } => {
                unreachable!("DNAT not supported in EGRESS; got {result:?}")
            }
        }
    }

    fn redirect_addr<P, CC, BT>(
        _: &mut CC,
        _: &P,
        _: Option<&CC::DeviceId>,
    ) -> Option<(I::Addr, Option<CachedAddr<I, CC::WeakAddressId>>)>
    where
        P: IpPacket<I>,
        CC: NatContext<I, BT>,
        BT: FilterBindingsTypes,
    {
        unreachable!("DNAT not supported in EGRESS; cannot perform redirect action")
    }

    fn interface<DeviceId>(interfaces: Interfaces<'_, DeviceId>) -> &DeviceId {
        let Interfaces { ingress: _, egress } = interfaces;
        egress.expect("egress interface must be provided to EGRESS hook")
    }
}

impl<I: IpExt, A, BT: FilterBindingsTypes> Connection<I, NatConfig<I, A>, BT> {
    fn relevant_config(
        &self,
        hook_nat_type: NatType,
        direction: ConnectionDirection,
    ) -> (&OnceCell<ShouldNat<I, A>>, NatType) {
        let NatConfig { source, destination } = self.external_data();
        match (hook_nat_type, direction) {
            // If either this is a DNAT hook and we are looking at a packet in the
            // "original" direction, or this is an SNAT hook and we are looking at a reply
            // packet, then we want to decide whether to NAT based on whether the connection
            // has DNAT configured.
            (NatType::Destination, ConnectionDirection::Original)
            | (NatType::Source, ConnectionDirection::Reply) => (destination, NatType::Destination),
            // If either this is an SNAT hook and we are looking at a packet in the
            // "original" direction, or this is a DNAT hook and we are looking at a reply
            // packet, then we want to decide whether to NAT based on whether the connection
            // has SNAT configured.
            (NatType::Source, ConnectionDirection::Original)
            | (NatType::Destination, ConnectionDirection::Reply) => (source, NatType::Source),
        }
    }

    fn relevant_reply_tuple_addr(&self, nat_type: NatType) -> I::Addr {
        match nat_type {
            NatType::Destination => self.reply_tuple().src_addr,
            NatType::Source => self.reply_tuple().dst_addr,
        }
    }

    /// Gets a reference to the appropriate NAT config depending on the NAT hook we
    /// are in and the direction of the packet with respect to its conntrack
    /// connection.
    fn nat_config(
        &self,
        hook_nat_type: NatType,
        direction: ConnectionDirection,
    ) -> Option<&ShouldNat<I, A>> {
        let (config, _nat_type) = self.relevant_config(hook_nat_type, direction);
        config.get()
    }

    /// Sets the appropriate NAT config to the provided value. Which NAT config is
    /// appropriate depends on the NAT hook we are in and the direction of the
    /// packet with respect to its conntrack connection.
    ///
    /// Returns a reference to the value that was inserted, if the config had not
    /// yet been initialized; otherwise, returns the existing value of the config,
    /// along with the type of NAT it is (for diagnostic purposes).
    fn set_nat_config(
        &self,
        hook_nat_type: NatType,
        direction: ConnectionDirection,
        value: ShouldNat<I, A>,
    ) -> Result<&ShouldNat<I, A>, (ShouldNat<I, A>, NatType)> {
        let (config, nat_type) = self.relevant_config(hook_nat_type, direction);
        let mut value = Some(value);
        let config = config.get_or_init(|| value.take().unwrap());
        match value {
            None => Ok(config),
            Some(value) => Err((value, nat_type)),
        }
    }
}

/// The entry point for NAT logic from an IP layer filtering hook.
///
/// This function configures NAT, if it has not yet been configured for the
/// connection, and performs NAT on the provided packet based on the hook and
/// the connection's NAT configuration.
pub(crate) fn perform_nat<N, I, P, CC, BC>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    nat_installed: bool,
    table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
    conn: &mut Connection<I, NatConfig<I, CC::WeakAddressId>, BC>,
    direction: ConnectionDirection,
    hook: &Hook<I, BC::DeviceClass, ()>,
    packet: &mut P,
    interfaces: Interfaces<'_, CC::DeviceId>,
) -> N::Verdict<()>
where
    N: NatHook<I>,
    I: IpExt,
    P: IpPacket<I>,
    CC: NatContext<I, BC>,
    BC: FilterBindingsContext,
{
    if !nat_installed {
        return Verdict::Accept(()).into();
    }

    let nat_config = if let Some(nat) = conn.nat_config(N::NAT_TYPE, direction) {
        nat
    } else {
        // NAT has not yet been configured for this connection; traverse the installed
        // NAT routines in order to configure NAT.
        let (verdict, exclusive) = match (&mut *conn, direction) {
            (Connection::Exclusive(_), ConnectionDirection::Reply) => {
                // This is the first packet in the flow (hence the connection being exclusive),
                // yet the packet is determined to be in the "reply" direction. This means that
                // this is a self-connected flow. When a connection's original tuple is the same
                // as its reply tuple, every packet on the connection is considered to be in the
                // reply direction, which is an implementation quirk that allows self-connected
                // flows to be considered immediately "established".
                //
                // Handle this by just configuring the connection not to be NATed. It does not
                // make sense to NAT a self-connected flow since the original and reply
                // directions are indistinguishable.
                (Verdict::Accept(NatConfigurationResult::Result(ShouldNat::No)).into(), true)
            }
            (Connection::Shared(_), _) => {
                // In most scenarios, NAT should be configured for every connection before it is
                // inserted in the conntrack table, at which point it becomes a shared
                // connection. (This should apply whether or not NAT will actually be performed;
                // the configuration could be `DoNotNat`.) However, as an optimization, when no
                // NAT rules have been installed, performing NAT is skipped entirely, which can
                // result in some connections being finalized without NAT being configured for
                // them. To handle this, just don't NAT the connection.
                (Verdict::Accept(NatConfigurationResult::Result(ShouldNat::No)).into(), false)
            }
            (Connection::Exclusive(conn), ConnectionDirection::Original) => {
                let verdict = configure_nat::<N, _, _, _, _>(
                    core_ctx,
                    bindings_ctx,
                    table,
                    conn,
                    hook,
                    packet,
                    interfaces.clone(),
                );
                // Configure source port remapping for a connection by default even if its first
                // packet does not match any NAT rules, in order to ensure that source ports for
                // locally-generated traffic do not clash with ports used by existing NATed
                // connections (such as for forwarded masqueraded traffic).
                let verdict = if matches!(
                    verdict.accept(),
                    Some(&NatConfigurationResult::Result(ShouldNat::No))
                ) && N::NAT_TYPE == NatType::Source
                {
                    configure_snat_port(
                        bindings_ctx,
                        table,
                        conn,
                        None, /* src_port_range */
                        ConflictStrategy::AdoptExisting,
                    )
                    .into()
                } else {
                    verdict
                };
                (verdict, true)
            }
        };

        let result = match N::verdict_behavior(verdict) {
            ControlFlow::Break(verdict) => {
                return verdict;
            }
            ControlFlow::Continue(result) => result,
        };
        let new_nat_config = match result {
            NatConfigurationResult::Result(config) => Some(config),
            NatConfigurationResult::AdoptExisting(existing) => {
                *conn = existing;
                None
            }
        };
        if let Some(config) = new_nat_config {
            conn.set_nat_config(N::NAT_TYPE, direction, config).unwrap_or_else(
                |(value, nat_type)| {
                    // We can only assert that NAT has not been configured yet if we are configuring
                    // NAT on an exclusive connection. If the connection has already been inserted
                    // in the table and we are holding a shared reference to it, we could race with
                    // another thread also configuring NAT for the connection.
                    if exclusive {
                        unreachable!(
                            "{nat_type:?} NAT should not have been configured yet, but found \
                            {value:?}"
                        );
                    }
                    &ShouldNat::No
                },
            )
        } else {
            conn.nat_config(N::NAT_TYPE, direction).unwrap_or(&ShouldNat::No)
        }
    };

    match nat_config {
        ShouldNat::No => return Verdict::Accept(()).into(),
        // If we are NATing this connection, but are not using an IP address that is
        // dynamically assigned to an interface to do so, continue to rewrite the
        // packet.
        ShouldNat::Yes(None) => {}
        // If we are NATing to an IP address assigned to an interface, ensure that the
        // address is still valid before performing NAT.
        ShouldNat::Yes(Some(cached_addr)) => {
            // Only check the validity of the cached address if we are looking at a packet
            // in the original direction. Packets in the reply direction are rewritten based
            // on the original tuple, which is unchanged by NAT and therefore not necessary
            // to check for validity.
            if direction == ConnectionDirection::Original {
                match cached_addr.validate_or_replace(
                    core_ctx,
                    N::interface(interfaces),
                    conn.relevant_reply_tuple_addr(N::NAT_TYPE),
                ) {
                    Verdict::Accept(()) => {}
                    Verdict::Drop => return Verdict::Drop.into(),
                }
            }
        }
    }
    rewrite_packet(conn, direction, N::NAT_TYPE, packet).into()
}

/// Configure NAT by rewriting the provided reply tuple of a connection.
///
/// Evaluates the NAT routines at the provided hook and, on finding a rule that
/// matches the provided packet, configures NAT based on the rule's action. Note
/// that because NAT routines can contain a superset of the rules filter
/// routines can, it's possible for this packet to hit a non-NAT action.
fn configure_nat<N, I, P, CC, BC>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
    conn: &mut ConnectionExclusive<I, NatConfig<I, CC::WeakAddressId>, BC>,
    hook: &Hook<I, BC::DeviceClass, ()>,
    packet: &P,
    interfaces: Interfaces<'_, CC::DeviceId>,
) -> N::Verdict<NatConfigurationResult<I, CC::WeakAddressId, BC>>
where
    N: NatHook<I>,
    I: IpExt,
    P: IpPacket<I>,
    CC: NatContext<I, BC>,
    BC: FilterBindingsContext,
{
    let Hook { routines } = hook;
    for routine in routines {
        let result = super::check_routine(&routine, packet, &interfaces);
        match N::evaluate_result(core_ctx, bindings_ctx, table, conn, packet, &interfaces, result) {
            ControlFlow::Break(result) => return result,
            ControlFlow::Continue(()) => {}
        }
    }
    Verdict::Accept(NatConfigurationResult::Result(ShouldNat::No)).into()
}

/// Configure Redirect NAT, a special case of DNAT that redirects the packet to
/// the local host.
fn configure_redirect_nat<N, I, P, CC, BC>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
    conn: &mut ConnectionExclusive<I, NatConfig<I, CC::WeakAddressId>, BC>,
    packet: &P,
    interfaces: &Interfaces<'_, CC::DeviceId>,
    dst_port_range: Option<RangeInclusive<NonZeroU16>>,
) -> N::Verdict<NatConfigurationResult<I, CC::WeakAddressId, BC>>
where
    N: NatHook<I>,
    I: IpExt,
    P: IpPacket<I>,
    CC: NatContext<I, BC>,
    BC: FilterBindingsContext,
{
    match N::NAT_TYPE {
        NatType::Source => panic!("DNAT action called from SNAT-only hook"),
        NatType::Destination => {}
    }

    // Choose an appropriate new destination address and, optionally, port. Then
    // rewrite the source address/port of the reply tuple for the connection to use
    // as the guide for future packet rewriting.
    //
    // If we are in INGRESS, use the primary address of the incoming interface; if
    // we are in LOCAL_EGRESS, use the loopback address.
    let Some((addr, cached_addr)) = N::redirect_addr(core_ctx, packet, interfaces.ingress) else {
        return Verdict::Drop.into();
    };
    conn.rewrite_reply_src_addr(addr);

    let Some(range) = dst_port_range else {
        return Verdict::Accept(NatConfigurationResult::Result(ShouldNat::Yes(cached_addr))).into();
    };
    match rewrite_reply_tuple_port(
        bindings_ctx,
        table,
        conn,
        ReplyTuplePort::Source,
        range,
        true, /* ensure_port_in_range */
        ConflictStrategy::RewritePort,
    ) {
        // We are already NATing the address, so even if NATing the port is unnecessary,
        // the connection as a whole still needs to be NATed.
        Verdict::Accept(
            NatConfigurationResult::Result(ShouldNat::Yes(_))
            | NatConfigurationResult::Result(ShouldNat::No),
        ) => Verdict::Accept(NatConfigurationResult::Result(ShouldNat::Yes(cached_addr))),
        Verdict::Accept(NatConfigurationResult::AdoptExisting(_)) => {
            unreachable!("cannot adopt existing connection")
        }
        Verdict::Drop => Verdict::Drop,
    }
    .into()
}

/// Configure Masquerade NAT, a special case of SNAT that rewrites the source IP
/// address of the packet to an address that is assigned to the outgoing
/// interface.
fn configure_masquerade_nat<I, P, CC, BC>(
    core_ctx: &mut CC,
    bindings_ctx: &mut BC,
    table: &Table<I, NatConfig<I, CC::WeakAddressId>, BC>,
    conn: &mut ConnectionExclusive<I, NatConfig<I, CC::WeakAddressId>, BC>,
    packet: &P,
    interfaces: &Interfaces<'_, CC::DeviceId>,
    src_port_range: Option<RangeInclusive<NonZeroU16>>,
) -> Verdict<NatConfigurationResult<I, CC::WeakAddressId, BC>>
where
    I: IpExt,
    P: IpPacket<I>,
    CC: NatContext<I, BC>,
    BC: FilterBindingsContext,
{
    // Choose an appropriate new source address and, optionally, port. Then rewrite
    // the destination address/port of the reply tuple for the connection to use as
    // the guide for future packet rewriting.
    let interface = interfaces.egress.expect(
        "must have egress interface in EGRESS hook; Masquerade NAT is only valid in EGRESS",
    );
    let Some(addr) =
        core_ctx.get_local_addr_for_remote(interface, SpecifiedAddr::new(packet.dst_addr()))
    else {
        // TODO(https://fxbug.dev/372549231): add a counter for this scenario.
        warn!(
            "cannot masquerade because there is no address assigned to the outgoing interface \
            {interface:?}; dropping packet",
        );
        return Verdict::Drop;
    };
    conn.rewrite_reply_dst_addr(addr.addr().addr());

    // Rewrite the source port if necessary to avoid conflicting with existing
    // tracked connections.
    match configure_snat_port(
        bindings_ctx,
        table,
        conn,
        src_port_range,
        ConflictStrategy::RewritePort,
    ) {
        // We are already NATing the address, so even if NATing the port is unnecessary,
        // the connection as a whole still needs to be NATed.
        Verdict::Accept(
            NatConfigurationResult::Result(ShouldNat::Yes(_))
            | NatConfigurationResult::Result(ShouldNat::No),
        ) => Verdict::Accept(NatConfigurationResult::Result(ShouldNat::Yes(Some(
            CachedAddr::new(addr.downgrade()),
        )))),
        Verdict::Accept(NatConfigurationResult::AdoptExisting(_)) => {
            unreachable!("cannot adopt existing connection")
        }
        Verdict::Drop => Verdict::Drop,
    }
}

fn configure_snat_port<I, A, BC>(
    bindings_ctx: &mut BC,
    table: &Table<I, NatConfig<I, A>, BC>,
    conn: &mut ConnectionExclusive<I, NatConfig<I, A>, BC>,
    src_port_range: Option<RangeInclusive<NonZeroU16>>,
    conflict_strategy: ConflictStrategy,
) -> Verdict<NatConfigurationResult<I, A, BC>>
where
    I: IpExt,
    BC: FilterBindingsContext,
    A: PartialEq,
{
    // Rewrite the source port if necessary to avoid conflicting with existing
    // tracked connections. If a source port range was specified, we also ensure the
    // port is in that range; otherwise, we attempt to rewrite into a "similar"
    // range to the current value, and only if required to avoid a conflict.
    let (range, ensure_port_in_range) = if let Some(range) = src_port_range {
        (range, true)
    } else {
        let reply_tuple = conn.reply_tuple();
        let Some(range) =
            similar_port_or_id_range(reply_tuple.protocol, reply_tuple.dst_port_or_id)
        else {
            return Verdict::Drop;
        };
        (range, false)
    };
    rewrite_reply_tuple_port(
        bindings_ctx,
        table,
        conn,
        ReplyTuplePort::Destination,
        range,
        ensure_port_in_range,
        conflict_strategy,
    )
}

/// Choose a range of "similar" values to which transport-layer port or ID can
/// be rewritten -- that is, a value that is likely to be similar in terms of
/// privilege, or lack thereof.
///
/// The heuristics used in this function are chosen to roughly match those used
/// by Netstack2/gVisor and Linux.
fn similar_port_or_id_range(
    protocol: TransportProtocol,
    port_or_id: u16,
) -> Option<RangeInclusive<NonZeroU16>> {
    match protocol {
        TransportProtocol::Tcp | TransportProtocol::Udp => Some(match port_or_id {
            _ if port_or_id < 512 => NonZeroU16::MIN..=NonZeroU16::new(511).unwrap(),
            _ if port_or_id < 1024 => NonZeroU16::MIN..=NonZeroU16::new(1023).unwrap(),
            _ => NonZeroU16::new(1024).unwrap()..=NonZeroU16::MAX,
        }),
        // TODO(https://fxbug.dev/341128580): allow rewriting ICMP echo ID to zero.
        TransportProtocol::Icmp => Some(NonZeroU16::MIN..=NonZeroU16::MAX),
        TransportProtocol::Other(p) => {
            error!(
                "cannot rewrite port or ID of unsupported transport protocol {p}; dropping packet"
            );
            None
        }
    }
}

#[derive(Clone, Copy)]
enum ReplyTuplePort {
    Source,
    Destination,
}

enum ConflictStrategy {
    AdoptExisting,
    RewritePort,
}

/// Attempt to rewrite the indicated port of the reply tuple of the provided
/// connection such that it results in a unique tuple, and, if
/// `ensure_port_in_range` is `true`, also that it fits in the specified range.
fn rewrite_reply_tuple_port<I: IpExt, BC: FilterBindingsContext, A: PartialEq>(
    bindings_ctx: &mut BC,
    table: &Table<I, NatConfig<I, A>, BC>,
    conn: &mut ConnectionExclusive<I, NatConfig<I, A>, BC>,
    which_port: ReplyTuplePort,
    port_range: RangeInclusive<NonZeroU16>,
    ensure_port_in_range: bool,
    conflict_strategy: ConflictStrategy,
) -> Verdict<NatConfigurationResult<I, A, BC>> {
    // We only need to rewrite the port if the reply tuple of the connection
    // conflicts with another connection in the table, or if the port must be
    // rewritten to fall in the specified range.
    let current_port = match which_port {
        ReplyTuplePort::Source => conn.reply_tuple().src_port_or_id,
        ReplyTuplePort::Destination => conn.reply_tuple().dst_port_or_id,
    };
    let already_in_range = !ensure_port_in_range
        || NonZeroU16::new(current_port).map(|port| port_range.contains(&port)).unwrap_or(false);
    if already_in_range {
        match table.get_shared_connection(conn.reply_tuple()) {
            None => return Verdict::Accept(NatConfigurationResult::Result(ShouldNat::No)),
            Some(conflict) => match conflict_strategy {
                ConflictStrategy::AdoptExisting => {
                    // If this connection is identical to the conflicting one that's already in the
                    // table, including both its original and reply tuple and its NAT configuration,
                    // simply adopt the existing one rather than attempting port remapping.
                    if conflict.compatible_with(&*conn) {
                        return Verdict::Accept(NatConfigurationResult::AdoptExisting(
                            Connection::Shared(conflict),
                        ));
                    }
                }
                ConflictStrategy::RewritePort => {}
            },
        }
    }

    // Attempt to find a new port in the provided range that results in a unique
    // tuple. Start by selecting a random offset into the target range, and from
    // there increment the candidate port, wrapping around until all ports in
    // the range have been checked (or MAX_ATTEMPTS has been exceeded).
    //
    // As soon as we find a port that would result in a unique tuple, stop and
    // accept the packet. If we search the entire range and fail to find a port
    // that creates a unique tuple, drop the packet.
    const MAX_ATTEMPTS: u16 = 128;
    let len = port_range.end().get() - port_range.start().get() + 1;
    let mut rng = bindings_ctx.rng();
    let start = rng.gen_range(port_range.start().get()..=port_range.end().get());
    for i in 0..core::cmp::min(MAX_ATTEMPTS, len) {
        // `offset` is <= the size of `port_range`, which is a range of `NonZerou16`, so
        // `port_range.start()` + `offset` is guaranteed to fit in a `NonZeroU16`.
        let offset = (start + i) % len;
        let new_port = port_range.start().checked_add(offset).unwrap();
        match which_port {
            ReplyTuplePort::Source => conn.rewrite_reply_src_port_or_id(new_port.get()),
            ReplyTuplePort::Destination => conn.rewrite_reply_dst_port_or_id(new_port.get()),
        };
        if !table.contains_tuple(conn.reply_tuple()) {
            return Verdict::Accept(NatConfigurationResult::Result(ShouldNat::Yes(None)));
        }
    }

    Verdict::Drop
}

/// Perform NAT on a packet, using its connection in the conntrack table as a
/// guide.
fn rewrite_packet<I, P, A, BT>(
    conn: &Connection<I, NatConfig<I, A>, BT>,
    direction: ConnectionDirection,
    nat: NatType,
    packet: &mut P,
) -> Verdict
where
    I: IpExt,
    P: IpPacket<I>,
    BT: FilterBindingsTypes,
{
    // If this packet is in the "original" direction of the connection, rewrite its
    // address and port from the connection's reply tuple. If this is a reply
    // packet, rewrite it using the *original* tuple so the traffic is seen to
    // originate from or be destined to the expected peer.
    //
    // The reply tuple functions both as a way to mark what we expect to see coming
    // in, *and* a way to stash what the NAT remapping should be.
    let tuple = match direction {
        ConnectionDirection::Original => conn.reply_tuple(),
        ConnectionDirection::Reply => conn.original_tuple(),
    };
    match nat {
        NatType::Destination => {
            let (new_dst_addr, new_dst_port) = (tuple.src_addr, tuple.src_port_or_id);

            packet.set_dst_addr(new_dst_addr);
            let proto = packet.protocol();
            let mut transport = packet.transport_packet_mut();
            let Some(mut transport) = transport.transport_packet_mut() else {
                return Verdict::Accept(());
            };
            let Some(new_dst_port) = NonZeroU16::new(new_dst_port) else {
                // TODO(https://fxbug.dev/341128580): allow rewriting port to zero if allowed by
                // the transport-layer protocol.
                error!("cannot rewrite dst port to unspecified; dropping {proto} packet");
                return Verdict::Drop;
            };
            transport.set_dst_port(new_dst_port);
        }
        NatType::Source => {
            let (new_src_addr, new_src_port) = (tuple.dst_addr, tuple.dst_port_or_id);

            packet.set_src_addr(new_src_addr);
            let proto = packet.protocol();
            let mut transport = packet.transport_packet_mut();
            let Some(mut transport) = transport.transport_packet_mut() else {
                return Verdict::Accept(());
            };
            let Some(new_src_port) = NonZeroU16::new(new_src_port) else {
                // TODO(https://fxbug.dev/341128580): allow rewriting port to zero if allowed by
                // the transport-layer protocol.
                error!("cannot rewrite src port to unspecified; dropping {proto} packet");
                return Verdict::Drop;
            };
            transport.set_src_port(new_src_port);
        }
    }
    Verdict::Accept(())
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use core::marker::PhantomData;

    use assert_matches::assert_matches;
    use ip_test_macro::ip_test;
    use net_types::ip::{AddrSubnet, Ipv4};
    use netstack3_base::IntoCoreTimerCtx;
    use test_case::test_case;

    use super::*;
    use crate::conntrack::{PacketMetadata, Tuple};
    use crate::context::testutil::{
        FakeBindingsCtx, FakeDeviceClass, FakeNatCtx, FakePrimaryAddressId, FakeWeakAddressId,
    };
    use crate::matchers::testutil::{ethernet_interface, FakeDeviceId};
    use crate::matchers::PacketMatcher;
    use crate::packets::testutil::internal::{ArbitraryValue, FakeIpPacket, FakeUdpPacket};
    use crate::state::{Action, Routine, Rule, TransparentProxy};
    use crate::testutil::TestIpExt;

    impl<I: IpExt, A: PartialEq, BT: FilterBindingsTypes> PartialEq
        for NatConfigurationResult<I, A, BT>
    {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Result(lhs), Self::Result(rhs)) => lhs == rhs,
                (Self::AdoptExisting(_), Self::AdoptExisting(_)) => {
                    panic!("equality check for connections is not supported")
                }
                _ => false,
            }
        }
    }

    impl<I: IpExt, A, BC: FilterBindingsContext> ConnectionExclusive<I, NatConfig<I, A>, BC> {
        fn from_packet<P: IpPacket<I>>(bindings_ctx: &BC, packet: &P) -> Self {
            let packet = PacketMetadata::new(packet).expect("packet should be trackable");
            ConnectionExclusive::from_deconstructed_packet(bindings_ctx, &packet)
                .expect("create conntrack entry")
        }
    }

    impl<A, BC: FilterBindingsContext> ConnectionExclusive<Ipv4, NatConfig<Ipv4, A>, BC> {
        fn with_reply_tuple(bindings_ctx: &BC, which: ReplyTuplePort, port: u16) -> Self {
            Self::from_packet(bindings_ctx, &packet_with_port(which, port).reply())
        }
    }

    fn packet_with_port(which: ReplyTuplePort, port: u16) -> FakeIpPacket<Ipv4, FakeUdpPacket> {
        let mut packet = FakeIpPacket::<Ipv4, FakeUdpPacket>::arbitrary_value();
        match which {
            ReplyTuplePort::Source => packet.body.src_port = port,
            ReplyTuplePort::Destination => packet.body.dst_port = port,
        }
        packet
    }

    fn tuple_with_port(which: ReplyTuplePort, port: u16) -> Tuple<Ipv4> {
        Tuple::from_packet(&packet_with_port(which, port)).unwrap()
    }

    #[test]
    fn accept_by_default_if_no_matching_rules_in_hook() {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        assert_eq!(
            configure_nat::<LocalEgressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &Hook::default(),
                &packet,
                Interfaces { ingress: None, egress: None },
            ),
            Verdict::Accept(NatConfigurationResult::Result(ShouldNat::No))
        );
    }

    #[test]
    fn accept_by_default_if_return_from_routine() {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        let hook = Hook {
            routines: vec![Routine {
                rules: vec![Rule::new(PacketMatcher::default(), Action::Return)],
            }],
        };
        assert_eq!(
            configure_nat::<LocalEgressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &hook,
                &packet,
                Interfaces { ingress: None, egress: None },
            ),
            Verdict::Accept(NatConfigurationResult::Result(ShouldNat::No))
        );
    }

    #[test]
    fn accept_terminal_for_installed_routine() {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        // The first installed routine should terminate at its `Accept` result.
        let routine = Routine {
            rules: vec![
                // Accept all traffic.
                Rule::new(PacketMatcher::default(), Action::Accept),
                // Drop all traffic.
                Rule::new(PacketMatcher::default(), Action::Drop),
            ],
        };
        assert_eq!(
            configure_nat::<LocalEgressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &Hook { routines: vec![routine.clone()] },
                &packet,
                Interfaces { ingress: None, egress: None },
            ),
            Verdict::Accept(NatConfigurationResult::Result(ShouldNat::No))
        );

        // The first installed routine should terminate at its `Accept` result, but the
        // hook should terminate at the `Drop` result in the second routine.
        let hook = Hook {
            routines: vec![
                routine,
                Routine {
                    rules: vec![
                        // Drop all traffic.
                        Rule::new(PacketMatcher::default(), Action::Drop),
                    ],
                },
            ],
        };
        assert_eq!(
            configure_nat::<LocalEgressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &hook,
                &packet,
                Interfaces { ingress: None, egress: None },
            ),
            Verdict::Drop.into()
        );
    }

    #[test]
    fn drop_terminal_for_entire_hook() {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        let hook = Hook {
            routines: vec![
                Routine {
                    rules: vec![
                        // Drop all traffic.
                        Rule::new(PacketMatcher::default(), Action::Drop),
                    ],
                },
                Routine {
                    rules: vec![
                        // Accept all traffic.
                        Rule::new(PacketMatcher::default(), Action::Accept),
                    ],
                },
            ],
        };

        assert_eq!(
            configure_nat::<LocalEgressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &hook,
                &packet,
                Interfaces { ingress: None, egress: None },
            ),
            Verdict::Drop.into()
        );
    }

    #[test]
    fn transparent_proxy_terminal_for_entire_hook() {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        let ingress = Hook {
            routines: vec![
                Routine {
                    rules: vec![Rule::new(
                        PacketMatcher::default(),
                        Action::TransparentProxy(TransparentProxy::LocalPort(LOCAL_PORT)),
                    )],
                },
                Routine {
                    rules: vec![
                        // Accept all traffic.
                        Rule::new(PacketMatcher::default(), Action::Accept),
                    ],
                },
            ],
        };

        assert_eq!(
            configure_nat::<IngressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &ingress,
                &packet,
                Interfaces { ingress: None, egress: None },
            ),
            IngressVerdict::TransparentLocalDelivery {
                addr: <Ipv4 as crate::packets::testutil::internal::TestIpExt>::DST_IP,
                port: LOCAL_PORT
            }
        );
    }

    #[test]
    fn redirect_terminal_for_entire_hook() {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        let hook = Hook {
            routines: vec![
                Routine {
                    rules: vec![
                        // Redirect all traffic.
                        Rule::new(PacketMatcher::default(), Action::Redirect { dst_port: None }),
                    ],
                },
                Routine {
                    rules: vec![
                        // Drop all traffic.
                        Rule::new(PacketMatcher::default(), Action::Drop),
                    ],
                },
            ],
        };

        assert_eq!(
            configure_nat::<LocalEgressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &hook,
                &packet,
                Interfaces { ingress: None, egress: None },
            ),
            Verdict::Accept(NatConfigurationResult::Result(ShouldNat::Yes(None)))
        );
    }

    #[ip_test(I)]
    fn masquerade_terminal_for_entire_hook<I: TestIpExt>() {
        let mut bindings_ctx = FakeBindingsCtx::<I>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let assigned_addr = AddrSubnet::new(I::SRC_IP_2, I::SUBNET.prefix()).unwrap();
        let mut core_ctx = FakeNatCtx::new([(ethernet_interface(), assigned_addr)]);
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        let hook = Hook {
            routines: vec![
                Routine {
                    rules: vec![
                        // Masquerade all traffic.
                        Rule::new(PacketMatcher::default(), Action::Masquerade { src_port: None }),
                    ],
                },
                Routine {
                    rules: vec![
                        // Drop all traffic.
                        Rule::new(PacketMatcher::default(), Action::Drop),
                    ],
                },
            ],
        };

        assert_matches!(
            configure_nat::<EgressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &hook,
                &packet,
                Interfaces { ingress: None, egress: Some(&ethernet_interface()) },
            ),
            Verdict::Accept(NatConfigurationResult::Result(ShouldNat::Yes(Some(_))))
        );
    }

    #[test]
    fn redirect_ingress_drops_packet_if_no_assigned_address() {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        let hook = Hook {
            routines: vec![Routine {
                rules: vec![Rule::new(
                    PacketMatcher::default(),
                    Action::Redirect { dst_port: None },
                )],
            }],
        };

        assert_eq!(
            configure_nat::<IngressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &hook,
                &packet,
                Interfaces { ingress: Some(&ethernet_interface()), egress: None },
            ),
            Verdict::Drop.into()
        );
    }

    #[test]
    fn masquerade_egress_drops_packet_if_no_assigned_address() {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();
        let packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);

        let hook = Hook {
            routines: vec![Routine {
                rules: vec![Rule::new(
                    PacketMatcher::default(),
                    Action::Masquerade { src_port: None },
                )],
            }],
        };

        assert_eq!(
            configure_nat::<EgressHook, _, _, _, _>(
                &mut core_ctx,
                &mut bindings_ctx,
                &conntrack,
                &mut conn,
                &hook,
                &packet,
                Interfaces { ingress: None, egress: Some(&ethernet_interface()) },
            ),
            Verdict::Drop.into()
        );
    }

    trait NatHookExt<I: IpExt>: NatHook<I, Verdict<()>: PartialEq> {
        fn interfaces<'a>(interface: &'a FakeDeviceId) -> Interfaces<'a, FakeDeviceId>;
    }

    impl<I: IpExt> NatHookExt<I> for IngressHook {
        fn interfaces<'a>(interface: &'a FakeDeviceId) -> Interfaces<'a, FakeDeviceId> {
            Interfaces { ingress: Some(interface), egress: None }
        }
    }

    impl<I: IpExt> NatHookExt<I> for LocalEgressHook {
        fn interfaces<'a>(interface: &'a FakeDeviceId) -> Interfaces<'a, FakeDeviceId> {
            Interfaces { ingress: None, egress: Some(interface) }
        }
    }

    impl<I: IpExt> NatHookExt<I> for EgressHook {
        fn interfaces<'a>(interface: &'a FakeDeviceId) -> Interfaces<'a, FakeDeviceId> {
            Interfaces { ingress: None, egress: Some(interface) }
        }
    }

    const NAT_ENABLED_FOR_TESTS: bool = true;

    #[ip_test(I)]
    fn nat_disabled_for_self_connected_flows<I: TestIpExt>() {
        let mut bindings_ctx = FakeBindingsCtx::<I>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();

        let mut packet = FakeIpPacket::<I, _> {
            src_ip: I::SRC_IP,
            dst_ip: I::SRC_IP,
            body: FakeUdpPacket { src_port: 22222, dst_port: 22222 },
        };
        let (mut conn, direction) = conntrack
            .get_connection_for_packet_and_update(&bindings_ctx, &packet)
            .expect("packet should be valid")
            .expect("packet should be trackable");

        // Even with a Redirect NAT rule in LOCAL_EGRESS, and a Masquerade NAT rule in
        // EGRESS, DNAT and SNAT should both be disabled for the connection because it
        // is self-connected.
        let verdict = perform_nat::<LocalEgressHook, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            direction,
            &Hook {
                routines: vec![Routine {
                    rules: vec![Rule::new(
                        PacketMatcher::default(),
                        Action::Redirect { dst_port: None },
                    )],
                }],
            },
            &mut packet,
            <LocalEgressHook as NatHookExt<I>>::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());

        let verdict = perform_nat::<EgressHook, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            direction,
            &Hook {
                routines: vec![Routine {
                    rules: vec![Rule::new(
                        PacketMatcher::default(),
                        Action::Masquerade { src_port: None },
                    )],
                }],
            },
            &mut packet,
            <EgressHook as NatHookExt<I>>::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());

        assert_eq!(conn.external_data().destination.get(), Some(&ShouldNat::No));
        assert_eq!(conn.external_data().source.get(), Some(&ShouldNat::No));
    }

    #[ip_test(I)]
    fn nat_disabled_if_not_configured_before_connection_finalized<I: TestIpExt>() {
        let mut bindings_ctx = FakeBindingsCtx::<I>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::default();

        let mut packet = FakeIpPacket::<I, FakeUdpPacket>::arbitrary_value();
        let (mut conn, direction) = conntrack
            .get_connection_for_packet_and_update(&bindings_ctx, &packet)
            .expect("packet should be valid")
            .expect("packet should be trackable");

        // Skip NAT so the connection is finalized without DNAT configured for it.
        let verdict = perform_nat::<LocalEgressHook, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            false, /* nat_installed */
            &conntrack,
            &mut conn,
            direction,
            &Hook::default(),
            &mut packet,
            <LocalEgressHook as NatHookExt<I>>::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());
        assert_eq!(conn.external_data().destination.get(), None);
        assert_eq!(conn.external_data().source.get(), None);

        // Skip NAT so the connection is finalized without SNAT configured for it.
        let verdict = perform_nat::<EgressHook, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            false, /* nat_installed */
            &conntrack,
            &mut conn,
            direction,
            &Hook::default(),
            &mut packet,
            <EgressHook as NatHookExt<I>>::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());
        assert_eq!(conn.external_data().destination.get(), None);
        assert_eq!(conn.external_data().source.get(), None);

        let (inserted, _weak) = conntrack
            .finalize_connection(&mut bindings_ctx, conn)
            .expect("connection should not conflict");
        assert!(inserted);

        // Now, when a reply comes in to INGRESS, expect that SNAT will be configured as
        // `DoNotNat` given it has not already been configured for the connection.
        let mut reply = packet.reply();
        let (mut conn, direction) = conntrack
            .get_connection_for_packet_and_update(&bindings_ctx, &reply)
            .expect("packet should be valid")
            .expect("packet should be trackable");
        let verdict = perform_nat::<IngressHook, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            direction,
            &Hook::default(),
            &mut reply,
            <IngressHook as NatHookExt<I>>::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());
        assert_eq!(conn.external_data().destination.get(), None);
        assert_eq!(conn.external_data().source.get(), Some(&ShouldNat::No));

        // And finally, on LOCAL_INGRESS, DNAT should also be configured as `DoNotNat`.
        let verdict = perform_nat::<LocalIngressHook, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            direction,
            &Hook::default(),
            &mut reply,
            <IngressHook as NatHookExt<I>>::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());
        assert_eq!(conn.external_data().destination.get(), Some(&ShouldNat::No));
        assert_eq!(conn.external_data().source.get(), Some(&ShouldNat::No));
    }

    const LOCAL_PORT: NonZeroU16 = NonZeroU16::new(55555).unwrap();

    #[ip_test(I)]
    #[test_case(
        PhantomData::<IngressHook>, PhantomData::<EgressHook>, None;
        "redirect INGRESS"
    )]
    #[test_case(
        PhantomData::<IngressHook>, PhantomData::<EgressHook>, Some(LOCAL_PORT);
        "redirect INGRESS to local port"
    )]
    #[test_case(
        PhantomData::<LocalEgressHook>, PhantomData::<EgressHook>, None;
        "redirect LOCAL_EGRESS"
    )]
    #[test_case(
        PhantomData::<LocalEgressHook>, PhantomData::<EgressHook>, Some(LOCAL_PORT);
        "redirect LOCAL_EGRESS to local port"
    )]
    fn redirect<I: TestIpExt, Original: NatHookExt<I>, Reply: NatHookExt<I>>(
        _original_nat_hook: PhantomData<Original>,
        _reply_nat_hook: PhantomData<Reply>,
        dst_port: Option<NonZeroU16>,
    ) {
        let mut bindings_ctx = FakeBindingsCtx::<I>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut core_ctx = FakeNatCtx::new([(
            ethernet_interface(),
            AddrSubnet::new(I::DST_IP_2, I::SUBNET.prefix()).unwrap(),
        )]);

        // Create a packet and get the corresponding connection from conntrack.
        let mut packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let pre_nat_packet = packet.clone();
        let (mut conn, direction) = conntrack
            .get_connection_for_packet_and_update(&bindings_ctx, &packet)
            .expect("packet should be valid")
            .expect("packet should be trackable");
        let original = conn.original_tuple().clone();

        // Perform NAT at the first hook where we'd encounter this packet (either
        // INGRESS, if it's an incoming packet, or LOCAL_EGRESS, if it's an outgoing
        // packet).
        let nat_routines = Hook {
            routines: vec![Routine {
                rules: vec![Rule::new(
                    PacketMatcher::default(),
                    Action::Redirect { dst_port: dst_port.map(|port| port..=port) },
                )],
            }],
        };
        let verdict = perform_nat::<Original, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            direction,
            &nat_routines,
            &mut packet,
            Original::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());

        // The packet's destination should be rewritten, and DNAT should be configured
        // for the packet; the reply tuple's source should be rewritten to match the new
        // destination.
        let (redirect_addr, cached_addr) = Original::redirect_addr(
            &mut core_ctx,
            &packet,
            Original::interfaces(&ethernet_interface()).ingress,
        )
        .expect("get redirect addr for NAT hook");
        let expected = FakeIpPacket::<_, FakeUdpPacket> {
            src_ip: packet.src_ip,
            dst_ip: redirect_addr,
            body: FakeUdpPacket {
                src_port: packet.body.src_port,
                dst_port: dst_port.map(NonZeroU16::get).unwrap_or(packet.body.dst_port),
            },
        };
        assert_eq!(packet, expected);
        assert_eq!(
            conn.external_data().destination.get().expect("DNAT should be configured"),
            &ShouldNat::Yes(cached_addr)
        );
        assert_eq!(conn.external_data().source.get(), None, "SNAT should not be configured");
        assert_eq!(conn.original_tuple(), &original);
        let mut reply = Tuple { src_addr: redirect_addr, ..original.invert() };
        if let Some(port) = dst_port {
            reply.src_port_or_id = port.get();
        }
        assert_eq!(conn.reply_tuple(), &reply);

        // When a reply to the original packet arrives at the corresponding hook, it
        // should have reverse DNAT applied, i.e. its source should be rewritten to
        // match the original destination of the connection.
        let mut reply_packet = packet.reply();
        // Install a NAT routine that simply drops all packets. This should have no
        // effect, because only the first packet for a given connection traverses NAT
        // routines.
        let nat_routines = Hook {
            routines: vec![Routine {
                rules: vec![Rule::new(PacketMatcher::default(), Action::Drop)],
            }],
        };
        let verdict = perform_nat::<Reply, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            ConnectionDirection::Reply,
            &nat_routines,
            &mut reply_packet,
            Reply::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());
        assert_eq!(reply_packet, pre_nat_packet.reply());
    }

    #[ip_test(I)]
    #[test_case(None; "masquerade")]
    #[test_case(Some(LOCAL_PORT); "masquerade to specified port")]
    fn masquerade<I: TestIpExt>(src_port: Option<NonZeroU16>) {
        let mut bindings_ctx = FakeBindingsCtx::<I>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let assigned_addr = AddrSubnet::new(I::SRC_IP_2, I::SUBNET.prefix()).unwrap();
        let mut core_ctx = FakeNatCtx::new([(ethernet_interface(), assigned_addr)]);

        // Create a packet and get the corresponding connection from conntrack.
        let mut packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let pre_nat_packet = packet.clone();
        let (mut conn, direction) = conntrack
            .get_connection_for_packet_and_update(&bindings_ctx, &packet)
            .expect("packet should be valid")
            .expect("packet should be trackable");
        let original = conn.original_tuple().clone();

        // Perform Masquerade NAT at EGRESS.
        let nat_routines = Hook {
            routines: vec![Routine {
                rules: vec![Rule::new(
                    PacketMatcher::default(),
                    Action::Masquerade { src_port: src_port.map(|port| port..=port) },
                )],
            }],
        };
        let verdict = perform_nat::<EgressHook, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            direction,
            &nat_routines,
            &mut packet,
            Interfaces { ingress: None, egress: Some(&ethernet_interface()) },
        );
        assert_eq!(verdict, Verdict::Accept(()).into());

        // The packet's source address should be rewritten, and SNAT should be
        // configured for the packet; the reply tuple's destination should be rewritten
        // to match the new source.
        let expected = FakeIpPacket::<_, FakeUdpPacket> {
            src_ip: I::SRC_IP_2,
            dst_ip: packet.dst_ip,
            body: FakeUdpPacket {
                src_port: src_port.map(NonZeroU16::get).unwrap_or(packet.body.src_port),
                dst_port: packet.body.dst_port,
            },
        };
        assert_eq!(packet, expected);
        assert_matches!(
            conn.external_data().source.get().expect("SNAT should be configured"),
            &ShouldNat::Yes(Some(_))
        );
        assert_eq!(conn.external_data().destination.get(), None, "DNAT should not be configured");
        assert_eq!(conn.original_tuple(), &original);
        let mut reply = Tuple { dst_addr: I::SRC_IP_2, ..original.invert() };
        if let Some(port) = src_port {
            reply.dst_port_or_id = port.get();
        }
        assert_eq!(conn.reply_tuple(), &reply);

        // When a reply to the original packet arrives at INGRESS, it should have
        // reverse SNAT applied, i.e. its destination should be rewritten to match the
        // original source of the connection.
        let mut reply_packet = packet.reply();
        // Install a NAT routine that simply drops all packets. This should have no
        // effect, because only the first packet for a given connection traverses NAT
        // routines.
        let nat_routines = Hook {
            routines: vec![Routine {
                rules: vec![Rule::new(PacketMatcher::default(), Action::Drop)],
            }],
        };
        let verdict = perform_nat::<IngressHook, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            ConnectionDirection::Reply,
            &nat_routines,
            &mut reply_packet,
            Interfaces { ingress: Some(&ethernet_interface()), egress: None },
        );
        assert_eq!(verdict, Verdict::Accept(()).into());
        assert_eq!(reply_packet, pre_nat_packet.reply());
    }

    #[ip_test(I)]
    #[test_case(22, 1..=511)]
    #[test_case(853, 1..=1023)]
    #[test_case(11111, 1024..=u16::MAX)]
    fn masquerade_reply_tuple_dst_port_rewritten_even_if_target_range_unspecified<I: TestIpExt>(
        src_port: u16,
        expected_range: RangeInclusive<u16>,
    ) {
        let mut bindings_ctx = FakeBindingsCtx::<I>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let assigned_addr = AddrSubnet::new(I::SRC_IP_2, I::SUBNET.prefix()).unwrap();
        let mut core_ctx = FakeNatCtx::new([(ethernet_interface(), assigned_addr)]);
        let packet = FakeIpPacket {
            body: FakeUdpPacket { src_port, ..ArbitraryValue::arbitrary_value() },
            ..ArbitraryValue::arbitrary_value()
        };

        // First, insert a connection in conntrack with the same the source address the
        // packet will be masqueraded to, and the same source port, to cause a conflict.
        let reply = FakeIpPacket { src_ip: I::SRC_IP_2, ..packet.clone() };
        let (conn, _dir) = conntrack
            .get_connection_for_packet_and_update(&bindings_ctx, &reply)
            .expect("packet should be valid")
            .expect("packet should be trackable");
        assert_matches!(
            conntrack
                .finalize_connection(&mut bindings_ctx, conn)
                .expect("connection should not conflict"),
            (true, Some(_))
        );

        // Now, configure Masquerade NAT for a new connection that conflicts with the
        // existing one, but do not specify a port range to which the source port should
        // be rewritten.
        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);
        let verdict = configure_masquerade_nat(
            &mut core_ctx,
            &mut bindings_ctx,
            &conntrack,
            &mut conn,
            &packet,
            &Interfaces { ingress: None, egress: Some(&ethernet_interface()) },
            /* src_port */ None,
        );

        // The destination address of the reply tuple should have been rewritten to the
        // new source address, and the destination port should also have been rewritten
        // (to a "similar" value), even though a rewrite range was not specified,
        // because otherwise it would conflict with the existing connection in the
        // table.
        assert_matches!(
            verdict,
            Verdict::Accept(NatConfigurationResult::Result(ShouldNat::Yes(Some(_))))
        );
        let reply_tuple = conn.reply_tuple();
        assert_eq!(reply_tuple.dst_addr, I::SRC_IP_2);
        assert_ne!(reply_tuple.dst_port_or_id, src_port);
        assert!(expected_range.contains(&reply_tuple.dst_port_or_id));
    }

    #[ip_test(I)]
    #[test_case(
        PhantomData::<IngressHook>, Action::Redirect { dst_port: None };
        "redirect in INGRESS"
    )]
    #[test_case(
        PhantomData::<EgressHook>, Action::Masquerade { src_port: None };
        "masquerade in EGRESS"
    )]
    fn assigned_addr_cached_and_validated<I: TestIpExt, N: NatHookExt<I>>(
        _nat_hook: PhantomData<N>,
        action: Action<I, FakeDeviceClass, ()>,
    ) {
        let mut bindings_ctx = FakeBindingsCtx::<I>::new();
        let conntrack = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let assigned_addr = AddrSubnet::new(I::SRC_IP_2, I::SUBNET.prefix()).unwrap();
        let mut core_ctx = FakeNatCtx::new([(ethernet_interface(), assigned_addr)]);

        // Create a packet and get the corresponding connection from conntrack.
        let mut packet = FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value();
        let (mut conn, direction) = conntrack
            .get_connection_for_packet_and_update(&bindings_ctx, &packet)
            .expect("packet should be valid")
            .expect("packet should be trackable");

        // Perform NAT.
        let nat_routines = Hook {
            routines: vec![Routine { rules: vec![Rule::new(PacketMatcher::default(), action)] }],
        };
        let verdict = perform_nat::<N, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            direction,
            &nat_routines,
            &mut packet,
            N::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());

        // A weakly-held reference to the address assigned to the interface should be
        // cached in the NAT config.
        let (nat, _nat_type) = conn.relevant_config(N::NAT_TYPE, ConnectionDirection::Original);
        let nat = nat.get().unwrap_or_else(|| panic!("{:?} NAT should be configured", N::NAT_TYPE));
        let id = assert_matches!(nat, ShouldNat::Yes(Some(CachedAddr { id, _marker })) => id);
        let id = id
            .lock()
            .as_ref()
            .expect("address ID should be cached in NAT config")
            .upgrade()
            .expect("address ID should be valid");
        assert_eq!(*id, assigned_addr);
        drop(id);

        // Remove the assigned address; subsequent packets in the original direction on
        // the same flow should check the validity of the cached address, see that it's
        // invalid, and be dropped.
        core_ctx.device_addrs.clear();
        let verdict = perform_nat::<N, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            ConnectionDirection::Original,
            &nat_routines,
            &mut FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value(),
            N::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Drop.into());
        let (nat, _nat_type) = conn.relevant_config(N::NAT_TYPE, ConnectionDirection::Original);
        let nat = nat.get().unwrap_or_else(|| panic!("{:?} NAT should be configured", N::NAT_TYPE));
        let id = assert_matches!(nat, ShouldNat::Yes(Some(CachedAddr { id, _marker })) => id);
        assert_eq!(*id.lock(), None, "cached weak address ID should be cleared");

        // Reassign the address to the interface, and packet traversal should now
        // succeed, with NAT re-acquiring a handle to the address.
        assert_matches!(
            core_ctx
                .device_addrs
                .insert(ethernet_interface(), FakePrimaryAddressId(Arc::new(assigned_addr))),
            None
        );
        let verdict = perform_nat::<N, _, _, _, _>(
            &mut core_ctx,
            &mut bindings_ctx,
            NAT_ENABLED_FOR_TESTS,
            &conntrack,
            &mut conn,
            ConnectionDirection::Original,
            &nat_routines,
            &mut FakeIpPacket::<_, FakeUdpPacket>::arbitrary_value(),
            N::interfaces(&ethernet_interface()),
        );
        assert_eq!(verdict, Verdict::Accept(()).into());
        let (nat, _nat_type) = conn.relevant_config(N::NAT_TYPE, ConnectionDirection::Original);
        let nat = nat.get().unwrap_or_else(|| panic!("{:?} NAT should be configured", N::NAT_TYPE));
        let id = assert_matches!(nat, ShouldNat::Yes(Some(CachedAddr { id, _marker })) => id);
        let id = id
            .lock()
            .as_ref()
            .expect("address ID should be cached in NAT config")
            .upgrade()
            .expect("address Id should be valid");
        assert_eq!(*id, assigned_addr);
    }

    #[test_case(ReplyTuplePort::Source)]
    #[test_case(ReplyTuplePort::Destination)]
    fn rewrite_port_noop_if_in_range(which: ReplyTuplePort) {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let table = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut conn =
            ConnectionExclusive::with_reply_tuple(&bindings_ctx, which, LOCAL_PORT.get());

        // If the port is already in the specified range, rewriting should succeed and
        // be a no-op.
        let pre_nat = conn.reply_tuple().clone();
        let result = rewrite_reply_tuple_port(
            &mut bindings_ctx,
            &table,
            &mut conn,
            which,
            LOCAL_PORT..=LOCAL_PORT,
            true, /* ensure_port_in_range */
            ConflictStrategy::RewritePort,
        );
        assert_eq!(
            result,
            Verdict::Accept(NatConfigurationResult::Result(
                ShouldNat::<_, FakeWeakAddressId<Ipv4>>::No
            ))
        );
        assert_eq!(conn.reply_tuple(), &pre_nat);
    }

    #[test_case(ReplyTuplePort::Source)]
    #[test_case(ReplyTuplePort::Destination)]
    fn rewrite_port_noop_if_no_conflict(which: ReplyTuplePort) {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let table = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut conn =
            ConnectionExclusive::with_reply_tuple(&bindings_ctx, which, LOCAL_PORT.get());

        // If there is no conflicting tuple in the table and we provide `false` for
        // `ensure_port_in_range` (as is done for implicit SNAT), then rewriting should
        // succeed and be a no-op, even if the port is not in the specified range,
        let pre_nat = conn.reply_tuple().clone();
        const NEW_PORT: NonZeroU16 = LOCAL_PORT.checked_add(1).unwrap();
        let result = rewrite_reply_tuple_port(
            &mut bindings_ctx,
            &table,
            &mut conn,
            which,
            NEW_PORT..=NEW_PORT,
            false, /* ensure_port_in_range */
            ConflictStrategy::RewritePort,
        );
        assert_eq!(
            result,
            Verdict::Accept(NatConfigurationResult::Result(
                ShouldNat::<_, FakeWeakAddressId<Ipv4>>::No
            ))
        );
        assert_eq!(conn.reply_tuple(), &pre_nat);
    }

    #[test_case(ReplyTuplePort::Source)]
    #[test_case(ReplyTuplePort::Destination)]
    fn rewrite_port_succeeds_if_available_port_in_range(which: ReplyTuplePort) {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let table = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);
        let mut conn =
            ConnectionExclusive::with_reply_tuple(&bindings_ctx, which, LOCAL_PORT.get());

        // If the port is not in the specified range, but there is an available port,
        // rewriting should succeed.
        const NEW_PORT: NonZeroU16 = LOCAL_PORT.checked_add(1).unwrap();
        let result = rewrite_reply_tuple_port(
            &mut bindings_ctx,
            &table,
            &mut conn,
            which,
            NEW_PORT..=NEW_PORT,
            true, /* ensure_port_in_range */
            ConflictStrategy::RewritePort,
        );
        assert_eq!(
            result,
            Verdict::Accept(NatConfigurationResult::Result(
                ShouldNat::<_, FakeWeakAddressId<Ipv4>>::Yes(None)
            ))
        );
        assert_eq!(conn.reply_tuple(), &tuple_with_port(which, NEW_PORT.get()));
    }

    #[test_case(ReplyTuplePort::Source)]
    #[test_case(ReplyTuplePort::Destination)]
    fn rewrite_port_fails_if_no_available_port_in_range(which: ReplyTuplePort) {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let table: Table<_, NatConfig<_, FakeWeakAddressId<Ipv4>>, _> =
            Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);

        // If there is no port available in the specified range that does not conflict
        // with a tuple already in the table, rewriting should fail and the packet
        // should be dropped.
        let packet = packet_with_port(which, LOCAL_PORT.get());
        let (conn, _dir) = table
            .get_connection_for_packet_and_update(&bindings_ctx, &packet)
            .expect("packet should be valid")
            .expect("packet should be trackable");
        assert_matches!(
            table
                .finalize_connection(&mut bindings_ctx, conn)
                .expect("connection should not conflict"),
            (true, Some(_))
        );

        let mut conn =
            ConnectionExclusive::with_reply_tuple(&bindings_ctx, which, LOCAL_PORT.get());
        let result = rewrite_reply_tuple_port(
            &mut bindings_ctx,
            &table,
            &mut conn,
            which,
            LOCAL_PORT..=LOCAL_PORT,
            true, /* ensure_port_in_range */
            ConflictStrategy::RewritePort,
        );
        assert_eq!(result, Verdict::Drop.into());
    }

    #[test_case(ReplyTuplePort::Source)]
    #[test_case(ReplyTuplePort::Destination)]
    fn port_rewritten_to_ensure_unique_tuple_even_if_in_range(which: ReplyTuplePort) {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let table = Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);

        // Fill the conntrack table with tuples such that there is only one tuple that
        // does not conflict with an existing one and which has a port in the specified
        // range.
        const MAX_PORT: NonZeroU16 = NonZeroU16::new(LOCAL_PORT.get() + 100).unwrap();
        for port in LOCAL_PORT.get()..=MAX_PORT.get() {
            let packet = packet_with_port(which, port);
            let (conn, _dir) = table
                .get_connection_for_packet_and_update(&bindings_ctx, &packet)
                .expect("packet should be valid")
                .expect("packet should be trackable");
            assert_matches!(
                table
                    .finalize_connection(&mut bindings_ctx, conn)
                    .expect("connection should not conflict"),
                (true, Some(_))
            );
        }

        // If the port is in the specified range, but results in a non-unique tuple,
        // rewriting should succeed as long as some port exists in the range that
        // results in a unique tuple.
        let mut conn =
            ConnectionExclusive::with_reply_tuple(&bindings_ctx, which, LOCAL_PORT.get());
        const MIN_PORT: NonZeroU16 = NonZeroU16::new(LOCAL_PORT.get() - 1).unwrap();
        let result = rewrite_reply_tuple_port(
            &mut bindings_ctx,
            &table,
            &mut conn,
            which,
            MIN_PORT..=MAX_PORT,
            true, /* ensure_port_in_range */
            ConflictStrategy::RewritePort,
        );
        assert_eq!(
            result,
            Verdict::Accept(NatConfigurationResult::Result(
                ShouldNat::<_, FakeWeakAddressId<Ipv4>>::Yes(None)
            ))
        );
        assert_eq!(conn.reply_tuple(), &tuple_with_port(which, MIN_PORT.get()));
    }

    #[test_case(ReplyTuplePort::Source)]
    #[test_case(ReplyTuplePort::Destination)]
    fn rewrite_port_skipped_if_existing_connection_can_be_adopted(which: ReplyTuplePort) {
        let mut bindings_ctx = FakeBindingsCtx::<Ipv4>::new();
        let table: Table<_, NatConfig<_, FakeWeakAddressId<Ipv4>>, _> =
            Table::new::<IntoCoreTimerCtx>(&mut bindings_ctx);

        // If there is a conflicting connection already in the table, but the caller
        // specifies that existing connections can be adopted if they are identical to
        // the one we are NATing, and the one in the table is a match, the existing
        // connection should be adopted.
        let packet = packet_with_port(which, LOCAL_PORT.get());
        let (conn, _dir) = table
            .get_connection_for_packet_and_update(&bindings_ctx, &packet)
            .expect("packet should be valid")
            .expect("packet should be trackable");
        let existing = assert_matches!(
            table
                .finalize_connection(&mut bindings_ctx, conn)
                .expect("connection should not conflict"),
            (true, Some(conn)) => conn
        );

        let mut conn = ConnectionExclusive::from_packet(&bindings_ctx, &packet);
        let result = rewrite_reply_tuple_port(
            &mut bindings_ctx,
            &table,
            &mut conn,
            which,
            NonZeroU16::MIN..=NonZeroU16::MAX,
            false, /* ensure_port_in_range */
            ConflictStrategy::AdoptExisting,
        );
        let conn = assert_matches!(
            result,
            Verdict::Accept(NatConfigurationResult::AdoptExisting(Connection::Shared(conn))) => conn
        );
        assert!(Arc::ptr_eq(&existing, &conn));
    }
}
