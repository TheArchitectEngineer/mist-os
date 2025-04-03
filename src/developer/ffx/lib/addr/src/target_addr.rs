// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use fidl_fuchsia_developer_ffx::{
    TargetAddrInfo, TargetIp, TargetIpAddrInfo, TargetIpPort, TargetVSockCtx,
};
use fidl_fuchsia_net::{IpAddress, Ipv4Address, Ipv6Address};
use netext::{scope_id_to_name, IsLocalAddr};
use std::cmp::Ordering;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::str::FromStr;

/// Error returned when we try to turn a non-network address into a `SocketAddr`
#[derive(Copy, Clone, Debug)]
pub struct NotANetworkAddress;

/// Represents an address associated with a target, like [`TargetAddr`], but is
/// restricted only to network addresses, i.e. addresses which might be suitable
/// for SSH. This saves us some type conversions when passing these addresses
/// around in parts of the code that are specifically supporting network
/// operations or opening SSH connections.
#[derive(Clone, Debug, Copy)]
pub struct TargetIpAddr(SocketAddr);

impl TargetIpAddr {
    pub fn new(ip: IpAddr, scope_id: u32, port: u16) -> Self {
        match ip {
            IpAddr::V6(addr) => Self(SocketAddr::V6(SocketAddrV6::new(addr, port, 0, scope_id))),
            IpAddr::V4(addr) => Self(SocketAddr::V4(SocketAddrV4::new(addr, port))),
        }
    }

    pub fn scope_id(&self) -> u32 {
        match self.0 {
            SocketAddr::V6(v6) => v6.scope_id(),
            _ => 0,
        }
    }

    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }
}

/// Construct a new TargetIpAddr from a string representation of the form
/// accepted by std::net::SocketAddr, e.g. 127.0.0.1:22, or [fe80::1%1]:0.
impl FromStr for TargetIpAddr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let sa = s.parse::<SocketAddr>()?;
        Ok(Self::from(sa))
    }
}
// Only compare `TargetAddr` by ip and port, since we want to deduplicate targets if they are
// addressable over multiple IPv6 interfaces.
impl std::hash::Hash for TargetIpAddr {
    fn hash<H>(&self, state: &mut H)
    where
        H: std::hash::Hasher,
    {
        (self.0.ip(), self.0.port()).hash(state)
    }
}

impl PartialEq for TargetIpAddr {
    fn eq(&self, other: &Self) -> bool {
        self.0.ip() == other.0.ip() && self.0.port() == other.0.port()
    }
}

impl Eq for TargetIpAddr {}

impl Ord for TargetIpAddr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.ip().cmp(&other.0.ip()).then(self.0.port().cmp(&other.0.port()))
    }
}

impl PartialOrd for TargetIpAddr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<SocketAddr> for TargetIpAddr {
    fn from(s: SocketAddr) -> Self {
        TargetIpAddr(s)
    }
}

impl From<TargetIpAddr> for SocketAddr {
    fn from(t: TargetIpAddr) -> Self {
        Self::from(&t)
    }
}

impl From<&TargetIpAddr> for SocketAddr {
    fn from(t: &TargetIpAddr) -> Self {
        t.0
    }
}

impl Into<TargetIpAddrInfo> for &TargetIpAddr {
    fn into(self) -> TargetIpAddrInfo {
        let scope_id = self.scope_id();
        let ip = match self.0.ip() {
            IpAddr::V6(i) => IpAddress::Ipv6(Ipv6Address { addr: i.octets().into() }),
            IpAddr::V4(i) => IpAddress::Ipv4(Ipv4Address { addr: i.octets().into() }),
        };
        if self.0.port() == 0 {
            TargetIpAddrInfo::Ip(TargetIp { ip, scope_id })
        } else {
            TargetIpAddrInfo::IpPort(TargetIpPort { ip, scope_id, port: self.0.port() })
        }
    }
}

impl Into<TargetIpAddrInfo> for TargetIpAddr {
    fn into(self) -> TargetIpAddrInfo {
        (&self).into()
    }
}

impl Into<TargetAddrInfo> for &TargetIpAddr {
    fn into(self) -> TargetAddrInfo {
        let s: TargetIpAddrInfo = self.into();
        match s {
            TargetIpAddrInfo::IpPort(i) => TargetAddrInfo::IpPort(i),
            TargetIpAddrInfo::Ip(i) => TargetAddrInfo::Ip(i),
        }
    }
}

impl Into<TargetAddrInfo> for TargetIpAddr {
    fn into(self) -> TargetAddrInfo {
        (&self).into()
    }
}

impl From<TargetIpAddrInfo> for TargetIpAddr {
    fn from(t: TargetIpAddrInfo) -> Self {
        (&t).into()
    }
}

impl From<TargetIp> for TargetIpAddr {
    fn from(t: TargetIp) -> Self {
        let (addr, scope): (IpAddr, u32) = match t.ip {
            IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), t.scope_id),
            IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), t.scope_id),
        };
        TargetIpAddr::new(addr, scope, 0)
    }
}

