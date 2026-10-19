//! 输入路由：决定每个本地输入事件留在主机还是发往某台被控设备。
//!
//! 运行在采集线程里（macOS 的事件 tap 回调 / Windows 的低级钩子），必须快，不做 I/O：
//! 发往被控设备的消息只是塞进通道，由网络任务发送。
//!
//! 布局是网格（见 [`crate::layout`]）：光标从某台设备的一边推出去，就进入那一边相邻格子里的设备，
//! 位置沿边按比例对应。光标在被控设备上时，用该设备自己的本地坐标跟踪。

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use roam_proto::{Datagram, MouseButton, Os, Rect, StreamMsg};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    keymap::{hid, remap_for_target},
    layout::{DeviceGeom, DeviceId, RectExt, World, entry_point},
    settings::{DeviceSettings, KeyboardProfile, KeyboardSettings, Modifier, SwitchSettings},
};

/// 采集层产生的本地输入事件（坐标为主机本地逻辑点）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// `x, y` 为移动后的光标位置，`dx, dy` 为位移。
    Move {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Key {
        usage: u16,
        down: bool,
    },
    /// 120 = 一格，正值为向上 / 向右。
    WheelLines {
        dx: i32,
        dy: i32,
    },
    /// 正值为向上 / 向右。
    WheelPixels {
        dx: f32,
        dy: f32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outbound {
    Datagram(Datagram),
    Stream(StreamMsg),
}

/// 主机光标控制，由平台层实现。
pub trait CursorControl: Send {
    fn hide(&mut self);
    fn show(&mut self);
    /// 移动到主机本地坐标。
    fn warp(&mut self, x: f64, y: f64);
    /// 锁定后物理鼠标移动不再带动光标，只产生位移事件。
    fn set_locked(&mut self, locked: bool);
}

/// 一台在线的被控设备。
pub struct PeerLink {
    pub os: Os,
    pub tx: UnboundedSender<Outbound>,
}

#[derive(Debug, Clone, PartialEq)]
enum Focus {
    Local,
    /// 光标在被控设备上，`cursor` 为该设备的本地坐标（逻辑点）。
    Remote {
        id: DeviceId,
        cursor: (f64, f64),
    },
}

/// 正在"顶边"的状态，用于停留时间和双击撞边判断。
#[derive(Debug, Default)]
struct EdgePush {
    /// 当前这次顶边要去的设备，以及开始时间。
    current: Option<(DeviceId, Instant)>,
    /// 上一次顶边（双击撞边用）。
    last_tap: Option<(DeviceId, Instant)>,
}

/// 判断"光标顶到屏幕边缘"时向外探测的距离。
const EDGE_PROBE: f64 = 2.0;

/// HID：数字键 1..9、L、Q、退格（Mac 的 delete）、向前删除。
const DIGIT_1: u16 = 0x1E;
const DIGIT_9: u16 = 0x26;
const KEY_L: u16 = 0x0F;
const KEY_Q: u16 = 0x14;
const BACKSPACE: u16 = 0x2A;
const FORWARD_DELETE: u16 = 0x4C;

pub struct Router {
    host_id: DeviceId,
    host_os: Os,
    world: World,
    peers: HashMap<DeviceId, PeerLink>,
    cursor: Box<dyn CursorControl>,
    focus: Focus,
    swap_cmd_ctrl: bool,
    keyboard: KeyboardSettings,
    keyboard_overrides: HashMap<DeviceId, KeyboardProfile>,
    settings: SwitchSettings,
    device_settings: HashMap<DeviceId, DeviceSettings>,
    /// 快捷键「组合 + 数字」对应的设备顺序，第 1 个是主机。
    device_order: Vec<DeviceId>,
    /// 在主机上按下、主机已经看到按下的键。抬起时必须交给主机，否则主机会卡键。
    local_keys: HashSet<u16>,
    /// 在被控设备上按下的键（物理 usage）。
    remote_keys: HashSet<u16>,
    /// 被快捷键吞掉的键，抬起时也要吞掉。
    hotkey_keys: HashSet<u16>,
    buttons_down: HashSet<MouseButton>,
    mouse_seq: u32,
    /// 暂停时不会切到任何被控设备。
    paused: bool,
    /// 锁定在当前屏幕：不通过边缘切屏，快捷键仍然可用。
    edge_locked: bool,
    edge: EdgePush,
    /// 每台被控设备上次离开时的光标位置（本地坐标），快捷键切回时用。
    last_remote_pos: HashMap<DeviceId, (f64, f64)>,
}

impl Router {
    pub fn new(host_id: DeviceId, host_os: Os, cursor: Box<dyn CursorControl>) -> Self {
        Self {
            device_order: vec![host_id.clone()],
            host_id,
            host_os,
            world: World::default(),
            peers: HashMap::new(),
            cursor,
            focus: Focus::Local,
            swap_cmd_ctrl: true,
            keyboard: KeyboardSettings::default(),
            keyboard_overrides: HashMap::new(),
            settings: SwitchSettings::default(),
            device_settings: HashMap::new(),
            local_keys: HashSet::new(),
            remote_keys: HashSet::new(),
            hotkey_keys: HashSet::new(),
            buttons_down: HashSet::new(),
            mouse_seq: 0,
            paused: false,
            edge_locked: false,
            edge: EdgePush::default(),
            last_remote_pos: HashMap::new(),
        }
    }

    // ------------------------------------------------------------ 配置

    pub fn set_swap_cmd_ctrl(&mut self, swap: bool) {
        if self.swap_cmd_ctrl != swap {
            self.return_home();
        }
        self.swap_cmd_ctrl = swap;
    }

    pub fn set_keyboard_settings(&mut self, settings: KeyboardSettings) {
        if self.keyboard != settings {
            self.return_home();
        }
        self.keyboard = settings;
    }

    pub fn set_keyboard_override(&mut self, id: &str, profile: KeyboardProfile) {
        let wanted = profile.enabled.then_some(&profile);
        if self.keyboard_overrides.get(id) == wanted {
            return;
        }
        if self.focused() == Some(id) {
            self.return_home();
        }
        if profile.enabled {
            self.keyboard_overrides.insert(id.to_string(), profile);
        } else {
            self.keyboard_overrides.remove(id);
        }
    }

    pub fn set_switch_settings(&mut self, settings: SwitchSettings) {
        self.settings = settings;
        self.edge = EdgePush::default();
    }

    pub fn set_device_settings(&mut self, settings: HashMap<DeviceId, DeviceSettings>) {
        self.device_settings = settings;
    }

    /// 快捷键「组合 + 数字」的设备顺序（不含主机，主机固定为 1）。
    pub fn set_device_order(&mut self, peers: Vec<DeviceId>) {
        self.device_order = std::iter::once(self.host_id.clone()).chain(peers).collect();
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            self.return_home();
        }
    }

    pub fn edge_locked(&self) -> bool {
        self.edge_locked
    }

    pub fn set_edge_locked(&mut self, locked: bool) {
        self.edge_locked = locked;
        self.edge = EdgePush::default();
    }

    pub fn set_world(&mut self, world: World) {
        self.world = world;
        if let Focus::Remote { id, .. } = &self.focus
            && self.world.find(id).is_none()
        {
            self.return_home();
        }
    }

    pub fn add_peer(&mut self, id: DeviceId, link: PeerLink) {
        self.peers.insert(id, link);
    }

    pub fn remove_peer(&mut self, id: &str) {
        if matches!(&self.focus, Focus::Remote { id: cur, .. } if cur == id) {
            self.return_home();
        }
        self.peers.remove(id);
        self.keyboard_overrides.remove(id);
    }

    /// 当前光标所在的被控设备。
    pub fn focused(&self) -> Option<&str> {
        match &self.focus {
            Focus::Local => None,
            Focus::Remote { id, .. } => Some(id),
        }
    }

    /// 定时调用（约 1 秒）：向当前被控设备同步按键状态，纠正可能的卡键。
    pub fn tick(&mut self) {
        if let Focus::Remote { id, .. } = &self.focus {
            let id = id.clone();
            let pressed = self
                .remote_keys
                .iter()
                .map(|&u| self.map_key(&id, u))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            self.send(&id, Outbound::Stream(StreamMsg::KeyState { pressed }));
        }
    }

    /// 锁定某台被控设备的电脑。
    pub fn lock_device(&mut self, id: &str) {
        self.send(id, Outbound::Stream(StreamMsg::LockScreen));
    }

    /// 强制回到主机（逃生热键、断线等），光标放在主屏中央。
    pub fn return_home(&mut self) {
        if !matches!(self.focus, Focus::Remote { .. }) {
            return;
        }
        self.leave_remote();
        let (x, y) = self.host_center();
        self.enter_local(x, y);
    }

    // ------------------------------------------------------------ 入口

    /// 处理一个本地输入事件。返回 `true` 表示事件已被转发，应在主机上吞掉。
    pub fn handle(&mut self, ev: InputEvent) -> bool {
        self.handle_at(ev, Instant::now())
    }

    pub fn handle_at(&mut self, ev: InputEvent, now: Instant) -> bool {
        match ev {
            InputEvent::Move { x, y, dx, dy } => self.on_move(x, y, dx, dy, now),
            InputEvent::Button { button, down } => self.on_button(button, down),
            InputEvent::Key { usage, down } => self.on_key(usage, down),
            InputEvent::WheelLines { dx, dy } => {
                let Some(s) = self.focused_settings() else {
                    return false;
                };
                let (fx, fy) = wheel_factors(&s);
                let msg = StreamMsg::WheelLines {
                    dx: (dx as f64 * fx).round() as i32,
                    dy: (dy as f64 * fy).round() as i32,
                };
                self.forward(Outbound::Stream(msg))
            }
            InputEvent::WheelPixels { dx, dy } => {
                let Some(s) = self.focused_settings() else {
                    return false;
                };
                let (fx, fy) = wheel_factors(&s);
                let msg = StreamMsg::WheelPixels {
                    dx: (dx as f64 * fx) as f32,
                    dy: (dy as f64 * fy) as f32,
                };
                self.forward(Outbound::Stream(msg))
            }
        }
    }

    // ------------------------------------------------------------ 鼠标

    fn on_move(&mut self, x: f64, y: f64, dx: f64, dy: f64, now: Instant) -> bool {
        match self.focus.clone() {
            Focus::Local => self.move_local(x, y, dx, dy, now),
            Focus::Remote { id, cursor } => {
                let speed = self.settings_of(&id).pointer_speed;
                self.move_remote(id, cursor, (dx * speed, dy * speed), now);
                true
            }
        }
    }

    fn move_local(&mut self, x: f64, y: f64, dx: f64, dy: f64, now: Instant) -> bool {
        if self.paused || self.edge_locked || !self.buttons_down.is_empty() {
            self.edge.current = None;
            return false;
        }
        let Some(host) = self.world.find(&self.host_id).cloned() else {
            return false;
        };
        // 只有光标已经顶在某块屏幕的边缘、还在往外推时才考虑切屏
        let probe = (x + dx.signum() * EDGE_PROBE, y + dy.signum() * EDGE_PROBE);
        if host.contains(probe.0, probe.1) {
            self.edge.current = None;
            return false;
        }
        let src_index = host.nearest_index(x, y);
        let src = host.displays[src_index];
        let target = src.exit_side(probe.0, probe.1).and_then(|side| {
            self.world
                .neighbor(&self.host_id, src_index, side)
                .filter(|(d, _)| self.peers.contains_key(&d.id))
                .map(|(d, i)| (side, d.clone(), i))
        });
        let Some((side, target, dst_index)) = target else {
            self.edge.current = None;
            return false;
        };
        if !self.allow_switch(&target.id, src, (x, y), now) {
            return false;
        }
        let (ex, ey) = entry_point(src, target.displays[dst_index], side, x, y);
        let entry = target.clamp(ex, ey);
        self.cursor.set_locked(true);
        self.cursor.hide();
        self.enter_remote(target.id, entry);
        true
    }

    fn move_remote(
        &mut self,
        id: DeviceId,
        cursor: (f64, f64),
        (dx, dy): (f64, f64),
        now: Instant,
    ) {
        let Some(current) = self.world.find(&id).cloned() else {
            self.return_home();
            return;
        };
        let (cx, cy) = (cursor.0 + dx, cursor.1 + dy);
        // 同一台电脑的几块屏幕之间按它自己的排列移动
        if current.contains(cx, cy) {
            self.edge.current = None;
            self.set_cursor(&id, (cx, cy));
            return;
        }

        // 推出了当前这块屏幕：看看这一边的格子里有没有另一台电脑的屏幕
        let src_index = current.nearest_index(cursor.0, cursor.1);
        let src = current.displays[src_index];
        let clamped = src.clamp(cx, cy);
        let next = match src.exit_side(cx, cy) {
            Some(side) if !self.edge_locked && self.buttons_down.is_empty() => self
                .world
                .neighbor(&id, src_index, side)
                .filter(|(d, _)| d.id == self.host_id || self.peers.contains_key(&d.id))
                .map(|(d, i)| (side, d.clone(), i)),
            _ => None,
        };
        match next {
            Some((side, next, dst_index)) if self.allow_switch(&next.id, src, clamped, now) => {
                let (ex, ey) =
                    entry_point(src, next.displays[dst_index], side, clamped.0, clamped.1);
                let entry = next.clamp(ex, ey);
                self.leave_remote();
                if next.id == self.host_id {
                    self.enter_local(entry.0, entry.1);
                } else {
                    self.enter_remote(next.id, entry);
                }
            }
            next => {
                if next.is_none() {
                    self.edge.current = None;
                }
                self.set_cursor(&id, current.clamp(cx, cy));
            }
        }
    }

    /// 是否允许这次切屏：修饰键、角落死区、双击撞边、停留时间。
    fn allow_switch(&mut self, target: &str, src: Rect, (x, y): (f64, f64), now: Instant) -> bool {
        if let Some(m) = self.settings.require_modifier
            && !self.modifier_held(m)
        {
            self.edge.current = None;
            return false;
        }
        let dz = self.settings.corner_deadzone;
        if dz > 0.0 {
            let near_x = x - src.x < dz || src.x + src.w - x <= dz;
            let near_y = y - src.y < dz || src.y + src.h - y <= dz;
            if near_x && near_y {
                self.edge.current = None;
                return false;
            }
        }

        let starting = !matches!(&self.edge.current, Some((t, _)) if t == target);
        if starting {
            self.edge.current = Some((target.to_string(), now));
        }
        if self.settings.double_tap {
            if !starting {
                return false;
            }
            let window = Duration::from_millis(self.settings.double_tap_window_ms as u64);
            let second = matches!(
                &self.edge.last_tap,
                Some((t, at)) if t == target && now.duration_since(*at) <= window
            );
            if second {
                self.edge = EdgePush::default();
                return true;
            }
            self.edge.last_tap = Some((target.to_string(), now));
            return false;
        }
        let since = self.edge.current.as_ref().map(|(_, at)| *at).unwrap_or(now);
        if now.duration_since(since) >= Duration::from_millis(self.settings.dwell_ms as u64) {
            self.edge.current = None;
            true
        } else {
            false
        }
    }

    fn set_cursor(&mut self, id: &str, cursor: (f64, f64)) {
        self.focus = Focus::Remote {
            id: id.to_string(),
            cursor,
        };
        self.send_abs(id);
    }

    fn enter_remote(&mut self, id: DeviceId, cursor: (f64, f64)) {
        self.focus = Focus::Remote {
            id: id.clone(),
            cursor,
        };
        self.send(
            &id,
            Outbound::Stream(StreamMsg::Enter {
                x: cursor.0 as f32,
                y: cursor.1 as f32,
            }),
        );
    }

    /// 离开当前被控设备：记住位置，通知对方释放按键。
    fn leave_remote(&mut self) {
        if let Focus::Remote { id, cursor } = self.focus.clone() {
            self.last_remote_pos.insert(id.clone(), cursor);
            self.send(&id, Outbound::Stream(StreamMsg::Leave));
            self.remote_keys.clear();
        }
    }

    fn enter_local(&mut self, x: f64, y: f64) {
        self.focus = Focus::Local;
        self.remote_keys.clear();
        self.edge = EdgePush::default();
        self.cursor.warp(x, y);
        self.cursor.set_locked(false);
        self.cursor.show();
    }

    fn host_center(&self) -> (f64, f64) {
        self.world
            .find(&self.host_id)
            .map(DeviceGeom::center)
            .unwrap_or((0.0, 0.0))
    }

    /// 快捷键直接切到第 `index` 台设备（0 = 主机）。
    fn jump_to(&mut self, index: usize) {
        let Some(id) = self.device_order.get(index).cloned() else {
            return;
        };
        if self.paused || self.focused() == Some(id.as_str()) {
            return;
        }
        if id == self.host_id {
            self.return_home();
            return;
        }
        let Some(target) = self.world.find(&id).cloned() else {
            return;
        };
        if !self.peers.contains_key(&id) {
            return;
        }
        let entry = self
            .last_remote_pos
            .get(&id)
            .copied()
            .filter(|&(x, y)| target.contains(x, y))
            .unwrap_or_else(|| target.center());
        if self.focused().is_some() {
            self.leave_remote();
        } else {
            self.cursor.set_locked(true);
            self.cursor.hide();
        }
        self.enter_remote(id, entry);
    }

    fn send_abs(&mut self, id: &str) {
        let Focus::Remote { cursor, .. } = &self.focus else {
            return;
        };
        let (x, y) = *cursor;
        self.mouse_seq = self.mouse_seq.wrapping_add(1);
        let msg = Datagram::MouseAbs {
            seq: self.mouse_seq,
            x: x as f32,
            y: y as f32,
        };
        self.send(id, Outbound::Datagram(msg));
    }

    fn on_button(&mut self, button: MouseButton, down: bool) -> bool {
        if down {
            self.buttons_down.insert(button);
        } else {
            self.buttons_down.remove(&button);
        }
        let Focus::Remote { id, cursor } = self.focus.clone() else {
            return false;
        };
        if down {
            // 数据报可能丢，点击前用可靠流把位置再发一次，保证点在正确的地方
            self.send(
                &id,
                Outbound::Stream(StreamMsg::MouseTo {
                    x: cursor.0 as f32,
                    y: cursor.1 as f32,
                }),
            );
        }
        self.send(&id, Outbound::Stream(StreamMsg::Button { button, down }));
        true
    }

    // ------------------------------------------------------------ 键盘

    fn on_key(&mut self, usage: u16, down: bool) -> bool {
        if !down && self.hotkey_keys.remove(&usage) {
            return true;
        }
        if down && self.hotkey_held() && self.on_hotkey(usage) {
            self.hotkey_keys.insert(usage);
            return true;
        }
        // Mac 的锁屏键 Ctrl+Cmd+Q：正在控制哪台就锁哪台（不让 Mac 自己锁屏）
        if down
            && usage == KEY_Q
            && self.host_os == Os::MacOs
            && self.modifier_held(Modifier::Ctrl)
            && self.modifier_held(Modifier::Cmd)
            && let Some(id) = self.focused().map(str::to_string)
        {
            self.lock_device(&id);
            self.hotkey_keys.insert(usage);
            return true;
        }

        let Focus::Remote { id, .. } = self.focus.clone() else {
            if down {
                self.local_keys.insert(usage);
            } else {
                self.local_keys.remove(&usage);
            }
            return false;
        };

        if down {
            if self.local_keys.contains(&usage) {
                // 切屏前就按住的键（自动重复），不发给被控端，否则抬起时对方会卡键
                return true;
            }
            self.remote_keys.insert(usage);
        } else if self.local_keys.remove(&usage) {
            // 按下时主机看到了，抬起也必须交给主机
            return false;
        } else {
            self.remote_keys.remove(&usage);
        }
        let mapped = self.map_key(&id, usage);
        if !down
            && self
                .remote_keys
                .iter()
                .any(|&u| self.map_key(&id, u) == mapped)
        {
            return true;
        }
        self.send(
            &id,
            Outbound::Stream(StreamMsg::Key {
                usage: mapped,
                down,
            }),
        );
        true
    }

    /// 处理快捷键，返回是否消费了这个键。
    fn on_hotkey(&mut self, usage: u16) -> bool {
        match usage {
            hid::ESCAPE => {
                if self.focused().is_none() {
                    return false;
                }
                self.return_home();
                true
            }
            DIGIT_1..=DIGIT_9 => {
                self.jump_to((usage - DIGIT_1) as usize);
                true
            }
            KEY_L => {
                self.set_edge_locked(!self.edge_locked);
                true
            }
            BACKSPACE | FORWARD_DELETE => match self.focused().map(str::to_string) {
                Some(id) => {
                    self.lock_device(&id);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    fn modifier_held(&self, m: Modifier) -> bool {
        m.usages()
            .iter()
            .any(|k| self.local_keys.contains(k) || self.remote_keys.contains(k))
    }

    fn hotkey_held(&self) -> bool {
        !self.settings.hotkey_modifiers.is_empty()
            && self
                .settings
                .hotkey_modifiers
                .iter()
                .all(|&m| self.modifier_held(m))
    }

    fn map_key(&self, id: &str, usage: u16) -> u16 {
        let to = self.peers.get(id).map(|p| p.os).unwrap_or(Os::Other);
        let fallback = remap_for_target(usage, self.host_os, to, self.swap_cmd_ctrl);
        let profile = self
            .keyboard_overrides
            .get(id)
            .filter(|p| p.enabled)
            .or_else(|| self.keyboard.profile(self.host_os, to));
        profile.map_or(fallback, |p| p.apply(usage, fallback))
    }

    // ------------------------------------------------------------ 发送

    fn settings_of(&self, id: &str) -> DeviceSettings {
        self.device_settings.get(id).cloned().unwrap_or_default()
    }

    fn focused_settings(&self) -> Option<DeviceSettings> {
        self.focused().map(|id| self.settings_of(id))
    }

    fn forward(&mut self, msg: Outbound) -> bool {
        match self.focus.clone() {
            Focus::Local => false,
            Focus::Remote { id, .. } => {
                self.send(&id, msg);
                true
            }
        }
    }

    fn send(&self, id: &str, msg: Outbound) {
        if let Some(peer) = self.peers.get(id) {
            let _ = peer.tx.send(msg);
        }
    }
}

/// 滚轮倍率（含反转）。
fn wheel_factors(s: &DeviceSettings) -> (f64, f64) {
    let sign = |invert: bool| if invert { -1.0 } else { 1.0 };
    (
        s.scroll_speed * sign(s.invert_scroll_x),
        s.scroll_speed * sign(s.invert_scroll_y),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::sync::mpsc::{self, UnboundedReceiver};

    use super::*;
    use crate::layout::Cell;

    #[derive(Debug, Clone, PartialEq)]
    enum CursorCall {
        Hide,
        Show,
        Warp(f64, f64),
        Locked(bool),
    }

    #[derive(Clone, Default)]
    struct MockCursor(Arc<Mutex<Vec<CursorCall>>>);

    impl CursorControl for MockCursor {
        fn hide(&mut self) {
            self.0.lock().unwrap().push(CursorCall::Hide);
        }
        fn show(&mut self) {
            self.0.lock().unwrap().push(CursorCall::Show);
        }
        fn warp(&mut self, x: f64, y: f64) {
            self.0.lock().unwrap().push(CursorCall::Warp(x, y));
        }
        fn set_locked(&mut self, locked: bool) {
            self.0.lock().unwrap().push(CursorCall::Locked(locked));
        }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    fn geom(id: &str, w: f64, h: f64, col: i32, row: i32) -> DeviceGeom {
        DeviceGeom {
            id: id.into(),
            displays: vec![rect(0.0, 0.0, w, h)],
            cells: vec![Cell { col, row }],
        }
    }

    /// Mac（1000×800）在中间格子，Windows（2000×1000）在右边相邻格子。
    fn setup() -> (Router, MockCursor, UnboundedReceiver<Outbound>) {
        let cursor = MockCursor::default();
        let mut router = Router::new("mac".into(), Os::MacOs, Box::new(cursor.clone()));
        router.set_world(World {
            devices: vec![
                geom("mac", 1000.0, 800.0, 2, 1),
                geom("win", 2000.0, 1000.0, 3, 1),
            ],
        });
        let (tx, rx) = mpsc::unbounded_channel();
        router.add_peer(
            "win".into(),
            PeerLink {
                os: Os::Windows,
                tx,
            },
        );
        router.set_device_order(vec!["win".into()]);
        (router, cursor, rx)
    }

    fn drain(rx: &mut UnboundedReceiver<Outbound>) -> Vec<Outbound> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn mv(x: f64, y: f64, dx: f64, dy: f64) -> InputEvent {
        InputEvent::Move { x, y, dx, dy }
    }

    fn key(usage: u16, down: bool) -> InputEvent {
        InputEvent::Key { usage, down }
    }

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    fn enter_of(msgs: &[Outbound]) -> (f32, f32) {
        msgs.iter()
            .find_map(|m| match m {
                Outbound::Stream(StreamMsg::Enter { x, y }) => Some((*x, *y)),
                _ => None,
            })
            .expect("expected Enter")
    }

    #[test]
    fn stays_local_away_from_edge() {
        let (mut r, _, mut rx) = setup();
        assert!(!r.handle(mv(500.0, 400.0, 30.0, 0.0)));
        assert!(drain(&mut rx).is_empty());
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn crosses_right_edge_proportionally_and_back() {
        let (mut r, cursor, mut rx) = setup();
        // 在 Mac 右边 50% 高度推出去 → Windows 左边 50% 高度
        assert!(r.handle(mv(999.0, 400.0, 5.0, 0.0)));
        assert_eq!(r.focused(), Some("win"));
        let (x, y) = enter_of(&drain(&mut rx));
        assert_eq!(x, 0.0);
        assert!((y - 500.0).abs() < 1.0, "y = {y}");
        assert_eq!(
            *cursor.0.lock().unwrap(),
            vec![CursorCall::Locked(true), CursorCall::Hide]
        );

        // 在 Windows 上移动：发本地绝对坐标
        assert!(r.handle(mv(999.0, 400.0, 100.0, 50.0)));
        let Outbound::Datagram(Datagram::MouseAbs { x, y, .. }) = drain(&mut rx).remove(0) else {
            panic!("expected MouseAbs");
        };
        assert_eq!(x, 100.0);
        assert!((y - 550.0).abs() < 1.0);

        // 往左推出 Windows 左边 → 回到 Mac 右边缘，高度按比例换算
        cursor.0.lock().unwrap().clear();
        assert!(r.handle(mv(999.0, 400.0, -200.0, 0.0)));
        assert_eq!(r.focused(), None);
        assert_eq!(drain(&mut rx), vec![Outbound::Stream(StreamMsg::Leave)]);
        let calls = cursor.0.lock().unwrap().clone();
        let CursorCall::Warp(wx, wy) = calls[0] else {
            panic!("expected warp, got {calls:?}");
        };
        assert_eq!(wx, 999.0);
        assert!((wy - 439.6).abs() < 1.0, "wy = {wy}");
        assert_eq!(&calls[1..], [CursorCall::Locked(false), CursorCall::Show]);
    }

    #[test]
    fn empty_neighbor_cell_blocks_crossing() {
        let (mut r, _, _rx) = setup();
        // 左边格子没有设备
        assert!(!r.handle(mv(0.0, 400.0, -5.0, 0.0)));
        // 往下推也没有
        assert!(!r.handle(mv(500.0, 799.0, 0.0, 5.0)));
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn clamps_at_outer_edges() {
        let (mut r, _, mut rx) = setup();
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        drain(&mut rx);
        // Windows 右边没有设备：钳制在最右
        r.handle(mv(0.0, 0.0, 5000.0, 0.0));
        assert_eq!(r.focused(), Some("win"));
        let Outbound::Datagram(Datagram::MouseAbs { x, .. }) = drain(&mut rx).remove(0) else {
            panic!("expected MouseAbs");
        };
        assert_eq!(x, 1999.0);
    }

    #[test]
    fn vertical_neighbor() {
        let (mut r, _, mut rx) = setup();
        let (tx, mut imac_rx) = mpsc::unbounded_channel();
        r.set_world(World {
            devices: vec![
                geom("mac", 1000.0, 800.0, 2, 1),
                geom("win", 2000.0, 1000.0, 3, 1),
                geom("imac", 2560.0, 1440.0, 2, 0),
            ],
        });
        r.add_peer("imac".into(), PeerLink { os: Os::MacOs, tx });
        // 从 Mac 顶边 25% 处推上去 → iMac 底边 25% 处
        assert!(r.handle(mv(250.0, 0.0, 0.0, -5.0)));
        assert_eq!(r.focused(), Some("imac"));
        let (x, y) = enter_of(&drain(&mut imac_rx));
        assert!((x - 639.75).abs() < 1.0, "x = {x}");
        assert_eq!(y, 1439.0);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn multi_display_crossing_uses_each_display_cell() {
        // Mac 主屏 (2,1) + 副屏在系统里位于主屏右侧、格子 (3,1)；Windows 在 Mac 副屏右边 (4,1)
        let cursor = MockCursor::default();
        let mut r = Router::new("mac".into(), Os::MacOs, Box::new(cursor.clone()));
        r.set_world(World {
            devices: vec![
                DeviceGeom {
                    id: "mac".into(),
                    displays: vec![
                        rect(0.0, 0.0, 1000.0, 800.0),
                        rect(1000.0, 0.0, 1000.0, 800.0),
                    ],
                    cells: vec![Cell { col: 2, row: 1 }, Cell { col: 3, row: 1 }],
                },
                geom("win", 2000.0, 1000.0, 4, 1),
            ],
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        r.add_peer(
            "win".into(),
            PeerLink {
                os: Os::Windows,
                tx,
            },
        );
        // 主屏右边缘是自己的副屏：交给系统处理，不切
        assert!(!r.handle(mv(999.0, 400.0, 5.0, 0.0)));
        // 副屏右边缘 → Windows
        assert!(r.handle(mv(1999.0, 400.0, 5.0, 0.0)));
        assert_eq!(r.focused(), Some("win"));
        let (x, y) = enter_of(&drain(&mut rx));
        assert_eq!(x, 0.0);
        assert!((y - 500.0).abs() < 1.0);
        // 从 Windows 左边回来 → Mac 副屏右边缘
        cursor.0.lock().unwrap().clear();
        r.handle(mv(0.0, 0.0, -50.0, 0.0));
        assert_eq!(r.focused(), None);
        let CursorCall::Warp(wx, _) = cursor.0.lock().unwrap()[0] else {
            panic!("expected warp");
        };
        assert_eq!(wx, 1999.0);
    }

    #[test]
    fn offline_peer_is_not_entered() {
        let (mut r, _, mut rx) = setup();
        r.remove_peer("win");
        assert!(!r.handle(mv(999.0, 400.0, 5.0, 0.0)));
        assert_eq!(r.focused(), None);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn no_switch_while_dragging() {
        let (mut r, _, _rx) = setup();
        r.handle(InputEvent::Button {
            button: MouseButton::Left,
            down: true,
        });
        assert!(!r.handle(mv(999.0, 400.0, 5.0, 0.0)));
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn cmd_maps_to_ctrl_and_held_keys_do_not_stick() {
        let (mut r, _, mut rx) = setup();
        assert!(!r.handle(key(hid::LEFT_SHIFT, true)));
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        drain(&mut rx);

        assert!(r.handle(key(hid::LEFT_GUI, true)));
        assert!(r.handle(key(0x06, true)));
        assert_eq!(
            drain(&mut rx),
            vec![
                Outbound::Stream(StreamMsg::Key {
                    usage: hid::LEFT_CTRL,
                    down: true
                }),
                Outbound::Stream(StreamMsg::Key {
                    usage: 0x06,
                    down: true
                }),
            ]
        );
        assert!(!r.handle(key(hid::LEFT_SHIFT, false)));

        r.tick();
        let Outbound::Stream(StreamMsg::KeyState { mut pressed }) = drain(&mut rx).remove(0) else {
            panic!("expected KeyState");
        };
        pressed.sort();
        assert_eq!(pressed, vec![0x06, hid::LEFT_CTRL]);
    }

    fn hold_hotkey_mods(r: &mut Router) {
        for usage in [hid::LEFT_CTRL, hid::LEFT_ALT, hid::LEFT_GUI] {
            r.handle(key(usage, true));
        }
    }

    fn profile(rules: &[(u16, u16)]) -> KeyboardProfile {
        KeyboardProfile {
            enabled: true,
            mappings: rules
                .iter()
                .map(|&(from, to)| crate::settings::KeyRemap { from, to })
                .collect(),
        }
    }

    #[test]
    fn custom_mapping_only_changes_remote_keys_in_its_direction() {
        let (mut r, _, mut rx) = setup();
        r.set_keyboard_settings(KeyboardSettings {
            mac_to_windows: profile(&[(4, 5), (5, 6), (hid::LEFT_GUI, hid::LEFT_GUI)]),
            windows_to_mac: profile(&[(4, 7)]),
        });
        assert!(!r.handle(key(4, true)));
        assert!(!r.handle(key(4, false)));
        assert!(drain(&mut rx).is_empty());
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        drain(&mut rx);
        for (physical, mapped) in [(4, 5), (5, 6), (hid::LEFT_GUI, hid::LEFT_GUI)] {
            for down in [true, false] {
                assert!(r.handle(key(physical, down)));
                assert_eq!(
                    drain(&mut rx),
                    vec![Outbound::Stream(StreamMsg::Key {
                        usage: mapped,
                        down
                    })]
                );
            }
        }
        r.return_home();
        r.host_os = Os::Windows;
        r.peers.get_mut("win").unwrap().os = Os::MacOs;
        assert_eq!(r.map_key("win", 4), 7);
    }

    #[test]
    fn receiver_override_and_changes_release_remote_keys() {
        let (mut r, _, mut rx) = setup();
        r.set_keyboard_settings(KeyboardSettings {
            mac_to_windows: profile(&[(4, 5)]),
            ..Default::default()
        });
        r.set_keyboard_override("win", profile(&[(4, 6)]));
        assert_eq!(r.map_key("win", 4), 6);
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        r.handle(key(4, true));
        drain(&mut rx);
        r.set_keyboard_override("win", KeyboardProfile::default());
        assert_eq!(r.focused(), None);
        assert!(r.remote_keys.is_empty());
        assert_eq!(drain(&mut rx), vec![Outbound::Stream(StreamMsg::Leave)]);
        assert_eq!(r.map_key("win", 4), 5);
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        drain(&mut rx);
        r.set_keyboard_settings(KeyboardSettings::default());
        assert_eq!(r.focused(), None);
        assert_eq!(drain(&mut rx), vec![Outbound::Stream(StreamMsg::Leave)]);
    }

    #[test]
    fn many_to_one_mapping_holds_output_until_all_sources_release() {
        let (mut r, _, mut rx) = setup();
        r.set_keyboard_settings(KeyboardSettings {
            mac_to_windows: profile(&[(hid::LEFT_GUI, hid::LEFT_CTRL)]),
            ..Default::default()
        });
        r.set_swap_cmd_ctrl(false);
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        r.handle(key(hid::LEFT_GUI, true));
        r.handle(key(hid::LEFT_CTRL, true));
        drain(&mut rx);
        r.tick();
        assert_eq!(
            drain(&mut rx),
            vec![Outbound::Stream(StreamMsg::KeyState {
                pressed: vec![hid::LEFT_CTRL]
            })]
        );
        r.handle(key(hid::LEFT_GUI, false));
        assert!(drain(&mut rx).is_empty());
        r.handle(key(hid::LEFT_CTRL, false));
        assert_eq!(
            drain(&mut rx),
            vec![Outbound::Stream(StreamMsg::Key {
                usage: hid::LEFT_CTRL,
                down: false
            })]
        );
    }

    #[test]
    fn return_home_hotkey_uses_physical_keys_before_custom_mapping() {
        let (mut r, _, mut rx) = setup();
        r.set_keyboard_settings(KeyboardSettings {
            mac_to_windows: profile(&[
                (hid::LEFT_GUI, 4),
                (hid::LEFT_CTRL, 5),
                (hid::LEFT_ALT, 6),
                (hid::ESCAPE, 7),
            ]),
            ..Default::default()
        });
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        hold_hotkey_mods(&mut r);
        drain(&mut rx);
        assert!(r.handle(key(hid::ESCAPE, true)));
        assert_eq!(r.focused(), None);
        assert_eq!(drain(&mut rx), vec![Outbound::Stream(StreamMsg::Leave)]);
    }

    #[test]
    fn escape_chord_returns_home() {
        let (mut r, cursor, mut rx) = setup();
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        hold_hotkey_mods(&mut r);
        drain(&mut rx);
        cursor.0.lock().unwrap().clear();

        assert!(r.handle(key(hid::ESCAPE, true)));
        assert_eq!(r.focused(), None);
        assert_eq!(drain(&mut rx), vec![Outbound::Stream(StreamMsg::Leave)]);
        assert_eq!(cursor.0.lock().unwrap()[0], CursorCall::Warp(500.0, 400.0));
        assert!(r.handle(key(hid::ESCAPE, false)));
    }

    #[test]
    fn hotkey_digit_jumps_and_remembers_position() {
        let (mut r, _, mut rx) = setup();
        hold_hotkey_mods(&mut r);
        assert!(r.handle(key(DIGIT_1 + 1, true)));
        assert_eq!(r.focused(), Some("win"));
        assert_eq!(enter_of(&drain(&mut rx)), (1000.0, 500.0));
        assert!(r.handle(key(DIGIT_1 + 1, false)));

        r.handle(mv(0.0, 0.0, 100.0, 0.0));
        assert!(r.handle(key(DIGIT_1, true)));
        assert_eq!(r.focused(), None);
        drain(&mut rx);
        r.handle(key(DIGIT_1 + 1, true));
        assert_eq!(enter_of(&drain(&mut rx)), (1100.0, 500.0));
    }

    #[test]
    fn lock_chords_lock_the_focused_device() {
        let (mut r, _, mut rx) = setup();
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        drain(&mut rx);
        // Mac 的 Ctrl+Cmd+Q：发锁屏消息，Q 本身不转发
        r.handle(key(hid::LEFT_CTRL, true));
        r.handle(key(hid::LEFT_GUI, true));
        drain(&mut rx);
        assert!(r.handle(key(KEY_Q, true)));
        assert_eq!(
            drain(&mut rx),
            vec![Outbound::Stream(StreamMsg::LockScreen)]
        );
        assert!(r.handle(key(KEY_Q, false)));
        assert!(drain(&mut rx).is_empty());

        // 组合键 + Delete 也可以
        hold_hotkey_mods(&mut r);
        drain(&mut rx);
        assert!(r.handle(key(BACKSPACE, true)));
        assert_eq!(
            drain(&mut rx),
            vec![Outbound::Stream(StreamMsg::LockScreen)]
        );

        // 在本机时组合键 + Delete 不拦截
        r.return_home();
        drain(&mut rx);
        assert!(!r.handle(key(FORWARD_DELETE, true)));
    }

    #[test]
    fn edge_lock_blocks_edges_but_not_hotkeys() {
        let (mut r, _, _rx) = setup();
        hold_hotkey_mods(&mut r);
        r.handle(key(KEY_L, true));
        assert!(r.edge_locked());
        assert!(!r.handle(mv(999.0, 400.0, 5.0, 0.0)));
        assert_eq!(r.focused(), None);
        r.handle(key(DIGIT_1 + 1, true));
        assert_eq!(r.focused(), Some("win"));
    }

    #[test]
    fn dwell_time_requires_holding_at_edge() {
        let (mut r, _, _rx) = setup();
        r.set_switch_settings(SwitchSettings {
            dwell_ms: 200,
            ..Default::default()
        });
        let t = Instant::now();
        assert!(!r.handle_at(mv(999.0, 400.0, 5.0, 0.0), t));
        assert!(!r.handle_at(mv(999.0, 400.0, 5.0, 0.0), ms(t, 150)));
        r.handle_at(mv(990.0, 400.0, -5.0, 0.0), ms(t, 160));
        assert!(!r.handle_at(mv(999.0, 400.0, 5.0, 0.0), ms(t, 250)));
        assert!(r.handle_at(mv(999.0, 400.0, 5.0, 0.0), ms(t, 460)));
        assert_eq!(r.focused(), Some("win"));
    }

    #[test]
    fn double_tap_requires_two_pushes() {
        let (mut r, _, _rx) = setup();
        r.set_switch_settings(SwitchSettings {
            double_tap: true,
            ..Default::default()
        });
        let t = Instant::now();
        assert!(!r.handle_at(mv(999.0, 400.0, 5.0, 0.0), t));
        assert!(!r.handle_at(mv(999.0, 400.0, 5.0, 0.0), ms(t, 50)));
        r.handle_at(mv(990.0, 400.0, -9.0, 0.0), ms(t, 100));
        assert!(r.handle_at(mv(999.0, 400.0, 9.0, 0.0), ms(t, 200)));
        assert_eq!(r.focused(), Some("win"));

        let (mut r, _, _rx) = setup();
        r.set_switch_settings(SwitchSettings {
            double_tap: true,
            ..Default::default()
        });
        r.handle_at(mv(999.0, 400.0, 5.0, 0.0), t);
        r.handle_at(mv(990.0, 400.0, -9.0, 0.0), ms(t, 100));
        assert!(!r.handle_at(mv(999.0, 400.0, 9.0, 0.0), ms(t, 900)));
    }

    #[test]
    fn modifier_and_corner_deadzone() {
        let (mut r, _, _rx) = setup();
        r.set_switch_settings(SwitchSettings {
            require_modifier: Some(Modifier::Shift),
            corner_deadzone: 50.0,
            ..Default::default()
        });
        assert!(!r.handle(mv(999.0, 400.0, 5.0, 0.0)));
        r.handle(key(hid::LEFT_SHIFT, true));
        assert!(!r.handle(mv(999.0, 10.0, 5.0, 0.0)));
        assert!(r.handle(mv(999.0, 400.0, 5.0, 0.0)));
    }

    #[test]
    fn per_device_speed_and_scroll_inversion() {
        let (mut r, _, mut rx) = setup();
        r.set_device_settings(HashMap::from([(
            "win".to_string(),
            DeviceSettings {
                pointer_speed: 2.0,
                scroll_speed: 0.5,
                invert_scroll_y: true,
                ..Default::default()
            },
        )]));
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        drain(&mut rx);
        r.handle(mv(0.0, 0.0, 10.0, 0.0));
        let Outbound::Datagram(Datagram::MouseAbs { x, .. }) = drain(&mut rx).remove(0) else {
            panic!("expected MouseAbs");
        };
        assert_eq!(x, 20.0);
        assert!(r.handle(InputEvent::WheelLines { dx: 240, dy: 240 }));
        assert_eq!(
            drain(&mut rx),
            vec![Outbound::Stream(StreamMsg::WheelLines {
                dx: 120,
                dy: -120
            })]
        );
        r.return_home();
        assert!(!r.handle(InputEvent::WheelLines { dx: 0, dy: 120 }));
    }

    #[test]
    fn pause_returns_home_and_blocks_switching() {
        let (mut r, _, _rx) = setup();
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        r.set_paused(true);
        assert_eq!(r.focused(), None);
        assert!(!r.handle(mv(999.0, 400.0, 5.0, 0.0)));
        r.set_paused(false);
        assert!(r.handle(mv(999.0, 400.0, 5.0, 0.0)));
    }

    #[test]
    fn disconnect_while_remote_returns_home() {
        let (mut r, _, _rx) = setup();
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        r.remove_peer("win");
        assert_eq!(r.focused(), None);
    }
}
