//! Windows：显示器信息与输入注入（SendInput）。

use std::collections::HashSet;

use roam_core::keymap::hid_to_win;
use roam_proto::{DisplayInfo, MouseButton, Rect};
use windows_sys::Win32::{
    Foundation::{LPARAM, RECT},
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO},
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
            SetProcessDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
            MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
            MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
            MOUSEINPUT, SendInput,
        },
        WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
        },
    },
};

/// 标记自己注入的事件（dwExtraInfo），本机的采集钩子看到后会忽略。
pub const INJECTED_MARK: usize = 0x524F_414D;

/// 一个 WHEEL_DELTA 大约对应多少像素（Windows 默认一格滚 3 行）。
const PIXELS_PER_NOTCH: f32 = 48.0;

#[derive(Clone, Copy)]
struct Monitor {
    logical: Rect,
    pixel: RECT,
    scale: f64,
}

/// 让进程按物理像素看待屏幕，否则高 DPI 下坐标会被系统缩放。
pub fn init_dpi() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> windows_sys::core::BOOL {
        let out = unsafe { &mut *(data as *mut Vec<HMONITOR>) };
        out.push(hmon);
        1
    }

    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            &mut handles as *mut _ as LPARAM,
        );
    }

    handles
        .into_iter()
        .filter_map(|h| {
            let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
            info.cbSize = size_of::<MONITORINFO>() as u32;
            if unsafe { GetMonitorInfoW(h, &mut info) } == 0 {
                return None;
            }
            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
            let scale = dpi_x.max(96) as f64 / 96.0;
            let r = info.rcMonitor;
            Some(Monitor {
                logical: Rect {
                    x: r.left as f64 / scale,
                    y: r.top as f64 / scale,
                    w: (r.right - r.left) as f64 / scale,
                    h: (r.bottom - r.top) as f64 / scale,
                },
                pixel: r,
                scale,
            })
        })
        .collect()
}

pub fn displays() -> Vec<DisplayInfo> {
    monitors()
        .into_iter()
        .map(|m| DisplayInfo {
            rect: m.logical,
            scale: m.scale,
        })
        .collect()
}

/// 把收到的输入注入到本机，并记录按下的键，便于断线时全部释放。
pub struct Injector {
    monitors: Vec<Monitor>,
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    last_seq: Option<u32>,
    wheel_acc: (f32, f32),
}

impl Default for Injector {
    fn default() -> Self {
        Self::new()
    }
}

impl Injector {
    pub fn new() -> Self {
        Self {
            monitors: monitors(),
            keys: HashSet::new(),
            buttons: HashSet::new(),
            last_seq: None,
            wheel_acc: (0.0, 0.0),
        }
    }

