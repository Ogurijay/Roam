//! 主机：采集本机键鼠，按布局路由到各台被控设备。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint,
    endpoint::{Connection, ConnectionError, VarInt},
};
use roam_core::{
    config::{Config, Peer, Store},
    health::{DOWN_AFTER, HEARTBEAT, LinkEvent, LinkHealth, STALL_AFTER, TIMER_LATE},
    layout::{Cell, DeviceGeom, HOST_CELL, World, default_cells, grid_key, nearest_free},
    net::{PathKind, describe_paths, selected_path},
    router::{CursorControl, Outbound, PeerLink, Router},
    ticket,
};
use roam_proto::{
    ALPN, ClipboardPolicy, Datagram, DisplayInfo, FileAnnounce, Hello, Os, StreamMsg, side,
};
use tokio::{sync::mpsc, task::AbortHandle};

use crate::{
    clipboard,
    conn::{read_msg, write_msg},
    discovery::{self, Nearby},
    files::{self, Transfers},
    pairing,
    pull::{self, KeyAction, PasteKeys, Pending, Shelf, Source},
    sync::{self, Inbound, LastDigest},
    tray::{self, TrayHandle},
    update::{self, Updater},
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
    /// 对方的程序版本。
    pub version: Option<String>,
    /// 连着但超过几秒没有响应：暂时不让光标切过去。
    pub stalled: bool,
    /// 本次连接里网络中断过几次、最长一次多久（毫秒）。
    pub drops: u32,
    pub longest_drop_ms: u64,
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
    /// 在线设备的连接（剪贴板同步用）。
    conns: Mutex<HashMap<String, Connection>>,
    /// 每台设备上次同步的剪贴板摘要，没变就不重复发送。
    clip_digest: Mutex<HashMap<String, LastDigest>>,
    /// 文件传输进度。
    pub transfers: Transfers,
    pub updater: Updater,
    /// 正在执行更新，防止重复点击。
    updating: Mutex<bool>,
    /// 文件按需传：被控设备宣告的文件、主机宣告出去的文件。
    shelf: Shelf,
    /// 正在替另一台设备向某台设备取文件：设备 ID → 文件到了之后通知。
    relay_waiters: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Vec<PathBuf>>>>,
    /// 识别主机本机按的粘贴键。
    paste_keys: Mutex<PasteKeys>,
    /// 采集线程不在 tokio 运行时里，要通过它启动异步任务。
    rt: tokio::runtime::Handle,
    _mdns: Option<mdns_sd::ServiceDaemon>,
    me: Mutex<std::sync::Weak<Host>>,
}

