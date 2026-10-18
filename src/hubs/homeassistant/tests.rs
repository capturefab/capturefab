use super::*;
use crate::hubs::{
    client::tests::{hub, secret},
    fake_hub::{Camera, HomeAssistant},
    image::tests::jpeg_of,
    pump::{HubBackend, Meta},
};
use crate::types::Backend;
use std::time::Instant;

const TOKEN: &str = "CANARY-long-lived-token";

fn frames() -> Vec<Vec<u8>> {
    (0..4).map(|value| jpeg_of(64, 36, value * 60)).collect()
}

fn meta() -> Meta {
    Meta {
        id: "hub:test/camera.front_door".into(),
        product: "Home Assistant",
        latch: None,
    }
}

fn connect(fake: &crate::hubs::fake_http::FakeHttp, selector: &str) -> anyhow::Result<HubBackend> {
    let hub = hub(Kind::HomeAssistant, &fake.url(), true);
    let plan = plan(
        &hub,
        Some(secret(Kind::HomeAssistant, TOKEN)),
        &Selector::parse(selector).unwrap(),
    )
    .map_err(Failure::into_error)?;
    HubBackend::connect(plan.sources, Duration::from_secs(5), meta())
}

fn next(backend: &mut HubBackend) -> crate::types::Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match backend.next_frame(Duration::from_millis(100)) {
            Ok(frame) => return frame,
            Err(error) if Instant::now() < deadline && error.is::<crate::types::FrameWait>() => {}
            Err(error) => panic!("{error:#}"),
        }
    }
}

#[test]
fn checks_and_lists_cameras_without_keeping_their_tokens() {
    let fake = HomeAssistant::new(TOKEN, frames()).start();
    let hub = hub(Kind::HomeAssistant, &fake.url(), true);
    let token = secret(Kind::HomeAssistant, TOKEN);
    let check = check(&hub, Some(&token), Duration::ZERO).unwrap();
    assert_eq!(check.product, "Home Assistant");
    assert_eq!(check.version.as_deref(), Some("2026.10.2"));
    assert_eq!(check.name.as_deref(), Some("Test Home"));
    assert_eq!(
        check.cameras, 2,
        "lights and malformed entity IDs are not cameras"
    );
    let listed = cameras(&hub, Some(&token), Duration::ZERO).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|c| (c.camera.as_str(), c.name.as_str()))
            .collect::<Vec<_>>(),
        [
            ("camera.front_door", "Front door"),
            ("camera.garage", "Garage")
        ]
    );
    assert!(format!("{listed:?}").find("CANARY").is_none());
    for request in fake.requests() {
        assert_eq!(request.bearer(), Some(TOKEN), "{}", request.target);
        assert!(!request.target.contains("token="), "{}", request.target);
    }
}

#[test]
fn a_refused_token_is_reported_once_and_never_retried() {
    let fake = HomeAssistant::new(TOKEN, frames()).start();
    let hub = hub(Kind::HomeAssistant, &fake.url(), true);
    let failure = check(
        &hub,
        Some(&secret(Kind::HomeAssistant, "CANARY-wrong-token")),
        Duration::ZERO,
    )
    .expect_err("refused");
    assert_eq!(failure.auth_status, Some(401));
    assert!(failure.permanent && !failure.fallback);
    assert!(
        failure
            .message
            .contains("rejected the access token (HTTP 401)")
    );
    assert_eq!(fake.requests().len(), 1);
    let error = connect_with(&fake, "CANARY-wrong-token", "hub:test/camera.front_door")
        .err()
        .unwrap();
    assert!(format!("{error}").contains("HTTP 401"));
    assert_eq!(
        fake.requests().len(),
        2,
        "connecting sent one request and stopped"
    );
}

fn connect_with(
    fake: &crate::hubs::fake_http::FakeHttp,
    token: &str,
    selector: &str,
) -> anyhow::Result<HubBackend> {
    let hub = hub(Kind::HomeAssistant, &fake.url(), true);
    let plan = plan(
        &hub,
        Some(secret(Kind::HomeAssistant, token)),
        &Selector::parse(selector).unwrap(),
    )
    .map_err(Failure::into_error)?;
    HubBackend::connect(plan.sources, Duration::from_secs(5), meta())
}

#[test]
fn mjpeg_streams_drop_the_repeated_first_frame() {
    let fake = HomeAssistant::new(TOKEN, frames()).start();
    let mut backend = connect(&fake, "hub:test/camera.front_door").unwrap();
    backend.start(0).unwrap();
    let got = (0..3).map(|_| next(&mut backend)).collect::<Vec<_>>();
    assert!(
        got.iter()
            .all(|frame| (frame.width, frame.height) == (64, 36))
    );
    // Distinct images: the duplicate first part was skipped.
    assert_ne!(got[0].data[0], got[1].data[0]);
    assert_ne!(got[1].data[0], got[2].data[0]);
    let notes = backend.stats().unwrap().notes.join("; ");
    assert!(notes.contains("mode: mjpeg"), "{notes}");
    assert!(
        fake.requests()
            .iter()
            .any(|r| r.path() == "/api/camera_proxy_stream/camera.front_door")
    );
}

#[test]
fn snapshots_poll_the_camera_proxy() {
    let fake = HomeAssistant::new(TOKEN, frames()).start();
    let mut backend = connect(&fake, "hub:test/camera.garage?stream=snapshot&fps=2").unwrap();
    backend.start(0).unwrap();
    next(&mut backend);
    let start = Instant::now();
    next(&mut backend);
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "two frames per second"
    );
    let proxies = fake
        .requests()
        .iter()
        .filter(|r| r.path() == "/api/camera_proxy/camera.garage")
        .count();
    assert!(proxies >= 2);
}

#[test]
fn missing_and_offline_cameras_have_their_own_errors() {
    let mut config = HomeAssistant::new(TOKEN, frames());
    config
        .cameras
        .push(Camera::new("camera.porch", "Porch").status(503));
    let fake = config.start();
    let error = connect(&fake, "hub:test/camera.attic").err().unwrap();
    assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
    assert!(
        format!("{error}").contains("camera camera.attic not found on hub test"),
        "{error}"
    );
    let error = connect(&fake, "hub:test/camera.porch?stream=snapshot")
        .err()
        .unwrap();
    assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
    assert!(
        format!("{error}").contains("is offline (HTTP 503)"),
        "{error}"
    );
}
