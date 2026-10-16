//! Network checks are snapshots; stream health comes from live transport counters.
use super::*;
use crate::{
    network::{InterfaceKind, NetworkCamera, NetworkHealth},
    session::CameraSnapshot,
    types::Transport,
};
use iced::widget::column;

pub(super) struct LinkStatus {
    pub label: &'static str,
    pub detail: String,
    pub level: Level,
}

impl LinkStatus {
    pub fn color(&self, p: &Palette) -> Color {
        match self.level {
            Level::Error => p.ink(p.danger),
            Level::Warning => p.ink(p.warn),
            Level::Done => p.ink(p.live),
        }
    }
}

fn checked_status(camera: &NetworkCamera) -> LinkStatus {
    let (label, level) = match camera.health {
        NetworkHealth::Unreachable => ("Unreachable", Level::Error),
        NetworkHealth::Limited => ("Limited link", Level::Warning),
        NetworkHealth::Unverified => ("Stability unverified", Level::Warning),
        NetworkHealth::Ready => ("Link ready", Level::Done),
        NetworkHealth::Unstable => ("Unstable link", Level::Warning),
    };
    LinkStatus {
        label,
        detail: camera.summary.clone(),
        level,
    }
}

/// Zero loss without any traffic is no evidence of stream stability. Counters
/// are cumulative, so recovered retries are named without claiming current loss.
fn live_status(camera: &CameraSnapshot, stalled: bool) -> LinkStatus {
    let status = |label, detail, level| LinkStatus {
        label,
        detail,
        level,
    };
    if !camera.streaming {
        return status(
            "Stability unverified",
            "Start a stream to check delivery. Network discovery alone cannot verify stability."
                .into(),
            Level::Warning,
        );
    }
    if stalled {
        return status("No frames arriving", "The stream is active but new frames have stopped arriving. Open the network report to check its connection.".into(), Level::Warning);
    }
    if camera.stale {
        return status(
            "Awaiting live checks",
            "The camera worker has not supplied fresh transport counters yet.".into(),
            Level::Warning,
        );
    }
    let Some(stats) = &camera.transport else {
        return status(
            "Stability unverified",
            "This camera does not report packet delivery statistics.".into(),
            Level::Warning,
        );
    };
    let counters = format!(
        "{} packets received · {} retries requested, {} recovered · {} incomplete frames, {} missing frames. These are cumulative transport counters.",
        grouped(stats.packets_received),
        grouped(stats.resend_requested),
        grouped(stats.resend_recovered),
        grouped(stats.incomplete_frames),
        grouped(stats.lost_frames)
    );
    if stats.incomplete_frames > 0 || stats.lost_frames > 0 {
        status("Frame loss observed", counters, Level::Error)
    } else if stats.resend_requested > 0 {
        status("Packet retries observed", counters, Level::Warning)
    } else if stats.packets_received == 0 || camera.frames == 0 {
        status("Awaiting traffic", "No delivered frame with received packets has been reported yet. Stability is unverified.".into(), Level::Warning)
    } else if !stats.notes.is_empty() {
        status(
            "Connection needs attention",
            format!("{counters}\n{}", stats.notes.join("\n")),
            Level::Warning,
        )
    } else {
        status("No loss observed", counters, Level::Done)
    }
}

