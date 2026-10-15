//! 键码映射。线上统一用 USB HID Usage（Keyboard/Keypad page 0x07）。
//!
//! - macOS 虚拟键码（kVK_*，Carbon Events.h）→ HID
//! - HID → Windows Set 1 扫描码（SendInput + KEYEVENTF_SCANCODE）

use roam_proto::Os;

pub mod hid {
    pub const V: u16 = 0x19;
    pub const ESCAPE: u16 = 0x29;
    pub const CAPS_LOCK: u16 = 0x39;
    pub const LEFT_CTRL: u16 = 0xE0;
    pub const LEFT_SHIFT: u16 = 0xE1;
    pub const LEFT_ALT: u16 = 0xE2;
    pub const LEFT_GUI: u16 = 0xE3;
    pub const RIGHT_CTRL: u16 = 0xE4;
    pub const RIGHT_SHIFT: u16 = 0xE5;
    pub const RIGHT_ALT: u16 = 0xE6;
    pub const RIGHT_GUI: u16 = 0xE7;
}

/// macOS kVK → HID。
pub fn mac_to_hid(keycode: u16) -> Option<u16> {
    let usage = match keycode {
        0x00 => 0x04, // A
        0x01 => 0x16, // S
        0x02 => 0x07, // D
        0x03 => 0x09, // F
        0x04 => 0x0B, // H
        0x05 => 0x0A, // G
        0x06 => 0x1D, // Z
        0x07 => 0x1B, // X
        0x08 => 0x06, // C
        0x09 => 0x19, // V
        0x0A => 0x64, // ISO §
        0x0B => 0x05, // B
        0x0C => 0x14, // Q
        0x0D => 0x1A, // W
        0x0E => 0x08, // E
        0x0F => 0x15, // R
        0x10 => 0x1C, // Y
        0x11 => 0x17, // T
        0x12 => 0x1E, // 1
        0x13 => 0x1F, // 2
        0x14 => 0x20, // 3
        0x15 => 0x21, // 4
        0x16 => 0x23, // 6
        0x17 => 0x22, // 5
        0x18 => 0x2E, // =
        0x19 => 0x26, // 9
        0x1A => 0x24, // 7
        0x1B => 0x2D, // -
        0x1C => 0x25, // 8
        0x1D => 0x27, // 0
        0x1E => 0x30, // ]
        0x1F => 0x12, // O
        0x20 => 0x18, // U
        0x21 => 0x2F, // [
        0x22 => 0x0C, // I
        0x23 => 0x13, // P
        0x24 => 0x28, // Return
        0x25 => 0x0F, // L
        0x26 => 0x0D, // J
        0x27 => 0x34, // '
        0x28 => 0x0E, // K
        0x29 => 0x33, // ;
        0x2A => 0x31, // \
        0x2B => 0x36, // ,
        0x2C => 0x38, // /
        0x2D => 0x11, // N
        0x2E => 0x10, // M
        0x2F => 0x37, // .
        0x30 => 0x2B, // Tab
        0x31 => 0x2C, // Space
        0x32 => 0x35, // `
        0x33 => 0x2A, // Delete（退格）
        0x35 => 0x29, // Escape
        0x36 => hid::RIGHT_GUI,
        0x37 => hid::LEFT_GUI,
        0x38 => hid::LEFT_SHIFT,
        0x39 => hid::CAPS_LOCK,
        0x3A => hid::LEFT_ALT,
        0x3B => hid::LEFT_CTRL,
        0x3C => hid::RIGHT_SHIFT,
        0x3D => hid::RIGHT_ALT,
        0x3E => hid::RIGHT_CTRL,
        0x40 => 0x6C, // F17
        0x41 => 0x63, // Keypad .
        0x43 => 0x55, // Keypad *
        0x45 => 0x57, // Keypad +
        0x47 => 0x53, // Keypad Clear → Num Lock
        0x48 => 0x80, // Volume Up
        0x49 => 0x81, // Volume Down
        0x4A => 0x7F, // Mute
        0x4B => 0x54, // Keypad /
        0x4C => 0x58, // Keypad Enter
        0x4E => 0x56, // Keypad -
        0x4F => 0x6D, // F18
        0x50 => 0x6E, // F19
        0x51 => 0x67, // Keypad =
        0x52 => 0x62, // Keypad 0
        0x53 => 0x59, // Keypad 1
        0x54 => 0x5A, // Keypad 2
        0x55 => 0x5B, // Keypad 3
        0x56 => 0x5C, // Keypad 4
        0x57 => 0x5D, // Keypad 5
        0x58 => 0x5E, // Keypad 6
        0x59 => 0x5F, // Keypad 7
        0x5A => 0x6F, // F20
        0x5B => 0x60, // Keypad 8
        0x5C => 0x61, // Keypad 9
        0x5D => 0x89, // JIS ¥
        0x5E => 0x87, // JIS _
        0x5F => 0x85, // JIS Keypad ,
        0x60 => 0x3E, // F5
        0x61 => 0x3F, // F6
        0x62 => 0x40, // F7
        0x63 => 0x3C, // F3
        0x64 => 0x41, // F8
        0x65 => 0x42, // F9
        0x66 => 0x91, // JIS 英数 (LANG2)
        0x67 => 0x44, // F11
        0x68 => 0x90, // JIS かな (LANG1)
        0x69 => 0x68, // F13
        0x6A => 0x6B, // F16
        0x6B => 0x69, // F14
        0x6D => 0x43, // F10
        0x6E => 0x65, // Context Menu
        0x6F => 0x45, // F12
        0x71 => 0x6A, // F15
        0x72 => 0x49, // Help → Insert
        0x73 => 0x4A, // Home
        0x74 => 0x4B, // Page Up
        0x75 => 0x4C, // Forward Delete
        0x76 => 0x3D, // F4
        0x77 => 0x4D, // End
        0x78 => 0x3B, // F2
        0x79 => 0x4E, // Page Down
        0x7A => 0x3A, // F1
        0x7B => 0x50, // ←
        0x7C => 0x4F, // →
        0x7D => 0x51, // ↓
        0x7E => 0x52, // ↑
        _ => return None,
    };
    Some(usage)
}

