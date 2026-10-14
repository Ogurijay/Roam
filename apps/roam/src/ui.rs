//! 布局页面：只监听 127.0.0.1，浏览器打开后拖拽排列屏幕。

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
};
use roam_core::config::Point;
use roam_proto::{DisplayInfo, Os};
use serde::{Deserialize, Serialize};

use crate::{discovery::Discovered, host::Host};

const INDEX_HTML: &str = include_str!("../web/index.html");

pub async fn serve(host: Arc<Host>, port: u16) -> Result<SocketAddr> {
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/api/state", get(state))
        .route("/api/layout", put(set_layout))
        .route("/api/peers", post(add_peer))
        .route("/api/peers/{id}", delete(remove_peer))
        .route("/api/peers/{id}/retry", post(retry_peer))
        .route("/api/pause", put(set_pause))
        .route("/api/settings", put(set_settings))
        .route("/api/home", post(go_home))
        .with_state(host);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!("布局页面服务退出: {err:#}");
        }
    });
    Ok(addr)
}

#[derive(Serialize)]
struct DeviceView {
    id: String,
    name: String,
    os: Option<Os>,
    is_host: bool,
    online: bool,
    path: Option<&'static str>,
    rtt_ms: Option<f64>,
    error: Option<String>,
    pending_code: Option<String>,
    denied: bool,
    displays: Vec<DisplayInfo>,
    pos: Option<Point>,
}

#[derive(Serialize)]
struct StateView {
    host_id: String,
    capture_ok: bool,
    capture_message: Option<String>,
    focused: Option<String>,
    swap_cmd_ctrl: bool,
    paused: bool,
    devices: Vec<DeviceView>,
    /// 局域网里发现、但还没添加的设备。
    nearby: Vec<Discovered>,
}

async fn state(State(host): State<Arc<Host>>) -> Json<StateView> {
    let config = host.config.lock().unwrap().clone();
    let status = host.status.lock().unwrap().clone();
    let capture = host.capture.lock().unwrap().clone();
    let focused = host.router.lock().unwrap().focused().map(str::to_string);

    let mut devices = vec![DeviceView {
        id: host.host_id.clone(),
        name: config.name.clone(),
        os: Some(Os::current()),
        is_host: true,
        online: true,
        path: None,
        rtt_ms: None,
        error: None,
        pending_code: None,
        denied: false,
        displays: host.displays.clone(),
        pos: config.layout.get(&host.host_id).copied(),
    }];
    devices.extend(config.peers.iter().map(|p| {
        let s = status.get(&p.id).cloned().unwrap_or_default();
        DeviceView {
            id: p.id.clone(),
            name: p.name.clone(),
            os: p.os,
            is_host: false,
            online: s.online,
            path: s.path.map(|k| k.label()),
            rtt_ms: s.rtt_ms,
            error: s.error,
            pending_code: s.pending_code,
            denied: s.denied,
            displays: p.displays.clone(),
            pos: config.layout.get(&p.id).copied(),
        }
    }));

    Json(StateView {
        host_id: host.host_id.clone(),
        capture_ok: capture.is_ok(),
        capture_message: capture.err(),
        focused,
        swap_cmd_ctrl: config.swap_cmd_ctrl,
        paused: *host.paused.lock().unwrap(),
        nearby: host
            .nearby
            .list()
            .into_iter()
            .filter(|d| d.id != host.host_id && !config.peers.iter().any(|p| p.id == d.id))
            .collect(),
        devices,
    })
}

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[derive(Deserialize)]
struct LayoutBody {
    positions: BTreeMap<String, Point>,
}

async fn set_layout(
    State(host): State<Arc<Host>>,
    Json(body): Json<LayoutBody>,
) -> Result<StatusCode, ApiError> {
    {
        let mut config = host.config.lock().unwrap();
        for (id, pos) in body.positions {
            config.layout.insert(id, pos);
        }
    }
    host.save_config()?;
    host.rebuild_world();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AddPeerBody {
    ticket: String,
    name: Option<String>,
}

async fn add_peer(
    State(host): State<Arc<Host>>,
    Json(body): Json<AddPeerBody>,
) -> Result<Json<String>, ApiError> {
    Ok(Json(host.add_peer(&body.ticket, body.name)?))
}

async fn remove_peer(
    State(host): State<Arc<Host>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    host.remove_peer(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SettingsBody {
    swap_cmd_ctrl: bool,
}

async fn set_settings(
    State(host): State<Arc<Host>>,
    Json(body): Json<SettingsBody>,
) -> Result<StatusCode, ApiError> {
    host.config.lock().unwrap().swap_cmd_ctrl = body.swap_cmd_ctrl;
    host.save_config()?;
    host.router
        .lock()
        .unwrap()
        .set_swap_cmd_ctrl(body.swap_cmd_ctrl);
    Ok(StatusCode::NO_CONTENT)
}

async fn go_home(State(host): State<Arc<Host>>) -> StatusCode {
    host.router.lock().unwrap().return_home();
    StatusCode::NO_CONTENT
}

async fn retry_peer(
    State(host): State<Arc<Host>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    host.retry_peer(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PauseBody {
    paused: bool,
}

async fn set_pause(State(host): State<Arc<Host>>, Json(body): Json<PauseBody>) -> StatusCode {
    host.set_paused(body.paused);
    StatusCode::NO_CONTENT
}
