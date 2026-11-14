//! Receive-only `SocketCAN` acquisition on a disposable container's vcan interface.
#![cfg(target_os = "linux")]
mod support;

use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs, io,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
    process::{Child, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use support::cli::{Home, Run};

fn isolated() {
    assert_eq!(
        std::env::var("CAPTUREFAB_CAN_TEST_ISOLATED").as_deref(),
        Ok("1"),
        "run only in a disposable Linux container/network namespace"
    );
    assert!(std::path::Path::new("/.dockerenv").exists());
}
fn interface() -> String {
    isolated();
    let interface =
        std::env::var("CAPTUREFAB_CAN_TEST_INTERFACE").unwrap_or_else(|_| "vcan0".into());
    assert!(
        interface.starts_with("vcan"),
        "fixtures transmit only on virtual CAN interfaces"
    );
    virtual_interface(&interface);
    interface
}

// Linux rtnetlink RTM_NEWLINK/IFLA_LINKINFO, as used by `ip link add type vcan`.
// Only invoked behind the Docker + explicit isolation checks above. No host
// namespace, interface, or kernel module is configured by this fixture.
fn virtual_interface(interface: &str) {
    static SETUP: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SETUP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let name = CString::new(interface).unwrap();
    // SAFETY: name is a live NUL-terminated interface name.
    let mut index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        let mut body = vec![0; 16]; // struct ifinfomsg
        attribute(&mut body, 3, name.as_bytes_with_nul()); // IFLA_IFNAME
        let mut info = Vec::new();
        attribute(&mut info, 1, b"vcan\0"); // IFLA_INFO_KIND
        attribute(&mut body, 18 | 0x8000, &info); // IFLA_LINKINFO | NLA_F_NESTED
        netlink(&body, 0x605); // NLM_F_REQUEST | ACK | CREATE | EXCL
        // SAFETY: name remains a live NUL-terminated interface name.
        index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        assert!(index > 0, "kernel did not create virtual CAN interface");
    }
    let mut body = vec![0; 16];
    body[4..8].copy_from_slice(&index.to_ne_bytes());
    body[8..12].copy_from_slice(&1_u32.to_ne_bytes()); // IFF_UP
    body[12..16].copy_from_slice(&1_u32.to_ne_bytes()); // change IFF_UP only
    netlink(&body, 5); // NLM_F_REQUEST | NLM_F_ACK
}

fn attribute(body: &mut Vec<u8>, kind: u16, data: &[u8]) {
    body.extend_from_slice(&u16::try_from(4 + data.len()).unwrap().to_ne_bytes());
    body.extend_from_slice(&kind.to_ne_bytes());
    body.extend_from_slice(data);
    body.resize(body.len().next_multiple_of(4), 0);
}

fn netlink(body: &[u8], flags: u16) {
    // SAFETY: creates an independent descriptor without passing pointers.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    assert!(fd >= 0, "{}", io::Error::last_os_error());
    // SAFETY: socket returned a newly owned descriptor.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let timeout = libc::timeval {
        tv_sec: 1,
        tv_usec: 0,
    };
    assert_eq!(
        // SAFETY: timeout is initialized and kernel copies exactly its structure size.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                libc::socklen_t::try_from(size_of_val(&timeout)).unwrap(),
            )
        },
        0
    );
    // SAFETY: zero initializes the C address including its reserved padding.
    let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    kernel.nl_family = u16::try_from(libc::AF_NETLINK).unwrap();
    let mut message = Vec::new();
    message.extend_from_slice(&u32::try_from(16 + body.len()).unwrap().to_ne_bytes());
    message.extend_from_slice(&16_u16.to_ne_bytes()); // RTM_NEWLINK
    message.extend_from_slice(&flags.to_ne_bytes());
    message.extend_from_slice(&42_u32.to_ne_bytes()); // request sequence
    message.extend_from_slice(&0_u32.to_ne_bytes()); // kernel assigns local port ID
    message.extend_from_slice(body);
    // SAFETY: message and initialized kernel address remain live for the synchronous send.
    let sent = unsafe {
        libc::sendto(
            fd,
            message.as_ptr().cast(),
            message.len(),
            0,
            (&raw const kernel).cast(),
            libc::socklen_t::try_from(size_of_val(&kernel)).unwrap(),
        )
    };
    assert_eq!(
        usize::try_from(sent).unwrap(),
        message.len(),
        "{}",
        io::Error::last_os_error()
    );
    let mut reply = [0_u8; 1024];
    // SAFETY: reply is writable for its length and socket remains owned/open.
    let count = unsafe {
        libc::recv(
            socket.as_raw_fd(),
            reply.as_mut_ptr().cast(),
            reply.len(),
            libc::MSG_TRUNC,
        )
    };
    assert!(
        (20..1024).contains(&count),
        "rtnetlink acknowledgment failed: {}",
        io::Error::last_os_error()
    );
    assert_eq!(u16::from_ne_bytes(reply[4..6].try_into().unwrap()), 2); // NLMSG_ERROR ACK
    assert_eq!(u32::from_ne_bytes(reply[8..12].try_into().unwrap()), 42);
    let error = i32::from_ne_bytes(reply[16..20].try_into().unwrap());
    assert_eq!(
        error,
        0,
        "isolated kernel cannot configure vcan: {}",
        io::Error::from_raw_os_error(-error)
    );
}

