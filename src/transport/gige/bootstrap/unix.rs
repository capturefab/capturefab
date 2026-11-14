//! Per-datagram source address while retaining wildcard broadcast reception.
use super::Interface;
use std::{
    io,
    net::{SocketAddrV4, UdpSocket},
    os::fd::AsRawFd as _,
};

fn packet_info(interface: &Interface) -> io::Result<libc::in_pktinfo> {
    if interface.index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index is zero",
        ));
    }
    Ok(libc::in_pktinfo {
        // Darwin gives a nonzero ancillary index precedence over spec_dst.
        // IP_BOUND_IF already pins the socket, so zero here preserves that
        // bound interface while selecting the explicit source IPv4 address.
        #[cfg(target_os = "macos")]
        ipi_ifindex: 0,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        ipi_ifindex: i32::try_from(interface.index).map_err(invalid)?,
        ipi_spec_dst: libc::in_addr {
            s_addr: u32::from_ne_bytes(interface.address.octets()),
        },
        ipi_addr: libc::in_addr { s_addr: 0 },
    })
}

pub(super) fn send(
    socket: &UdpSocket,
    interface: &Interface,
    packet: &[u8],
    destination: SocketAddrV4,
) -> io::Result<usize> {
    let info = packet_info(interface)?;
    let size = u32::try_from(size_of_val(&info)).map_err(invalid)?;
    // SAFETY: the native CMSG helpers only calculate lengths for this small
    // fixed packet-info structure; they receive no pointers.
    let length = unsafe { libc::CMSG_LEN(size) };
    // SAFETY: as above; this includes the platform's actual alignment padding.
    let space = unsafe { libc::CMSG_SPACE(size) };
    // SAFETY: zero requests only the aligned native control-header length.
    let header = unsafe { libc::CMSG_LEN(0) } as usize;
    let mut control = [0_usize; 8];
    if space as usize > size_of_val(&control) {
        return Err(io::Error::other(
            "native packet-info control exceeds its buffer",
        ));
    }
    let record = libc::cmsghdr {
        #[cfg(target_os = "macos")]
        cmsg_len: length,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        cmsg_len: length as usize,
        cmsg_level: libc::IPPROTO_IP,
        cmsg_type: libc::IP_PKTINFO,
    };
    // SAFETY: usize backing is sufficiently aligned and sized for the native
    // cmsghdr, and control remains exclusively owned throughout this call.
    unsafe {
        control.as_mut_ptr().cast::<libc::cmsghdr>().write(record);
    }
    if align_of::<libc::in_pktinfo>() > align_of::<u32>() || !header.is_multiple_of(4) {
        return Err(io::Error::other("unsupported native packet-info alignment"));
    }
    // SAFETY: the native structure is at most u32-aligned (checked above),
    // and the native header's whole-word offset is inside the checked buffer.
    unsafe {
        control
            .as_mut_ptr()
            .cast::<u32>()
            .wrapping_add(header / 4)
            .cast::<libc::in_pktinfo>()
            .write(info);
    }
    let address = libc::sockaddr_in {
        #[cfg(target_os = "macos")]
        sin_len: u8::try_from(size_of::<libc::sockaddr_in>()).map_err(invalid)?,
        sin_family: libc::AF_INET.try_into().map_err(invalid)?,
        sin_port: destination.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(destination.ip().octets()),
        },
        sin_zero: [0; 8],
    };
    let mut data = libc::iovec {
        iov_base: packet.as_ptr().cast_mut().cast(),
        iov_len: packet.len(),
    };
    let message = libc::msghdr {
        msg_name: (&raw const address).cast_mut().cast(),
        msg_namelen: u32::try_from(size_of_val(&address)).map_err(invalid)?,
        msg_iov: &raw mut data,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr().cast(),
        #[cfg(target_os = "macos")]
        msg_controllen: space,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        msg_controllen: space as usize,
        msg_flags: 0,
    };
    // SAFETY: every read-only address/data/control region outlives this single
    // nonblocking sendmsg call; no async operation retains these pointers.
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &raw const message, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(sent).map_err(invalid)
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::gige::bootstrap::{Channel as _, Socket};
    use anyhow::{Context as _, Result, ensure};
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        time::{Duration, Instant},
    };

    #[test]
    fn actual_loopback_packet_info_preserves_source_and_receive_interface() -> Result<()> {
        // Optional root-run isolated-container alias probe. Production ignores
        // this test-only variable. No host interface is modified by the test.
        let source = match std::env::var("CAPTUREFAB_TEST_BOOTSTRAP_SOURCE") {
            Ok(source) => source.parse::<Ipv4Addr>()?,
            Err(std::env::VarError::NotPresent) => Ipv4Addr::LOCALHOST,
            Err(error) => return Err(error.into()),
        };
        ensure!(
            source.is_loopback(),
            "bootstrap probe must stay on loopback"
        );
        let selected = if_addrs::get_if_addrs()?
            .into_iter()
            .find(|i| i.ip() == IpAddr::V4(source))
            .context("requested loopback alias must already be configured")?;
        let if_addrs::IfAddr::V4(address) = selected.addr else {
            anyhow::bail!("loopback source is not IPv4");
        };
        let interface = Interface {
            name: selected.name,
            index: selected.index.context("loopback interface has no index")?,
            address: source,
            netmask: address.netmask,
        };
        let mut socket = Socket::open(&interface)?;
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        peer.set_read_timeout(Some(Duration::from_secs(1)))?;
        let SocketAddr::V4(destination) = peer.local_addr()? else {
            anyhow::bail!("probe peer is not IPv4");
        };
        const PROBE: &[u8] = b"capturefab-loopback-packet-info";
        assert_eq!(
            send(&socket.socket, &interface, PROBE, destination)?,
            PROBE.len()
        );
        let mut received = [0; 128];
        let (count, sender) = peer.recv_from(&mut received)?;
        assert_eq!(&received[..count], PROBE);
        assert_eq!(sender.ip(), IpAddr::V4(source));
        assert_eq!(peer.send_to(b"loopback-reply", sender)?, 14);
        let deadline = Instant::now() + Duration::from_secs(1);
        let reply = loop {
            if let Some(reply) = socket.receive(&mut received)? {
                break reply;
            }
            ensure!(
                Instant::now() < deadline,
                "loopback reply did not reach the selected bootstrap interface"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(reply.selected_interface);
        assert_eq!(reply.source, destination);
        assert_eq!(&received[..reply.bytes], b"loopback-reply");
        Ok(())
    }

    #[test]
    fn explicit_source_alias_is_retained_independently_of_interface_primary() {
        let primary = Interface {
            name: "literal-nic".into(),
            index: 7,
            address: Ipv4Addr::new(10, 20, 30, 1),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
        };
        let selected = Interface {
            address: Ipv4Addr::new(192, 168, 50, 1),
            ..primary.clone()
        };
        let first = packet_info(&primary).unwrap();
        let alias = packet_info(&selected).unwrap();
        assert_eq!(first.ipi_spec_dst.s_addr.to_ne_bytes(), [10, 20, 30, 1]);
        assert_eq!(alias.ipi_spec_dst.s_addr.to_ne_bytes(), [192, 168, 50, 1]);
        assert_eq!(alias.ipi_addr.s_addr, 0);
        #[cfg(target_os = "macos")]
        assert_eq!(alias.ipi_ifindex, 0);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(alias.ipi_ifindex, 7);
    }
}