    /// 显示器变化（插拔、改缩放）后调用。
    pub fn refresh(&mut self) {
        self.monitors = monitors();
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
        let (px, py) = self.to_pixel(x as f64, y as f64);
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        // 绝对坐标归一化到 0..=65535，覆盖整个虚拟桌面
        let nx = ((px - vx as f64) * 65535.0 / (vw - 1).max(1) as f64).round() as i32;
        let ny = ((py - vy as f64) * 65535.0 / (vh - 1).max(1) as f64).round() as i32;
        send_mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        );
    }

    fn to_pixel(&self, x: f64, y: f64) -> (f64, f64) {
        let inside = |m: &&Monitor| {
            x >= m.logical.x
                && x < m.logical.x + m.logical.w
                && y >= m.logical.y
                && y < m.logical.y + m.logical.h
        };
        let nearest = |m: &&Monitor| {
            let cx = x.clamp(m.logical.x, m.logical.x + m.logical.w);
            let cy = y.clamp(m.logical.y, m.logical.y + m.logical.h);
            (cx - x).powi(2) + (cy - y).powi(2)
        };
        let Some(m) = self.monitors.iter().find(inside).or_else(|| {
            self.monitors
                .iter()
                .min_by(|a, b| nearest(a).total_cmp(&nearest(b)))
        }) else {
            return (x, y);
        };
        let lx = x.clamp(m.logical.x, m.logical.x + m.logical.w - 1.0 / m.scale);
        let ly = y.clamp(m.logical.y, m.logical.y + m.logical.h - 1.0 / m.scale);
        (
            m.pixel.left as f64 + (lx - m.logical.x) * m.scale,
            m.pixel.top as f64 + (ly - m.logical.y) * m.scale,
        )
    }

    pub fn button(&mut self, button: MouseButton, down: bool) {
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        send_mouse(0, 0, data as u32, flags);
    }

    pub fn key(&mut self, usage: u16, down: bool) {
        // Windows 不认程序注入的 Win+L，识别出来后直接调用系统锁屏
        const KEY_L: u16 = 0x0F;
        let win_held = self.keys.contains(&roam_core::keymap::hid::LEFT_GUI)
            || self.keys.contains(&roam_core::keymap::hid::RIGHT_GUI);
        if usage == KEY_L && win_held {
            if down {
                self.lock_screen();
            }
            return;
        }
        let Some((scan, extended)) = hid_to_win(usage) else {
            tracing::debug!("未映射的按键 HID {usage:#x}");
            return;
        };
        if down {
            self.keys.insert(usage);
        } else {
            self.keys.remove(&usage);
        }
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_MARK,
                },
            },
        };
        send(&[input]);
    }

    /// 行滚动，120 = 一格。
    pub fn wheel_lines(&mut self, dx: i32, dy: i32) {
        if dy != 0 {
            send_mouse(0, 0, dy as u32, MOUSEEVENTF_WHEEL);
        }
        if dx != 0 {
            send_mouse(0, 0, dx as u32, MOUSEEVENTF_HWHEEL);
        }
    }

    /// 像素滚动：换算成 WHEEL_DELTA 的分数，累积到至少 1 个单位再发。
    pub fn wheel_pixels(&mut self, dx: f32, dy: f32) {
        self.wheel_acc.0 += dx * 120.0 / PIXELS_PER_NOTCH;
        self.wheel_acc.1 += dy * 120.0 / PIXELS_PER_NOTCH;
        let (wx, wy) = (self.wheel_acc.0.trunc(), self.wheel_acc.1.trunc());
        self.wheel_acc.0 -= wx;
        self.wheel_acc.1 -= wy;
        self.wheel_lines(wx as i32, wy as i32);
    }

    /// 按控制端的按键状态纠正：控制端认为没按下的键全部抬起。
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
        self.wheel_acc = (0.0, 0.0);
    }

    /// 锁定这台电脑。先松开所有按键，避免解锁后卡键。
    pub fn lock_screen(&mut self) {
        // Win 键按下后直接抬起会弹出开始菜单，先插一个 Ctrl 按键"打断"它
        let win_held = self.keys.contains(&roam_core::keymap::hid::LEFT_GUI)
            || self.keys.contains(&roam_core::keymap::hid::RIGHT_GUI);
        if win_held {
            let ctrl = roam_core::keymap::hid::LEFT_CTRL;
            self.key(ctrl, true);
            self.key(ctrl, false);
        }
        self.release_all();
        unsafe {
            windows_sys::Win32::System::Shutdown::LockWorkStation();
        }
    }

    /// 新的会话开始时重置乱序判断。
    pub fn reset_sequence(&mut self) {
        self.last_seq = None;
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

fn send_mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_MARK,
            },
        },
    };
    send(&[input]);
}

fn send(inputs: &[INPUT]) {
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if sent as usize != inputs.len() {
        // 常见原因：目标窗口是管理员权限（UIPI），或处于锁屏 / UAC 安全桌面
        tracing::debug!("SendInput 被系统拦截");
    }
}

// ---------------------------------------------------------------- 采集（主机）

use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, AtomicI32, Ordering},
};

use roam_core::{
    keymap::win_to_hid,
    router::{CursorControl, InputEvent},
};
use windows_sys::Win32::{
    Foundation::{LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CallNextHookEx, CreateCursor, DispatchMessageW, GetCursorPos, GetMessageW, KBDLLHOOKSTRUCT,
        LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, OCR_APPSTARTING,
        OCR_CROSS, OCR_HAND, OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS,
        OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, SPI_SETCURSORS, SetCursorPos, SetSystemCursor,
        SetWindowsHookExW, SystemParametersInfoW, TranslateMessage, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
        WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    },
};

type Handler = Box<dyn FnMut(InputEvent) -> bool + Send>;

/// 钩子回调没有用户数据参数，只能用全局状态。
static HANDLER: StdMutex<Option<Handler>> = StdMutex::new(None);
static MONITORS: StdMutex<Vec<Monitor>> = StdMutex::new(Vec::new());
/// 锁定时光标停在这里（物理像素），每次移动都算出相对这里的位移，再把事件吞掉。
static LOCKED: AtomicBool = AtomicBool::new(false);
static PARK_X: AtomicI32 = AtomicI32::new(0);
static PARK_Y: AtomicI32 = AtomicI32::new(0);
static LAST_X: AtomicI32 = AtomicI32::new(i32::MIN);
static LAST_Y: AtomicI32 = AtomicI32::new(0);
/// 测试用：接受其他程序注入的输入（平时忽略，避免和远程控制软件互相干扰）。
static ACCEPT_INJECTED: AtomicBool = AtomicBool::new(false);

