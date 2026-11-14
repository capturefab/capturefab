//! Receive-only Winsock packet information, including exact destination and interface.
use super::Datagram;
use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::windows::io::AsRawSocket as _,
    ptr,
};
use windows_sys::{
    Win32::Networking::WinSock::{
        AF_INET, CMSGHDR, IN_PKTINFO, IP_PKTINFO, IPPROTO_IP, LPFN_WSARECVMSG, MSG_CTRUNC,
        MSG_TRUNC, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR, SOCKADDR_IN, SOCKET, SOCKET_ERROR,
        WSABUF, WSAGetLastError, WSAID_WSARECVMSG, WSAIoctl, WSAMSG, setsockopt,
    },
    core::GUID,
};

#[derive(Clone, Copy)]
pub(super) struct PacketInfo {
    receive: LPFN_WSARECVMSG,
}

impl PacketInfo {
    pub(super) fn new(socket: &UdpSocket) -> io::Result<Self> {
        let socket = raw(socket)?;
        let enabled = 1_u32;
        // SAFETY: the option is a live, correctly sized DWORD and the socket
        // remains owned by the caller. No device datagram is sent here.
        let result = unsafe {
            setsockopt(
                socket,
                IPPROTO_IP,
                IP_PKTINFO,
                (&raw const enabled).cast(),
                i32::try_from(size_of_val(&enabled)).map_err(invalid)?,
            )
        };
        if result == SOCKET_ERROR {
            return Err(last_error());
        }
        let receive: LPFN_WSARECVMSG = extension(socket, WSAID_WSARECVMSG)?;
        if receive.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Winsock packet-info functions are unavailable",
            ));
        }
        Ok(Self { receive })
    }

    pub(super) fn receive(
        &self,
        socket: &UdpSocket,
        buffer: &mut [u8],
    ) -> io::Result<Option<Datagram>> {
        let receive = self
            .receive
            .ok_or_else(|| io::Error::other("missing Winsock receive function"))?;
        let mut address = SOCKADDR_IN::default();
        let mut data = WSABUF {
            len: u32::try_from(buffer.len()).map_err(invalid)?,
            buf: buffer.as_mut_ptr(),
        };
        let mut control = Control::new();
        let bytes = control.bytes();
        let mut message = WSAMSG {
            name: (&raw mut address).cast::<SOCKADDR>(),
            namelen: i32::try_from(size_of_val(&address)).map_err(invalid)?,
            lpBuffers: &raw mut data,
            dwBufferCount: 1,
            Control: WSABUF {
                len: u32::try_from(bytes.len()).map_err(invalid)?,
                buf: bytes.as_mut_ptr(),
            },
            dwFlags: 0,
        };
        let mut received = 0;
        // SAFETY: all buffers are writable for their stated lengths and remain
        // alive until this synchronous, non-overlapped call returns.
        let result = unsafe {
            receive(
                raw(socket)?,
                &raw mut message,
                &raw mut received,
                ptr::null_mut(),
                None,
            )
        };
        if result == SOCKET_ERROR {
            let error = last_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        if message.dwFlags & (MSG_TRUNC | MSG_CTRUNC) != 0 || received as usize > buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated GigE monitor datagram or packet info",
            ));
        }
        if address.sin_family != AF_INET
            || message.namelen != i32::try_from(size_of_val(&address)).map_err(invalid)?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid IPv4 monitor reply address",
            ));
        }
        let length = usize::try_from(message.Control.len).map_err(invalid)?;
        let (destination, interface_index) = packet_evidence(
            bytes
                .get(..length)
                .ok_or_else(|| invalid("packet-info length exceeds buffer"))?,
        )?;
        // SAFETY: Winsock returned a checked AF_INET sockaddr_in; S_addr is
        // the active representation of its four IPv4 network bytes.
        let source = Ipv4Addr::from(unsafe { address.sin_addr.S_un.S_addr }.to_ne_bytes());
        Ok(Some(Datagram {
            bytes: received as usize,
            source: SocketAddrV4::new(source, u16::from_be(address.sin_port)),
            destination,
            interface_index,
        }))
    }
}

fn raw(socket: &UdpSocket) -> io::Result<SOCKET> {
    SOCKET::try_from(socket.as_raw_socket()).map_err(invalid)
}

fn extension<T: Copy + Default>(socket: SOCKET, guid: GUID) -> io::Result<T> {
    let mut function = T::default();
    let mut returned = 0;
    // SAFETY: callers pair each documented GUID with its corresponding
    // optional function-pointer type. Buffers outlive the synchronous ioctl.
    let result = unsafe {
        WSAIoctl(
            socket,
            SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&raw const guid).cast(),
            u32::try_from(size_of::<GUID>()).map_err(invalid)?,
            (&raw mut function).cast(),
            u32::try_from(size_of::<T>()).map_err(invalid)?,
            &raw mut returned,
            ptr::null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR {
        return Err(last_error());
    }
    if returned as usize != size_of::<T>() {
        return Err(io::Error::other("invalid Winsock extension function size"));
    }
    Ok(function)
}

struct Control([usize; 8]);
impl Control {
    const fn new() -> Self {
        Self([0; 8])
    }
    const fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: usize backing provides native CMSG alignment; the byte view
        // spans exactly this live initialized array and is exclusively borrowed.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), size_of_val(&self.0)) }
    }
}

