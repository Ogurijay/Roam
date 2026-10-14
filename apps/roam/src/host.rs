//! 主机：采集本机键鼠，按布局路由到各台被控设备。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint,
    endpoint::{Connection, ConnectionError, VarInt},
};
use roam_core::{
    config::{Config, Peer, Store},
    layout::{Cell, DeviceGeom, HOST_CELL, World, default_cells, grid_key, nearest_free},
    net::{PathKind, selected_path},
    router::{CursorControl, Outbound, PeerLink, Router},
    ticket,
};
use roam_proto::{ALPN, ClipboardPolicy, DisplayInfo, Hello, Os, StreamMsg, side};
use tokio::{sync::mpsc, task::AbortHandle};

use crate::{
    clipboard,
    conn::{read_msg, write_msg},
    discovery::{self, Nearby},
    files::Transfers,
    pairing,
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
    _mdns: Option<mdns_sd::ServiceDaemon>,
    me: Mutex<std::sync::Weak<Host>>,
}

impl Host {
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
        let title = match (&focused_name, paused) {
            (_, true) => Some("⏸".to_string()),
            (Some(name), _) => Some(format!("→ {name}")),
            (None, _) => None,
        };
        let update_label = {
            let u = self.updater.state.lock().unwrap();
            match (&u.phase, &u.latest) {
                (Some(phase), _) => Some(phase.clone()),
                (None, Some(r)) if u.available => Some(format!("更新到 v{}", r.version)),
                _ => None,
            }
        };
        tray::State {
            title,
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
                && status.get(&p.id).is_some_and(|s| s.online)
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
        self.update_status(&peer.id, &conn, &hello.version);
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
                on_updated: Arc::new(move || {
                    let host = host.clone();
                    let id = id.clone();
                    rt.spawn(async move {
                        let focused = host.router.lock().unwrap().focused().map(str::to_string);
                        if let Some(other) = focused.filter(|f| *f != id) {
                            host.sync_clipboard_to(&other).await;
                        }
                    });
                }),
            };
            tokio::spawn(sync::receive_loop(conn.clone(), ctx))
        };

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
                _ = status_tick.tick() => self.update_status(&peer.id, &conn, &hello.version),
                reason = conn.closed() => break Err(anyhow::anyhow!("连接断开: {reason}")),
            }
        };
        reader.abort();
        side_task.abort();
        self.conns.lock().unwrap().remove(&peer.id);
        result
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

    /// 把主机剪贴板（文本、图片或文件）发给某台设备，内容没变就跳过。
    async fn sync_clipboard_to(&self, id: &str) {
        let policy = self.clipboard_policy(id);
        let Some(conn) = self.conns.lock().unwrap().get(id).cloned() else {
            return;
        };
        let name = self
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| short_id(id).to_string());
        let last = self.last_digest(id);
        if let Err(err) = sync::push(&conn, policy, &last, &name, &self.transfers).await {
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
        if self.updater.state.lock().unwrap().latest.is_none() {
            self.updater.set_phase(Some("正在检查更新…".into()));
            self.updater.check().await?;
        }
        let (release, available) = {
            let state = self.updater.state.lock().unwrap();
            (
                state.latest.clone().context("没有可用的版本")?,
                state.available,
            )
        };
        if !available {
            bail!("已经是最新版本");
        }
        let mirror = self.mirror();

        // 先下载本机的安装包，确保主机自己能更新成功再动被控端
        let own = self
            .updater
            .fetch(&release, Os::current(), mirror.as_deref())
            .await?;

        let peers: Vec<Peer> = self
            .config
            .lock()
            .unwrap()
            .peers
            .iter()
            .filter(|p| p.settings.enabled)
            .cloned()
            .collect();
        for peer in peers {
            let current = self
                .status
                .lock()
                .unwrap()
                .get(&peer.id)
                .and_then(|s| s.version.clone());
            if current.as_deref() == Some(release.version.as_str()) {
                self.record_update(&peer.name, "已是最新");
                continue;
            }
            let result = self.push_release(&peer, &release, mirror.as_deref()).await;
            self.record_update(
                &peer.name,
                &match result {
                    Ok(()) => "已更新".to_string(),
                    Err(err) => format!("{err:#}"),
                },
            );
        }

        self.updater.set_phase(Some(format!(
            "正在更新本机到 v{}，马上重启…",
            release.version
        )));
        self.router.lock().unwrap().return_home();
        update::install(&own)?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        update::restart();
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
        // 不知道对方系统时（从没连上过）按 Windows 处理，目前被控端只有 Windows
        let os = peer.os.unwrap_or(Os::Windows);
        let bin = self.updater.fetch(release, os, mirror).await?;
        self.updater
            .set_phase(Some(format!("正在推送到「{}」…", peer.name)));
        let addr = ticket::decode(&peer.ticket)?;
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
        self.updater.set_error(None);
        let result = async {
            let release = self.updater.release_for(roam_proto::VERSION).await?;
            self.push_release(&peer, &release, self.mirror().as_deref())
                .await
        }
        .await;
        self.updater.set_phase(None);
        match &result {
            Ok(()) => self.record_update(&peer.name, "已同步"),
            Err(err) => self
                .updater
                .set_error(Some(format!("同步「{}」失败：{err:#}", peer.name))),
        }
        result
    }

    fn update_status(&self, id: &str, conn: &iroh::endpoint::Connection, version: &str) {
        let path = selected_path(conn);
        self.status.lock().unwrap().insert(
            id.to_string(),
            PeerStatus {
                online: true,
                path: path.as_ref().map(|p| p.0),
                rtt_ms: path.map(|p| p.2.as_secs_f64() * 1000.0),
                version: Some(version.to_string()),
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

fn short_id(id: &str) -> &str {
    &id[..id.len().min(10)]
}