fn monitors_snapshot() -> Vec<Monitor> {
    let mut cached = MONITORS.lock().unwrap();
    if cached.is_empty() {
        *cached = monitors();
    }
    cached.clone()
}

/// 物理像素 → 逻辑点（返回所在显示器的缩放比例）。
fn pixel_to_logical(px: i32, py: i32) -> (f64, f64, f64) {
    let mons = monitors_snapshot();
    let inside = |m: &&Monitor| {
        px >= m.pixel.left && px < m.pixel.right && py >= m.pixel.top && py < m.pixel.bottom
    };
    let Some(m) = mons.iter().find(inside).or_else(|| mons.first()) else {
        return (px as f64, py as f64, 1.0);
    };
    (
        m.logical.x + (px - m.pixel.left) as f64 / m.scale,
        m.logical.y + (py - m.pixel.top) as f64 / m.scale,
        m.scale,
    )
}

fn logical_to_pixel(x: f64, y: f64) -> (i32, i32) {
    let mons = monitors_snapshot();
    let nearest = mons.iter().min_by(|a, b| {
        let d = |m: &Monitor| {
            let cx = x.clamp(m.logical.x, m.logical.x + m.logical.w);
            let cy = y.clamp(m.logical.y, m.logical.y + m.logical.h);
            (cx - x).powi(2) + (cy - y).powi(2)
        };
        d(a).total_cmp(&d(b))
    });
    let Some(m) = nearest else {
        return (x as i32, y as i32);
    };
    (
        m.pixel.left + ((x - m.logical.x) * m.scale).round() as i32,
        m.pixel.top + ((y - m.logical.y) * m.scale).round() as i32,
    )
}

fn dispatch(ev: InputEvent) -> bool {
    match HANDLER.lock() {
        Ok(mut guard) => guard.as_mut().is_some_and(|h| h(ev)),
        Err(_) => false,
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
    let ours = info.dwExtraInfo == INJECTED_MARK;
    let injected = info.flags & LLMHF_INJECTED != 0;
    if ours || (injected && !ACCEPT_INJECTED.load(Ordering::Relaxed)) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let wheel = (info.mouseData >> 16) as i16 as i32;
    let event = match wparam as u32 {
        WM_MOUSEMOVE => {
            let (px, py) = (info.pt.x, info.pt.y);
            let (lx, ly, scale) = pixel_to_logical(px, py);
            let (dx, dy) = if LOCKED.load(Ordering::Relaxed) {
                let (ox, oy) = (
                    PARK_X.load(Ordering::Relaxed),
                    PARK_Y.load(Ordering::Relaxed),
                );
                ((px - ox) as f64 / scale, (py - oy) as f64 / scale)
            } else {
                let last_x = LAST_X.load(Ordering::Relaxed);
                let d = if last_x == i32::MIN {
                    (0.0, 0.0)
                } else {
                    (
                        (px - last_x) as f64 / scale,
                        (py - LAST_Y.load(Ordering::Relaxed)) as f64 / scale,
                    )
                };
                LAST_X.store(px, Ordering::Relaxed);
                LAST_Y.store(py, Ordering::Relaxed);
                d
            };
            Some(InputEvent::Move {
                x: lx,
                y: ly,
                dx,
                dy,
            })
        }
        WM_LBUTTONDOWN => Some(button_event(MouseButton::Left, true)),
        WM_LBUTTONUP => Some(button_event(MouseButton::Left, false)),
        WM_RBUTTONDOWN => Some(button_event(MouseButton::Right, true)),
        WM_RBUTTONUP => Some(button_event(MouseButton::Right, false)),
        WM_MBUTTONDOWN => Some(button_event(MouseButton::Middle, true)),
        WM_MBUTTONUP => Some(button_event(MouseButton::Middle, false)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let down = wparam as u32 == WM_XBUTTONDOWN;
            let button = if wheel == 1 {
                MouseButton::Back
            } else {
                MouseButton::Forward
            };
            Some(button_event(button, down))
        }
        WM_MOUSEWHEEL => Some(InputEvent::WheelLines { dx: 0, dy: wheel }),
        WM_MOUSEHWHEEL => Some(InputEvent::WheelLines { dx: wheel, dy: 0 }),
        _ => None,
    };

    let swallow = event.is_some_and(dispatch);
    // 锁定期间所有鼠标事件都吞掉，光标就一直停在原地
    if swallow || LOCKED.load(Ordering::Relaxed) {
        1
    } else {
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

fn button_event(button: MouseButton, down: bool) -> InputEvent {
    InputEvent::Button { button, down }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let ours = info.dwExtraInfo == INJECTED_MARK;
    let injected = info.flags & LLKHF_INJECTED != 0;
    if ours || (injected && !ACCEPT_INJECTED.load(Ordering::Relaxed)) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let down = match wparam as u32 {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) },
    };
    const VK_NUMLOCK: u32 = 0x90;
    const VK_PAUSE: u32 = 0x13;
    let usage = match info.vkCode {
        VK_NUMLOCK => Some(0x53),
        VK_PAUSE => None,
        _ => win_to_hid(info.scanCode as u16, info.flags & LLKHF_EXTENDED != 0),
    };
    let swallow = usage.is_some_and(|usage| dispatch(InputEvent::Key { usage, down }));
    if swallow {
        1
    } else {
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

/// 安装键鼠钩子并在当前线程运行消息循环（不返回）。
///
/// `handler` 对每个输入事件返回 `true` 表示吞掉（不交给 Windows）。
pub fn run_capture(handler: impl FnMut(InputEvent) -> bool + Send + 'static) -> anyhow::Result<()> {
    ACCEPT_INJECTED.store(
        std::env::var_os("ROAM_ACCEPT_INJECTED").is_some(),
        Ordering::Relaxed,
    );
    *MONITORS.lock().unwrap() = monitors();
    *HANDLER.lock().unwrap() = Some(Box::new(handler));
    // 低级钩子处理太慢会被系统悄悄摘掉，钩子线程用最高优先级
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) };
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0) };
    if mouse.is_null() || keyboard.is_null() {
        anyhow::bail!("无法安装键鼠钩子");
    }
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    unsafe {
        UnhookWindowsHookEx(mouse);
        UnhookWindowsHookEx(keyboard);
    }
    Ok(())
}

