//! 被控端：等待主机连接，把收到的键鼠注入本机。

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use roam_core::{config::Store, net::selected_path, ticket};
use roam_input::Injector;
use roam_proto::{ClipboardPolicy, Datagram, Hello, Os, StreamMsg};

use crate::{
    conn::{self, read_msg, write_msg},
    dialog, discovery,
    files::{Transfers, human_bytes},
    pairing,
    sync::{self, Inbound, LastDigest},
    tray::{self, TrayHandle},
};

pub async fn run(store: Store, tray: Option<TrayHandle>) -> Result<()> {
    if !roam_input::CAN_INJECT {
        bail!("被控端目前只支持 Windows，macOS / Linux 被控端在后续版本中加入。");
    }
    #[cfg(windows)]
    roam_input::windows::init_dpi();

    let config = store.load()?;
    let endpoint = conn::bind(&store, Some(conn::CLIENT_PORT)).await?;
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
    let transfers = Transfers::default();
    // 同一时间只弹一个授权窗口
    let prompt_lock = Arc::new(tokio::sync::Mutex::new(()));

    while let Some(incoming) = endpoint.accept().await {
        let store = store.clone();
        let injector = injector.clone();
        let prompt_lock = prompt_lock.clone();
        let status = status.clone();
        let transfers = transfers.clone();
        let endpoint_addr = endpoint.addr();
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
            // 更新通道：只接受已授权主机推送的新版本
            if conn.alpn() == roam_proto::UPDATE_ALPN {
                let host_id = conn.remote_id().to_string();
                let trusted = store
                    .load()
                    .map(|c| c.trusted_hosts.contains(&host_id))
                    .unwrap_or(false);
                status.set("正在接收新版本…".into());
                injector.lock().unwrap().release_all();
                if let Err(err) =
                    crate::update::receive(conn, trusted, store.dir().to_path_buf()).await
                {
                    eprintln!("更新失败: {err:#}");
                    status.waiting();
                }
                return;
            }
            let ctx = SessionCtx {
                store,
                my_id,
                name,
                prompt_lock,
                status: status.clone(),
                transfers: transfers.clone(),
                ticket: ticket::encode(&endpoint_addr).unwrap_or_default(),
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
    transfers: Transfers,
    ticket: String,
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
            version: roam_proto::VERSION.into(),
            ticket: ctx.ticket.clone(),
        }),
    )
    .await?;

    println!("✓ 主机「{}」已连接", host.name);
    injector.lock().unwrap().reset_sequence();

    // 连接刚建立时通常先走中继，打洞成功后切到直连；定期刷新托盘状态
    let path_conn = conn.clone();
    let path_status = ctx.status.clone();
    let path_transfers = ctx.transfers.clone();
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
                let transfer = path_transfers.active().map(|t| {
                    let pct = (t.done * 100).checked_div(t.total).unwrap_or(100);
                    let arrow = if t.direction == "send" {
                        "发送"
                    } else {
                        "接收"
                    };
                    format!("{arrow}「{}」{pct}%（{}）", t.title, human_bytes(t.total))
                });
                path_status.connected(
                    &host_name,
                    kind.label(),
                    rtt.as_secs_f64() * 1000.0,
                    transfer,
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

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
            transfers: ctx.transfers.clone(),
            on_updated: Arc::new(|| {}),
        };
        tokio::spawn(sync::receive_loop(conn.clone(), ctx))
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
                let policy = *policy.lock().unwrap();
                let last = last.clone();
                let transfers = ctx.transfers.clone();
                let host_name = host.name.clone();
                tokio::spawn(async move {
                    if let Err(err) = sync::push(&conn, policy, &last, &host_name, &transfers).await
                    {
                        eprintln!("同步到主机失败: {err:#}");
                    }
                });
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

    fn connected(&self, host: &str, path: &str, rtt_ms: f64, transfer: Option<String>) {
        match transfer {
            Some(t) => self.set(format!("已连接「{host}」· {t}")),
            None => self.set(format!("已连接「{host}」· {path} · {rtt_ms:.1} ms")),
        }
    }

    fn set(&self, status: String) {
        if let Some(tray) = &self.tray {
            tray.update(tray::State {
                title: None,
                tooltip: format!("Roam 被控端 v{} · {status}", roam_proto::VERSION),
                status,
                paused: false,
                update_label: None,
            });
        }
    }
}
