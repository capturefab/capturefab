//! A small loopback FTP server for tests: passive mode only, files kept in
//! memory, every command recorded. It replaces existing files on STOR and
//! RNTO, as vsftpd and ProFTPD do, so tests can see that the client itself
//! never overwrites.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const HOME: &str = "/home/cam";

pub struct State {
    pub user: String,
    pub password: String,
    /// The greeting status; 421 refuses the connection.
    pub greeting: u32,
    /// Whether SIZE and MLST are implemented.
    pub size: bool,
    pub mlst: bool,
    pub files: BTreeMap<String, Vec<u8>>,
    pub dirs: BTreeSet<String>,
    /// Every command received, with PASS arguments hidden.
    pub commands: Vec<String>,
    pub logins: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            user: "cam".into(),
            password: "secret".into(),
            greeting: 220,
            size: true,
            mlst: true,
            files: BTreeMap::new(),
            dirs: ["/".to_string(), "/home".into(), HOME.into()].into(),
            commands: Vec::new(),
            logins: 0,
        }
    }
}

pub struct FakeFtp {
    pub port: u16,
    pub state: Arc<Mutex<State>>,
}

impl FakeFtp {
    pub fn start(configure: impl FnOnce(&mut State)) -> Self {
        let mut state = State::default();
        configure(&mut state);
        let state = Arc::new(Mutex::new(state));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                thread::spawn(move || {
                    let _ = serve(stream, &state);
                });
            }
        });
        Self { port, state }
    }

    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().files.get(path).cloned()
    }

    pub fn commands(&self) -> Vec<String> {
        self.state.lock().unwrap().commands.clone()
    }
}

fn resolve(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        cwd.split('/').filter(|p| !p.is_empty()).collect()
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

fn serve(stream: TcpStream, state: &Mutex<State>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut out = stream.try_clone()?;
    let mut lines = BufReader::new(stream);
    let greeting = state.lock().unwrap().greeting;
    if greeting != 220 {
        write!(out, "{greeting} Service not available\r\n")?;
        return Ok(());
    }
    write!(out, "220 fake ftp ready\r\n")?;
    let mut cwd = HOME.to_string();
    let mut user = String::new();
    let mut logged_in = false;
    let mut passive: Option<TcpListener> = None;
    let mut rename_from: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let (verb, arg) = line.split_once(' ').unwrap_or((line, ""));
        let verb = verb.to_ascii_uppercase();
        let mut st = state.lock().unwrap();
        st.commands.push(if verb == "PASS" {
            "PASS ***".into()
        } else {
            line.to_string()
        });
        let reply: String = match verb.as_str() {
            "USER" => {
                user = arg.into();
                "331 Password required".into()
            }
            "PASS" => {
                if user == st.user && arg == st.password {
                    logged_in = true;
                    st.logins += 1;
                    "230 Logged in".into()
                } else {
                    "530 Login incorrect".into()
                }
            }
            "QUIT" => {
                write!(out, "221 Bye\r\n")?;
                return Ok(());
            }
            "NOOP" => "200 OK".into(),
            "AUTH" => "502 TLS not available".into(),
            _ if !logged_in => "530 Please log in".into(),
            "TYPE" => "200 Type set".into(),
            "PWD" => format!("257 \"{cwd}\" is the current directory"),
            "CWD" => {
                let target = resolve(&cwd, arg);
                if st.dirs.contains(&target) {
                    cwd = target;
                    "250 Directory changed".into()
                } else {
                    "550 No such directory".into()
                }
            }
            "MKD" => {
                let target = resolve(&cwd, arg);
                if st.dirs.contains(&target) || st.files.contains_key(&target) {
                    "550 Exists".into()
                } else {
                    st.dirs.insert(target.clone());
                    format!("257 \"{target}\" created")
                }
            }
            "SIZE" if !st.size => "502 Command not implemented".into(),
            "SIZE" => match st.files.get(&resolve(&cwd, arg)) {
                Some(data) => format!("213 {}", data.len()),
                None => "550 No such file".into(),
            },
            "MLST" if !st.mlst => "500 Unknown command".into(),
            "MLST" => {
                let target = resolve(&cwd, arg);
                if let Some(data) = st.files.get(&target) {
                    format!(
                        "250-Listing {arg}\r\n type=file;size={}; {target}\r\n250 End",
                        data.len()
                    )
                } else if st.dirs.contains(&target) {
                    format!("250-Listing {arg}\r\n type=dir; {target}\r\n250 End")
                } else {
                    "550 No such file".into()
                }
            }
            "PASV" => {
                let listener = TcpListener::bind("127.0.0.1:0")?;
                let port = listener.local_addr()?.port();
                passive = Some(listener);
                format!(
                    "227 Entering Passive Mode (127,0,0,1,{},{})",
                    port >> 8,
                    port & 0xff
                )
            }
            "EPSV" => {
                let listener = TcpListener::bind("127.0.0.1:0")?;
                let port = listener.local_addr()?.port();
                passive = Some(listener);
                format!("229 Entering Extended Passive Mode (|||{port}|)")
            }
            "STOR" => {
                let Some(listener) = passive.take() else {
                    write!(out, "425 Use PASV first\r\n")?;
                    continue;
                };
                let target = resolve(&cwd, arg);
                let parent = resolve(&target, "..");
                if !st.dirs.contains(&parent) {
                    "553 No such directory".into()
                } else {
                    drop(st);
                    write!(out, "150 Opening data connection\r\n")?;
                    let (mut data, _) = listener.accept()?;
                    let mut bytes = Vec::new();
                    data.read_to_end(&mut bytes)?;
                    state.lock().unwrap().files.insert(target, bytes);
                    write!(out, "226 Transfer complete\r\n")?;
                    continue;
                }
            }
            "DELE" => {
                if st.files.remove(&resolve(&cwd, arg)).is_some() {
                    "250 Deleted".into()
                } else {
                    "550 No such file".into()
                }
            }
            "RNFR" => {
                let target = resolve(&cwd, arg);
                if st.files.contains_key(&target) {
                    rename_from = Some(target);
                    "350 Ready for RNTO".into()
                } else {
                    "550 No such file".into()
                }
            }
            "RNTO" => match rename_from.take() {
                Some(from) => {
                    let data = st.files.remove(&from).unwrap_or_default();
                    st.files.insert(resolve(&cwd, arg), data);
                    "250 Renamed".into()
                }
                None => "503 RNFR first".into(),
            },
            _ => "502 Command not implemented".into(),
        };
        drop(st);
        write!(out, "{reply}\r\n")?;
        out.flush()?;
    }
}
