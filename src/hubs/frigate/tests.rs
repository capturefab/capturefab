use super::*;
use crate::hubs::{
    client::tests::{hub, secret},
    fake_http::refused_port,
    fake_hub::Frigate as FakeFrigate,
    image::tests::jpeg_of,
    pump::{HubBackend, Meta},
};
use crate::types::Backend;
use std::time::Instant;

fn frames() -> Vec<Vec<u8>> {
    (0..3).map(|value| jpeg_of(48, 32, value * 80)).collect()
}

fn camera(streams: &[(&str, &str)]) -> Camera {
    Camera {
        enabled: true,
        friendly_name: None,
        detect: None,
        live: Some(Live {
            streams: Ordered(
                streams
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        }),
    }
}

#[test]
fn restreams_are_chosen_like_frigates_own_integration() {
    let restreams = ["front", "front_sub", "back"];
    assert_eq!(
        restream(
            "front",
            &camera(&[("Sub", "front_sub"), ("Main", "front")]),
            &restreams
        ),
        Some("front")
    );
    assert_eq!(
        restream("front", &camera(&[("Sub", "front_sub")]), &restreams),
        Some("front_sub")
    );
    assert_eq!(
        restream("back", &camera(&[("A", "missing")]), &restreams),
        Some("back")
    );
    assert_eq!(restream("side", &camera(&[]), &restreams), None);
    assert_eq!(
        restream("side", &camera(&[("A", "elsewhere")]), &restreams),
        None
    );
}

#[test]
fn live_stream_order_survives_json_deserialization() {
    // The map's order, rather than its labels' alphabetical order, selects
    // the first stream when the camera name is not itself a restream.
    let config: Config = serde_json::from_str(
        r#"{"cameras":{"porch":{"live":{"streams":{"ZSub":"sub","AMain":"main"}}}},"go2rtc":{"streams":{"sub":[],"main":[]}}}"#,
    )
    .unwrap();
    assert_eq!(
        restream("porch", &config.cameras["porch"], &config.restreams()),
        Some("sub")
    );
}

#[test]
fn session_cookies_are_found_by_shape_not_name() {
    let fake = crate::hubs::fake_http::FakeHttp::start(|_| {
        crate::hubs::fake_http::Reply::new(200)
            .header("Set-Cookie", "other=1; Path=/")
            .header(
                "Set-Cookie",
                "custom_name=eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjQxMDI0NDQ4MDB9.c2ln; HttpOnly",
            )
    });
    let endpoint = Endpoint::for_hub(&hub(Kind::Frigate, &fake.url(), false)).unwrap();
    let response = http::send(
        &endpoint,
        &Request::get(
            "x",
            "/".into(),
            "*/*",
            client::limits(5, 5, 5, 1024, Duration::ZERO),
        ),
        &Auth::None,
        None,
    )
    .unwrap();
    let session = session_cookie(&response).unwrap();
    assert_eq!(session.expires, 4_102_444_800);
    assert!(session.token.starts_with("eyJhbGciOiJIUzI1NiJ9."));
}

#[test]
fn the_open_port_lists_enabled_cameras_with_their_streams() {
    let fake = FakeFrigate::new(frames()).start();
    let hub = hub(Kind::Frigate, &fake.url(), false);
    let check = check(&hub, None, Duration::ZERO).unwrap();
    assert_eq!(
        (check.product, check.version.as_deref(), check.cameras),
        ("Frigate", Some("0.18.2-test"), 2)
    );
    let listed = cameras(&hub, None, Duration::ZERO).unwrap();
    assert_eq!(listed.len(), 2, "disabled cameras are left out");
    let driveway = listed.iter().find(|c| c.camera == "driveway").unwrap();
    assert_eq!(driveway.name, "Driveway");
    assert_eq!(
        driveway.streams,
        [Stream::Rtsp, Stream::Mjpeg, Stream::Snapshot]
    );
    let yard = listed.iter().find(|c| c.camera == "back_yard").unwrap();
    assert_eq!(yard.streams, [Stream::Mjpeg, Stream::Snapshot]);
    assert!(format!("{listed:?}").find("CANARY").is_none());
}

#[test]
fn sessions_sign_in_once_and_again_when_refused() {
    let mut config = FakeFrigate::new(frames());
    config.login = Some(("viewer".into(), "CANARY-frigate-password".into()));
    let logins = config.logins.clone();
    let fake = config.start();
    let hub = hub(Kind::Frigate, &fake.url(), true);
    // The record says https for passwords; the client itself does not care.
    let frigate =
        Frigate::new(&hub, Some(secret(Kind::Frigate, "CANARY-frigate-password"))).unwrap();
    list(&frigate, Duration::ZERO).unwrap();
    assert_eq!(logins.load(std::sync::atomic::Ordering::SeqCst), 1);
    list(&frigate, Duration::ZERO).unwrap();
    assert_eq!(
        logins.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the session is reused"
    );
    // Another sign-in invalidates this session: one refusal, one new login.
    let other = Frigate::new(&hub, Some(secret(Kind::Frigate, "CANARY-frigate-password"))).unwrap();
    list(&other, Duration::ZERO).unwrap();
    list(&frigate, Duration::ZERO).unwrap();
    assert_eq!(logins.load(std::sync::atomic::Ordering::SeqCst), 3);
    for request in fake.requests().iter().filter(|r| r.path() == "/api/login") {
        assert!(request.header("origin").is_none());
    }
    let wrong = Frigate::new(&hub, Some(secret(Kind::Frigate, "CANARY-wrong"))).unwrap();
    let failure = list(&wrong, Duration::ZERO).err().expect("refused");
    assert_eq!(failure.auth_status, Some(401));
    assert!(
        failure
            .message
            .contains("rejected the user name or password"),
        "{}",
        failure.message
    );
}

#[test]
fn snapshots_skip_offline_placeholders_and_rtsp_falls_back() {
    let mut config = FakeFrigate::new(frames());
    config.offline = true;
    let fake = config.start();
    let mut hub = hub(Kind::Frigate, &fake.url(), false);
    hub.rtsp_port = Some(refused_port());
    let selector = Selector::parse("hub:test/driveway").unwrap();
    let plan = plan(&hub, None, &selector).unwrap();
    assert_eq!(plan.name, "Driveway");
    assert_eq!(
        plan.sources.iter().map(|s| s.stream()).collect::<Vec<_>>(),
        [Stream::Rtsp, Stream::Snapshot]
    );
    // RTSP is refused, and every snapshot is Frigate's offline placeholder.
    let start = Instant::now();
    let error = HubBackend::connect(
        plan.sources,
        Duration::from_millis(500),
        Meta {
            id: selector.id(),
            product: "Frigate",
            latch: None,
        },
    )
    .err()
    .expect("only placeholders");
    assert!(
        format!("{error}").contains("timed out waiting for the first frame"),
        "{error}"
    );
    assert!(start.elapsed() < Duration::from_secs(20));
    let error = plan_error(&hub, "hub:test/back_yard?stream=rtsp");
    assert_eq!(crate::cli::error_code(&error), ("unsupported", 5));
    let error = plan_error(&hub, "hub:test/attic");
    assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
}

fn plan_error(hub: &Hub, selector: &str) -> anyhow::Error {
    plan(hub, None, &Selector::parse(selector).unwrap())
        .err()
        .expect("plan fails")
        .into_error()
}

#[test]
fn live_snapshots_and_mjpeg_without_lengths_stream() {
    let fake = FakeFrigate::new(frames()).start();
    let hub = hub(Kind::Frigate, &fake.url(), false);
    for selector in [
        "hub:test/back_yard",
        "hub:test/back_yard?stream=mjpeg&fps=5",
    ] {
        let selector = Selector::parse(selector).unwrap();
        let plan = plan(&hub, None, &selector).unwrap();
        let mut backend = HubBackend::connect(
            plan.sources,
            Duration::from_secs(5),
            Meta {
                id: selector.id(),
                product: "Frigate",
                latch: None,
            },
        )
        .unwrap();
        backend.start(0).unwrap();
        let frame = backend.next_frame(Duration::from_secs(5)).unwrap();
        assert_eq!((frame.width, frame.height), (48, 32));
    }
    assert!(
        fake.requests()
            .iter()
            .any(|r| r.target == "/api/back_yard?fps=5&height=360")
    );
}

#[test]
fn profiles_restrict_listing_and_explicit_rtsp_connections() {
    let mut config = FakeFrigate::new(frames());
    config.allowed_cameras = Some(vec!["back_yard".into()]);
    let fake = config.start();
    let hub = hub(Kind::Frigate, &fake.url(), false);
    let listed = cameras(&hub, None, Duration::ZERO).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|camera| camera.camera.as_str())
            .collect::<Vec<_>>(),
        ["back_yard"]
    );
    // /api/config exposes all cameras, and the unauthenticated RTSP server
    // cannot enforce the HTTP user's role. Never open a disallowed stream.
    let failure = plan(
        &hub,
        None,
        &Selector::parse("hub:test/driveway?stream=rtsp").unwrap(),
    )
    .err()
    .expect("the profile excludes this camera");
    assert_eq!(failure.kind, ErrorKind::Unavailable);
}