impl From<&TargetIpAddrInfo> for TargetIpAddr {
    fn from(t: &TargetIpAddrInfo) -> Self {
        let (addr, scope, port): (IpAddr, u32, u16) = match t {
            TargetIpAddrInfo::Ip(ip) => match ip.ip {
                IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), ip.scope_id, 0),
                IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), ip.scope_id, 0),
            },
            TargetIpAddrInfo::IpPort(ip) => match ip.ip {
                IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), ip.scope_id, ip.port),
                IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), ip.scope_id, ip.port),
            },
        };

        TargetIpAddr::new(addr, scope, port)
    }
}

impl TryFrom<&TargetAddr> for TargetIpAddr {
    type Error = NotANetworkAddress;

    fn try_from(value: &TargetAddr) -> std::result::Result<Self, Self::Error> {
        match value {
            TargetAddr::Net(socket_addr) => Ok(TargetIpAddr(*socket_addr)),
            TargetAddr::VSockCtx(_) => Err(NotANetworkAddress),
        }
    }
}

impl TryFrom<TargetAddr> for TargetIpAddr {
    type Error = NotANetworkAddress;

    fn try_from(value: TargetAddr) -> std::result::Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl TryFrom<&TargetAddrInfo> for TargetIpAddr {
    type Error = NotANetworkAddress;

    fn try_from(value: &TargetAddrInfo) -> std::result::Result<Self, Self::Error> {
        Self::try_from(TargetAddr::from(value))
    }
}

impl TryFrom<TargetAddrInfo> for TargetIpAddr {
    type Error = NotANetworkAddress;

    fn try_from(value: TargetAddrInfo) -> std::result::Result<Self, Self::Error> {
        Self::try_from(TargetAddr::from(value))
    }
}

impl std::fmt::Display for TargetIpAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.ip() {
            IpAddr::V4(ip) => {
                write!(f, "{}", ip)?;
            }
            IpAddr::V6(ip) => {
                write!(f, "{}", ip)?;
                if ip.is_link_local_addr() && self.scope_id() > 0 {
                    write!(f, "%{}", scope_id_to_name(self.scope_id()))?;
                }
            }
        }
        Ok(())
    }
}

/// Represents an address associated with a target, network or otherwise.
#[derive(Clone, Debug, Copy)]
pub enum TargetAddr {
    Net(SocketAddr),
    VSockCtx(u32),
}

// Only compare `TargetAddr` by ip and port, since we want to deduplicate targets if they are
// addressable over multiple IPv6 interfaces.
impl std::hash::Hash for TargetAddr {
    fn hash<H>(&self, state: &mut H)
    where
        H: std::hash::Hasher,
    {
        match self {
            TargetAddr::Net(addr) => (addr.ip(), addr.port()).hash(state),
            TargetAddr::VSockCtx(cid) => cid.hash(state),
        }
    }
}

impl PartialEq for TargetAddr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TargetAddr::Net(addr), TargetAddr::Net(other)) => {
                addr.ip() == other.ip() && addr.port() == other.port()
            }
            (TargetAddr::Net(_), _) | (_, TargetAddr::Net(_)) => false,
            (TargetAddr::VSockCtx(cid), TargetAddr::VSockCtx(other)) => cid == other,
        }
    }
}

impl Eq for TargetAddr {}

impl Ord for TargetAddr {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (TargetAddr::Net(addr), TargetAddr::Net(other)) => {
                addr.ip().cmp(&other.ip()).then(addr.port().cmp(&other.port()))
            }
            (TargetAddr::Net(_), TargetAddr::VSockCtx(_)) => Ordering::Less,
            (TargetAddr::VSockCtx(_), TargetAddr::Net(_)) => Ordering::Greater,
            (TargetAddr::VSockCtx(cid), TargetAddr::VSockCtx(other)) => cid.cmp(other),
        }
    }
}

impl PartialOrd for TargetAddr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Into<TargetAddrInfo> for &TargetAddr {
    fn into(self) -> TargetAddrInfo {
        match self {
            TargetAddr::Net(addr) => TargetIpAddr::from(*addr).into(),
            TargetAddr::VSockCtx(cid) => TargetAddrInfo::Vsock(TargetVSockCtx { cid: *cid }),
        }
    }
}

impl Into<TargetAddrInfo> for TargetAddr {
    fn into(self) -> TargetAddrInfo {
        (&self).into()
    }
}

impl TryInto<TargetIpAddrInfo> for TargetAddr {
    type Error = NotANetworkAddress;

    fn try_into(self) -> std::result::Result<TargetIpAddrInfo, Self::Error> {
        Ok(TargetIpAddr::try_from(self)?.into())
    }
}

