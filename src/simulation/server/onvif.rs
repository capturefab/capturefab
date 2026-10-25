//! ONVIF Device and Media1 operations used to discover and open a test stream.
//! This is an integration fixture, not a claim of ONVIF profile conformance.
//! Operation schemas: https://www.onvif.org/ver10/media/wsdl/media.wsdl and
//! https://www.onvif.org/ver10/device/wsdl/devicemgmt.wsdl
use super::{LatestFrame, ServerOptions, escape};
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, Timelike};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DEVICE_NS: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA_NS: &str = "http://www.onvif.org/ver10/media/wsdl";
const MAX_CLIENTS: usize = 16;
/// A SOAP fault: its code, ONVIF subcodes and reason (ONVIF Core 5.11).
#[derive(Debug)]
struct Fault(&'static str, &'static [&'static str], &'static str);
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.2)
    }
}
impl std::error::Error for Fault {}
const NOT_SUPPORTED: Fault = Fault(
    "s:Receiver",
    &["ter:ActionNotSupported"],
    "operation is not implemented by the test camera",
);
struct Service {
    name: String,
    endpoint: String,
    media: String,
    snapshot: String,
    rtsp: String,
    uuid: String,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    address: Ipv4Addr,
    shutdown: Arc<AtomicBool>,
    frames: Arc<LatestFrame>,
}
pub(crate) struct OnvifServer {
    service: Arc<Service>,
    threads: Vec<JoinHandle<()>>,
    discovery: bool,
}
impl OnvifServer {
    pub(crate) fn start(
        options: &ServerOptions,
        port: u16,
        width: u32,
        height: u32,
        rtsp: String,
        frames: Arc<LatestFrame>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        let listener =
            TcpListener::bind((options.bind, port)).context("binding virtual ONVIF HTTP port")?;
        listener.set_nonblocking(true)?;
        let base = format!(
            "http://{}:{}",
            options.advertise,
            listener.local_addr()?.port()
        );
        let service = Arc::new(Service {
            name: options.name.clone(),
            endpoint: format!("{base}/onvif/device_service"),
            media: format!("{base}/onvif/media_service"),
            snapshot: format!("{base}/snapshot.jpg"),
            rtsp,
            uuid: uuid()?,
            width,
            height,
            fps: options.fps.round() as u32,
            bitrate: bitrate_kbps(&options.bitrate),
            address: options.advertise,
            shutdown,
            frames,
        });
        let mut server = Self {
            service: service.clone(),
            threads: Vec::new(),
            discovery: false,
        };
        server.threads.push(
            thread::Builder::new()
                .name("capturefab-virtual-onvif".into())
                .spawn(move || {
                    let clients = Arc::new(AtomicUsize::new(0));
                    let mut workers: Vec<JoinHandle<()>> = Vec::new();
                    while !service.shutdown.load(Ordering::Relaxed) {
                        let mut index = 0;
                        while index < workers.len() {
                            if workers[index].is_finished() {
                                let _ = workers.swap_remove(index).join();
                            } else {
                                index += 1;
                            }
                        }
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                if clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
                                    let _ = response(
                                        &mut stream,
                                        503,
                                        "text/plain",
                                        b"Too many test camera clients",
                                    );
                                    continue;
                                }
                                clients.fetch_add(1, Ordering::Relaxed);
                                let service = service.clone();
                                let clients = clients.clone();
                                workers.push(thread::spawn(move || {
                                    let _ = serve(&mut stream, &service);
                                    clients.fetch_sub(1, Ordering::Relaxed);
                                }));
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(10))
                            }
                            Err(_) => break,
                        }
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                })
                .context("starting virtual ONVIF HTTP service")?,
        );
        // A loopback camera is opened directly. Multicast discovery is useful
        // only when its advertised endpoint is reachable on the selected LAN.
        if !options.bind.is_loopback()
            && let Ok(socket) = discovery_socket(options.bind)
        {
            let service = server.service.clone();
            server.threads.push(
                thread::Builder::new()
                    .name("capturefab-virtual-discovery".into())
                    .spawn(move || {
                        let mut buffer = [0u8; 16384];
                        while !service.shutdown.load(Ordering::Relaxed) {
                            let Ok((length, peer)) = socket.recv_from(&mut buffer) else {
                                continue;
                            };
                            let Ok(xml) = std::str::from_utf8(&buffer[..length]) else {
                                continue;
                            };
                            if let Some(body) = probe_reply(xml, &service) {
                                let _ = socket.send_to(body.as_bytes(), peer);
                            }
                        }
                    })
                    .context("starting virtual WS-Discovery")?,
            );
            server.discovery = true;
        }
        Ok(server)
    }
    pub(crate) fn endpoint(&self) -> &str {
        &self.service.endpoint
    }
    pub(crate) fn snapshot(&self) -> &str {
        &self.service.snapshot
    }
    pub(crate) fn discovery_enabled(&self) -> bool {
        self.discovery
    }
}
impl Drop for OnvifServer {
    fn drop(&mut self) {
        self.service.shutdown.store(true, Ordering::Relaxed);
        for worker in self.threads.drain(..) {
            let _ = worker.join();
        }
    }
}
fn bitrate_kbps(value: &str) -> u32 {
    let (number, multiplier) = if let Some(value) = value.strip_suffix(['M', 'm']) {
        (value, 1000.0)
    } else if let Some(value) = value.strip_suffix(['K', 'k']) {
        (value, 1.0)
    } else {
        (value, 0.001)
    };
    (number.parse::<f64>().unwrap_or(4_000_000.0) * multiplier).clamp(1.0, u32::MAX as f64) as u32
}
fn uuid() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("generating virtual camera UUID: {e}"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}
fn discovery_socket(interface: Ipv4Addr) -> Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 3702)).into())?;
    let socket: UdpSocket = socket.into();
    socket.join_multicast_v4(&Ipv4Addr::new(239, 255, 255, 250), &interface)?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(socket)
}
fn probe_reply(xml: &str, service: &Service) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let probe = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "Probe")?;
    let discovery = probe.tag_name().namespace()?;
    if ![
        "http://schemas.xmlsoap.org/ws/2005/04/discovery",
        "http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01",
    ]
    .contains(&discovery)
    {
        return None;
    }
    let message = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "MessageID")?;
    let addressing = message.tag_name().namespace()?;
    if ![
        "http://schemas.xmlsoap.org/ws/2004/08/addressing",
        "http://www.w3.org/2005/08/addressing",
    ]
    .contains(&addressing)
    {
        return None;
    }
    let types = probe
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Types")
        .and_then(|n| n.text())
        .unwrap_or("");
    if !types.is_empty()
        && !types
            .split_whitespace()
            .any(|kind| kind.ends_with(":NetworkVideoTransmitter") || kind.ends_with(":Device"))
    {
        return None;
    }
    let relates = escape(message.text().unwrap_or(""));
    let id = uuid().ok()?;
    Some(format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:a=\"{addressing}\" xmlns:d=\"{discovery}\" xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\"><s:Header><a:MessageID>urn:uuid:{id}</a:MessageID><a:RelatesTo>{relates}</a:RelatesTo><a:Action>{discovery}/ProbeMatches</a:Action></s:Header><s:Body><d:ProbeMatches><d:ProbeMatch><a:EndpointReference><a:Address>urn:uuid:{}</a:Address></a:EndpointReference><d:Types>dn:NetworkVideoTransmitter</d:Types><d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/Capturefab onvif://www.onvif.org/location/test</d:Scopes><d:XAddrs>{}</d:XAddrs><d:MetadataVersion>1</d:MetadataVersion></d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>",
        service.uuid,
        escape(&service.endpoint)
    ))
}
fn serve(stream: &mut TcpStream, service: &Service) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 2048];
    let header_end = loop {
        ensure!(bytes.len() <= 16384, "HTTP header limit");
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end + 4;
        }
        ensure!(
            Instant::now() < deadline && !service.shutdown.load(Ordering::Relaxed),
            "HTTP request timed out"
        );
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(length) => bytes.extend(&buffer[..length]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = headers.split("\r\n");
    let mut request = lines.next().unwrap_or("").split_whitespace();
    let method = request.next().unwrap_or("").to_owned();
    let path = request.next().unwrap_or("").to_owned();
    let mut content_length = 0;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>()?;
            }
            ensure!(
                !name.eq_ignore_ascii_case("Transfer-Encoding"),
                "chunked test camera requests are unsupported"
            );
        }
    }
    ensure!(content_length <= 65536, "SOAP request size limit");
    if method == "GET" && path == "/snapshot.jpg" {
        let Some((_, frame)) = service.frames.wait_after(0, Duration::from_secs(1)) else {
            return response(stream, 503, "text/plain", b"No test frame available");
        };
        let raster = crate::jpeg::Raster::new(&frame)?;
        return match crate::jpeg::software(&raster) {
            Ok(jpeg) => response(stream, 200, "image/jpeg", &jpeg),
            Err(_) => response(
                stream,
                503,
                "text/plain",
                b"JPEG snapshots require a JPEG-enabled build",
            ),
        };
    }
    // Operations are dispatched by namespace, so every service path works:
    // clients guess paths such as /onvif/imaging_service.
    if method != "POST" || !path.starts_with("/onvif/") {
        return response(
            stream,
            404,
            "text/plain",
            b"Use /onvif/device_service, /onvif/media_service or /snapshot.jpg",
        );
    }
    while bytes.len() < header_end + content_length {
        ensure!(
            Instant::now() < deadline && !service.shutdown.load(Ordering::Relaxed),
            "SOAP request timed out"
        );
        match stream.read(&mut buffer) {
            Ok(0) => anyhow::bail!("truncated SOAP request"),
            Ok(length) => bytes.extend(&buffer[..length]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let body = std::str::from_utf8(&bytes[header_end..header_end + content_length])?;
    let (status, reply) = soap_reply(body, service);
    response(
        stream,
        status,
        "application/soap+xml; charset=utf-8",
        reply.as_bytes(),
    )
}
fn response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Service Unavailable",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}
/// Returns the HTTP status and SOAP envelope; faults use HTTP 500 (SOAP 1.2
/// HTTP binding, WS-I Basic Profile).
fn soap_reply(request: &str, s: &Service) -> (u16, String) {
    let result = (|| -> Result<String> {
        const MALFORMED: Fault = Fault("s:Sender", &["ter:WellFormed"], "malformed SOAP request");
        let doc = roxmltree::Document::parse(request).map_err(|_| MALFORMED)?;
        let operation = doc
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "Body")
            .and_then(|body| body.children().find(|n| n.is_element()))
            .ok_or(MALFORMED)?;
        ensure!(
            [Some(DEVICE_NS), Some(MEDIA_NS)].contains(&operation.tag_name().namespace()),
            NOT_SUPPORTED
        );
        let argument = |name: &str| {
            operation
                .descendants()
                .find(|n| n.is_element() && n.tag_name().name() == name)
                .and_then(|n| n.text())
                .map(str::trim)
        };
        ensure!(
            argument("ProfileToken").is_none_or(|token| token == "main"),
            Fault(
                "s:Sender",
                &["ter:InvalidArgVal", "ter:NoProfile"],
                "unknown profile token"
            )
        );
        ensure!(
            argument("ConfigurationToken")
                .is_none_or(|token| ["sourceconfig0", "encoder0"].contains(&token)),
            Fault(
                "s:Sender",
                &["ter:InvalidArgVal", "ter:NoConfig"],
                "unknown configuration token"
            )
        );
        let endpoint = escape(&s.endpoint);
        let media = escape(&s.media);
        let encoding = encoder(s);
        let source = source_configuration(s);
        let include = argument("IncludeCapability") == Some("true");
        let (device_service, media_service) = if include {
            (
                format!(
                    "<tds:Capabilities>{}</tds:Capabilities>",
                    device_capabilities()
                ),
                format!(
                    "<tds:Capabilities>{}</tds:Capabilities>",
                    media_capabilities()
                ),
            )
        } else {
            Default::default()
        };
        Ok(match operation.tag_name().name() {
            "GetDeviceInformation"=>format!("<tds:GetDeviceInformationResponse><tds:Manufacturer>Capturefab</tds:Manufacturer><tds:Model>Virtual test camera</tds:Model><tds:FirmwareVersion>{}</tds:FirmwareVersion><tds:SerialNumber>{}</tds:SerialNumber><tds:HardwareId>simulation</tds:HardwareId></tds:GetDeviceInformationResponse>",env!("CARGO_PKG_VERSION"),s.uuid),
            "GetSystemDateAndTime"=>{let now=chrono::Utc::now();format!("<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:DateTimeType>Manual</tt:DateTimeType><tt:DaylightSavings>false</tt:DaylightSavings><tt:TimeZone><tt:TZ>UTC0</tt:TZ></tt:TimeZone><tt:UTCDateTime><tt:Time><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time><tt:Date><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>",now.hour(),now.minute(),now.second(),now.year(),now.month(),now.day())},
            "GetServices"=>format!("<tds:GetServicesResponse><tds:Service><tds:Namespace>{DEVICE_NS}</tds:Namespace><tds:XAddr>{endpoint}</tds:XAddr>{device_service}<tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version></tds:Service><tds:Service><tds:Namespace>{MEDIA_NS}</tds:Namespace><tds:XAddr>{media}</tds:XAddr>{media_service}<tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version></tds:Service></tds:GetServicesResponse>"),
            "GetCapabilities"=>format!("<tds:GetCapabilitiesResponse><tds:Capabilities><tt:Device><tt:XAddr>{endpoint}</tt:XAddr><tt:Network><tt:IPFilter>false</tt:IPFilter><tt:ZeroConfiguration>false</tt:ZeroConfiguration><tt:IPVersion6>false</tt:IPVersion6><tt:DynDNS>false</tt:DynDNS></tt:Network><tt:System><tt:DiscoveryResolve>false</tt:DiscoveryResolve><tt:DiscoveryBye>false</tt:DiscoveryBye><tt:RemoteDiscovery>false</tt:RemoteDiscovery><tt:SystemBackup>false</tt:SystemBackup><tt:SystemLogging>false</tt:SystemLogging><tt:FirmwareUpgrade>false</tt:FirmwareUpgrade><tt:SupportedVersions><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tt:SupportedVersions></tt:System><tt:Security><tt:TLS1.1>false</tt:TLS1.1><tt:TLS1.2>false</tt:TLS1.2><tt:OnboardKeyGeneration>false</tt:OnboardKeyGeneration><tt:AccessPolicyConfig>false</tt:AccessPolicyConfig><tt:X.509Token>false</tt:X.509Token><tt:SAMLToken>false</tt:SAMLToken><tt:KerberosToken>false</tt:KerberosToken><tt:RELToken>false</tt:RELToken></tt:Security></tt:Device><tt:Media><tt:XAddr>{media}</tt:XAddr><tt:StreamingCapabilities><tt:RTPMulticast>false</tt:RTPMulticast><tt:RTP_TCP>true</tt:RTP_TCP><tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities></tt:Media></tds:Capabilities></tds:GetCapabilitiesResponse>"),
            "GetServiceCapabilities" if operation.tag_name().namespace()==Some(MEDIA_NS)=>format!("<trt:GetServiceCapabilitiesResponse>{}</trt:GetServiceCapabilitiesResponse>",media_capabilities()),
            "GetServiceCapabilities"=>format!("<tds:GetServiceCapabilitiesResponse>{}</tds:GetServiceCapabilitiesResponse>",device_capabilities()),
            "GetScopes"=>"<tds:GetScopesResponse><tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/name/Capturefab</tt:ScopeItem></tds:Scopes><tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/type/video_encoder</tt:ScopeItem></tds:Scopes></tds:GetScopesResponse>".into(),
            "GetHostname"=>format!("<tds:GetHostnameResponse><tds:HostnameInformation><tt:FromDHCP>false</tt:FromDHCP><tt:Name>{}</tt:Name></tds:HostnameInformation></tds:GetHostnameResponse>",escape(&s.name)),
            "GetNetworkInterfaces"=>format!("<tds:GetNetworkInterfacesResponse><tds:NetworkInterfaces token=\"simulation\"><tt:Enabled>true</tt:Enabled><tt:Info><tt:Name>simulation</tt:Name><tt:HwAddress>02:00:00:00:00:01</tt:HwAddress><tt:MTU>1500</tt:MTU></tt:Info><tt:IPv4><tt:Enabled>true</tt:Enabled><tt:Config><tt:Manual><tt:Address>{}</tt:Address><tt:PrefixLength>24</tt:PrefixLength></tt:Manual><tt:DHCP>false</tt:DHCP></tt:Config></tt:IPv4></tds:NetworkInterfaces></tds:GetNetworkInterfacesResponse>",s.address),
            "GetNetworkProtocols"=>format!("<tds:GetNetworkProtocolsResponse><tds:NetworkProtocols><tt:Name>HTTP</tt:Name><tt:Enabled>true</tt:Enabled><tt:Port>{}</tt:Port></tds:NetworkProtocols><tds:NetworkProtocols><tt:Name>RTSP</tt:Name><tt:Enabled>true</tt:Enabled><tt:Port>{}</tt:Port></tds:NetworkProtocols></tds:GetNetworkProtocolsResponse>",port(&s.endpoint,80),port(&s.rtsp,554)),
            "GetDNS"=>"<tds:GetDNSResponse><tds:DNSInformation><tt:FromDHCP>false</tt:FromDHCP></tds:DNSInformation></tds:GetDNSResponse>".into(),
            "GetNTP"=>"<tds:GetNTPResponse><tds:NTPInformation><tt:FromDHCP>false</tt:FromDHCP></tds:NTPInformation></tds:GetNTPResponse>".into(),
            "GetNetworkDefaultGateway"=>"<tds:GetNetworkDefaultGatewayResponse><tds:NetworkGateway/></tds:GetNetworkDefaultGatewayResponse>".into(),
            "GetDiscoveryMode"=>"<tds:GetDiscoveryModeResponse><tds:DiscoveryMode>Discoverable</tds:DiscoveryMode></tds:GetDiscoveryModeResponse>".into(),
            "GetVideoSources"=>format!("<trt:GetVideoSourcesResponse><trt:VideoSources token=\"source0\"><tt:Framerate>{}</tt:Framerate><tt:Resolution><tt:Width>{}</tt:Width><tt:Height>{}</tt:Height></tt:Resolution></trt:VideoSources></trt:GetVideoSourcesResponse>",s.fps,s.width,s.height),
            "GetAudioSources"=>"<trt:GetAudioSourcesResponse/>".into(),
            "GetProfiles"=>format!("<trt:GetProfilesResponse><trt:Profiles token=\"main\" fixed=\"true\">{}</trt:Profiles></trt:GetProfilesResponse>",profile(s)),
            "GetProfile"=>format!("<trt:GetProfileResponse><trt:Profile token=\"main\" fixed=\"true\">{}</trt:Profile></trt:GetProfileResponse>",profile(s)),
            "GetStreamUri" if argument("Stream").is_some_and(|stream| stream != "RTP-Unicast") || argument("Protocol") == Some("HTTP") => anyhow::bail!(Fault("s:Sender", &["ter:InvalidArgVal", "ter:InvalidStreamSetup"], "the test camera streams RTP unicast over UDP or RTSP")),
            "GetStreamUri"=>format!("<trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>{}</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT0S</tt:Timeout></trt:MediaUri></trt:GetStreamUriResponse>",escape(&s.rtsp)),
            "GetSnapshotUri" if cfg!(feature="jpeg")=>format!("<trt:GetSnapshotUriResponse><trt:MediaUri><tt:Uri>{}</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT0S</tt:Timeout></trt:MediaUri></trt:GetSnapshotUriResponse>",escape(&s.snapshot)),
            "GetVideoEncoderConfiguration"=>format!("<trt:GetVideoEncoderConfigurationResponse><trt:Configuration token=\"encoder0\">{encoding}</trt:Configuration></trt:GetVideoEncoderConfigurationResponse>"),
            "GetVideoEncoderConfigurations"=>format!("<trt:GetVideoEncoderConfigurationsResponse><trt:Configurations token=\"encoder0\">{encoding}</trt:Configurations></trt:GetVideoEncoderConfigurationsResponse>"),
            "GetVideoEncoderConfigurationOptions"=>format!("<trt:GetVideoEncoderConfigurationOptionsResponse><trt:Options><tt:QualityRange><tt:Min>1</tt:Min><tt:Max>10</tt:Max></tt:QualityRange><tt:H264><tt:ResolutionsAvailable><tt:Width>{}</tt:Width><tt:Height>{}</tt:Height></tt:ResolutionsAvailable><tt:GovLengthRange><tt:Min>{}</tt:Min><tt:Max>{}</tt:Max></tt:GovLengthRange><tt:FrameRateRange><tt:Min>{}</tt:Min><tt:Max>{}</tt:Max></tt:FrameRateRange><tt:EncodingIntervalRange><tt:Min>1</tt:Min><tt:Max>1</tt:Max></tt:EncodingIntervalRange><tt:H264ProfilesSupported>Baseline</tt:H264ProfilesSupported></tt:H264></trt:Options></trt:GetVideoEncoderConfigurationOptionsResponse>",s.width,s.height,s.fps,s.fps,s.fps,s.fps),
            "GetVideoSourceConfiguration"=>format!("<trt:GetVideoSourceConfigurationResponse><trt:Configuration token=\"sourceconfig0\">{source}</trt:Configuration></trt:GetVideoSourceConfigurationResponse>"),
            "GetVideoSourceConfigurations"=>format!("<trt:GetVideoSourceConfigurationsResponse><trt:Configurations token=\"sourceconfig0\">{source}</trt:Configurations></trt:GetVideoSourceConfigurationsResponse>"),
            "GetCompatibleVideoSourceConfigurations"=>format!("<trt:GetCompatibleVideoSourceConfigurationsResponse><trt:Configurations token=\"sourceconfig0\">{source}</trt:Configurations></trt:GetCompatibleVideoSourceConfigurationsResponse>"),
            "GetCompatibleVideoEncoderConfigurations"=>format!("<trt:GetCompatibleVideoEncoderConfigurationsResponse><trt:Configurations token=\"encoder0\">{encoding}</trt:Configurations></trt:GetCompatibleVideoEncoderConfigurationsResponse>"),
            "GetVideoSourceConfigurationOptions"=>format!("<trt:GetVideoSourceConfigurationOptionsResponse><trt:Options><tt:BoundsRange><tt:XRange><tt:Min>0</tt:Min><tt:Max>0</tt:Max></tt:XRange><tt:YRange><tt:Min>0</tt:Min><tt:Max>0</tt:Max></tt:YRange><tt:WidthRange><tt:Min>{w}</tt:Min><tt:Max>{w}</tt:Max></tt:WidthRange><tt:HeightRange><tt:Min>{h}</tt:Min><tt:Max>{h}</tt:Max></tt:HeightRange></tt:BoundsRange><tt:VideoSourceTokensAvailable>source0</tt:VideoSourceTokensAvailable></trt:Options></trt:GetVideoSourceConfigurationOptionsResponse>",w=s.width,h=s.height),
            "GetGuaranteedNumberOfVideoEncoderInstances"=>"<trt:GetGuaranteedNumberOfVideoEncoderInstancesResponse><trt:TotalNumber>1</trt:TotalNumber><trt:H264>1</trt:H264></trt:GetGuaranteedNumberOfVideoEncoderInstancesResponse>".into(),
            _=>anyhow::bail!(NOT_SUPPORTED),
        })
    })();
    let (status, body) = match result {
        Ok(body) => (200, body),
        Err(error) => {
            let Fault(code, subcodes, reason) =
                error.downcast_ref::<Fault>().unwrap_or(&NOT_SUPPORTED);
            let mut subcode = String::new();
            for value in subcodes.iter().rev() {
                subcode = format!("<s:Subcode><s:Value>{value}</s:Value>{subcode}</s:Subcode>");
            }
            (
                500,
                format!(
                    "<s:Fault><s:Code><s:Value>{code}</s:Value>{subcode}</s:Code><s:Reason><s:Text xml:lang=\"en\">{reason}</s:Text></s:Reason></s:Fault>"
                ),
            )
        }
    };
    (
        status,
        format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:tds=\"{DEVICE_NS}\" xmlns:trt=\"{MEDIA_NS}\" xmlns:tt=\"http://www.onvif.org/ver10/schema\" xmlns:ter=\"http://www.onvif.org/ver10/error\"><s:Body>{body}</s:Body></s:Envelope>"
        ),
    )
}
/// The port of an http:// or rtsp:// URL, or the scheme's default.
fn port(url: &str, default: u16) -> u16 {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .and_then(|authority| authority.rsplit_once(':'))
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(default)
}
fn device_capabilities() -> &'static str {
    "<tds:Capabilities><tds:Network IPFilter=\"false\" ZeroConfiguration=\"false\" IPVersion6=\"false\" DynDNS=\"false\"/><tds:Security UsernameToken=\"false\" HttpDigest=\"false\"/><tds:System DiscoveryResolve=\"false\" DiscoveryBye=\"false\" RemoteDiscovery=\"false\" SystemBackup=\"false\" SystemLogging=\"false\" FirmwareUpgrade=\"false\"/></tds:Capabilities>"
}
fn media_capabilities() -> String {
    format!(
        "<trt:Capabilities SnapshotUri=\"{}\"><trt:ProfileCapabilities MaximumNumberOfProfiles=\"1\"/><trt:StreamingCapabilities RTPMulticast=\"false\" RTP_TCP=\"true\" RTP_RTSP_TCP=\"true\"/></trt:Capabilities>",
        cfg!(feature = "jpeg")
    )
}
fn encoder(s: &Service) -> String {
    format!(
        "<tt:Name>H264 test stream</tt:Name><tt:UseCount>1</tt:UseCount><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>{}</tt:Width><tt:Height>{}</tt:Height></tt:Resolution><tt:Quality>5</tt:Quality><tt:RateControl><tt:FrameRateLimit>{}</tt:FrameRateLimit><tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>{}</tt:BitrateLimit></tt:RateControl><tt:H264><tt:GovLength>{}</tt:GovLength><tt:H264Profile>Baseline</tt:H264Profile></tt:H264><tt:Multicast><tt:Address><tt:Type>IPv4</tt:Type><tt:IPv4Address>0.0.0.0</tt:IPv4Address></tt:Address><tt:Port>0</tt:Port><tt:TTL>1</tt:TTL><tt:AutoStart>false</tt:AutoStart></tt:Multicast><tt:SessionTimeout>PT60S</tt:SessionTimeout>",
        s.width, s.height, s.fps, s.bitrate, s.fps
    )
}
fn source_configuration(s: &Service) -> String {
    format!(
        "<tt:Name>Replay source</tt:Name><tt:UseCount>1</tt:UseCount><tt:SourceToken>source0</tt:SourceToken><tt:Bounds x=\"0\" y=\"0\" width=\"{}\" height=\"{}\"/>",
        s.width, s.height
    )
}
fn profile(s: &Service) -> String {
    format!(
        "<tt:Name>{}</tt:Name><tt:VideoSourceConfiguration token=\"sourceconfig0\">{}</tt:VideoSourceConfiguration><tt:VideoEncoderConfiguration token=\"encoder0\">{}</tt:VideoEncoderConfiguration>",
        escape(&s.name),
        source_configuration(s),
        encoder(s)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn loopback() -> (OnvifServer, Arc<AtomicBool>) {
        let options = ServerOptions {
            bind: Ipv4Addr::LOCALHOST,
            advertise: Ipv4Addr::LOCALHOST,
            gige_port: None,
            rtsp_port: Some(8554),
            onvif_port: Some(0),
            name: "Lobby".into(),
            fps: 15.0,
            bitrate: "2M".into(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server = OnvifServer::start(
            &options,
            0,
            640,
            480,
            "rtsp://127.0.0.1:8554/live".into(),
            Arc::new(LatestFrame::default()),
            stop.clone(),
        )
        .unwrap();
        (server, stop)
    }
    /// Posts one SOAP body; returns the HTTP status and response body.
    fn post(endpoint: &str, path: &str, body: &str) -> (u16, String) {
        let authority = endpoint
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .unwrap();
        let mut stream = TcpStream::connect(authority).unwrap();
        let envelope = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:tds=\"{DEVICE_NS}\" xmlns:trt=\"{MEDIA_NS}\" xmlns:tt=\"http://www.onvif.org/ver10/schema\"><s:Body>{body}</s:Body></s:Envelope>"
        );
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\n\r\n{envelope}",
            envelope.len()
        )
        .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        roxmltree::Document::parse(body).unwrap();
        (status, body.to_owned())
    }

    #[test]
    fn faults_use_http_500_and_onvif_subcodes() {
        let (server, _stop) = loopback();
        let device = "/onvif/device_service";
        let (status, body) = post(
            server.endpoint(),
            device,
            "<tds:SetHostname><tds:Name>x</tds:Name></tds:SetHostname>",
        );
        assert_eq!(status, 500);
        assert!(body.contains("<s:Fault><s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:ActionNotSupported</s:Value></s:Subcode></s:Code>"), "{body}");
        let (status, body) = post(
            server.endpoint(),
            "/onvif/media_service",
            "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>old</trt:ProfileToken></trt:GetStreamUri>",
        );
        assert_eq!(status, 500);
        assert!(body.contains("<s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:InvalidArgVal</s:Value><s:Subcode><s:Value>ter:NoProfile</s:Value></s:Subcode></s:Subcode></s:Code>"), "{body}");
        // RTSP over HTTP and multicast are not served.
        for (stream, protocol) in [("RTP-Unicast", "HTTP"), ("RTP-Multicast", "UDP")] {
            let (status, body) = post(
                server.endpoint(),
                "/onvif/media_service",
                &format!(
                    "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>{stream}</tt:Stream><tt:Transport><tt:Protocol>{protocol}</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>main</trt:ProfileToken></trt:GetStreamUri>"
                ),
            );
            assert_eq!(status, 500);
            assert!(
                body.contains("<s:Value>ter:InvalidStreamSetup</s:Value>"),
                "{body}"
            );
        }
        let (status, body) = post(
            server.endpoint(),
            "/onvif/media_service",
            "<trt:GetVideoEncoderConfiguration><trt:ConfigurationToken>gone</trt:ConfigurationToken></trt:GetVideoEncoderConfiguration>",
        );
        assert_eq!(status, 500);
        assert!(body.contains("<s:Value>ter:NoConfig</s:Value>"), "{body}");
        let authority = server
            .endpoint()
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        let mut stream = TcpStream::connect(authority).unwrap();
        write!(
            stream,
            "POST {device} HTTP/1.1\r\nContent-Length: 6\r\n\r\n<s:Env"
        )
        .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(
            reply.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
            "{reply}"
        );
        assert!(reply.contains("<s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:WellFormed</s:Value></s:Subcode></s:Code>"), "{reply}");
    }

    #[test]
    fn profile_s_getters_answer_on_any_service_path() {
        let (server, _stop) = loopback();
        let http = port(server.endpoint(), 0);
        for (path, body, expected) in [
            (
                "/onvif/device_service",
                "<tds:GetNetworkProtocols/>".to_owned(),
                format!("<tds:NetworkProtocols><tt:Name>HTTP</tt:Name><tt:Enabled>true</tt:Enabled><tt:Port>{http}</tt:Port></tds:NetworkProtocols><tds:NetworkProtocols><tt:Name>RTSP</tt:Name><tt:Enabled>true</tt:Enabled><tt:Port>8554</tt:Port></tds:NetworkProtocols>"),
            ),
            ("/onvif/device_service", "<tds:GetDNS/>".into(), "<tt:FromDHCP>false</tt:FromDHCP>".into()),
            ("/onvif/device_service", "<tds:GetNTP/>".into(), "<tds:NTPInformation>".into()),
            ("/onvif/device_service", "<tds:GetNetworkDefaultGateway/>".into(), "<tds:NetworkGateway/>".into()),
            (
                "/onvif/device_service",
                "<tds:GetServices><tds:IncludeCapability>true</tds:IncludeCapability></tds:GetServices>".into(),
                "</tds:XAddr><tds:Capabilities><trt:Capabilities SnapshotUri=".into(),
            ),
            (
                "/onvif/media_service",
                "<trt:GetVideoSourceConfigurations/>".into(),
                "<trt:Configurations token=\"sourceconfig0\"><tt:Name>Replay source</tt:Name><tt:UseCount>1</tt:UseCount><tt:SourceToken>source0</tt:SourceToken><tt:Bounds x=\"0\" y=\"0\" width=\"640\" height=\"480\"/>".into(),
            ),
            (
                "/onvif/media_service",
                "<trt:GetVideoSourceConfiguration><trt:ConfigurationToken>sourceconfig0</trt:ConfigurationToken></trt:GetVideoSourceConfiguration>".into(),
                "<trt:Configuration token=\"sourceconfig0\">".into(),
            ),
            (
                "/onvif/media_service",
                "<trt:GetCompatibleVideoEncoderConfigurations><trt:ProfileToken>main</trt:ProfileToken></trt:GetCompatibleVideoEncoderConfigurations>".into(),
                "<trt:Configurations token=\"encoder0\"><tt:Name>H264 test stream</tt:Name>".into(),
            ),
            (
                "/onvif/media_service",
                "<trt:GetCompatibleVideoSourceConfigurations><trt:ProfileToken>main</trt:ProfileToken></trt:GetCompatibleVideoSourceConfigurations>".into(),
                "<trt:Configurations token=\"sourceconfig0\">".into(),
            ),
            (
                "/onvif/media_service",
                "<trt:GetVideoSourceConfigurationOptions><trt:ConfigurationToken>sourceconfig0</trt:ConfigurationToken></trt:GetVideoSourceConfigurationOptions>".into(),
                "<tt:WidthRange><tt:Min>640</tt:Min><tt:Max>640</tt:Max></tt:WidthRange><tt:HeightRange><tt:Min>480</tt:Min><tt:Max>480</tt:Max></tt:HeightRange></tt:BoundsRange><tt:VideoSourceTokensAvailable>source0</tt:VideoSourceTokensAvailable>".into(),
            ),
            (
                "/onvif/media_service",
                "<trt:GetGuaranteedNumberOfVideoEncoderInstances><trt:ConfigurationToken>sourceconfig0</trt:ConfigurationToken></trt:GetGuaranteedNumberOfVideoEncoderInstances>".into(),
                "<trt:TotalNumber>1</trt:TotalNumber><trt:H264>1</trt:H264>".into(),
            ),
            // go2rtc asks the imaging path it falls back to.
            (
                "/onvif/imaging_service",
                "<trt:GetStreamUri><trt:ProfileToken>main</trt:ProfileToken></trt:GetStreamUri>".into(),
                "<tt:Uri>rtsp://127.0.0.1:8554/live</tt:Uri>".into(),
            ),
        ] {
            let (status, reply) = post(server.endpoint(), path, &body);
            assert_eq!(status, 200, "{body}: {reply}");
            assert!(reply.contains(&expected), "{body}: {reply}");
        }
    }

    #[test]
    fn device_and_media_resolution_use_real_advertised_endpoint() {
        let options = ServerOptions {
            bind: Ipv4Addr::LOCALHOST,
            advertise: Ipv4Addr::LOCALHOST,
            gige_port: None,
            rtsp_port: Some(8554),
            onvif_port: Some(0),
            name: "Camera & test".into(),
            fps: 15.0,
            bitrate: "2M".into(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server = OnvifServer::start(
            &options,
            0,
            640,
            480,
            "rtsp://127.0.0.1:8554/camera".into(),
            Arc::new(LatestFrame::default()),
            stop,
        )
        .unwrap();
        let profiles =
            crate::onvif::resolve(server.endpoint(), None, None, Duration::from_secs(2)).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].token, "main");
        assert_eq!(profiles[0].name, "Camera & test");
        assert_eq!(profiles[0].width, Some(640));
        assert_eq!(profiles[0].height, Some(480));
        assert_eq!(profiles[0].uri, "rtsp://127.0.0.1:8554/camera");
    }
}
