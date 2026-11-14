//! Receive interface evidence for wildcard-bound Darwin broadcast sockets.
use super::{Datagram, Interface};
use std::{
    io,
    mem::offset_of,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::fd::AsRawFd as _,
};

#[derive(Clone, Copy)]
pub(super) struct PacketInfo {
    interface: u32,
}

impl PacketInfo {
    pub(super) fn new(socket: &UdpSocket, interface: &Interface) -> io::Result<Self> {
        if u16::try_from(interface.index).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Darwin receive-interface index exceeds sockaddr_dl",
            ));
        }
        let enabled = 1_i32;
        // SAFETY: the option points to a correctly sized live integer; no
        // datagram is transmitted while preparing receive-interface evidence.
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_RECVIF,
                (&raw const enabled).cast(),
                u32::try_from(size_of_val(&enabled)).map_err(invalid)?,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            interface: interface.index,
        })
    }

    pub(super) fn receive(
        self,
        socket: &UdpSocket,
        buffer: &mut [u8],
    ) -> io::Result<Option<Datagram>> {
        let mut source = libc::sockaddr_in {
            sin_len: 0,
            sin_family: 0,
            sin_port: 0,
            sin_addr: libc::in_addr { s_addr: 0 },
            sin_zero: [0; 8],
        };
        let mut data = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        let mut control = [0_usize; 16];
        let control_bytes = size_of_val(&control);
        let mut message = libc::msghdr {
            msg_name: (&raw mut source).cast(),
            msg_namelen: u32::try_from(size_of_val(&source)).map_err(invalid)?,
            msg_iov: &raw mut data,
            msg_iovlen: 1,
            msg_control: control.as_mut_ptr().cast(),
            msg_controllen: u32::try_from(control_bytes).map_err(invalid)?,
            msg_flags: 0,
        };
        // SAFETY: every data/address/control pointer is live, writable for its
        // advertised extent and correctly aligned for this synchronous call.
        let count = unsafe { libc::recvmsg(socket.as_raw_fd(), &raw mut message, 0) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        let bytes = usize::try_from(count).map_err(invalid)?;
        if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
            || bytes > buffer.len()
            || message.msg_controllen as usize > control_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated GigE bootstrap data or receive-interface evidence",
            ));
        }
        if i32::from(source.sin_family) != libc::AF_INET
            || message.msg_namelen as usize != size_of_val(&source)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid IPv4 bootstrap reply address",
            ));
        }
        // SAFETY: the array is initialized and remains live; the checked
        // kernel-written control length is within its allocated byte extent.
        let ancillary = unsafe {
            std::slice::from_raw_parts(
                control.as_ptr().cast::<u8>(),
                message.msg_controllen as usize,
            )
        };
        let selected_interface = packet_interface(ancillary)? == Some(self.interface);
        Ok(Some(Datagram {
            bytes,
            source: SocketAddrV4::new(
                Ipv4Addr::from(source.sin_addr.s_addr.to_ne_bytes()),
                u16::from_be(source.sin_port),
            ),
            selected_interface,
        }))
    }
}