impl Host {
    pub fn receiver_keyboard(
        &self,
        id: &str,
        profile: roam_core::settings::KeyboardProfile,
    ) -> Result<()> {
        profile.validate()?;
        let to = self
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.os);
        let from = Os::current();
        if matches!(
            (from, to),
            (Os::Windows, Some(Os::MacOs)) | (Os::MacOs, Some(Os::Windows))
        ) {
            self.router
                .lock()
                .unwrap()
                .set_keyboard_override(id, profile);
        }
        Ok(())
    }

    /// 主机可向在线被控端发送；被控端查询时也包含主机本身。
    pub fn file_targets(&self, exclude: &str) -> Vec<roam_proto::FileTarget> {
        let config = self.config.lock().unwrap();
        let status = self.status.lock().unwrap();
        let mut targets = Vec::new();
        if exclude != self.host_id {
            targets.push(roam_proto::FileTarget {
                id: self.host_id.clone(),
                name: config.name.clone(),
                ready: true,
            });
        }
        targets.extend(config.peers.iter().filter_map(|p| {
            let s = status.get(&p.id)?;
            (p.id != exclude && s.online && !s.stalled).then(|| roam_proto::FileTarget {
                id: p.id.clone(),
                name: p.name.clone(),
                ready: files::supports_direct(s.version.as_deref().unwrap_or("")),
            })
        }));
        targets
    }

    pub fn check_file_target(&self, target: &str) -> anyhow::Result<()> {
        let target = self
            .file_targets(&self.host_id)
            .into_iter()
            .find(|t| t.id == target)
            .context("目标电脑不在线")?;
        anyhow::ensure!(
            target.ready,
            "请先将「{}」更新到 Roam v0.3.3 或更新版本",
            target.name
        );
        Ok(())
    }

    pub async fn send_files(&self, target: &str, paths: Vec<PathBuf>) -> anyhow::Result<()> {
        self.check_file_target(target)?;
        let conn = self
            .conns
            .lock()
            .unwrap()
            .get(target)
            .cloned()
            .context("目标电脑已断开连接")?;
        let name = self.peer_name(target);
        let max = self.config.lock().unwrap().clipboard.max_file_bytes();
        files::send_direct(&conn, paths, &name, max, &self.transfers, None).await
    }

    pub fn new(
        store: Store,
        endpoint: Endpoint,
        cursor: Box<dyn CursorControl>,
    ) -> Result<Arc<Self>> {
        let config = store.load()?;
        let host_id = endpoint.id().to_string();
        let updater = Updater::new(&endpoint, store.dir().join("updates"))?;
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
            conns: Mutex::new(HashMap::new()),
            clip_digest: Mutex::new(HashMap::new()),
            transfers: Transfers::default(),
            updater,
            updating: Mutex::new(false),
            shelf: Shelf::default(),
            relay_waiters: Mutex::new(HashMap::new()),
            paste_keys: Mutex::new(PasteKeys::default()),
            rt: tokio::runtime::Handle::current(),
            _mdns: mdns,
            me: Mutex::new(std::sync::Weak::new()),
        });
        *host.me.lock().unwrap() = Arc::downgrade(&host);
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
        let host = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            loop {
                let _ = host.updater.check().await;
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
            }
        });

        // 光标切到某台设备时，把主机剪贴板同步过去
        let host = self.clone();
        tokio::spawn(async move {
            let mut last: Option<String> = None;
            loop {
                tokio::time::sleep(Duration::from_millis(30)).await;
                let now = host.router.lock().unwrap().focused().map(str::to_string);
                if now != last {
                    if let Some(id) = &now {
                        let host = host.clone();
                        let id = id.clone();
                        tokio::spawn(async move { host.sync_clipboard_to(&id).await });
                    }
                    last = now;
                }
            }
        });

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
        let update_label = {
            let u = self.updater.state.lock().unwrap();
            match (&u.phase, &u.latest) {
                (Some(phase), _) => Some(phase.clone()),
                (None, Some(r)) if u.available => Some(format!("更新到 v{}", r.version)),
                (None, Some(r)) => Some(format!("同步其他设备到 v{}", r.version)),
                _ => None,
            }
        };
        tray::State {
            tooltip: format!("Roam v{} · {status}", roam_proto::VERSION),
            status,
            paused,
            update_label,
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
        let mut changed = false;

        // 旧版本按设备记格子（键只有设备 ID），迁移成第 0 块显示器
        let old_keys: Vec<String> = config
            .grid
            .keys()
            .filter(|k| !k.contains('#'))
            .cloned()
            .collect();
        for key in old_keys {
            if let Some(cell) = config.grid.remove(&key) {
                config.grid.entry(grid_key(&key, 0)).or_insert(cell);
            }
            changed = true;
        }

        // 每台设备的显示器（还没连上过的设备先按一块算，方便提前摆放）
        let mut devices: Vec<(String, Vec<roam_proto::Rect>)> = vec![(
            self.host_id.clone(),
            self.displays.iter().map(|d| d.rect).collect(),
        )];
        devices.extend(
            config
                .peers
                .iter()
                .map(|p| (p.id.clone(), p.displays.iter().map(|d| d.rect).collect())),
        );
        let valid: std::collections::HashSet<String> = devices
            .iter()
            .flat_map(|(id, ds)| (0..ds.len().max(1)).map(|i| grid_key(id, i)))
            .collect();
        let before = config.grid.len();
        config.grid.retain(|key, _| valid.contains(key));
        changed |= config.grid.len() != before;

        // 缺格子的显示器补上默认位置
        for (id, displays) in &devices {
            let n = displays.len().max(1);
            let missing: Vec<usize> = (0..n)
                .filter(|&i| !config.grid.contains_key(&grid_key(id, i)))
                .collect();
            if missing.is_empty() {
                continue;
            }
            changed = true;
            let taken: Vec<Cell> = config.grid.values().copied().collect();
            if missing.len() == n {
                // 整台设备都没有位置：主机放中间，被控设备放在主机最右边那块屏幕的右侧
                let anchor = if *id == self.host_id {
                    HOST_CELL
                } else {
                    let host_cells: Vec<Cell> = (0..self.displays.len().max(1))
                        .filter_map(|i| config.grid.get(&grid_key(&self.host_id, i)).copied())
                        .collect();
                    host_cells
                        .iter()
                        .max_by_key(|c| c.col)
                        .map(|c| Cell {
                            col: c.col + 1,
                            row: c.row,
                        })
                        .unwrap_or(HOST_CELL)
                };
                for (i, cell) in default_cells(displays, &taken, anchor)
                    .into_iter()
                    .enumerate()
                {
                    config.grid.insert(grid_key(id, i), cell);
                }
            } else {
                // 新接了显示器：放在这台设备已有屏幕旁边
                let near = (0..n)
                    .find_map(|i| config.grid.get(&grid_key(id, i)).copied())
                    .unwrap_or(HOST_CELL);
                for i in missing {
                    let taken: Vec<Cell> = config.grid.values().copied().collect();
                    let cell = nearest_free(&taken, near);
                    config.grid.insert(grid_key(id, i), cell);
                }
            }
        }
        if changed && let Err(err) = self.store.save(&config) {
            tracing::warn!("保存配置失败: {err:#}");
        }

        let geom = |id: &str, displays: &[roam_proto::Rect]| DeviceGeom {
            id: id.to_string(),
            displays: displays.to_vec(),
            cells: (0..displays.len())
                .map(|i| config.grid[&grid_key(id, i)])
                .collect(),
        };
        // 只有主机和在线、已启用的设备参与切屏
        let host_rects: Vec<roam_proto::Rect> = self.displays.iter().map(|d| d.rect).collect();
        let mut world_devices = vec![geom(&self.host_id, &host_rects)];
        for p in &config.peers {
            if p.settings.enabled
                && !p.displays.is_empty()
                && status.get(&p.id).is_some_and(|s| s.online && !s.stalled)
            {
                let rects: Vec<roam_proto::Rect> = p.displays.iter().map(|d| d.rect).collect();
                world_devices.push(geom(&p.id, &rects));
            }
        }
        let world = World {
            devices: world_devices,
        };
        drop(status);
        let mut router = self.router.lock().unwrap();
        router.set_swap_cmd_ctrl(config.swap_cmd_ctrl);
        router.set_keyboard_settings(config.input.keyboard.clone());
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

    /// 把某台设备的第 `display` 块屏幕挪到某个格子；格子里已有屏幕时两者交换。
    pub fn move_display(&self, id: &str, display: usize, cell: Cell) -> Result<()> {
        if !cell.in_grid() {
            bail!("格子超出范围");
        }
        let key = grid_key(id, display);
        {
            let mut config = self.config.lock().unwrap();
            let old = *config.grid.get(&key).context("没有这块屏幕")?;
            let other = config
                .grid
                .iter()
                .find(|(k, c)| **c == cell && **k != key)
                .map(|(k, _)| k.clone());
            if let Some(other) = other {
                config.grid.insert(other, old);
            }
            config.grid.insert(key, cell);
        }
        self.save_config()?;
        self.rebuild_world();
        Ok(())
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
            let prefix = format!("{id}#");
            config.grid.retain(|k, _| !k.starts_with(&prefix));
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
            // 等待重连；局域网里发现对方换了地址（例如重启）就立刻重试
            let known = self
                .config
                .lock()
                .unwrap()
                .peers
                .iter()
                .find(|p| p.id == peer.id)
                .map(|p| p.ticket.clone());
            let deadline = tokio::time::Instant::now() + backoff;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let fresh = self.nearby.get(&peer.id).map(|d| d.ticket);
                if fresh.is_some() && fresh != known {
                    tracing::info!("发现「{}」的新地址，立即重连", peer.name);
                    break;
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }

    async fn peer_session(&self, peer: &Peer) -> Result<()> {
        // 保存的连接码里的地址可能已经过时（对方重启换了端口、IP 变了），
        // 局域网里发现了这台设备时优先用它广播的最新连接码
        let fresh = self
            .nearby
            .get(&peer.id)
            .map(|d| d.ticket)
            .filter(|t| *t != peer.ticket);
        if let Some(fresh) = &fresh {
            if let Some(p) = self.config.lock().unwrap().peer_mut(&peer.id) {
                p.ticket = fresh.clone();
            }
            let _ = self.save_config();
        }
        let addr = ticket::decode(fresh.as_deref().unwrap_or(&peer.ticket))?;
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
                version: roam_proto::VERSION.into(),
                ticket: String::new(),
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
                if !hello.ticket.is_empty()
                    && ticket::decode(&hello.ticket).is_ok_and(|a| a.id.to_string() == p.id)
                {
                    p.ticket = hello.ticket.clone();
                }
            }
        }
        self.save_config()?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Outbound>();
        self.router
            .lock()
            .unwrap()
            .add_peer(peer.id.clone(), PeerLink { os: hello.os, tx });
        let health = Arc::new(Mutex::new(LinkHealth::default()));
        self.update_status(&peer.id, &conn, &hello.version, &health);
        self.rebuild_world();
        tracing::info!("已连接 {}", hello.name);

        self.conns
            .lock()
            .unwrap()
            .insert(peer.id.clone(), conn.clone());
        self.clip_digest
            .lock()
            .unwrap()
            .insert(peer.id.clone(), LastDigest::default());
        let policy = self.clipboard_policy(&peer.id);
        if let Err(err) = clipboard::send_side(&conn, side::CLIPBOARD_POLICY, &policy).await {
            tracing::warn!("发送剪贴板策略失败: {err:#}");
        }
        let side_task = {
            let host = self.this();
            let id = peer.id.clone();
            let rt = tokio::runtime::Handle::current();
            let ctx = Inbound {
                host: Some(host.clone()),
                peer: hello.name.clone(),
                policy: {
                    let host = host.clone();
                    let id = id.clone();
                    Arc::new(move || host.clipboard_policy(&id))
                },
                on_policy: None,
                last: self.last_digest(&id),
                transfers: self.transfers.clone(),
                // 对方的内容进了主机剪贴板；如果光标已经去了另一台设备，顺手转发过去
                on_updated: {
                    let host = host.clone();
                    let id = id.clone();
                    let rt = rt.clone();
                    Arc::new(move || host.forward_clipboard(&rt, &id))
                },
                // 对方复制了文件：记下来，粘贴时再取；光标已经在别的设备上就转告过去
                on_announce: {
                    let host = host.clone();
                    let id = id.clone();
                    let rt = rt.clone();
                    Arc::new(move |announce: FileAnnounce| {
                        tracing::info!(
                            "「{}」复制了 {}，粘贴时再传",
                            announce.from,
                            sync::describe(&announce)
                        );
                        host.shelf.arm(announce, Some(id.clone()));
                        host.forward_clipboard(&rt, &id);
                    })
                },
                on_pull: {
                    let host = host.clone();
                    let id = id.clone();
                    let rt = rt.clone();
                    Arc::new(move |token| {
                        rt.spawn(host.clone().serve_pull(id.clone(), token));
                    })
                },
                on_files: {
                    let host = host.clone();
                    let id = id.clone();
                    Arc::new(move |tops: &[PathBuf], ok| host.files_arrived(&id, tops, ok))
                },
            };
            tokio::spawn(sync::receive_loop(conn.clone(), ctx))
        };

        // 被控端只会回 Pong，后台读掉即可
        let reader = tokio::spawn(async move { while read_msg(&mut recv).await.is_ok() {} });
        // 心跳单独一个任务：网络卡住时下面的循环会阻塞在写流上，不能靠它来发现问题
        let heartbeat = {
            let host = self.this();
            let id = peer.id.clone();
            let name = hello.name.clone();
            let version = hello.version.clone();
            let conn = conn.clone();
            let health = health.clone();
            tokio::spawn(async move { host.heartbeat(id, name, version, conn, health).await })
        };
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
                _ = status_tick.tick() => self.update_status(&peer.id, &conn, &hello.version, &health),
                reason = conn.closed() => break Err(anyhow::anyhow!("连接断开: {reason}")),
            }
        };
        reader.abort();
        heartbeat.abort();
        side_task.abort();
        self.conns.lock().unwrap().remove(&peer.id);
        result
    }

    /// 定期给被控端发心跳并读回复。对方超过 [`STALL_AFTER`] 没有响应时把光标收回本机，
    /// 并且暂时不让光标切过去，恢复后自动放开。中断情况都写进日志。
    async fn heartbeat(
        self: Arc<Self>,
        id: String,
        name: String,
        version: String,
        conn: Connection,
        health: Arc<Mutex<LinkHealth>>,
    ) {
        let pongs = {
            let host = self.clone();
            let (id, name, version) = (id.clone(), name.clone(), version.clone());
            let conn = conn.clone();
            let health = health.clone();
            tokio::spawn(async move {
                while let Ok(bytes) = conn.read_datagram().await {
                    if !matches!(Datagram::decode(&bytes), Ok(Datagram::Pong { .. })) {
                        continue;
                    }
                    let (outage, was_stalled) = {
                        let mut h = health.lock().unwrap();
                        let was_stalled = h.stalled();
                        (h.heard(Instant::now()), was_stalled)
                    };
                    let Some(outage) = outage else { continue };
                    tracing::info!(
                        "「{name}」恢复响应，中断了 {:.1} 秒（{}）",
                        outage.as_secs_f64(),
                        describe_paths(&conn)
                    );
                    if was_stalled {
                        host.update_status(&id, &conn, &version, &health);
                        host.rebuild_world();
                    }
                }
            })
        };
        let _pongs = AbortOnDrop(pongs.abort_handle());

        let mut tick = tokio::time::interval(HEARTBEAT);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seq = 0u64;
        let mut late_logged: Option<Instant> = None;
        loop {
            let scheduled = tick.tick().await.into_std();
            let now = Instant::now();
            let late = now.saturating_duration_since(scheduled);
            if late >= TIMER_LATE && late_logged.is_none_or(|t| now - t > Duration::from_secs(60)) {
                tracing::warn!(
                    "心跳定时器晚了 {:.1} 秒：本机 Roam 被系统节流（App Nap / 节能模式）或者卡住了",
                    late.as_secs_f64()
                );
                late_logged = Some(now);
            }
            seq = seq.wrapping_add(1);
            if let Ok(bytes) = (Datagram::Ping { seq, sent_us: 0 }).encode() {
                let _ = conn.send_datagram(bytes.into());
            }
            let event = health.lock().unwrap().check(now);
            match event {
                Some(LinkEvent::Down) => tracing::warn!(
                    "「{name}」{:.0} 秒没有响应（{}）",
                    DOWN_AFTER.as_secs_f64(),
                    describe_paths(&conn)
                ),
                Some(LinkEvent::Stalled) => {
                    tracing::warn!(
                        "「{name}」超过 {} 秒没有响应，暂时不切到这台电脑（{}）",
                        STALL_AFTER.as_secs(),
                        describe_paths(&conn)
                    );
                    let returned = {
                        let mut router = self.router.lock().unwrap();
                        let focused = router.focused() == Some(id.as_str());
                        if focused {
                            router.return_home();
                        }
                        focused
                    };
                    if returned {
                        tracing::info!("光标已收回本机");
                    }
                    self.update_status(&id, &conn, &version, &health);
                    self.rebuild_world();
                }
                None => {}
            }
        }
    }

    // ------------------------------------------------------------ 剪贴板

    fn this(&self) -> Arc<Self> {
        self.me
            .lock()
            .unwrap()
            .upgrade()
            .expect("host is alive while sessions run")
    }

    /// 某台设备的剪贴板同步策略。
    fn clipboard_policy(&self, id: &str) -> ClipboardPolicy {
        let config = self.config.lock().unwrap();
        let per_device = config
            .peers
            .iter()
            .find(|p| p.id == id)
            .is_some_and(|p| p.settings.clipboard);
        ClipboardPolicy {
            enabled: config.clipboard.enabled && per_device,
            images: config.clipboard.images,
            max_bytes: config.clipboard.max_bytes(),
            files: config.clipboard.files,
            max_file_bytes: config.clipboard.max_file_bytes(),
        }
    }

    /// 设置变化后，把新的策略告诉所有在线设备。
    pub fn push_clipboard_policies(&self) {
        let conns: Vec<_> = self
            .conns
            .lock()
            .unwrap()
            .iter()
            .map(|(id, c)| (id.clone(), c.clone()))
            .collect();
        for (id, conn) in conns {
            let policy = self.clipboard_policy(&id);
            tokio::spawn(async move {
                let _ = clipboard::send_side(&conn, side::CLIPBOARD_POLICY, &policy).await;
            });
        }
    }

    fn last_digest(&self, id: &str) -> LastDigest {
        self.clip_digest
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    /// 被控设备的剪贴板内容到了主机：光标已经在另一台设备上时，顺手转发过去。
    fn forward_clipboard(self: &Arc<Self>, rt: &tokio::runtime::Handle, from: &str) {
        let host = self.clone();
        let from = from.to_string();
        rt.spawn(async move {
            let focused = host.router.lock().unwrap().focused().map(str::to_string);
            if let Some(other) = focused.filter(|f| *f != from) {
                host.sync_clipboard_to(&other).await;
            }
        });
    }

    /// 主机本机的按键（在采集线程里调用）：本机按了粘贴、而最近一次复制的是别的设备上的文件时，
    /// 拦下这次粘贴先去取文件。返回 true 表示吞掉这个按键。
    pub fn intercept_local_key(self: &Arc<Self>, usage: u16, down: bool) -> bool {
        let local = self.router.lock().unwrap().focused().is_none();
        let action = self
            .paste_keys
            .lock()
            .unwrap()
            .on_key(Os::current(), usage, down, || {
                local && self.local_pull_ready().is_some()
            });
        match action {
            KeyAction::Pass => false,
            KeyAction::Swallow => true,
            KeyAction::Pull => {
                if let Some(p) = self.local_pull_ready()
                    && self.shelf.begin_pull(p.announce.token)
                {
                    self.rt.spawn(self.clone().pull_for_local(p));
                }
                true
            }
        }
    }

    /// 本机按粘贴时可以去取的文件：最近一次复制的是某台在线设备上的文件。
    fn local_pull_ready(&self) -> Option<Pending> {
        let p = self.shelf.current()?;
        let online = p
            .peer
            .as_ref()
            .is_some_and(|peer| self.conns.lock().unwrap().contains_key(peer));
        online.then_some(p)
    }

    async fn pull_for_local(self: Arc<Self>, p: Pending) {
        let Some(conn) = p
            .peer
            .as_ref()
            .and_then(|peer| self.conns.lock().unwrap().get(peer).cloned())
        else {
            return;
        };
        tracing::info!(
            "在本机按了粘贴：向「{}」取 {}",
            p.announce.from,
            sync::describe(&p.announce)
        );
        if let Err(err) = sync::request_files(&conn, p.announce.token).await {
            tracing::warn!("请求文件失败: {err:#}");
        }
    }

    /// 某台设备在它那边按了粘贴，来取主机宣告过的文件；文件在另一台设备上时先取过来再转发。
    async fn serve_pull(self: Arc<Self>, to: String, token: u64) {
        let name = self.peer_name(&to);
        let Some(source) = self.shelf.lookup(token) else {
            tracing::warn!("「{name}」要取的文件已经过期");
            return;
        };
        let Some(conn) = self.conns.lock().unwrap().get(&to).cloned() else {
            return;
        };
        let paths = match source {
            Source::Local(paths) => paths,
            Source::Remote { peer, token } => match self.fetch_from(&peer, token).await {
                Ok(paths) => paths,
                Err(err) => {
                    tracing::warn!("为「{name}」中转文件失败: {err:#}");
                    return;
                }
            },
        };
        let max = self.clipboard_policy(&to).max_file_bytes;
        match files::send(&conn, paths.clone(), &name, max, &self.transfers).await {
            // 记下来：主机剪贴板里的这些文件对方已经有了，下次切过去不用再宣告
            Ok(()) => *self.last_digest(&to).lock().unwrap() = Some(files::digest(&paths)),
            Err(err) => tracing::warn!("发送文件给「{name}」失败: {err:#}"),
        }
    }

    /// 向某台设备取它宣告过的文件（中转用），返回收到后在主机上的路径。
    async fn fetch_from(&self, peer: &str, token: u64) -> Result<Vec<PathBuf>> {
        let conn = self
            .conns
            .lock()
            .unwrap()
            .get(peer)
            .cloned()
            .context("文件所在的设备不在线")?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.relay_waiters
            .lock()
            .unwrap()
            .insert(peer.to_string(), tx);
        sync::request_files(&conn, token).await?;
        tokio::time::timeout(Duration::from_secs(30 * 60), rx)
            .await
            .context("等待对方发送文件超时")?
            .context("传输中断")
    }

    /// 从某台设备收到的文件已经放进主机剪贴板（在普通线程里调用）。返回 true 表示不用再提醒用户。
    fn files_arrived(&self, from: &str, tops: &[PathBuf], ok: bool) -> bool {
        if let Some(tx) = self.relay_waiters.lock().unwrap().remove(from) {
            let _ = tx.send(tops.to_vec());
            return true;
        }
        let local = self.router.lock().unwrap().focused().is_none();
        match self.shelf.pull_done() {
            Some(true) if ok && local => {
                tracing::info!("文件已就绪，自动粘贴");
                let (left, right) = pull::paste_modifier(Os::current());
                // Windows 上修饰键还按着就只按 V；macOS 的按键事件要自己带上 Cmd 标志，总是一起按
                let held = cfg!(windows) && {
                    let keys = self.paste_keys.lock().unwrap();
                    keys.holds(left) || keys.holds(right)
                };
                let mut injector = roam_input::Injector::new();
                if !held {
                    injector.key(left, true);
                }
                injector.key(roam_core::keymap::hid::V, true);
                injector.key(roam_core::keymap::hid::V, false);
                if !held {
                    injector.key(left, false);
                }
                true
            }
            _ => false,
        }
    }

    fn peer_name(&self, id: &str) -> String {
        self.config
            .lock()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| short_id(id).to_string())
    }

    /// 把主机剪贴板发给某台设备：文本、图片直接发，文件只宣告。内容没变就跳过。
    async fn sync_clipboard_to(&self, id: &str) {
        let policy = self.clipboard_policy(id);
        let Some(conn) = self.conns.lock().unwrap().get(id).cloned() else {
            return;
        };
        let name = self.peer_name(id);
        let last = self.last_digest(id);

        // 最近一次复制的是某台设备上的文件（主机剪贴板之后没变过）：转告给这台设备，
        // 由主机中转；光标回到了文件所在的设备就什么都不用做
        if let Some(p) = self.shelf.current() {
            let Some(source) = p.peer.clone() else { return };
            if source == id || !policy.enabled || !policy.files {
                return;
            }
            let digest = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                ("relay", &source, p.announce.token).hash(&mut h);
                h.finish()
            };
            {
                let mut last = last.lock().unwrap();
                if *last == Some(digest) {
                    return;
                }
                *last = Some(digest);
            }
            let token = self.shelf.register(Source::Remote {
                peer: source,
                token: p.announce.token,
            });
            let announce = FileAnnounce {
                token,
                ..p.announce
            };
            if let Err(err) = sync::send_announce(&conn, &announce, &name).await {
                *last.lock().unwrap() = None;
                tracing::warn!("同步到「{name}」失败: {err:#}");
            }
            return;
        }

        let me = self.config.lock().unwrap().name.clone();
        if let Err(err) = sync::push(&conn, policy, &last, &name, &me, &self.shelf).await {
            tracing::warn!("同步到「{name}」失败: {err:#}");
        }
    }

    // ------------------------------------------------------------ 更新

    fn mirror(&self) -> Option<String> {
        self.config.lock().unwrap().update_mirror.clone()
    }

    /// 一键更新：先把新版本推给所有被控端，再更新主机自己并重启。
    pub async fn update_all(self: Arc<Self>) -> Result<()> {
        {
            let mut updating = self.updating.lock().unwrap();
            if *updating {
                bail!("正在更新中");
            }
            *updating = true;
        }
        let result = self.clone().update_all_inner().await;
        *self.updating.lock().unwrap() = false;
        if let Err(err) = &result {
            self.updater.set_phase(None);
            self.updater.set_error(Some(format!("更新失败：{err:#}")));
        }
        result
    }

    async fn update_all_inner(self: Arc<Self>) -> Result<()> {
        self.updater.set_error(None);
        self.updater.state.lock().unwrap().devices.clear();
        self.updater.set_phase(Some("正在检查更新…".into()));
        self.updater.check().await?;
        let (release, available) = {
            let state = self.updater.state.lock().unwrap();
            (
                state.latest.clone().context("没有可用的版本")?,
                state.available,
            )
        };
        let mirror = self.mirror();

        let peers: Vec<Peer> = self
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .filter(|p| p.settings.enabled)
            .cloned()
            .collect();
        let mut failures = Vec::new();
        for peer in peers {
            let current = self
                .status
                .lock()
                .unwrap()
                .get(&peer.id)
                .and_then(|s| s.version.clone());
            if !update::needs_update(current.as_deref(), &release.version) {
                self.record_update(&peer.name, "已是最新");
                continue;
            }
            // 更新通道独立于控制协议，即使版本不兼容、控制连接失败，也应尝试升级。
            let result = self.push_release(&peer, &release, mirror.as_deref()).await;
            match result {
                Ok(()) => self.record_update(&peer.name, "已安装，等待设备重启重连"),
                Err(err) => {
                    let message = format!("{err:#}");
                    self.record_update(&peer.name, &message);
                    failures.push(format!("{}：{message}", peer.name));
                }
            }
        }

        self.updater.save_results();
        if !available {
            self.updater.set_phase(None);
            if !failures.is_empty() {
                bail!("部分设备更新失败，请重试：{}", failures.join("；"));
            }
            return Ok(());
        }
        // 本机下载失败不应阻止其他系统的被控端先更新。
        let own = self
            .updater
            .fetch(&release, Os::current(), mirror.as_deref())
            .await?;

        self.updater.set_phase(Some(format!(
            "正在更新本机到 v{}，马上重启…",
            release.version
        )));
        self.router.lock().unwrap().return_home();
        let exe = update::install(&own)?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        update::restart_at(&exe);
    }

    fn record_update(&self, name: &str, result: &str) {
        tracing::info!("更新「{name}」: {result}");
        self.updater
            .state
            .lock()
            .unwrap()
            .devices
            .push((name.to_string(), result.to_string()));
    }

    async fn push_release(
        &self,
        peer: &Peer,
        release: &update::Release,
        mirror: Option<&str>,
    ) -> Result<()> {
        let os = peer.os.context("尚未连接过这台设备，无法确定安装包平台")?;
        let bin = self.updater.fetch(release, os, mirror).await?;
        self.updater
            .set_phase(Some(format!("正在推送到「{}」…", peer.name)));
        let fresh = self.nearby.get(&peer.id).map(|d| d.ticket);
        let saved = self
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.id == peer.id)
            .map(|p| p.ticket.clone());
        let addr = ticket::decode(
            fresh
                .as_deref()
                .or(saved.as_deref())
                .unwrap_or(&peer.ticket),
        )?;
        if addr.id.to_string() != peer.id {
            bail!("设备连接码与已配对的设备不符，请重新发现设备");
        }
        update::push(&self.endpoint, addr, &bin, &release.version).await
    }

    /// 把一台版本不一致的设备同步到主机当前的版本。
    pub async fn sync_version(self: Arc<Self>, id: String) -> Result<()> {
        let peer = self
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .context("没有这台设备")?;
        let current = self
            .status
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|s| s.version.clone());
        if !update::needs_update(current.as_deref(), roam_proto::VERSION) {
            self.record_update(&peer.name, "已是当前或更高版本，无需同步");
            self.updater.save_results();
            return Ok(());
        }
        {
            let mut updating = self.updating.lock().unwrap();
            if *updating {
                bail!("正在更新中，请等待当前任务完成");
            }
            *updating = true;
        }
        self.updater.set_error(None);
        let result = async {
            let release = self.updater.release_for(roam_proto::VERSION).await?;
            self.push_release(&peer, &release, self.mirror().as_deref())
                .await
        }
        .await;
        *self.updating.lock().unwrap() = false;
        self.updater.set_phase(None);
        match &result {
            Ok(()) => self.record_update(&peer.name, "已同步"),
            Err(err) => {
                self.record_update(&peer.name, &format!("同步失败：{err:#}"));
                self.updater
                    .set_error(Some(format!("同步「{}」失败：{err:#}", peer.name)));
            }
        }
        self.updater.save_results();
        result
    }

    fn update_status(
        &self,
        id: &str,
        conn: &iroh::endpoint::Connection,
        version: &str,
        health: &Mutex<LinkHealth>,
    ) {
        let path = selected_path(conn);
        let health = health.lock().unwrap().clone();
        self.status.lock().unwrap().insert(
            id.to_string(),
            PeerStatus {
                online: true,
                path: path.as_ref().map(|p| p.0),
                rtt_ms: path.map(|p| p.2.as_secs_f64() * 1000.0),
                version: Some(version.to_string()),
                stalled: health.stalled(),
                drops: health.drops,
                longest_drop_ms: health.longest.as_millis() as u64,
                ..Default::default()
            },
        );
    }
}

