//! macOS：显示器信息、键鼠采集（CGEventTap）与主机光标控制。
//!
//! 需要两项系统权限：辅助功能（拦截并吞掉事件）、输入监控（读取键盘）。

use std::{
    ffi::{CStr, c_void},
    sync::{
        Mutex,
        atomic::{AtomicPtr, Ordering},
    },
};

use anyhow::{Result, bail};
use core_foundation::{
    base::TCFType,
    boolean::CFBoolean,
    mach_port::CFMachPortRef,
    runloop::{CFRunLoop, kCFRunLoopCommonModes},
    string::{CFString, CFStringRef},
};
use core_graphics::{
    display::CGDisplay,
    event::{
        CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, CallbackResult, EventField,
    },
    geometry::CGPoint,
};
use roam_core::{
    keymap::{mac_modifier_flag, mac_to_hid},
    router::{CursorControl, InputEvent},
};
use roam_proto::{DisplayInfo, MouseButton, Rect};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

/// 运行时查找可选 / 私有符号，找不到就跳过，避免链接失败。
fn dlsym(name: &CStr) -> Option<*mut c_void> {
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!ptr.is_null()).then_some(ptr)
}

pub fn displays() -> Vec<DisplayInfo> {
    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let display = CGDisplay::new(id);
            let b = display.bounds();
            let scale = if b.size.width > 0.0 {
                display.pixels_wide() as f64 / b.size.width
            } else {
                1.0
            };
            DisplayInfo {
                rect: Rect {
                    x: b.origin.x,
                    y: b.origin.y,
                    w: b.size.width,
                    h: b.size.height,
                },
                scale,
            }
        })
        .collect()
}

// ---------------------------------------------------------------- 权限

/// 检查辅助功能与输入监控权限；缺失时触发系统授权弹窗并返回错误说明。
pub fn check_permissions() -> Result<()> {
    let accessibility = unsafe { AXIsProcessTrusted() };

    type Preflight = unsafe extern "C" fn() -> bool;
    let listen = match dlsym(c"CGPreflightListenEventAccess") {
        Some(f) => unsafe { std::mem::transmute::<*mut c_void, Preflight>(f)() },
        None => true, // 10.15 以前没有这项权限
    };

    if accessibility && listen {
        return Ok(());
    }
    if !listen && let Some(f) = dlsym(c"CGRequestListenEventAccess") {
        unsafe { std::mem::transmute::<*mut c_void, Preflight>(f)() };
    }
    let mut missing = Vec::new();
    if !accessibility {
        missing.push("辅助功能");
    }
    if !listen {
        missing.push("输入监控");
    }
    bail!(
        "缺少权限：{}。\n请打开 系统设置 → 隐私与安全性，在对应列表里勾选运行 roam 的程序\
         （例如\"终端\"或 iTerm），然后重新运行。",
        missing.join("、")
    )
}

// ---------------------------------------------------------------- 光标

/// 主机光标控制。
pub struct MacCursor;

impl MacCursor {
    pub fn new() -> Self {
        // 让后台进程也能隐藏光标（私有接口，Deskflow 同样使用）
        type DefaultConnection = unsafe extern "C" fn() -> i32;
        type SetProperty = unsafe extern "C" fn(i32, i32, CFStringRef, *const c_void) -> i32;
        if let (Some(conn), Some(set)) = (
            dlsym(c"_CGSDefaultConnection"),
            dlsym(c"CGSSetConnectionProperty"),
        ) {
            unsafe {
                let conn = std::mem::transmute::<*mut c_void, DefaultConnection>(conn)();
                let set = std::mem::transmute::<*mut c_void, SetProperty>(set);
                let key = CFString::new("SetsCursorInBackground");
                set(
                    conn,
                    conn,
                    key.as_concrete_TypeRef(),
                    CFBoolean::true_value().as_CFTypeRef(),
                );
            }
        }
        Self
    }
}

impl Default for MacCursor {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorControl for MacCursor {
    fn hide(&mut self) {
        let _ = CGDisplay::main().hide_cursor();
    }

    fn show(&mut self) {
        let _ = CGDisplay::main().show_cursor();
    }

