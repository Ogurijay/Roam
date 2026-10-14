//! 托盘图标（Windows 右下角）/ 菜单栏图标（macOS），以及应用窗口。
//!
//! 托盘和窗口必须运行在主线程的事件循环里，网络逻辑在后台的 tokio 运行时中，
//! 两者通过 [`TrayHandle`] 通信。窗口里是一个 WebView，加载本机的界面服务；
//! 关闭窗口只是隐藏，程序继续在托盘里运行（和 Deskflow 一样）。

use tao::{
    dpi::LogicalSize,
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget},
    window::{Window, WindowBuilder},
};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use wry::{WebView, WebViewBuilder};

/// 菜单项被点击后交给业务逻辑处理的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 应用窗口打不开（例如系统缺少 WebView 组件）时，改用浏览器打开界面。
    OpenInBrowser,
    CopyTicket,
    Pause(bool),
    Autostart(bool),
    /// 有新版本时更新，否则检查更新。
    Update,
    Quit,
}

/// 托盘显示的状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    /// macOS 菜单栏图标旁的文字（Windows 忽略）。
    pub title: Option<String>,
    pub tooltip: String,
    /// 菜单第一行的状态说明。
    pub status: String,
    pub paused: bool,
    /// 更新菜单项的文字；None 时显示「检查更新」。
    pub update_label: Option<String>,
}

/// 需要显示哪些菜单项。
#[derive(Debug, Clone, Copy, Default)]
pub struct Spec {
    pub copy_ticket: bool,
    pub pause: bool,
    pub autostart: bool,
    pub update: bool,
}

pub enum UserEvent {
    Update(State),
    Menu(MenuEvent),
    TrayClick,
}

#[derive(Clone)]
pub struct TrayHandle(EventLoopProxy<UserEvent>);

impl TrayHandle {
    pub fn update(&self, state: State) {
        let _ = self.0.send_event(UserEvent::Update(state));
    }
}

pub struct Tray {
    event_loop: EventLoop<UserEvent>,
}

impl Default for Tray {
    fn default() -> Self {
        Self::new()
    }
}

/// 应用窗口：第一次打开时创建，之后只是显示 / 隐藏。
struct AppWindow {
    window: Window,
    _webview: WebView,
}