/// 没有光标控制能力的平台（非 macOS 主机，仅用于布局预览和演示）。
#[cfg(not(any(target_os = "macos", windows)))]
pub struct NoopCursor;

#[cfg(not(any(target_os = "macos", windows)))]
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

/// 任务句柄被丢弃时顺带结束任务（外层任务被 abort 时，子任务也跟着结束）。
struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(10)]
}

#[cfg(test)]
mod file_send_tests {
    use super::*;
    use crate::sync::{Inbound, LastDigest};
    use iroh::{EndpointAddr, endpoint::presets};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct NoCursor;
    impl CursorControl for NoCursor {
        fn hide(&mut self) {}
        fn show(&mut self) {}
        fn warp(&mut self, _: f64, _: f64) {}
        fn set_locked(&mut self, _: bool) {}
    }

    async fn endpoint() -> Endpoint {
        Endpoint::builder(presets::Minimal)
            .alpns(vec![roam_proto::ALPN.to_vec()])
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap()
    }

    async fn pair(a: &Endpoint, b: &Endpoint) -> (Connection, Connection) {
        let addr = EndpointAddr::new(b.id()).with_ip_addr(b.bound_sockets()[0]);
        let (a, b) = tokio::join!(a.connect(addr, roam_proto::ALPN), async {
            b.accept().await.unwrap().await.unwrap()
        });
        (a.unwrap(), b)
    }

