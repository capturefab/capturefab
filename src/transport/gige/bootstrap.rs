//! Explicit, interface-scoped bootstrap datagrams. No controller acquisition,
//! command retries or device cleanup belongs on this path.
use anyhow::{Context as _, Result};
use serde::Serialize;
use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
};

#[cfg(not(any(windows, target_os = "macos")))]
use std::net::SocketAddr;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
mod unix;
#[cfg(windows)]
mod windows;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Interface {
    pub name: String,
    pub index: u32,
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Datagram {
    pub bytes: usize,
    pub source: SocketAddrV4,
    pub selected_interface: bool,
}

/// One nonblocking datagram attempt; the caller owns the overall deadline.
pub(crate) trait Channel {
    fn send(&mut self, packet: &[u8]) -> io::Result<usize>;
    fn receive(&mut self, buffer: &mut [u8]) -> io::Result<Option<Datagram>>;
    #[cfg(test)]
    fn test_source_port(&self) -> u16 {
        super::GVCP_PORT
    }
}

pub(crate) struct Socket {
    socket: UdpSocket,
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    interface: Interface,
    #[cfg(windows)]
    packet_info: windows::PacketInfo,
    #[cfg(target_os = "macos")]
    packet_info: macos::PacketInfo,
}

impl Socket {
    pub(crate) fn open(interface: &Interface) -> Result<Self> {
        let socket = bound_socket(interface).context("binding the requested GigE interface")?;
        socket.set_broadcast(true)?;
        socket.set_nonblocking(true)?;
        #[cfg(windows)]
        let packet_info = windows::PacketInfo::new(&socket, interface)?;
        #[cfg(target_os = "macos")]
        let packet_info = macos::PacketInfo::new(&socket, interface)?;
        Ok(Self {
            socket,
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
            interface: interface.clone(),
            #[cfg(any(windows, target_os = "macos"))]
            packet_info,
        })
    }

    fn send_to(&self, packet: &[u8], destination: SocketAddrV4) -> io::Result<usize> {
        #[cfg(windows)]
        return self.packet_info.send(&self.socket, packet, destination);
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        return unix::send(&self.socket, &self.interface, packet, destination);
        #[cfg(not(any(
            windows,
            target_os = "linux",
            target_os = "android",
            target_os = "macos"
        )))]
        self.socket.send_to(packet, destination)
    }

    /// Tests use an ephemeral loopback peer; release code has no destination override.
    #[cfg(test)]
    pub(crate) fn test_send_to(
        &self,
        packet: &[u8],
        destination: SocketAddrV4,
    ) -> io::Result<usize> {
        self.send_to(packet, destination)
    }
}

impl Channel for Socket {
    fn send(&mut self, packet: &[u8]) -> io::Result<usize> {
        let destination = SocketAddrV4::new(Ipv4Addr::BROADCAST, super::GVCP_PORT);
        self.send_to(packet, destination)
    }

    fn receive(&mut self, buffer: &mut [u8]) -> io::Result<Option<Datagram>> {
        #[cfg(any(windows, target_os = "macos"))]
        return self.packet_info.receive(&self.socket, buffer);
        #[cfg(not(any(windows, target_os = "macos")))]
        match self.socket.recv_from(buffer) {
            Ok((bytes, SocketAddr::V4(source))) => Ok(Some(Datagram {
                bytes,
                source,
                selected_interface: true,
            })),
            Ok((_, SocketAddr::V6(_))) => Err(io::Error::other("non-IPv4 bootstrap reply")),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn bound_socket(interface: &Interface) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let index = std::num::NonZeroU32::new(interface.index)
        .ok_or_else(|| io::Error::other("interface index is zero"))?;
    let pinned = socket.bind_device_by_index_v4(Some(index));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let pinned = pinned.or_else(|_| socket.bind_device(Some(interface.name.as_bytes())));
    pinned?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into())?;
    Ok(socket.into())
}

#[cfg(windows)]
fn bound_socket(_interface: &Interface) -> io::Result<UdpSocket> {
    UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn bound_socket(_interface: &Interface) -> io::Result<UdpSocket> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "interface-scoped GigE bootstrap is unsupported on this platform",
    ))
}