    fn warp(&mut self, x: f64, y: f64) {
        let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
        // warp 之后系统默认会屏蔽本地鼠标 0.25 秒，回到 Mac 时会感觉"卡一下"
        type Suppress = unsafe extern "C" fn(f64) -> i32;
        if let Some(f) = dlsym(c"CGSetLocalEventsSuppressionInterval") {
            unsafe { std::mem::transmute::<*mut c_void, Suppress>(f)(0.0) };
        }
    }

    fn set_locked(&mut self, locked: bool) {
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(!locked);
    }
}

// ---------------------------------------------------------------- 采集

static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// 在当前线程安装事件 tap 并运行 run loop（不返回）。
///
/// `handler` 对每个输入事件返回 `true` 表示吞掉（不交给 macOS）。
pub fn run_capture(handler: impl FnMut(InputEvent) -> bool + Send + 'static) -> Result<()> {
    let handler = Mutex::new(handler);
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::ScrollWheel,
        ],
        move |_proxy, etype, event| {
            if matches!(
                etype,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                let port = TAP_PORT.load(Ordering::Acquire);
                if !port.is_null() {
                    unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
                }
                return CallbackResult::Keep;
            }
            let mut handler = handler.lock().unwrap();
            let swallow = translate(etype, event)
                .into_iter()
                .fold(false, |acc, ev| handler(ev) | acc);
            if swallow {
                CallbackResult::Drop
            } else {
                CallbackResult::Keep
            }
        },
    )
    .map_err(|_| anyhow::anyhow!("无法创建事件 tap（通常是缺少辅助功能权限）"))?;

    TAP_PORT.store(
        tap.mach_port().as_concrete_TypeRef() as *mut c_void,
        Ordering::Release,
    );
    let source = tap
        .mach_port()
        .create_runloop_source(0)
        .map_err(|_| anyhow::anyhow!("无法创建 run loop source"))?;
    unsafe { CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes) };
    tap.enable();
    CFRunLoop::run_current();
    Ok(())
}

/// CGEvent → 平台无关的输入事件。一个事件可能对应多个（例如 Caps Lock 的按下 + 抬起）。
fn translate(etype: CGEventType, event: &CGEvent) -> Vec<InputEvent> {
    let button = |down: bool, b: MouseButton| vec![InputEvent::Button { button: b, down }];
    match etype {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            let loc = event.location();
            vec![InputEvent::Move {
                x: loc.x,
                y: loc.y,
                dx: event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X),
                dy: event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y),
            }]
        }
        CGEventType::LeftMouseDown => button(true, MouseButton::Left),
        CGEventType::LeftMouseUp => button(false, MouseButton::Left),
        CGEventType::RightMouseDown => button(true, MouseButton::Right),
        CGEventType::RightMouseUp => button(false, MouseButton::Right),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            let down = matches!(etype, CGEventType::OtherMouseDown);
            match event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
                2 => button(down, MouseButton::Middle),
                3 => button(down, MouseButton::Back),
                4 => button(down, MouseButton::Forward),
                _ => vec![],
            }
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let down = matches!(etype, CGEventType::KeyDown);
            mac_to_hid(keycode)
                .map(|usage| vec![InputEvent::Key { usage, down }])
                .unwrap_or_default()
        }
        CGEventType::FlagsChanged => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let (Some(usage), Some(flag)) = (mac_to_hid(keycode), mac_modifier_flag(keycode))
            else {
                return vec![];
            };
            if usage == roam_core::keymap::hid::CAPS_LOCK {
                // macOS 只报告大写锁定状态切换，对方需要一次完整的按下 + 抬起
                return vec![
                    InputEvent::Key { usage, down: true },
                    InputEvent::Key { usage, down: false },
                ];
            }
            let down = event.get_flags().bits() & flag != 0;
            vec![InputEvent::Key { usage, down }]
        }
        CGEventType::ScrollWheel => {
            let continuous =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0;
            // macOS 横向正值为向左，线协议约定正值向右
            if continuous {
                let dy =
                    event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
                let dx =
                    event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2);
                vec![InputEvent::WheelPixels {
                    dx: -dx as f32,
                    dy: dy as f32,
                }]
            } else {
                let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
                let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
                vec![InputEvent::WheelLines {
                    dx: (-dx * 120) as i32,
                    dy: (dy * 120) as i32,
                }]
            }
        }
        _ => vec![],
    }
}