impl Workbench {
    /// Synthetic read-only reports for the real-renderer UI review. Apply late
    /// so ordinary snapshot refresh cannot replace the report before capture.
    pub(super) fn scene_network(&mut self, word: &str, late: bool) -> bool {
        if !matches!(word, "network" | "network-empty" | "network-error") {
            return false;
        }
        self.network_open = true;
        if !late {
            return false;
        }
        if word == "network-empty" {
            return true;
        }
        use crate::network::{NetworkInterface, NetworkScan, NetworkTuning};
        let mut camera = crate::transport::simulator::info();
        camera.id = "gige:demo-laboratory-camera".into();
        camera.vendor = "Example Imaging".into();
        camera.model = "Laboratory acquisition camera".into();
        camera.transport = Transport::GigE;
        camera.address = Some("192.168.10.50".into());
        self.snapshot.network_scan = Some(NetworkScan {
            interfaces: vec![NetworkInterface {
                name: "Ethernet adapter".into(),
                address: "192.168.10.1".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
                mtu: Some(1500),
                speed_mbps: Some(1000),
                kind: InterfaceKind::Wired,
                up: Some(true),
            }],
            cameras: vec![NetworkCamera {
                camera,
                active: false,
                interface: Some("Ethernet adapter".into()),
                host_address: Some("192.168.10.1".parse().unwrap()),
                reachable: false,
                health: NetworkHealth::Unreachable,
                summary: "Discovery found this camera, but it did not answer control requests. Check its subnet and Ethernet cable.".into(),
                details: vec!["A discovery reply does not verify that an image stream can reach this computer.".into()],
                tuning: Some(NetworkTuning {
                    packet_size: Some(1500),
                    throughput_limit_bps: Some(100_000_000),
                    changes: vec!["Packet size kept within adapter MTU.".into()],
                    warnings: vec!["Could not apply settings because the camera is unreachable.".into()],
                }),
            }],
            warnings: vec!["One discovered camera is unreachable.".into()],
            scanned_at_ms: 1_792_012_000_000,
        });
        if word == "network-error" {
            self.network_error = Some("The network scan worker stopped before every camera answered. Check the adapter connection and scan again.".into());
        }
        true
    }

    pub(super) fn network_camera(&self, camera: &CameraInfo) -> Option<&NetworkCamera> {
        self.snapshot
            .network_scan
            .as_ref()?
            .cameras
            .iter()
            .find(|check| check.camera.id == camera.id && check.camera.address == camera.address)
    }

    pub(super) fn network_status(&self, camera: &CameraInfo) -> Option<LinkStatus> {
        if camera.transport != Transport::GigE {
            return None;
        }
        let check = self.network_camera(camera);
        if let Some(active) = self
            .snapshot
            .cameras
            .iter()
            .find(|active| active.info.id == camera.id)
        {
            let mut live = live_status(active, self.stalled(&camera.id).is_some());
            if let Some(check) = check {
                let checked = checked_status(check);
                live.detail = format!(
                    "{}\nAt the last scan: {}\nRescan after changing cables, adapters, or camera addresses.",
                    live.detail, checked.detail
                );
                // Limited topology remains useful even when delivery currently
                // has no loss. An old reachability check cannot override traffic.
                if check.health == NetworkHealth::Limited && live.level == Level::Done {
                    live.label = checked.label;
                    live.level = checked.level;
                }
                if check.active
                    && check.reachable
                    && let Some(tuning) = &check.tuning
                    && !tuning.warnings.is_empty()
                {
                    live.detail.push_str(&format!(
                        "\nAt the last scan: {}",
                        tuning.warnings.join("\n")
                    ));
                    if live.level == Level::Done {
                        live.label = "Connection needs attention";
                        live.level = Level::Warning;
                    }
                }
            } else {
                live.detail.push_str("\nNetwork adapters and link speed have not been checked. Scan the network to inspect and tune this connection.");
            }
            Some(live)
        } else {
            Some(check.map(checked_status).unwrap_or_else(|| LinkStatus {
                label: "Network not checked",
                detail: "Discovered cameras may be unreachable or use a slow connection. Scan the network to check this camera's connection.".into(),
                level: Level::Warning,
            }))
        }
    }

    pub(super) fn show_network(&mut self, open: bool) -> Task<Message> {
        if open && !self.network_open && self.sheet_open() {
            return Task::none();
        }
        let opening = open && !self.network_open;
        self.network_open = open;
        if opening {
            self.network_scrolled = false;
            Task::batch([
                operation::focus("sheet-close"),
                operation::snap_to(SHEET_BODY, operation::RelativeOffset::START),
            ])
        } else {
            Task::none()
        }
    }

