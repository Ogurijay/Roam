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
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef)
    -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
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

/// 被控端注入键鼠只需要「辅助功能」权限；缺失时弹出系统授权提示并返回说明。
pub fn check_accessibility() -> Result<()> {
    use core_foundation::dictionary::CFDictionary;
    let trusted = unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
    };
    if trusted {
        Ok(())
    } else {
        bail!(
            "缺少「辅助功能」权限，无法控制本机的键鼠。\n请打开 系统设置 → 隐私与安全性 → 辅助功能，\
             勾选 roam（或运行它的终端），然后重新启动 Roam。"
        )
    }
}

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
    // 回调太慢会被系统暂停 tap（整个系统的键鼠都会跟着卡一下），采集线程用最高的 QoS
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
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
                tracing::warn!("事件 tap 被系统暂停（{etype:?}），已重新启用");
                let port = TAP_PORT.load(Ordering::Acquire);
                if !port.is_null() {
                    unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
                }
                return CallbackResult::Keep;
            }
            if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == INJECTED_MARK {
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

// ---------------------------------------------------------------- 注入（被控端）

use std::{collections::HashSet, time::Instant};

use core_graphics::{
    event::{CGEventFlags, CGMouseButton, ScrollEventUnit},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use roam_core::keymap::{hid, hid_to_mac};

/// 标记自己注入的事件，主机端的事件 tap 看到后会忽略，避免同一台机器上自己控制自己时形成回路。
pub const INJECTED_MARK: i64 = 0x524F_414D;

/// 系统默认的双击间隔与距离。
const DOUBLE_CLICK_MS: u128 = 500;
const DOUBLE_CLICK_DIST: f64 = 4.0;

/// 把收到的输入注入到本机（被控端）。需要「辅助功能」权限。
pub struct Injector {
    source: Option<CGEventSource>,
    pos: (f64, f64),
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    last_seq: Option<u32>,
    /// 当前按住的修饰键对应的标志位（每个按键事件都要带上，快捷键才生效）。
    flags: u64,
    caps_on: bool,
    /// 连击判断：上次按下的按钮、时间、位置、连击数。
    last_click: Option<(MouseButton, Instant, (f64, f64), i64)>,
    click_count: i64,
    wheel_acc: (f32, f32),
}

impl Default for Injector {
    fn default() -> Self {
        Self::new()
    }
}

// CGEventSource 是 CoreFoundation 对象，可以在线程间传递；注入器总是放在 Mutex 里使用，不会并发访问。
unsafe impl Send for Injector {}

impl Injector {
    pub fn new() -> Self {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok();
        let pos = source
            .clone()
            .and_then(|s| CGEvent::new(s).ok())
            .map(|e| {
                let p = e.location();
                (p.x, p.y)
            })
            .unwrap_or((0.0, 0.0));
        Self {
            source,
            pos,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            last_seq: None,
            flags: 0,
            caps_on: false,
            last_click: None,
            click_count: 1,
            wheel_acc: (0.0, 0.0),
        }
    }

    pub fn refresh(&mut self) {}

    fn source(&self) -> Option<CGEventSource> {
        self.source.clone()
    }

    fn post(&self, event: CGEvent) {
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECTED_MARK);
        event.post(CGEventTapLocation::HID);
    }

    pub fn reset_sequence(&mut self) {
        self.last_seq = None;
    }

    /// 数据报通道的绝对移动：丢弃乱序到达的旧包。
    pub fn mouse_abs(&mut self, seq: u32, x: f32, y: f32) {
        if let Some(last) = self.last_seq
            && (seq.wrapping_sub(last) as i32) <= 0
        {
            return;
        }
        self.last_seq = Some(seq);
        self.move_to(x, y);
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = (x as f64, y as f64);
        let (dx, dy) = (x - self.pos.0, y - self.pos.1);
        self.pos = (x, y);
        // 按住按钮时必须发"拖拽"事件，否则拖动窗口、选中文字都不生效
        let (etype, button) = if self.buttons.contains(&MouseButton::Left) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons.contains(&MouseButton::Right) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if !self.buttons.is_empty() {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        let Some(source) = self.source() else { return };
        if let Ok(event) = CGEvent::new_mouse_event(source, etype, CGPoint::new(x, y), button) {
            // 依赖相对位移的程序（游戏、某些绘图软件）读取这两个字段
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx.round() as i64);
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy.round() as i64);
            self.post(event);
        }
    }

    pub fn button(&mut self, button: MouseButton, down: bool) {
        if down {
            self.buttons.insert(button);
            // 连击：同一个按钮、间隔短、位置接近 → 连击数 +1（双击、三击）
            let now = Instant::now();
            self.click_count = match self.last_click {
                Some((b, at, (px, py), n))
                    if b == button
                        && now.duration_since(at).as_millis() <= DOUBLE_CLICK_MS
                        && (px - self.pos.0).abs() <= DOUBLE_CLICK_DIST
                        && (py - self.pos.1).abs() <= DOUBLE_CLICK_DIST =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.last_click = Some((button, now, self.pos, self.click_count));
        } else {
            self.buttons.remove(&button);
        }
        let (etype, cg_button, number) = match (button, down) {
            (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
            (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
            (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
            (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
            (b, d) => {
                let number = match b {
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    _ => 4,
                };
                let etype = if d {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                };
                (etype, CGMouseButton::Center, number)
            }
        };
        let Some(source) = self.source() else { return };
        let pos = CGPoint::new(self.pos.0, self.pos.1);
        if let Ok(event) = CGEvent::new_mouse_event(source, etype, pos, cg_button) {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, self.click_count);
            self.post(event);
        }
    }

    pub fn key(&mut self, usage: u16, down: bool) {
        let Some(keycode) = hid_to_mac(usage) else {
            tracing::debug!("未映射的按键 HID {usage:#x}");
            return;
        };
        if down {
            self.keys.insert(usage);
        } else {
            self.keys.remove(&usage);
        }
        let Some(source) = self.source() else { return };
        let Ok(event) = CGEvent::new_keyboard_event(source, keycode, down) else {
            return;
        };

        if usage == hid::CAPS_LOCK {
            // 大写锁定是开关：按下时切换状态
            if down {
                self.caps_on = !self.caps_on;
            }
            event.set_type(CGEventType::FlagsChanged);
            event.set_flags(CGEventFlags::from_bits_truncate(self.current_flags()));
            self.post(event);
            return;
        }
        if let Some(bits) = modifier_bits(usage) {
            // 修饰键：更新标志位，发 FlagsChanged 事件
            if down {
                self.flags |= bits;
            } else {
                self.flags &= !bits;
            }
            event.set_type(CGEventType::FlagsChanged);
        }
        event.set_flags(CGEventFlags::from_bits_truncate(self.current_flags()));
        self.post(event);
    }

    /// 锁定这台 Mac：模拟系统锁屏快捷键 Ctrl+Cmd+Q。
    pub fn lock_screen(&mut self) {
        self.release_all();
        const KVK_Q: u16 = 0x0C;
        let flags = CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagCommand;
        for down in [true, false] {
            let Some(source) = self.source() else { return };
            if let Ok(event) = CGEvent::new_keyboard_event(source, KVK_Q, down) {
                event.set_flags(flags);
                self.post(event);
            }
        }
    }

    fn current_flags(&self) -> u64 {
        let mut flags = self.flags;
        if self.caps_on {
            flags |= CGEventFlags::CGEventFlagAlphaShift.bits();
        }
        flags
    }

    /// 行滚动，120 = 一格。不足一行的部分累积起来。
    pub fn wheel_lines(&mut self, dx: i32, dy: i32) {
        self.wheel_acc.0 += dx as f32 / 120.0;
        self.wheel_acc.1 += dy as f32 / 120.0;
        let (lx, ly) = (self.wheel_acc.0.trunc(), self.wheel_acc.1.trunc());
        self.wheel_acc.0 -= lx;
        self.wheel_acc.1 -= ly;
        if lx != 0.0 || ly != 0.0 {
            // macOS 横向正值为向左
            self.scroll(ScrollEventUnit::LINE, ly as i32, -(lx as i32));
        }
    }

    pub fn wheel_pixels(&mut self, dx: f32, dy: f32) {
        self.scroll(
            ScrollEventUnit::PIXEL,
            dy.round() as i32,
            -(dx.round() as i32),
        );
    }

    fn scroll(&self, unit: u32, vertical: i32, horizontal: i32) {
        let Some(source) = self.source() else { return };
        if let Ok(event) = CGEvent::new_scroll_event(source, unit, 2, vertical, horizontal, 0) {
            self.post(event);
        }
    }

    pub fn sync_keys(&mut self, pressed: &[u16]) {
        let stale: Vec<u16> = self
            .keys
            .iter()
            .copied()
            .filter(|k| !pressed.contains(k))
            .collect();
        for usage in stale {
            self.key(usage, false);
        }
    }

    pub fn release_all(&mut self) {
        for usage in self.keys.clone() {
            self.key(usage, false);
        }
        for button in self.buttons.clone() {
            self.button(button, false);
        }
        self.flags = 0;
        self.wheel_acc = (0.0, 0.0);
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// HID 修饰键 → CGEventFlags 中的标志位（设备无关位 + 区分左右的设备相关位）。
fn modifier_bits(usage: u16) -> Option<u64> {
    let (generic, device) = match usage {
        hid::LEFT_CTRL => (CGEventFlags::CGEventFlagControl, 0x0000_0001),
        hid::RIGHT_CTRL => (CGEventFlags::CGEventFlagControl, 0x0000_2000),
        hid::LEFT_SHIFT => (CGEventFlags::CGEventFlagShift, 0x0000_0002),
        hid::RIGHT_SHIFT => (CGEventFlags::CGEventFlagShift, 0x0000_0004),
        hid::LEFT_ALT => (CGEventFlags::CGEventFlagAlternate, 0x0000_0020),
        hid::RIGHT_ALT => (CGEventFlags::CGEventFlagAlternate, 0x0000_0040),
        hid::LEFT_GUI => (CGEventFlags::CGEventFlagCommand, 0x0000_0008),
        hid::RIGHT_GUI => (CGEventFlags::CGEventFlagCommand, 0x0000_0010),
        _ => return None,
    };
    Some(generic.bits() | device)
}
