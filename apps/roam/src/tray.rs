//! 托盘图标（Windows 右下角）/ 菜单栏图标（macOS）。
//!
//! 托盘必须运行在主线程的事件循环里，网络逻辑在后台的 tokio 运行时中，
//! 两者通过 [`TrayHandle`] 通信。

use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

/// 菜单项被点击后交给业务逻辑处理的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    OpenUi,
    CopyTicket,
    Pause(bool),
    Autostart(bool),
    RevokeHosts,
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
    pub open_ui: bool,
    pub copy_ticket: bool,
    pub pause: bool,
    pub revoke_hosts: bool,
    pub autostart: bool,
    pub update: bool,
}

pub enum UserEvent {
    Update(State),
    Menu(MenuEvent),
    Quit,
}

#[derive(Clone)]
pub struct TrayHandle(EventLoopProxy<UserEvent>);

impl TrayHandle {
    pub fn update(&self, state: State) {
        let _ = self.0.send_event(UserEvent::Update(state));
    }

    pub fn quit(&self) {
        let _ = self.0.send_event(UserEvent::Quit);
    }
}

pub struct Tray {
    event_loop: EventLoop<UserEvent>,
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

    /// 在主线程运行事件循环，不返回。
    pub fn run(self, spec: Spec, initial: State, on_action: impl Fn(Action) + 'static) -> ! {
        let proxy = self.event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = proxy.send_event(UserEvent::Menu(e));
        }));

        let status = MenuItem::new(&initial.status, false, None);
        let open_ui = MenuItem::new("打开布局页面", true, None);
        let copy_ticket = MenuItem::new("复制连接码", true, None);
        let pause = CheckMenuItem::new("暂停切屏", true, initial.paused, None);
        let revoke = MenuItem::new("撤销所有已授权的主机", true, None);
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
        if spec.open_ui {
            let _ = menu.append(&open_ui);
        }
        if spec.copy_ticket {
            let _ = menu.append(&copy_ticket);
        }
        if spec.pause {
            let _ = menu.append(&pause);
        }
        if spec.revoke_hosts {
            let _ = menu.append(&revoke);
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

        self.event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                // macOS 要求事件循环启动后再创建状态栏图标
                Event::NewEvents(tao::event::StartCause::Init) => {
                    if let Some(menu) = pending_menu.take() {
                        let builder = TrayIconBuilder::new()
                            .with_menu(Box::new(menu))
                            .with_tooltip(&current.tooltip);
                        // macOS 用模板图标，自动适配浅色 / 深色菜单栏
                        #[cfg(target_os = "macos")]
                        let builder = {
                            let b = builder.with_icon_templated(icon());
                            match &current.title {
                                Some(title) => b.with_title(title),
                                None => b,
                            }
                        };
                        #[cfg(not(target_os = "macos"))]
                        let builder = builder.with_icon(icon());
                        tray = builder.build().ok();
                    }
                }
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
                    let action = if id == open_ui.id() {
                        Some(Action::OpenUi)
                    } else if id == copy_ticket.id() {
                        Some(Action::CopyTicket)
                    } else if id == pause.id() {
                        Some(Action::Pause(pause.is_checked()))
                    } else if id == revoke.id() {
                        Some(Action::RevokeHosts)
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
                Event::UserEvent(UserEvent::Quit) => {
                    tray.take();
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        })
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