    pub(super) fn scan_network(&mut self) -> Task<Message> {
        let focus = self.show_network(true);
        if !self.network_open || self.pending(Job::NetworkScan) {
            return focus;
        }
        self.network_error = None;
        if let Err(error) = self.try_submit(
            None,
            Job::NetworkScan,
            SessionCommand::NetworkScan { timeout_ms: 1500 },
        ) {
            let full = format!("{error:#}");
            self.network_error = Some(full.clone());
            self.set_notice("Network scan failed", Some(full), Level::Error);
        }
        focus
    }

    pub(super) fn network_scan_button(&self, p: &'static Palette) -> Element<'_, Message> {
        let busy = self.pending(Job::NetworkScan);
        tip(
            button(
                row![
                    if busy {
                        icon::spinner(14.0, p.secondary, self.spin())
                    } else {
                        icon(Icon::Network, 14.0, p.secondary)
                    },
                    text(if busy {
                        "Scanning network…"
                    } else {
                        "Scan network"
                    })
                    .size(style::SMALL),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .name("Scan network")
            .padding([7, 10])
            .style(style::secondary)
            .on_press_maybe((!busy).then_some(Message::ScanNetwork)),
            "Check camera connections and tune connected GigE cameras for the available bandwidth. Live previews may pause briefly.",
        )
    }

    pub(super) fn network_indicator<'a>(
        &self,
        status: LinkStatus,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        tip_above(
            button(
                row![
                    dot(status.color(p), 6.0),
                    text(status.label).size(style::SMALL).color(status.color(p))
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .name(format!(
                "GigE connection: {}. Open network report",
                status.label
            ))
            .description(status.detail.clone())
            .padding([3, 5])
            .style(style::plain)
            .on_press(Message::NetworkReport(true)),
            status.detail,
        )
    }

    pub(super) fn network_sheet(&self, p: &'static Palette, height: f32) -> Element<'_, Message> {
        let busy = self.pending(Job::NetworkScan);
        let mut body = column![
            text("Checks network adapters and GigE cameras, then tunes connected cameras to share the available bandwidth.")
                .size(style::BODY).color(p.secondary),
            text("Live previews may pause briefly while settings are applied. Cameras recording, forwarding, or running scheduled captures keep their settings and show a warning.")
                .size(style::SMALL).color(p.secondary),
        ].spacing(14);
        if busy {
            body = body.push(accessibility::label(
                row![
                    icon::spinner(16.0, p.accent_text, self.spin()),
                    text("Checking connections and applying defaults…").size(style::BODY)
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
                "Checking connections and applying defaults".into(),
                accessibility::Role::Status,
            ));
        }
        if let Some(error) = &self.network_error {
            body = body.push(field_error(format!("Network scan failed: {error}"), p));
        }
        if let Some(report) = &self.snapshot.network_scan {
            let scanned = chrono::DateTime::from_timestamp_millis(
                report.scanned_at_ms.min(i64::MAX as u64) as i64,
            )
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%b %-d, %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown time".into());
            body = body.push(
                text(format!(
                    "{} · scanned at {scanned}. Rescan after changing your network.",
                    if busy || self.network_error.is_some() {
                        "Previous results"
                    } else {
                        "Last scan"
                    }
                ))
                .size(style::SMALL)
                .color(p.secondary),
            );
            if !report.warnings.is_empty() {
                body = body.push(heading("Needs attention", p));
                for warning in &report.warnings {
                    body = body.push(text(warning).size(style::SMALL).color(p.ink(p.warn)));
                }
            }
            body = body.push(heading("Network adapters", p));
            if report.interfaces.is_empty() {
                body = body.push(
                    text("No IPv4 network adapters found.")
                        .size(style::SMALL)
                        .color(p.ink(p.warn)),
                );
            }
            for interface in &report.interfaces {
                let kind = match interface.kind {
                    InterfaceKind::Wired => "Wired Ethernet",
                    InterfaceKind::Wireless => "Wi-Fi",
                    InterfaceKind::Loopback => "Loopback",
                    InterfaceKind::Other => "Other adapter",
                    InterfaceKind::Unknown => "Adapter type unknown",
                };
                let speed = interface
                    .speed_mbps
                    .map_or_else(|| "speed unknown".into(), |speed| format!("{speed} Mbps"));
                let mtu = interface
                    .mtu
                    .map_or_else(|| "MTU unknown".into(), |mtu| format!("MTU {mtu}"));
                let mut detail = format!(
                    "{kind} · {speed} · {mtu}\n{} · subnet mask {}",
                    interface.address, interface.netmask
                );
                if interface.up == Some(false) {
                    detail.push_str("\nAdapter is down");
                }
                body = body.push(accessibility::group(
                    column![
                        text(&interface.name)
                            .size(style::BODY)
                            .font(style::SEMIBOLD),
                        text(detail.clone()).size(style::SMALL).color(p.secondary)
                    ]
                    .spacing(4)
                    .into(),
                    format!("{} ({})", interface.name, interface.address),
                    accessibility::Role::Group,
                ));
            }
            body = body.push(heading("GigE cameras", p));
            if report.cameras.is_empty() {
                body = body.push(text("No GigE cameras found. Check the power and Ethernet cable, then scan again.").size(style::SMALL).color(p.secondary));
            }
            for camera in &report.cameras {
                let checked = checked_status(camera);
                let identity = format!(
                    "{} {} ({})",
                    camera.camera.vendor,
                    camera.camera.model,
                    camera
                        .camera
                        .address
                        .as_deref()
                        .unwrap_or(&camera.camera.id)
                );
                let mut card = column![
                    text(format!(
                        "{} {} · {}",
                        camera.camera.vendor,
                        camera.camera.model,
                        if camera.active {
                            "Connected"
                        } else {
                            "Discovered"
                        }
                    ))
                    .size(style::BODY)
                    .font(style::SEMIBOLD),
                    text(format!("At scan: {}", checked.label))
                        .size(style::BODY)
                        .font(style::SEMIBOLD)
                        .color(checked.color(p)),
                    text(&camera.summary).size(style::SMALL).color(p.secondary),
                ]
                .spacing(6);
                if let Some(address) = &camera.camera.address {
                    card = card.push(
                        text(format!(
                            "Camera {address} · {}",
                            camera.interface.as_deref().unwrap_or("no matching adapter")
                        ))
                        .size(style::SMALL)
                        .color(p.secondary),
                    );
                }
                if let Some(host) = camera.host_address {
                    card = card.push(
                        text(format!("Host address {host}"))
                            .size(style::SMALL)
                            .color(p.secondary),
                    );
                }
                if let Some(live) = self
                    .snapshot
                    .cameras
                    .iter()
                    .find(|active| active.info.id == camera.camera.id)
                {
                    let status = live_status(live, self.stalled(&live.info.id).is_some());
                    card = card
                        .push(
                            text(format!("Live delivery: {}", status.label))
                                .size(style::BODY)
                                .color(status.color(p)),
                        )
                        .push(text(status.detail).size(style::SMALL).color(p.secondary));
                }
                for detail in &camera.details {
                    card = card.push(text(detail).size(style::SMALL).color(p.secondary));
                }
                if let Some(tuning) = &camera.tuning {
                    let packet = tuning
                        .packet_size
                        .map_or_else(|| "unavailable".into(), |size| format!("{size} bytes"));
                    let budget = tuning.throughput_limit_bps.map_or_else(
                        || "camera default".into(),
                        |rate| format!("{:.1} MB/s", rate as f64 / 1_000_000.0),
                    );
                    card = card.push(
                        text(format!("Packet size {packet} · bandwidth budget {budget}"))
                            .size(style::SMALL),
                    );
                    for change in &tuning.changes {
                        card = card.push(text(change).size(style::SMALL).color(p.secondary));
                    }
                    for warning in &tuning.warnings {
                        card = card.push(text(warning).size(style::SMALL).color(p.ink(p.warn)));
                    }
                }
                body = body.push(accessibility::group(
                    container(card)
                        .padding(12)
                        .width(Fill)
                        .style(style::code)
                        .into(),
                    identity.trim().to_owned(),
                    accessibility::Role::Group,
                ));
            }
        } else if !busy && self.network_error.is_none() {
            body = body.push(text("Scan to check reachability, link speed, packet size, and bandwidth for every GigE camera.").size(style::BODY).color(p.secondary));
        }
        sheet_frame("Camera network", Message::NetworkReport(false), body, p)
            .mark(Icon::Network)
            .size(680.0, height)
            .scrolled(self.network_scrolled)
            .on_scroll(Message::NetworkScrolled)
            .footer(
                row![space::horizontal(), self.network_scan_button(p)].align_y(Alignment::Center),
            )
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_review_network_report_does_not_scan_or_change_session_data() {
        let mut bench = Workbench::new(SessionHandle::new(), "ui-review-test".into(), true, None);
        assert!(!bench.scene_network("network-error", false));
        assert!(bench.scene_network("network-error", true));
        assert!(bench.network_open);
        let report = bench.snapshot.network_scan.as_ref().unwrap();
        assert_eq!(report.interfaces.len(), 1);
        assert_eq!(report.cameras.len(), 1);
        assert_eq!(report.cameras[0].health, NetworkHealth::Unreachable);
        assert!(bench.network_error.is_some());
        assert!(!bench.pending(Job::NetworkScan));
        let actual = bench.handle.snapshot();
        assert!(actual.network_scan.is_none());
        assert!(actual.cameras.is_empty());
    }

    fn camera() -> CameraSnapshot {
        let mut info = crate::transport::simulator::info();
        info.transport = Transport::GigE;
        serde_json::from_value(serde_json::json!({
            "info":info,"features":[],"streaming":true,"frames":0,"dropped":0,"fps":0.0,"last_error":null,"worker_pid":0,"transport":TransportStats::default()
        })).unwrap()
    }

    fn report() -> crate::network::NetworkScan {
        use crate::network::{NetworkInterface, NetworkScan, NetworkTuning};
        let mut info = camera().info;
        info.id = "gige:unreachable".into();
        info.model = "Laboratory camera".into();
        info.address = Some("192.168.10.50".into());
        NetworkScan {
            interfaces: vec![NetworkInterface {
                name: "Ethernet adapter".into(), address: "192.168.10.1".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(), mtu: Some(1500), speed_mbps: Some(1000),
                kind: InterfaceKind::Wired, up: Some(true),
            }],
            cameras: vec![NetworkCamera {
                camera: info, active: false, interface: Some("Ethernet adapter".into()),
                host_address: Some("192.168.10.1".parse().unwrap()), reachable: false,
                health: NetworkHealth::Unreachable,
                summary: "Discovery found this camera, but it did not answer control requests. Check its subnet and Ethernet cable.".into(),
                details: vec!["A discovery reply does not verify that an image stream can reach this computer.".into()],
                tuning: Some(NetworkTuning { packet_size: Some(1500), throughput_limit_bps: Some(100_000_000), changes: vec!["Packet size kept within adapter MTU.".into()], warnings: vec!["Could not apply camera settings because the camera is unreachable.".into()] }),
            }],
            warnings: vec!["One discovered camera is unreachable.".into()], scanned_at_ms: 1_792_012_000_000,
        }
    }

    fn card_children<'a>(
        snapshot: &'a accesskit::TreeUpdate,
        name: &str,
    ) -> Vec<&'a accessibility::Node> {
        let group = snapshot
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == accessibility::Role::Group && node.label() == Some(name)
            })
            .expect("named network card group");
        assert!(
            !group.1.children().is_empty(),
            "a card owns its visible details"
        );
        group
            .1
            .children()
            .iter()
            .map(|id| {
                &snapshot
                    .nodes
                    .iter()
                    .find(|(node_id, _)| node_id == id)
                    .unwrap()
                    .1
            })
            .collect()
    }

    #[test]
    fn zero_loss_without_received_packets_never_says_stable() {
        let mut camera = camera();
        assert_eq!(live_status(&camera, false).label, "Awaiting traffic");
        camera.frames = 10;
        assert_eq!(live_status(&camera, false).label, "Awaiting traffic");
        camera.transport.as_mut().unwrap().packets_received = 100;
        assert_eq!(live_status(&camera, false).label, "No loss observed");
        camera.stale = true;
        assert_eq!(live_status(&camera, false).label, "Awaiting live checks");
    }

    #[test]
    fn retries_loss_stalls_and_idle_are_visible_without_overclaiming() {
        let mut camera = camera();
        camera.frames = 10;
        let stats = camera.transport.as_mut().unwrap();
        stats.packets_received = 100;
        stats.resend_requested = 3;
        stats.resend_recovered = 3;
        assert_eq!(live_status(&camera, false).label, "Packet retries observed");
        assert!(live_status(&camera, false).detail.contains("cumulative"));
        camera.transport.as_mut().unwrap().incomplete_frames = 1;
        assert_eq!(live_status(&camera, false).level, Level::Error);
        assert_eq!(live_status(&camera, true).label, "No frames arriving");
        camera.streaming = false;
        assert_eq!(live_status(&camera, false).label, "Stability unverified");
    }

    #[test]
    fn scan_failure_stays_in_report_and_escape_closes_it() {
        let mut bench = Workbench::new(SessionHandle::new(), "network-test".into(), true, None);
        let _ = bench.show_network(true);
        assert!(bench.sheet_open());
        let pending = Pending::unanswered(Job::NetworkScan);
        bench.settle(&pending, &Err(anyhow::anyhow!("adapter scan failed")));
        assert_eq!(bench.network_error.as_deref(), Some("adapter scan failed"));
        let _ = bench.dispatch_action(Action::Overview);
        assert!(!bench.network_open);
        assert!(bench.network_error.is_some());
    }

    #[test]
    fn discovery_reports_unchecked_then_unreachable_without_claiming_live_health() {
        let mut bench = Workbench::new(SessionHandle::new(), "network-test".into(), true, None);
        let report = report();
        let info = report.cameras[0].camera.clone();
        assert_eq!(
            bench.network_status(&info).unwrap().label,
            "Network not checked"
        );
        bench.snapshot.network_scan = Some(report);
        assert_eq!(bench.network_status(&info).unwrap().label, "Unreachable");
        let mut connected = camera();
        connected.info = info.clone();
        connected.frames = 10;
        connected.transport.as_mut().unwrap().packets_received = 100;
        bench.snapshot.cameras.push(connected);
        assert_eq!(
            bench.network_status(&info).unwrap().label,
            "No loss observed",
            "an old scan cannot override delivered traffic"
        );
    }

    #[test]
    fn network_report_keeps_scan_and_close_reachable_with_native_camera_status() {
        use super::super::destinations::layout_tests::{render_artifact, renderer};
        use iced::advanced::{Layout, layout, widget::Tree};
        let mut renderer = renderer();
        let mut bench = Workbench::new(SessionHandle::new(), "network-layout".into(), true, None);
        bench.snapshot.network_scan = Some(report());
        for size in [
            Size::new(680.0, 620.0),
            Size::new(540.0, 430.0),
            Size::new(480.0, 320.0),
        ] {
            let mut view = bench.network_sheet(Palette::of(false), size.height);
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            for label in ["Close Camera network", "Scan network"] {
                let control = snapshot
                    .nodes
                    .iter()
                    .find(|(_, control)| control.label() == Some(label))
                    .expect("named report action")
                    .1
                    .clone();
                let bounds = control.bounds().unwrap();
                assert!(
                    bounds.x0 >= 0.0
                        && bounds.y0 >= 0.0
                        && bounds.x1 <= f64::from(size.width) + 0.5
                        && bounds.y1 <= f64::from(size.height) + 0.5,
                    "{label} is clipped at {size:?}: {bounds:?}"
                );
                let mut messages = Vec::new();
                let cursor = iced::mouse::Cursor::Available(iced::Point::new(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                ));
                for event in [
                    iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                    iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
                ] {
                    view.as_widget_mut().update(
                        &mut tree,
                        &iced::Event::Mouse(event),
                        Layout::new(&node),
                        cursor,
                        &renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut iced::advanced::Shell::new(&mut messages),
                        &iced::Rectangle::new(iced::Point::ORIGIN, size),
                    );
                }
                assert!(
                    messages.iter().any(|message| matches!(
                        (label, message),
                        ("Close Camera network", Message::NetworkReport(false))
                            | ("Scan network", Message::ScanNetwork)
                    )),
                    "{label} is pointer reachable at {size:?}"
                );
            }
            let camera_name = snapshot
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == accessibility::Role::Group
                        && node
                            .label()
                            .is_some_and(|name| name.contains("Laboratory camera"))
                })
                .unwrap()
                .1
                .label()
                .unwrap();
            assert!(
                card_children(&snapshot, camera_name)
                    .iter()
                    .any(|node| { node.label() == Some("At scan: Unreachable") }),
                "unreachable status belongs to its camera card"
            );
            render_artifact(
                &format!("network-report-{}x{}", size.width, size.height),
                &view,
                &tree,
                &node,
                &mut renderer,
                size,
            );
        }
        let result = serde_json::json!({"network_scan": bench.snapshot.network_scan});
        assert_eq!(
            command_notice(&Job::NetworkScan, &Ok(result)).unwrap().2,
            Level::Warning
        );
    }

    #[test]
    fn pending_scan_announces_progress_and_prevents_duplicate_requests() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut bench = Workbench::new(SessionHandle::new(), "network-progress".into(), true, None);
        bench.pending.push(Pending::unanswered(Job::NetworkScan));
        let _ = bench.scan_network();
        assert!(bench.network_open);
        assert_eq!(
            bench.pending.len(),
            1,
            "a pending scan cannot be duplicated"
        );
        let mut view = bench.network_sheet(Palette::of(false), 430.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(540.0, 430.0)),
        );
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        let control = |label| {
            snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap()
                .1
                .clone()
        };
        assert!(control("Scan network").is_disabled());
        assert!(!control("Close Camera network").is_disabled());
        assert_eq!(
            control("Checking connections and applying defaults").role(),
            accessibility::Role::Status
        );
    }

    #[test]
    fn native_network_cards_own_each_visible_detail_once_under_concise_identity_names() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut bench = Workbench::new(SessionHandle::new(), "network-groups".into(), true, None);
        let report = report();
        let summary = report.cameras[0].summary.clone();
        let camera_name = format!(
            "{} Laboratory camera (192.168.10.50)",
            report.cameras[0].camera.vendor
        );
        let warning = report.cameras[0].tuning.as_ref().unwrap().warnings[0].clone();
        bench.snapshot.network_scan = Some(report);
        let mut view = bench.network_sheet(Palette::of(false), 430.0);
        let mut tree = Tree::new(view.as_widget());
        let layout = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(540.0, 430.0)),
        );
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&layout),
            &renderer,
        );
        let adapter_children = card_children(&snapshot, "Ethernet adapter (192.168.10.1)");
        assert!(
            adapter_children
                .iter()
                .any(|node| node.label() == Some("Ethernet adapter"))
        );
        assert!(
            adapter_children
                .iter()
                .any(|node| node.label().is_some_and(|label| {
                    label.contains("Wired Ethernet · 1000 Mbps · MTU 1500")
                        && label.contains("192.168.10.1 · subnet mask 255.255.255.0")
                }))
        );
        let camera_children = card_children(&snapshot, &camera_name);
        for detail in [
            "At scan: Unreachable",
            summary.as_str(),
            "Camera 192.168.10.50 · Ethernet adapter",
            "Host address 192.168.10.1",
            "Packet size 1500 bytes · bandwidth budget 100.0 MB/s",
            "Packet size kept within adapter MTU.",
            warning.as_str(),
        ] {
            let (_, node) = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(detail))
                .unwrap_or_else(|| panic!("missing readable detail {detail}"));
            assert_eq!(
                snapshot
                    .nodes
                    .iter()
                    .filter(|(_, node)| node.label() == Some(detail))
                    .count(),
                1,
                "visible details are exposed once"
            );
            assert!(
                camera_children.contains(&node),
                "{detail} must remain inside its camera's group"
            );
            let id = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(detail))
                .unwrap()
                .0;
            assert_eq!(
                snapshot
                    .nodes
                    .iter()
                    .filter(|(_, parent)| parent.children().contains(&id))
                    .count(),
                1,
                "the detail has exactly one semantic parent"
            );
        }
        for (_, group) in snapshot
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accessibility::Role::Group)
        {
            assert!(!group.label().unwrap().contains(&summary));
            assert!(!group.supports_action(accesskit::Action::Focus));
            assert!(!group.supports_action(accesskit::Action::Click));
        }
    }

    #[test]
    fn native_network_indicator_retains_its_short_action_name_as_counters_change() {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::futures::StreamExt;
        let renderer = super::super::destinations::layout_tests::renderer();
        let bench = Workbench::new(SessionHandle::new(), "network-indicator".into(), true, None);
        let mut camera = camera();
        camera.frames = 10;
        let mut tree = Tree::empty();
        let mut previous = None;
        for packets in [100, 150] {
            camera.transport.as_mut().unwrap().packets_received = packets;
            let status = live_status(&camera, false);
            let expected = status.detail.clone();
            let mut view = bench.network_indicator(status, Palette::of(false));
            tree.diff(view.as_widget());
            let layout = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(420.0, 50.0)),
            );
            let (snapshot, bridge, window) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&layout),
                &renderer,
            );
            let (id, control) = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.role() == accessibility::Role::Button)
                .unwrap();
            assert_eq!(
                control.label(),
                Some("GigE connection: No loss observed. Open network report")
            );
            assert_eq!(control.description(), Some(expected.as_str()));
            assert!(
                control
                    .description()
                    .unwrap()
                    .contains(&format!("{packets} packets received"))
            );
            assert!(!control.label().unwrap().contains("packets"));
            assert!(control.supports_action(accesskit::Action::Click));
            assert!(control.live().is_none());
            if let Some(previous) = previous {
                assert_eq!(*id, previous);
            }
            previous = Some(*id);
            let task = bridge.dispatch(accessibility::Request {
                window,
                replay: None,
                action: accesskit::ActionRequest {
                    action: accesskit::Action::Click,
                    target: *id,
                    data: None,
                },
            });
            let mut actions = iced_runtime::task::into_stream(task).unwrap();
            assert!(
                matches!(
                    iced::futures::executor::block_on(actions.next()),
                    Some(iced_runtime::Action::Output(Message::NetworkReport(true)))
                ),
                "the concise native control still opens the report"
            );
            assert!(iced::futures::executor::block_on(actions.next()).is_none());
        }
    }
}
