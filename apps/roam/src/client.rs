//! 被控端：等待主机连接，把收到的键鼠注入本机。

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use roam_core::{config::Store, net::selected_path, ticket};
use roam_input::Injector;
use roam_proto::{ClipboardData, ClipboardPolicy, Datagram, Hello, Os, StreamMsg, side};

use crate::{
    clipboard,
    conn::{self, read_msg, write_msg},
    dialog, discovery, pairing,
    tray::{self, TrayHandle},
};

pub async fn run(store: Store, tray: Option<TrayHandle>) -> Result<()> {
    if !roam_input::CAN_INJECT {
        bail!("被控端目前只支持 Windows，macOS / Linux 被控端在后续版本中加入。");
    }
    #[cfg(windows)]
    roam_input::windows::init_dpi();

    let config = store.load()?;
    let endpoint = conn::bind(&store).await?;
    let my_id = endpoint.id().to_string();
    let code = ticket::encode(&endpoint.addr())?;
    std::fs::write(store.dir().join("ticket.txt"), &code)?;

    // 局域网广播，主机的布局页面里会出现在"附近的设备"
    let _advertiser = match discovery::advertise(&my_id, &config.name, &code) {
        Ok(a) => Some(a),
        Err(err) => {
            eprintln!("⚠ 局域网广播失败（主机将无法自动发现本机，仍可用连接码添加）: {err:#}");
            None
        }
    };

    println!("\nRoam 被控端 · {}", config.name);
    println!("本机 ID: {my_id}");
    println!("\n同一局域网内的主机会自动发现本机；跨网络时在主机上粘贴连接码：\n");
    println!("{code}\n");
    println!("等待主机连接...");

    let status = Status::new(tray);
    status.waiting();

    let injector = Arc::new(Mutex::new(Injector::new()));
    // 同一时间只弹一个授权窗口
    let prompt_lock = Arc::new(tokio::sync::Mutex::new(()));

    while let Some(incoming) = endpoint.accept().await {
        let store = store.clone();
        let injector = injector.clone();
        let prompt_lock = prompt_lock.clone();
        let status = status.clone();
        let my_id = my_id.clone();
        let name = config.name.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!("连接握手失败: {err:#}");
                    return;
                }
            };
            let ctx = SessionCtx {
                store,
                my_id,
                name,
                prompt_lock,
                status: status.clone(),
            };
            if let Err(err) = session(conn, injector.clone(), ctx).await {
                eprintln!("会话结束: {err:#}");
            }
            injector.lock().unwrap().release_all();
            status.waiting();
        });
    }
    Ok(())
}

struct SessionCtx {
    store: Store,
    my_id: String,
    name: String,
    prompt_lock: Arc<tokio::sync::Mutex<()>>,
    status: Status,
}

/// 已信任的主机直接放行；陌生主机弹窗询问，并显示校验码供核对。
async fn authorize(ctx: &SessionCtx, host_id: &str, host_name: &str) -> Result<bool> {
    let _guard = ctx.prompt_lock.lock().await;
    if ctx.store.load()?.trusted_hosts.iter().any(|h| h == host_id) {
        return Ok(true);
    }
    let code = pairing::verification_code(host_id, &ctx.my_id);
    println!("主机「{host_name}」请求控制本机，校验码 {code}");
    let message = format!(
        "主机「{host_name}」请求控制本机的键盘和鼠标。\n\n\
         校验码：{code}\n\
         请确认主机布局页面上显示的校验码与此一致。\n\n\
         允许并记住这台主机吗？"
    );
    let allowed =
        tokio::task::spawn_blocking(move || dialog::confirm("Roam · 连接请求", &message)).await?;
    if !allowed {
        println!("已拒绝。");
        return Ok(false);
    }
    let mut config = ctx.store.load()?;
    if !config.trusted_hosts.iter().any(|h| h == host_id) {
        config.trusted_hosts.push(host_id.to_string());
        ctx.store.save(&config)?;
    }
    println!("已允许，之后这台主机连接时不再询问。");
    Ok(true)
}

