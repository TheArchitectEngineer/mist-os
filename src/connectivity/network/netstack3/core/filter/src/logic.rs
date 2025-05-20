// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub(crate) mod nat;

use core::num::NonZeroU16;
use core::ops::RangeInclusive;

use log::error;
use net_types::ip::{GenericOverIp, Ip, IpVersionMarker};
use netstack3_base::{AnyDevice, DeviceIdContext, HandleableTimer, IpDeviceAddressIdContext};
use packet_formats::ip::IpExt;

use crate::conntrack::{Connection, FinalizeConnectionError, GetConnectionError};
use crate::context::{FilterBindingsContext, FilterBindingsTypes, FilterIpContext};
use crate::matchers::InterfaceProperties;
use crate::packets::{FilterIpExt, IpPacket, MaybeTransportPacket};
use crate::state::{
    Action, FilterIpMetadata, FilterMarkMetadata, Hook, Routine, Rule, TransparentProxy,
};

/// The final result of packet processing at a given filtering hook.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict<R = ()> {
    /// The packet should continue traversing the stack.
    Accept(R),
    /// The packet should be dropped immediately.
    Drop,
}

/// The final result of packet processing at the INGRESS hook.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IngressVerdict<I: IpExt, R = ()> {
    /// A verdict that is valid at any hook.
    Verdict(Verdict<R>),
    /// The packet should be immediately redirected to a local socket without its
    /// header being changed in any way.
    TransparentLocalDelivery {
        /// The bound address of the local socket to redirect the packet to.
        addr: I::Addr,
        /// The bound port of the local socket to redirect the packet to.
        port: NonZeroU16,
    },
}

impl<I: IpExt, R> From<Verdict<R>> for IngressVerdict<I, R> {
    fn from(verdict: Verdict<R>) -> Self {
        IngressVerdict::Verdict(verdict)
    }
}

/// A witness type to indicate that the egress filtering hook has been run.
#[derive(Debug)]
pub struct ProofOfEgressCheck {
    _private_field_to_prevent_construction_outside_of_module: (),
}

impl ProofOfEgressCheck {
    /// Clones this proof of egress check.
    ///
    /// May only be used in case of fragmentation after going through the egress
    /// hook.
    pub fn clone_for_fragmentation(&self) -> Self {
        Self { _private_field_to_prevent_construction_outside_of_module: () }
    }
}

#[derive(Clone)]
pub(crate) struct Interfaces<'a, D> {
    pub ingress: Option<&'a D>,
    pub egress: Option<&'a D>,
}

/// The result of packet processing for a given routine.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) enum RoutineResult<I: IpExt> {
    /// The packet should stop traversing the rest of the current installed
    /// routine, but continue travsering other routines installed in the hook.
    Accept,
    /// The packet should continue at the next rule in the calling chain.
    Return,
    /// The packet should be dropped immediately.
    Drop,
    /// The packet should be immediately redirected to a local socket without its
    /// header being changed in any way.
    TransparentLocalDelivery {
        /// The bound address of the local socket to redirect the packet to.
        addr: I::Addr,
        /// The bound port of the local socket to redirect the packet to.
        port: NonZeroU16,
    },
    /// Destination NAT (DNAT) should be performed to redirect the packet to the
    /// local host.
    Redirect {
        /// The optional range of destination ports used to rewrite the packet.
        ///
        /// If absent, the destination port of the packet is not rewritten.
        dst_port: Option<RangeInclusive<NonZeroU16>>,
    },
    /// Source NAT (SNAT) should be performed to rewrite the source address of the
    /// packet to one owned by the outgoing interface.
    Masquerade {
        /// The optional range of source ports used to rewrite the packet.
        ///
        /// If absent, the source port of the packet is not rewritten.
        src_port: Option<RangeInclusive<NonZeroU16>>,
    },
}

impl<I: IpExt> RoutineResult<I> {
    fn is_terminal(&self) -> bool {
        match self {
            RoutineResult::Accept
            | RoutineResult::Drop
            | RoutineResult::TransparentLocalDelivery { .. }
            | RoutineResult::Redirect { .. }
            | RoutineResult::Masquerade { .. } => true,
            RoutineResult::Return => false,
        }
    }
}

fn apply_transparent_proxy<I: IpExt, P: MaybeTransportPacket>(
    proxy: &TransparentProxy<I>,
    dst_addr: I::Addr,
    maybe_transport_packet: P,
) -> RoutineResult<I> {
    let (addr, port) = match proxy {
        TransparentProxy::LocalPort(port) => (dst_addr, *port),
        TransparentProxy::LocalAddr(addr) => {
            let Some(transport_packet_data) = maybe_transport_packet.transport_packet_data() else {
                // We ensure that TransparentProxy rules are always accompanied by a
                // TCP or UDP matcher when filtering state is provided to Core, but
                // given this invariant is enforced far from here, we log an error
                // and drop the packet, which would likely happen at the transport
                // layer anyway.
                error!(
                    "transparent proxy action is only valid on a rule that matches \
                    on transport protocol, but this packet has no transport header",
                );
                return RoutineResult::Drop;
            };
            let port = NonZeroU16::new(transport_packet_data.dst_port())
                .expect("TCP and UDP destination port is always non-zero");
            (*addr, port)
        }
        TransparentProxy::LocalAddrAndPort(addr, port) => (*addr, *port),
    };
    RoutineResult::TransparentLocalDelivery { addr, port }
}

fn check_routine<I, P, D, DeviceClass, M>(
    Routine { rules }: &Routine<I, DeviceClass, ()>,
    packet: &P,
    interfaces: &Interfaces<'_, D>,
    metadata: &mut M,
) -> RoutineResult<I>
where
    I: FilterIpExt,
    P: IpPacket<I>,
    D: InterfaceProperties<DeviceClass>,
    M: FilterMarkMetadata,
{
    for Rule { matcher, action, validation_info: () } in rules {
        if matcher.matches(packet, &interfaces) {
            match action {
                Action::Accept => return RoutineResult::Accept,
                Action::Return => return RoutineResult::Return,
                Action::Drop => return RoutineResult::Drop,
                // TODO(https://fxbug.dev/332739892): enforce some kind of maximum depth on the
                // routine graph to prevent a stack overflow here.
                Action::Jump(target) => {
                    let result = check_routine(target.get(), packet, interfaces, metadata);
                    if result.is_terminal() {
                        return result;
                    }
                    continue;
                }
                Action::TransparentProxy(proxy) => {
                    return apply_transparent_proxy(
                        proxy,
                        packet.dst_addr(),
                        packet.maybe_transport_packet(),
                    );
                }
                Action::Redirect { dst_port } => {
                    return RoutineResult::Redirect { dst_port: dst_port.clone() }
                }
                Action::Masquerade { src_port } => {
                    return RoutineResult::Masquerade { src_port: src_port.clone() }
                }
                Action::Mark { domain, action } => {
                    // Mark is a non-terminating action, it will not yield a `RoutineResult` but
                    // it will continue on processing the next rule in the routine.
                    metadata.apply_mark_action(*domain, *action);
                }
            }
        }
    }
    RoutineResult::Return
}

