//! Winsock packet-info identifies both outgoing and incoming interfaces.
use super::{Datagram, Interface};
use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::windows::io::AsRawSocket as _,
    ptr,
};
use windows_sys::{
    Win32::Networking::WinSock::{
        AF_INET, CMSGHDR, IN_ADDR, IN_ADDR_0, IN_PKTINFO, IP_PKTINFO, IPPROTO_IP, LPFN_WSARECVMSG,
        LPFN_WSASENDMSG, MSG_CTRUNC, MSG_TRUNC, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR,
        SOCKADDR_IN, SOCKET, SOCKET_ERROR, WSABUF, WSAGetLastError, WSAID_WSARECVMSG,
        WSAID_WSASENDMSG, WSAIoctl, WSAMSG, setsockopt,
    },
    core::GUID,
};

#[derive(Clone, Copy)]
pub(super) struct PacketInfo {
    interface: u32,
    address: Ipv4Addr,
    send: LPFN_WSASENDMSG,
    receive: LPFN_WSARECVMSG,
}

impl PacketInfo {
    pub(super) fn new(socket: &UdpSocket, interface: &Interface) -> io::Result<Self> {
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
        let send: LPFN_WSASENDMSG = extension(socket, WSAID_WSASENDMSG)?;
        let receive: LPFN_WSARECVMSG = extension(socket, WSAID_WSARECVMSG)?;
        if send.is_none() || receive.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Winsock packet-info functions are unavailable",
            ));
        }
        Ok(Self {
            interface: interface.index,
            address: interface.address,
            send,
            receive,
        })
    }

    pub(super) fn send(
        &self,
        socket: &UdpSocket,
        packet: &[u8],
        destination: SocketAddrV4,
    ) -> io::Result<usize> {
        let send = self
            .send
            .ok_or_else(|| io::Error::other("missing Winsock send function"))?;
        let mut address = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: destination.port().to_be(),
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_addr: u32::from_ne_bytes(destination.ip().octets()),
                },
            },
            ..Default::default()
        };
        let mut buffer = WSABUF {
            len: u32::try_from(packet.len()).map_err(invalid)?,
            buf: packet.as_ptr().cast_mut(),
        };
        let mut control = Control::new();
        let header = aligned(size_of::<CMSGHDR>())?;
        let length = header
            .checked_add(size_of::<IN_PKTINFO>())
            .ok_or_else(|| io::Error::other("packet-info length overflow"))?;
        let bytes = control.bytes();
        bytes[..size_of::<usize>()].copy_from_slice(&length.to_ne_bytes());
        let level = std::mem::offset_of!(CMSGHDR, cmsg_level);
        bytes[level..level + size_of::<i32>()].copy_from_slice(&IPPROTO_IP.to_ne_bytes());
        let kind = std::mem::offset_of!(CMSGHDR, cmsg_type);
        bytes[kind..kind + size_of::<i32>()].copy_from_slice(&IP_PKTINFO.to_ne_bytes());
        bytes[header..header + 4].copy_from_slice(&self.address.octets());
        bytes[header + 4..header + 8].copy_from_slice(&self.interface.to_ne_bytes());
        let message = WSAMSG {
            name: (&raw mut address).cast::<SOCKADDR>(),
            namelen: i32::try_from(size_of_val(&address)).map_err(invalid)?,
            lpBuffers: &raw mut buffer,
            dwBufferCount: 1,
            Control: WSABUF {
                len: u32::try_from(aligned(length)?).map_err(invalid)?,
                buf: bytes.as_mut_ptr(),
            },
            dwFlags: 0,
        };
        let mut sent = 0;
        // SAFETY: all message/address/control/data buffers outlive this
        // synchronous non-overlapped call; packet data is not modified by it.
        let result = unsafe {
            send(
                raw(socket)?,
                &raw const message,
                0,
                &raw mut sent,
                ptr::null_mut(),
                None,
            )
        };
        if result == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(sent as usize)
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
                "truncated GigE bootstrap datagram or packet info",
            ));
        }
        if address.sin_family != AF_INET
            || message.namelen != i32::try_from(size_of_val(&address)).map_err(invalid)?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid IPv4 bootstrap reply address",
            ));
        }
        let length = usize::try_from(message.Control.len).map_err(invalid)?;
        let selected = packet_interface(
            bytes
                .get(..length)
                .ok_or_else(|| io::Error::other("packet-info length exceeds buffer"))?,
        )? == Some(self.interface);
        // SAFETY: Winsock returned a checked AF_INET sockaddr_in; S_addr is
        // the active representation of its four IPv4 network bytes.
        let source = Ipv4Addr::from(unsafe { address.sin_addr.S_un.S_addr }.to_ne_bytes());
        Ok(Some(Datagram {
            bytes: received as usize,
            source: SocketAddrV4::new(source, u16::from_be(address.sin_port)),
            selected_interface: selected,
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
        .ok_or_else(|| io::Error::other("packet-info alignment overflow"))
}

fn packet_interface(mut bytes: &[u8]) -> io::Result<Option<u32>> {
    let mut found = None;
    let header = aligned(size_of::<CMSGHDR>())?;
    while bytes.len() >= size_of::<CMSGHDR>() {
        let length = usize::from_ne_bytes(bytes[..size_of::<usize>()].try_into().map_err(invalid)?);
        if length < header || length > bytes.len() {
            return Err(io::Error::other("invalid Winsock control record length"));
        }
        let level = std::mem::offset_of!(CMSGHDR, cmsg_level);
        let kind = std::mem::offset_of!(CMSGHDR, cmsg_type);
        let level = i32::from_ne_bytes(bytes[level..level + 4].try_into().map_err(invalid)?);
        let kind = i32::from_ne_bytes(bytes[kind..kind + 4].try_into().map_err(invalid)?);
        if level == IPPROTO_IP && kind == IP_PKTINFO {
            if length != header + size_of::<IN_PKTINFO>() || found.is_some() {
                return Err(io::Error::other("invalid or duplicate Winsock packet info"));
            }
            found = Some(u32::from_ne_bytes(
                bytes[header + 4..header + 8].try_into().map_err(invalid)?,
            ));
        }
        let next = aligned(length)?;
        if next >= bytes.len() {
            break;
        }
        bytes = &bytes[next..];
    }
    Ok(found)
}

fn last_error() -> io::Error {
    // SAFETY: thread-local Winsock error retrieval has no pointer arguments.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: u32) -> Vec<u8> {
        let header = aligned(size_of::<CMSGHDR>()).unwrap();
        let length = header + size_of::<IN_PKTINFO>();
        let mut bytes = vec![0; aligned(length).unwrap()];
        bytes[..size_of::<usize>()].copy_from_slice(&length.to_ne_bytes());
        let level = std::mem::offset_of!(CMSGHDR, cmsg_level);
        let kind = std::mem::offset_of!(CMSGHDR, cmsg_type);
        bytes[level..level + 4].copy_from_slice(&IPPROTO_IP.to_ne_bytes());
        bytes[kind..kind + 4].copy_from_slice(&IP_PKTINFO.to_ne_bytes());
        bytes[header..header + 4].copy_from_slice(&[192, 168, 50, 1]);
        bytes[header + 4..header + 8].copy_from_slice(&index.to_ne_bytes());
        bytes
    }

    #[test]
    fn winsock_packet_info_has_checked_native_lengths_and_unique_interface() {
        assert_eq!(packet_interface(&record(7)).unwrap(), Some(7));
        assert_eq!(packet_interface(&record(0)).unwrap(), Some(0));
        assert_eq!(packet_interface(&[]).unwrap(), None);
        let mut duplicate = record(7);
        duplicate.extend_from_slice(&record(8));
        assert!(
            packet_interface(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        let mut short = record(7);
        short.pop();
        assert!(
            packet_interface(&short)
                .unwrap_err()
                .to_string()
                .contains("length")
        );
        let mut bad = record(7);
        bad[..size_of::<usize>()].copy_from_slice(&0_usize.to_ne_bytes());
        assert!(
            packet_interface(&bad)
                .unwrap_err()
                .to_string()
                .contains("length")
        );
        let mut other = record(7);
        let kind = std::mem::offset_of!(CMSGHDR, cmsg_type);
        other[kind..kind + 4].copy_from_slice(&999_i32.to_ne_bytes());
        assert_eq!(packet_interface(&other).unwrap(), None);
    }
}
