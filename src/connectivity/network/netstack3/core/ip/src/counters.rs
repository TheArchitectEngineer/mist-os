// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Facilities for tracking the counts of various IP events.

use core::fmt::Debug;
use net_types::ip::{GenericOverIp, Ip, Ipv4, Ipv6};
use netstack3_base::{
    Counter, CounterRepr, Inspectable, Inspector, InspectorExt as _, TestOnlyFrom,
    TestOnlyPartialEq,
};

use crate::internal::fragmentation::FragmentationCounters;

/// An IP extension trait supporting counters at the IP layer.
pub trait IpCountersIpExt: Ip {
    /// Receive counters.
    type RxCounters<C: CounterRepr>: Default
        + Debug
        + Inspectable
        + TestOnlyPartialEq
        + for<'a> TestOnlyFrom<&'a Self::RxCounters<Counter>>;
}

impl IpCountersIpExt for Ipv4 {
    type RxCounters<C: CounterRepr> = Ipv4RxCounters<C>;
}

impl IpCountersIpExt for Ipv6 {
    type RxCounters<C: CounterRepr> = Ipv6RxCounters<C>;
}

/// Ip layer counters.
#[derive(Default, Debug, GenericOverIp)]
#[generic_over_ip(I, Ip)]
#[cfg_attr(any(test, feature = "testutils"), derive(PartialEq))]
pub struct IpCounters<I: IpCountersIpExt, C: CounterRepr = Counter> {
    /// Count of incoming IP unicast packets delivered.
    pub deliver_unicast: C,
    /// Count of incoming IP multicast packets delivered.
    pub deliver_multicast: C,
    /// Count of incoming IP packets that are dispatched to the appropriate protocol.
    pub dispatch_receive_ip_packet: C,
    /// Count of incoming IP packets destined to another host.
    pub dispatch_receive_ip_packet_other_host: C,
    /// Count of incoming IP packets received by the stack.
    pub receive_ip_packet: C,
    /// Count of sent outgoing IP packets.
    pub send_ip_packet: C,
    /// Count of packets to be forwarded which are instead dropped because
    /// forwarding is disabled.
    pub forwarding_disabled: C,
    /// Count of incoming packets forwarded to another host.
    pub forward: C,
    /// Count of incoming packets which cannot be forwarded because there is no
    /// route to the destination host.
    pub no_route_to_host: C,
    /// Count of incoming packets which cannot be forwarded because the MTU has
    /// been exceeded.
    pub mtu_exceeded: C,
    /// Count of incoming packets which cannot be forwarded because the TTL has
    /// expired.
    pub ttl_expired: C,
    /// Count of ICMP error messages received.
    pub receive_icmp_error: C,
    /// Count of IP fragment reassembly errors.
    pub fragment_reassembly_error: C,
    /// Count of IP fragments that could not be reassembled because more
    /// fragments were needed.
    pub need_more_fragments: C,
    /// Count of IP fragments that could not be reassembled because the fragment
    /// was invalid.
    pub invalid_fragment: C,
    /// Count of IP fragments that could not be reassembled because the stack's
    /// per-IP-protocol fragment cache was full.
    pub fragment_cache_full: C,
    /// Count of incoming IP packets not delivered because of a parameter problem.
    pub parameter_problem: C,
    /// Count of incoming IP packets with an unspecified destination address.
    pub unspecified_destination: C,
    /// Count of incoming IP packets with an unspecified source address.
    pub unspecified_source: C,
    /// Count of incoming IP packets with an invalid source address.
    /// See the definitions of [`net_types::ip::Ipv4SourceAddr`] and
    /// [`net_types::ip::Ipv6SourceAddr`] for the exact requirements.
    pub invalid_source: C,
    /// Count of incoming IP packets dropped.
    pub dropped: C,
    /// Number of frames rejected because they'd cause illegal loopback
    /// addresses on the wire.
    pub tx_illegal_loopback_address: C,
    /// Version specific rx counters.
    pub version_rx: I::RxCounters<C>,
    /// Count of incoming IP multicast packets that were dropped because
    /// The stack doesn't have any sockets that belong to the multicast group,
    /// and the stack isn't configured to forward the multicast packet.
    pub multicast_no_interest: C,
    /// Count of looped-back packets that held a cached conntrack entry that could
    /// not be downcasted to the expected type. This would happen if, for example, a
    /// packet was modified to a different IP version between EGRESS and INGRESS.
    pub invalid_cached_conntrack_entry: C,
    /// IP fragmentation counters.
    pub fragmentation: FragmentationCounters<C>,
    /// Number of packets filtered out by the socket egress filter.
    pub socket_egress_filter_dropped: C,
}