fn check_routines_for_hook<I, P, D, DeviceClass, M>(
    hook: &Hook<I, DeviceClass, ()>,
    packet: &P,
    interfaces: Interfaces<'_, D>,
    metadata: &mut M,
) -> Verdict
where
    I: FilterIpExt,
    P: IpPacket<I>,
    D: InterfaceProperties<DeviceClass>,
    M: FilterMarkMetadata,
{
    let Hook { routines } = hook;
    for routine in routines {
        match check_routine(&routine, packet, &interfaces, metadata) {
            RoutineResult::Accept | RoutineResult::Return => {}
            RoutineResult::Drop => return Verdict::Drop,
            result @ RoutineResult::TransparentLocalDelivery { .. } => {
                unreachable!(
                    "transparent local delivery is only valid in INGRESS hook; got {result:?}"
                )
            }
            result @ (RoutineResult::Redirect { .. } | RoutineResult::Masquerade { .. }) => {
                unreachable!("NAT actions are only valid in NAT routines; got {result:?}")
            }
        }
    }
    Verdict::Accept(())
}

fn check_routines_for_ingress<I, P, D, DeviceClass, M>(
    hook: &Hook<I, DeviceClass, ()>,
    packet: &P,
    interfaces: Interfaces<'_, D>,
    metadata: &mut M,
) -> IngressVerdict<I>
where
    I: FilterIpExt,
    P: IpPacket<I>,
    D: InterfaceProperties<DeviceClass>,
    M: FilterMarkMetadata,
{
    let Hook { routines } = hook;
    for routine in routines {
        match check_routine(&routine, packet, &interfaces, metadata) {
            RoutineResult::Accept | RoutineResult::Return => {}
            RoutineResult::Drop => return Verdict::Drop.into(),
            RoutineResult::TransparentLocalDelivery { addr, port } => {
                return IngressVerdict::TransparentLocalDelivery { addr, port };
            }
            result @ (RoutineResult::Redirect { .. } | RoutineResult::Masquerade { .. }) => {
                unreachable!("NAT actions are only valid in NAT routines; got {result:?}")
            }
        }
    }
    Verdict::Accept(()).into()
}

