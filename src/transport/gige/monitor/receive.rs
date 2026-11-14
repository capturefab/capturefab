//! Host-only multicast membership and per-datagram destination/interface evidence.
use super::super::bootstrap::Interface;
use super::{Route, before};
use anyhow::Result;
use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    time::Instant,
};

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
mod unix;
#[cfg(windows)]
mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Datagram {
    pub bytes: usize,
    pub source: SocketAddrV4,
    pub destination: Ipv4Addr,
    pub interface_index: u32,
}

fn bound(interface: &Interface, port: u16, reuse: bool) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(reuse)?;
    #[cfg(target_os = "macos")]
    if reuse {
        // Darwin needs duplicate-port binding for independent multicast listeners.
        // Every packet still requires this receiver's exact group/interface/source.
        socket.set_reuse_port(true)?;
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        let index = std::num::NonZeroU32::new(interface.index)
            .ok_or_else(|| io::Error::other("monitor interface index is zero"))?;
        let pinned = socket.bind_device_by_index_v4(Some(index));
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let pinned = pinned.or_else(|_| socket.bind_device(Some(interface.name.as_bytes())));
        pinned?;
    }
    #[cfg(windows)]
    windows::pin(&socket, interface.index)?;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        windows
    )))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "multicast monitor interface evidence is unavailable on this platform",
    ));
    let address = if port == 0 {
        interface.address
    } else {
        Ipv4Addr::UNSPECIFIED
    };
    socket.bind(&SocketAddrV4::new(address, port).into())?;
    Ok(socket.into())
}
pub(super) fn control_socket(interface: &Interface) -> io::Result<UdpSocket> {
    bound(interface, 0, false)
}

#[cfg(windows)]
type SocketId = usize;
#[cfg(not(windows))]
type SocketId = u64;

fn socket_id(socket: &UdpSocket) -> io::Result<SocketId> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        u64::try_from(socket.as_raw_fd()).map_err(|e| io::Error::other(e.to_string()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket as _;
        SocketId::try_from(socket.as_raw_socket()).map_err(|e| io::Error::other(e.to_string()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = socket;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "monitor socket evidence unsupported",
        ))
    }
}

pub(super) struct Evidence {
    socket_id: SocketId,
    #[cfg(windows)]
    packet_info: windows::PacketInfo,
}
impl Evidence {
    pub(super) fn new(socket: &UdpSocket) -> io::Result<Self> {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        unix::prepare(socket)?;
        #[cfg(windows)]
        let packet_info = windows::PacketInfo::new(socket)?;
        Ok(Self {
            socket_id: socket_id(socket)?,
            #[cfg(windows)]
            packet_info,
        })
    }
    pub(super) fn receive(
        &self,
        socket: &UdpSocket,
        buffer: &mut [u8],
    ) -> io::Result<Option<Datagram>> {
        if socket_id(socket)? != self.socket_id {
            return Err(io::Error::other(
                "monitor packet evidence belongs to another socket",
            ));
        }
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        return unix::receive(socket, buffer);
        #[cfg(windows)]
        return self.packet_info.receive(socket, buffer);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            windows
        )))]
        {
            let _ = (socket, buffer);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "monitor receive evidence unavailable",
            ))
        }
    }
}

pub(super) struct Receiver {
    socket: UdpSocket,
    group: Ipv4Addr,
    interface: Ipv4Addr,
    evidence: Evidence,
}
impl Receiver {
    pub(super) fn open(route: &Route, until: Instant) -> Result<Self> {
        before(until)?;
        let socket = bound(&route.interface, route.port, true)?;
        super::super::request_receive_buffer(route.payload, |size| {
            super::super::set_receive_buffer(&socket, size)
        });
        socket.set_nonblocking(true)?;
        let evidence = Evidence::new(&socket)?;
        before(until)?;
        socket.join_multicast_v4(&route.group, &route.interface.address)?;
        before(until)?;
        Ok(Self {
            socket,
            group: route.group,
            interface: route.interface.address,
            evidence,
        })
    }
    pub(super) fn receive(&self, buffer: &mut [u8]) -> io::Result<Option<Datagram>> {
        self.evidence.receive(&self.socket, buffer)
    }
    pub(super) fn buffer_size(&self) -> Option<u64> {
        socket2::SockRef::from(&self.socket)
            .recv_buffer_size()
            .ok()
            .and_then(|n| u64::try_from(n).ok())
    }
    pub(super) fn leave(self) -> io::Result<()> {
        self.socket.leave_multicast_v4(&self.group, &self.interface)
    }
}
