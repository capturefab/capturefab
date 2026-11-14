use super::*;
use crate::types::classify;
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex};

fn config() -> Config {
    serde_json::from_value(json!({
        "profile":"rc_plus_8_0", "address":"127.0.0.1:5000", "password_env":"EPSON_TEST_SECRET",
        "terminator":"crlf", "reads":[{"signal":"state","resource":"status"}]
    }))
    .unwrap()
}
fn selection(resource: Resource) -> Selection {
    Selection {
        signal: "value".into(),
        resource,
        address: resource.io_bits().map(|_| 1),
        name: None,
        declared_type: None,
        indices: Vec::new(),
        count: None,
    }
}
fn native(read: &Selection, payload: &str) -> Value {
    serde_json::to_value(parse::native(read, payload).unwrap()).unwrap()
}
#[test]
fn static_profiles_and_exact_configuration_refuse_unproved_reads() {
    for profile in [Profile::RcPlus75, Profile::RcPlus80, Profile::RcPlus80Rc800] {
        let mut cfg = config();
        cfg.profile = profile;
        cfg.validate().unwrap();
        cfg.reads[0].resource = Resource::CurrentRobot;
        if profile == Profile::RcPlus75 {
            assert_eq!(
                classify(&cfg.validate().unwrap_err()).kind,
                ErrorKind::Unsupported
            );
        } else {
            cfg.validate().unwrap();
        }
        cfg.reads[0].resource = Resource::Alarms;
        if profile == Profile::RcPlus80Rc800 {
            assert_eq!(
                classify(&cfg.validate().unwrap_err()).kind,
                ErrorKind::Unsupported
            );
        } else {
            cfg.validate().unwrap();
        }
    }
    let mut cfg = config();
    cfg.reads = vec![serde_json::from_value(json!({"signal":"v","resource":"variable","name":"gby2","declared_type":"Byte","indices":[3.0,0e0],"count":3e0})).unwrap()];
    cfg.validate().unwrap();
    assert_eq!(
        cfg.reads[0].request(Terminator::Crlf),
        "$GetVariable,gby2(3,0),Byte,3\r\n"
    );
    cfg.reads[0].declared_type = Some(VariableType::String);
    assert_eq!(
        classify(&cfg.validate().unwrap_err()).kind,
        ErrorKind::Unsupported
    );
    for bad in ["a,b", "a(0)", "a\r\n$Reset", "1value", "é"] {
        cfg.reads[0].declared_type = Some(VariableType::Byte);
        cfg.reads[0].name = Some(bad.into());
        assert_eq!(
            classify(&cfg.validate().unwrap_err()).kind,
            ErrorKind::Usage
        );
    }
    for bad in ["1.5", "-1", "65536", "true", "null"] {
        let input = format!(
            r#"{{"signal":"v","resource":"variable","name":"g","declared_type":"Byte","indices":[{bad}],"count":1}}"#
        );
        serde_json::from_str::<Selection>(&input).unwrap_err();
    }
    for bad in [",", "password\n", "é"] {
        validate_password(bad).unwrap_err();
    }
    validate_password("").unwrap();
    validate_password("private secret").unwrap();
}
#[test]
fn status_preserves_both_documented_widths_without_shifting_flags() {
    let read = selection(Resource::Status);
    let eleven = native(&read, "00100000001,0000");
    assert_eq!(eleven["flags_raw"], "00100000001");
    assert_eq!(eleven["named_flags"]["auto"], true);
    assert_eq!(eleven["named_flags"]["ready"], true);
    assert_eq!(eleven["named_flags"]["test"], false);
    let ten = native(&read, "0100000001,0042");
    assert_eq!(ten["flag_width"], 10);
    assert_eq!(ten["flag_mapping"], "unqualified_documented_width");
    assert_eq!(ten["code_text"], "0042");
    assert_eq!(ten.get("named_flags"), None);
    for bad in [
        "00100000001,000",
        "00100000001,00000",
        "0010000000x,0000",
        "001000000,0000",
        "00100000001,0000,0",
    ] {
        assert_eq!(
            classify(&parse::native(&read, bad).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
    }
}
#[test]
fn native_io_alarm_robot_and_variables_keep_exact_text_and_cardinality() {
    assert_eq!(
        native(&selection(Resource::IoWord), "010F"),
        json!({"kind":"io","text":"010F","value":271,"bits":16})
    );
    assert_eq!(
        native(&selection(Resource::Alarms), "2,1,009"),
        json!({"kind":"alarms","count":2,"alarm_codes_text":["1","009"]})
    );
    assert_eq!(
        native(&selection(Resource::CurrentRobot), "0002"),
        json!({"kind":"current_robot","number_text":"0002","number":2})
    );
    let mut read = selection(Resource::Variable);
    read.name = Some("gby2".into());
    read.declared_type = Some(VariableType::Byte);
    read.indices = vec![3, 0];
    read.count = Some(3);
    let value = native(&read, "001,-02,+3");
    assert_eq!(value["values_text"], json!(["001", "-02", "+3"]));
    read.indices.clear();
    read.count = None;
    assert_eq!(native(&read, " 1.2500")["values_text"], json!([" 1.2500"]));
    for (resource, bad) in [
        (Resource::Io, "2"),
        (Resource::IoByte, "F"),
        (Resource::IoWord, "10000"),
        (Resource::Alarms, "2,1"),
        (Resource::Alarms, "0,1"),
        (Resource::CurrentRobot, "-1"),
        (Resource::Variable, "1,2"),
    ] {
        assert_eq!(
            classify(&parse::native(&selection(resource), bad).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
    }
}
#[test]
fn replies_correlate_exactly_and_errors_never_reflect_unknown_credentials() {
    assert_eq!(
        parse::payload(b"#Login,0\r\n", Terminator::Crlf, "Login").unwrap(),
        "0"
    );
    for (raw, kind) in [
        (b"!Login,13\r\n".as_slice(), ErrorKind::AuthRejected),
        (b"!GetIO,19\r\n", ErrorKind::Timeout),
        (b"!GetIO,15\r\n", ErrorKind::OperationFailed),
    ] {
        let method = if raw.starts_with(b"!Login") {
            "Login"
        } else {
            "GetIO"
        };
        assert_eq!(
            classify(&parse::payload(raw, Terminator::Crlf, method).unwrap_err()).kind,
            kind
        );
    }
    for raw in [
        b"!Login,short\r\n".as_slice(),
        b"!Login,1234567\r\n",
        b"#Login,secret\r\n\r\n",
    ] {
        let error = parse::payload(raw, Terminator::Crlf, "Login").unwrap_err();
        let classified = classify(&error);
        let text = format!("{error:#} {:?}", classified.details);
        assert!(!text.contains("short"));
        assert!(!text.contains("1234567"));
        assert!(!text.contains("secret"));
    }
    for raw in [
        b"#GetIO,1\r\n".as_slice(),
        b"#GetStatus,1\n",
        b"#GetStatus,\x80\r\n",
        b"#GetStatus\r\n",
    ] {
        assert_eq!(
            classify(&parse::payload(raw, Terminator::Crlf, "GetStatus").unwrap_err()).kind,
            ErrorKind::Incomplete
        );
    }
    let future =
        parse::payload(b"!GetVariable,1234567\r\n", Terminator::Crlf, "GetVariable").unwrap_err();
    assert_eq!(
        classify(&future).details.unwrap()["native_error_code"],
        1_234_567
    );
}

type Sent = Arc<Mutex<Vec<Vec<u8>>>>;
type Reads = Arc<Mutex<usize>>;
struct Script {
    replies: VecDeque<Vec<u8>>,
    sent: Sent,
    reads: Reads,
    interrupts: usize,
}
impl io::Read for Script {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        *self
            .reads
            .lock()
            .map_err(|_| io::Error::other("fixture read counter poisoned"))? += 1;
        if self.interrupts != 0 {
            self.interrupts -= 1;
            return Err(io::ErrorKind::Interrupted.into());
        }
        let Some(front) = self.replies.front_mut() else {
            return Ok(0);
        };
        let count = output.len().min(front.len());
        output[..count].copy_from_slice(&front[..count]);
        drop(front.drain(..count));
        if front.is_empty() {
            self.replies.pop_front();
        }
        Ok(count)
    }
}
impl io::Write for Script {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.sent
            .lock()
            .map_err(|_| io::Error::other("fixture request log poisoned"))?
            .push(bytes.to_vec());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl net::Transport for Script {
    fn set_deadline(&mut self, _deadline: Instant) {}
}
fn script(replies: Vec<Vec<u8>>) -> (wire::Link, Sent, Reads) {
    script_with_interrupts(replies, 0)
}
fn script_with_interrupts(replies: Vec<Vec<u8>>, interrupts: usize) -> (wire::Link, Sent, Reads) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let reads = Arc::new(Mutex::new(0));
    (
        wire::Link::from_transport(Box::new(Script {
            replies: replies.into(),
            sent: sent.clone(),
            reads: reads.clone(),
            interrupts,
        })),
        sent,
        reads,
    )
}
#[test]
fn interrupted_one_byte_reads_keep_the_original_request_and_split_terminator() {
    let raw = b"#GetStatus,00100000001,0000\r\n";
    let fragments = raw.iter().map(|byte| vec![*byte]).collect();
    let (link, sent, reads) = script_with_interrupts(fragments, 1);
    let mut source = reader(link);
    let rows = source.read(Duration::from_secs(1)).unwrap();
    assert_eq!(rows[0].value["reply_bytes"], raw.len());
    assert_eq!(*reads.lock().unwrap(), raw.len() + 1);
    assert_eq!(*sent.lock().unwrap(), [b"$GetStatus\r\n".to_vec()]);
}
fn reader(link: wire::Link) -> Reader {
    Reader {
        config: config(),
        password: Zeroizing::new(String::new()),
        link: Some(link),
        failure: None,
        observed: None,
        ready: false,
        finished: false,
    }
}
#[test]
fn fragmented_framing_keeps_raw_bytes_and_known_unsolicited_suffix_retires() {
    let (link, sent, _) = script(vec![
        b"#GetSta".to_vec(),
        b"tus,00100000001,0000\r".to_vec(),
        b"\n".to_vec(),
    ]);
    let mut source = reader(link);
    let rows = source.read(Duration::from_secs(1)).unwrap();
    assert_eq!(
        rows[0].value["raw_reply_text"],
        "#GetStatus,00100000001,0000"
    );
    assert_eq!(
        rows[0].value["raw_reply_base64"],
        "I0dldFN0YXR1cywwMDEwMDAwMDAwMSwwMDAwDQo="
    );
    assert_eq!(*sent.lock().unwrap(), [b"$GetStatus\r\n".to_vec()]);
    assert!(source.ready());
    assert!(source.observed_ns().is_some());
    let (link, sent, reads) = script(vec![
        b"#GetStatus,00100000001,0000\r\n#GetStatus,0100000001,0000\r\n".to_vec(),
    ]);
    let mut source = reader(link);
    let first = source.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(classify(&first).kind, ErrorKind::Incomplete);
    let calls = *reads.lock().unwrap();
    let later = source.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(classify(&later).kind, ErrorKind::Incomplete);
    assert_eq!(*reads.lock().unwrap(), calls);
    assert_eq!(sent.lock().unwrap().len(), 1);
}
#[test]
fn original_zero_budget_sends_nothing_and_finish_never_relogs_or_logs_out() {
    let (link, sent, reads) = script(Vec::new());
    let mut source = reader(link);
    assert_eq!(
        classify(&source.read(Duration::ZERO).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert_eq!(sent.lock().unwrap().len(), 0);
    assert_eq!(*reads.lock().unwrap(), 0);
    let (link, sent, _) = script(Vec::new());
    let mut source = reader(link);
    source.finish(Duration::from_secs(1)).unwrap();
    assert_eq!(
        classify(&source.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::OperationFailed
    );
    assert_eq!(sent.lock().unwrap().len(), 0);
}
#[test]
fn escaped_batch_is_precharged_before_value_materialization_and_publication() {
    let mut cfg = config();
    cfg.reads.clear();
    for index in 0..32 {
        let mut read = selection(Resource::Variable);
        read.signal = format!("v{index}");
        read.name = Some("globalValue".into());
        read.declared_type = Some(VariableType::Double);
        cfg.reads.push(read);
    }
    let reply = format!("#GetVariable,{}\r\n", "\"".repeat(15000)).into_bytes();
    let (link, sent, _) = script(vec![reply; 32]);
    let mut source = reader(link);
    source.config = cfg;
    let error = source.read(Duration::from_secs(5)).unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
    assert!(!source.ready());
    assert_eq!(source.observed_ns(), None);
    assert!(sent.lock().unwrap().len() < 32);
}