/// HID → macOS kVK（[`mac_to_hid`] 的反查）。
pub fn hid_to_mac(usage: u16) -> Option<u16> {
    (0u16..0x80).find(|&code| mac_to_hid(code) == Some(usage))
}

/// Windows 低级键盘钩子给出的扫描码 → HID（[`hid_to_win`] 的反查）。
pub fn win_to_hid(scan: u16, extended: bool) -> Option<u16> {
    // 非美式键盘的反斜杠与 # 共用扫描码 0x2B，统一当作反斜杠
    (0u16..0xE8).find(|&usage| usage != 0x32 && hid_to_win(usage) == Some((scan, extended)))
}

/// macOS 修饰键的"设备相关"标志位（区分左右），用于在 FlagsChanged 事件中判断按下还是抬起。
pub fn mac_modifier_flag(keycode: u16) -> Option<u64> {
    Some(match keycode {
        0x3B => 0x0000_0001, // 左 Control
        0x38 => 0x0000_0002, // 左 Shift
        0x3C => 0x0000_0004, // 右 Shift
        0x37 => 0x0000_0008, // 左 Command
        0x36 => 0x0000_0010, // 右 Command
        0x3A => 0x0000_0020, // 左 Option
        0x3D => 0x0000_0040, // 右 Option
        0x3E => 0x0000_2000, // 右 Control
        0x39 => 0x0001_0000, // Caps Lock（AlphaShift）
        _ => return None,
    })
}

