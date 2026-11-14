//! Independent private-worker framing and C-trace clock boundary check.
use super::{
    Home, counter_source, digital_source, engineering_source, five_source, mixed_source, on_demand,
    resistive_source, source,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read as _},
    os::unix::process::CommandExt as _,
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_MESSAGE: usize = 512 * 1024;
const RPC_WAIT: Duration = Duration::from_secs(4);

struct OwnedWorker {
    child: Child,
    reaped: bool,
}
impl OwnedWorker {
    fn wait(&mut self, timeout: Duration) -> bool {
        let until = Instant::now().checked_add(timeout).unwrap();
        while Instant::now() < until {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reaped = true;
                return status.success();
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }
}
impl Drop for OwnedWorker {
    fn drop(&mut self) {
        if !self.reaped {
            if let Ok(pid) = i32::try_from(self.child.id()) {
                // SAFETY: the unreaped owned child was created as this group's
                // leader; its PID cannot be reused before this owner reaps it.
                let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            let _ = self.child.kill();
            let until = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
            while Instant::now() < until {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        self.reaped = true;
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(2)),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        eprintln!("cannot confirm owned NI clock-test worker reap: {error}");
                        break;
                    }
                }
            }
        }
    }
}

fn monotonic_ns() -> u64 {
    let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes this valid timespec on success.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, value.as_mut_ptr()) };
    assert_eq!(status, 0);
    // SAFETY: the successful call above initialized this timespec.
    let value = unsafe { value.assume_init() };
    u64::try_from(value.tv_sec)
        .unwrap()
        .checked_mul(1_000_000_000)
        .unwrap()
        .checked_add(u64::try_from(value.tv_nsec).unwrap())
        .unwrap()
}

fn read_message(output: &mut ChildStdout) -> io::Result<Value> {
    let mut prefix = [0u8; 4];
    output.read_exact(&mut prefix)?;
    let size = usize::try_from(u32::from_le_bytes(prefix))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !(1..=MAX_MESSAGE).contains(&size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "independent NI reply extent is outside the bound",
        ));
    }
    let mut bytes = vec![0; size];
    output.read_exact(&mut bytes)?;
    let packet: Value = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if packet["protocol"] != 10 || packet.as_object().map_or(0, serde_json::Map::len) != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "independent NI reply has the wrong envelope",
        ));
    }
    Ok(packet["body"].clone())
}

fn pump(mut output: ChildStdout, sender: &SyncSender<io::Result<Value>>, count: usize) {
    // The caller fixes a finite reply count; the mailbox remains one slot.
    assert!((1..=4).contains(&count));
    for _ in 0..count {
        let response = read_message(&mut output);
        let failed = response.is_err();
        if sender.send(response).is_err() || failed {
            break;
        }
    }
}

fn write_message(input: &mut impl io::Write, body: &Value) {
    let bytes = serde_json::to_vec(&json!({"protocol":10,"body":body})).unwrap();
    assert!((1..=MAX_MESSAGE).contains(&bytes.len()));
    input
        .write_all(&u32::try_from(bytes.len()).unwrap().to_le_bytes())
        .unwrap();
    input.write_all(&bytes).unwrap();
    input.flush().unwrap();
}

fn deadline_ns() -> u64 {
    monotonic_ns().checked_add(3_000_000_000).unwrap()
}

