use super::*;
use crate::hubs::{
    client::tests::{hub, secret},
    fake_hub::Go2rtc as FakeGo2rtc,
    image::tests::jpeg_of,
    pump::{HubBackend, Meta},
};
use crate::types::Backend;

fn frames() -> Vec<Vec<u8>> {
    (0..3).map(|value| jpeg_of(40, 30, value * 80)).collect()
}

#[test]
fn rtsp_is_used_only_where_this_computer_can_reach_it() {
    assert_eq!(rtsp_port(Some(":8554"), "10.0.0.7"), Some(8554));
    assert_eq!(rtsp_port(Some("0.0.0.0:9554"), "nvr.lan"), Some(9554));
    assert_eq!(rtsp_port(Some("[::]:8554"), "nvr.lan"), Some(8554));
    assert_eq!(rtsp_port(Some("127.0.0.1:8554"), "10.0.0.7"), None);
    assert_eq!(rtsp_port(Some("127.0.0.1:8554"), "127.0.0.1"), Some(8554));
    assert_eq!(rtsp_port(Some(""), "10.0.0.7"), None);
    assert_eq!(rtsp_port(None, "10.0.0.7"), None);
    assert_eq!(rtsp_port(Some(":0"), "10.0.0.7"), None);
}

#[test]
fn stream_names_are_listed_without_their_sources() {
    let fake = FakeGo2rtc::new(frames()).start();
    let hub = hub(Kind::Go2rtc, &fake.url(), false);
    let check = check(&hub, None, Duration::ZERO).unwrap();
    assert_eq!(
        (check.product, check.version.as_deref(), check.cameras),
        ("go2rtc", Some("1.9.14"), 2)
    );
    let listed = cameras(&hub, None, Duration::ZERO).unwrap();
    assert_eq!(
        listed.iter().map(|c| c.camera.as_str()).collect::<Vec<_>>(),
        ["garage cam", "porch"]
    );
    assert!(format!("{listed:?}").find("CANARY").is_none());
    assert!(fake.requests().iter().all(|r| r.path() != "/api/config"));
}

#[test]
fn only_listed_names_are_requested_and_encoded() {
    let fake = FakeGo2rtc::new(frames()).start();
    let hub = hub(Kind::Go2rtc, &fake.url(), false);
    let failure = plan(&hub, None, &Selector::parse("hub:test/unknown").unwrap())
        .err()
        .unwrap();
    assert_eq!(failure.kind, ErrorKind::Unavailable);
    assert!(
        fake.requests()
            .iter()
            .all(|r| !r.path().starts_with("/api/frame"))
    );
    let selector = Selector::parse("hub:test/garage%20cam?stream=snapshot").unwrap();
    let plan = plan(&hub, None, &selector).unwrap();
    let mut backend = HubBackend::connect(
        plan.sources,
        Duration::from_secs(5),
        Meta {
            id: selector.id(),
            product: "go2rtc",
            latch: None,
        },
    )
    .unwrap();
    backend.start(0).unwrap();
    assert_eq!(
        backend.next_frame(Duration::from_secs(5)).unwrap().width,
        40
    );
    assert!(
        fake.requests()
            .iter()
            .any(|r| r.target == "/api/frame.jpeg?src=garage%20cam")
    );
}

#[test]
fn empty_frames_and_missing_passwords_are_explained() {
    let mut config = FakeGo2rtc::new(frames());
    config.empty_frames = true;
    let fake = config.start();
    let hub = hub(Kind::Go2rtc, &fake.url(), false);
    let selector = Selector::parse("hub:test/porch?stream=snapshot").unwrap();
    let plan = plan(&hub, None, &selector).unwrap();
    let error = HubBackend::connect(
        plan.sources,
        Duration::from_secs(5),
        Meta {
            id: selector.id(),
            product: "go2rtc",
            latch: None,
        },
    )
    .err()
    .unwrap();
    assert!(
        format!("{error}").contains("returned an empty image"),
        "{error}"
    );
    let mut config = FakeGo2rtc::new(frames());
    config.basic = Some(("admin".into(), "CANARY-basic-password".into()));
    let fake = config.start();
    let anonymous = super::super::client::tests::hub(Kind::Go2rtc, &fake.url(), false);
    let failure = check(&anonymous, None, Duration::ZERO).err().unwrap();
    assert!(
        failure
            .message
            .contains("requires a user name and password"),
        "{}",
        failure.message
    );
    let mut signed = hub_with_user(&fake.url(), "admin");
    signed.user = Some("admin".into());
    let ok = check(
        &signed,
        Some(&secret(Kind::Go2rtc, "CANARY-basic-password")),
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!(ok.cameras, 2);
    let failure = check(
        &signed,
        Some(&secret(Kind::Go2rtc, "CANARY-wrong")),
        Duration::ZERO,
    )
    .err()
    .unwrap();
    assert_eq!(failure.auth_status, Some(401));
}

fn hub_with_user(url: &str, user: &str) -> Hub {
    let mut hub = hub(Kind::Go2rtc, url, true);
    hub.user = Some(user.into());
    hub
}

#[test]
fn a_server_cannot_echo_the_password_as_a_public_stream_id() {
    let password = "CANARY-go2rtc-reflected-password";
    let mut config = FakeGo2rtc::new(frames());
    config.streams.push(password.into());
    let fake = config.start();
    let hub = hub(Kind::Go2rtc, &fake.url(), true);
    let password = secret(Kind::Go2rtc, password);
    let listed = cameras(&hub, Some(&password), Duration::ZERO).unwrap();
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .all(|camera| !camera.camera.contains("CANARY"))
    );
}
