//! 被控端：等待主机连接，把收到的键鼠注入本机。

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use roam_core::{config::Store, net::selected_path, ticket};
use roam_input::Injector;
use roam_proto::{Datagram, Hello, Os, StreamMsg};

use crate::conn::{self, read_msg, write_msg};

pub async fn run(store: Store) -> Result<()> {
    if !roam_input::CAN_INJECT {
        bail!("被控端目前只支持 Windows，macOS / Linux 被控端在后续版本中加入。");
    }
    #[cfg(windows)]
    roam_input::windows::init_dpi();

    let config = store.load()?;
    let endpoint = conn::bind(&store).await?;
    let code = ticket::encode(&endpoint.addr())?;
    std::fs::write(store.dir().join("ticket.txt"), &code)?;

    println!("\nRoam 被控端 · {}", config.name);
    println!("本机 ID: {}", endpoint.id());
    println!(
        "\n在主机的布局页面里粘贴下面的连接码（也保存在 {}）：\n",
        store.dir().join("ticket.txt").display()
    );
    println!("{code}\n");
    println!("等待主机连接... (Ctrl+C 退出)");

    let injector = Arc::new(Mutex::new(Injector::new()));
    // 同一时间只处理一个授权询问
    let prompt_lock = Arc::new(tokio::sync::Mutex::new(()));

    while let Some(incoming) = endpoint.accept().await {
        let store = store.clone();
        let injector = injector.clone();
        let prompt_lock = prompt_lock.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!("连接握手失败: {err:#}");
                    return;
                }
            };
            let host_id = conn.remote_id().to_string();
            match authorize(&store, &host_id, &prompt_lock).await {
                Ok(true) => {}
                Ok(false) => {
                    conn.close(1u32.into(), b"denied");
                    return;
                }
                Err(err) => {
                    eprintln!("读取配置失败: {err:#}");
                    return;
                }
            }
            let name = store.load().map(|c| c.name).unwrap_or_default();
            if let Err(err) = session(conn, injector.clone(), name).await {
                eprintln!("会话结束: {err:#}");
            }
            injector.lock().unwrap().release_all();
        });
    }
    Ok(())
}

/// 已信任的主机直接放行；陌生主机在终端里询问。
async fn authorize(
    store: &Store,
    host_id: &str,
    prompt_lock: &tokio::sync::Mutex<()>,
) -> Result<bool> {
    let _guard = prompt_lock.lock().await;
    if store.load()?.trusted_hosts.iter().any(|h| h == host_id) {
        return Ok(true);
    }
    println!("\n主机 {host_id} 请求控制本机的键盘和鼠标。");
    println!("允许并记住这台主机吗？[y/N] ");
    let answer = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        line
    })
    .await?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        println!("已拒绝。");
        return Ok(false);
    }
    let mut config = store.load()?;
    config.trusted_hosts.push(host_id.to_string());
    store.save(&config)?;
    println!("已允许，之后这台主机连接时不再询问。");
    Ok(true)
}

async fn session(conn: Connection, injector: Arc<Mutex<Injector>>, name: String) -> Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    let StreamMsg::Hello(host) = read_msg(&mut recv).await? else {
        bail!("主机没有先发送 Hello");
    };
    write_msg(
        &mut send,
        &StreamMsg::Hello(Hello {
            name,
            os: Os::current(),
            displays: roam_input::displays(),
        }),
    )
    .await?;

    let path = selected_path(&conn)
        .map(|(kind, addr, _)| format!("{} {addr}", kind.label()))
        .unwrap_or_default();
    println!("✓ 主机「{}」已连接 {path}", host.name);
    injector.lock().unwrap().reset_sequence();

    // 连接刚建立时通常先走中继，打洞成功后会切到直连，变化时提示一下
    let path_conn = conn.clone();
    let path_task = tokio::spawn(async move {
        let mut last = selected_path(&path_conn).map(|p| p.0);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let now = selected_path(&path_conn);
            if now.as_ref().map(|p| p.0) != last {
                if let Some((kind, addr, rtt)) = &now {
                    println!(
                        "  路径 → {} {addr}（{:.1} ms）",
                        kind.label(),
                        rtt.as_secs_f64() * 1000.0
                    );
                }
                last = now.map(|p| p.0);
            }
        }
    });

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
    println!("主机「{}」已断开", host.name);
    result
}