/// An implementation of packet filtering logic, providing entry points at
/// various stages of packet processing.
pub trait FilterHandler<I: FilterIpExt, BC: FilterBindingsTypes>:
    IpDeviceAddressIdContext<I, DeviceId: InterfaceProperties<BC::DeviceClass>>
{
    /// The ingress hook intercepts incoming traffic before a routing decision
    /// has been made.
    fn ingress_hook<P, M>(
        &mut self,
        bindings_ctx: &mut BC,
        packet: &mut P,
        interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> IngressVerdict<I>
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>;

    /// The local ingress hook intercepts incoming traffic that is destined for
    /// the local host.
    fn local_ingress_hook<P, M>(
        &mut self,
        bindings_ctx: &mut BC,
        packet: &mut P,
        interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> Verdict
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>;

    /// The forwarding hook intercepts incoming traffic that is destined for
    /// another host.
    fn forwarding_hook<P, M>(
        &mut self,
        packet: &mut P,
        in_interface: &Self::DeviceId,
        out_interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> Verdict
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>;

    /// The local egress hook intercepts locally-generated traffic before a
    /// routing decision has been made.
    fn local_egress_hook<P, M>(
        &mut self,
        bindings_ctx: &mut BC,
        packet: &mut P,
        interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> Verdict
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>;

    /// The egress hook intercepts all outgoing traffic after a routing decision
    /// has been made.
    fn egress_hook<P, M>(
        &mut self,
        bindings_ctx: &mut BC,
        packet: &mut P,
        interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> (Verdict, ProofOfEgressCheck)
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>;
}

/// The "production" implementation of packet filtering.
///
/// Provides an implementation of [`FilterHandler`] for any `CC` that implements
/// [`FilterIpContext`].
pub struct FilterImpl<'a, CC>(pub &'a mut CC);

impl<CC: DeviceIdContext<AnyDevice>> DeviceIdContext<AnyDevice> for FilterImpl<'_, CC> {
    type DeviceId = CC::DeviceId;
    type WeakDeviceId = CC::WeakDeviceId;
}

impl<I, CC> IpDeviceAddressIdContext<I> for FilterImpl<'_, CC>
where
    I: FilterIpExt,
    CC: IpDeviceAddressIdContext<I>,
{
    type AddressId = CC::AddressId;
    type WeakAddressId = CC::WeakAddressId;
}

impl<I, BC, CC> FilterHandler<I, BC> for FilterImpl<'_, CC>
where
    I: FilterIpExt,
    BC: FilterBindingsContext,
    CC: FilterIpContext<I, BC>,
{
    fn ingress_hook<P, M>(
        &mut self,
        bindings_ctx: &mut BC,
        packet: &mut P,
        interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> IngressVerdict<I>
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
    {
        let Self(this) = self;
        this.with_filter_state_and_nat_ctx(|state, core_ctx| {
            // There usually isn't going to be an existing connection in the metadata before
            // this hook, but it's possible in the case of looped-back packets, so check for
            // one first before looking in the conntrack table.
            let conn = match metadata.take_connection_and_direction() {
                Some((c, d)) => Some((c, d)),
                None => {
                    packet.conntrack_packet().and_then(|packet| {
                        match state
                            .conntrack
                            .get_connection_for_packet_and_update(bindings_ctx, packet)
                        {
                            Ok(result) => result,
                            // TODO(https://fxbug.dev/328064909): Support configurable dropping of
                            // invalid packets.
                            Err(GetConnectionError::InvalidPacket(c, d)) => Some((c, d)),
                        }
                    })
                }
            };

            let mut verdict = match check_routines_for_ingress(
                &state.installed_routines.get().ip.ingress,
                packet,
                Interfaces { ingress: Some(interface), egress: None },
                metadata,
            ) {
                v @ IngressVerdict::Verdict(Verdict::Drop) => return v,
                v @ IngressVerdict::Verdict(Verdict::Accept(()))
                | v @ IngressVerdict::TransparentLocalDelivery { .. } => v,
            };

            if let Some((mut conn, direction)) = conn {
                // TODO(https://fxbug.dev/343683914): provide a way to run filter routines
                // post-NAT, but in the same hook. Currently all filter routines are run before
                // all NAT routines in the same hook.
                match nat::perform_nat::<nat::IngressHook, _, _, _, _>(
                    core_ctx,
                    bindings_ctx,
                    state.nat_installed.get(),
                    &state.conntrack,
                    &mut conn,
                    direction,
                    &state.installed_routines.get().nat.ingress,
                    packet,
                    Interfaces { ingress: Some(interface), egress: None },
                ) {
                    // NB: we only overwrite the verdict returned from the IP routines if it is
                    // `TransparentLocalDelivery`; in case of an `Accept` verdict from the NAT
                    // routines, we do not change the existing verdict.
                    v @ IngressVerdict::Verdict(Verdict::Drop) => return v,
                    IngressVerdict::Verdict(Verdict::Accept(())) => {}
                    v @ IngressVerdict::TransparentLocalDelivery { .. } => {
                        verdict = v;
                    }
                }

                let res = metadata.replace_connection_and_direction(conn, direction);
                debug_assert!(res.is_none());
            }

            verdict
        })
    }

    fn local_ingress_hook<P, M>(
        &mut self,
        bindings_ctx: &mut BC,
        packet: &mut P,
        interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> Verdict
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
    {
        let Self(this) = self;
        this.with_filter_state_and_nat_ctx(|state, core_ctx| {
            let conn = match metadata.take_connection_and_direction() {
                Some((c, d)) => Some((c, d)),
                // It's possible that there won't be a connection in the metadata by this point;
                // this could be, for example, because the packet is for a protocol not tracked
                // by conntrack.
                None => packet.conntrack_packet().and_then(|packet| {
                    match state.conntrack.get_connection_for_packet_and_update(bindings_ctx, packet)
                    {
                        Ok(result) => result,
                        // TODO(https://fxbug.dev/328064909): Support configurable dropping of
                        // invalid packets.
                        Err(GetConnectionError::InvalidPacket(c, d)) => Some((c, d)),
                    }
                }),
            };

            let verdict = match check_routines_for_hook(
                &state.installed_routines.get().ip.local_ingress,
                packet,
                Interfaces { ingress: Some(interface), egress: None },
                metadata,
            ) {
                Verdict::Drop => return Verdict::Drop,
                Verdict::Accept(()) => Verdict::Accept(()),
            };

            if let Some((mut conn, direction)) = conn {
                // TODO(https://fxbug.dev/343683914): provide a way to run filter routines
                // post-NAT, but in the same hook. Currently all filter routines are run before
                // all NAT routines in the same hook.
                match nat::perform_nat::<nat::LocalIngressHook, _, _, _, _>(
                    core_ctx,
                    bindings_ctx,
                    state.nat_installed.get(),
                    &state.conntrack,
                    &mut conn,
                    direction,
                    &state.installed_routines.get().nat.local_ingress,
                    packet,
                    Interfaces { ingress: Some(interface), egress: None },
                ) {
                    Verdict::Drop => return Verdict::Drop,
                    Verdict::Accept(()) => {}
                }

                match state.conntrack.finalize_connection(bindings_ctx, conn) {
                    Ok((_inserted, _weak_conn)) => {}
                    // If finalizing the connection would result in a conflict in the connection
                    // tracking table, or if the table is at capacity, drop the packet.
                    Err(FinalizeConnectionError::Conflict | FinalizeConnectionError::TableFull) => {
                        return Verdict::Drop;
                    }
                }
            }

            verdict
        })
    }

    fn forwarding_hook<P, M>(
        &mut self,
        packet: &mut P,
        in_interface: &Self::DeviceId,
        out_interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> Verdict
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
    {
        let Self(this) = self;
        this.with_filter_state(|state| {
            check_routines_for_hook(
                &state.installed_routines.get().ip.forwarding,
                packet,
                Interfaces { ingress: Some(in_interface), egress: Some(out_interface) },
                metadata,
            )
        })
    }

    fn local_egress_hook<P, M>(
        &mut self,
        bindings_ctx: &mut BC,
        packet: &mut P,
        interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> Verdict
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
    {
        let Self(this) = self;
        this.with_filter_state_and_nat_ctx(|state, core_ctx| {
            // There isn't going to be an existing connection in the metadata
            // before this hook, so we don't have to look.
            let conn = packet.conntrack_packet().and_then(|packet| {
                match state.conntrack.get_connection_for_packet_and_update(bindings_ctx, packet) {
                    Ok(result) => result,
                    // TODO(https://fxbug.dev/328064909): Support configurable dropping of invalid
                    // packets.
                    Err(GetConnectionError::InvalidPacket(c, d)) => Some((c, d)),
                }
            });

            let verdict = match check_routines_for_hook(
                &state.installed_routines.get().ip.local_egress,
                packet,
                Interfaces { ingress: None, egress: Some(interface) },
                metadata,
            ) {
                Verdict::Drop => return Verdict::Drop,
                Verdict::Accept(()) => Verdict::Accept(()),
            };

            if let Some((mut conn, direction)) = conn {
                // TODO(https://fxbug.dev/343683914): provide a way to run filter routines
                // post-NAT, but in the same hook. Currently all filter routines are run before
                // all NAT routines in the same hook.
                match nat::perform_nat::<nat::LocalEgressHook, _, _, _, _>(
                    core_ctx,
                    bindings_ctx,
                    state.nat_installed.get(),
                    &state.conntrack,
                    &mut conn,
                    direction,
                    &state.installed_routines.get().nat.local_egress,
                    packet,
                    Interfaces { ingress: None, egress: Some(interface) },
                ) {
                    Verdict::Drop => return Verdict::Drop,
                    Verdict::Accept(()) => {}
                }

                let res = metadata.replace_connection_and_direction(conn, direction);
                debug_assert!(res.is_none());
            }

            verdict
        })
    }

    fn egress_hook<P, M>(
        &mut self,
        bindings_ctx: &mut BC,
        packet: &mut P,
        interface: &Self::DeviceId,
        metadata: &mut M,
    ) -> (Verdict, ProofOfEgressCheck)
    where
        P: IpPacket<I>,
        M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
    {
        let Self(this) = self;
        let verdict = this.with_filter_state_and_nat_ctx(|state, core_ctx| {
            let conn = match metadata.take_connection_and_direction() {
                Some((c, d)) => Some((c, d)),
                // It's possible that there won't be a connection in the metadata by this point;
                // this could be, for example, because the packet is for a protocol not tracked
                // by conntrack.
                None => packet.conntrack_packet().and_then(|packet| {
                    match state.conntrack.get_connection_for_packet_and_update(bindings_ctx, packet)
                    {
                        Ok(result) => result,
                        // TODO(https://fxbug.dev/328064909): Support configurable dropping of
                        // invalid packets.
                        Err(GetConnectionError::InvalidPacket(c, d)) => Some((c, d)),
                    }
                }),
            };

            let verdict = match check_routines_for_hook(
                &state.installed_routines.get().ip.egress,
                packet,
                Interfaces { ingress: None, egress: Some(interface) },
                metadata,
            ) {
                Verdict::Drop => return Verdict::Drop,
                Verdict::Accept(()) => Verdict::Accept(()),
            };

            if let Some((mut conn, direction)) = conn {
                // TODO(https://fxbug.dev/343683914): provide a way to run filter routines
                // post-NAT, but in the same hook. Currently all filter routines are run before
                // all NAT routines in the same hook.
                match nat::perform_nat::<nat::EgressHook, _, _, _, _>(
                    core_ctx,
                    bindings_ctx,
                    state.nat_installed.get(),
                    &state.conntrack,
                    &mut conn,
                    direction,
                    &state.installed_routines.get().nat.egress,
                    packet,
                    Interfaces { ingress: None, egress: Some(interface) },
                ) {
                    Verdict::Drop => return Verdict::Drop,
                    Verdict::Accept(()) => {}
                }

                match state.conntrack.finalize_connection(bindings_ctx, conn) {
                    Ok((_inserted, conn)) => {
                        if let Some(conn) = conn {
                            let res = metadata.replace_connection_and_direction(
                                Connection::Shared(conn),
                                direction,
                            );
                            debug_assert!(res.is_none());
                        }
                    }
                    // If finalizing the connection would result in a conflict in the connection
                    // tracking table, or if the table is at capacity, drop the packet.
                    Err(FinalizeConnectionError::Conflict | FinalizeConnectionError::TableFull) => {
                        return Verdict::Drop;
                    }
                }
            }

            verdict
        });
        (
            verdict,
            ProofOfEgressCheck { _private_field_to_prevent_construction_outside_of_module: () },
        )
    }
}

/// A timer ID for the filtering crate.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, GenericOverIp, Hash)]
#[generic_over_ip(I, Ip)]
pub enum FilterTimerId<I: Ip> {
    /// A trigger for the conntrack module to perform garbage collection.
    ConntrackGc(IpVersionMarker<I>),
}

impl<I: FilterIpExt, BC: FilterBindingsContext, CC: FilterIpContext<I, BC>> HandleableTimer<CC, BC>
    for FilterTimerId<I>
{
    fn handle(self, core_ctx: &mut CC, bindings_ctx: &mut BC, _: BC::UniqueTimerId) {
        match self {
            FilterTimerId::ConntrackGc(_) => core_ctx.with_filter_state(|state| {
                state.conntrack.perform_gc(bindings_ctx);
            }),
        }
    }
}

#[cfg(any(test, feature = "testutils"))]
pub mod testutil {
    use core::marker::PhantomData;

    use net_types::ip::AddrSubnet;
    use netstack3_base::testutil::{FakeStrongDeviceId, FakeWeakAddressId, FakeWeakDeviceId};
    use netstack3_base::AssignedAddrIpExt;

    use super::*;

    /// A no-op implementation of packet filtering that accepts any packet that
    /// passes through it, useful for unit tests of other modules where trait bounds
    /// require that a `FilterHandler` is available but no filtering logic is under
    /// test.
    ///
    /// Provides an implementation of [`FilterHandler`].
    pub struct NoopImpl<DeviceId>(PhantomData<DeviceId>);

    impl<DeviceId> Default for NoopImpl<DeviceId> {
        fn default() -> Self {
            Self(PhantomData)
        }
    }

    impl<DeviceId: FakeStrongDeviceId> DeviceIdContext<AnyDevice> for NoopImpl<DeviceId> {
        type DeviceId = DeviceId;
        type WeakDeviceId = FakeWeakDeviceId<DeviceId>;
    }

    impl<I: AssignedAddrIpExt, DeviceId: FakeStrongDeviceId> IpDeviceAddressIdContext<I>
        for NoopImpl<DeviceId>
    {
        type AddressId = AddrSubnet<I::Addr, I::AssignedWitness>;
        type WeakAddressId = FakeWeakAddressId<Self::AddressId>;
    }

    impl<I, BC, DeviceId> FilterHandler<I, BC> for NoopImpl<DeviceId>
    where
        I: FilterIpExt + AssignedAddrIpExt,
        BC: FilterBindingsContext,
        DeviceId: FakeStrongDeviceId + InterfaceProperties<BC::DeviceClass>,
    {
        fn ingress_hook<P, M>(
            &mut self,
            _: &mut BC,
            _: &mut P,
            _: &Self::DeviceId,
            _: &mut M,
        ) -> IngressVerdict<I>
        where
            P: IpPacket<I>,
            M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
        {
            Verdict::Accept(()).into()
        }

        fn local_ingress_hook<P, M>(
            &mut self,
            _: &mut BC,
            _: &mut P,
            _: &Self::DeviceId,
            _: &mut M,
        ) -> Verdict
        where
            P: IpPacket<I>,
            M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
        {
            Verdict::Accept(())
        }

        fn forwarding_hook<P, M>(
            &mut self,
            _: &mut P,
            _: &Self::DeviceId,
            _: &Self::DeviceId,
            _: &mut M,
        ) -> Verdict
        where
            P: IpPacket<I>,
            M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
        {
            Verdict::Accept(())
        }

        fn local_egress_hook<P, M>(
            &mut self,
            _: &mut BC,
            _: &mut P,
            _: &Self::DeviceId,
            _: &mut M,
        ) -> Verdict
        where
            P: IpPacket<I>,
            M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
        {
            Verdict::Accept(())
        }

        fn egress_hook<P, M>(
            &mut self,
            _: &mut BC,
            _: &mut P,
            _: &Self::DeviceId,
            _: &mut M,
        ) -> (Verdict, ProofOfEgressCheck)
        where
            P: IpPacket<I>,
            M: FilterIpMetadata<I, Self::WeakAddressId, BC>,
        {
            (Verdict::Accept(()), ProofOfEgressCheck::forge_proof_for_test())
        }
    }

    impl ProofOfEgressCheck {
        /// For tests where it's not feasible to run the egress hook.
        pub(crate) fn forge_proof_for_test() -> Self {
            ProofOfEgressCheck { _private_field_to_prevent_construction_outside_of_module: () }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::HashMap;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;

    use assert_matches::assert_matches;
    use derivative::Derivative;
    use ip_test_macro::ip_test;
    use net_types::ip::{AddrSubnet, Ipv4};
    use netstack3_base::{AssignedAddrIpExt, MarkDomain, Marks, SegmentHeader};
    use test_case::test_case;

    use super::*;
    use crate::actions::MarkAction;
    use crate::conntrack::{self, ConnectionDirection};
    use crate::context::testutil::{FakeBindingsCtx, FakeCtx, FakeDeviceClass, FakeWeakAddressId};
    use crate::logic::nat::NatConfig;
    use crate::matchers::testutil::{ethernet_interface, wlan_interface, FakeDeviceId};
    use crate::matchers::{
        AddressMatcher, AddressMatcherType, InterfaceMatcher, PacketMatcher, PortMatcher,
        TransportProtocolMatcher,
    };
    use crate::packets::testutil::internal::{
        ArbitraryValue, FakeIpPacket, FakeTcpSegment, FakeUdpPacket, TransportPacketExt,
    };
    use crate::state::{IpRoutines, NatRoutines, UninstalledRoutine};
    use crate::testutil::TestIpExt;

    impl<I: IpExt> Rule<I, FakeDeviceClass, ()> {
        pub(crate) fn new(
            matcher: PacketMatcher<I, FakeDeviceClass>,
            action: Action<I, FakeDeviceClass, ()>,
        ) -> Self {
            Rule { matcher, action, validation_info: () }
        }
    }

    #[test]
    fn return_by_default_if_no_matching_rules_in_routine() {
        assert_eq!(
            check_routine::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &Routine { rules: Vec::new() },
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                &Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            RoutineResult::Return
        );

        // A subroutine should also yield `Return` if no rules match, allowing
        // the calling routine to continue execution after the `Jump`.
        let routine = Routine {
            rules: vec![
                Rule::new(
                    PacketMatcher::default(),
                    Action::Jump(UninstalledRoutine::new(Vec::new(), 0)),
                ),
                Rule::new(PacketMatcher::default(), Action::Drop),
            ],
        };
        assert_eq!(
            check_routine::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &routine,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                &Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            RoutineResult::Drop
        );
    }

    struct NullMetadata {}

    impl<I: IpExt, A, BT: FilterBindingsTypes> FilterIpMetadata<I, A, BT> for NullMetadata {
        fn take_connection_and_direction(
            &mut self,
        ) -> Option<(Connection<I, NatConfig<I, A>, BT>, ConnectionDirection)> {
            None
        }

        fn replace_connection_and_direction(
            &mut self,
            _conn: Connection<I, NatConfig<I, A>, BT>,
            _direction: ConnectionDirection,
        ) -> Option<Connection<I, NatConfig<I, A>, BT>> {
            None
        }
    }

    impl FilterMarkMetadata for NullMetadata {
        fn apply_mark_action(&mut self, _domain: MarkDomain, _action: MarkAction) {}
    }

    #[derive(Derivative)]
    #[derivative(Default(bound = ""))]
    struct PacketMetadata<I: IpExt + AssignedAddrIpExt, A, BT: FilterBindingsTypes> {
        conn: Option<(Connection<I, NatConfig<I, A>, BT>, ConnectionDirection)>,
        marks: Marks,
    }

    impl<I: TestIpExt, A, BT: FilterBindingsTypes> FilterIpMetadata<I, A, BT>
        for PacketMetadata<I, A, BT>
    {
        fn take_connection_and_direction(
            &mut self,
        ) -> Option<(Connection<I, NatConfig<I, A>, BT>, ConnectionDirection)> {
            let Self { conn, marks: _ } = self;
            conn.take()
        }

        fn replace_connection_and_direction(
            &mut self,
            new_conn: Connection<I, NatConfig<I, A>, BT>,
            direction: ConnectionDirection,
        ) -> Option<Connection<I, NatConfig<I, A>, BT>> {
            let Self { conn, marks: _ } = self;
            conn.replace((new_conn, direction)).map(|(conn, _dir)| conn)
        }
    }

    impl<I: TestIpExt, A, BT: FilterBindingsTypes> FilterMarkMetadata for PacketMetadata<I, A, BT> {
        fn apply_mark_action(&mut self, domain: MarkDomain, action: MarkAction) {
            action.apply(self.marks.get_mut(domain))
        }
    }

    #[test]
    fn accept_by_default_if_no_matching_rules_in_hook() {
        assert_eq!(
            check_routines_for_hook::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &Hook::default(),
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            Verdict::Accept(())
        );
    }

    #[test]
    fn accept_by_default_if_return_from_routine() {
        let hook = Hook {
            routines: vec![Routine {
                rules: vec![Rule::new(PacketMatcher::default(), Action::Return)],
            }],
        };

        assert_eq!(
            check_routines_for_hook::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &hook,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            Verdict::Accept(())
        );
    }

    #[test]
    fn accept_terminal_for_installed_routine() {
        let routine = Routine {
            rules: vec![
                // Accept all traffic.
                Rule::new(PacketMatcher::default(), Action::Accept),
                // Drop all traffic.
                Rule::new(PacketMatcher::default(), Action::Drop),
            ],
        };
        assert_eq!(
            check_routine::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &routine,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                &Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            RoutineResult::Accept
        );

        // `Accept` should also be propagated from subroutines.
        let routine = Routine {
            rules: vec![
                // Jump to a routine that accepts all traffic.
                Rule::new(
                    PacketMatcher::default(),
                    Action::Jump(UninstalledRoutine::new(
                        vec![Rule::new(PacketMatcher::default(), Action::Accept)],
                        0,
                    )),
                ),
                // Drop all traffic.
                Rule::new(PacketMatcher::default(), Action::Drop),
            ],
        };
        assert_eq!(
            check_routine::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &routine,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                &Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            RoutineResult::Accept
        );

        // Now put that routine in a hook that also includes *another* installed
        // routine which drops all traffic. The first installed routine should
        // terminate at its `Accept` result, but the hook should terminate at
        // the `Drop` result in the second routine.
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
            check_routines_for_hook::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &hook,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            Verdict::Drop
        );
    }

    #[test]
    fn drop_terminal_for_entire_hook() {
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
            check_routines_for_hook::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &hook,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            Verdict::Drop
        );
    }

    #[test]
    fn transparent_proxy_terminal_for_entire_hook() {
        const TPROXY_PORT: NonZeroU16 = NonZeroU16::new(8080).unwrap();

        let ingress = Hook {
            routines: vec![
                Routine {
                    rules: vec![Rule::new(
                        PacketMatcher::default(),
                        Action::TransparentProxy(TransparentProxy::LocalPort(TPROXY_PORT)),
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
            check_routines_for_ingress::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &ingress,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            IngressVerdict::TransparentLocalDelivery {
                addr: <Ipv4 as crate::packets::testutil::internal::TestIpExt>::DST_IP,
                port: TPROXY_PORT
            }
        );
    }

    #[test]
    fn jump_recursively_evaluates_target_routine() {
        // Drop result from a target routine is propagated to the calling
        // routine.
        let routine = Routine {
            rules: vec![Rule::new(
                PacketMatcher::default(),
                Action::Jump(UninstalledRoutine::new(
                    vec![Rule::new(PacketMatcher::default(), Action::Drop)],
                    0,
                )),
            )],
        };
        assert_eq!(
            check_routine::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &routine,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                &Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            RoutineResult::Drop
        );

        // Accept result from a target routine is also propagated to the calling
        // routine.
        let routine = Routine {
            rules: vec![
                Rule::new(
                    PacketMatcher::default(),
                    Action::Jump(UninstalledRoutine::new(
                        vec![Rule::new(PacketMatcher::default(), Action::Accept)],
                        0,
                    )),
                ),
                Rule::new(PacketMatcher::default(), Action::Drop),
            ],
        };
        assert_eq!(
            check_routine::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &routine,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                &Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            RoutineResult::Accept
        );

        // Return from a target routine results in continued evaluation of the
        // calling routine.
        let routine = Routine {
            rules: vec![
                Rule::new(
                    PacketMatcher::default(),
                    Action::Jump(UninstalledRoutine::new(
                        vec![Rule::new(PacketMatcher::default(), Action::Return)],
                        0,
                    )),
                ),
                Rule::new(PacketMatcher::default(), Action::Drop),
            ],
        };
        assert_eq!(
            check_routine::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &routine,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                &Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            RoutineResult::Drop
        );
    }

    #[test]
    fn return_terminal_for_single_routine() {
        let routine = Routine {
            rules: vec![
                Rule::new(PacketMatcher::default(), Action::Return),
                // Drop all traffic.
                Rule::new(PacketMatcher::default(), Action::Drop),
            ],
        };

        assert_eq!(
            check_routine::<Ipv4, _, FakeDeviceId, FakeDeviceClass, _>(
                &routine,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                &Interfaces { ingress: None, egress: None },
                &mut NullMetadata {},
            ),
            RoutineResult::Return
        );
    }

    #[ip_test(I)]
    fn filter_handler_implements_ip_hooks_correctly<I: TestIpExt>() {
        fn drop_all_traffic<I: TestIpExt>(
            matcher: PacketMatcher<I, FakeDeviceClass>,
        ) -> Hook<I, FakeDeviceClass, ()> {
            Hook { routines: vec![Routine { rules: vec![Rule::new(matcher, Action::Drop)] }] }
        }

        let mut bindings_ctx = FakeBindingsCtx::new();

        // Ingress hook should use ingress routines and check the input
        // interface.
        let mut ctx = FakeCtx::with_ip_routines(
            &mut bindings_ctx,
            IpRoutines {
                ingress: drop_all_traffic(PacketMatcher {
                    in_interface: Some(InterfaceMatcher::DeviceClass(FakeDeviceClass::Wlan)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert_eq!(
            FilterImpl(&mut ctx).ingress_hook(
                &mut bindings_ctx,
                &mut FakeIpPacket::<I, FakeTcpSegment>::arbitrary_value(),
                &wlan_interface(),
                &mut NullMetadata {},
            ),
            Verdict::Drop.into()
        );

        // Local ingress hook should use local ingress routines and check the
        // input interface.
        let mut ctx = FakeCtx::with_ip_routines(
            &mut bindings_ctx,
            IpRoutines {
                local_ingress: drop_all_traffic(PacketMatcher {
                    in_interface: Some(InterfaceMatcher::DeviceClass(FakeDeviceClass::Wlan)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert_eq!(
            FilterImpl(&mut ctx).local_ingress_hook(
                &mut bindings_ctx,
                &mut FakeIpPacket::<I, FakeTcpSegment>::arbitrary_value(),
                &wlan_interface(),
                &mut NullMetadata {},
            ),
            Verdict::Drop
        );

        // Forwarding hook should use forwarding routines and check both the
        // input and output interfaces.
        let mut ctx = FakeCtx::with_ip_routines(
            &mut bindings_ctx,
            IpRoutines {
                forwarding: drop_all_traffic(PacketMatcher {
                    in_interface: Some(InterfaceMatcher::DeviceClass(FakeDeviceClass::Wlan)),
                    out_interface: Some(InterfaceMatcher::DeviceClass(FakeDeviceClass::Ethernet)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert_eq!(
            FilterImpl(&mut ctx).forwarding_hook(
                &mut FakeIpPacket::<I, FakeTcpSegment>::arbitrary_value(),
                &wlan_interface(),
                &ethernet_interface(),
                &mut NullMetadata {},
            ),
            Verdict::Drop
        );

        // Local egress hook should use local egress routines and check the
        // output interface.
        let mut ctx = FakeCtx::with_ip_routines(
            &mut bindings_ctx,
            IpRoutines {
                local_egress: drop_all_traffic(PacketMatcher {
                    out_interface: Some(InterfaceMatcher::DeviceClass(FakeDeviceClass::Wlan)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert_eq!(
            FilterImpl(&mut ctx).local_egress_hook(
                &mut bindings_ctx,
                &mut FakeIpPacket::<I, FakeTcpSegment>::arbitrary_value(),
                &wlan_interface(),
                &mut NullMetadata {},
            ),
            Verdict::Drop
        );

        // Egress hook should use egress routines and check the output
        // interface.
        let mut ctx = FakeCtx::with_ip_routines(
            &mut bindings_ctx,
            IpRoutines {
                egress: drop_all_traffic(PacketMatcher {
                    out_interface: Some(InterfaceMatcher::DeviceClass(FakeDeviceClass::Wlan)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert_eq!(
            FilterImpl(&mut ctx)
                .egress_hook(
                    &mut bindings_ctx,
                    &mut FakeIpPacket::<I, FakeTcpSegment>::arbitrary_value(),
                    &wlan_interface(),
                    &mut NullMetadata {},
                )
                .0,
            Verdict::Drop
        );
    }

    #[ip_test(I)]
    #[test_case(22 => Verdict::Accept(()); "port 22 allowed for SSH")]
    #[test_case(80 => Verdict::Accept(()); "port 80 allowed for HTTP")]
    #[test_case(1024 => Verdict::Accept(()); "ephemeral port 1024 allowed")]
    #[test_case(65535 => Verdict::Accept(()); "ephemeral port 65535 allowed")]
    #[test_case(1023 => Verdict::Drop; "privileged port 1023 blocked")]
    #[test_case(53 => Verdict::Drop; "privileged port 53 blocked")]
    fn block_privileged_ports_except_ssh_http<I: TestIpExt>(port: u16) -> Verdict {
        fn tcp_port_rule<I: FilterIpExt>(
            src_port: Option<PortMatcher>,
            dst_port: Option<PortMatcher>,
            action: Action<I, FakeDeviceClass, ()>,
        ) -> Rule<I, FakeDeviceClass, ()> {
            Rule::new(
                PacketMatcher {
                    transport_protocol: Some(TransportProtocolMatcher {
                        proto: <&FakeTcpSegment as TransportPacketExt<I>>::proto().unwrap(),
                        src_port,
                        dst_port,
                    }),
                    ..Default::default()
                },
                action,
            )
        }

        fn default_filter_rules<I: FilterIpExt>() -> Routine<I, FakeDeviceClass, ()> {
            Routine {
                rules: vec![
                    // pass in proto tcp to port 22;
                    tcp_port_rule(
                        /* src_port */ None,
                        Some(PortMatcher { range: 22..=22, invert: false }),
                        Action::Accept,
                    ),
                    // pass in proto tcp to port 80;
                    tcp_port_rule(
                        /* src_port */ None,
                        Some(PortMatcher { range: 80..=80, invert: false }),
                        Action::Accept,
                    ),
                    // pass in proto tcp to range 1024:65535;
                    tcp_port_rule(
                        /* src_port */ None,
                        Some(PortMatcher { range: 1024..=65535, invert: false }),
                        Action::Accept,
                    ),
                    // drop in proto tcp to range 1:6553;
                    tcp_port_rule(
                        /* src_port */ None,
                        Some(PortMatcher { range: 1..=65535, invert: false }),
                        Action::Drop,
                    ),
                ],
            }
        }

        let mut bindings_ctx = FakeBindingsCtx::new();

        let mut ctx = FakeCtx::with_ip_routines(
            &mut bindings_ctx,
            IpRoutines {
                local_ingress: Hook { routines: vec![default_filter_rules()] },
                ..Default::default()
            },
        );

        FilterImpl(&mut ctx).local_ingress_hook(
            &mut bindings_ctx,
            &mut FakeIpPacket::<I, _> {
                body: FakeTcpSegment {
                    dst_port: port,
                    src_port: 11111,
                    segment: SegmentHeader::arbitrary_value(),
                    payload_len: 8888,
                },
                ..ArbitraryValue::arbitrary_value()
            },
            &wlan_interface(),
            &mut NullMetadata {},
        )
    }

    #[ip_test(I)]
    #[test_case(
        ethernet_interface() => Verdict::Accept(());
        "allow incoming traffic on ethernet interface"
    )]
    #[test_case(wlan_interface() => Verdict::Drop; "drop incoming traffic on wlan interface")]
    fn filter_on_wlan_only<I: TestIpExt>(interface: FakeDeviceId) -> Verdict {
        fn drop_wlan_traffic<I: IpExt>() -> Routine<I, FakeDeviceClass, ()> {
            Routine {
                rules: vec![Rule::new(
                    PacketMatcher {
                        in_interface: Some(InterfaceMatcher::Id(wlan_interface().id)),
                        ..Default::default()
                    },
                    Action::Drop,
                )],
            }
        }

        let mut bindings_ctx = FakeBindingsCtx::new();

        let mut ctx = FakeCtx::with_ip_routines(
            &mut bindings_ctx,
            IpRoutines {
                local_ingress: Hook { routines: vec![drop_wlan_traffic()] },
                ..Default::default()
            },
        );

        FilterImpl(&mut ctx).local_ingress_hook(
            &mut bindings_ctx,
            &mut FakeIpPacket::<I, FakeTcpSegment>::arbitrary_value(),
            &interface,
            &mut NullMetadata {},
        )
    }

    #[test]
    fn ingress_reuses_cached_connection_when_available() {
        let mut bindings_ctx = FakeBindingsCtx::new();
        let mut core_ctx = FakeCtx::new(&mut bindings_ctx);

        // When a connection is finalized in the EGRESS hook, it should stash a shared
        // reference to the connection in the packet metadata.
        let mut packet = FakeIpPacket::<Ipv4, FakeUdpPacket>::arbitrary_value();
        let mut metadata = PacketMetadata::default();
        let (verdict, _proof) = FilterImpl(&mut core_ctx).egress_hook(
            &mut bindings_ctx,
            &mut packet,
            &ethernet_interface(),
            &mut metadata,
        );
        assert_eq!(verdict, Verdict::Accept(()));

        // The stashed reference should point to the connection that is in the table.
        let (stashed, _dir) =
            metadata.take_connection_and_direction().expect("metadata should include connection");
        let tuple = packet.conntrack_packet().expect("packet should be trackable").tuple().clone();
        let table = core_ctx
            .conntrack()
            .get_connection(&tuple)
            .expect("packet should be inserted in table");
        assert_matches!(
            (table, stashed),
            (Connection::Shared(table), Connection::Shared(stashed)) => {
                assert!(Arc::ptr_eq(&table, &stashed));
            }
        );

        // Provided with the connection, the INGRESS hook should reuse it rather than
        // creating a new one.
        let verdict = FilterImpl(&mut core_ctx).ingress_hook(
            &mut bindings_ctx,
            &mut packet,
            &ethernet_interface(),
            &mut metadata,
        );
        assert_eq!(verdict, Verdict::Accept(()).into());

        // As a result, rather than there being a new connection in the packet metadata,
        // it should contain the same connection that is still in the table.
        let (after_ingress, _dir) =
            metadata.take_connection_and_direction().expect("metadata should include connection");
        let table = core_ctx
            .conntrack()
            .get_connection(&tuple)
            .expect("packet should be inserted in table");
        assert_matches!(
            (table, after_ingress),
            (Connection::Shared(before), Connection::Shared(after)) => {
                assert!(Arc::ptr_eq(&before, &after));
            }
        );
    }

    #[ip_test(I)]
    fn drop_packet_on_finalize_connection_failure<I: TestIpExt>() {
        let mut bindings_ctx = FakeBindingsCtx::new();
        let mut ctx = FakeCtx::new(&mut bindings_ctx);

        for i in 0..u32::try_from(conntrack::MAXIMUM_ENTRIES / 2).unwrap() {
            let (mut packet, mut reply_packet) = conntrack::testutils::make_test_udp_packets(i);
            let (verdict, _proof) = FilterImpl(&mut ctx).egress_hook(
                &mut bindings_ctx,
                &mut packet,
                &ethernet_interface(),
                &mut NullMetadata {},
            );
            assert_eq!(verdict, Verdict::Accept(()));

            let (verdict, _proof) = FilterImpl(&mut ctx).egress_hook(
                &mut bindings_ctx,
                &mut reply_packet,
                &ethernet_interface(),
                &mut NullMetadata {},
            );
            assert_eq!(verdict, Verdict::Accept(()));

            let (verdict, _proof) = FilterImpl(&mut ctx).egress_hook(
                &mut bindings_ctx,
                &mut packet,
                &ethernet_interface(),
                &mut NullMetadata {},
            );
            assert_eq!(verdict, Verdict::Accept(()));
        }

        // Finalizing the connection should fail when the conntrack table is at maximum
        // capacity and there are no connections to remove, because all existing
        // connections are considered established.
        let (verdict, _proof) = FilterImpl(&mut ctx).egress_hook(
            &mut bindings_ctx,
            &mut FakeIpPacket::<I, FakeUdpPacket>::arbitrary_value(),
            &ethernet_interface(),
            &mut NullMetadata {},
        );
        assert_eq!(verdict, Verdict::Drop);
    }

    #[ip_test(I)]
    fn implicit_snat_to_prevent_tuple_clash<I: TestIpExt>() {
        let mut bindings_ctx = FakeBindingsCtx::new();
        let mut ctx = FakeCtx::with_nat_routines_and_device_addrs(
            &mut bindings_ctx,
            NatRoutines {
                egress: Hook {
                    routines: vec![Routine {
                        rules: vec![Rule::new(
                            PacketMatcher {
                                src_address: Some(AddressMatcher {
                                    matcher: AddressMatcherType::Range(I::SRC_IP_2..=I::SRC_IP_2),
                                    invert: false,
                                }),
                                ..Default::default()
                            },
                            Action::Masquerade { src_port: None },
                        )],
                    }],
                },
                ..Default::default()
            },
            HashMap::from([(
                ethernet_interface(),
                AddrSubnet::new(I::SRC_IP, I::SUBNET.prefix()).unwrap(),
            )]),
        );

        // Simulate a forwarded packet, originally from I::SRC_IP_2, that is masqueraded
        // to be from I::SRC_IP. The packet should have had SNAT performed.
        let mut packet = FakeIpPacket {
            src_ip: I::SRC_IP_2,
            dst_ip: I::DST_IP,
            body: FakeUdpPacket::arbitrary_value(),
        };
        let (verdict, _proof) = FilterImpl(&mut ctx).egress_hook(
            &mut bindings_ctx,
            &mut packet,
            &ethernet_interface(),
            &mut NullMetadata {},
        );
        assert_eq!(verdict, Verdict::Accept(()));
        assert_eq!(packet.src_ip, I::SRC_IP);

        // Now simulate a locally-generated packet that conflicts with this flow; it is
        // from I::SRC_IP to I::DST_IP and has the same source and destination ports.
        // Finalizing the connection would typically fail, causing the packet to be
        // dropped, because the reply tuple conflicts with the reply tuple of the
        // masqueraded flow. So instead this new flow is implicitly SNATed to a free
        // port and the connection should be successfully finalized.
        let mut packet = FakeIpPacket::<I, FakeUdpPacket>::arbitrary_value();
        let src_port = packet.body.src_port;
        let (verdict, _proof) = FilterImpl(&mut ctx).egress_hook(
            &mut bindings_ctx,
            &mut packet,
            &ethernet_interface(),
            &mut NullMetadata {},
        );
        assert_eq!(verdict, Verdict::Accept(()));
        assert_ne!(packet.body.src_port, src_port);
    }

    #[ip_test(I)]
    fn packet_adopts_tracked_connection_in_table_if_identical<I: TestIpExt>() {
        let mut bindings_ctx = FakeBindingsCtx::new();
        let mut core_ctx = FakeCtx::new(&mut bindings_ctx);

        // Simulate a race where two packets in the same flow both end up
        // creating identical exclusive connections.
        let mut first_packet = FakeIpPacket::<I, FakeUdpPacket>::arbitrary_value();
        let mut first_metadata = PacketMetadata::default();
        let verdict = FilterImpl(&mut core_ctx).local_egress_hook(
            &mut bindings_ctx,
            &mut first_packet,
            &ethernet_interface(),
            &mut first_metadata,
        );
        assert_eq!(verdict, Verdict::Accept(()));

        let mut second_packet = FakeIpPacket::<I, FakeUdpPacket>::arbitrary_value();
        let mut second_metadata = PacketMetadata::default();
        let verdict = FilterImpl(&mut core_ctx).local_egress_hook(
            &mut bindings_ctx,
            &mut second_packet,
            &ethernet_interface(),
            &mut second_metadata,
        );
        assert_eq!(verdict, Verdict::Accept(()));

        // Finalize the first connection; it should get inserted in the table.
        let (verdict, _proof) = FilterImpl(&mut core_ctx).egress_hook(
            &mut bindings_ctx,
            &mut first_packet,
            &ethernet_interface(),
            &mut first_metadata,
        );
        assert_eq!(verdict, Verdict::Accept(()));

        // The second packet conflicts with the connection that's in the table, but it's
        // identical to the first one, so it should adopt the finalized connection.
        let (verdict, _proof) = FilterImpl(&mut core_ctx).egress_hook(
            &mut bindings_ctx,
            &mut second_packet,
            &ethernet_interface(),
            &mut second_metadata,
        );
        assert_eq!(second_packet.body.src_port, first_packet.body.src_port);
        assert_eq!(verdict, Verdict::Accept(()));

        let (first_conn, _dir) = first_metadata.take_connection_and_direction().unwrap();
        let (second_conn, _dir) = second_metadata.take_connection_and_direction().unwrap();
        assert_matches!(
            (first_conn, second_conn),
            (Connection::Shared(first), Connection::Shared(second)) => {
                assert!(Arc::ptr_eq(&first, &second));
            }
        );
    }

    #[ip_test(I)]
    fn both_source_and_destination_nat_configured<I: TestIpExt>() {
        let mut bindings_ctx = FakeBindingsCtx::new();
        // Install NAT rules to perform both DNAT (in LOCAL_EGRESS) and SNAT (in
        // EGRESS).
        let mut core_ctx = FakeCtx::with_nat_routines_and_device_addrs(
            &mut bindings_ctx,
            NatRoutines {
                local_egress: Hook {
                    routines: vec![Routine {
                        rules: vec![Rule::new(
                            PacketMatcher::default(),
                            Action::Redirect { dst_port: None },
                        )],
                    }],
                },
                egress: Hook {
                    routines: vec![Routine {
                        rules: vec![Rule::new(
                            PacketMatcher::default(),
                            Action::Masquerade { src_port: None },
                        )],
                    }],
                },
                ..Default::default()
            },
            HashMap::from([(
                ethernet_interface(),
                AddrSubnet::new(I::SRC_IP_2, I::SUBNET.prefix()).unwrap(),
            )]),
        );

        // Even though the packet is modified after the first hook, where DNAT is
        // configured...
        let mut packet = FakeIpPacket::<I, FakeUdpPacket>::arbitrary_value();
        let mut metadata = PacketMetadata::default();
        let verdict = FilterImpl(&mut core_ctx).local_egress_hook(
            &mut bindings_ctx,
            &mut packet,
            &ethernet_interface(),
            &mut metadata,
        );
        assert_eq!(verdict, Verdict::Accept(()));
        assert_eq!(packet.dst_ip, *I::LOOPBACK_ADDRESS);

        // ...SNAT is also successfully configured for the packet, because the packet's
        // [`ConnectionDirection`] is cached in the metadata.
        let (verdict, _proof) = FilterImpl(&mut core_ctx).egress_hook(
            &mut bindings_ctx,
            &mut packet,
            &ethernet_interface(),
            &mut metadata,
        );
        assert_eq!(verdict, Verdict::Accept(()));
        assert_eq!(packet.src_ip, I::SRC_IP_2);
    }

    #[ip_test(I)]
    #[test_case(
        Hook {
            routines: vec![
                Routine {
                    rules: vec![
                        Rule::new(
                            PacketMatcher::default(),
                            Action::Mark {
                                domain: MarkDomain::Mark1,
                                action: MarkAction::SetMark { clearing_mask: 0, mark: 1 },
                            },
                        ),
                        Rule::new(PacketMatcher::default(), Action::Drop),
                    ],
                },
            ],
        }; "non terminal for routine"
    )]
    #[test_case(
        Hook {
            routines: vec![
                Routine {
                    rules: vec![Rule::new(
                        PacketMatcher::default(),
                        Action::Mark {
                            domain: MarkDomain::Mark1,
                            action: MarkAction::SetMark { clearing_mask: 0, mark: 1 },
                        },
                    )],
                },
                Routine {
                    rules: vec![
                        Rule::new(PacketMatcher::default(), Action::Drop),
                    ],
                },
            ],
        }; "non terminal for hook"
    )]
    fn mark_action<I: TestIpExt>(ingress: Hook<I, FakeDeviceClass, ()>) {
        let mut metadata = PacketMetadata::<I, FakeWeakAddressId<I>, FakeBindingsCtx<I>>::default();
        assert_eq!(
            check_routines_for_ingress::<I, _, FakeDeviceId, FakeDeviceClass, _>(
                &ingress,
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces { ingress: None, egress: None },
                &mut metadata,
            ),
            IngressVerdict::Verdict(Verdict::Drop),
        );
        assert_eq!(metadata.marks, Marks::new([(MarkDomain::Mark1, 1)]));
    }

    #[ip_test(I)]
    fn mark_action_applied_in_succession<I: TestIpExt>() {
        fn hook_with_single_mark_action<I: TestIpExt>(
            domain: MarkDomain,
            action: MarkAction,
        ) -> Hook<I, FakeDeviceClass, ()> {
            Hook {
                routines: vec![Routine {
                    rules: vec![Rule::new(
                        PacketMatcher::default(),
                        Action::Mark { domain, action },
                    )],
                }],
            }
        }
        let mut metadata = PacketMetadata::<I, FakeWeakAddressId<I>, FakeBindingsCtx<I>>::default();
        assert_eq!(
            check_routines_for_ingress::<I, _, FakeDeviceId, FakeDeviceClass, _>(
                &hook_with_single_mark_action(
                    MarkDomain::Mark1,
                    MarkAction::SetMark { clearing_mask: 0, mark: 1 }
                ),
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces { ingress: None, egress: None },
                &mut metadata,
            ),
            IngressVerdict::Verdict(Verdict::Accept(())),
        );
        assert_eq!(metadata.marks, Marks::new([(MarkDomain::Mark1, 1)]));

        assert_eq!(
            check_routines_for_hook(
                &hook_with_single_mark_action::<I>(
                    MarkDomain::Mark2,
                    MarkAction::SetMark { clearing_mask: 0, mark: 1 }
                ),
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces::<FakeDeviceId> { ingress: None, egress: None },
                &mut metadata,
            ),
            Verdict::Accept(())
        );
        assert_eq!(metadata.marks, Marks::new([(MarkDomain::Mark1, 1), (MarkDomain::Mark2, 1)]));

        assert_eq!(
            check_routines_for_hook(
                &hook_with_single_mark_action::<I>(
                    MarkDomain::Mark1,
                    MarkAction::SetMark { clearing_mask: 1, mark: 2 }
                ),
                &FakeIpPacket::<_, FakeTcpSegment>::arbitrary_value(),
                Interfaces::<FakeDeviceId> { ingress: None, egress: None },
                &mut metadata,
            ),
            Verdict::Accept(())
        );
        assert_eq!(metadata.marks, Marks::new([(MarkDomain::Mark1, 2), (MarkDomain::Mark2, 1)]));
    }
}
