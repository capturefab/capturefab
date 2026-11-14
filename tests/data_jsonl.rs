//! Native sensor JSON Lines capture through actual TCP and Unix terminal devices.
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;

const EXACT: &str =
    "[18446744073709551617,-18446744073709551617,0.123456789012345678901234567890123456789]";

fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "sensor client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn write_config(home: &Home, sources: &Value) {
    fs::write(
        home.work.join("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
}
fn record(home: &Home) -> support::cli::Run {
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "350ms",
        "-o",
        "run.jsonl",
    ])
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.work.join("run.jsonl"))
        .unwrap()
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect()
}

#[test]
fn tcp_object_and_scalar_streams_preserve_payloads_timestamps_and_exports() {
    let home = Home::new("jsonl-tcp");
    let object_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let scalar_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    write_config(
        &home,
        &json!([
            {"id":"device", "protocol":"tcp_jsonl", "address":object_listener.local_addr().unwrap().to_string(), "every_ms":3_600_000, "timeout_ms":30},
            {"id":"numeric", "protocol":"tcp_jsonl", "address":scalar_listener.local_addr().unwrap().to_string(), "signal":"weight", "every_ms":3_600_000, "timeout_ms":30},
            {"id":"cycle", "protocol":"simulator", "signal":"tick", "every_ms":10}
        ]),
    );
    let (stop_objects, wait_objects) = mpsc::channel();
    let (stop_scalars, wait_scalars) = mpsc::channel();
    let objects = thread::spawn(move || {
        let mut stream = accept(&object_listener);
        let payload = format!(
            "{{\"signal\":\"count\",\"value\":{EXACT},\"source_time\":\"2026-11-10T14:00:00.123456789+01:00\"}}\r\n{{\"signal\":\"running\",\"value\":true}}\n"
        );
        // Leave a partial line over an ordinary idle timeout before finishing it.
        stream.write_all(&payload.as_bytes()[..20]).unwrap();
        thread::sleep(Duration::from_millis(60));
        stream.write_all(&payload.as_bytes()[20..]).unwrap();
        wait_objects.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            stream.read(&mut [0]).unwrap(),
            0,
            "receive-only sensor must not get commands"
        );
    });
    let scalars = thread::spawn(move || {
        let mut stream = accept(&scalar_listener);
        stream
            .write_all(b"12.345678901234567890123456789\n13.5\n")
            .unwrap();
        wait_scalars.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            stream.read(&mut [0]).unwrap(),
            0,
            "receive-only sensor must not get commands"
        );
    });
    let result = record(&home);
    stop_objects.send(()).unwrap();
    stop_scalars.send(()).unwrap();
    objects.join().unwrap();
    scalars.join().unwrap();
    result.success();
    let rows = rows(&home);
    let device: Vec<_> = rows
        .iter()
        .filter(|row| row["source"] == "device")
        .collect();
    assert_eq!(device.len(), 2);
    assert_eq!(device[0]["value"].to_string(), EXACT);
    assert_eq!(device[0]["source_time"], "2026-11-10T13:00:00.123456789Z");
    assert_eq!(device[1]["value"], true);
    let numeric: Vec<_> = rows
        .iter()
        .filter(|row| row["source"] == "numeric")
        .collect();
    assert_eq!(numeric.len(), 2);
    assert_eq!(
        numeric[0]["value"].to_string(),
        "12.345678901234567890123456789"
    );
    assert_eq!(numeric[0]["signal"], "weight");
    assert!(numeric[0].get("source_time").is_none());
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.csv",
        "--source",
        "device",
    ]);
    assert!(
        fs::read_to_string(home.work.join("values.csv"))
            .unwrap()
            .contains(EXACT)
    );
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.jsonl",
        "--format",
        "jsonl",
        "--source",
        "numeric",
    ]);
    assert!(
        fs::read_to_string(home.work.join("values.jsonl"))
            .unwrap()
            .contains("12.345678901234567890123456789")
    );
}

