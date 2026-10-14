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
use roam_core::{
    config::Point,
    settings::{ClipboardSettings, DeviceSettings, SwitchSettings},
};
use roam_proto::{DisplayInfo, Os};
use serde::{Deserialize, Serialize};

use crate::{
    discovery::Discovered,
    files::{self, TransferInfo},
    host::Host,
    update::UpdateState,
};

const INDEX_HTML: &str = include_str!("../web/index.html");

pub async fn serve(host: Arc<Host>, port: u16) -> Result<SocketAddr> {
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/api/state", get(state))
        .route("/api/layout", put(set_layout))
        .route("/api/peers", post(add_peer))
        .route("/api/peers/{id}", delete(remove_peer))
        .route("/api/peers/{id}/retry", post(retry_peer))
        .route("/api/peers/{id}/settings", put(set_peer_settings))
        .route("/api/lock", put(set_lock))
        .route("/api/update/check", post(check_update))
        .route("/api/update/apply", post(apply_update))
        .route("/api/peers/{id}/sync-version", post(sync_version))
        .route("/api/open-downloads", post(open_downloads))
        .route("/api/pause", put(set_pause))
        .route("/api/settings", put(set_settings))
        .route("/api/home", post(go_home))
        .with_state(host);
    // 更新后重启时旧进程可能还没完全退出，端口被占用就稍等重试
    let mut attempt = 0;
    let listener = loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => break listener,
            Err(err) if attempt < 20 => {
                attempt += 1;
                tracing::debug!("端口 {port} 暂时不可用，重试: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            Err(err) => return Err(err.into()),
        }
    };
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
    settings: Option<DeviceSettings>,
    version: Option<String>,
    displays: Vec<DisplayInfo>,
    pos: Option<Point>,
}

#[derive(Serialize)]
struct StateView {
    host_id: String,
    version: &'static str,
    update: UpdateState,
    update_mirror: Option<String>,
    transfers: Vec<TransferInfo>,
    downloads_dir: String,
    capture_ok: bool,
    capture_message: Option<String>,
    focused: Option<String>,
    swap_cmd_ctrl: bool,
    switching: SwitchSettings,
    clipboard: ClipboardSettings,
    paused: bool,
    edge_locked: bool,
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
        settings: None,
        version: Some(roam_proto::VERSION.into()),
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
            settings: Some(p.settings.clone()),
            version: s.version,
            displays: p.displays.clone(),
            pos: config.layout.get(&p.id).copied(),
        }
    }));

    Json(StateView {
        host_id: host.host_id.clone(),
        version: roam_proto::VERSION,
        update: host.updater.state.lock().unwrap().clone(),
        update_mirror: config.update_mirror.clone(),
        transfers: host.transfers.list(),
        downloads_dir: files::default_dir().display().to_string(),
        capture_ok: capture.is_ok(),
        capture_message: capture.err(),
        focused,
        swap_cmd_ctrl: config.swap_cmd_ctrl,
        switching: config.switching.clone(),
        clipboard: config.clipboard.clone(),
        paused: *host.paused.lock().unwrap(),
        edge_locked: host.router.lock().unwrap().edge_locked(),
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
    swap_cmd_ctrl: Option<bool>,
    switching: Option<SwitchSettings>,
    clipboard: Option<ClipboardSettings>,
    /// 空字符串表示不用镜像。
    update_mirror: Option<String>,
}

async fn set_settings(
    State(host): State<Arc<Host>>,
    Json(body): Json<SettingsBody>,
) -> Result<StatusCode, ApiError> {
    {
        let mut config = host.config.lock().unwrap();
        if let Some(swap) = body.swap_cmd_ctrl {
            config.swap_cmd_ctrl = swap;
        }
        if let Some(switching) = body.switching {
            config.switching = switching.sanitized();
        }
        if let Some(clipboard) = body.clipboard {
            config.clipboard = clipboard.sanitized();
        }
        if let Some(mirror) = body.update_mirror {
            let mirror = mirror.trim().to_string();
            if !mirror.is_empty()
                && !mirror.starts_with("https://")
                && !mirror.starts_with("http://")
            {
                return Err(anyhow::anyhow!("镜像地址需要以 http:// 或 https:// 开头").into());
            }
            config.update_mirror = (!mirror.is_empty()).then_some(mirror);
        }
    }
    host.save_config()?;
    host.rebuild_world();
    host.push_clipboard_policies();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PeerSettingsBody {
    name: Option<String>,
    settings: Option<DeviceSettings>,
}

async fn set_peer_settings(
    State(host): State<Arc<Host>>,
    Path(id): Path<String>,
    Json(body): Json<PeerSettingsBody>,
) -> Result<StatusCode, ApiError> {
    {
        let mut config = host.config.lock().unwrap();
        let peer = config
            .peer_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("没有这台设备"))?;
        if let Some(name) = body
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
        {
            peer.name = name;
        }
        if let Some(settings) = body.settings {
            peer.settings = settings.sanitized();
        }
    }
    host.save_config()?;
    host.rebuild_world();
    host.push_clipboard_policies();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LockBody {
    locked: bool,
}

async fn set_lock(State(host): State<Arc<Host>>, Json(body): Json<LockBody>) -> StatusCode {
    host.router.lock().unwrap().set_edge_locked(body.locked);
    StatusCode::NO_CONTENT
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

async fn check_update(State(host): State<Arc<Host>>) -> Result<Json<bool>, ApiError> {
    Ok(Json(host.updater.check().await?))
}

async fn apply_update(State(host): State<Arc<Host>>) -> StatusCode {
    tokio::spawn(async move {
        if let Err(err) = host.update_all().await {
            tracing::warn!("更新失败: {err:#}");
        }
    });
    StatusCode::ACCEPTED
}

async fn sync_version(State(host): State<Arc<Host>>, Path(id): Path<String>) -> StatusCode {
    tokio::spawn(async move {
        if let Err(err) = host.sync_version(id).await {
            tracing::warn!("同步版本失败: {err:#}");
        }
    });
    StatusCode::ACCEPTED
}

async fn open_downloads() -> StatusCode {
    let dir = files::default_dir();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
    StatusCode::NO_CONTENT
}