fn trace(home: &Home) -> Vec<Value> {
    let bytes = fs::read(home.path("clock-driver.jsonl")).unwrap();
    assert!(bytes.len() <= 1024 * 1024);
    std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn diagnostic(row: &Value) -> bool {
    matches!(
        row["fn"].as_str(),
        Some("DAQmxGetExtendedErrorInfo" | "DAQmxGetErrorString")
    )
}

fn join_finished(reader: JoinHandle<()>) {
    let until = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
    while !reader.is_finished() && Instant::now() < until {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(reader.is_finished(), "owned worker output did not close");
    reader.join().unwrap();
}

#[test]
fn private_worker_clock_is_between_native_read_and_deferred_diagnostic() {
    for profile in 0..26 {
        let home = Home::new("ni-private-clock");
        let library = home.path("clock-fixture.so");
        let (mut config, operation, expected) = match profile {
            0 => (
                source("DiagSlowPartial"),
                "ReadAnalogF64",
                "000000000000f03f00000000000004c0",
            ),
            1 => (
                mixed_source("MixedDiagSlowPartial"),
                "ReadAnalogF64",
                "000000000000f03f0000000000003940000000000000903f0000000000003940",
            ),
            2 => (
                digital_source("DigitalDiagSlowPartial"),
                "ReadDigitalU32",
                "01000000a500000000000080",
            ),
            3 => (
                counter_source("CountDiagSlowPartial", false),
                "ReadCounterU32",
                "00000000",
            ),
            4 => (
                counter_source("FrequencyDiagSlowPartial", true),
                "ReadCounterF64",
                "0000000000003940",
            ),
            5..=12 => {
                let family = profile - 5;
                let device = format!(
                    "{}DiagSlowPartial",
                    [
                        "Strain",
                        "Ratio",
                        "Accel",
                        "EngineeringMixed",
                        "Period",
                        "Pulse",
                        "Angle",
                        "Linear"
                    ][family]
                );
                (
                    engineering_source(family, &device),
                    if family < 4 {
                        "ReadAnalogF64"
                    } else {
                        "ReadCounterF64"
                    },
                    [
                        "000000000000903f000000000000903f",
                        "000000000000903f000000000000903f",
                        "000000000000903f000000000000903f000000000000903f",
                        "000000000000f03f0000000000003940000000000000903f0000000000003940000000000000503f000000000000903f000000000000f03f",
                        "000000000000903f",
                        "000000000000903f",
                        "000000000000903f",
                        "000000000000903f",
                    ][family],
                )
            }
            13..=19 => {
                let family = profile - 13;
                let device = format!(
                    "{}DiagSlowPartial",
                    [
                        "Resistance",
                        "ThermIex",
                        "ThermVex",
                        "Force",
                        "Pressure",
                        "Torque",
                        "ResistiveMixed"
                    ][family]
                );
                (
                    resistive_source(family, &device),
                    "ReadAnalogF64",
                    [
                        "00000000000039400000000000003940",
                        "00000000000039400000000000003940",
                        "00000000000039400000000000003940",
                        "000000000000903f000000000000903f",
                        "000000000000903f000000000000903f",
                        "000000000000903f000000000000903f",
                        "000000000000f03f0000000000003940000000000000903f0000000000003940000000000000503f000000000000903f000000000000f03f000000000000394000000000000039400000000000003940000000000000903f000000000000903f000000000000903f",
                    ][family],
                )
            }
            20..=25 => {
                let family = profile - 20;
                let device = format!(
                    "{}DiagSlowPartial",
                    [
                        "Velocity",
                        "IepeForce",
                        "Charge",
                        "Lvdt",
                        "Rvdt",
                        "FiveMixed"
                    ][family]
                );
                (
                    five_source(family, &device),
                    "ReadAnalogF64",
                    [
                        "00000000000039400000000000003940",
                        "00000000000039400000000000003940",
                        "00000000000039400000000000003940",
                        "000000000000903f000000000000903f",
                        "000000000000903f000000000000903f",
                        "000000000000f03f0000000000003940000000000000903f0000000000003940000000000000503f000000000000903f000000000000f03f000000000000394000000000000039400000000000003940000000000000903f000000000000903f000000000000903f000000000000394000000000000039400000000000003940000000000000903f000000000000903f",
                    ][family],
                )
            }
            _ => unreachable!(),
        };
        fs::write(
            &library,
            if profile < 2 {
                &super::linux::libraries()[0]
            } else if profile < 5 {
                &super::linux::input_libraries()[profile - 2]
            } else if matches!(profile, 8 | 19 | 25) {
                &super::linux::libraries()[0]
            } else if profile >= 20 {
                &super::linux::five_libraries()[0]
            } else if profile >= 13 {
                &super::linux::resistive_libraries()[0]
            } else {
                &super::linux::engineering_libraries()[if profile >= 9 { 2 } else { 0 }]
            },
        )
        .unwrap();
        if profile == 0 {
            config["measurement"] = json!("voltage");
        }
        let read_function = format!("DAQmx{operation}");
        let object = config.as_object_mut().unwrap();
        for key in ["id", "protocol", "timeout_ms"] {
            object.remove(key);
        }
        config["library"] = json!(library);
        let child = home
            .command()
            .arg("__nidaqmx-worker")
            .env_remove("CAPTUREFAB_EXECUTABLE")
            .env(
                "CAPTUREFAB_NIDAQMX_FIXTURE_LOG",
                home.path("clock-driver.jsonl"),
            )
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut worker = OwnedWorker {
            child,
            reaped: false,
        };
        let mut input = worker.child.stdin.take().unwrap();
        let output = worker.child.stdout.take().unwrap();
        let (sender, replies) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || pump(output, &sender, 3));
        let cutoff = monotonic_ns().checked_add(15_000_000_000).unwrap();
        write_message(
            &mut input,
            &json!({"open":{"config":config,"deadline_ns":deadline_ns(),"cutoff_ns":cutoff}}),
        );
        let ready = replies.recv_timeout(RPC_WAIT).unwrap().unwrap();
        assert!(ready.get("ready").is_some(), "{ready}");
        write_message(
            &mut input,
            &json!({"read":{"deadline_ns":deadline_ns(),"cutoff_ns":cutoff}}),
        );
        let read = replies.recv_timeout(RPC_WAIT).unwrap().unwrap();
        let response = &read["read"];
        assert_eq!(response["block"]["sample_index"], 0);
        assert_eq!(response["block"]["samples_per_channel"], 1);
        assert_eq!(response["block"]["status_code"], -200_284);
        assert!(response["terminal"].is_object());
        let notices = response["notices"].as_array().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["operation"], operation);
        assert_eq!(notices[0]["status_code"], -200_284);
        assert_eq!(notices[0]["diagnostic_source"], "deferred");
        assert_eq!(response["block"]["bits_le_hex"], expected);
        let observed = response["observed_ns"].as_u64().unwrap();
        let before_finish = trace(&home);
        assert!(!before_finish.iter().any(diagnostic), "{before_finish:?}");
        let read_return = before_finish
            .iter()
            .find(|row| row["fn"] == read_function && row["phase"] == "return")
            .unwrap()["monotonic_ns"]
            .as_u64()
            .unwrap();
        assert!(read_return <= observed);
        write_message(&mut input, &json!({"finish":{"deadline_ns":deadline_ns()}}));
        let finished = replies.recv_timeout(RPC_WAIT).unwrap().unwrap();
        assert!(finished.get("finished").is_some(), "{finished}");
        drop(input);
        assert!(worker.wait(Duration::from_secs(2)));
        join_finished(reader);
        let log = trace(&home);
        assert!(!log.iter().any(|row| row["phase"] == "contract"), "{log:?}");
        let first_diagnostic = log
            .iter()
            .find(|row| diagnostic(row) && row["phase"] == "enter")
            .unwrap();
        assert_eq!(first_diagnostic["fn"], "DAQmxGetExtendedErrorInfo");
        assert!(observed < first_diagnostic["monotonic_ns"].as_u64().unwrap());
        assert_eq!(first_diagnostic["clock"], "CLOCK_MONOTONIC");
        assert_eq!(first_diagnostic["pid"], u64::from(worker.child.id()));
        let first_cleanup = log
            .iter()
            .find(|row| row["fn"] == "DAQmxStopTask" && row["phase"] == "enter")
            .unwrap();
        assert!(first_diagnostic["seq"].as_u64().unwrap() < first_cleanup["seq"].as_u64().unwrap());
    }
}