impl Tray {
    pub fn new() -> Self {
        #[allow(unused_mut)]
        let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
            // 只在菜单栏显示，不出现在程序坞
            event_loop.set_activation_policy(ActivationPolicy::Accessory);
        }
        Self { event_loop }
    }

    pub fn handle(&self) -> TrayHandle {
        TrayHandle(self.event_loop.create_proxy())
    }

    /// 在主线程运行事件循环，不返回。`url` 为界面地址，`show_on_start` 为启动时是否打开窗口。
    pub fn run(
        self,
        spec: Spec,
        initial: State,
        url: String,
        show_on_start: bool,
        on_action: impl Fn(Action) + 'static,
    ) -> ! {
        let proxy = self.event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = proxy.send_event(UserEvent::Menu(e));
        }));
        let proxy = self.event_loop.create_proxy();
        TrayIconEvent::set_event_handler(Some(move |e| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                let _ = proxy.send_event(UserEvent::TrayClick);
            }
        }));

        let status = MenuItem::new(&initial.status, false, None);
        let open_ui = MenuItem::new("打开 Roam", true, None);
        let copy_ticket = MenuItem::new("复制连接码", true, None);
        let pause = CheckMenuItem::new("暂停切屏", true, initial.paused, None);
        let autostart = CheckMenuItem::new("开机自动启动", true, spec.autostart, None);
        let update = MenuItem::new(
            initial.update_label.as_deref().unwrap_or("检查更新"),
            true,
            None,
        );
        let quit = MenuItem::new("退出 Roam", true, None);

        let menu = Menu::new();
        let _ = menu.append(&status);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&open_ui);
        if spec.copy_ticket {
            let _ = menu.append(&copy_ticket);
        }
        if spec.pause {
            let _ = menu.append(&pause);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        if spec.update {
            let _ = menu.append(&update);
        }
        let _ = menu.append(&autostart);
        let _ = menu.append(&quit);

        let mut tray: Option<TrayIcon> = None;
        let mut pending_menu = Some(menu);
        let mut current = initial;
        let mut app_window: Option<AppWindow> = None;

        self.event_loop.run(move |event, target, control_flow| {
            *control_flow = ControlFlow::Wait;
            let mut show = |target: &EventLoopWindowTarget<UserEvent>| {
                if !show_window(&mut app_window, target, &url) {
                    on_action(Action::OpenInBrowser);
                }
            };
            match event {
                // macOS 要求事件循环启动后再创建状态栏图标
                Event::NewEvents(StartCause::Init) => {
                    if let Some(menu) = pending_menu.take() {
                        let builder = TrayIconBuilder::new()
                            .with_menu(Box::new(menu))
                            .with_tooltip(&current.tooltip);
                        // macOS 用模板图标，自动适配浅色 / 深色菜单栏；点击图标弹出菜单
                        #[cfg(target_os = "macos")]
                        let builder = {
                            let b = builder.with_icon_templated(icon());
                            match &current.title {
                                Some(title) => b.with_title(title),
                                None => b,
                            }
                        };
                        // Windows：左键打开窗口，右键弹出菜单
                        #[cfg(not(target_os = "macos"))]
                        let builder = builder.with_icon(icon()).with_menu_on_left_click(false);
                        tray = builder.build().ok();
                    }
                    if show_on_start {
                        show(target);
                    }
                }
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    // 关闭窗口只是隐藏，程序继续在托盘里运行
                    if let Some(w) = &app_window {
                        w.window.set_visible(false);
                    }
                }
                Event::UserEvent(UserEvent::TrayClick) => show(target),
                Event::UserEvent(UserEvent::Update(state)) => {
                    if state != current {
                        status.set_text(&state.status);
                        pause.set_checked(state.paused);
                        update.set_text(state.update_label.as_deref().unwrap_or("检查更新"));
                        if let Some(tray) = &tray {
                            let _ = tray.set_tooltip(Some(&state.tooltip));
                            #[cfg(target_os = "macos")]
                            tray.set_title(state.title.as_deref());
                        }
                        current = state;
                    }
                }
                Event::UserEvent(UserEvent::Menu(e)) => {
                    let id = e.id();
                    if id == open_ui.id() {
                        show(target);
                        return;
                    }
                    let action = if id == copy_ticket.id() {
                        Some(Action::CopyTicket)
                    } else if id == pause.id() {
                        Some(Action::Pause(pause.is_checked()))
                    } else if id == update.id() {
                        Some(Action::Update)
                    } else if id == autostart.id() {
                        Some(Action::Autostart(autostart.is_checked()))
                    } else if id == quit.id() {
                        Some(Action::Quit)
                    } else {
                        None
                    };
                    if let Some(action) = action {
                        on_action(action);
                    }
                }
                _ => {}
            }
        })
    }
}

/// 显示应用窗口（第一次调用时创建）。创建失败返回 false。
fn show_window(
    slot: &mut Option<AppWindow>,
    target: &EventLoopWindowTarget<UserEvent>,
    url: &str,
) -> bool {
    if let Some(w) = slot {
        w.window.set_visible(true);
        w.window.set_minimized(false);
        w.window.set_focus();
        return true;
    }
    let window = match WindowBuilder::new()
        .with_title("Roam")
        .with_inner_size(LogicalSize::new(900.0, 660.0))
        .with_min_inner_size(LogicalSize::new(760.0, 540.0))
        .build(target)
    {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!("无法创建窗口: {err}");
            return false;
        }
    };
    match WebViewBuilder::new().with_url(url).build(&window) {
        Ok(webview) => {
            window.set_focus();
            *slot = Some(AppWindow {
                window,
                _webview: webview,
            });
            true
        }
        Err(err) => {
            tracing::warn!("无法创建 WebView（改用浏览器）: {err}");
            false
        }
    }
}

/// 程序生成的图标：两块相连的屏幕。macOS 作为模板图标只使用 alpha 通道。
fn icon() -> Icon {
    const N: u32 = 32;
    let mut rgba = vec![0u8; (N * N * 4) as usize];
    let color: [u8; 3] = if cfg!(target_os = "macos") {
        [0, 0, 0]
    } else {
        [0x25, 0x63, 0xeb]
    };
    let mut put = |x: u32, y: u32| {
        let i = ((y * N + x) * 4) as usize;
        rgba[i..i + 3].copy_from_slice(&color);
        rgba[i + 3] = 255;
    };
    // 左右两块屏幕的边框（线宽 3），中间一条连接线
    let screens = [(1u32, 6u32, 14u32, 14u32), (17, 12, 14, 14)];
    for &(x0, y0, w, h) in &screens {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let edge = x < x0 + 3 || x >= x0 + w - 3 || y < y0 + 3 || y >= y0 + h - 3;
                if edge {
                    put(x, y);
                }
            }
        }
    }
    for x in 12..20 {
        for y in 18..21 {
            put(x, y);
        }
    }
    Icon::from_rgba(rgba, N, N).expect("valid icon")
}
