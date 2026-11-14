//! Checked native IPv4 destination and arrival-interface control messages.
use super::Datagram;
use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::fd::AsRawFd as _,
};

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}
fn option(socket: &UdpSocket, kind: i32) -> io::Result<()> {
    let yes = 1_i32;
    // SAFETY: a live fixed integer and owned socket cover this synchronous option call.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            kind,
            (&raw const yes).cast(),
            u32::try_from(size_of_val(&yes)).map_err(invalid)?,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(super) fn prepare(socket: &UdpSocket) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        option(socket, libc::IP_PKTINFO)
    }
    #[cfg(target_os = "macos")]
    {
        option(socket, libc::IP_RECVIF)?;
        option(socket, libc::IP_RECVDSTADDR)
    }
}
pub(super) fn receive(socket: &UdpSocket, buffer: &mut [u8]) -> io::Result<Option<Datagram>> {
    // SAFETY: zero is a valid initial address; recvmsg fills its checked AF_INET representation.
    let mut source: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut data = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    let mut control = [0_usize; 32];
    let mut msg = libc::msghdr {
        msg_name: (&raw mut source).cast(),
        msg_namelen: u32::try_from(size_of_val(&source)).map_err(invalid)?,
        msg_iov: &raw mut data,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr().cast(),
        #[cfg(target_os = "macos")]
        msg_controllen: u32::try_from(size_of_val(&control)).map_err(invalid)?,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        msg_controllen: size_of_val(&control),
        msg_flags: 0,
    };
    // SAFETY: all writable buffers are aligned and remain live for their advertised extents.
    let received = unsafe { libc::recvmsg(socket.as_raw_fd(), &raw mut msg, 0) };
    if received < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(e)
        };
    }
    let bytes = usize::try_from(received).map_err(invalid)?;
    #[cfg(target_os = "macos")]
    let used = usize::try_from(msg.msg_controllen).map_err(invalid)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let used = msg.msg_controllen;
    if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || bytes > buffer.len()
        || used > size_of_val(&control)
    {
        return Err(invalid("truncated monitor datagram or packet information"));
    }
    if i32::from(source.sin_family) != libc::AF_INET
        || usize::try_from(msg.msg_namelen).map_err(invalid)? != size_of_val(&source)
    {
        return Err(invalid("invalid monitor IPv4 source"));
    }
    // SAFETY: the initialized aligned backing and checked kernel length bound the borrowed view.
    let ancillary = unsafe { std::slice::from_raw_parts(control.as_ptr().cast::<u8>(), used) };
    let (destination, interface_index) = evidence(ancillary)?;
    Ok(Some(Datagram {
        bytes,
        source: SocketAddrV4::new(
            Ipv4Addr::from(source.sin_addr.s_addr.to_ne_bytes()),
            u16::from_be(source.sin_port),
        ),
        destination,
        interface_index,
    }))
}
fn evidence(mut raw: &[u8]) -> io::Result<(Ipv4Addr, u32)> {
    let mut destination = None;
    let mut interface = None;
    // SAFETY: native CMSG arithmetic with zero only calculates a small fixed header size.
    let header = unsafe { libc::CMSG_LEN(0) } as usize;
    while raw.len() >= size_of::<libc::cmsghdr>() {
        // SAFETY: the complete native header is present; unaligned read avoids imposing slice alignment.
        let cmsg = unsafe { raw.as_ptr().cast::<libc::cmsghdr>().read_unaligned() };
        #[cfg(target_os = "macos")]
        let length = usize::try_from(cmsg.cmsg_len).map_err(invalid)?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let length = cmsg.cmsg_len;
        if length < header || length > raw.len() {
            return Err(invalid("invalid monitor ancillary extent"));
        }
        let value = &raw[header..length];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if cmsg.cmsg_level == libc::IPPROTO_IP && cmsg.cmsg_type == libc::IP_PKTINFO {
            if value.len() != size_of::<libc::in_pktinfo>()
                || destination.is_some()
                || interface.is_some()
            {
                return Err(invalid("invalid or duplicate monitor packet info"));
            }
            // SAFETY: exact native structure size was checked; input may be unaligned.
            let info = unsafe { value.as_ptr().cast::<libc::in_pktinfo>().read_unaligned() };
            destination = Some(Ipv4Addr::from(info.ipi_addr.s_addr.to_ne_bytes()));
            interface = Some(u32::try_from(info.ipi_ifindex).map_err(invalid)?);
        }
        #[cfg(target_os = "macos")]
        if cmsg.cmsg_level == libc::IPPROTO_IP {
            if cmsg.cmsg_type == libc::IP_RECVDSTADDR {
                if destination.is_some() {
                    return Err(invalid("duplicate monitor destination evidence"));
                }
                let address: [u8; 4] = value.try_into().map_err(invalid)?;
                destination = Some(Ipv4Addr::from(address));
            } else if cmsg.cmsg_type == libc::IP_RECVIF {
                const PREFIX: usize = std::mem::offset_of!(libc::sockaddr_dl, sdl_data);
                let bytes = value
                    .first_chunk::<PREFIX>()
                    .ok_or_else(|| invalid("short monitor interface evidence"))?;
                if interface.is_some()
                    || usize::from(bytes[0]) < PREFIX
                    || usize::from(bytes[0]) > value.len()
                    || i32::from(bytes[1]) != libc::AF_LINK
                {
                    return Err(invalid("invalid monitor interface evidence"));
                }
                let at = std::mem::offset_of!(libc::sockaddr_dl, sdl_index);
                interface = Some(u32::from(u16::from_ne_bytes(
                    bytes[at..at + 2].try_into().map_err(invalid)?,
                )));
            }
        }
        // SAFETY: length is within the small control buffer; native arithmetic supplies platform alignment.
        let step =
            unsafe { libc::CMSG_SPACE(u32::try_from(length - header).map_err(invalid)?) } as usize;
        if step >= raw.len() {
            break;
        }
        raw = &raw[step..];
    }
    let destination = destination.ok_or_else(|| invalid("missing monitor destination evidence"))?;
    let interface = interface
        .filter(|i| *i != 0)
        .ok_or_else(|| invalid("missing monitor interface evidence"))?;
    Ok((destination, interface))
}

#[cfg(test)]
mod tests;
