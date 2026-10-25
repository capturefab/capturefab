//! `capturefab version [--check]` and doctor's update fields through the
//! public binary, against a loopback release list. Nothing here reaches
//! capturefab.com: every spawn sets CAPTUREFAB_UPDATE_URL and clears proxies.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-updates-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

/// The binary with isolated state, a release list `url` and no proxies.
fn binary(dir: &Path, url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_capturefab"));
    command
        .env_remove("CAPTUREFAB_SESSION")
        .env_remove("CAPTUREFAB_UPDATE_CHECK")
        .env("CAPTUREFAB_SESSION_DIR", dir)
        .env("CAPTUREFAB_CACHE_DIR", dir.join("cache"))
        .env("CAPTUREFAB_UPDATE_URL", url);
    for proxy in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        command.env_remove(proxy);
    }
    command
}

fn run(command: &mut Command, args: &[&str]) -> (Output, Value) {
    let out = command.args(args).output().unwrap();
    let value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out, value)
}

/// A loopback server answering every request with `body`, logging each
/// request line.
struct Server {
    url: String,
    log: Arc<Mutex<Vec<String>>>,
}

fn serve(body: Vec<u8>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/r/releases.json",
        listener.local_addr().unwrap().port()
    );
    let log = Arc::new(Mutex::new(Vec::new()));
    let requests = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            requests.lock().unwrap().push(first.trim().to_owned());
            let mut reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            reply.extend_from_slice(&body);
            let _ = stream.write_all(&reply);
        }
    });
    Server { url, log }
}

fn host() -> (&'static str, &'static str) {
    let arch = match std::env::consts::ARCH {
        "arm" => "armv7",
        other => other,
    };
    (std::env::consts::OS, arch)
}

fn manifest() -> Vec<u8> {
    let (os, arch) = host();
    serde_json::to_vec(&json!({
        "schema_version": 1,
        "latest": "999.0.0",
        "releases": [{
            "version": "999.0.0",
            "date": "2026-11-20",
            "url": "notes.html",
            "highlights": ["Faster\u{202E} previews"],
            "assets": [{
                "name": "capturefab-999.0.0-host",
                "os": os, "arch": arch, "available": true,
                "sha256": "ab".repeat(32), "size": 1,
                "url": "capturefab-999.0.0-host"
            }]
        }]
    }))
    .unwrap()
}

