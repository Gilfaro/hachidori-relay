// SPDX-License-Identifier: GPL-3.0-or-later
//! Sockets: binding listeners the way upstream does, and finding this computer's addresses.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use serde::Serialize;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

/// How many times a rebind retries while the previous socket lets go of the port (40 × 50 ms).
const REBIND_ATTEMPTS: u32 = 40;
const REBIND_PAUSE: Duration = Duration::from_millis(50);
const BACKLOG: i32 = 128;

/// The address a listener binds: this computer only, or every interface.
pub(crate) const fn listen_address(network: bool, port: u16) -> SocketAddrV4 {
    SocketAddrV4::new(if network { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST }, port)
}

/// Binds a nonblocking IPv4 listener.
///
/// `SO_REUSEADDR` frees the port straight after a restart on Unix. Windows would
/// instead let two listeners share the port, so there it is left off.
pub(crate) fn bind(address: SocketAddrV4) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.bind(&address.into())?;
    socket.listen(BACKLOG)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

/// Binds, retrying `EADDRINUSE` for up to 2 s when `rebinding`: the socket that
/// was just closed may hold the port for a moment.
pub(crate) async fn bind_retrying(address: SocketAddrV4, rebinding: bool) -> io::Result<TcpListener> {
    let mut attempt = 1;
    loop {
        match bind(address) {
            Err(error) if rebinding && error.kind() == io::ErrorKind::AddrInUse && attempt < REBIND_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(REBIND_PAUSE).await;
            }
            result => return result,
        }
    }
}

/// One address other computers reach this one at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LocalAddress {
    pub(crate) address: String,
    pub(crate) kind: &'static str,
}

/// Where a UDP socket would send from is this computer's address on that route:
/// first Tailscale's own resolver, then the default route. Nothing is sent to
/// either address.
const ADDRESS_PROBES: [Ipv4Addr; 2] = [Ipv4Addr::new(100, 100, 100, 100), Ipv4Addr::new(8, 8, 8, 8)];

/// `100.64.0.0/10`, which only marks an address as Tailscale's.
const fn is_tailscale(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    first == 100 && second & 0xC0 == 64
}

/// The addresses other computers reach this one at, Tailscale's first if there is one.
pub(crate) fn local_addresses() -> Vec<LocalAddress> {
    let mut found: Vec<LocalAddress> = Vec::with_capacity(ADDRESS_PROBES.len());
    for target in ADDRESS_PROBES {
        let Some(address) = route_source(target) else { continue };
        if address.is_loopback() || address.is_unspecified() {
            continue;
        }
        let address_text = address.to_string();
        if found.iter().any(|entry| entry.address == address_text) {
            continue;
        }
        let kind = if is_tailscale(address) { "tailscale" } else { "local" };
        found.push(LocalAddress { address: address_text, kind });
    }
    found
}

fn route_source(target: Ipv4Addr) -> Option<Ipv4Addr> {
    let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect((target, 53)).ok()?;
    match probe.local_addr().ok()? {
        SocketAddr::V4(local) => Some(*local.ip()),
        SocketAddr::V6(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_range_is_100_64_slash_10() {
        assert!(is_tailscale(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_tailscale(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_tailscale(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_tailscale(Ipv4Addr::new(100, 63, 0, 1)));
        assert!(!is_tailscale(Ipv4Addr::new(192, 168, 1, 2)));
    }

    #[test]
    fn local_addresses_are_never_loopback_and_never_repeat() {
        let found = local_addresses();
        for (index, entry) in found.iter().enumerate() {
            let address: Ipv4Addr = entry.address.parse().unwrap_or(Ipv4Addr::LOCALHOST);
            assert!(!address.is_loopback());
            assert!(entry.kind == "local" || entry.kind == "tailscale");
            assert!(found[index + 1..].iter().all(|other| other.address != entry.address));
        }
    }

    #[tokio::test]
    async fn bind_reports_an_occupied_port() {
        let first = bind(listen_address(false, 0)).unwrap_or_else(|error| panic!("{error}"));
        let port = first.local_addr().map(|address| address.port()).unwrap_or_default();
        let error = bind(listen_address(false, port)).err().map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::AddrInUse));
    }
}