/// HID → Windows Set 1 扫描码，返回 (扫描码, 是否 E0 扩展键)。
pub fn hid_to_win(usage: u16) -> Option<(u16, bool)> {
    let (scan, ext) = match usage {
        0x04 => (0x1E, false), // A
        0x05 => (0x30, false),
        0x06 => (0x2E, false),
        0x07 => (0x20, false),
        0x08 => (0x12, false),
        0x09 => (0x21, false),
        0x0A => (0x22, false),
        0x0B => (0x23, false),
        0x0C => (0x17, false),
        0x0D => (0x24, false),
        0x0E => (0x25, false),
        0x0F => (0x26, false),
        0x10 => (0x32, false),
        0x11 => (0x31, false),
        0x12 => (0x18, false),
        0x13 => (0x19, false),
        0x14 => (0x10, false),
        0x15 => (0x13, false),
        0x16 => (0x1F, false),
        0x17 => (0x14, false),
        0x18 => (0x16, false),
        0x19 => (0x2F, false),
        0x1A => (0x11, false),
        0x1B => (0x2D, false),
        0x1C => (0x15, false),
        0x1D => (0x2C, false),                       // Z
        0x1E..=0x26 => (usage - 0x1E + 0x02, false), // 1..9
        0x27 => (0x0B, false),                       // 0
        0x28 => (0x1C, false),                       // Enter
        0x29 => (0x01, false),                       // Esc
        0x2A => (0x0E, false),                       // Backspace
        0x2B => (0x0F, false),                       // Tab
        0x2C => (0x39, false),                       // Space
        0x2D => (0x0C, false),                       // -
        0x2E => (0x0D, false),                       // =
        0x2F => (0x1A, false),                       // [
        0x30 => (0x1B, false),                       // ]
        0x31 => (0x2B, false),                       // \
        0x32 => (0x2B, false),                       // 非美式 #
        0x33 => (0x27, false),                       // ;
        0x34 => (0x28, false),                       // '
        0x35 => (0x29, false),                       // `
        0x36 => (0x33, false),                       // ,
        0x37 => (0x34, false),                       // .
        0x38 => (0x35, false),                       // /
        0x39 => (0x3A, false),                       // Caps Lock
        0x3A..=0x43 => (usage - 0x3A + 0x3B, false), // F1..F10
        0x44 => (0x57, false),                       // F11
        0x45 => (0x58, false),                       // F12
        0x46 => (0x37, true),                        // Print Screen
        0x47 => (0x46, false),                       // Scroll Lock
        0x49 => (0x52, true),                        // Insert
        0x4A => (0x47, true),                        // Home
        0x4B => (0x49, true),                        // Page Up
        0x4C => (0x53, true),                        // Delete
        0x4D => (0x4F, true),                        // End
        0x4E => (0x51, true),                        // Page Down
        0x4F => (0x4D, true),                        // →
        0x50 => (0x4B, true),                        // ←
        0x51 => (0x50, true),                        // ↓
        0x52 => (0x48, true),                        // ↑
        0x53 => (0x45, true),                        // Num Lock（SendInput 需要扩展位）
        0x54 => (0x35, true),                        // Keypad /
        0x55 => (0x37, false),                       // Keypad *
        0x56 => (0x4A, false),                       // Keypad -
        0x57 => (0x4E, false),                       // Keypad +
        0x58 => (0x1C, true),                        // Keypad Enter
        0x59 => (0x4F, false),                       // Keypad 1
        0x5A => (0x50, false),
        0x5B => (0x51, false),
        0x5C => (0x4B, false),
        0x5D => (0x4C, false),
        0x5E => (0x4D, false),
        0x5F => (0x47, false),
        0x60 => (0x48, false),
        0x61 => (0x49, false),                       // Keypad 9
        0x62 => (0x52, false),                       // Keypad 0
        0x63 => (0x53, false),                       // Keypad .
        0x64 => (0x56, false),                       // 非美式 \
        0x65 => (0x5D, true),                        // Application
        0x67 => (0x59, false),                       // Keypad =
        0x68..=0x6E => (usage - 0x68 + 0x64, false), // F13..F19
        0x6F => (0x6B, false),                       // F20
        0x7F => (0x20, true),                        // Mute
        0x80 => (0x30, true),                        // Volume Up
        0x81 => (0x2E, true),                        // Volume Down
        0x85 => (0x7E, false),                       // Keypad ,
        0x87 => (0x73, false),                       // JIS _ (Ro)
        0x88 => (0x70, false),                       // かな
        0x89 => (0x7D, false),                       // ¥
        0x8A => (0x79, false),                       // 変換
        0x8B => (0x7B, false),                       // 無変換
        hid::LEFT_CTRL => (0x1D, false),
        hid::LEFT_SHIFT => (0x2A, false),
        hid::LEFT_ALT => (0x38, false),
        hid::LEFT_GUI => (0x5B, true),
        hid::RIGHT_CTRL => (0x1D, true),
        hid::RIGHT_SHIFT => (0x36, false),
        hid::RIGHT_ALT => (0x38, true),
        hid::RIGHT_GUI => (0x5C, true),
        _ => return None,
    };
    Some((scan, ext))
}

