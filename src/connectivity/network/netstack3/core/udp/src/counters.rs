// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Facilities for tracking the counts of various UDP events.

use net_types::ip::{Ip, IpMarked};
use netstack3_base::{
    Counter, CounterContext, Inspectable, Inspector, InspectorExt as _, ResourceCounterContext,
    WeakDeviceIdentifier,
};
use netstack3_datagram::IpExt;

use crate::internal::base::{UdpBindingsTypes, UdpSocketId};

/// A marker trait to simplify bounds for UDP counters.
pub trait UdpCounterContext<I: IpExt, D: WeakDeviceIdentifier, BT: UdpBindingsTypes>:
    ResourceCounterContext<UdpSocketId<I, D, BT>, UdpCountersWithSocket<I>>
    + CounterContext<UdpCountersWithoutSocket<I>>
{
}

impl<I, D, BT, CC> UdpCounterContext<I, D, BT> for CC
where
    I: IpExt,
    D: WeakDeviceIdentifier,
    BT: UdpBindingsTypes,
    CC: ResourceCounterContext<UdpSocketId<I, D, BT>, UdpCountersWithSocket<I>>
        + CounterContext<UdpCountersWithoutSocket<I>>,
{
}

/// Counters for UDP events that cannot be attributed to an individual socket.
///
/// These counters are tracked stack wide.
///
/// Note on dual stack sockets: These counters are tracked for `WireI`.
pub type UdpCountersWithoutSocket<I> = IpMarked<I, UdpCountersWithoutSocketInner>;

/// The IP agnostic version of [`UdpCountersWithoutSocket`].
///
/// The counter type `C` is generic to facilitate testing.
#[derive(Default)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct UdpCountersWithoutSocketInner<C = Counter> {
    /// Count of ICMP error messages received.
    pub rx_icmp_error: C,
    /// Count of UDP datagrams received from the IP layer, including error
    /// cases.
    pub rx: C,
    /// Count of incoming UDP datagrams dropped because it contained a mapped IP
    /// address in the header.
    pub rx_mapped_addr: C,
    /// Count of incoming UDP datagrams dropped because of an unknown
    /// destination port.
    pub rx_unknown_dest_port: C,
    /// Count of incoming UDP datagrams dropped because their UDP header was in
    /// a malformed state.
    pub rx_malformed: C,
}

/// Counters for UDP events that can be attributed to an individual socket.
///
/// These counters are tracked stack wide and per socket.
///
/// Note on dual stack sockets: These counters are tracked for `SockI`.
// TODO(https://fxbug.dev/396127493): For some of these events, it would be
// better to track them for `WireI` (e.g. `received_segments_dispatched`,
// `segments_sent`, etc.). Doing so may require splitting up the struct and/or
// reworking the `ResourceCounterContext` trait.
pub type UdpCountersWithSocket<I> = IpMarked<I, UdpCountersWithSocketInner>;

/// The IP agnostic version of [`UdpCountersWithSocket`].
///
/// The counter type `C` is generic to facilitate testing.
#[derive(Default, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct UdpCountersWithSocketInner<C = Counter> {
    /// Count of UDP datagrams that were delivered to a socket. Because some
    /// datagrams may be delivered to multiple sockets (e.g. multicast traffic)
    /// this counter may exceed the total number of individual UDP datagrams
    /// received by the stack.
    pub rx_delivered: C,
    /// Count of outgoing UDP datagrams sent from the socket layer, including
    /// error cases.
    pub tx: C,
    /// Count of outgoing UDP datagrams which failed to be sent out of the
    /// transport layer.
    pub tx_error: C,
}

/// A composition of the UDP counters with and without a socket.
pub struct CombinedUdpCounters<'a, I: Ip> {
    /// The UDP counters that can be associated with a socket.
    pub with_socket: &'a UdpCountersWithSocket<I>,
    /// The UDP counters that cannot be associated with a socket.
    ///
    /// This field is optional so that the same [`Inspectable`] implementation
    /// can be used for both the stack-wide counters and the per-socket
    /// counters.
    pub without_socket: Option<&'a UdpCountersWithoutSocket<I>>,
}

