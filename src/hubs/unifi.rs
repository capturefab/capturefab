//! UniFi Protect snapshots through the official integration API, with an
//! API key in `X-API-KEY`. Consoles use self-signed certificates, so the
//! certificate is usually pinned when the hub is added. The RTSPS stream
//! routes are never called: creating one changes the console's settings and
//! hands out a capability URL that FFmpeg would have to carry in its
//! arguments.
use super::{
    Hub, Kind, Selector, Stream,
    client::{self, Check, Listed, Plan, Route},
    http::{self, Auth, Endpoint, Request, Response},
    pump::{Cx, Failure, SourceFactory},
    secret::Secret,
    sources::{Access, MediaRequest, SnapshotSource},
};
use crate::types::ErrorKind;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

const BASE: &str = "/proxy/protect/integration/v1";

fn refused(hub: &Hub) -> impl FnOnce(u16) -> Failure + '_ {
    move |status| {
        Failure::rejected(
            status,
            format!(
                "{} rejected the API key (HTTP {status}); create an API key under Settings > Control Plane > Integrations and add the hub again",
                http::label(hub)
            ),
        )
    }
}

struct Protect {
    hub: Hub,
    endpoint: Endpoint,
    key: Secret,
}

impl Protect {
    fn new(hub: &Hub, key: Option<Secret>) -> Result<Self, Failure> {
        Ok(Self {
            endpoint: Endpoint::for_hub(hub).map_err(|error| {
                Failure::permanent(ErrorKind::OperationFailed, format!("{error:#}"))
            })?,
            hub: hub.clone(),
            key: key.ok_or_else(|| {
                Failure::permanent(
                    ErrorKind::OperationFailed,
                    "UniFi Protect hubs need an API key",
                )
            })?,
        })
    }
    fn send(&self, request: &Request, cx: Option<&Cx>, route: Route) -> Result<Response, Failure> {
        client::send_ok(
            &self.hub,
            &self.endpoint,
            request,
            &Auth::ApiKey(&self.key),
            cx,
            route,
            refused(&self.hub),
        )
    }
    fn api<T: serde::de::DeserializeOwned>(
        &self,
        route: Route,
        path: &str,
        floor: Duration,
    ) -> Result<T, Failure> {
        let what = match route {
            Route::Api(what) | Route::Camera(_, what) => what,
        };
        let response = self.send(
            &Request::get(
                what,
                format!("{BASE}{path}"),
                "application/json",
                client::limits(10, 10, 30, 16 << 20, floor),
            ),
            None,
            route,
        )?;
        client::json(&self.hub, response, 16 << 20, what)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Info {
    #[serde(default)]
    application_version: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Flags {
    #[serde(default)]
    support_full_hd_snapshot: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Camera {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    feature_flags: Flags,
}

fn listed(camera: Camera) -> Option<Listed> {
    let id = Kind::UnifiProtect.validate_camera(&camera.id, "").ok()?;
    Some(Listed {
        name: camera
            .name
            .as_deref()
            .map(|name| client::display(name, 128))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.clone()),
        online: camera.state.as_deref() == Some("CONNECTED"),
        state: camera.state.map(|state| client::display(&state, 32)),
        camera: id,
        streams: vec![Stream::Snapshot],
    })
}

pub(crate) fn check(hub: &Hub, secret: Option<&Secret>, floor: Duration) -> Result<Check, Failure> {
    let protect = Protect::new(hub, secret.map(Secret::duplicate))?;
    let info: Info = protect.api(
        Route::Api("the application information"),
        "/meta/info",
        floor,
    )?;
    let cameras: Vec<Camera> = protect.api(Route::Api("the camera list"), "/cameras", floor)?;
    Ok(Check {
        product: "UniFi Protect",
        version: info
            .application_version
            .map(|version| client::display(&version, 64)),
        name: None,
        cameras: cameras.into_iter().filter_map(listed).count(),
        warnings: Vec::new(),
    })
}

pub(crate) fn cameras(
    hub: &Hub,
    secret: Option<&Secret>,
    floor: Duration,
) -> Result<Vec<Listed>, Failure> {
    let protect = Protect::new(hub, secret.map(Secret::duplicate))?;
    let cameras: Vec<Camera> = protect.api(Route::Api("the camera list"), "/cameras", floor)?;
    Ok(cameras.into_iter().filter_map(listed).collect())
}

struct ProtectAccess {
    protect: Arc<Protect>,
    camera: String,
}

impl Access for ProtectAccess {
    fn media(&self, request: &MediaRequest, cx: &Cx) -> Result<Response, Failure> {
        self.protect.send(
            &Request::get(
                request.route,
                request.target.clone(),
                request.accept,
                request.limits,
            ),
            Some(cx),
            Route::Camera(&self.camera, request.route),
        )
    }
}

pub(crate) fn plan(
    hub: &Hub,
    secret: Option<Secret>,
    selector: &Selector,
) -> Result<Plan, Failure> {
    let protect = Arc::new(Protect::new(hub, secret)?);
    let camera: Camera = protect.api(
        Route::Camera(&selector.camera, "the camera"),
        &format!("/cameras/{}", super::encode(&selector.camera)),
        Duration::ZERO,
    )?;
    let quality = if camera.feature_flags.support_full_hd_snapshot {
        "?highQuality=true"
    } else {
        ""
    };
    let name = camera
        .name
        .as_deref()
        .map(|name| client::display(name, 128))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| selector.camera.clone());
    let access = Arc::new(ProtectAccess {
        protect,
        camera: selector.camera.clone(),
    });
    let source: Arc<dyn SourceFactory> = Arc::new(SnapshotSource {
        access,
        request: client::media(
            "the camera image",
            format!(
                "{BASE}/cameras/{}/snapshot{quality}",
                super::encode(&selector.camera)
            ),
            "image/jpeg",
            client::limits(10, 10, 10, 32 << 20, Duration::ZERO),
        ),
        label: client::camera_label(hub, &selector.camera),
        fps: client::snapshot_fps(Kind::UnifiProtect, selector),
        offline_header: None,
    });
    Ok(Plan {
        sources: vec![source],
        name,
    })
}

#[cfg(test)]
mod tests;
