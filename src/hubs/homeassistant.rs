//! Home Assistant cameras over its REST API with a long-lived access token.
//! Every request carries `Authorization: Bearer`; none is ever sent without
//! one, since each refused request raises a "Login attempt failed"
//! notification in Home Assistant and counts toward an IP ban. Camera access
//! tokens (`?token=`) are never used, and the secret-bearing attributes in
//! `/api/states` are never kept.
use super::{
    Hub, Kind, Selector, Stream,
    client::{self, Check, Listed, Plan, Route},
    http::{self, Auth, Endpoint, Request, Response},
    pump::{Cx, Failure},
    secret::Secret,
    sources::{Access, MediaRequest, MjpegSource, SnapshotSource},
};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

fn refused(hub: &Hub) -> impl FnOnce(u16) -> Failure + '_ {
    move |status| {
        let label = http::label(hub);
        Failure::rejected(
            status,
            if status == 401 {
                format!(
                    "{label} rejected the access token (HTTP 401); create a long-lived access token (Profile > Security) and add the hub again"
                )
            } else {
                format!(
                    "{label} refused access (HTTP 403); after repeated failed logins Home Assistant bans the address until the ban is removed"
                )
            },
        )
    }
}

fn missing_token() -> Failure {
    Failure::permanent(
        crate::types::ErrorKind::OperationFailed,
        "Home Assistant hubs need an access token",
    )
}

fn token(secret: Option<&Secret>) -> Result<&Secret, Failure> {
    secret.ok_or_else(missing_token)
}

fn get(
    hub: &Hub,
    endpoint: &Endpoint,
    secret: &Secret,
    route: Route,
    target: &str,
    limits: http::Limits,
) -> Result<Response, Failure> {
    let what = match route {
        Route::Api(what) | Route::Camera(_, what) => what,
    };
    let request = Request::get(what, target.into(), "application/json", limits);
    client::send_ok(
        hub,
        endpoint,
        &request,
        &Auth::Bearer(secret.expose()),
        None,
        route,
        refused(hub),
    )
}

#[derive(Deserialize)]
struct Running {
    message: String,
}

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    location_name: Option<String>,
}

#[derive(Deserialize)]
struct State {
    entity_id: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    attributes: Attributes,
}

/// Only the name: `access_token` and `entity_picture` hold live camera
/// tokens and are never read.
#[derive(Deserialize, Default)]
struct Attributes {
    #[serde(default)]
    friendly_name: Option<String>,
}

fn endpoint(hub: &Hub) -> Result<Endpoint, Failure> {
    Endpoint::for_hub(hub).map_err(|error| {
        Failure::permanent(
            crate::types::ErrorKind::OperationFailed,
            format!("{error:#}"),
        )
    })
}

pub(crate) fn check(hub: &Hub, secret: Option<&Secret>, floor: Duration) -> Result<Check, Failure> {
    let secret = token(secret)?;
    let endpoint = endpoint(hub)?;
    let quick = client::limits(10, 10, 10, 1 << 20, floor);
    let running: Running = client::json(
        hub,
        get(
            hub,
            &endpoint,
            secret,
            Route::Api("the API status"),
            "/api/",
            quick,
        )?,
        1 << 20,
        "API status",
    )
    .map_err(|_| not_home_assistant(hub))?;
    if running.message != "API running." {
        return Err(not_home_assistant(hub));
    }
    // The configuration also holds the home's location; only the version
    // and name are read.
    let config: Config = client::json(
        hub,
        get(
            hub,
            &endpoint,
            secret,
            Route::Api("the configuration"),
            "/api/config",
            quick,
        )?,
        1 << 20,
        "configuration",
    )?;
    let cameras = list(hub, &endpoint, secret, floor)?;
    Ok(Check {
        product: "Home Assistant",
        version: config.version.map(|v| client::display(&v, 64)),
        name: config.location_name.map(|v| client::display(&v, 128)),
        cameras: cameras.len(),
        warnings: Vec::new(),
    })
}

fn not_home_assistant(hub: &Hub) -> Failure {
    Failure::permanent(
        crate::types::ErrorKind::OperationFailed,
        format!(
            "hub {} at {} did not answer like Home Assistant",
            hub.name,
            hub.parsed_url().map(|url| url.origin()).unwrap_or_default()
        ),
    )
}

