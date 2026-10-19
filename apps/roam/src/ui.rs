//! 界面后端：只监听 127.0.0.1，应用窗口（或浏览器）里显示。主机和被控端共用一个页面。

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
};
use roam_core::{
    config::Role,
    layout::{Cell, GRID_COLS, GRID_ROWS, grid_key},
    settings::{ClipboardSettings, DeviceSettings, InputSettings, SwitchSettings},
};
use roam_proto::{DisplayInfo, Os};
use serde::{Deserialize, Serialize};

use crate::{
    client::{Client, ConnectedHost},
    discovery::Discovered,
    files::{self, TransferInfo},
    host::Host,
    update::UpdateState,
};

const INDEX_HTML: &str = include_str!("../web/index.html");

/// 当前运行的角色。
#[derive(Clone)]
pub enum App {
    Host(Arc<Host>),
    Client(Arc<Client>),
}

impl App {
    fn updater(&self) -> &crate::update::Updater {
        match self {
            Self::Host(h) => &h.updater,
            Self::Client(c) => &c.updater,
        }
    }
    fn host(&self) -> Result<&Arc<Host>, ApiError> {
        match self {
            App::Host(h) => Ok(h),
            App::Client(_) => Err(anyhow::anyhow!("本机是被控端，不能执行这个操作").into()),
        }
    }

    fn client(&self) -> Result<&Arc<Client>, ApiError> {
        match self {
            App::Client(c) => Ok(c),
            App::Host(_) => Err(anyhow::anyhow!("本机是主机，不能执行这个操作").into()),
        }
    }

    fn store(&self) -> &roam_core::config::Store {
        match self {
            App::Host(h) => &h.store,
            App::Client(c) => &c.store,
        }
    }
}

pub async fn serve(app: App, port: u16) -> Result<SocketAddr> {
    let router = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/api/state", get(state))
        .route("/api/logs", get(logs))
        .route("/api/role", put(set_role))
        .route("/api/name", put(set_name))
        .route("/api/open-downloads", post(open_downloads))
        .route("/api/open-logs", post(open_logs))
        .route("/api/context-menu", put(set_context_menu))
        .route("/api/input-settings", put(set_input_settings))
        .route(
            "/api/files/requests/{token}",
            get(file_request).post(send_request),
        )
        // 主机
        .route("/api/grid/{id}/{display}", put(move_display))
        .route("/api/peers", post(add_peer))
        .route("/api/peers/{id}", delete(remove_peer))
        .route("/api/peers/{id}/retry", post(retry_peer))
        .route("/api/peers/{id}/settings", put(set_peer_settings))
        .route("/api/peers/{id}/sync-version", post(sync_version))
        .route("/api/peers/{id}/lock", post(lock_peer))
        .route("/api/lock", put(set_lock))
        .route("/api/pause", put(set_pause))
        .route("/api/home", post(go_home))
        .route("/api/settings", put(set_settings))
        .route("/api/update/check", post(check_update))
        .route("/api/update/apply", post(apply_update))
        .route("/api/update/mirror", put(set_update_mirror))
        // 被控端
        .route("/api/trusted/{id}", delete(revoke_host))
        .route("/api/copy-ticket", post(copy_ticket))
        .route("/api/lock-control/{action}", post(lock_control))
        .with_state(app);
    // 更新或切换角色后重启时旧进程可能还没完全退出，端口被占用就稍等重试
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
        if let Err(err) = axum::serve(listener, router.layer(Extension(addr.port()))).await {
            tracing::error!("界面服务退出: {err:#}");
        }
    });
    Ok(addr)
}

// ---------------------------------------------------------------- 状态

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
    /// 连着但暂时没有响应。
    stalled: bool,
    /// 本次连接里网络中断的次数和最长一次（毫秒）。
    drops: u32,
    longest_drop_ms: u64,
    settings: Option<DeviceSettings>,
    version: Option<String>,
    displays: Vec<DisplayInfo>,
    /// 每块显示器所在的格子（还没连上过的设备按一块算）。
    cells: Vec<Option<Cell>>,
}

