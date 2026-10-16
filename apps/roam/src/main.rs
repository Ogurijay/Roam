//! Roam：一套键鼠控制多台电脑。
//!
//! 每台电脑装同一个程序，在界面里选择角色：
//! - 主机：用本机键鼠控制其他电脑
//! - 被控端：被其他电脑控制
//!
//! 不带参数运行时使用配置里的角色（macOS 默认主机，Windows 默认被控端）；
//! `roam host` / `roam client` 可临时指定角色，`roam add <连接码>` 在主机上添加设备。

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
mod logging;
mod pairing;
mod pull;
mod send_menu;
mod sync;
mod tray;
mod ui;
mod update;
#[cfg(windows)]
mod winsvc;

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use roam_core::{
    config::{Role, Store},
    router::InputEvent,
};

use crate::{
    client::Client,
    host::Host,
    tray::{Action, Spec, Tray},
    ui::App,
};

#[derive(Parser)]
#[command(name = "roam", version, about = "一套键鼠控制多台电脑")]
struct Cli {
    /// 配置目录（同一台机器上同时运行主机和被控端测试时使用）
    #[arg(long, global = true, env = "ROAM_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// 不显示托盘 / 菜单栏图标和窗口，只在终端运行
    #[arg(long, global = true)]
    no_tray: bool,
    /// 启动时不打开窗口（开机自启用）
    #[arg(long, global = true)]
    background: bool,
    /// 界面服务端口
    #[arg(long, global = true, default_value_t = 7878)]
    ui_port: u16,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 从文件管理器发送文件：打开在线设备选择窗口
    Send {
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// 安装或移除文件管理器的右键菜单（当前用户，无需管理员权限）
    ContextMenu {
        #[arg(value_parser = ["install", "uninstall"])]
        action: String,
    },
    /// 作为主机运行：用本机键鼠控制其他设备
    Host {
        /// 演示：被控端上线后，自动把光标移过去画一圈再回来（用于验证链路）
        #[arg(long, hide = true)]
        demo: bool,
    },
    /// 作为被控端运行：等待主机连接
    Client,
    /// Windows 锁屏控制服务（由界面调用，一般不需要手动运行）
    #[command(hide = true)]
    Service {
        /// install / uninstall / run / allow-elevated / deny-elevated
        action: String,
        #[arg(long)]
        log_dir: PathBuf,
    },
    /// 锁屏控制的输入助手（由系统服务启动）
    #[command(hide = true)]
    InputHelper {
        #[arg(long)]
        log_dir: PathBuf,
    },
    /// 在主机上添加一台被控设备
    Add {
        /// 被控端的连接码
        ticket: String,
        /// 显示名称
        #[arg(long)]
        name: Option<String>,
    },
}

/// 开机自启时使用的参数：角色从配置里读。
const AUTOSTART_ARGS: &[&str] = &["--background"];

/// 重启（切换角色）时沿用的参数：只保留全局选项，不带子命令。
static RELAUNCH_ARGS: OnceLock<Vec<String>> = OnceLock::new();

/// 用当前的全局选项重新启动程序（角色从配置里读）。
pub fn relaunch() -> ! {
    let args = RELAUNCH_ARGS.get().cloned().unwrap_or_default();
    update::restart_with(&args)
}

fn main() -> Result<()> {
    dialog::attach_parent_console();
    let cli = Cli::parse();
    if let Some(Cmd::Service { .. } | Cmd::InputHelper { .. }) = &cli.cmd {
        return run_system_component(cli.cmd.expect("checked above"));
    }
    let store = Store::open(cli.config_dir.clone())?;
    let command_result = match &cli.cmd {
        Some(Cmd::Send { paths }) => Some(send_menu::open(&store, cli.ui_port, paths)),
        Some(Cmd::ContextMenu { action }) => Some(send_menu::set_enabled(
            &store,
            cli.ui_port,
            action == "install",
        )),
        _ => None,
    };
    if let Some(result) = command_result {
        if let Err(err) = &result {
            dialog::alert("Roam", &format!("{err:#}"));
        }
        return result;
    }
    update::cleanup_old();
    logging::init(&store.dir().join("logs"));
    if cli.config_dir.is_none() && send_menu::enabled() {
        if let Err(err) = send_menu::set_enabled(&store, cli.ui_port, true) {
            tracing::warn!("刷新右键菜单失败: {err:#}");
        }
    }

    let mut relaunch_args = Vec::new();
    if let Some(dir) = &cli.config_dir {
        relaunch_args.extend(["--config-dir".to_string(), dir.display().to_string()]);
    }
    if cli.no_tray {
        relaunch_args.push("--no-tray".into());
    }
    if cli.ui_port != 7878 {
        relaunch_args.extend(["--ui-port".to_string(), cli.ui_port.to_string()]);
    }
    let _ = RELAUNCH_ARGS.set(relaunch_args);

    let (role, demo) = match &cli.cmd {
        Some(Cmd::Add { ticket, name }) => return add(store, ticket, name.clone()),
        Some(Cmd::Host { demo }) => (Role::Host, *demo),
        Some(Cmd::Client) => (Role::Client, false),
        Some(
            Cmd::Service { .. }
            | Cmd::InputHelper { .. }
            | Cmd::Send { .. }
            | Cmd::ContextMenu { .. },
        ) => unreachable!("handled above"),
        None => (store.load()?.role(), false),
    };
    // 常驻后台也要保持实时响应（关闭 App Nap / 后台节能）
    roam_input::power::keep_responsive();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (app, url) = runtime.block_on(async {
        let app = match role {
            Role::Host => App::Host(start_host(store, demo).await?),
            Role::Client => App::Client(Client::start(store).await?),
        };
        let addr = ui::serve(app.clone(), cli.ui_port).await?;
        tracing::info!("界面：http://{addr}");
        anyhow::Ok((app, format!("http://{addr}")))
    })?;

    if cli.no_tray {
        return runtime.block_on(async {
            tokio::signal::ctrl_c().await?;
            if let App::Host(host) = &app {
                host.router.lock().unwrap().return_home();
                host.endpoint.close().await;
            }
            anyhow::Ok(())
        });
    }
    run_with_tray(runtime, app, url, !cli.background)
}

fn run_with_tray(
    runtime: tokio::runtime::Runtime,
    app: App,
    url: String,
    show: bool,
) -> Result<()> {
    let tray = Tray::new();
    let handle = tray.handle();
    let (spec, initial) = match &app {
        App::Host(host) => {
            {
                let _guard = runtime.enter();
                host.start_tray_updates(handle.clone());
            }
            let spec = Spec {
                pause: true,
                update: true,
                autostart: autostart::is_enabled(AUTOSTART_ARGS),
                ..Default::default()
            };
            (spec, host.tray_state())
        }
        App::Client(client) => {
            client.set_tray(handle.clone());
            let spec = Spec {
                copy_ticket: true,
                autostart: autostart::is_enabled(AUTOSTART_ARGS),
                ..Default::default()
            };
            let state = tray::State {
                status: client.status_text(),
                tooltip: "Roam 被控端".into(),
                ..Default::default()
            };
            (spec, state)
        }
    };

    let browser_url = url.clone();
    // 运行时必须保持存活，交给闭包持有
    tray.run(spec, initial, url, show, move |action| {
        let _keep = &runtime;
        match action {
            Action::OpenInBrowser => open_browser(&browser_url),
            Action::CopyTicket => {
                if let App::Client(client) = &app {
                    copy_to_clipboard(&client.ticket);
                }
            }
            Action::Pause(paused) => {
                if let App::Host(host) = &app {
                    host.set_paused(paused);
                }
            }
            Action::Update => {
                if let App::Host(host) = &app {
                    let host = host.clone();
                    runtime.spawn(async move {
                        let checked = host.updater.state.lock().unwrap().latest.is_some();
                        if checked {
                            let _ = host.update_all().await;
                        } else {
                            let _ = host.updater.check().await;
                        }
                    });
                }
            }
            Action::Autostart(on) => {
                if let Err(err) = autostart::set_enabled(AUTOSTART_ARGS, on) {
                    dialog::alert("Roam", &format!("设置开机自启失败：{err:#}"));
                }
            }
            Action::Quit => {
                if let App::Host(host) = &app {
                    host.router.lock().unwrap().return_home();
                }
                #[cfg(windows)]
                roam_input::windows::restore_cursors();
                std::process::exit(0);
            }
        }
    })
}

/// Windows 锁屏控制：系统服务与输入助手（SYSTEM 身份运行，不显示界面）。
fn run_system_component(cmd: Cmd) -> Result<()> {
    #[cfg(windows)]
    {
        match cmd {
            Cmd::Service { action, log_dir } => {
                logging::init_named(&log_dir, "service.log");
                let result = match action.as_str() {
                    "install" => winsvc::install(&log_dir),
                    "uninstall" => winsvc::uninstall(),
                    "allow-elevated" => winsvc::set_allow_elevated(true),
                    "deny-elevated" => winsvc::set_allow_elevated(false),
                    "run" => winsvc::run_service(),
                    other => Err(anyhow::anyhow!("未知操作 {other}")),
                };
                if let Err(err) = &result {
                    tracing::error!("服务操作 {action} 失败: {err:#}");
                }
                result
            }
            Cmd::InputHelper { log_dir } => {
                logging::init_named(&log_dir, "helper.log");
                let result = winsvc::run_helper();
                if let Err(err) = &result {
                    tracing::error!("输入助手退出: {err:#}");
                }
                result
            }
            _ => Ok(()),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
        anyhow::bail!("锁屏控制服务只在 Windows 上使用")
    }
}

fn copy_to_clipboard(text: &str) {
    let result = clipboard::with_clipboard(|c| c.set_text(text.to_string()));
    if !matches!(result, Ok(Ok(()))) {
        dialog::alert("Roam", "复制失败，请稍后再试");
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
    println!("已添加。以主机身份运行 roam 后会自动连接。");
    Ok(())
}

async fn start_host(store: Store, demo: bool) -> Result<Arc<Host>> {
    #[cfg(windows)]
    roam_input::windows::init_dpi();

    let endpoint = conn::bind(&store, None).await?;

    #[cfg(target_os = "macos")]
    let cursor: Box<dyn roam_core::router::CursorControl> =
        Box::new(roam_input::mac::MacCursor::new());
    #[cfg(windows)]
    let cursor: Box<dyn roam_core::router::CursorControl> =
        Box::new(roam_input::windows::WinCursor::new());
    #[cfg(not(any(target_os = "macos", windows)))]
    let cursor: Box<dyn roam_core::router::CursorControl> = Box::new(host::NoopCursor);

    let host = Host::new(store, endpoint, cursor)?;
    host.start();
    if demo {
        // 演示模式用程序模拟鼠标，不采集真实键鼠
        *host.capture.lock().unwrap() = Err("演示模式".into());
        tokio::spawn(run_demo(host.clone()));
    } else {
        start_capture(&host);
    }
    tracing::info!("主机「{}」已启动", host.config.lock().unwrap().name);
    Ok(host)
}

#[cfg(target_os = "macos")]
fn start_capture(host: &Arc<Host>) {
    if let Err(err) = roam_input::mac::check_permissions() {
        tracing::warn!("{err:#}");
        *host.capture.lock().unwrap() = Err(format!("{err:#}"));
        return;
    }
    spawn_capture(host, roam_input::mac::run_capture);
}

#[cfg(windows)]
fn start_capture(host: &Arc<Host>) {
    spawn_capture(host, roam_input::windows::run_capture);
}

#[cfg(not(any(target_os = "macos", windows)))]
fn start_capture(host: &Arc<Host>) {
    *host.capture.lock().unwrap() = Err("这个系统暂不支持作为主机".into());
}

/// 采集层回调：返回 true 表示吞掉事件。
#[cfg(any(target_os = "macos", windows))]
type CaptureHandler = Box<dyn FnMut(InputEvent) -> bool + Send>;

#[cfg(any(target_os = "macos", windows))]
fn spawn_capture(host: &Arc<Host>, run: fn(CaptureHandler) -> Result<()>) {
    let router = host.router.clone();
    let capture = host.clone();
    let paste = host.clone();
    std::thread::Builder::new()
        .name("roam-capture".into())
        .spawn(move || {
            *capture.capture.lock().unwrap() = Ok(());
            // 光标在别的电脑上时本机键鼠事件都被拦下，定期告诉系统用户仍在操作，免得本机息屏、睡眠
            let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
            {
                let active = active.clone();
                std::thread::Builder::new()
                    .name("roam-activity".into())
                    .spawn(move || {
                        loop {
                            std::thread::sleep(Duration::from_secs(5));
                            if active.swap(false, std::sync::atomic::Ordering::Relaxed) {
                                roam_input::power::declare_user_active();
                            }
                        }
                    })
                    .expect("spawn activity thread");
            }
            let result = run(Box::new(move |ev| {
                // 本机按粘贴、而最近复制的是别的设备上的文件：先去取文件
                if let InputEvent::Key { usage, down } = ev
                    && paste.intercept_local_key(usage, down)
                {
                    return true;
                }
                let swallowed = router.lock().unwrap().handle(ev);
                if swallowed {
                    active.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                swallowed
            }));
            if let Err(err) = result {
                tracing::error!("键鼠采集失败: {err:#}");
                *capture.capture.lock().unwrap() = Err(format!("{err:#}"));
            }
        })
        .expect("spawn capture thread");
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
        tracing::warn!("请手动在浏览器打开 {url}");
    }
}

/// 演示：模拟鼠标从主机右边缘移到被控设备，画一圈后再回来。
async fn run_demo(host: Arc<Host>) {
    tracing::info!("[演示] 等待被控设备上线...");
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

    tracing::info!("[演示] 向右移动，越过主机右边缘...");
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
        tracing::warn!("[演示] ✗ 没能切到被控设备，请检查布局里设备是否在主机右边的格子");
        return;
    };
    tracing::info!("[演示] ✓ 已切到 {target}，停留 3 秒后在对方屏幕上画圈...");
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
    tracing::info!("[演示] 向左移回主机...");
    for _ in 0..1000 {
        feed(InputEvent::Move {
            x,
            y,
            dx: -20.0,
            dy: 0.0,
        });
        if host.router.lock().unwrap().focused().is_none() {
            tracing::info!("[演示] ✓ 已回到主机，链路正常");
            return;
        }
        tokio::time::sleep(step).await;
    }
    tracing::warn!("[演示] ✗ 没能回到主机");
}
