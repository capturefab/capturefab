use super::*;
use crate::hubs::{
    client::tests::{hub, secret},
    fake_http,
    fake_hub::Protect,
    image::tests::jpeg_of,
    pump::{HubBackend, Meta},
};
use crate::types::Backend;

const KEY: &str = "CANARY-protect-api-key";

#[test]
fn protect_lists_cameras_and_serves_snapshots_over_pinned_tls() {
    let fake = Protect::new(KEY, vec![jpeg_of(32, 18, 99)], fake_http::localhost()).start();
    let hub = hub(Kind::UnifiProtect, &fake.url(), true);
    let key = secret(Kind::UnifiProtect, KEY);
    let check = check(&hub, Some(&key), Duration::ZERO).unwrap();
    assert_eq!(
        (check.product, check.version.as_deref(), check.cameras),
        ("UniFi Protect", Some("7.3.70"), 2)
    );
    let listed = cameras(&hub, Some(&key), Duration::ZERO).unwrap();
    assert_eq!(listed[0].name, "Doorbell");
    assert!(listed[0].online && !listed[1].online);
    let selector = Selector::parse("hub:test/66D025B301EBC903E80003EA").unwrap();
    let mut selector = selector;
    selector.camera = Kind::UnifiProtect
        .validate_camera(&selector.camera, "test")
        .unwrap();
    let plan = plan(&hub, Some(secret(Kind::UnifiProtect, KEY)), &selector).unwrap();
    assert_eq!(plan.name, "Doorbell");
    let mut backend = HubBackend::connect(
        plan.sources,
        Duration::from_secs(5),
        Meta {
            id: selector.id(),
            product: "UniFi Protect",
            latch: None,
        },
    )
    .unwrap();
    backend.start(0).unwrap();
    assert_eq!(
        backend.next_frame(Duration::from_secs(5)).unwrap().width,
        32
    );
    let targets = fake
        .requests()
        .iter()
        .map(|r| r.target.clone())
        .collect::<Vec<_>>();
    assert!(
        targets
            .iter()
            .any(|t| t.ends_with("/cameras/66d025b301ebc903e80003ea/snapshot?highQuality=true"))
    );
    assert!(targets.iter().all(|t| !t.contains("rtsps")));
    assert!(
        fake.requests()
            .iter()
            .all(|r| r.header("x-api-key") == Some(KEY))
    );
}

#[test]
fn offline_cameras_and_refused_keys() {
    let fake = Protect::new(KEY, vec![jpeg_of(32, 18, 99)], fake_http::localhost()).start();
    let hub = hub(Kind::UnifiProtect, &fake.url(), true);
    let selector = Selector::parse("hub:test/66d025b301ebc903e80003eb").unwrap();
    let plan = plan(&hub, Some(secret(Kind::UnifiProtect, KEY)), &selector).unwrap();
    let error = HubBackend::connect(
        plan.sources,
        Duration::from_secs(5),
        Meta {
            id: selector.id(),
            product: "UniFi Protect",
            latch: None,
        },
    )
    .err()
    .unwrap();
    assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
    let failure = check(
        &hub,
        Some(&secret(Kind::UnifiProtect, "CANARY-wrong-key")),
        Duration::ZERO,
    )
    .err()
    .unwrap();
    assert_eq!(failure.auth_status, Some(401));
    assert!(
        failure.message.contains("rejected the API key"),
        "{}",
        failure.message
    );
}