impl<I: IpCountersIpExt, C: CounterRepr> Inspectable for IpCounters<I, C> {
    fn record<II: Inspector>(&self, inspector: &mut II) {
        let IpCounters {
            deliver_unicast,
            deliver_multicast,
            dispatch_receive_ip_packet,
            dispatch_receive_ip_packet_other_host,
            receive_ip_packet,
            send_ip_packet,
            forwarding_disabled,
            forward,
            no_route_to_host,
            mtu_exceeded,
            ttl_expired,
            receive_icmp_error,
            fragment_reassembly_error,
            need_more_fragments,
            invalid_fragment,
            fragment_cache_full,
            parameter_problem,
            unspecified_destination,
            unspecified_source,
            invalid_source,
            dropped,
            tx_illegal_loopback_address,
            version_rx,
            multicast_no_interest,
            invalid_cached_conntrack_entry,
            fragmentation,
            socket_egress_filter_dropped,
        } = self;
        inspector.record_child("PacketTx", |inspector| {
            inspector.record_counter("Sent", send_ip_packet);
            inspector.record_counter("IllegalLoopbackAddress", tx_illegal_loopback_address);
            inspector.record_counter("SocketEgressFilterDropped", socket_egress_filter_dropped);
        });
        inspector.record_child("PacketRx", |inspector| {
            inspector.record_counter("Received", receive_ip_packet);
            inspector.record_counter("Dispatched", dispatch_receive_ip_packet);
            inspector.record_counter("OtherHost", dispatch_receive_ip_packet_other_host);
            inspector.record_counter("ParameterProblem", parameter_problem);
            inspector.record_counter("UnspecifiedDst", unspecified_destination);
            inspector.record_counter("UnspecifiedSrc", unspecified_source);
            inspector.record_counter("InvalidSrc", invalid_source);
            inspector.record_counter("Dropped", dropped);
            inspector.record_counter("MulticastNoInterest", multicast_no_interest);
            inspector.record_counter("DeliveredUnicast", deliver_unicast);
            inspector.record_counter("DeliveredMulticast", deliver_multicast);
            inspector.record_counter("InvalidCachedConntrackEntry", invalid_cached_conntrack_entry);
            inspector.delegate_inspectable(version_rx);
        });
        inspector.record_child("Forwarding", |inspector| {
            inspector.record_counter("Forwarded", forward);
            inspector.record_counter("ForwardingDisabled", forwarding_disabled);
            inspector.record_counter("NoRouteToHost", no_route_to_host);
            inspector.record_counter("MtuExceeded", mtu_exceeded);
            inspector.record_counter("TtlExpired", ttl_expired);
        });
        inspector.record_counter("RxIcmpError", receive_icmp_error);
        inspector.record_child("FragmentsRx", |inspector| {
            inspector.record_counter("ReassemblyError", fragment_reassembly_error);
            inspector.record_counter("NeedMoreFragments", need_more_fragments);
            inspector.record_counter("InvalidFragment", invalid_fragment);
            inspector.record_counter("CacheFull", fragment_cache_full);
        });
        inspector.record_child("FragmentsTx", |inspector| {
            let FragmentationCounters {
                fragmentation_required,
                fragments,
                error_not_allowed,
                error_mtu_too_small,
                error_body_too_long,
                error_inner_size_limit_exceeded,
                error_fragmented_serializer,
            } = fragmentation;
            inspector.record_counter("FragmentationRequired", fragmentation_required);
            inspector.record_counter("Fragments", fragments);
            inspector.record_counter("ErrorNotAllowed", error_not_allowed);
            inspector.record_counter("ErrorMtuTooSmall", error_mtu_too_small);
            inspector.record_counter("ErrorBodyTooLong", error_body_too_long);
            inspector
                .record_counter("ErrorInnerSizeLimitExceeded", error_inner_size_limit_exceeded);
            inspector.record_counter("ErrorFragmentedSerializer", error_fragmented_serializer);
        });
    }
}

/// IPv4-specific Rx counters.
#[derive(Default, Debug)]
#[cfg_attr(any(test, feature = "testutils"), derive(PartialEq))]
pub struct Ipv4RxCounters<C: CounterRepr = Counter> {
    /// Count of incoming broadcast IPv4 packets delivered.
    pub deliver_broadcast: C,
}

impl<C: CounterRepr> Inspectable for Ipv4RxCounters<C> {
    fn record<I: Inspector>(&self, inspector: &mut I) {
        let Self { deliver_broadcast } = self;
        inspector.record_counter("DeliveredBroadcast", deliver_broadcast);
    }
}

/// IPv6-specific Rx counters.
#[derive(Default, Debug)]
#[cfg_attr(any(test, feature = "testutils"), derive(PartialEq))]
pub struct Ipv6RxCounters<C: CounterRepr = Counter> {
    /// Count of incoming IPv6 packets dropped because the destination address
    /// is only tentatively assigned to the device.
    pub drop_for_tentative: C,
    /// Count of incoming IPv6 packets discarded while processing extension
    /// headers.
    pub extension_header_discard: C,
    /// Count of incoming neighbor solicitations discarded as looped-back
    /// DAD probes.
    pub drop_looped_back_dad_probe: C,
}