#[test]
fn profile_failures_propagate_but_old_servers_can_omit_the_route() {
    for status in [401, 403, 500] {
        let mut config = FakeFrigate::new(frames());
        config.profile_status = status;
        let fake = config.start();
        let hub = hub(Kind::Frigate, &fake.url(), false);
        let failure = cameras(&hub, None, Duration::ZERO).expect_err("profile failure");
        if status == 500 {
            assert!(!failure.permanent);
        } else {
            assert_eq!(failure.auth_status, Some(status));
            assert!(!failure.fallback);
        }
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|request| request.path() == "/api/profile")
                .count(),
            1
        );
    }
    let mut config = FakeFrigate::new(frames());
    config.profile_status = 404;
    let fake = config.start();
    let hub = hub(Kind::Frigate, &fake.url(), false);
    assert_eq!(cameras(&hub, None, Duration::ZERO).unwrap().len(), 2);
    assert!(
        plan(
            &hub,
            None,
            &Selector::parse("hub:test/driveway?stream=rtsp").unwrap()
        )
        .is_ok()
    );
}

#[test]
fn a_second_refused_profile_stops_after_one_new_login() {
    let mut config = FakeFrigate::new(frames());
    config.login = Some(("viewer".into(), "CANARY-frigate-password".into()));
    config.profile_status = 401;
    let logins = config.logins.clone();
    let fake = config.start();
    let hub = hub(Kind::Frigate, &fake.url(), true);
    let failure = cameras(
        &hub,
        Some(&secret(Kind::Frigate, "CANARY-frigate-password")),
        Duration::ZERO,
    )
    .expect_err("the renewed session was also refused");
    assert_eq!(failure.auth_status, Some(401));
    assert_eq!(logins.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        fake.requests()
            .iter()
            .filter(|request| request.path() == "/api/profile")
            .count(),
        2
    );
}

#[test]
fn url_like_restream_names_never_reach_ffmpeg() {
    let name = "rtsp://admin:CANARY-password@10.0.0.7/video";
    let config: Config = serde_json::from_value(serde_json::json!({
        "cameras": {"front": {"live": {"streams": {"Main": name}}}},
        "go2rtc": {"streams": {name: []}}
    }))
    .unwrap();
    assert!(config.restreams().is_empty());
    assert_eq!(
        restream("front", &config.cameras["front"], &config.restreams()),
        None
    );
}
