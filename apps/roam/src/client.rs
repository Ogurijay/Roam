//! 被控端：等待主机连接，把收到的键鼠注入本机。

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use roam_core::{
    config::Store,
    health::{DOWN_AFTER, HEARTBEAT, LinkEvent, LinkHealth, STALL_AFTER, TIMER_LATE},
    net::{describe_paths, selected_path},
    ticket,
};
use roam_input::Injector;
use roam_proto::{ClipboardPolicy, Datagram, Hello, Os, StreamMsg};
use serde::Serialize;

use crate::{
    conn::{self, read_msg, write_msg},
    dialog, discovery,
    files::{Transfers, human_bytes},
    pairing,
    sync::{self, Inbound, LastDigest},
    tray::{self, TrayHandle},
};

/// 当前连接的主机（界面显示用）。
#[derive(Debug, Clone, Serialize)]
pub struct ConnectedHost {
    pub id: String,
    pub name: String,
    pub os: Os,
    pub version: String,
    pub path: Option<&'static str>,
    pub rtt_ms: Option<f64>,
    /// 超过几秒没收到主机的数据。
    pub stalled: bool,
    /// 本次连接里网络中断的次数和最长一次（毫秒）。
    pub drops: u32,
    pub longest_drop_ms: u64,
}

/// 被控端的运行状态，界面和托盘共用。
pub struct Client {
    pub store: Store,
    pub id: String,
    pub ticket: String,
    pub transfers: Transfers,
    pub host: Mutex<Option<ConnectedHost>>,
    /// 正在等待本机确认的授权请求：主机名和校验码。
    pub pending: Mutex<Option<(String, String)>>,
    /// 每台主机当前的连接：同一台主机重连时关掉旧连接，避免两个会话同时注入、同时同步剪贴板。
    sessions: Mutex<std::collections::HashMap<String, Connection>>,
    /// 本机注入键鼠所需的系统权限是否就绪；失败时为说明。
    pub permission: Result<(), String>,
    tray: Mutex<Option<TrayHandle>>,
    _advertiser: Option<discovery::Advertiser>,
}

impl Client {
    pub async fn start(store: Store) -> Result<Arc<Self>> {
        if !roam_input::CAN_INJECT {
            bail!("这个系统暂不支持作为被控端");
        }
        #[cfg(windows)]
        roam_input::windows::init_dpi();

        let permission = check_permission();
        if let Err(err) = &permission {
            tracing::warn!("{err}");
        }

        let config = store.load()?;
        let endpoint = conn::bind(&store, Some(conn::CLIENT_PORT)).await?;
        let id = endpoint.id().to_string();
        let code = ticket::encode(&endpoint.addr())?;
        std::fs::write(store.dir().join("ticket.txt"), &code)?;

        // 局域网广播，主机的界面里会出现在"附近的设备"
        let advertiser = match discovery::advertise(&id, &config.name, &code) {
            Ok(a) => Some(a),
            Err(err) => {
                tracing::warn!("局域网广播失败（主机无法自动发现本机，仍可用连接码添加）: {err:#}");
                None
            }
        };
        tracing::info!("被控端「{}」已启动，等待主机连接", config.name);
        tracing::info!("连接码：{code}");

        let client = Arc::new(Self {
            store,
            id,
            ticket: code,
            transfers: Transfers::default(),
            host: Mutex::new(None),
            pending: Mutex::new(None),
            sessions: Mutex::new(std::collections::HashMap::new()),
            permission,
            tray: Mutex::new(None),
            _advertiser: advertiser,
        });
        client.refresh_tray();
        tokio::spawn(client.clone().accept_loop(endpoint));
        Ok(client)
    }

    pub fn set_tray(&self, tray: TrayHandle) {
        *self.tray.lock().unwrap() = Some(tray);
        self.refresh_tray();
    }

    /// 托盘上显示的状态。
    pub fn status_text(&self) -> String {
        if let Some((name, code)) = self.pending.lock().unwrap().clone() {
            return format!("「{name}」请求控制本机，校验码 {code}");
        }
        let transfer = self.transfers.active().map(|t| {
            let pct = (t.done * 100).checked_div(t.total).unwrap_or(100);
            let arrow = if t.direction == "send" {
                "发送"
            } else {
                "接收"
            };
            format!("{arrow}「{}」{pct}%（{}）", t.title, human_bytes(t.total))
        });
        match (self.host.lock().unwrap().clone(), transfer) {
            (Some(h), _) if h.stalled => format!("主机「{}」暂时没有响应", h.name),
            (Some(h), Some(t)) => format!("已被「{}」控制 · {t}", h.name),
            (Some(h), None) => match (h.path, h.rtt_ms) {
                (Some(path), Some(rtt)) => format!("已被「{}」控制 · {path} · {rtt:.1} ms", h.name),
                _ => format!("已被「{}」控制", h.name),
            },
            (None, _) => "等待主机连接".into(),
        }
    }