impl<C: CounterRepr> Inspectable for Ipv6RxCounters<C> {
    fn record<I: Inspector>(&self, inspector: &mut I) {
        let Self { drop_for_tentative, extension_header_discard, drop_looped_back_dad_probe } =
            self;
        inspector.record_counter("DroppedTentativeDst", drop_for_tentative);
        inspector.record_counter("DroppedExtensionHeader", extension_header_discard);
        inspector.record_counter("DroppedLoopedBackDadProbe", drop_looped_back_dad_probe);
    }
}

#[cfg(any(test, feature = "testutils"))]
pub mod testutil {
    use super::*;

    use netstack3_base::ResourceCounterContext;

    impl<C: CounterRepr> From<&Ipv4RxCounters> for Ipv4RxCounters<C> {
        fn from(counters: &Ipv4RxCounters) -> Ipv4RxCounters<C> {
            let Ipv4RxCounters { deliver_broadcast } = counters;
            Ipv4RxCounters { deliver_broadcast: deliver_broadcast.into_repr() }
        }
    }

    impl<C: CounterRepr> From<&Ipv6RxCounters> for Ipv6RxCounters<C> {
        fn from(counters: &Ipv6RxCounters) -> Ipv6RxCounters<C> {
            let Ipv6RxCounters {
                drop_for_tentative,
                extension_header_discard,
                drop_looped_back_dad_probe,
            } = counters;
            Ipv6RxCounters {
                drop_for_tentative: drop_for_tentative.get().into_repr(),
                extension_header_discard: extension_header_discard.into_repr(),
                drop_looped_back_dad_probe: drop_looped_back_dad_probe.into_repr(),
            }
        }
    }

    /// Expected values of [`IpCounters<I>`].
    pub type IpCounterExpectations<I> = IpCounters<I, u64>;

    impl<I: IpCountersIpExt> From<&IpCounters<I>> for IpCounterExpectations<I> {
        fn from(counters: &IpCounters<I>) -> IpCounterExpectations<I> {
            let IpCounters {
                deliver_unicast,
                deliver_multicast,
                dispatch_receive_ip_packet,
                dispatch_receive_ip_packet_other_host,
                receive_ip_packet,
                send_ip_packet,
                forwarding_disabled,
                forward,
                no_route_to_host,
                mtu_exceeded,
                ttl_expired,
                receive_icmp_error,
                fragment_reassembly_error,
                need_more_fragments,
                invalid_fragment,
                fragment_cache_full,
                parameter_problem,
                unspecified_destination,
                unspecified_source,
                invalid_source,
                dropped,
                tx_illegal_loopback_address,
                version_rx,
                multicast_no_interest,
                invalid_cached_conntrack_entry,
                fragmentation,
                socket_egress_filter_dropped,
            } = counters;
            IpCounterExpectations {
                deliver_unicast: deliver_unicast.get(),
                deliver_multicast: deliver_multicast.get(),
                dispatch_receive_ip_packet: dispatch_receive_ip_packet.get(),
                dispatch_receive_ip_packet_other_host: dispatch_receive_ip_packet_other_host.get(),
                receive_ip_packet: receive_ip_packet.get(),
                send_ip_packet: send_ip_packet.get(),
                forwarding_disabled: forwarding_disabled.get(),
                forward: forward.get(),
                no_route_to_host: no_route_to_host.get(),
                mtu_exceeded: mtu_exceeded.get(),
                ttl_expired: ttl_expired.get(),
                receive_icmp_error: receive_icmp_error.get(),
                fragment_reassembly_error: fragment_reassembly_error.get(),
                need_more_fragments: need_more_fragments.get(),
                invalid_fragment: invalid_fragment.get(),
                fragment_cache_full: fragment_cache_full.get(),
                parameter_problem: parameter_problem.get(),
                unspecified_destination: unspecified_destination.get(),
                unspecified_source: unspecified_source.get(),
                invalid_source: invalid_source.get(),
                dropped: dropped.get(),
                tx_illegal_loopback_address: tx_illegal_loopback_address.get(),
                version_rx: version_rx.into(),
                multicast_no_interest: multicast_no_interest.get(),
                invalid_cached_conntrack_entry: invalid_cached_conntrack_entry.get(),
                fragmentation: fragmentation.into(),
                socket_egress_filter_dropped: socket_egress_filter_dropped.get(),
            }
        }
    }

    impl<I: IpCountersIpExt> IpCounterExpectations<I> {
        /// Constructs the expected counter state when the given count of IP
        /// packets have been received & dispatched.
        pub fn expect_dispatched(count: u64) -> Self {
            IpCounterExpectations {
                receive_ip_packet: count,
                dispatch_receive_ip_packet: count,
                deliver_unicast: count,
                ..Default::default()
            }
        }

        /// Assert that the counters tracked by `core_ctx` match expectations.
        #[track_caller]
        pub fn assert_counters<D, CC: ResourceCounterContext<D, IpCounters<I>>>(
            self,
            core_ctx: &CC,
            device: &D,
        ) {
            assert_eq!(
                &IpCounterExpectations::from(core_ctx.counters()),
                &self,
                "stack-wide counters"
            );
            assert_eq!(
                &IpCounterExpectations::from(core_ctx.per_resource_counters(device)),
                &self,
                "per-device counters"
            );
        }
    }
}