fn aligned(length: usize) -> io::Result<usize> {
    let alignment = size_of::<usize>();
    length
        .checked_add(alignment - 1)
        .map(|n| n & !(alignment - 1))
        .ok_or_else(|| invalid("packet-info alignment overflow"))
}

fn packet_evidence(mut bytes: &[u8]) -> io::Result<(Ipv4Addr, u32)> {
    let mut found = None;
    let header = aligned(size_of::<CMSGHDR>())?;
    while bytes.len() >= size_of::<CMSGHDR>() {
        let length = usize::from_ne_bytes(bytes[..size_of::<usize>()].try_into().map_err(invalid)?);
        if length < header || length > bytes.len() {
            return Err(invalid("invalid Winsock control record length"));
        }
        let level = std::mem::offset_of!(CMSGHDR, cmsg_level);
        let kind = std::mem::offset_of!(CMSGHDR, cmsg_type);
        let level = i32::from_ne_bytes(bytes[level..level + 4].try_into().map_err(invalid)?);
        let kind = i32::from_ne_bytes(bytes[kind..kind + 4].try_into().map_err(invalid)?);
        if level == IPPROTO_IP && kind == IP_PKTINFO {
            if length != header + size_of::<IN_PKTINFO>() || found.is_some() {
                return Err(invalid("invalid or duplicate Winsock packet info"));
            }
            let data: [u8; 8] = bytes
                .get(header..length)
                .ok_or_else(|| invalid("missing packet info body"))?
                .try_into()
                .map_err(invalid)?;
            let (address, index) = data.split_at(4);
            found = Some((
                Ipv4Addr::from(<[u8; 4]>::try_from(address).map_err(invalid)?),
                u32::from_ne_bytes(index.try_into().map_err(invalid)?),
            ));
        }
        let next = aligned(length)?;
        if next >= bytes.len() {
            break;
        }
        bytes = &bytes[next..];
    }
    let (destination, index) = found.ok_or_else(|| invalid("missing monitor packet info"))?;
    if index == 0 {
        return Err(invalid("zero monitor receive interface"));
    }
    Ok((destination, index))
}

fn last_error() -> io::Error {
    // SAFETY: thread-local Winsock error retrieval has no pointer arguments.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

/// Pin outbound read-only control traffic. Receive identity is checked independently per packet.
pub(super) fn pin(socket: &socket2::Socket, index: u32) -> io::Result<()> {
    use windows_sys::Win32::Networking::WinSock::IP_UNICAST_IF;
    if index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "monitor interface index is zero",
        ));
    }
    let network = index.to_be();
    // SAFETY: the correctly sized live DWORD and owned socket cover this synchronous option call.
    let result = unsafe {
        setsockopt(
            SOCKET::try_from(socket.as_raw_socket()).map_err(invalid)?,
            IPPROTO_IP,
            IP_UNICAST_IF,
            (&raw const network).cast(),
            i32::try_from(size_of_val(&network)).map_err(invalid)?,
        )
    };
    if result == SOCKET_ERROR {
        return Err(last_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(destination: [u8; 4], index: u32) -> Vec<u8> {
        let header = aligned(size_of::<CMSGHDR>()).unwrap();
        let mut bytes = vec![0_u8; aligned(header + 8).unwrap()];
        bytes[..size_of::<usize>()].copy_from_slice(&(header + 8).to_ne_bytes());
        let level = std::mem::offset_of!(CMSGHDR, cmsg_level);
        let kind = std::mem::offset_of!(CMSGHDR, cmsg_type);
        bytes[level..level + 4].copy_from_slice(&IPPROTO_IP.to_ne_bytes());
        bytes[kind..kind + 4].copy_from_slice(&IP_PKTINFO.to_ne_bytes());
        bytes[header..header + 4].copy_from_slice(&destination);
        bytes[header + 4..header + 8].copy_from_slice(&index.to_ne_bytes());
        bytes
    }
    #[test]
    fn windows_requires_exact_destination_and_nonzero_interface_evidence() {
        let valid = record([239, 255, 42, 19], 7);
        assert_eq!(
            packet_evidence(&valid).unwrap(),
            (Ipv4Addr::new(239, 255, 42, 19), 7)
        );
        for invalid in [
            Vec::new(),
            record([239, 255, 42, 19], 0),
            [valid.clone(), valid].concat(),
        ] {
            assert_eq!(
                packet_evidence(&invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