struct SoftwareWorker {
    worker: OwnedWorker,
    input: ChildStdin,
    replies: Receiver<io::Result<Value>>,
    reader: Option<JoinHandle<()>>,
    cutoff: u64,
}
impl SoftwareWorker {
    fn open(home: &Home, scenario: &str, interval: u64, profile: usize) -> Self {
        let library = home.path("software-clock-fixture.so");
        fs::write(
            &library,
            if profile == 0 {
                &super::linux::on_demand_libraries()[0]
            } else if profile < 4 {
                &super::linux::input_libraries()[profile - 1]
            } else if matches!(profile, 7 | 18 | 24) {
                &super::linux::on_demand_libraries()[0]
            } else if profile >= 19 {
                &super::linux::five_libraries()[1]
            } else if profile >= 12 {
                &super::linux::resistive_libraries()[1]
            } else {
                &super::linux::engineering_libraries()[if profile >= 8 { 2 } else { 1 }]
            },
        )
        .unwrap();
        let mut config = match profile {
            0 => on_demand(source(scenario), interval),
            1 => digital_source(&format!("Digital{scenario}")),
            2 => counter_source(&format!("Count{scenario}"), false),
            3 => counter_source(&format!("Frequency{scenario}"), true),
            4..=11 => engineering_source(
                profile - 4,
                &format!(
                    "{}{scenario}",
                    [
                        "Strain",
                        "Ratio",
                        "Accel",
                        "EngineeringMixed",
                        "Period",
                        "Pulse",
                        "Angle",
                        "Linear"
                    ][profile - 4]
                ),
            ),
            12..=18 => resistive_source(
                profile - 12,
                &format!(
                    "{}{scenario}",
                    [
                        "Resistance",
                        "ThermIex",
                        "ThermVex",
                        "Force",
                        "Pressure",
                        "Torque",
                        "ResistiveMixed"
                    ][profile - 12]
                ),
            ),
            19..=24 => five_source(
                profile - 19,
                &format!(
                    "{}{scenario}",
                    [
                        "Velocity",
                        "IepeForce",
                        "Charge",
                        "Lvdt",
                        "Rvdt",
                        "FiveMixed"
                    ][profile - 19]
                ),
            ),
            _ => unreachable!(),
        };
        if profile == 0 {
            config["measurement"] = json!("voltage");
        }
        if profile >= 4 {
            config = on_demand(config, interval);
        }
        config["poll_interval_ms"] = json!(interval);
        for key in ["id", "protocol", "timeout_ms"] {
            config.as_object_mut().unwrap().remove(key);
        }
        config["library"] = json!(library);
        let child = home
            .command()
            .arg("__nidaqmx-worker")
            .env_remove("CAPTUREFAB_EXECUTABLE")
            .env(
                "CAPTUREFAB_NIDAQMX_FIXTURE_LOG",
                home.path("clock-driver.jsonl"),
            )
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut worker = OwnedWorker {
            child,
            reaped: false,
        };
        let input = worker.child.stdin.take().unwrap();
        let output = worker.child.stdout.take().unwrap();
        let (sender, replies) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || pump(output, &sender, 4));
        let cutoff = monotonic_ns().checked_add(15_000_000_000).unwrap();
        let mut result = Self {
            worker,
            input,
            replies,
            reader: Some(reader),
            cutoff,
        };
        let ready = result
            .rpc(&json!({"open":{"config":config,"deadline_ns":deadline_ns(),"cutoff_ns":cutoff}}));
        assert_eq!(
            ready["ready"]["descriptor"]["timing"],
            json!({"mode":"on_demand","poll_interval_ms":interval})
        );
        assert!(!trace(home).iter().any(|row| matches!(
            row["fn"].as_str(),
            Some(
                "DAQmxReadAnalogF64"
                    | "DAQmxReadDigitalU32"
                    | "DAQmxReadCounterU32"
                    | "DAQmxReadCounterF64"
            )
        )));
        result
    }
    fn rpc(&mut self, request: &Value) -> Value {
        write_message(&mut self.input, request);
        self.replies.recv_timeout(RPC_WAIT).unwrap().unwrap()
    }
    fn read(&mut self) -> (Value, u64) {
        let deadline = deadline_ns();
        let cutoff = self.cutoff;
        let result = self.rpc(&json!({"read":{"deadline_ns":deadline,"cutoff_ns":cutoff}}));
        (result, deadline.min(cutoff))
    }
    fn finish(mut self) {
        let response = self.rpc(&json!({"finish":{"deadline_ns":deadline_ns()}}));
        assert!(response.get("finished").is_some(), "{response}");
        assert!(self.worker.wait(Duration::from_secs(2)));
        join_finished(self.reader.take().unwrap());
    }
}

