//! Native Windows installer integration. Unix installer scenarios run in
//! scripts/test-site.py. Serve real, checksummed fixture archives locally.
#![deny(clippy::doc_markdown)]
#[cfg(windows)]
mod support;

#[cfg(windows)]
mod windows {
    use super::support;
    use sha2::{Digest as _, Sha256};
    use std::{
        collections::HashMap,
        io::{Cursor, Read as _, Write as _},
        net::TcpListener,
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };
    #[test]
    fn install_upgrade_reinstall_and_uninstall() {
        let home = support::cli::Home::new("installers");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let binary = std::fs::read(env!("CARGO_BIN_EXE_capturefab")).unwrap();
        let mut files = HashMap::new();
        for (script, version, bytes) in [
            ("old", "9.9.8", binary.as_slice()),
            ("new", "9.9.9", binary.as_slice()),
            ("broken", "9.9.10", b"not an executable".as_slice()),
        ] {
            let stem = format!("capturefab-{version}-windows-fixture");
            let name = format!("{stem}.zip");
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file(format!("{stem}/capturefab.exe"), options)
                .unwrap();
            zip.write_all(bytes).unwrap();
            zip.start_file(format!("{stem}/README.txt"), options)
                .unwrap();
            zip.write_all(format!("Capturefab {version} (windows fixture)\n").as_bytes())
                .unwrap();
            let archive = zip.finish().unwrap().into_inner();
            let sha = format!("{:x}", Sha256::digest(&archive));
            let assets = ["aarch64", "x86_64"].map(|arch| format!(
                "  '{arch}' = @{{ Name = '{name}'; Url = 'http://{address}/{name}'; Sha256 = '{sha}' }}\n"
            )).concat();
            let installer = include_str!("../scripts/install-template.ps1")
                .replace("@VERSION_LABEL@", version)
                .replace("@VERSION@", version)
                .replace("@SITE@", "https://example.invalid/")
                .replace("@ASSETS@", &assets);
            std::fs::write(home.root.join(format!("{script}.ps1")), installer).unwrap();
            files.insert(format!("/{name}"), archive);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let server = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("fixture server: {e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while request.len() < 16384 && !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let (status, body) = files
                    .get(path)
                    .map_or(("404 Not Found", &b"missing"[..]), |data| {
                        ("200 OK", data.as_slice())
                    });
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream
                    .write_all(header.as_bytes())
                    .and_then(|()| stream.write_all(body));
            }
        });
        let result = Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/installers.ps1"
            ))
            .env("CAPTUREFAB_INSTALL_FIXTURE", &home.root)
            .env_remove("CAPTUREFAB_INSTALL_BASE_URL")
            // CI itself runs under PowerShell 7. Let Windows PowerShell
            // 5.1 discover its own compatible built-in modules.
            .env_remove("PSModulePath")
            .env("CAPTUREFAB_SESSION_DIR", home.root.join("sessions"))
            .env("CAPTUREFAB_CACHE_DIR", home.root.join("cache"))
            .output();
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        let output = result.unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
