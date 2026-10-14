//! 可调的切屏手感设置。

use serde::{Deserialize, Serialize};

use crate::keymap::hid;

/// 修饰键（左右任意一个按下即算按下）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    Shift,
    Ctrl,
    /// macOS 的 Option，Windows 的 Alt。
    Alt,
    /// macOS 的 Command，Windows 的 Win。
    Cmd,
}

impl Modifier {
    pub fn usages(self) -> [u16; 2] {
        match self {
            Modifier::Shift => [hid::LEFT_SHIFT, hid::RIGHT_SHIFT],
            Modifier::Ctrl => [hid::LEFT_CTRL, hid::RIGHT_CTRL],
            Modifier::Alt => [hid::LEFT_ALT, hid::RIGHT_ALT],
            Modifier::Cmd => [hid::LEFT_GUI, hid::RIGHT_GUI],
        }
    }
}

/// 光标跨屏时，进入位置如何计算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeMapping {
    /// 按布局里的实际位置一一对应。
    #[default]
    Aligned,
    /// 按边的比例对应：从源屏幕边的 30% 处离开，就从目标屏幕边的 30% 处进入。
    /// 两块屏幕大小差别很大时更顺手。
    Proportional,
}

/// 全局切屏设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwitchSettings {
    /// 光标顶住边缘多久才切屏（毫秒），0 表示立即切。
    pub dwell_ms: u32,
    /// 需要快速撞两次边缘才切屏（开启后忽略停留时间）。
    pub double_tap: bool,
    /// 两次撞边的最大间隔（毫秒）。
    pub double_tap_window_ms: u32,
    /// 需要按住这个修饰键才允许切屏。
    pub require_modifier: Option<Modifier>,
    /// 屏幕四角的死区大小（逻辑点），角落附近不切屏。
    pub corner_deadzone: f64,
    pub edge_mapping: EdgeMapping,
    /// 快捷键的修饰组合：组合 + Esc 回到本机，组合 + 数字直切设备，组合 + L 锁定当前屏幕。
    pub hotkey_modifiers: Vec<Modifier>,
}

impl Default for SwitchSettings {
    fn default() -> Self {
        Self {
            dwell_ms: 0,
            double_tap: false,
            double_tap_window_ms: 500,
            require_modifier: None,
            corner_deadzone: 0.0,
            edge_mapping: EdgeMapping::Aligned,
            hotkey_modifiers: vec![Modifier::Ctrl, Modifier::Alt, Modifier::Cmd],
        }
    }
}

/// 每台被控设备的设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    /// 停用后不参与切屏，也不能用快捷键切过去，但保留配置和布局。
    pub enabled: bool,
    /// 指针速度倍率。
    pub pointer_speed: f64,
    /// 滚轮速度倍率。
    pub scroll_speed: f64,
    /// 反转纵向滚动。
    pub invert_scroll_y: bool,
    /// 反转横向滚动。
    pub invert_scroll_x: bool,
    /// 和这台设备同步剪贴板。
    pub clipboard: bool,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            pointer_speed: 1.0,
            scroll_speed: 1.0,
            invert_scroll_y: false,
            invert_scroll_x: false,
            clipboard: true,
        }
    }
}

/// 剪贴板同步设置（全局）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardSettings {
    pub enabled: bool,
    /// 同步图片（没有文本时）。
    pub images: bool,
    /// 单次同步的大小上限（MB），超过就不同步。
    pub max_mb: u32,
    /// 复制文件后切屏时传输文件。
    pub files: bool,
    /// 单次文件传输的总大小上限（MB）。
    pub max_file_mb: u32,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            images: true,
            max_mb: 20,
            files: true,
            max_file_mb: 4096,
        }
    }
}

impl ClipboardSettings {
    pub fn sanitized(mut self) -> Self {
        self.max_mb = self.max_mb.clamp(1, 200);
        self.max_file_mb = self.max_file_mb.clamp(1, 100 * 1024);
        self
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_mb as u64 * 1024 * 1024
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_mb as u64 * 1024 * 1024
    }
}

impl DeviceSettings {
    /// 把数值限制在合理范围内，防止页面传来离谱的值。
    pub fn sanitized(mut self) -> Self {
        self.pointer_speed = self.pointer_speed.clamp(0.1, 5.0);
        self.scroll_speed = self.scroll_speed.clamp(0.1, 10.0);
        self
    }
}

impl SwitchSettings {
    pub fn sanitized(mut self) -> Self {
        self.dwell_ms = self.dwell_ms.min(2000);
        self.double_tap_window_ms = self.double_tap_window_ms.clamp(150, 1500);
        self.corner_deadzone = self.corner_deadzone.clamp(0.0, 400.0);
        self.hotkey_modifiers.dedup();
        if self.hotkey_modifiers.is_empty() {
            // 没有修饰键的快捷键会和正常打字冲突
            self.hotkey_modifiers = Self::default().hotkey_modifiers;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_configs_get_defaults() {
        let s: SwitchSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, SwitchSettings::default());
        let d: DeviceSettings = serde_json::from_str(r#"{"invert_scroll_y": true}"#).unwrap();
        assert!(d.enabled && d.invert_scroll_y && d.pointer_speed == 1.0);
    }

    #[test]
    fn sanitize_rejects_empty_hotkeys() {
        let s = SwitchSettings {
            hotkey_modifiers: vec![],
            ..Default::default()
        }
        .sanitized();
        assert_eq!(s.hotkey_modifiers.len(), 3);
    }
}