async fn session(conn: Connection, injector: Arc<Mutex<Injector>>, ctx: SessionCtx) -> Result<()> {
    let host_id = conn.remote_id().to_string();
    let (mut send, mut recv) = conn.accept_bi().await?;
    let StreamMsg::Hello(host) = read_msg(&mut recv).await? else {
        bail!("主机没有先发送 Hello");
    };
    if !authorize(&ctx, &host_id, &host.name).await? {
        conn.close(1u32.into(), b"denied");
        // 给关闭帧一点时间发出去
        tokio::time::sleep(Duration::from_millis(200)).await;
        return Ok(());
    }
    write_msg(
        &mut send,
        &StreamMsg::Hello(Hello {
            name: ctx.name.clone(),
            os: Os::current(),
            displays: roam_input::displays(),
        }),
    )
    .await?;

    println!("✓ 主机「{}」已连接", host.name);
    injector.lock().unwrap().reset_sequence();

    // 连接刚建立时通常先走中继，打洞成功后切到直连；定期刷新托盘状态
    let path_conn = conn.clone();
    let path_status = ctx.status.clone();
    let host_name = host.name.clone();
    let path_task = tokio::spawn(async move {
        let mut last = None;
        loop {
            let now = selected_path(&path_conn);
            if let Some((kind, addr, rtt)) = &now {
                if last != Some(*kind) {
                    println!("  路径 → {} {addr}", kind.label());
                    last = Some(*kind);
                }
                path_status.connected(&host_name, kind.label(), rtt.as_secs_f64() * 1000.0);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    // 剪贴板：主机发来的内容写入本机；光标离开时把本机剪贴板发回主机
    let clip = ClipState::default();
    let side_task = {
        let conn = conn.clone();
        let clip = clip.clone();
        tokio::spawn(async move { receive_side(conn, clip).await })
    };

    // 鼠标移动走数据报，单独一个任务处理
    let dgram_conn = conn.clone();
    let dgram_injector = injector.clone();
    let dgram_task = tokio::spawn(async move {
        while let Ok(bytes) = dgram_conn.read_datagram().await {
            match Datagram::decode(&bytes) {
                Ok(Datagram::MouseAbs { seq, x, y }) => {
                    dgram_injector.lock().unwrap().mouse_abs(seq, x, y)
                }
                Ok(Datagram::Ping { seq, sent_us }) => {
                    let pong = Datagram::Pong { seq, sent_us }.encode().expect("encode");
                    let _ = dgram_conn.send_datagram(pong.into());
                }
                _ => {}
            }
        }
    });

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
                let clip = clip.clone();
                tokio::spawn(async move { send_clipboard(&conn, &clip).await });
            }
            {
                let mut inj = injector.lock().unwrap();
                match msg {
                    StreamMsg::Enter { x, y } | StreamMsg::MouseTo { x, y } => inj.move_to(x, y),
                    StreamMsg::Leave | StreamMsg::ReleaseAll => inj.release_all(),
                    StreamMsg::Key { usage, down } => inj.key(usage, down),
                    StreamMsg::Button { button, down } => inj.button(button, down),
                    StreamMsg::WheelLines { dx, dy } => inj.wheel_lines(dx, dy),
                    StreamMsg::WheelPixels { dx, dy } => inj.wheel_pixels(dx, dy),
                    StreamMsg::KeyState { pressed } => inj.sync_keys(&pressed),
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
    path_task.abort();
    side_task.abort();
    println!("主机「{}」已断开", host.name);
    result
}

/// 剪贴板同步状态（每个会话一份）。
#[derive(Clone, Default)]
struct ClipState {
    /// 主机下发的策略；老版本主机不会下发，默认不同步。
    policy: Arc<Mutex<ClipboardPolicy>>,
    /// 最近一次收到或发出的内容摘要，没变就不重复发送。
    last_digest: Arc<Mutex<Option<u64>>>,
}

async fn receive_side(conn: Connection, clip: ClipState) {
    while let Ok(mut stream) = conn.accept_uni().await {
        let max = clip.policy.lock().unwrap().max_bytes.max(64 * 1024) + 1024;
        let Ok((kind, body)) = clipboard::read_side(&mut stream, max).await else {
            continue;
        };
        match kind {
            side::CLIPBOARD_POLICY => {
                if let Ok(policy) = clipboard::decode::<ClipboardPolicy>(&body) {
                    *clip.policy.lock().unwrap() = policy;
                }
            }
            side::CLIPBOARD => {
                if !clip.policy.lock().unwrap().enabled {
                    continue;
                }
                let Ok(data) = clipboard::decode::<ClipboardData>(&body) else {
                    continue;
                };
                let digest = data.digest();
                println!("  剪贴板 ← 主机（{} 字节）", data.size());
                let written = tokio::task::spawn_blocking(move || clipboard::write(&data)).await;
                if matches!(written, Ok(Ok(()))) {
                    *clip.last_digest.lock().unwrap() = Some(digest);
                }
            }
            _ => {}
        }
    }
}

async fn send_clipboard(conn: &Connection, clip: &ClipState) {
    let policy = *clip.policy.lock().unwrap();
    if !policy.enabled {
        return;
    }
    let Ok(Some(data)) = tokio::task::spawn_blocking(move || clipboard::read(&policy)).await else {
        return;
    };
    let digest = data.digest();
    if *clip.last_digest.lock().unwrap() == Some(digest) {
        return;
    }
    let size = data.size();
    match clipboard::send_side(conn, side::CLIPBOARD, &data).await {
        Ok(()) => {
            println!("  剪贴板 → 主机（{size} 字节）");
            *clip.last_digest.lock().unwrap() = Some(digest);
        }
        Err(err) => eprintln!("同步剪贴板失败: {err:#}"),
    }
}

/// 被控端状态，同步到托盘。
#[derive(Clone)]
struct Status {
    tray: Option<TrayHandle>,
}

impl Status {
    fn new(tray: Option<TrayHandle>) -> Self {
        Self { tray }
    }

    fn waiting(&self) {
        self.set("等待主机连接".into());
    }

    fn connected(&self, host: &str, path: &str, rtt_ms: f64) {
        self.set(format!("已连接「{host}」· {path} · {rtt_ms:.1} ms"));
    }

    fn set(&self, status: String) {
        if let Some(tray) = &self.tray {
            tray.update(tray::State {
                title: None,
                tooltip: format!("Roam 被控端 · {status}"),
                status,
                paused: false,
            });
        }
    }
}
