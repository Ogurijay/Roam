//! roam-probe：在真实网络环境下测量两台机器之间能不能直连、走哪条路、延迟多少。
//!
//! 被测机器 A：`roam-probe listen`，会打印一个连接码。
//! 控制机器 B：`roam-probe connect <连接码>`，每秒输出路径与 RTT，结束时给出结论。

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use data_encoding::BASE32_NOPAD;
use iroh::{
    Endpoint, EndpointAddr, RelayMode, TransportAddr,
    endpoint::{Connection, RecvStream, SendStream, presets},
};
use roam_proto::{ALPN, Datagram, StreamMsg};
use tokio::sync::mpsc;

const TICKET_PREFIX: &str = "roam1";
/// 数据报超过这个时间没有回包就算丢失。
const LOSS_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Parser)]
#[command(name = "roam-probe", about = "Roam 连通性测试：打洞、路径类型、RTT")]
struct Cli {
    /// 禁用中继，只测直连（局域网 / 公网 IP / 打洞）
    #[arg(long, global = true)]
    no_relay: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 在被测机器上运行，等待连接
    Listen,
    /// 连接到 listen 端并测量
    Connect {
        /// listen 端打印的连接码
        ticket: String,
        /// 测量时长（秒）
        #[arg(long, default_value_t = 30)]
        secs: u64,
        /// 数据报发送频率（Hz），模拟鼠标移动的发包频率
        #[arg(long, default_value_t = 250)]
        rate: u32,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,iroh=error".into()),
        )
        .init();

    let cli = Cli::parse();
    let endpoint = bind(cli.no_relay).await?;
    match cli.cmd {
        Cmd::Listen => listen(endpoint).await,
        Cmd::Connect { ticket, secs, rate } => {
            connect(endpoint, &ticket, Duration::from_secs(secs), rate).await
        }
    }
}

async fn bind(no_relay: bool) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::N0).alpns(vec![ALPN.to_vec()]);
    if no_relay {
        builder = builder.relay_mode(RelayMode::Disabled);
    }
    let endpoint = builder.bind().await.context("绑定端点失败")?;

    if !no_relay {
        eprintln!("正在连接中继服务器（用于协调打洞）...");
        if tokio::time::timeout(Duration::from_secs(10), endpoint.online())
            .await
            .is_err()
        {
            eprintln!("⚠ 10 秒内未连上中继服务器，跨网打洞可能失败，局域网直连不受影响。");
        }
    }
    Ok(endpoint)
}

// ---------------------------------------------------------------- 连接码

fn encode_ticket(addr: &EndpointAddr) -> Result<String> {
    let bytes = postcard::to_stdvec(addr)?;
    Ok(format!(
        "{TICKET_PREFIX}{}",
        BASE32_NOPAD.encode(&bytes).to_lowercase()
    ))
}

fn decode_ticket(ticket: &str) -> Result<EndpointAddr> {
    let Some(body) = ticket.trim().strip_prefix(TICKET_PREFIX) else {
        bail!("连接码格式不对，应以 {TICKET_PREFIX} 开头");
    };
    let bytes = BASE32_NOPAD
        .decode(body.to_uppercase().as_bytes())
        .context("连接码解码失败")?;
    Ok(postcard::from_bytes(&bytes)?)
}

// ---------------------------------------------------------------- 路径

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Lan,
    PublicV4,
    PublicV6,
    Relay,
    Unknown,
}

impl PathKind {
    fn label(self) -> &'static str {
        match self {
            PathKind::Lan => "局域网直连",
            PathKind::PublicV4 => "公网直连(IPv4)",
            PathKind::PublicV6 => "公网直连(IPv6)",
            PathKind::Relay => "中继",
            PathKind::Unknown => "未知",
        }
    }

    fn is_direct(self) -> bool {
        matches!(
            self,
            PathKind::Lan | PathKind::PublicV4 | PathKind::PublicV6
        )
    }
}

fn classify(addr: &TransportAddr) -> PathKind {
    match addr {
        TransportAddr::Relay(_) => PathKind::Relay,
        TransportAddr::Ip(sa) => classify_ip(sa),
        _ => PathKind::Unknown,
    }
}

