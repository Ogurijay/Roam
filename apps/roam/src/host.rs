//! 主机：采集本机键鼠，按布局路由到各台被控设备。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint,
    endpoint::{ConnectionError, VarInt},
};
use roam_core::{
    config::{Config, Peer, Store},
    layout::{DeviceGeom, World, default_position},
    net::{PathKind, selected_path},
    router::{CursorControl, Outbound, PeerLink, Router},
    ticket,
};
use roam_proto::{ALPN, DisplayInfo, Hello, Os, StreamMsg};
use tokio::{sync::mpsc, task::AbortHandle};

use crate::{
    conn::{read_msg, write_msg},
    discovery::{self, Nearby},
    pairing,
    tray::{self, TrayHandle},
};

/// 被控端拒绝授权时关闭连接用的错误码。
const DENIED: u32 = 1;

/// 被控端拒绝了授权：不再自动重连，避免反复弹窗打扰对方。
#[derive(Debug)]
struct Denied;

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("对方拒绝了授权，可以在页面上点「重试」重新请求")
    }
}

impl std::error::Error for Denied {}

/// 被控设备的运行时状态（不落盘）。
#[derive(Debug, Clone, Default)]
pub struct PeerStatus {
    pub online: bool,
    pub path: Option<PathKind>,
    pub rtt_ms: Option<f64>,
    pub error: Option<String>,
    /// 等待被控端确认时显示的校验码。
    pub pending_code: Option<String>,
    /// 被拒绝后停止重连，等用户手动重试。
    pub denied: bool,
}

pub struct Host {
    pub store: Store,
    pub config: Mutex<Config>,
    pub endpoint: Endpoint,
    pub router: Arc<Mutex<Router>>,
    pub host_id: String,
    pub displays: Vec<DisplayInfo>,
    pub status: Mutex<HashMap<String, PeerStatus>>,
    /// 键鼠采集是否就绪；失败时为原因说明。
    pub capture: Mutex<Result<(), String>>,
    /// 局域网里发现的被控设备。
    pub nearby: Nearby,
    pub paused: Mutex<bool>,
    tasks: Mutex<HashMap<String, AbortHandle>>,
    _mdns: Option<mdns_sd::ServiceDaemon>,
}

impl Host {
    pub fn new(
        store: Store,
        endpoint: Endpoint,
        cursor: Box<dyn CursorControl>,
    ) -> Result<Arc<Self>> {
        let config = store.load()?;
        let host_id = endpoint.id().to_string();
        let nearby = Nearby::default();
        let mdns = match discovery::browse(nearby.clone()) {
            Ok(daemon) => Some(daemon),
            Err(err) => {
                tracing::warn!("局域网发现不可用: {err:#}");
                None
            }
        };
        let router = Router::new(host_id.clone(), Os::current(), cursor);
        let host = Arc::new(Self {
            store,
            config: Mutex::new(config),
            endpoint,
            router: Arc::new(Mutex::new(router)),
            host_id,
            displays: roam_input::displays(),
            status: Mutex::new(HashMap::new()),
            capture: Mutex::new(Err("尚未启动".into())),
            nearby: nearby.clone(),
            paused: Mutex::new(false),
            tasks: Mutex::new(HashMap::new()),
            _mdns: mdns,
        });
        host.rebuild_world();
        Ok(host)
    }

