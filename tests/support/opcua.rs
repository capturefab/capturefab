//! A real OPC-UA TCP server, with certificates generated entirely in Rust.
use opcua::{
    crypto::{CertificateStore, SecurityPolicy, X509Data},
    server::{
        ANONYMOUS_USER_TOKEN_ID, ServerBuilder, ServerHandle, ServerUserToken,
        address_space::Variable,
        diagnostics::NamespaceMetadata,
        node_manager::memory::{
            SimpleNodeManager, simple_node_manager, simple_node_manager_imports,
        },
    },
    types::{ApplicationDescription, DataValue, MessageSecurityMode, NodeId, StatusCode},
};
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub const NAMESPACE: &str = "urn:capturefab:opcua-test";
pub const USER: &str = "fixture-reader";
pub const PASSWORD: &str = "fixture-password-private";

pub struct Server {
    pub endpoint: String,
    pub namespace: u16,
    pub certificate: PathBuf,
    pub reads: Arc<AtomicU64>,
    pub namespace_reads: Arc<AtomicU64>,
    namespace_reply: Option<Arc<Mutex<DataValue>>>,
    handle: ServerHandle,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(root: &Path, values: Vec<(&'static str, DataValue)>) -> Self {
        Self::start_with(root, values, None, false, None)
    }
    pub fn start_on(
        root: &Path,
        values: Vec<(&'static str, DataValue)>,
        listener: TcpListener,
        padding: bool,
    ) -> Self {
        Self::start_with(root, values, Some(listener), padding, None)
    }
    pub fn start_namespace_reply(
        root: &Path,
        values: Vec<(&'static str, DataValue)>,
        reply: DataValue,
        delay: Duration,
    ) -> Self {
        Self::start_with(root, values, None, false, Some((reply, delay)))
    }
    pub fn set_namespace_reply(&self, value: DataValue) {
        *self.namespace_reply.as_ref().unwrap().lock().unwrap() = value;
    }
    fn start_with(
        root: &Path,
        values: Vec<(&'static str, DataValue)>,
        listener: Option<TcpListener>,
        padding: bool,
        reply: Option<(DataValue, Duration)>,
    ) -> Self {
        let listener = listener.unwrap_or_else(|| TcpListener::bind("127.0.0.1:0").unwrap());
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = format!("opc.tcp://{address}/fixture");
        let pki = root.join("server-pki");
        let certificate = pki.join("own/cert.der");
        let reads = Arc::new(AtomicU64::new(0));
        let observed_reads = reads.clone();
        let namespace_reads = Arc::new(AtomicU64::new(0));
        let observed_namespace_reads = namespace_reads.clone();
        let namespace_reply = reply
            .as_ref()
            .map(|(value, _)| Arc::new(Mutex::new(value.clone())));
        let reply_for_worker = namespace_reply.clone();
        let advertised_endpoint = endpoint.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let tokens = [ANONYMOUS_USER_TOKEN_ID, "reader"];
                let mut builder = ServerBuilder::new()
                    .application_name("Capturefab OPC-UA test fixture")
                    .application_uri("urn:capturefab:opcua-fixture")
                    .host("127.0.0.1")
                    .port(address.port())
                    .discovery_urls(vec![advertised_endpoint])
                    .create_sample_keypair(true)
                    .pki_dir(pki)
                    .trust_client_certs(true)
                    .add_user_token("reader", ServerUserToken::user_pass(USER, PASSWORD))
                    .add_endpoint(
                        "none",
                        (
                            "/fixture",
                            SecurityPolicy::None,
                            MessageSecurityMode::None,
                            &tokens[..1],
                        ),
                    )
                    .add_endpoint(
                        "secure",
                        (
                            "/fixture",
                            SecurityPolicy::Basic256Sha256,
                            MessageSecurityMode::SignAndEncrypt,
                            &tokens[..],
                        ),
                    )
                    .add_endpoint(
                        "aes128",
                        (
                            "/fixture",
                            SecurityPolicy::Aes128Sha256RsaOaep,
                            MessageSecurityMode::SignAndEncrypt,
                            &tokens[..],
                        ),
                    )
                    .add_endpoint(
                        "aes256",
                        (
                            "/fixture",
                            SecurityPolicy::Aes256Sha256RsaPss,
                            MessageSecurityMode::SignAndEncrypt,
                            &tokens[..],
                        ),
                    );
                if reply.is_some() {
                    // Import the stock core node set through its public simple
                    // manager, so only NamespaceArray's read callback is replaced.
                    builder = builder
                        .without_node_managers()
                        .with_node_manager(simple_node_manager_imports(
                            vec![Box::new(opcua::core_namespace::CoreNamespace)],
                            "namespace-core",
                        ))
                        .with_node_manager(simple_node_manager(
                            NamespaceMetadata {
                                namespace_uri: "urn:capturefab:opcua-fixture".into(),
                                ..Default::default()
                            },
                            "namespace-app",
                        ));
                }
                if padding {
                    builder = builder.with_node_manager(simple_node_manager(
                        NamespaceMetadata {
                            namespace_uri: "urn:capturefab:padding".into(),
                            ..Default::default()
                        },
                        "padding",
                    ));
                }
                let (server, handle) = builder
                    .with_node_manager(simple_node_manager(
                        NamespaceMetadata {
                            namespace_uri: NAMESPACE.into(),
                            ..Default::default()
                        },
                        "fixture",
                    ))
                    .build()
                    .unwrap();
                let namespace = handle.get_namespace_index(NAMESPACE).unwrap();
                let manager = handle
                    .node_managers()
                    .get_by_name::<SimpleNodeManager>("fixture")
                    .unwrap();
                if let Some((_, delay)) = reply {
                    let core = handle
                        .node_managers()
                        .get_by_name::<SimpleNodeManager>("namespace-core")
                        .unwrap();
                    let reply = reply_for_worker.unwrap();
                    core.inner()
                        .add_read_callback(NodeId::new(0, 2255_u32), move |_, _, _| {
                            observed_namespace_reads.fetch_add(1, Ordering::Relaxed);
                            if !delay.is_zero() {
                                thread::sleep(delay);
                            }
                            Ok(reply
                                .lock()
                                .map_err(|_| StatusCode::BadInternalError)?
                                .clone())
                        });
                }
                for (name, value) in values {
                    let id = NodeId::new(namespace, name);
                    // A nonempty initial value supplies the mandatory data type;
                    // the callback returns the actual value, including Null.
                    assert!(
                        manager
                            .address_space()
                            .write()
                            .add_variables(
                                vec![Variable::new(&id, name, name, 0.0_f64)],
                                &NodeId::objects_folder_id(),
                            )
                            .into_iter()
                            .all(|inserted| inserted)
                    );
                    let reads = observed_reads.clone();
                    manager.inner().add_read_callback(id, move |_, _, _| {
                        reads.fetch_add(1, Ordering::Relaxed);
                        Ok(value.clone())
                    });
                }
                ready_tx.send((namespace, handle)).unwrap();
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                server.run_with(listener).await.unwrap();
            });
        });
        let (namespace, handle) = ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        Self {
            endpoint,
            namespace,
            certificate,
            reads,
            namespace_reads,
            namespace_reply,
            handle,
            worker: Some(worker),
        }
    }

    pub fn node(&self, name: &str) -> String {
        format!("ns={};s={name}", self.namespace)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.cancel();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

/// Generate a key pair without invoking an interpreter or an OpenSSL command.
pub fn client_keypair(root: &Path) -> (PathBuf, PathBuf) {
    fs::create_dir_all(root).unwrap();
    let certificate = root.join("client.der");
    let private_key = root.join("client.pem");
    let description = ApplicationDescription {
        application_uri: "urn:capturefab:opcua".into(),
        application_name: "Capturefab test client".into(),
        ..Default::default()
    };
    let parameters = X509Data::from((
        description,
        Some(vec!["localhost".into(), "127.0.0.1".into()]),
    ));
    CertificateStore::create_certificate_and_key(&parameters, false, &certificate, &private_key)
        .unwrap();
    (certificate, private_key)
}