fn list(
    hub: &Hub,
    endpoint: &Endpoint,
    secret: &Secret,
    floor: Duration,
) -> Result<Vec<Listed>, Failure> {
    let response = get(
        hub,
        endpoint,
        secret,
        Route::Api("the entity states"),
        "/api/states",
        client::limits(30, 10, 60, 64 << 20, floor),
    )?;
    let states: Vec<State> = client::json(hub, response, 64 << 20, "list of entity states")?;
    let mut cameras = states
        .into_iter()
        .filter(|state| {
            Kind::HomeAssistant
                .validate_camera(&state.entity_id, &hub.name)
                .is_ok()
                && client::public_identifier(&state.entity_id)
        })
        .map(|state| Listed {
            name: state
                .attributes
                .friendly_name
                .map(|name| client::display(&name, 128))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| state.entity_id.clone()),
            online: state.state != "unavailable",
            state: Some(client::display(&state.state, 32)),
            camera: state.entity_id,
            streams: vec![Stream::Mjpeg, Stream::Snapshot],
        })
        .collect::<Vec<_>>();
    cameras.sort_by(|a, b| a.camera.cmp(&b.camera));
    Ok(cameras)
}

pub(crate) fn cameras(
    hub: &Hub,
    secret: Option<&Secret>,
    floor: Duration,
) -> Result<Vec<Listed>, Failure> {
    list(hub, &endpoint(hub)?, token(secret)?, floor)
}

/// Sends a camera's media requests with the hub's token.
struct HaAccess {
    hub: Hub,
    endpoint: Endpoint,
    token: Secret,
    camera: String,
}

impl Access for HaAccess {
    fn media(&self, request: &MediaRequest, cx: &Cx) -> Result<Response, Failure> {
        let http_request = Request::get(
            request.route,
            request.target.clone(),
            request.accept,
            request.limits,
        );
        client::send_ok(
            &self.hub,
            &self.endpoint,
            &http_request,
            &Auth::Bearer(self.token.expose()),
            Some(cx),
            Route::Camera(&self.camera, request.route),
            refused(&self.hub),
        )
    }
}

pub(crate) fn plan(
    hub: &Hub,
    secret: Option<Secret>,
    selector: &Selector,
) -> Result<Plan, Failure> {
    let secret = match secret {
        Some(secret) => secret,
        None => return Err(missing_token()),
    };
    let endpoint = endpoint(hub)?;
    let entity = super::encode(&selector.camera);
    // One state read for the camera's name; it also confirms the entity
    // exists before any stream is opened.
    let response = get(
        hub,
        &endpoint,
        &secret,
        Route::Camera(&selector.camera, "the camera state"),
        &format!("/api/states/{entity}"),
        client::limits(12, 12, 12, 1 << 20, Duration::ZERO),
    )?;
    let state: State = client::json(hub, response, 1 << 20, "camera state")?;
    let name = state
        .attributes
        .friendly_name
        .map(|name| client::display(&name, 128))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| selector.camera.clone());
    let label = client::camera_label(hub, &selector.camera);
    let access = Arc::new(HaAccess {
        hub: hub.clone(),
        endpoint,
        token: secret,
        camera: selector.camera.clone(),
    });
    let sources = client::streams(Kind::HomeAssistant, selector.stream)
        .into_iter()
        .map(|stream| -> Arc<dyn super::pump::SourceFactory> {
            match stream {
                Stream::Snapshot => Arc::new(SnapshotSource {
                    access: access.clone(),
                    request: client::media(
                        "the camera image",
                        format!("/api/camera_proxy/{entity}"),
                        "image/*",
                        client::limits(12, 12, 12, 32 << 20, Duration::ZERO),
                    ),
                    label: label.clone(),
                    fps: client::snapshot_fps(Kind::HomeAssistant, selector),
                    offline_header: None,
                }),
                _ => Arc::new(MjpegSource {
                    access: access.clone(),
                    request: client::media(
                        "the camera stream",
                        format!("/api/camera_proxy_stream/{entity}"),
                        "multipart/x-mixed-replace",
                        http::Limits {
                            headers: Duration::from_secs(12),
                            // Home Assistant sends nothing while the image is
                            // unchanged.
                            idle: Duration::from_secs(120),
                            total: None,
                            max_body: None,
                        },
                    ),
                    label: label.clone(),
                    quiet_idle: true,
                }),
            }
        })
        .collect();
    Ok(Plan { sources, name })
}

#[cfg(test)]
mod tests;