    /// 定期把状态推给菜单栏图标（需要在 tokio 运行时上下文中调用）。
    pub fn start_tray_updates(self: &Arc<Self>, tray: TrayHandle) {
        let host = self.clone();
        tokio::spawn(async move {
            loop {
                tray.update(host.tray_state());
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        });
    }

    pub fn start(self: &Arc<Self>) {
        let peers = self.config.lock().unwrap().peers.clone();
        for peer in peers {
            self.spawn_peer(peer);
        }
        let router = self.router.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                router.lock().unwrap().tick();
            }
        });
    }

    /// 菜单栏显示的状态。
    pub fn tray_state(&self) -> tray::State {
        let focused = self.router.lock().unwrap().focused().map(str::to_string);
        let paused = *self.paused.lock().unwrap();
        let locked = self.router.lock().unwrap().edge_locked();
        let online = self
            .status
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.online)
            .count();
        let focused_name = {
            let config = self.config.lock().unwrap();
            focused.and_then(|id| {
                config
                    .peers
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
            })
        };
        let mut status = match (&focused_name, paused) {
            (_, true) => "已暂停切屏".to_string(),
            (Some(name), _) => format!("正在控制「{name}」"),
            (None, _) => format!("在本机 · {online} 台设备在线"),
        };
        if locked && !paused {
            status.push_str(" · 已锁定当前屏幕");
        }
        let title = match (&focused_name, paused) {
            (_, true) => Some("⏸".to_string()),
            (Some(name), _) => Some(format!("→ {name}")),
            (None, _) => None,
        };
        tray::State {
            title,
            tooltip: format!("Roam · {status}"),
            status,
            paused,
        }
    }

    pub fn set_paused(&self, paused: bool) {
        *self.paused.lock().unwrap() = paused;
        self.router.lock().unwrap().set_paused(paused);
    }

    /// 被拒绝或出错后手动重试。
    pub fn retry_peer(self: &Arc<Self>, id: &str) -> Result<()> {
        let peer = self
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .context("没有这台设备")?;
        self.status.lock().unwrap().remove(id);
        self.spawn_peer(peer);
        Ok(())
    }

    pub fn save_config(&self) -> Result<()> {
        let config = self.config.lock().unwrap();
        self.store.save(&config)
    }

    /// 根据配置与在线状态重建布局，并交给路由器。
    /// 有显示器信息但还没有位置的设备会被放到右侧，并写回配置。
    pub fn rebuild_world(&self) {
        let mut config = self.config.lock().unwrap();
        let status = self.status.lock().unwrap();

        let mut placed: Vec<DeviceGeom> = Vec::new();
        let mut changed = false;
        let mut entries: Vec<(String, Vec<DisplayInfo>)> =
            vec![(self.host_id.clone(), self.displays.clone())];
        entries.extend(
            config
                .peers
                .iter()
                .filter(|p| !p.displays.is_empty() && p.settings.enabled)
                .map(|p| (p.id.clone(), p.displays.clone())),
        );
        for (id, displays) in entries {
            let pos = match config.layout.get(&id) {
                Some(pos) => *pos,
                None => {
                    let pos = default_position(&placed);
                    config.layout.insert(id.clone(), pos);
                    changed = true;
                    pos
                }
            };
            placed.push(DeviceGeom {
                id,
                displays: displays.iter().map(|d| d.rect).collect(),
                pos,
            });
        }
        if changed && let Err(err) = self.store.save(&config) {
            tracing::warn!("保存配置失败: {err:#}");
        }

        // 只有主机和在线设备参与切屏
        let world = World {
            devices: placed
                .into_iter()
                .filter(|d| d.id == self.host_id || status.get(&d.id).is_some_and(|s| s.online))
                .collect(),
        };
        drop(status);
        let mut router = self.router.lock().unwrap();
        router.set_swap_cmd_ctrl(config.swap_cmd_ctrl);
        router.set_switch_settings(config.switching.clone());
        router.set_device_settings(
            config
                .peers
                .iter()
                .map(|p| (p.id.clone(), p.settings.clone()))
                .collect(),
        );
        router.set_device_order(
            config
                .peers
                .iter()
                .filter(|p| p.settings.enabled)
                .map(|p| p.id.clone())
                .collect(),
        );
        drop(config);
        router.set_world(world);
    }

    pub fn add_peer(self: &Arc<Self>, code: &str, name: Option<String>) -> Result<String> {
        let addr = ticket::decode(code)?;
        let id = addr.id.to_string();
        if id == self.host_id {
            bail!("这是本机自己的连接码");
        }
        let peer = {
            let mut config = self.config.lock().unwrap();
            if config.peers.iter().any(|p| p.id == id) {
                bail!("这台设备已经添加过了");
            }
            let peer = Peer {
                id: id.clone(),
                ticket: code.trim().to_string(),
                name: name
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| format!("设备 {}", addr.id.fmt_short())),
                os: None,
                displays: Vec::new(),
                settings: Default::default(),
            };
            config.peers.push(peer.clone());
            peer
        };
        self.save_config()?;
        self.spawn_peer(peer);
        Ok(id)
    }

    pub fn remove_peer(&self, id: &str) -> Result<()> {
        if let Some(task) = self.tasks.lock().unwrap().remove(id) {
            task.abort();
        }
        self.router.lock().unwrap().remove_peer(id);
        self.status.lock().unwrap().remove(id);
        {
            let mut config = self.config.lock().unwrap();
            config.peers.retain(|p| p.id != id);
            config.layout.remove(id);
        }
        self.save_config()?;
        self.rebuild_world();
        Ok(())
    }

    fn spawn_peer(self: &Arc<Self>, peer: Peer) {
        let host = self.clone();
        let id = peer.id.clone();
        let task = tokio::spawn(async move { host.peer_loop(peer).await });
        if let Some(old) = self.tasks.lock().unwrap().insert(id, task.abort_handle()) {
            old.abort();
        }
    }

    /// 维持与一台被控设备的连接：断了就退避重连。
    async fn peer_loop(self: Arc<Self>, peer: Peer) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let result = self.peer_session(&peer).await;
            self.router.lock().unwrap().remove_peer(&peer.id);
            let denied = matches!(&result, Err(e) if e.is::<Denied>());
            {
                let mut status = self.status.lock().unwrap();
                let entry = status.entry(peer.id.clone()).or_default();
                let was_online = entry.online;
                *entry = PeerStatus {
                    error: result.err().map(|e| friendly_error(&e)),
                    denied,
                    ..Default::default()
                };
                if was_online {
                    backoff = Duration::from_secs(1);
                }
            }
            self.rebuild_world();
            if denied {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }

    async fn peer_session(&self, peer: &Peer) -> Result<()> {
        let addr = ticket::decode(&peer.ticket)?;
        let conn = tokio::time::timeout(Duration::from_secs(15), self.endpoint.connect(addr, ALPN))
            .await
            .context("连接超时：对方可能没有运行 Roam，或网络不通")??;
        let (mut send, mut recv) = conn.open_bi().await?;
        let name = self.config.lock().unwrap().name.clone();
        write_msg(
            &mut send,
            &StreamMsg::Hello(Hello {
                name,
                os: Os::current(),
                displays: self.displays.clone(),
            }),
        )
        .await?;
        // 已信任的主机会立即收到回应；否则对方正在弹窗确认，页面上显示同一个校验码供核对
        let code = pairing::verification_code(&self.host_id, &peer.id);
        let first = match tokio::time::timeout(Duration::from_millis(500), read_msg(&mut recv))
            .await
        {
            Ok(result) => result,
            Err(_) => {
                self.status.lock().unwrap().insert(
                    peer.id.clone(),
                    PeerStatus {
                        pending_code: Some(code),
                        ..Default::default()
                    },
                );
                match tokio::time::timeout(Duration::from_secs(120), read_msg(&mut recv)).await {
                    Ok(result) => result,
                    Err(_) => bail!("等待对方确认超时"),
                }
            }
        };
        let hello = match first {
            Ok(StreamMsg::Hello(hello)) => hello,
            Ok(_) => bail!("对方没有发送 Hello"),
            Err(err) => {
                if let Some(ConnectionError::ApplicationClosed(close)) = conn.close_reason()
                    && close.error_code == VarInt::from_u32(DENIED)
                {
                    return Err(Denied.into());
                }
                return Err(err);
            }
        };

        // 记住对方的名字和显示器，离线时布局里也能显示
        {
            let mut config = self.config.lock().unwrap();
            if let Some(p) = config.peer_mut(&peer.id) {
                if p.name.starts_with("设备 ") && !hello.name.is_empty() {
                    p.name = hello.name.clone();
                }
                p.os = Some(hello.os);
                p.displays = hello.displays.clone();
            }
        }
        self.save_config()?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Outbound>();
        self.router
            .lock()
            .unwrap()
            .add_peer(peer.id.clone(), PeerLink { os: hello.os, tx });
        self.update_status(&peer.id, &conn);
        self.rebuild_world();
        tracing::info!("已连接 {}", hello.name);

        // 被控端只会回 Pong，后台读掉即可
        let reader = tokio::spawn(async move { while read_msg(&mut recv).await.is_ok() {} });
        let mut status_tick = tokio::time::interval(Duration::from_secs(1));
        let result = loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break Ok(()) };
                    match msg {
                        Outbound::Datagram(d) => {
                            let _ = conn.send_datagram(d.encode()?.into());
                        }
                        Outbound::Stream(m) => {
                            if let Err(err) = write_msg(&mut send, &m).await {
                                break Err(err);
                            }
                        }
                    }
                }
                _ = status_tick.tick() => self.update_status(&peer.id, &conn),
                reason = conn.closed() => break Err(anyhow::anyhow!("连接断开: {reason}")),
            }
        };
        reader.abort();
        result
    }

    fn update_status(&self, id: &str, conn: &iroh::endpoint::Connection) {
        let path = selected_path(conn);
        self.status.lock().unwrap().insert(
            id.to_string(),
            PeerStatus {
                online: true,
                path: path.as_ref().map(|p| p.0),
                rtt_ms: path.map(|p| p.2.as_secs_f64() * 1000.0),
                ..Default::default()
            },
        );
    }
}

/// 没有光标控制能力的平台（非 macOS 主机，仅用于布局预览和演示）。
#[cfg(not(target_os = "macos"))]
pub struct NoopCursor;

#[cfg(not(target_os = "macos"))]
impl CursorControl for NoopCursor {
    fn hide(&mut self) {}
    fn show(&mut self) {}
    fn warp(&mut self, x: f64, y: f64) {
        tracing::info!("光标回到主机 ({x:.0}, {y:.0})");
    }
    fn set_locked(&mut self, _locked: bool) {}
}

/// 把底层错误翻译成用户看得懂的说明。
fn friendly_error(err: &anyhow::Error) -> String {
    if err.is::<Denied>() {
        return err.to_string();
    }
    let text = format!("{err:#}");
    if text.contains("doesn't support any known protocol")
        || text.contains("no application protocol")
    {
        return "对方的 Roam 版本不兼容，请两边都更新到最新版".into();
    }
    if text.contains("timed out") || text.contains("TimedOut") {
        return "连接超时：对方可能已关闭 Roam，或网络不通".into();
    }
    if text.contains("closed by peer") || text.contains("reset") {
        return "对方断开了连接".into();
    }
    text
}