    fn refresh_tray(&self) {
        if let Some(tray) = &*self.tray.lock().unwrap() {
            let status = self.status_text();
            tray.update(tray::State {
                title: None,
                tooltip: format!("Roam 被控端 v{} · {status}", roam_proto::VERSION),
                status,
                paused: false,
                update_label: None,
            });
        }
    }

    async fn accept_loop(self: Arc<Self>, endpoint: iroh::Endpoint) {
        let injector = Arc::new(Mutex::new(Sink::new()));
        // 同一时间只弹一个授权窗口
        let prompt_lock = Arc::new(tokio::sync::Mutex::new(()));
        let addr_endpoint = endpoint.clone();

        while let Some(incoming) = endpoint.accept().await {
            let client = self.clone();
            let injector = injector.clone();
            let prompt_lock = prompt_lock.clone();
            let ticket = ticket::encode(&addr_endpoint.addr()).unwrap_or_default();
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(conn) => conn,
                    Err(err) => {
                        // 首包解密失败：通常是公网扫描器碰到了路由器映射出去的端口，不是真正的主机
                        if err.to_string().contains("authentication failed") {
                            tracing::debug!("忽略无法解密的连接请求: {err:#}");
                        } else {
                            tracing::warn!("连接握手失败: {err:#}");
                        }
                        return;
                    }
                };
                // 更新通道：只接受已授权主机推送的新版本
                if conn.alpn() == roam_proto::UPDATE_ALPN {
                    let host_id = conn.remote_id().to_string();
                    let trusted = client
                        .store
                        .load()
                        .map(|c| c.trusted_hosts.contains(&host_id))
                        .unwrap_or(false);
                    tracing::info!("正在接收主机推送的新版本…");
                    injector.lock().unwrap().release_all();
                    let staging = client.store.dir().to_path_buf();
                    if let Err(err) = crate::update::receive(conn, trusted, staging).await {
                        tracing::warn!("更新失败: {err:#}");
                    }
                    return;
                }
                if let Err(err) = client
                    .session(conn, injector.clone(), prompt_lock, ticket)
                    .await
                {
                    tracing::info!("会话结束: {err:#}");
                }
                // 被新连接替换掉的旧会话结束时，不要清掉新会话的状态
                if client.sessions.lock().unwrap().is_empty() {
                    injector.lock().unwrap().release_all();
                    *client.host.lock().unwrap() = None;
                }
                client.refresh_tray();
            });
        }
    }

    /// 已信任的主机直接放行；陌生主机弹窗询问，并显示校验码供核对。
    async fn authorize(
        &self,
        host_id: &str,
        host_name: &str,
        prompt_lock: &tokio::sync::Mutex<()>,
    ) -> Result<bool> {
        let _guard = prompt_lock.lock().await;
        if self
            .store
            .load()?
            .trusted_hosts
            .iter()
            .any(|h| h == host_id)
        {
            return Ok(true);
        }
        let code = pairing::verification_code(host_id, &self.id);
        tracing::info!("主机「{host_name}」请求控制本机，校验码 {code}");
        *self.pending.lock().unwrap() = Some((host_name.to_string(), code.clone()));
        self.refresh_tray();
        let message = format!(
            "主机「{host_name}」请求控制本机的键盘和鼠标。\n\n\
             校验码：{code}\n\
             请确认主机界面上显示的校验码与此一致。\n\n\
             允许并记住这台主机吗？"
        );
        let allowed =
            tokio::task::spawn_blocking(move || dialog::confirm("Roam · 连接请求", &message))
                .await?;
        *self.pending.lock().unwrap() = None;
        self.refresh_tray();
        if !allowed {
            tracing::info!("已拒绝「{host_name}」");
            return Ok(false);
        }
        let mut config = self.store.load()?;
        if !config.trusted_hosts.iter().any(|h| h == host_id) {
            config.trusted_hosts.push(host_id.to_string());
        }
        config
            .host_names
            .insert(host_id.to_string(), host_name.to_string());
        self.store.save(&config)?;
        tracing::info!("已允许「{host_name}」，之后连接时不再询问");
        Ok(true)
    }

    /// 撤销对某台主机的授权（界面里调用）。
    pub fn revoke(&self, host_id: &str) -> Result<()> {
        let mut config = self.store.load()?;
        config.trusted_hosts.retain(|h| h != host_id);
        config.host_names.remove(host_id);
        self.store.save(&config)
    }

    async fn session(
        self: &Arc<Self>,
        conn: Connection,
        injector: Arc<Mutex<Sink>>,
        prompt_lock: Arc<tokio::sync::Mutex<()>>,
        my_ticket: String,
    ) -> Result<()> {
        let host_id = conn.remote_id().to_string();
        let (mut send, mut recv) = conn.accept_bi().await?;
        let StreamMsg::Hello(host) = read_msg(&mut recv).await? else {
            bail!("主机没有先发送 Hello");
        };
        if !self.authorize(&host_id, &host.name, &prompt_lock).await? {
            conn.close(1u32.into(), b"denied");
            // 给关闭帧一点时间发出去
            tokio::time::sleep(Duration::from_millis(200)).await;
            return Ok(());
        }
        let name = self.store.load()?.name;
        write_msg(
            &mut send,
            &StreamMsg::Hello(Hello {
                name,
                os: Os::current(),
                displays: roam_input::displays(),
                version: roam_proto::VERSION.into(),
                ticket: my_ticket,
            }),
        )
        .await?;

        if let Some(old) = self
            .sessions
            .lock()
            .unwrap()
            .insert(host_id.clone(), conn.clone())
        {
            tracing::info!("主机「{}」重新连接，关闭旧连接", host.name);
            old.close(0u32.into(), b"replaced");
        }
        tracing::info!("主机「{}」已连接", host.name);
        *self.host.lock().unwrap() = Some(ConnectedHost {
            id: host_id.clone(),
            name: host.name.clone(),
            os: host.os,
            version: host.version.clone(),
            path: None,
            rtt_ms: None,
            stalled: false,
            drops: 0,
            longest_drop_ms: 0,
        });
        self.refresh_tray();
        injector.lock().unwrap().reset_sequence();
        // 主机定期发心跳（老版本主机不发，那就不做判断）
        let health = Arc::new(Mutex::new(LinkHealth::default()));

        // 连接刚建立时通常先走中继，打洞成功后切到直连；定期刷新状态
        let path_task = {
            let client = self.clone();
            let conn = conn.clone();
            let health = health.clone();
            tokio::spawn(async move {
                let mut last = None;
                loop {
                    if let Some((kind, addr, rtt)) = selected_path(&conn) {
                        if last != Some(kind) {
                            tracing::info!(
                                "连接路径：{} {addr}（{}）",
                                kind.label(),
                                describe_paths(&conn)
                            );
                            last = Some(kind);
                        }
                        let health = health.lock().unwrap().clone();
                        if let Some(h) = &mut *client.host.lock().unwrap() {
                            h.path = Some(kind.label());
                            h.rtt_ms = Some(rtt.as_secs_f64() * 1000.0);
                            h.stalled = health.stalled();
                            h.drops = health.drops;
                            h.longest_drop_ms = health.longest.as_millis() as u64;
                        }
                        client.refresh_tray();
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })
        };

        // 检查主机的心跳：长时间收不到就松开所有按键，免得卡键；中断情况写进日志
        let watch_task = {
            let conn = conn.clone();
            let health = health.clone();
            let injector = injector.clone();
            let name = host.name.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(HEARTBEAT);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut late_logged: Option<Instant> = None;
                loop {
                    let scheduled = tick.tick().await.into_std();
                    let now = Instant::now();
                    let late = now.saturating_duration_since(scheduled);
                    if late >= TIMER_LATE
                        && late_logged.is_none_or(|t| now - t > Duration::from_secs(60))
                    {
                        tracing::warn!(
                            "心跳定时器晚了 {:.1} 秒：本机 Roam 被系统节流（App Nap / 节能模式）或者卡住了",
                            late.as_secs_f64()
                        );
                        late_logged = Some(now);
                    }
                    let event = health.lock().unwrap().check(now);
                    match event {
                        Some(LinkEvent::Down) => tracing::warn!(
                            "{:.0} 秒没有收到主机「{name}」的数据（{}）",
                            DOWN_AFTER.as_secs_f64(),
                            describe_paths(&conn)
                        ),
                        Some(LinkEvent::Stalled) => {
                            tracing::warn!(
                                "超过 {} 秒没有收到主机「{name}」的数据，松开所有按键（{}）",
                                STALL_AFTER.as_secs(),
                                describe_paths(&conn)
                            );
                            injector.lock().unwrap().release_all();
                        }
                        None => {}
                    }
                }
            })
        };

        // 剪贴板与文件：主机发来的内容写入本机；光标离开时把本机剪贴板发回主机。
        // 老版本主机不会下发策略，默认不同步。
        let policy = Arc::new(Mutex::new(ClipboardPolicy::default()));
        let last = LastDigest::default();
        let side_task = {
            let ctx = Inbound {
                peer: host.name.clone(),
                policy: {
                    let policy = policy.clone();
                    Arc::new(move || *policy.lock().unwrap())
                },
                on_policy: {
                    let policy = policy.clone();
                    Some(Arc::new(move |p| *policy.lock().unwrap() = p))
                },
                last: last.clone(),
                transfers: self.transfers.clone(),
                on_updated: Arc::new(|| {}),
            };
            tokio::spawn(sync::receive_loop(conn.clone(), ctx))
        };

        // 鼠标移动走数据报，单独一个任务处理
        let dgram_task = {
            let conn = conn.clone();
            let injector = injector.clone();
            let health = health.clone();
            let name = host.name.clone();
            tokio::spawn(async move {
                while let Ok(bytes) = conn.read_datagram().await {
                    let outage = health.lock().unwrap().heard(Instant::now());
                    if let Some(outage) = outage {
                        tracing::info!(
                            "恢复收到主机「{name}」的数据，中断了 {:.1} 秒（{}）",
                            outage.as_secs_f64(),
                            describe_paths(&conn)
                        );
                    }
                    match Datagram::decode(&bytes) {
                        Ok(Datagram::MouseAbs { seq, x, y }) => {
                            injector.lock().unwrap().mouse_abs(seq, x, y)
                        }
                        Ok(Datagram::Ping { seq, sent_us }) => {
                            let pong = Datagram::Pong { seq, sent_us }.encode().expect("encode");
                            let _ = conn.send_datagram(pong.into());
                        }
                        _ => {}
                    }
                }
            })
        };

        let result = async {
            loop {
                let msg = read_msg(&mut recv).await?;
                let reply = match &msg {
                    StreamMsg::Ping { seq, sent_us } => Some(StreamMsg::Pong {
                        seq: *seq,
                        sent_us: *sent_us,
                    }),
                    _ => None,
                };
                if matches!(msg, StreamMsg::Leave) {
                    let conn = conn.clone();
                    let policy = *policy.lock().unwrap();
                    let last = last.clone();
                    let transfers = self.transfers.clone();
                    let host_name = host.name.clone();
                    tokio::spawn(async move {
                        if let Err(err) =
                            sync::push(&conn, policy, &last, &host_name, &transfers).await
                        {
                            tracing::warn!("同步到主机失败: {err:#}");
                        }
                    });
                }
                {
                    let mut inj = injector.lock().unwrap();
                    match msg {
                        StreamMsg::Enter { x, y } | StreamMsg::MouseTo { x, y } => {
                            inj.move_to(x, y)
                        }
                        StreamMsg::Leave | StreamMsg::ReleaseAll => inj.release_all(),
                        StreamMsg::Key { usage, down } => inj.key(usage, down),
                        StreamMsg::Button { button, down } => inj.button(button, down),
                        StreamMsg::WheelLines { dx, dy } => inj.wheel_lines(dx, dy),
                        StreamMsg::WheelPixels { dx, dy } => inj.wheel_pixels(dx, dy),
                        StreamMsg::KeyState { pressed } => inj.sync_keys(&pressed),
                        StreamMsg::LockScreen => {
                            tracing::info!("主机「{}」请求锁定本机", host.name);
                            inj.lock_screen()
                        }
                        StreamMsg::Hello(_) | StreamMsg::Pong { .. } | StreamMsg::Ping { .. } => {}
                    }
                }
                if let Some(reply) = reply {
                    write_msg(&mut send, &reply).await?;
                }
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        }
        .await;

        dgram_task.abort();
        watch_task.abort();
        path_task.abort();
        side_task.abort();
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions
                .get(&host_id)
                .is_some_and(|c| c.stable_id() == conn.stable_id())
            {
                sessions.remove(&host_id);
            }
        }
        tracing::info!("主机「{}」已断开", host.name);
        result
    }
}