#[derive(Serialize)]
struct HostView {
    capture_ok: bool,
    capture_message: Option<String>,
    focused: Option<String>,
    swap_cmd_ctrl: bool,
    switching: SwitchSettings,
    clipboard: ClipboardSettings,
    paused: bool,
    edge_locked: bool,
    devices: Vec<DeviceView>,
    nearby: Vec<Discovered>,
    update: UpdateState,
    update_mirror: Option<String>,
    grid: (i32, i32),
}

#[derive(Serialize)]
struct TrustedHost {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct ClientView {
    ticket: String,
    host: Option<ConnectedHost>,
    pending: Option<(String, String)>,
    permission: Option<String>,
    trusted: Vec<TrustedHost>,
    status: String,
    /// Windows 锁屏控制服务的状态："not_installed" / "stopped" / "running"；其他系统为 None。
    lock_control: Option<&'static str>,
    /// 是否允许控制以管理员身份运行的程序（Windows）。
    allow_elevated: Option<bool>,
    /// 最近 30 秒内因为是管理员程序而控制不了的程序名。
    elevated_blocked: Option<String>,
}

#[derive(Serialize)]
struct StateView {
    update: UpdateState,
    update_mirror: Option<String>,
    input: InputSettings,
    mac_scroll: Option<MacScrollView>,
    context_menu: bool,
    role: Role,
    id: String,
    name: String,
    os: Os,
    version: &'static str,
    transfers: Vec<TransferInfo>,
    downloads_dir: String,
    host: Option<HostView>,
    client: Option<ClientView>,
}

async fn state(State(app): State<App>) -> Json<StateView> {
    match app {
        App::Host(host) => Json(host_state(&host)),
        App::Client(client) => Json(client_state(&client)),
    }
}

fn host_state(host: &Host) -> StateView {
    let config = host.config.lock().unwrap().clone();
    let update = host.updater.state.lock().unwrap().clone();
    let status = host.status.lock().unwrap().clone();
    let capture = host.capture.lock().unwrap().clone();
    let (focused, edge_locked) = {
        let router = host.router.lock().unwrap();
        (router.focused().map(str::to_string), router.edge_locked())
    };

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
        stalled: false,
        drops: 0,
        longest_drop_ms: 0,
        settings: None,
        version: Some(roam_proto::VERSION.into()),
        displays: host.displays.clone(),
        cells: cells_of(&config.grid, &host.host_id, host.displays.len()),
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
            stalled: s.stalled,
            drops: s.drops,
            longest_drop_ms: s.longest_drop_ms,
            settings: Some(p.settings.clone()),
            version: s.version,
            displays: p.displays.clone(),
            cells: cells_of(&config.grid, &p.id, p.displays.len()),
        }
    }));

    StateView {
        update: update.clone(),
        update_mirror: config.update_mirror.clone(),
        input: config.input.clone(),
        mac_scroll: mac_scroll_view(),
        context_menu: crate::send_menu::enabled(),
        role: Role::Host,
        id: host.host_id.clone(),
        name: config.name.clone(),
        os: Os::current(),
        version: roam_proto::VERSION,
        transfers: host.transfers.list(),
        downloads_dir: files::default_dir().display().to_string(),
        client: None,
        host: Some(HostView {
            capture_ok: capture.is_ok(),
            capture_message: capture.err(),
            focused,
            swap_cmd_ctrl: config.swap_cmd_ctrl,
            switching: config.switching.clone(),
            clipboard: config.clipboard.clone(),
            paused: *host.paused.lock().unwrap(),
            edge_locked,
            nearby: host
                .nearby
                .list()
                .into_iter()
                .filter(|d| d.id != host.host_id && !config.peers.iter().any(|p| p.id == d.id))
                .collect(),
            devices,
            update,
            update_mirror: config.update_mirror.clone(),
            grid: (GRID_COLS, GRID_ROWS),
        }),
    }
}