/// 恢复系统光标（隐藏光标是通过替换系统光标实现的）。启动时也调用一次，防止上次异常退出后光标不见了。
pub fn restore_cursors() {
    unsafe {
        SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0);
    }
}

/// 主机光标控制。
pub struct WinCursor {
    hidden: bool,
}

impl WinCursor {
    pub fn new() -> Self {
        restore_cursors();
        Self { hidden: false }
    }
}

impl Default for WinCursor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WinCursor {
    fn drop(&mut self) {
        restore_cursors();
    }
}

impl CursorControl for WinCursor {
    fn hide(&mut self) {
        if self.hidden {
            return;
        }
        self.hidden = true;
        // 把各种系统光标都换成全透明光标
        let and_mask = [0xFFu8; 32 * 4];
        let xor_mask = [0u8; 32 * 4];
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        for id in [
            OCR_NORMAL,
            OCR_IBEAM,
            OCR_WAIT,
            OCR_CROSS,
            OCR_UP,
            OCR_SIZENWSE,
            OCR_SIZENESW,
            OCR_SIZEWE,
            OCR_SIZENS,
            OCR_SIZEALL,
            OCR_NO,
            OCR_HAND,
            OCR_APPSTARTING,
        ] {
            unsafe {
                let blank = CreateCursor(
                    module,
                    0,
                    0,
                    32,
                    32,
                    and_mask.as_ptr().cast(),
                    xor_mask.as_ptr().cast(),
                );
                if !blank.is_null() {
                    SetSystemCursor(blank, id);
                }
            }
        }
    }

    fn show(&mut self) {
        if self.hidden {
            self.hidden = false;
            restore_cursors();
        }
    }

    fn warp(&mut self, x: f64, y: f64) {
        let (px, py) = logical_to_pixel(x, y);
        unsafe { SetCursorPos(px, py) };
        LAST_X.store(px, Ordering::Relaxed);
        LAST_Y.store(py, Ordering::Relaxed);
    }

    fn set_locked(&mut self, locked: bool) {
        if locked {
            let mut pt = POINT { x: 0, y: 0 };
            unsafe { GetCursorPos(&mut pt) };
            PARK_X.store(pt.x, Ordering::Relaxed);
            PARK_Y.store(pt.y, Ordering::Relaxed);
        }
        LOCKED.store(locked, Ordering::Relaxed);
    }
}