fn raw_socket() -> OwnedFd {
    // SAFETY: creates a new socket descriptor without passing pointers.
    let fd = unsafe {
        libc::socket(
            libc::AF_CAN,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::CAN_RAW,
        )
    };
    assert!(fd >= 0, "{}", io::Error::last_os_error());
    // SAFETY: the successful socket call returned a newly owned descriptor.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let enabled = 1_i32;
    assert_eq!(
        // SAFETY: enabled is an initialized integer read synchronously by the kernel.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_CAN_RAW,
                libc::CAN_RAW_FD_FRAMES,
                (&raw const enabled).cast(),
                libc::socklen_t::try_from(size_of_val(&enabled)).unwrap(),
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    socket
}
fn socket(interface: &str) -> OwnedFd {
    let socket = raw_socket();
    let fd = socket.as_raw_fd();
    let interface = CString::new(interface).unwrap();
    // SAFETY: interface is a live NUL-terminated interface name.
    let index = unsafe { libc::if_nametoindex(interface.as_ptr()) };
    assert!(
        index > 0,
        "create and bring up vcan0 inside the disposable container first"
    );
    // SAFETY: a zeroed sockaddr_can is a valid C ABI structure.
    let mut address: libc::sockaddr_can = unsafe { std::mem::zeroed() };
    address.can_family = u16::try_from(libc::AF_CAN).unwrap();
    address.can_ifindex = i32::try_from(index).unwrap();
    assert_eq!(
        // SAFETY: the address pointer and length name one initialized sockaddr_can.
        unsafe {
            libc::bind(
                fd,
                (&raw const address).cast(),
                libc::socklen_t::try_from(size_of_val(&address)).unwrap(),
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    socket
}

#[test]
#[ignore = "requires disposable Docker Linux namespace and kernel SocketCAN support, without a CAN device"]
fn raw_socket_accepts_fd_filters_timestamps_and_loss_counters_without_a_device() {
    isolated();
    let socket = raw_socket();
    let fd = socket.as_raw_fd();
    for option in [libc::SO_TIMESTAMPNS, libc::SO_RXQ_OVFL] {
        let enabled = 1_i32;
        // SAFETY: enabled is initialized and the kernel copies exactly its integer size.
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                (&raw const enabled).cast(),
                libc::socklen_t::try_from(size_of_val(&enabled)).unwrap(),
            )
        };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
    }
    let filter = libc::can_filter {
        can_id: 0x8000_0123,
        can_mask: 0x9fff_ffff,
    };
    // SAFETY: filter is one initialized can_filter copied synchronously by the kernel.
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_CAN_RAW,
            libc::CAN_RAW_FILTER,
            (&raw const filter).cast(),
            libc::socklen_t::try_from(size_of_val(&filter)).unwrap(),
        )
    };
    assert_eq!(result, 0, "{}", io::Error::last_os_error());
    let mut counters = [0_u32; 9];
    let mut length = libc::socklen_t::try_from(size_of_val(&counters)).unwrap();
    // SAFETY: both output pointers name initialized writable allocations with the specified size.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_MEMINFO,
            counters.as_mut_ptr().cast(),
            &raw mut length,
        )
    };
    assert_eq!(result, 0, "{}", io::Error::last_os_error());
    assert_eq!(usize::try_from(length).unwrap(), size_of_val(&counters));
    assert_eq!(counters[8], 0);
}

