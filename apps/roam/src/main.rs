//! Roam：一套键鼠控制多台电脑。
//!
//! - `roam client`：被控端（Windows），打印连接码，等待主机连接
//! - `roam host`：主机（macOS），采集键鼠并打开布局页面
//! - `roam add <连接码>`：在主机上添加被控设备

mod client;
mod conn;
mod host;
mod ui;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use roam_core::{config::Store, router::InputEvent};

use crate::host::Host;

#[derive(Parser)]
#[command(name = "roam", version, about = "一套键鼠控制多台电脑")]
struct Cli {
    /// 配置目录（同一台机器上同时运行主机和被控端测试时使用）
    #[arg(long, global = true, env = "ROAM_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 作为主机运行：用本机键鼠控制其他设备，并打开布局页面
    Host {
        /// 布局页面端口
        #[arg(long, default_value_t = 7878)]
        ui_port: u16,
        /// 不自动打开浏览器
        #[arg(long)]
        no_browser: bool,
        /// 演示：被控端上线后，自动把光标移过去画一圈再回来（用于在没有 Mac 时验证链路）
        #[arg(long, hide = true)]
        demo: bool,
    },
    /// 作为被控端运行：等待主机连接
    Client,
    /// 在主机上添加一台被控设备
    Add {
        /// 被控端打印的连接码
        ticket: String,
        /// 显示名称
        #[arg(long)]
        name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=error".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let store = Store::open(cli.config_dir)?;
    match cli.cmd {
        Cmd::Client => client::run(store).await,
        Cmd::Add { ticket, name } => add(store, &ticket, name),
        Cmd::Host {
            ui_port,
            no_browser,
            demo,
        } => run_host(store, ui_port, no_browser, demo).await,
    }
}

fn add(store: Store, code: &str, name: Option<String>) -> Result<()> {
    let addr = roam_core::ticket::decode(code)?;
    let mut config = store.load()?;
    let id = addr.id.to_string();
    if config.peers.iter().any(|p| p.id == id) {
        println!("这台设备已经添加过了。");
        return Ok(());
    }
    config.peers.push(roam_core::config::Peer {
        id,
        ticket: code.trim().to_string(),
        name: name.unwrap_or_else(|| format!("设备 {}", addr.id.fmt_short())),
        os: None,
        displays: Vec::new(),
    });
    store.save(&config)?;
    println!("已添加。运行 roam host 后会自动连接。");
    Ok(())
}

async fn run_host(store: Store, ui_port: u16, no_browser: bool, demo: bool) -> Result<()> {
    #[cfg(windows)]
    roam_input::windows::init_dpi();

    let endpoint = conn::bind(&store).await?;

    #[cfg(target_os = "macos")]
    let cursor: Box<dyn roam_core::router::CursorControl> =
        Box::new(roam_input::mac::MacCursor::new());
    #[cfg(not(target_os = "macos"))]
    let cursor: Box<dyn roam_core::router::CursorControl> = Box::new(host::NoopCursor);

    let host = Host::new(store, endpoint, cursor)?;
    host.start();
    start_capture(&host);

    let addr = ui::serve(host.clone(), ui_port).await?;
    let url = format!("http://{addr}");
    println!("\nRoam 主机 · {}", host.config.lock().unwrap().name);
    println!("布局页面: {url}");
    println!("逃生热键: Ctrl + Option + Cmd + Esc（强制回到本机）\n");
    if !no_browser {
        open_browser(&url);
    }

    if demo {
        tokio::spawn(run_demo(host.clone()));
    }

    tokio::signal::ctrl_c().await?;
    host.router.lock().unwrap().return_home();
    host.endpoint.close().await;
    Ok(())
}

#[cfg(target_os = "macos")]
fn start_capture(host: &Arc<Host>) {
    if let Err(err) = roam_input::mac::check_permissions() {
        eprintln!("⚠ {err:#}");
        *host.capture.lock().unwrap() = Err(format!("{err:#}"));
        return;
    }
    let router = host.router.clone();
    let capture = host.clone();
    std::thread::Builder::new()
        .name("roam-capture".into())
        .spawn(move || {
            *capture.capture.lock().unwrap() = Ok(());
            let result = roam_input::mac::run_capture(move |ev| router.lock().unwrap().handle(ev));
            if let Err(err) = result {
                eprintln!("⚠ 键鼠采集失败: {err:#}");
                *capture.capture.lock().unwrap() = Err(format!("{err:#}"));
            }
        })
        .expect("spawn capture thread");
}

#[cfg(not(target_os = "macos"))]
fn start_capture(host: &Arc<Host>) {
    *host.capture.lock().unwrap() =
        Err("当前平台暂不支持作为主机采集键鼠，只能预览布局。主机请在 Mac 上运行。".into());
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(windows)]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    if result.is_err() {
        println!("请手动在浏览器打开 {url}");
    }
}

/// 演示：模拟鼠标从主机右边缘移到被控设备，画一圈后再回来。
async fn run_demo(host: Arc<Host>) {
    println!("[演示] 等待被控设备上线...");
    loop {
        if host.status.lock().unwrap().values().any(|s| s.online) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let Some(display) = host.displays.first().map(|d| d.rect) else {
        return;
    };
    let (mut x, y) = (display.x + display.w / 2.0, display.y + display.h / 2.0);
    let right = display.x + display.w - 1.0;
    let feed = |ev: InputEvent| host.router.lock().unwrap().handle(ev);
    let step = Duration::from_millis(8);

    println!("[演示] 向右移动，越过主机右边缘...");
    for _ in 0..1000 {
        let dx = 20.0;
        x = (x + dx).min(right); // 模拟系统把光标钳制在屏幕内
        feed(InputEvent::Move { x, y, dx, dy: 0.0 });
        if host.router.lock().unwrap().focused().is_some() {
            break;
        }
        tokio::time::sleep(step).await;
    }
    let Some(target) = host.router.lock().unwrap().focused().map(str::to_string) else {
        println!("[演示] ✗ 没能切到被控设备，请检查布局页面里设备是否贴在主机右侧");
        return;
    };
    println!("[演示] ✓ 已切到 {target}，在对方屏幕上画圈...");
    for _ in 0..20 {
        feed(InputEvent::Move {
            x,
            y,
            dx: 20.0,
            dy: 0.0,
        });
        tokio::time::sleep(step).await;
    }
    let steps = 120;
    for i in 0..steps {
        let a = i as f64 / steps as f64 * std::f64::consts::TAU;
        let (dx, dy) = (-a.sin() * 10.0, a.cos() * 10.0);
        feed(InputEvent::Move { x, y, dx, dy });
        tokio::time::sleep(step).await;
    }
    println!("[演示] 向左移回主机...");
    for _ in 0..1000 {
        feed(InputEvent::Move {
            x,
            y,
            dx: -20.0,
            dy: 0.0,
        });
        if host.router.lock().unwrap().focused().is_none() {
            println!("[演示] ✓ 已回到主机，链路正常");
            return;
        }
        tokio::time::sleep(step).await;
    }
    println!("[演示] ✗ 没能回到主机");
}