/// 控制端到被控端的修饰键转换：Mac 和其他系统互控时 Cmd ↔ Ctrl 对调。
/// Mac 控制 Windows：Cmd+C → Ctrl+C；Windows 控制 Mac：Ctrl+C → Cmd+C。
pub fn remap_for_target(usage: u16, from: Os, to: Os, swap_cmd_ctrl: bool) -> u16 {
    if !swap_cmd_ctrl || (from == Os::MacOs) == (to == Os::MacOs) {
        return usage;
    }
    match usage {
        hid::LEFT_GUI => hid::LEFT_CTRL,
        hid::LEFT_CTRL => hid::LEFT_GUI,
        hid::RIGHT_GUI => hid::RIGHT_CTRL,
        hid::RIGHT_CTRL => hid::RIGHT_GUI,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits() {
        // kVK_ANSI_A → HID A → 扫描码 0x1E
        assert_eq!(mac_to_hid(0x00).and_then(hid_to_win), Some((0x1E, false)));
        // kVK_ANSI_1 → HID 1 → 0x02；kVK_ANSI_9 → 0x0A
        assert_eq!(mac_to_hid(0x12).and_then(hid_to_win), Some((0x02, false)));
        assert_eq!(mac_to_hid(0x19).and_then(hid_to_win), Some((0x0A, false)));
        // F1 / F10 / F11
        assert_eq!(mac_to_hid(0x7A).and_then(hid_to_win), Some((0x3B, false)));
        assert_eq!(mac_to_hid(0x6D).and_then(hid_to_win), Some((0x44, false)));
        assert_eq!(mac_to_hid(0x67).and_then(hid_to_win), Some((0x57, false)));
    }

    #[test]
    fn every_mac_key_maps_to_windows() {
        for code in 0u16..0x80 {
            if let Some(usage) = mac_to_hid(code) {
                // LANG1/LANG2 在 Windows 上没有扫描码，允许缺失
                if usage == 0x90 || usage == 0x91 {
                    continue;
                }
                assert!(
                    hid_to_win(usage).is_some(),
                    "kVK {code:#x} → HID {usage:#x}"
                );
            }
        }
    }

    #[test]
    fn reverse_tables_roundtrip() {
        for code in 0u16..0x80 {
            if let Some(usage) = mac_to_hid(code) {
                assert_eq!(hid_to_mac(usage), Some(code), "kVK {code:#x}");
            }
        }
        // A、左 Ctrl、右 Alt、方向键
        assert_eq!(win_to_hid(0x1E, false), Some(0x04));
        assert_eq!(win_to_hid(0x1D, false), Some(hid::LEFT_CTRL));
        assert_eq!(win_to_hid(0x38, true), Some(hid::RIGHT_ALT));
        assert_eq!(win_to_hid(0x48, true), Some(0x52));
        assert_eq!(win_to_hid(0x48, false), Some(0x60)); // 小键盘 8
        assert_eq!(win_to_hid(0x2B, false), Some(0x31));
    }

    #[test]
    fn cmd_becomes_ctrl_on_windows() {
        let r = |u| remap_for_target(u, Os::MacOs, Os::Windows, true);
        assert_eq!(r(hid::LEFT_GUI), hid::LEFT_CTRL);
        assert_eq!(r(hid::LEFT_CTRL), hid::LEFT_GUI);
        assert_eq!(r(0x06), 0x06);
        assert_eq!(
            remap_for_target(hid::LEFT_GUI, Os::MacOs, Os::Windows, false),
            hid::LEFT_GUI
        );
        assert_eq!(
            remap_for_target(hid::LEFT_GUI, Os::MacOs, Os::MacOs, true),
            hid::LEFT_GUI
        );
        // Windows 控制 Mac：Ctrl → Cmd；同为 Windows 不变
        assert_eq!(
            remap_for_target(hid::LEFT_CTRL, Os::Windows, Os::MacOs, true),
            hid::LEFT_GUI
        );
        assert_eq!(
            remap_for_target(hid::LEFT_CTRL, Os::Windows, Os::Windows, true),
            hid::LEFT_CTRL
        );
    }
}
