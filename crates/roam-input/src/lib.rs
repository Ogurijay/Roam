//! 平台输入层：macOS 负责采集（主机），Windows 负责注入（被控端）。

use roam_proto::DisplayInfo;

#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(windows)]
pub mod windows;

/// 本机显示器（逻辑点坐标）。
pub fn displays() -> Vec<DisplayInfo> {
    #[cfg(target_os = "macos")]
    return mac::displays();
    #[cfg(windows)]
    return windows::displays();
    #[allow(unreachable_code)]
    Vec::new()
}

/// 本平台能否作为被控端注入输入。
pub const CAN_INJECT: bool = cfg!(any(windows, target_os = "macos"));

#[cfg(windows)]
pub use windows::Injector;

#[cfg(target_os = "macos")]
pub use mac::Injector;

#[cfg(not(any(windows, target_os = "macos")))]
pub use stub::Injector;

/// 其他平台的占位实现，保证主程序在所有平台都能编译。
#[cfg(not(any(windows, target_os = "macos")))]
mod stub {
    use roam_proto::MouseButton;

    #[derive(Default)]
    pub struct Injector;

    impl Injector {
        pub fn new() -> Self {
            Self
        }
        pub fn refresh(&mut self) {}
        pub fn mouse_abs(&mut self, _seq: u32, _x: f32, _y: f32) {}
        pub fn move_to(&mut self, _x: f32, _y: f32) {}
        pub fn button(&mut self, _button: MouseButton, _down: bool) {}
        pub fn key(&mut self, _usage: u16, _down: bool) {}
        pub fn wheel_lines(&mut self, _dx: i32, _dy: i32) {}
        pub fn wheel_pixels(&mut self, _dx: f32, _dy: f32) {}
        pub fn sync_keys(&mut self, _pressed: &[u16]) {}
        pub fn release_all(&mut self) {}
        pub fn reset_sequence(&mut self) {}
    }
}