#[test]
fn version_without_check_is_offline() {
    let dir = temp("offline");
    let server = serve(manifest());
    let (out, value) = run(&mut binary(&dir, &server.url), &["--json", "version"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result = &value["result"];
    assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["update_check"]["last_checked"], Value::Null);
    assert_eq!(result["update_check"]["update_available"], Value::Null);
    assert_eq!(
        result["update_check"]["compiled"],
        cfg!(feature = "updates")
    );
    let (out, value) = run(&mut binary(&dir, &server.url), &["--json", "doctor"]);
    assert!(out.status.success());
    assert_eq!(
        value["result"]["features"]["updates"],
        cfg!(feature = "updates")
    );
    assert!(value["result"]["update_check"].is_object());
    let (out, _) = run(&mut binary(&dir, &server.url), &["version"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Last check: never"), "{text}");
    assert!(server.log.lock().unwrap().is_empty(), "nothing fetched");
}

#[test]
fn schema_documents_the_version_command_and_update_contract() {
    let dir = temp("schema");
    let (_, value) = run(
        &mut binary(&dir, "http://127.0.0.1:9/releases.json"),
        &["--json", "schema"],
    );
    let schema = &value["result"];
    assert_eq!(
        schema["updates"]["manifest"],
        "https://capturefab.com/releases.json"
    );
    assert!(
        schema["cli"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "version")
    );
}

#[test]
fn a_turned_off_check_exits_unsupported() {
    let dir = temp("policy");
    let (out, value) = run(
        binary(&dir, "http://127.0.0.1:9/releases.json").env("CAPTUREFAB_UPDATE_CHECK", "off"),
        &["--json", "version", "--check"],
    );
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(value["error"]["code"], "unsupported");
}

#[cfg(feature = "updates")]
#[test]
fn version_check_reads_the_list_and_keeps_the_persons_choices() {
    let dir = temp("check");
    std::fs::write(
        dir.join("updates.json"),
        br#"{"schema_version":1,"automatic":false,"skipped":"999.0.0"}"#,
    )
    .unwrap();
    let server = serve(manifest());
    // An unreachable proxy proves loopback mirrors never use one.
    let (out, value) = run(
        binary(&dir, &server.url)
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("http_proxy", "http://127.0.0.1:9"),
        &["--json", "version", "--check"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result = &value["result"];
    assert_eq!(result["latest"], "999.0.0");
    assert_eq!(result["update_available"], true);
    assert_eq!(result["skipped"], true);
    let release = &result["release"];
    assert_eq!(release["highlights"], json!(["Faster previews"]));
    let origin = server.url.trim_end_matches("/r/releases.json");
    assert_eq!(release["url"], format!("{origin}/r/notes.html"));
    assert_eq!(
        release["download"]["url"],
        format!("{origin}/r/capturefab-999.0.0-host")
    );
    assert_eq!(
        server.log.lock().unwrap().as_slice(),
        ["GET /r/releases.json HTTP/1.1"]
    );
    let saved: Value =
        serde_json::from_slice(&std::fs::read(dir.join("updates.json")).unwrap()).unwrap();
    assert_eq!(saved["automatic"], false);
    assert_eq!(saved["skipped"], "999.0.0");
    assert_eq!(saved["last_outcome"]["latest"], "999.0.0");
    // A fresh state stays automatic-by-default after a requested check.
    let fresh = temp("fresh");
    let (out, value) = run(
        &mut binary(&fresh, &server.url),
        &["--json", "version", "--check"],
    );
    assert!(out.status.success());
    assert_eq!(value["result"]["skipped"], false);
    let saved: Value =
        serde_json::from_slice(&std::fs::read(fresh.join("updates.json")).unwrap()).unwrap();
    assert_eq!(saved["automatic"], Value::Null);
    // Offline reads now report it, and doctor warns once.
    let (_, value) = run(&mut binary(&fresh, &server.url), &["--json", "doctor"]);
    let checks = value["result"]["checks"].as_array().unwrap();
    assert!(
        checks
            .iter()
            .any(|check| check["id"] == "updates" && check["level"] == "warn"),
        "{checks:?}"
    );
    let (_, value) = run(&mut binary(&fresh, &server.url), &["--json", "version"]);
    assert_eq!(value["result"]["update_check"]["update_available"], true);
    assert_eq!(
        server.log.lock().unwrap().len(),
        2,
        "doctor and version never fetch"
    );
}

#[cfg(feature = "updates")]
#[test]
fn bad_urls_are_usage_errors_and_closed_ports_are_unavailable() {
    let dir = temp("errors");
    let (out, value) = run(
        &mut binary(&dir, "ftp://x"),
        &["--json", "version", "--check"],
    );
    assert_eq!(out.status.code(), Some(2), "{value}");
    assert_eq!(value["error"]["code"], "usage");
    let (out, value) = run(
        &mut binary(&dir, "http://127.0.0.1:9/releases.json"),
        &["--json", "version", "--check"],
    );
    assert_eq!(out.status.code(), Some(3), "{value}");
    assert_eq!(value["error"]["code"], "unavailable");
}

#[cfg(not(feature = "updates"))]
#[test]
fn builds_without_updates_say_so() {
    let dir = temp("uncompiled");
    let (out, value) = run(
        &mut binary(&dir, "http://127.0.0.1:9/releases.json"),
        &["--json", "version", "--check"],
    );
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(value["error"]["code"], "unsupported");
}