fn attempted(response: &Value, interval: u64) -> (u64, u64) {
    let response = &response["read"];
    assert!(response["terminal"].is_null(), "{response}");
    assert_eq!(response["block"]["samples_per_channel"], 1);
    let start = response["pacing"]["attempt_start_ns"].as_u64().unwrap();
    let due = response["pacing"]["next_not_before_ns"].as_u64().unwrap();
    assert_eq!(due, start.checked_add(interval).unwrap());
    assert!(response["observed_ns"].as_u64().unwrap() >= start);
    (start, due)
}

#[test]
fn private_on_demand_ready_and_early_idle_never_consume_or_renew_native_attempts() {
    for profile in 0..25 {
        let read_function = [
            "DAQmxReadAnalogF64",
            "DAQmxReadDigitalU32",
            "DAQmxReadCounterU32",
            "DAQmxReadCounterF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadCounterF64",
            "DAQmxReadCounterF64",
            "DAQmxReadCounterF64",
            "DAQmxReadCounterF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
        ][profile];
        let home = Home::new("ni-software-clock-idle");
        let mut worker = SoftwareWorker::open(&home, "Success", 3_600_000, profile);
        let (first, deadline) = worker.read();
        let (start, due) = attempted(&first, 3_600_000_000_000);
        assert_eq!(
            first["read"]["block"]["bits_le_hex"],
            [
                "000000000000f03f00000000000004c0",
                "01000000a500000000000080",
                "00000000",
                "0000000000003940",
                "000000000000903f000000000000903f",
                "000000000000903f000000000000903f",
                "000000000000903f000000000000903f000000000000903f",
                "000000000000f03f0000000000003940000000000000903f0000000000003940000000000000503f000000000000903f000000000000f03f",
                "000000000000903f",
                "000000000000903f",
                "000000000000903f",
                "000000000000903f",
                "00000000000039400000000000003940",
                "00000000000039400000000000003940",
                "00000000000039400000000000003940",
                "000000000000903f000000000000903f",
                "000000000000903f000000000000903f",
                "000000000000903f000000000000903f",
                "000000000000f03f0000000000003940000000000000903f0000000000003940000000000000503f000000000000903f000000000000f03f000000000000394000000000000039400000000000003940000000000000903f000000000000903f000000000000903f",
                "00000000000039400000000000003940",
                "00000000000039400000000000003940",
                "00000000000039400000000000003940",
                "000000000000903f000000000000903f",
                "000000000000903f000000000000903f",
                "000000000000f03f0000000000003940000000000000903f0000000000003940000000000000503f000000000000903f000000000000f03f000000000000394000000000000039400000000000003940000000000000903f000000000000903f000000000000903f000000000000394000000000000039400000000000003940000000000000903f000000000000903f",
            ][profile]
        );
        let first_observed = first["read"]["observed_ns"].as_u64().unwrap();
        let log = trace(&home);
        let entry = log
            .iter()
            .find(|row| row["fn"] == read_function && row["phase"] == "enter")
            .unwrap();
        let returned = log
            .iter()
            .find(|row| row["fn"] == read_function && row["phase"] == "return")
            .unwrap();
        assert!(start <= entry["monotonic_ns"].as_u64().unwrap());
        assert!(returned["monotonic_ns"].as_u64().unwrap() <= first_observed);
        let arguments = log
            .iter()
            .find(|row| row["fn"] == read_function && row["phase"] == "arguments")
            .unwrap();
        assert_eq!(arguments["requested"], 1);
        assert_eq!(
            arguments["capacity"],
            [
                2, 3, 1, 1, 2, 2, 3, 7, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 13, 2, 2, 2, 2, 2, 18
            ][profile]
        );
        let bits = u64::from_str_radix(arguments["timeout_bits"].as_str().unwrap(), 16).unwrap();
        let timeout = f64::from_bits(bits);
        assert!(timeout.is_finite() && timeout > 0.0);
        // The budget derives from the worker boundary, not this C entry or the RPC send.
        let allowed = Duration::from_nanos(deadline - start).as_secs_f64();
        assert!(timeout <= allowed + 1e-9);
        let (idle, _) = worker.read();
        assert!(idle["read"]["block"].is_null());
        assert!(idle["read"]["terminal"].is_null());
        assert!(idle["read"]["pacing"]["attempt_start_ns"].is_null());
        assert_eq!(idle["read"]["pacing"]["next_not_before_ns"], due);
        assert!(!trace(&home).iter().any(diagnostic));
        worker.finish();
        let log = trace(&home);
        assert!(!log.iter().any(|row| row["phase"] == "contract"), "{log:?}");
        for (function, count) in [
            (read_function, 1),
            ("DAQmxStartTask", 1),
            ("DAQmxStopTask", 1),
            ("DAQmxClearTask", 1),
        ] {
            assert_eq!(
                log.iter()
                    .filter(|row| row["fn"] == function && row["phase"] == "enter")
                    .count(),
                count
            );
        }
    }
}