fn classify_ip(sa: &SocketAddr) -> PathKind {
    match sa.ip() {
        IpAddr::V4(ip) if ip.is_private() || ip.is_loopback() || ip.is_link_local() => {
            PathKind::Lan
        }
        // 100.64.0.0/10：Tailscale 等组网工具的虚拟地址，按局域网看待
        IpAddr::V4(ip) if ip.octets()[0] == 100 && (ip.octets()[1] & 0xC0) == 64 => PathKind::Lan,
        IpAddr::V4(_) => PathKind::PublicV4,
        IpAddr::V6(ip)
            if ip.is_loopback() || ip.is_unicast_link_local() || ip.is_unique_local() =>
        {
            PathKind::Lan
        }
        IpAddr::V6(_) => PathKind::PublicV6,
    }
}

/// 当前被选中用于传输数据的路径。
fn selected_path(conn: &Connection) -> Option<(PathKind, String, Duration)> {
    conn.paths().iter().find(|p| p.is_selected()).map(|p| {
        let addr = p.remote_addr();
        let shown = match addr {
            TransportAddr::Ip(sa) => sa.to_string(),
            other => format!("{other:?}"),
        };
        (classify(addr), shown, p.rtt())
    })
}

// ---------------------------------------------------------------- listen

async fn listen(endpoint: Endpoint) -> Result<()> {
    let addr = endpoint.addr();
    println!("\n本机 ID: {}", endpoint.id().fmt_short());
    println!("可直连地址:");
    for ip in addr.ip_addrs() {
        println!("  {ip}  [{}]", classify_ip(ip).label());
    }
    for relay in addr.relay_urls() {
        println!("中继: {relay}");
    }
    println!("\n在另一台机器上运行:\n");
    println!("  roam-probe connect {}\n", encode_ticket(&addr)?);
    println!("等待连接... (Ctrl+C 退出)");

    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!("连接握手失败: {err:#}");
                    return;
                }
            };
            let peer = conn.remote_id().fmt_short();
            println!("[{peer}] 已连接");
            serve(conn.clone()).await;
            let summary = selected_path(&conn)
                .map(|(kind, addr, _)| format!("{} {addr}", kind.label()))
                .unwrap_or_default();
            println!("[{peer}] 已断开 {summary}");
        });
    }
    Ok(())
}