fn packet_interface(mut bytes: &[u8]) -> io::Result<Option<u32>> {
    let mut found = None;
    // SAFETY: these native CMSG arithmetic helpers have no pointer arguments;
    // zero requests only the native aligned cmsghdr extent.
    let header = unsafe { libc::CMSG_LEN(0) } as usize;
    while bytes.len() >= size_of::<libc::cmsghdr>() {
        let length = u32::from_ne_bytes(bytes[..4].try_into().map_err(invalid)?) as usize;
        if length < header || length > bytes.len() {
            return Err(io::Error::other("invalid Darwin control record length"));
        }
        let level = offset_of!(libc::cmsghdr, cmsg_level);
        let kind = offset_of!(libc::cmsghdr, cmsg_type);
        let level = i32::from_ne_bytes(bytes[level..level + 4].try_into().map_err(invalid)?);
        let kind = i32::from_ne_bytes(bytes[kind..kind + 4].try_into().map_err(invalid)?);
        if level == libc::IPPROTO_IP && kind == libc::IP_RECVIF {
            if found.is_some() {
                return Err(io::Error::other(
                    "duplicate Darwin receive-interface evidence",
                ));
            }
            let address = &bytes[header..length];
            let index = offset_of!(libc::sockaddr_dl, sdl_index);
            const PREFIX_BYTES: usize = offset_of!(libc::sockaddr_dl, sdl_data);
            let prefix = address.first_chunk::<PREFIX_BYTES>().ok_or_else(|| {
                io::Error::other("short Darwin receive-interface sockaddr_dl prefix")
            })?;
            if usize::from(prefix[0]) < PREFIX_BYTES
                || usize::from(prefix[0]) > address.len()
                || i32::from(prefix[1]) != libc::AF_LINK
            {
                return Err(io::Error::other(
                    "invalid Darwin receive-interface sockaddr_dl prefix",
                ));
            }
            found = Some(u32::from(u16::from_ne_bytes(
                address[index..index + 2].try_into().map_err(invalid)?,
            )));
        }
        let payload = u32::try_from(length - header).map_err(invalid)?;
        // SAFETY: payload is bounded by the small actual control buffer; the
        // native helper accounts for Darwin's alignment instead of guessing it.
        let next = unsafe { libc::CMSG_SPACE(payload) } as usize;
        if next >= bytes.len() {
            break;
        }
        bytes = &bytes[next..];
    }
    Ok(found)
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: u16, prefix: usize) -> Vec<u8> {
        // SAFETY: both arithmetic helpers receive only small native structure
        // lengths and have no pointer arguments.
        let header = unsafe { libc::CMSG_LEN(0) } as usize;
        // SAFETY: prefix is 8 or 20 in these independent ancillary fixtures.
        let mut bytes =
            vec![0; unsafe { libc::CMSG_SPACE(u32::try_from(prefix).unwrap()) } as usize];
        bytes[..4].copy_from_slice(&u32::try_from(header + prefix).unwrap().to_ne_bytes());
        let level = offset_of!(libc::cmsghdr, cmsg_level);
        let kind = offset_of!(libc::cmsghdr, cmsg_type);
        bytes[level..level + 4].copy_from_slice(&libc::IPPROTO_IP.to_ne_bytes());
        bytes[kind..kind + 4].copy_from_slice(&libc::IP_RECVIF.to_ne_bytes());
        bytes[header] = u8::try_from(prefix).unwrap();
        bytes[header + 1] = u8::try_from(libc::AF_LINK).unwrap();
        let offset = header + offset_of!(libc::sockaddr_dl, sdl_index);
        bytes[offset..offset + 2].copy_from_slice(&index.to_ne_bytes());
        bytes
    }

    #[test]
    fn darwin_interface_evidence_accepts_prefix_and_full_sockaddr_extents() {
        for size in [
            offset_of!(libc::sockaddr_dl, sdl_data),
            size_of::<libc::sockaddr_dl>(),
        ] {
            let bytes = record(7, size);
            assert_eq!(packet_interface(&bytes).unwrap(), Some(7));
            let mut duplicate = bytes.clone();
            duplicate.extend_from_slice(&bytes);
            assert!(
                packet_interface(&duplicate)
                    .unwrap_err()
                    .to_string()
                    .contains("duplicate")
            );
            let mut short = bytes.clone();
            short.pop();
            assert!(
                packet_interface(&short)
                    .unwrap_err()
                    .to_string()
                    .contains("length")
            );
        }
        assert_eq!(packet_interface(&[]).unwrap(), None);
        let mut bytes = record(9, 8);
        // SAFETY: zero requests only the native aligned header length.
        let header = unsafe { libc::CMSG_LEN(0) } as usize;
        bytes[header] = 20;
        assert!(
            packet_interface(&bytes)
                .unwrap_err()
                .to_string()
                .contains("prefix")
        );
    }
}