/// 注入键鼠需要的系统权限（macOS 需要「辅助功能」）。
fn check_permission() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return roam_input::mac::check_accessibility().map_err(|e| format!("{e:#}"));
    #[allow(unreachable_code)]
    Ok(())
}

/// 把收到的键鼠注入本机：平时由本进程注入；Windows 锁屏时（安全桌面）转交给系统服务的输入助手。
struct Sink {
    local: Injector,
    last_seq: Option<u32>,
    #[cfg(windows)]
    helper: crate::winsvc::HelperLink,
    #[cfg(windows)]
    secure: bool,
    #[cfg(windows)]
    checked: Option<std::time::Instant>,
}

impl Sink {
    fn new() -> Self {
        Self {
            local: Injector::new(),
            last_seq: None,
            #[cfg(windows)]
            helper: crate::winsvc::HelperLink::new(),
            #[cfg(windows)]
            secure: false,
            #[cfg(windows)]
            checked: None,
        }
    }

    /// 尝试交给输入助手；返回 false 表示应由本进程自己注入。
    #[cfg(windows)]
    fn via_helper(&mut self, msg: crate::winsvc::HelperMsg) -> bool {
        // 每 200 毫秒检查一次当前是不是安全桌面（锁屏 / UAC）
        if self
            .checked
            .is_none_or(|t| t.elapsed() > Duration::from_millis(200))
        {
            self.secure = crate::winsvc::secure_desktop_active();
            self.checked = Some(std::time::Instant::now());
        }
        self.secure && self.helper.send(&msg)
    }