fn frame(id: u32, payload: &[u8], fd: bool) -> Vec<u8> {
    let mut frame = vec![0; if fd { 72 } else { 16 }];
    frame[..4].copy_from_slice(&id.to_ne_bytes());
    frame[4] = u8::try_from(payload.len()).unwrap();
    if fd {
        frame[5] = 3; // Bit-rate switch and error-state indicator.
    }
    frame[8..8 + payload.len()].copy_from_slice(payload);
    frame
}

fn send(socket: &OwnedFd, bytes: &[u8]) {
    // SAFETY: the live byte slice is readable for its length and the socket stays open.
    let sent = unsafe { libc::send(socket.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
    assert_eq!(
        usize::try_from(sent).unwrap(),
        bytes.len(),
        "{}",
        io::Error::last_os_error()
    );
}

struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            drop(child.kill());
            drop(child.wait());
        }
    }
}

fn capture(home: &Home, interface: String, id: u32, malformed: bool) -> Run {
    let mut command = home.command();
    command
        .args([
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "650ms",
            "-o",
            "run.jsonl",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard(Some(command.spawn().unwrap()));
    let done = Arc::new(AtomicBool::new(false));
    let stopped = done.clone();
    let sender = thread::spawn(move || {
        let socket = socket(&interface);
        let good = frame(id, &[0xff, 0x38, 0, 1, 2, 3, 4, 5], false);
        let bad = frame(id, &[0xff], false);
        let mut payload = vec![0; 64];
        payload[56..].copy_from_slice(&u64::MAX.to_le_bytes());
        let fd = frame(0x8000_0000 | id, &payload, true);
        let noise = frame(id + 1, &[0xff], false);
        let started = Instant::now();
        while !stopped.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(3) {
            send(&socket, &noise);
            send(
                &socket,
                if malformed && started.elapsed() > Duration::from_millis(200) {
                    &bad
                } else {
                    &good
                },
            );
            send(&socket, &fd);
            thread::sleep(Duration::from_millis(15));
        }
    });
    let wait_by = Instant::now() + Duration::from_secs(5);
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < wait_by,
            "CAN capture exceeded its total deadline"
        );
        thread::sleep(Duration::from_millis(10));
    }
    done.store(true, Ordering::Release);
    sender.join().unwrap();
    Run::of_output(
        child.0.take().unwrap().wait_with_output().unwrap(),
        &["data", "record"],
    )
}

fn configure(home: &Home, interface: &str, id: u32) {
    fs::write(home.path("sources.json"), json!({"version":1,"sources":[{
        "id":"bus","protocol":"can","interface":interface,"timeout_ms":50,
        "filters":[{"can_id":id}],"signals":[
            {"name":"temperature","can_id":id,"offset":0,"data_type":"i16","byte_order":"big","scale":0.25},
            {"name":"counter","can_id":id,"extended":true,"offset":56,"data_type":"u64"}
        ]
    }]}).to_string()).unwrap();
}

fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
#[ignore = "requires disposable Docker Linux network namespace with an up vcan0 interface"]
fn classic_fd_frames_and_exact_mapped_values_share_receive_clock_and_export() {
    let interface = interface();
    let home = Home::new("can-kernel");
    configure(&home, &interface, 0x123);
    let result = capture(&home, interface, 0x123, false).success();
    assert_eq!(result["status"], "complete");
    let rows = rows(&home);
    let samples: Vec<_> = rows.iter().filter(|row| row["kind"] == "sample").collect();
    assert!(
        samples
            .iter()
            .any(|row| row["signal"] == "temperature" && row["value"] == -50.0)
    );
    assert!(
        samples
            .iter()
            .any(|row| row["signal"] == "counter" && row["value"].as_u64() == Some(u64::MAX))
    );
    for pair in samples.as_chunks::<2>().0 {
        assert_eq!(pair[0]["signal"], "frame");
        assert_eq!(pair[0]["elapsed_ns"], pair[1]["elapsed_ns"]);
        assert_eq!(pair[0]["value"]["can_id"], 0x123);
        assert_eq!(
            pair[0]["value"]["timestamp_clock"],
            "linux_socket_realtime_receive"
        );
        chrono::DateTime::parse_from_rfc3339(
            pair[0]["value"]["kernel_received_at"].as_str().unwrap(),
        )
        .unwrap();
        assert!(pair[0].get("source_time").is_none());
        if pair[0]["value"]["fd"] == true {
            assert_eq!(pair[0]["value"]["length"], 64);
            assert_eq!(pair[0]["value"]["bitrate_switch"], true);
            assert_eq!(pair[0]["value"]["error_state_indicator"], true);
        } else {
            assert_eq!(pair[0]["value"]["data_hex"], "ff38000102030405");
        }
    }
    assert_eq!(samples.len() % 2, 0);
    let exported = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "out.jsonl",
        "--format",
        "jsonl",
    ]);
    assert_eq!(exported["samples"], result["samples"]);
    let output = fs::read_to_string(home.path("out.jsonl")).unwrap();
    assert!(output.contains("18446744073709551615"));
}

#[test]
#[ignore = "requires disposable Docker Linux network namespace with an up vcan0 interface"]
fn short_matching_frame_fails_and_preserves_recoverable_rows() {
    let interface = interface();
    let home = Home::new("can-malformed-kernel");
    configure(&home, &interface, 0x456);
    let error = capture(&home, interface, 0x456, true).failure(1);
    assert_eq!(error["code"], "incomplete");
    assert_eq!(error["details"]["status"], "failed");
    let rows = rows(&home);
    assert!(rows.iter().any(|row| row["signal"] == "temperature"));
    assert_eq!(rows.last().unwrap()["status"], "failed");
    let exported = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "partial.jsonl",
        "--format",
        "jsonl",
        "--allow-incomplete",
    ]);
    assert_eq!(exported["samples"], error["details"]["samples"]);
}

#[test]
#[ignore = "requires disposable Docker Linux network namespace with an up vcan0 interface"]
fn quiet_bus_times_out_without_transmitting_probe_frames() {
    let interface = interface();
    let monitor = socket(&interface);
    let filter = libc::can_filter {
        can_id: 0x555,
        can_mask: 0x1fff_ffff,
    };
    assert_eq!(
        // SAFETY: filter is one initialized can_filter copied synchronously by the kernel.
        unsafe {
            libc::setsockopt(
                monitor.as_raw_fd(),
                libc::SOL_CAN_RAW,
                libc::CAN_RAW_FILTER,
                (&raw const filter).cast(),
                libc::socklen_t::try_from(size_of_val(&filter)).unwrap(),
            )
        },
        0
    );
    let home = Home::new("can-quiet-kernel");
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[{
            "id":"quiet","protocol":"can","interface":interface,"timeout_ms":40,
            "filters":[{"can_id":0x555}]
        }]})
        .to_string(),
    )
    .unwrap();
    let started = Instant::now();
    let error = home
        .run(&[
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            "200ms",
            "-o",
            "run.jsonl",
        ])
        .failure(4);
    assert_eq!(error["code"], "timeout");
    assert!(started.elapsed() < Duration::from_secs(3));
    let mut bytes = [0_u8; 72];
    // SAFETY: bytes is writable for its length and monitor remains open.
    let received = unsafe {
        libc::recv(
            monitor.as_raw_fd(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::MSG_DONTWAIT,
        )
    };
    assert_eq!(received, -1);
    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
}
