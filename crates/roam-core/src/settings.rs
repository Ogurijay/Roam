//! 可调的切屏手感设置。

use serde::{Deserialize, Serialize};

use crate::keymap::hid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRemap {
    pub from: u16,
    pub to: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardProfile {
    pub enabled: bool,
    pub mappings: Vec<KeyRemap>,
}

impl KeyboardProfile {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.mappings.len() <= 128, "映射最多 128 条");
        let mut sources = std::collections::HashSet::new();
        for rule in &self.mappings {
            anyhow::ensure!(sources.insert(rule.from), "同一个来源按键只能设置一次");
            for usage in [rule.from, rule.to] {
                anyhow::ensure!(
                    crate::keymap::hid_to_mac(usage).is_some()
                        && crate::keymap::hid_to_win(usage).is_some(),
                    "不支持的按键：{usage:#x}"
                );
            }
        }
        Ok(())
    }

    pub fn apply(&self, physical: u16, fallback: u16) -> u16 {
        if self.enabled {
            if let Some(rule) = self.mappings.iter().find(|r| r.from == physical) {
                return rule.to;
            }
        }
        fallback
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardSettings {
    pub mac_to_windows: KeyboardProfile,
    pub windows_to_mac: KeyboardProfile,
}

impl KeyboardSettings {
    pub fn profile(&self, from: roam_proto::Os, to: roam_proto::Os) -> Option<&KeyboardProfile> {
        match (from, to) {
            (roam_proto::Os::MacOs, roam_proto::Os::Windows) => Some(&self.mac_to_windows),
            (roam_proto::Os::Windows, roam_proto::Os::MacOs) => Some(&self.windows_to_mac),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MacScrollSettings {
    pub reverse_mouse: bool,
    /// 连续滚动设备，包括触摸板及部分触控/平滑滚动鼠标。
    pub reverse_trackpad: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InputSettings {
    pub mac_scroll: MacScrollSettings,
    pub keyboard: KeyboardSettings,
}

impl InputSettings {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.keyboard.mac_to_windows.validate()?;
        self.keyboard.windows_to_mac.validate()
    }
}

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
    fn keyboard_rules_are_explicit_non_cascading_and_validated() {
        let mut p = KeyboardProfile {
            enabled: true,
            mappings: vec![
                KeyRemap {
                    from: hid::LEFT_GUI,
                    to: hid::LEFT_GUI,
                },
                KeyRemap { from: 4, to: 5 },
                KeyRemap { from: 5, to: 6 },
            ],
        };
        p.validate().unwrap();
        assert_eq!(p.apply(hid::LEFT_GUI, hid::LEFT_CTRL), hid::LEFT_GUI);
        assert_eq!(p.apply(4, 4), 5);
        assert_eq!(p.apply(7, 7), 7);
        p.enabled = false;
        assert_eq!(p.apply(hid::LEFT_GUI, hid::LEFT_CTRL), hid::LEFT_CTRL);
        p.mappings.push(KeyRemap { from: 4, to: 6 });
        assert!(p.validate().is_err());
        p.mappings = vec![KeyRemap {
            from: 0xffff,
            to: 4,
        }];
        assert!(p.validate().is_err());
        assert_eq!(
            serde_json::from_str::<InputSettings>("{}").unwrap(),
            InputSettings::default()
        );
    }

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