    #[cfg(not(windows))]
    fn via_helper(&mut self, _msg: HelperMsgStub) -> bool {
        false
    }

    fn reset_sequence(&mut self) {
        self.last_seq = None;
    }

    /// 数据报通道的绝对移动：丢弃乱序到达的旧包。
    fn mouse_abs(&mut self, seq: u32, x: f32, y: f32) {
        if let Some(last) = self.last_seq
            && (seq.wrapping_sub(last) as i32) <= 0
        {
            return;
        }
        self.last_seq = Some(seq);
        self.move_to(x, y);
    }

    fn move_to(&mut self, x: f32, y: f32) {
        if !self.via_helper(helper_msg!(MoveTo { x, y })) {
            self.local.move_to(x, y);
        }
    }

    fn key(&mut self, usage: u16, down: bool) {
        if !self.via_helper(helper_msg!(Key { usage, down })) {
            self.local.key(usage, down);
        }
    }

    fn button(&mut self, button: roam_proto::MouseButton, down: bool) {
        if !self.via_helper(helper_msg!(Button { button, down })) {
            self.local.button(button, down);
        }
    }

    fn wheel_lines(&mut self, dx: i32, dy: i32) {
        if !self.via_helper(helper_msg!(WheelLines { dx, dy })) {
            self.local.wheel_lines(dx, dy);
        }
    }

    fn wheel_pixels(&mut self, dx: f32, dy: f32) {
        if !self.via_helper(helper_msg!(WheelPixels { dx, dy })) {
            self.local.wheel_pixels(dx, dy);
        }
    }

    fn sync_keys(&mut self, pressed: &[u16]) {
        self.local.sync_keys(pressed);
    }

    fn release_all(&mut self) {
        self.local.release_all();
        #[cfg(windows)]
        {
            self.helper.send(&crate::winsvc::HelperMsg::ReleaseAll);
        }
    }

    fn lock_screen(&mut self) {
        self.local.lock_screen();
    }
}

/// 构造发给输入助手的消息；非 Windows 平台没有助手，用占位类型。
#[cfg(windows)]
macro_rules! helper_msg {
    ($($t:tt)*) => { crate::winsvc::HelperMsg::$($t)* };
}
#[cfg(not(windows))]
macro_rules! helper_msg {
    ($($t:tt)*) => {
        HelperMsgStub
    };
}
use helper_msg;

#[cfg(not(windows))]
struct HelperMsgStub;
