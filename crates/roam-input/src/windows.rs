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
                    dwExtraInfo: 0,
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
                dwExtraInfo: 0,
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