    fn inbound(host: Option<Arc<Host>>, transfers: Transfers, limit: Arc<AtomicU64>) -> Inbound {
        Inbound {
            host,
            peer: "test".into(),
            // 主动发送应独立于剪贴板开关，且不能调用剪贴板回调。
            policy: Arc::new(move || ClipboardPolicy {
                max_file_bytes: limit.load(Ordering::Relaxed),
                ..Default::default()
            }),
            on_policy: None,
            last: LastDigest::default(),
            transfers,
            on_updated: Arc::new(|| panic!("explicit send changed clipboard")),
            on_announce: Arc::new(|_| panic!("unexpected clipboard announcement")),
            on_pull: Arc::new(|_| panic!("unexpected clipboard pull")),
            on_files: Arc::new(|_, _| panic!("explicit send changed clipboard")),
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[tokio::test]
    async fn receiver_keyboard_policy_crosses_connection_and_can_be_disabled() {
        use roam_core::{
            layout::{Cell, DeviceGeom, World},
            router::InputEvent,
            settings::{KeyRemap, KeyboardProfile, KeyboardSettings},
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            let root = std::env::temp_dir().join(format!("roam-keyboard-{}", files::new_id()));
            let (h, c) = (endpoint().await, endpoint().await);
            let (hc, ch) = pair(&h, &c).await;
            let host = Host::new(
                Store::open(Some(root.clone())).unwrap(),
                h.clone(),
                Box::new(NoCursor),
            )
            .unwrap();
            let id = c.id().to_string();
            let os = if Os::current() == Os::MacOs {
                Os::Windows
            } else {
                Os::MacOs
            };
            host.config
                .lock()
                .unwrap()
                .peers
                .push(roam_core::config::Peer {
                    id: id.clone(),
                    name: "receiver".into(),
                    ticket: String::new(),
                    os: Some(os),
                    displays: vec![],
                    settings: Default::default(),
                });
            let profile = |to| KeyboardProfile {
                enabled: true,
                mappings: vec![KeyRemap { from: 4, to }],
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            {
                let mut r = host.router.lock().unwrap();
                r.set_world(World {
                    devices: [host.host_id.clone(), id.clone()]
                        .into_iter()
                        .enumerate()
                        .map(|(col, id)| DeviceGeom {
                            id,
                            displays: vec![roam_proto::Rect {
                                x: 0.0,
                                y: 0.0,
                                w: 1000.0,
                                h: 800.0,
                            }],
                            cells: vec![Cell {
                                col: col as i32,
                                row: 0,
                            }],
                        })
                        .collect(),
                });
                r.add_peer(id.clone(), PeerLink { os, tx });
                r.set_keyboard_settings(KeyboardSettings {
                    mac_to_windows: profile(5),
                    windows_to_mac: profile(5),
                });
            }
            let task = tokio::spawn(sync::receive_loop_in(
                hc,
                inbound(
                    Some(host.clone()),
                    host.transfers.clone(),
                    Arc::new(AtomicU64::new(1 << 20)),
                ),
                root.join("receive"),
            ));
            for (settings, expected) in [(profile(6), 6), (KeyboardProfile::default(), 5)] {
                host.router.lock().unwrap().handle(InputEvent::Move {
                    x: 999.0,
                    y: 400.0,
                    dx: 5.0,
                    dy: 0.0,
                });
                assert!(host.router.lock().unwrap().focused().is_some());
                crate::clipboard::send_side(&ch, roam_proto::side::KEYBOARD_MAPPING, &settings)
                    .await
                    .unwrap();
                while host.router.lock().unwrap().focused().is_some() {
                    tokio::task::yield_now().await;
                }
                while rx.try_recv().is_ok() {}
                let mut r = host.router.lock().unwrap();
                r.handle(InputEvent::Move {
                    x: 999.0,
                    y: 400.0,
                    dx: 5.0,
                    dy: 0.0,
                });
                r.handle(InputEvent::Key {
                    usage: 4,
                    down: true,
                });
                assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|m| m
                    == Outbound::Stream(StreamMsg::Key {
                        usage: expected,
                        down: true
                    })));
                r.return_home();
            }
            task.abort();
            h.close().await;
            c.close().await;
            drop(host);
            std::fs::remove_dir_all(root).unwrap();
        })
        .await
        .expect("keyboard policy timed out");
    }