fn client_state(client: &Client) -> StateView {
    let config = client.store.load().unwrap_or_else(|_| {
        // 读不到配置时也要能显示界面
        serde_json::from_str(r#"{"name":"本机"}"#).expect("minimal config")
    });
    // 先分别取出各项再组装：在同一个表达式里多次加锁会自己等自己（死锁）
    let host = client.host.lock().unwrap().clone();
    let pending = client.pending.lock().unwrap().clone();
    let status = client.status_text();
    StateView {
        update: client.updater.state.lock().unwrap().clone(),
        update_mirror: config.update_mirror.clone(),
        input: config.input.clone(),
        mac_scroll: mac_scroll_view(),
        context_menu: crate::send_menu::enabled(),
        role: Role::Client,
        id: client.id.clone(),
        name: config.name.clone(),
        os: Os::current(),
        version: roam_proto::VERSION,
        transfers: client.transfers.list(),
        downloads_dir: files::default_dir().display().to_string(),
        host: None,
        client: Some(ClientView {
            ticket: client.ticket.clone(),
            host,
            pending,
            permission: client.permission.clone().err(),
            trusted: config
                .trusted_hosts
                .iter()
                .map(|id| TrustedHost {
                    id: id.clone(),
                    name: config
                        .host_names
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| format!("主机 {}", &id[..id.len().min(10)])),
                })
                .collect(),
            status,
            lock_control: lock_control_status(),
            allow_elevated: allow_elevated_status(),
            elevated_blocked: client
                .elevated_blocked
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(_, t)| t.elapsed() < std::time::Duration::from_secs(30))
                .map(|(app, _)| app.clone()),
        }),
    }
}

fn lock_control_status() -> Option<&'static str> {
    #[cfg(windows)]
    return Some(match crate::winsvc::status() {
        crate::winsvc::ServiceStatusView::NotInstalled => "not_installed",
        crate::winsvc::ServiceStatusView::Stopped => "stopped",
        crate::winsvc::ServiceStatusView::Running => "running",
    });
    #[allow(unreachable_code)]
    None
}

fn allow_elevated_status() -> Option<bool> {
    #[cfg(windows)]
    return Some(crate::winsvc::allow_elevated());
    #[allow(unreachable_code)]
    None
}

async fn logs() -> Json<Vec<String>> {
    Json(crate::logging::recent(400))
}

// ---------------------------------------------------------------- 错误

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

type ApiResult<T = StatusCode> = Result<T, ApiError>;

#[derive(Serialize)]
struct MacScrollView {
    ready: bool,
    error: Option<String>,
    source: &'static str,
}