impl From<TargetAddrInfo> for TargetAddr {
    fn from(t: TargetAddrInfo) -> Self {
        (&t).into()
    }
}

impl From<TargetIpAddr> for TargetAddr {
    fn from(t: TargetIpAddr) -> Self {
        TargetAddr::Net(t.0)
    }
}

impl From<&TargetIpAddr> for TargetAddr {
    fn from(t: &TargetIpAddr) -> Self {
        Self::from(t.clone())
    }
}

impl From<TargetIp> for TargetAddr {
    fn from(t: TargetIp) -> Self {
        let (addr, scope): (IpAddr, u32) = match t.ip {
            IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), t.scope_id),
            IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), t.scope_id),
        };
        TargetAddr::new(addr, scope, 0)
    }
}

impl From<&TargetAddrInfo> for TargetAddr {
    fn from(t: &TargetAddrInfo) -> Self {
        let (addr, scope, port): (IpAddr, u32, u16) = match t {
            TargetAddrInfo::Ip(ip) => match ip.ip {
                IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), ip.scope_id, 0),
                IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), ip.scope_id, 0),
            },
            TargetAddrInfo::IpPort(ip) => match ip.ip {
                IpAddress::Ipv6(Ipv6Address { addr }) => (addr.into(), ip.scope_id, ip.port),
                IpAddress::Ipv4(Ipv4Address { addr }) => (addr.into(), ip.scope_id, ip.port),
            },
            TargetAddrInfo::Vsock(ctx) => return TargetAddr::VSockCtx(ctx.cid),
            // TODO(https://fxbug.dev/42130068): Add serial numbers.,
        };

        TargetAddr::new(addr, scope, port)
    }
}

impl From<SocketAddr> for TargetAddr {
    fn from(s: SocketAddr) -> Self {
        Self::Net(s)
    }
}

/// Construct a new TargetAddr from a string representation of the form
/// accepted by std::net::SocketAddr, e.g. 127.0.0.1:22, or [fe80::1%1]:0.
impl FromStr for TargetAddr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let sa = s.parse::<SocketAddr>()?;
        Ok(Self::from(sa))
    }
}

impl TargetAddr {
    // TODO(colnnelson): clean up with wrapper types for `scope` and `port` to
    // avoid the "zero is default" legacy.
    pub fn new(ip: IpAddr, scope_id: u32, port: u16) -> Self {
        match ip {
            IpAddr::V6(addr) => {
                Self::Net(SocketAddr::V6(SocketAddrV6::new(addr, port, 0, scope_id)))
            }
            IpAddr::V4(addr) => Self::Net(SocketAddr::V4(SocketAddrV4::new(addr, port))),
        }
    }

    pub fn scope_id(&self) -> u32 {
        match self {
            TargetAddr::Net(SocketAddr::V6(v6)) => v6.scope_id(),
            _ => 0,
        }
    }

    pub fn set_scope_id(&mut self, scope_id: u32) {
        match self {
            TargetAddr::Net(SocketAddr::V6(mut v6)) => v6.set_scope_id(scope_id),
            _ => {}
        }
    }

    pub fn ip(&self) -> Option<IpAddr> {
        match self {
            TargetAddr::Net(addr) => Some(addr.ip()),
            TargetAddr::VSockCtx(_) => None,
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            TargetAddr::Net(addr) => Some(addr.port()),
            TargetAddr::VSockCtx(_) => None,
        }
    }

    pub fn cid(&self) -> Option<u32> {
        match self {
            TargetAddr::VSockCtx(cid) => Some(*cid),
            TargetAddr::Net(_) => None,
        }
    }

    pub fn set_port(&mut self, new_port: u16) -> Result<(), NotANetworkAddress> {
        match self {
            TargetAddr::Net(addr) => {
                addr.set_port(new_port);
                Ok(())
            }
            TargetAddr::VSockCtx(_) => Err(NotANetworkAddress),
        }
    }

    pub fn optional_port_str(&self) -> String {
        match self {
            TargetAddr::Net(addr) => match (addr.ip(), addr.port()) {
                (_, 0) | (_, 22) => format!("{self}"),
                (IpAddr::V6(_), p) => format!("[{self}]:{p}"),
                (_, p) => format!("{self}:{p}"),
            },
            TargetAddr::VSockCtx(_) => format!("{self}"),
        }
    }
}

impl std::fmt::Display for TargetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetAddr::Net(addr) => write!(f, "{}", TargetIpAddr::from(*addr)),
            TargetAddr::VSockCtx(cid) => write!(f, "vsock:cid:{cid}"),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[fuchsia::test]
    fn test_port_str_with_ipv6_bracket() {
        let v6addr: std::net::SocketAddr = std::net::SocketAddrV6::new(
            std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            8080,
            0,
            1,
        )
        .into();
        let addr: TargetAddr = v6addr.into();
        let s = addr.optional_port_str();
        assert_eq!(&s, "[2001:db8::1]:8080");
    }
}