#[test]
fn malformed_oversized_closed_and_quiet_tcp_sources_fail_explicitly() {
    for (payload, close, exit, code) in [
        (
            "{\"signal\":\"x\",\"value\":private-sensor-secret}\n".to_owned(),
            false,
            1,
            "failed",
        ),
        ("x".repeat((1 << 20) + 1), false, 1, "failed"),
        (
            "{\"signal\":\"x\",\"value\":".to_owned(),
            true,
            3,
            "disconnected",
        ),
        (String::new(), false, 4, "timeout"),
    ] {
        let home = Home::new("jsonl-invalid");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        write_config(
            &home,
            &json!([{ "id":"sensor", "protocol":"tcp_jsonl", "address":listener.local_addr().unwrap().to_string(), "timeout_ms":30 }]),
        );
        let (stop, wait) = mpsc::channel();
        let sensor = thread::spawn(move || {
            let mut stream = accept(&listener);
            let _ = stream.write_all(payload.as_bytes());
            if close {
                stream.shutdown(std::net::Shutdown::Write).unwrap();
            }
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let result = record(&home);
        stop.send(()).unwrap();
        sensor.join().unwrap();
        assert_eq!(result.failure(exit)["code"], code);
        assert!(!result.stdout.contains("private-sensor-secret"));
        assert!(!result.stderr.contains("private-sensor-secret"));
        assert_eq!(rows(&home).last().unwrap()["status"], "failed");
    }
}

#[cfg(unix)]
mod serial {
    use super::*;
    use std::{
        ffi::CStr,
        fs::File,
        os::fd::{AsRawFd as _, FromRawFd as _},
    };

    fn pair() -> (File, File, String) {
        let (mut master, mut slave) = (-1, -1);
        let mut name: [libc::c_char; 256] = [0; 256];
        // SAFETY: outputs/name are valid allocations; null options use defaults.
        assert_eq!(
            // SAFETY: fixture descriptor allocations stay valid for this call.
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    name.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: successful openpty returned independently owned descriptors.
        let master = unsafe { File::from_raw_fd(master) };
        // SAFETY: this is the other successful openpty descriptor.
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            // SAFETY: slave stays open and settings is a valid output allocation.
            unsafe { libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) },
            0
        );
        // SAFETY: successful tcgetattr initialized settings.
        let mut settings = unsafe { settings.assume_init() };
        settings.c_iflag |= libc::IXON | libc::IXOFF | libc::IXANY;
        settings.c_lflag &= !(libc::ICANON | libc::ECHO);
        settings.c_cc[libc::VMIN] = 1;
        assert_eq!(
            // SAFETY: slave is open and settings points to initialized termios.
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw const settings) },
            0
        );
        for file in [&master, &slave] {
            // SAFETY: file remains live through both descriptor flag calls.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            // SAFETY: a valid owned descriptor and close-on-exec flag mask.
            assert_eq!(
                // SAFETY: fixture descriptor allocations stay valid for this call.
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) },
                0
            );
        }
        // SAFETY: successful openpty initialized a NUL-terminated name.
        let name = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned();
        (master, slave, name)
    }

    fn wait_configured(slave: &File) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
            // SAFETY: slave stays open and the output allocation is valid.
            assert_eq!(
                // SAFETY: fixture descriptor allocations stay valid for this call.
                unsafe { libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) },
                0
            );
            // SAFETY: successful tcgetattr initialized the allocation.
            let settings = unsafe { settings.assume_init() };
            if settings.c_lflag & libc::ICANON == 0 && settings.c_cc[libc::VMIN] == 0 {
                assert_eq!(
                    settings.c_iflag & (libc::IXON | libc::IXOFF | libc::IXANY),
                    0
                );
                // SAFETY: settings is initialized; this reads only its speed.
                assert_eq!(
                    // SAFETY: fixture descriptor allocations stay valid for this call.
                    unsafe { libc::cfgetispeed(&raw const settings) },
                    libc::B115200
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "serial sensor was not configured"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn unix_serial_scalar_sensor_is_receive_only_and_preserves_precision() {
        let home = Home::new("jsonl-serial");
        let (mut master, slave, path) = pair();
        // Input already queued before the adapter opens must survive setup.
        master.write_all(b"122.5\n").unwrap();
        let baud: Value = serde_json::from_str("1.152e5").unwrap();
        let data_bits: Value = serde_json::from_str("8.0").unwrap();
        let stop_bits: Value = serde_json::from_str("1e0").unwrap();
        write_config(
            &home,
            &json!([{ "id":"scale", "protocol":"serial_jsonl", "device":path, "signal":"mass_kg", "timeout_ms":30, "baud":baud, "data_bits":data_bits, "stop_bits":stop_bits }]),
        );
        let (stop, wait) = mpsc::channel();
        let sensor = thread::spawn(move || {
            wait_configured(&slave);
            master
                .write_all(b"123.45678901234567890123456789\r\n124.5\n")
                .unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            // A serial source must never emit command bytes or echo its input.
            // SAFETY: master stays open through status flag operations.
            let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            // SAFETY: owned descriptor and valid nonblocking flag mask.
            assert_eq!(
                // SAFETY: fixture descriptor allocations stay valid for this call.
                unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            let error = master.read(&mut [0; 16]).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        });
        let result = record(&home);
        stop.send(()).unwrap();
        sensor.join().unwrap();
        assert_eq!(result.success()["samples"], 3);
        let rows = rows(&home);
        assert_eq!(rows[1]["value"].to_string(), "122.5");
        assert_eq!(
            rows[2]["value"].to_string(),
            "123.45678901234567890123456789"
        );
        assert_eq!(rows[1]["signal"], "mass_kg");
    }

    #[test]
    fn regular_file_is_rejected_as_a_serial_device_before_recording() {
        let home = Home::new("jsonl-not-tty");
        fs::write(home.work.join("sensor"), "42\n").unwrap();
        write_config(
            &home,
            &json!([{ "id":"sensor", "protocol":"serial_jsonl", "device":"sensor", "signal":"x" }]),
        );
        assert_eq!(record(&home).failure(3)["code"], "unavailable");
        assert!(!home.work.join("run.jsonl").exists());
    }
}