#[test]
fn private_on_demand_slow_completion_keeps_start_frontier_without_catchup() {
    for profile in 0..25 {
        let read_function = [
            "DAQmxReadAnalogF64",
            "DAQmxReadDigitalU32",
            "DAQmxReadCounterU32",
            "DAQmxReadCounterF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadCounterF64",
            "DAQmxReadCounterF64",
            "DAQmxReadCounterF64",
            "DAQmxReadCounterF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
            "DAQmxReadAnalogF64",
        ][profile];
        let home = Home::new("ni-software-clock-slow");
        let mut worker = SoftwareWorker::open(&home, "SlowRead", 25, profile);
        let (first, _) = worker.read();
        let (_, first_due) = attempted(&first, 25_000_000);
        let first_observed = first["read"]["observed_ns"].as_u64().unwrap();
        assert!(first_observed >= first_due);
        let (second, _) = worker.read();
        let (second_start, _) = attempted(&second, 25_000_000);
        assert!(second_start >= first_observed);
        assert_eq!(second["read"]["block"]["sample_index"], 1);
        assert_eq!(
            second["read"]["block"]["bits_le_hex"],
            [
                "0000000000000080000000000000f07f",
                "000000005a000000ffffffff",
                "00000080",
                "000000000000903f",
                "000000000000903f0000000000000080",
                "000000000000903f0000000000000080",
                "0000000000000080000000000000f07f000000000000f0ff",
                "0000000000000080a50000000000f87f000000000000f07f00000000000004c000000000000080bf000000000000f0ff0000000000000000",
                "0000000000000000",
                "0000000000000000",
                "0000000000000000",
                "0000000000000000",
                "0000000000000080a50000000000f87f",
                "0000000000000080a50000000000f87f",
                "0000000000000080a50000000000f87f",
                "00000000000000000000000000000080",
                "00000000000000000000000000000080",
                "00000000000000000000000000000080",
                "0000000000000080a50000000000f87f000000000000f07f00000000000004c000000000000080bf000000000000f0ff00000000000000000000000000000080000000000000f07fa50000000000f87f00000000000004c0000000000000f0ff0000000000000000",
                "00000000000000800000000000000000",
                "00000000000000800000000000000000",
                "00000000000000800000000000000000",
                "00000000000000800000000000000000",
                "00000000000000800000000000000000",
                "0000000000000080a50000000000f87f000000000000f07f00000000000004c000000000000080bf000000000000f0ff00000000000000000000000000000080000000000000f07fa50000000000f87f00000000000004c0000000000000f0ff00000000000000000000000000000080420000000000f87f000000000000f07f000000000000dabf000000000000eb3f",
            ][profile]
        );
        // Exact frontier arithmetic is independent of C-entry scheduling jitter;
        // no claim of a physical inter-entry gap or hard real-time cadence.
        worker.finish();
        let log = trace(&home);
        assert!(!log.iter().any(|row| row["phase"] == "contract"), "{log:?}");
        assert_eq!(
            log.iter()
                .filter(|row| row["fn"] == read_function && row["phase"] == "enter")
                .count(),
            2
        );
    }
}
