use super::*;
fn record(kind: i32, data: &[u8]) -> Vec<u8> {
    // SAFETY: native arithmetic receives only the small fixed fixture payload size.
    let header = unsafe { libc::CMSG_LEN(0) } as usize;
    // SAFETY: as above, this accounts for the platform control-record alignment.
    let mut bytes =
        vec![0_u8; unsafe { libc::CMSG_SPACE(u32::try_from(data.len()).unwrap()) } as usize];
    #[cfg(target_os = "macos")]
    bytes[..4].copy_from_slice(&u32::try_from(header + data.len()).unwrap().to_ne_bytes());
    #[cfg(any(target_os = "linux", target_os = "android"))]
    bytes[..size_of::<usize>()].copy_from_slice(&(header + data.len()).to_ne_bytes());
    let level = std::mem::offset_of!(libc::cmsghdr, cmsg_level);
    let at = std::mem::offset_of!(libc::cmsghdr, cmsg_type);
    bytes[level..level + 4].copy_from_slice(&libc::IPPROTO_IP.to_ne_bytes());
    bytes[at..at + 4].copy_from_slice(&kind.to_ne_bytes());
    bytes[header..header + data.len()].copy_from_slice(data);
    bytes
}
fn valid() -> Vec<u8> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let data = [7_u32.to_ne_bytes(), [127, 0, 0, 1], [239, 255, 42, 19]].concat();
        record(libc::IP_PKTINFO, &data)
    }
    #[cfg(target_os = "macos")]
    {
        const PREFIX: usize = std::mem::offset_of!(libc::sockaddr_dl, sdl_data);
        let mut data = [0_u8; PREFIX];
        data[0] = u8::try_from(PREFIX).unwrap();
        data[1] = u8::try_from(libc::AF_LINK).unwrap();
        let at = std::mem::offset_of!(libc::sockaddr_dl, sdl_index);
        data[at..at + 2].copy_from_slice(&7_u16.to_ne_bytes());
        [
            record(libc::IP_RECVIF, &data),
            record(libc::IP_RECVDSTADDR, &[239, 255, 42, 19]),
        ]
        .concat()
    }
}
#[test]
fn destination_and_interface_evidence_are_both_required_and_duplicates_are_invalid() {
    let bytes = valid();
    assert_eq!(
        evidence(&bytes).unwrap(),
        (Ipv4Addr::new(239, 255, 42, 19), 7)
    );
    for bad in [
        Vec::new(),
        [bytes.clone(), bytes.clone()].concat(),
        bytes[..size_of::<libc::cmsghdr>()].to_vec(),
    ] {
        assert_eq!(
            evidence(&bad).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[cfg(target_os = "macos")]
    for bad in [
        record(libc::IP_RECVDSTADDR, &[239, 255, 42, 19]),
        record(libc::IP_RECVDSTADDR, &[239, 255, 42]),
    ] {
        assert_eq!(
            evidence(&bad).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