/// 原样回显 Ping，同时定期打印路径变化。
async fn serve(conn: Connection) {
    let dgram_conn = conn.clone();
    let dgram_echo = tokio::spawn(async move {
        while let Ok(bytes) = dgram_conn.read_datagram().await {
            if let Ok(Datagram::Ping { seq, sent_us }) = Datagram::decode(&bytes) {
                let pong = Datagram::Pong { seq, sent_us }.encode().expect("encode");
                let _ = dgram_conn.send_datagram(pong.into());
            }
        }
    });

    let path_conn = conn.clone();
    let path_watch = tokio::spawn(async move {
        let peer = path_conn.remote_id().fmt_short();
        let mut last = None;
        loop {
            let now = selected_path(&path_conn).map(|(kind, addr, _)| (kind, addr));
            if now != last {
                if let Some((kind, addr)) = &now {
                    println!("[{peer}] 路径 → {} {addr}", kind.label());
                }
                last = now;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    while let Ok((send, recv)) = conn.accept_bi().await {
        tokio::spawn(async move {
            let _ = echo_stream(send, recv).await;
        });
    }
    dgram_echo.abort();
    path_watch.abort();
}

async fn echo_stream(mut send: SendStream, mut recv: RecvStream) -> Result<()> {
    loop {
        let msg = read_frame(&mut recv).await?;
        if let StreamMsg::Ping { seq, sent_us } = msg {
            send.write_all(&StreamMsg::Pong { seq, sent_us }.encode_frame()?)
                .await?;
        }
    }
}

async fn read_frame(recv: &mut RecvStream) -> Result<StreamMsg> {
    let mut len = [0u8; 2];
    recv.read_exact(&mut len).await?;
    let mut body = vec![0u8; u16::from_be_bytes(len) as usize];
    recv.read_exact(&mut body).await?;
    Ok(StreamMsg::decode_body(&body)?)
}

// ---------------------------------------------------------------- connect

/// 一个统计窗口内的 RTT 样本（微秒）。
#[derive(Default)]
struct Samples {
    rtts: Vec<u64>,
}

impl Samples {
    fn percentile(&self, p: f64) -> Option<f64> {
        if self.rtts.is_empty() {
            return None;
        }
        let mut sorted = self.rtts.clone();
        sorted.sort_unstable();
        let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
        Some(sorted[idx] as f64 / 1000.0)
    }
}

/// 已有结论的数据报：收到回包的和判定丢失的。仍在路上的不计入。
#[derive(Default)]
struct Loss {
    acked: u64,
    lost: u64,
}

impl Loss {
    fn percent(&self) -> f64 {
        let settled = self.acked + self.lost;
        if settled == 0 {
            0.0
        } else {
            100.0 * self.lost as f64 / settled as f64
        }
    }
}

/// 移除超时未回的数据报，返回本次判定丢失的数量。
fn expire_pending(pending: &mut HashMap<u64, Instant>) -> u64 {
    let before = pending.len();
    pending.retain(|_, sent| sent.elapsed() < LOSS_TIMEOUT);
    (before - pending.len()) as u64
}

fn fmt_ms(v: Option<f64>) -> String {
    v.map(|ms| format!("{ms:6.1}ms"))
        .unwrap_or_else(|| "     -  ".into())
}

async fn connect(endpoint: Endpoint, ticket: &str, secs: Duration, rate: u32) -> Result<()> {
    let addr = decode_ticket(ticket)?;
    println!("目标: {}", addr.id.fmt_short());
    for ip in addr.ip_addrs() {
        println!("  候选 {ip}  [{}]", classify_ip(ip).label());
    }

    let epoch = Instant::now();
    let now_us = move || epoch.elapsed().as_micros() as u64;

    let conn = tokio::time::timeout(Duration::from_secs(20), endpoint.connect(addr, ALPN))
        .await
        .context("20 秒内未能建立连接")??;
    let connect_time = epoch.elapsed();
    println!("✓ 连接建立，用时 {} ms\n", connect_time.as_millis());

    // 数据报回包 → 主循环
    let (dgram_tx, mut dgram_rx) = mpsc::unbounded_channel::<(u64, u64)>();
    let recv_conn = conn.clone();
    tokio::spawn(async move {
        while let Ok(bytes) = recv_conn.read_datagram().await {
            if let Ok(Datagram::Pong { seq, sent_us }) = Datagram::decode(&bytes) {
                let _ = dgram_tx.send((seq, now_us().saturating_sub(sent_us)));
            }
        }
    });

    // 可靠流：10Hz 串行 ping，模拟按键通道
    let stream_samples = Arc::new(Mutex::new(Samples::default()));
    let (mut send, mut recv) = conn.open_bi().await?;
    let stream_task = {
        let samples = stream_samples.clone();
        tokio::spawn(async move {
            for seq in 0u64.. {
                let ping = StreamMsg::Ping {
                    seq,
                    sent_us: now_us(),
                };
                if send.write_all(&ping.encode_frame()?).await.is_err() {
                    break;
                }
                if let StreamMsg::Pong { sent_us, .. } = read_frame(&mut recv).await? {
                    samples
                        .lock()
                        .unwrap()
                        .rtts
                        .push(now_us().saturating_sub(sent_us));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            anyhow::Ok(())
        })
    };

    let mut send_tick = tokio::time::interval(Duration::from_secs_f64(1.0 / rate.max(1) as f64));
    send_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut report_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let mut path_tick = tokio::time::interval(Duration::from_millis(20));
    let deadline = tokio::time::sleep(secs);
    tokio::pin!(deadline);

    let mut seq = 0u64;
    // 已发出、尚未收到回包的数据报：seq → 发送时刻
    let mut pending: HashMap<u64, Instant> = HashMap::new();
    let mut win = Loss::default();
    let mut total = Loss::default();
    let mut window = Samples::default();
    let mut all = Samples::default();
    let mut direct_rtts = Samples::default();
    let mut last_path: Option<PathKind> = None;
    let mut path_changes: Vec<(Duration, PathKind, String)> = Vec::new();
    let mut first_direct: Option<Duration> = None;

    println!(" 时间  路径              数据报 p50 / p95 / max            丢包    可靠流 p50");
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            _ = send_tick.tick() => {
                let ping = Datagram::Ping { seq, sent_us: now_us() }.encode()?;
                if conn.send_datagram(ping.into()).is_ok() {
                    pending.insert(seq, Instant::now());
                }
                seq += 1;
            }
            Some((pong_seq, rtt)) = dgram_rx.recv() => {
                // 已经判定丢失后才到的回包只计入 RTT，不改丢包统计
                if pending.remove(&pong_seq).is_some() {
                    win.acked += 1;
                    total.acked += 1;
                }
                window.rtts.push(rtt);
                all.rtts.push(rtt);
                if last_path.is_some_and(PathKind::is_direct) {
                    direct_rtts.rtts.push(rtt);
                }
            }
            _ = path_tick.tick() => {
                if let Some((kind, addr, _)) = selected_path(&conn)
                    && last_path != Some(kind)
                {
                    let at = epoch.elapsed();
                    if kind.is_direct() && first_direct.is_none() {
                        first_direct = Some(at);
                    }
                    path_changes.push((at, kind, addr));
                    last_path = Some(kind);
                }
            }
            _ = report_tick.tick() => {
                let lost = expire_pending(&mut pending);
                win.lost += lost;
                total.lost += lost;
                let stream_p50 = {
                    let mut s = stream_samples.lock().unwrap();
                    let v = s.percentile(0.5);
                    s.rtts.clear();
                    v
                };
                println!(
                    "{:4}s  {:<16}  {} / {} / {}   {:5.1}%   {}",
                    epoch.elapsed().as_secs(),
                    last_path.map(PathKind::label).unwrap_or("-"),
                    fmt_ms(window.percentile(0.5)),
                    fmt_ms(window.percentile(0.95)),
                    fmt_ms(window.percentile(1.0)),
                    win.percent(),
                    fmt_ms(stream_p50),
                );
                window.rtts.clear();
                win = Loss::default();
            }
        }
    }

    stream_task.abort();
    conn.close(0u32.into(), b"done");
    total.lost += expire_pending(&mut pending);

    // ------------------------------------------------------------ 结论
    println!("\n========== 结果 ==========");
    println!("建立连接: {} ms", connect_time.as_millis());
    println!("路径变化:");
    for (at, kind, addr) in &path_changes {
        println!("  {:6} ms  → {} {addr}", at.as_millis(), kind.label());
    }
    match first_direct {
        Some(at) => println!("直连建立: 启动后 {} ms", at.as_millis()),
        None => println!("直连建立: ✗ 全程未能直连（一直走中继）"),
    }
    println!(
        "数据报 RTT（全程）: p50 {}  p95 {}  p99 {}  max {}  丢包 {:.2}%",
        fmt_ms(all.percentile(0.5)),
        fmt_ms(all.percentile(0.95)),
        fmt_ms(all.percentile(0.99)),
        fmt_ms(all.percentile(1.0)),
        total.percent(),
    );

    let basis = if direct_rtts.rtts.is_empty() {
        &all
    } else {
        &direct_rtts
    };
    if let Some(p50) = basis.percentile(0.5) {
        let verdict = match p50 {
            x if x < 10.0 => "优秀：A 模式无感，B 模式流畅",
            x if x < 30.0 => "良好：A 模式基本无感，B 模式可用",
            x if x < 80.0 => "一般：A 模式能感觉到延迟，B 模式勉强可用",
            _ => "较差：建议检查网络（开启 IPv6 / 关闭代理 TUN 模式 / 换组网工具）",
        };
        let scope = if direct_rtts.rtts.is_empty() {
            "全程"
        } else {
            "直连后"
        };
        println!("结论（按{scope} p50 {p50:.1}ms）: {verdict}");
    }
    endpoint.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_addresses() {
        let k = |s: &str| classify_ip(&s.parse().unwrap());
        assert_eq!(k("192.168.1.2:1"), PathKind::Lan);
        assert_eq!(k("100.101.102.103:1"), PathKind::Lan);
        assert_eq!(k("203.0.113.7:1"), PathKind::PublicV4);
        assert_eq!(k("[fe80::1]:1"), PathKind::Lan);
        assert_eq!(k("[240e::1]:1"), PathKind::PublicV6);
    }
}