fn mac_scroll_view() -> Option<MacScrollView> {
    #[cfg(target_os = "macos")]
    {
        let (ready, error, source) = roam_input::mac_scroll::status();
        Some(MacScrollView {
            ready,
            error,
            source,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

async fn set_input_settings(
    State(app): State<App>,
    Json(settings): Json<InputSettings>,
) -> ApiResult {
    settings.validate()?;
    match &app {
        App::Host(host) => {
            host.config.lock().unwrap().input = settings.clone();
            host.save_config()?;
            host.rebuild_world();
        }
        App::Client(client) => {
            let mut config = client.store.load()?;
            config.input = settings.clone();
            client.store.save(&config)?;
            if let Err(err) = client.push_keyboard_settings().await {
                tracing::warn!("键盘映射已保存，将在重新连接主机后应用: {err:#}");
            }
        }
    }
    #[cfg(target_os = "macos")]
    roam_input::mac_scroll::configure(settings.mac_scroll);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MenuBody {
    enabled: bool,
}

async fn set_context_menu(
    State(app): State<App>,
    Extension(port): Extension<u16>,
    Json(body): Json<MenuBody>,
) -> ApiResult {
    crate::send_menu::set_enabled(app.store(), port, body.enabled)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct FileRequestView {
    names: Vec<String>,
    targets: Vec<roam_proto::FileTarget>,
}

async fn file_request(
    State(app): State<App>,
    Path(token): Path<String>,
) -> ApiResult<Json<FileRequestView>> {
    let request = crate::send_menu::read(app.store(), &token)?;
    let names = request
        .paths
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let targets = match &app {
        App::Host(host) => host.file_targets(&host.host_id),
        App::Client(client) => client.file_targets().await?,
    };
    Ok(Json(FileRequestView { names, targets }))
}

#[derive(Deserialize)]
struct SendRequestBody {
    target: String,
}

async fn send_request(
    State(app): State<App>,
    Path(token): Path<String>,
    Json(body): Json<SendRequestBody>,
) -> ApiResult {
    let request = crate::send_menu::claim(app.store(), &token)?;
    // 窗口关闭也让已确认的传输完成；成功删除请求，失败允许重试。
    tokio::spawn(async move {
        let result = match &app {
            App::Host(host) => host.send_files(&body.target, request.paths).await,
            App::Client(client) => client.send_files(&body.target, request.paths).await,
        };
        crate::send_menu::finish(app.store(), &token, result.is_ok())?;
        result
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- 通用

#[derive(Deserialize)]
struct RoleBody {
    role: Role,
}

/// 切换角色：保存后用新角色重启程序。
async fn set_role(State(app): State<App>, Json(body): Json<RoleBody>) -> ApiResult {
    let store = app.store().clone();
    let mut config = store.load()?;
    config.role = Some(body.role);
    store.save(&config)?;
    if let App::Host(host) = &app {
        host.router.lock().unwrap().return_home();
    }
    tracing::info!("切换角色为 {:?}，正在重启…", body.role);
    tokio::spawn(async {
        // 先把响应发回去，再重启
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        crate::relaunch();
    });
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn set_name(State(app): State<App>, Json(body): Json<NameBody>) -> ApiResult {
    let name = body.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(anyhow::anyhow!("名称不能为空，也不能超过 40 个字").into());
    }
    match &app {
        App::Host(host) => {
            host.config.lock().unwrap().name = name;
            host.save_config()?;
        }
        App::Client(client) => {
            let mut config = client.store.load()?;
            config.name = name;
            client.store.save(&config)?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn open_downloads() -> StatusCode {
    let dir = files::default_dir();
    let _ = std::fs::create_dir_all(&dir);
    open_path(&dir);
    StatusCode::NO_CONTENT
}

async fn open_logs() -> StatusCode {
    if let Some(dir) = crate::logging::dir() {
        open_path(&dir);
    }
    StatusCode::NO_CONTENT
}

fn open_path(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

// ---------------------------------------------------------------- 主机

fn cells_of(
    grid: &std::collections::BTreeMap<String, Cell>,
    id: &str,
    displays: usize,
) -> Vec<Option<Cell>> {
    (0..displays.max(1))
        .map(|i| grid.get(&grid_key(id, i)).copied())
        .collect()
}

async fn move_display(
    State(app): State<App>,
    Path((id, display)): Path<(String, usize)>,
    Json(cell): Json<Cell>,
) -> ApiResult {
    app.host()?.move_display(&id, display, cell)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AddPeerBody {
    ticket: String,
    name: Option<String>,
}

async fn add_peer(
    State(app): State<App>,
    Json(body): Json<AddPeerBody>,
) -> ApiResult<Json<String>> {
    Ok(Json(app.host()?.add_peer(&body.ticket, body.name)?))
}

async fn remove_peer(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    app.host()?.remove_peer(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn retry_peer(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    app.host()?.retry_peer(&id)?;
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

async fn set_settings(State(app): State<App>, Json(body): Json<SettingsBody>) -> ApiResult {
    let host = app.host()?;
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
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<PeerSettingsBody>,
) -> ApiResult {
    let host = app.host()?;
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

async fn set_lock(State(app): State<App>, Json(body): Json<LockBody>) -> ApiResult {
    app.host()?
        .router
        .lock()
        .unwrap()
        .set_edge_locked(body.locked);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PauseBody {
    paused: bool,
}

async fn set_pause(State(app): State<App>, Json(body): Json<PauseBody>) -> ApiResult {
    app.host()?.set_paused(body.paused);
    Ok(StatusCode::NO_CONTENT)
}

async fn go_home(State(app): State<App>) -> ApiResult {
    app.host()?.router.lock().unwrap().return_home();
    Ok(StatusCode::NO_CONTENT)
}

async fn check_update(State(app): State<App>) -> ApiResult<Json<bool>> {
    Ok(Json(app.updater().check().await?))
}

#[derive(Deserialize)]
struct MirrorBody {
    update_mirror: String,
}

async fn set_update_mirror(State(app): State<App>, Json(body): Json<MirrorBody>) -> ApiResult {
    let mirror = body.update_mirror.trim().to_string();
    if !mirror.is_empty() && !mirror.starts_with("https://") && !mirror.starts_with("http://") {
        return Err(anyhow::anyhow!("镜像地址需要以 http:// 或 https:// 开头").into());
    }
    let mirror = (!mirror.is_empty()).then_some(mirror);
    match &app {
        App::Host(h) => {
            h.config.lock().unwrap().update_mirror = mirror;
            h.save_config()?;
        }
        App::Client(c) => {
            let mut config = c.store.load()?;
            config.update_mirror = mirror;
            c.store.save(&config)?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn apply_update(State(app): State<App>) -> ApiResult {
    tokio::spawn(async move {
        let result = match app {
            App::Host(h) => h.update_all().await,
            App::Client(c) => c.update_self().await,
        };
        if let Err(err) = result {
            tracing::warn!("更新失败: {err:#}");
        }
    });
    Ok(StatusCode::ACCEPTED)
}

async fn lock_peer(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let host = app.host()?;
    if !host
        .status
        .lock()
        .unwrap()
        .get(&id)
        .is_some_and(|s| s.online)
    {
        return Err(anyhow::anyhow!("这台设备不在线").into());
    }
    host.router.lock().unwrap().lock_device(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn sync_version(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let host = app.host()?.clone();
    tokio::spawn(async move {
        if let Err(err) = host.sync_version(id).await {
            tracing::warn!("同步版本失败: {err:#}");
        }
    });
    Ok(StatusCode::ACCEPTED)
}

// ---------------------------------------------------------------- 被控端

async fn revoke_host(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    app.client()?.revoke(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn copy_ticket(State(app): State<App>) -> ApiResult {
    let ticket = app.client()?.ticket.clone();
    tokio::task::spawn_blocking(move || crate::clipboard::with_clipboard(|c| c.set_text(ticket)))
        .await???;
    Ok(StatusCode::NO_CONTENT)
}

/// 安装 / 卸载 Windows 锁屏控制服务（会弹出管理员确认）。
async fn lock_control(State(app): State<App>, Path(action): Path<String>) -> ApiResult {
    app.client()?;
    if !["install", "uninstall", "allow-elevated", "deny-elevated"].contains(&action.as_str()) {
        return Err(anyhow::anyhow!("未知操作").into());
    }
    #[cfg(windows)]
    {
        let log_dir = crate::logging::dir().unwrap_or_else(std::env::temp_dir);
        tokio::task::spawn_blocking(move || crate::winsvc::run_elevated(&action, &log_dir))
            .await??;
        Ok(StatusCode::NO_CONTENT)
    }
    #[cfg(not(windows))]
    Err(anyhow::anyhow!("只有 Windows 需要锁屏控制服务").into())
}