impl<I: Ip> Inspectable for CombinedUdpCounters<'_, I> {
    fn record<II: Inspector>(&self, inspector: &mut II) {
        let CombinedUdpCounters { with_socket, without_socket } = self;
        let UdpCountersWithSocketInner { rx_delivered, tx, tx_error } = with_socket.as_ref();

        // Note: Organize the "without socket" counters into helper struct to
        // make the optionality more ergonomic to handle.
        struct WithoutSocketRx<'a> {
            rx: &'a Counter,
            rx_mapped_addr: &'a Counter,
            rx_unknown_dest_port: &'a Counter,
            rx_malformed: &'a Counter,
        }
        struct WithoutSocketError<'a> {
            rx_icmp_error: &'a Counter,
        }
        let (without_socket_rx, without_socket_error) = match without_socket.map(AsRef::as_ref) {
            None => (None, None),
            Some(UdpCountersWithoutSocketInner {
                rx_icmp_error,
                rx,
                rx_mapped_addr,
                rx_unknown_dest_port,
                rx_malformed,
            }) => (
                Some(WithoutSocketRx { rx, rx_mapped_addr, rx_unknown_dest_port, rx_malformed }),
                Some(WithoutSocketError { rx_icmp_error }),
            ),
        };
        inspector.record_child("Rx", |inspector| {
            inspector.record_counter("Delivered", rx_delivered);
            if let Some(WithoutSocketRx {
                rx,
                rx_mapped_addr,
                rx_unknown_dest_port,
                rx_malformed,
            }) = without_socket_rx
            {
                inspector.record_counter("Received", rx);
                inspector.record_child("Errors", |inspector| {
                    inspector.record_counter("MappedAddr", rx_mapped_addr);
                    inspector.record_counter("UnknownDstPort", rx_unknown_dest_port);
                    inspector.record_counter("Malformed", rx_malformed);
                });
            }
        });
        inspector.record_child("Tx", |inspector| {
            inspector.record_counter("Sent", tx);
            inspector.record_counter("Errors", tx_error);
        });
        if let Some(WithoutSocketError { rx_icmp_error }) = without_socket_error {
            inspector.record_counter("IcmpErrors", rx_icmp_error);
        }
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub(crate) type CounterExpectationsWithSocket = UdpCountersWithSocketInner<u64>;

    impl From<&UdpCountersWithSocketInner> for CounterExpectationsWithSocket {
        fn from(counters: &UdpCountersWithSocketInner) -> CounterExpectationsWithSocket {
            let UdpCountersWithSocketInner { rx_delivered, tx, tx_error } = counters;
            CounterExpectationsWithSocket {
                rx_delivered: rx_delivered.get(),
                tx: tx.get(),
                tx_error: tx_error.get(),
            }
        }
    }

    pub(crate) type CounterExpectationsWithoutSocket = UdpCountersWithoutSocketInner<u64>;

    impl From<&UdpCountersWithoutSocketInner> for CounterExpectationsWithoutSocket {
        fn from(counters: &UdpCountersWithoutSocketInner) -> CounterExpectationsWithoutSocket {
            let UdpCountersWithoutSocketInner {
                rx_icmp_error,
                rx,
                rx_mapped_addr,
                rx_unknown_dest_port,
                rx_malformed,
            } = counters;
            CounterExpectationsWithoutSocket {
                rx_icmp_error: rx_icmp_error.get(),
                rx: rx.get(),
                rx_mapped_addr: rx_mapped_addr.get(),
                rx_unknown_dest_port: rx_unknown_dest_port.get(),
                rx_malformed: rx_malformed.get(),
            }
        }
    }
}
