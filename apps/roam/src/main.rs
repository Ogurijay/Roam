//! Roam：一套键鼠控制多台电脑。
//!
//! - `roam client`：被控端（Windows），托盘常驻，等待主机连接
//! - `roam host`：主机（macOS），菜单栏常驻，采集键鼠并提供布局页面
//! - `roam add <连接码>`：在主机上添加被控设备
//!
//! 不带参数运行时：Windows 默认启动被控端，macOS 默认启动主机。

// Windows 上双击启动时不弹出黑色控制台窗口；从终端启动时再挂回终端输出
#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod client;
mod clipboard;
mod conn;
mod dialog;
mod discovery;
mod files;
mod host;
mod pairing;
mod sync;
mod tray;
mod ui;
mod update;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use roam_core::{config::Store, router::InputEvent};

use crate::{
    host::Host,
    tray::{Action, Spec, Tray},
};

#[derive(Parser)]
#[command(name = "roam", version, about = "一套键鼠控制多台电脑")]
struct Cli {
    /// 配置目录（同一台机器上同时运行主机和被控端测试时使用）
    #[arg(long, global = true, env = "ROAM_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// 不显示托盘 / 菜单栏图标，只在终端运行
    #[arg(long, global = true)]
    no_tray: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
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

/// 开机自启时使用的参数。
const CLIENT_ARGS: &[&str] = &["client"];
const HOST_ARGS: &[&str] = &["host", "--no-browser"];

fn main() -> Result<()> {
    dialog::attach_parent_console();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=error,mdns_sd=error".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    update::cleanup_old();
    let store = Store::open(cli.config_dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let cmd = cli.cmd.unwrap_or(if cfg!(target_os = "macos") {
        Cmd::Host {
            ui_port: 7878,
            no_browser: false,
            demo: false,
        }
    } else {
        Cmd::Client
    });

    match cmd {
        Cmd::Add { ticket, name } => add(store, &ticket, name),
        Cmd::Client if cli.no_tray => runtime.block_on(client::run(store, None)),
        Cmd::Client => run_client_with_tray(runtime, store),
        Cmd::Host {
            ui_port,
            no_browser,
            demo,
        } => {
            let (host, url) = runtime.block_on(start_host(store, ui_port, no_browser, demo))?;
            if cli.no_tray {
                runtime.block_on(async {
                    tokio::signal::ctrl_c().await?;
                    host.router.lock().unwrap().return_home();
                    host.endpoint.close().await;
                    anyhow::Ok(())
                })
            } else {
                run_host_with_tray(runtime, host, url)
            }
        }
    }
}

fn run_client_with_tray(runtime: tokio::runtime::Runtime, store: Store) -> Result<()> {
    let tray = Tray::new();
    let handle = tray.handle();
    {
        let store = store.clone();
        let handle = handle.clone();
        runtime.spawn(async move {
            if let Err(err) = client::run(store, Some(handle.clone())).await {
                dialog::alert("Roam 启动失败", &format!("{err:#}"));
                handle.quit();
            }
        });
    }
    let spec = Spec {
        copy_ticket: true,
        revoke_hosts: true,
        autostart: autostart::is_enabled(CLIENT_ARGS),
        ..Default::default()
    };
    let initial = tray::State {
        status: "正在启动…".into(),
        tooltip: "Roam 被控端".into(),
        ..Default::default()
    };
    tray.run(spec, initial, move |action| match action {
        Action::CopyTicket => match std::fs::read_to_string(store.dir().join("ticket.txt")) {
            Ok(code) => copy_to_clipboard(&code),
            Err(_) => dialog::alert("Roam", "连接码还没有生成，请稍后再试。"),
        },
        Action::RevokeHosts => {
            if dialog::confirm(
                "Roam",
                "撤销后，所有主机下次连接时都需要重新确认。\n\n确定撤销吗？",
            ) && let Ok(mut config) = store.load()
            {
                config.trusted_hosts.clear();
                let _ = store.save(&config);
            }
        }
        Action::Autostart(on) => {
            if let Err(err) = autostart::set_enabled(CLIENT_ARGS, on) {
                dialog::alert("Roam", &format!("设置开机自启失败：{err:#}"));
            }
        }
        Action::Quit => std::process::exit(0),
        Action::OpenUi | Action::Pause(_) | Action::Update => {}
    })
}

fn run_host_with_tray(
    runtime: tokio::runtime::Runtime,
    host: Arc<Host>,
    url: String,
) -> Result<()> {
    let tray = Tray::new();
    {
        let _guard = runtime.enter();
        host.start_tray_updates(tray.handle());
    }
    let spec = Spec {
        open_ui: true,
        pause: true,
        update: true,
        autostart: autostart::is_enabled(HOST_ARGS),
        ..Default::default()
    };
    let initial = host.tray_state();
    // 运行时必须保持存活，交给闭包持有
    tray.run(spec, initial, move |action| {
        let _keep = &runtime;
        match action {
            Action::OpenUi => open_browser(&url),
            Action::Pause(paused) => host.set_paused(paused),
            Action::Autostart(on) => {
                if let Err(err) = autostart::set_enabled(HOST_ARGS, on) {
                    eprintln!("设置开机自启失败: {err:#}");
                }
            }
            Action::Quit => {
                host.router.lock().unwrap().return_home();
                std::process::exit(0);
            }
            Action::Update => {
                let host = host.clone();
                runtime.spawn(async move {
                    let available = host.updater.state.lock().unwrap().available;
                    if available {
                        let _ = host.update_all().await;
                    } else {
                        let _ = host.updater.check().await;
                    }
                });
            }
            Action::CopyTicket | Action::RevokeHosts => {}
        }
    })
}

fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())) {
        Ok(()) => {}
        Err(err) => dialog::alert("Roam", &format!("复制失败：{err}")),
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
        settings: Default::default(),
    });
    store.save(&config)?;
    println!("已添加。运行 roam host 后会自动连接。");
    Ok(())
}

async fn start_host(
    store: Store,
    ui_port: u16,
    no_browser: bool,
    demo: bool,
) -> Result<(Arc<Host>, String)> {
    #[cfg(windows)]
    roam_input::windows::init_dpi();

    let endpoint = conn::bind(&store, None).await?;

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
    let combo = host
        .config
        .lock()
        .unwrap()
        .switching
        .hotkey_modifiers
        .iter()
        .map(|m| match m {
            roam_core::settings::Modifier::Shift => "Shift",
            roam_core::settings::Modifier::Ctrl => "Ctrl",
            roam_core::settings::Modifier::Alt => "Option",
            roam_core::settings::Modifier::Cmd => "Cmd",
        })
        .collect::<Vec<_>>()
        .join(" + ");
    println!(
        "快捷键: {combo} + Esc 回到本机，{combo} + 数字 切到对应设备，{combo} + L 锁定当前屏幕\n"
    );
    if !no_browser {
        open_browser(&url);
    }
    if demo {
        tokio::spawn(run_demo(host.clone()));
    }
    Ok((host, url))
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
    println!("[演示] ✓ 已切到 {target}，停留 3 秒后在对方屏幕上画圈...");
    tokio::time::sleep(Duration::from_secs(3)).await;
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
