//! A testing-only directly executed helper; it never invokes an interpreter.
use serde_json::json;
use std::{
    env, fs,
    io::{self, Read as _, Write as _},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

fn emit(value: &serde_json::Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    out.flush()
}

fn wait_stdin(root: &str) -> io::Result<()> {
    let mut input = [0_u8; 16];
    while io::stdin().read(&mut input)? != 0 {}
    fs::write(format!("{root}/stdin-eof"), b"eof")
}

fn wait_owned_stop() {
    for _ in 0..300 {
        thread::sleep(Duration::from_millis(100));
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    #[cfg(feature = "usb")]
    if mode == "__usbtmc-worker" {
        return capturefab::testing::usbtmc_worker_fixture().map_err(io::Error::other);
    }
    #[cfg(feature = "usb")]
    if mode == "__usbtmc-probe" {
        let mut input = Vec::new();
        io::stdin().take(131_073).read_to_end(&mut input)?;
        if input.len() > 131_072 {
            return Err(io::Error::other("fixture client input too large"));
        }
        let input = serde_json::from_slice(&input)?;
        let value = capturefab::testing::usbtmc_client_probe(input).map_err(io::Error::other)?;
        return emit(&value);
    }
    let root = args
        .next()
        .ok_or_else(|| io::Error::other("missing fixture root"))?;
    if mode != "descendant" {
        fs::write(format!("{root}/helper.pid"), std::process::id().to_string())?;
    }
    match mode.as_str() {
        "descendant" => {
            fs::write(
                format!("{root}/descendant.pid"),
                std::process::id().to_string(),
            )?;
            wait_owned_stop();
        }
        "args" => {
            let values: Vec<_> = args.collect();
            emit(&json!({"signal":"arguments","value":values}))?;
            emit(
                &json!({"signal":"wide_integer","value":18_446_744_073_709_551_615_u64,
                "source_time":"2026-11-10T14:00:00.123456789+01:00"}),
            )?;
            eprintln!("helper-private-stderr-not-a-reading");
            wait_stdin(&root)?;
            wait_owned_stop();
        }
        "tree" => {
            let mut child = Command::new(env::current_exe()?)
                .args(["descendant", &root])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            fs::write(format!("{root}/descendant.pid"), child.id().to_string())?;
            emit(
                &json!({"signal":"owned","value":{"parent":std::process::id(),"child":child.id()}}),
            )?;
            wait_stdin(&root)?;
            wait_owned_stop();
            let _ = child.kill();
            let _ = child.wait();
        }
        "invalid" => {
            emit(&json!({"signal":"prefix","value":{"raw":[0,255,10],"text":"A\u{0}☃"}}))?;
            thread::sleep(Duration::from_millis(80));
            let mut out = io::stdout().lock();
            out.write_all(b"private-invalid-helper-line\n")?;
            out.flush()?;
            wait_stdin(&root)?;
        }
        "partial" => {
            emit(&json!({"signal":"prefix","value":17}))?;
            thread::sleep(Duration::from_millis(80));
            io::stdout().write_all(b"{\"signal\":\"partial\"")?;
            io::stdout().flush()?;
        }
        "idle" => wait_stdin(&root)?,
        _ => return Err(io::Error::other("unknown fixture mode")),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("controlled helper fixture failed: {error}");
        std::process::exit(1);
    }
}
