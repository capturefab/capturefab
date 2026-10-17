//! An in-process SSH server with an in-memory SFTP file system, for the SFTP
//! uploader tests. Its host key comes from a fixed seed; password `secret`
//! signs in user `cam`, whose home is `/home/cam`.
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

const HOME: &str = "/home/cam";

#[derive(Default)]
struct Fs {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
}

pub struct TestServer {
    pub port: u16,
    pub fingerprint: String,
    fs: Arc<Mutex<Fs>>,
}

impl TestServer {
    pub fn start() -> Self {
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[7; 32]));
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let fs = Arc::new(Mutex::new(Fs {
            dirs: ["/".to_string(), "/home".into(), HOME.into()].into(),
            ..Fs::default()
        }));
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            auth_rejection_time: std::time::Duration::from_millis(10),
            auth_rejection_time_initial: Some(std::time::Duration::ZERO),
            ..Default::default()
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = fs.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    let handler = Ssh {
                        fs: shared.clone(),
                        channels: HashMap::new(),
                    };
                    let config = config.clone();
                    tokio::spawn(async move {
                        if let Ok(session) =
                            russh::server::run_stream(config, socket, handler).await
                        {
                            let _ = session.await;
                        }
                    });
                }
            });
        });
        Self {
            port,
            fingerprint,
            fs,
        }
    }

    pub fn put(&self, path: &str, data: &[u8]) {
        let mut fs = self.fs.lock().unwrap();
        let mut parent = path.rsplit_once('/').unwrap().0.to_string();
        while !parent.is_empty() {
            fs.dirs.insert(parent.clone());
            parent = parent.rsplit_once('/').unwrap().0.to_string();
        }
        fs.files.insert(path.into(), data.to_vec());
    }

    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.fs.lock().unwrap().files.get(path).cloned()
    }

    pub fn files(&self) -> Vec<String> {
        self.fs.lock().unwrap().files.keys().cloned().collect()
    }
}

struct Ssh {
    fs: Arc<Mutex<Fs>>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl russh::server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "cam" && password == "secret" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.channels.remove(&id) {
            Some(channel) if name == "sftp" => {
                session.channel_success(id)?;
                let sftp = Sftp {
                    fs: self.fs.clone(),
                    open: HashMap::new(),
                    next: 0,
                };
                tokio::spawn(russh_sftp::server::run(channel.into_stream(), sftp));
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }
}

struct Sftp {
    fs: Arc<Mutex<Fs>>,
    /// Open handles and the file each writes.
    open: HashMap<String, String>,
    next: u64,
}

fn resolve(path: &str) -> String {
    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        HOME.split('/').filter(|p| !p.is_empty()).collect()
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

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = resolve(&filename);
        let mut fs = self.fs.lock().unwrap();
        let parent = path.rsplit_once('/').unwrap().0;
        if !fs
            .dirs
            .contains(if parent.is_empty() { "/" } else { parent })
        {
            return Err(StatusCode::NoSuchFile);
        }
        let exists = fs.files.contains_key(&path);
        if exists && flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUDE) {
            return Err(StatusCode::Failure);
        }
        if !exists {
            if !flags.contains(OpenFlags::CREATE) {
                return Err(StatusCode::NoSuchFile);
            }
            fs.files.insert(path.clone(), Vec::new());
        }
        self.next += 1;
        let handle = format!("h{}", self.next);
        self.open.insert(handle.clone(), path);
        Ok(Handle { id, handle })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let path = self.open.get(&handle).ok_or(StatusCode::Failure)?;
        let mut fs = self.fs.lock().unwrap();
        let file = fs.files.get_mut(path).ok_or(StatusCode::NoSuchFile)?;
        let end = offset as usize + data.len();
        if file.len() < end {
            file.resize(end, 0);
        }
        file[offset as usize..end].copy_from_slice(&data);
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.open.remove(&handle);
        Ok(ok(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let path = resolve(&path);
        let fs = self.fs.lock().unwrap();
        let mut attrs = FileAttributes::empty();
        if let Some(data) = fs.files.get(&path) {
            attrs.size = Some(data.len() as u64);
            attrs.set_regular(true);
        } else if fs.dirs.contains(&path) {
            attrs.set_dir(true);
        } else {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(Attrs { id, attrs })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = resolve(&path);
        let mut fs = self.fs.lock().unwrap();
        if fs.dirs.contains(&path) || fs.files.contains_key(&path) {
            return Err(StatusCode::Failure);
        }
        fs.dirs.insert(path);
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        match self.fs.lock().unwrap().files.remove(&resolve(&filename)) {
            Some(_) => Ok(ok(id)),
            None => Err(StatusCode::NoSuchFile),
        }
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let (from, to) = (resolve(&oldpath), resolve(&newpath));
        let mut fs = self.fs.lock().unwrap();
        // SFTP version 3: never replace an existing target.
        if fs.files.contains_key(&to) || fs.dirs.contains(&to) {
            return Err(StatusCode::Failure);
        }
        let data = fs.files.remove(&from).ok_or(StatusCode::NoSuchFile)?;
        fs.files.insert(to, data);
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![russh_sftp::protocol::File::dummy(resolve(&path))],
        })
    }
}