    #[tokio::test]
    async fn sends_multiselect_through_host_and_confirms_final_save() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let root = std::env::temp_dir().join(format!("roam-direct-{}", files::new_id()));
            let store = Store::open(Some(root.join("host"))).unwrap();
            let (a, h, b) = (endpoint().await, endpoint().await, endpoint().await);
            let (ah, ha) = pair(&a, &h).await;
            let (hb, bh) = pair(&h, &b).await;
            let host = Host::new(store, h.clone(), Box::new(NoCursor)).unwrap();
            for (endpoint, conn) in [(&a, ha.clone()), (&b, hb.clone())] {
                let id = endpoint.id().to_string();
                host.config
                    .lock()
                    .unwrap()
                    .peers
                    .push(roam_core::config::Peer {
                        id: id.clone(),
                        ticket: String::new(),
                        name: id.clone(),
                        os: None,
                        displays: vec![],
                        settings: Default::default(),
                    });
                host.status.lock().unwrap().insert(
                    id.clone(),
                    PeerStatus {
                        online: true,
                        version: Some(roam_proto::VERSION.into()),
                        ..Default::default()
                    },
                );
                host.conns.lock().unwrap().insert(id, conn);
            }
            let limit = Arc::new(AtomicU64::new(1 << 24));
            let transfers = Transfers::default();
            let receive = Transfers::default();
            let tasks = [
                tokio::spawn(sync::receive_loop_in(
                    ah.clone(),
                    inbound(None, transfers.clone(), limit.clone()),
                    root.join("a"),
                )),
                tokio::spawn(sync::receive_loop_in(
                    ha,
                    inbound(Some(host.clone()), host.transfers.clone(), limit.clone()),
                    root.join("h"),
                )),
                tokio::spawn(sync::receive_loop_in(
                    hb,
                    inbound(Some(host.clone()), host.transfers.clone(), limit.clone()),
                    root.join("h"),
                )),
                tokio::spawn(sync::receive_loop_in(
                    bh,
                    inbound(None, receive.clone(), limit.clone()),
                    root.join("b"),
                )),
            ];
            let targets = transfers.targets(&ah).await.unwrap();
            assert!(targets.iter().any(|t| t.id == h.id().to_string()));
            assert!(
                targets
                    .iter()
                    .any(|t| t.id == b.id().to_string() && t.ready)
            );
            assert!(!targets.iter().any(|t| t.id == a.id().to_string()));
            let source = root.join("source");
            std::fs::create_dir_all(source.join("项目/空目录")).unwrap();
            std::fs::create_dir_all(root.join("b")).unwrap();
            std::fs::write(source.join("项目/中文 空格 ' $().txt"), vec![42; 600_000]).unwrap();
            std::fs::write(source.join("项目/empty.txt"), b"").unwrap();
            std::fs::write(source.join("项目/packet"), vec![5; 300_000]).unwrap();
            std::fs::write(source.join("项目/packet.part"), b"keep separate").unwrap();
            std::fs::write(source.join("报告.txt"), b"new").unwrap();
            std::fs::write(root.join("b/报告.txt"), b"original").unwrap();
            let paths = vec![source.join("项目"), source.join("报告.txt")];
            files::send_direct(
                &ah,
                paths.clone(),
                "b",
                1 << 24,
                &transfers,
                Some(b.id().to_string()),
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(root.join("b/报告.txt")).unwrap(), b"original");
            assert_eq!(std::fs::read(root.join("b/报告 (1).txt")).unwrap(), b"new");
            assert_eq!(
                std::fs::read(root.join("b/项目/中文 空格 ' $().txt")).unwrap(),
                vec![42; 600_000]
            );
            assert!(root.join("b/项目/空目录").is_dir());
            assert!(root.join("b/项目/empty.txt").is_file());
            assert_eq!(
                std::fs::read(root.join("b/项目/packet")).unwrap(),
                vec![5; 300_000]
            );
            assert_eq!(
                std::fs::read(root.join("b/项目/packet.part")).unwrap(),
                b"keep separate"
            );
            assert_eq!(transfers.list()[0].state, "done");
            assert_eq!(receive.list()[0].state, "done");
            let cache = host
                .store
                .dir()
                .join("file-forward")
                .join(a.id().to_string());
            assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
            assert!(
                files::send_direct(
                    &ah,
                    paths.clone(),
                    "missing",
                    1 << 24,
                    &transfers,
                    Some("missing".into())
                )
                .await
                .is_err()
            );
            limit.store(1, Ordering::Relaxed);
            assert!(
                files::send_direct(&ah, paths, "h", 1 << 24, &transfers, None)
                    .await
                    .is_err()
            );
            assert_eq!(transfers.list()[0].state, "failed");
            for task in tasks {
                task.abort();
            }
            a.close().await;
            h.close().await;
            b.close().await;
            drop(host);
            std::fs::remove_dir_all(root).unwrap();
        })
        .await
        .expect("local file transfer timed out");
    }
}
