//! 输入路由：决定每个本地输入事件留在主机还是发往某台被控设备。
//!
//! 运行在采集线程里（macOS 的事件 tap 回调），必须快，不做 I/O：
//! 发往被控设备的消息只是塞进通道，由网络任务发送。

use std::collections::{HashMap, HashSet};

use roam_proto::{Datagram, MouseButton, Os, StreamMsg};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    keymap::{hid, remap_for_target},
    layout::{DeviceId, World},
};

/// 采集层产生的本地输入事件（坐标为主机本地逻辑点）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// `x, y` 为移动后的光标位置，`dx, dy` 为原始位移。
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
    /// 光标在被控设备上，`cursor` 为全局坐标。
    Remote {
        id: DeviceId,
        cursor: (f64, f64),
    },
}

/// 判断"光标顶到屏幕边缘"时向外探测的距离。
const EDGE_PROBE: f64 = 2.0;

pub struct Router {
    host_id: DeviceId,
    host_os: Os,
    world: World,
    peers: HashMap<DeviceId, PeerLink>,
    cursor: Box<dyn CursorControl>,
    focus: Focus,
    swap_cmd_ctrl: bool,
    /// 在主机上按下、主机已经看到按下的键。抬起时必须交给主机，否则主机会卡键。
    local_keys: HashSet<u16>,
    /// 在被控设备上按下的键（物理 usage）。
    remote_keys: HashSet<u16>,
    buttons_down: HashSet<MouseButton>,
    mouse_seq: u32,
    /// 暂停时不会切到任何被控设备。
    paused: bool,
}

impl Router {
    pub fn new(host_id: DeviceId, host_os: Os, cursor: Box<dyn CursorControl>) -> Self {
        Self {
            host_id,
            host_os,
            world: World::default(),
            peers: HashMap::new(),
            cursor,
            focus: Focus::Local,
            swap_cmd_ctrl: true,
            local_keys: HashSet::new(),
            remote_keys: HashSet::new(),
            buttons_down: HashSet::new(),
            mouse_seq: 0,
            paused: false,
        }
    }

    pub fn set_swap_cmd_ctrl(&mut self, swap: bool) {
        self.swap_cmd_ctrl = swap;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            self.return_home();
        }
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
                .collect();
            self.send(&id, Outbound::Stream(StreamMsg::KeyState { pressed }));
        }
    }

    /// 强制回到主机（逃生热键、断线等）。
    pub fn return_home(&mut self) {
        let Focus::Remote { id, .. } = &self.focus else {
            return;
        };
        let id = id.clone();
        self.send(&id, Outbound::Stream(StreamMsg::Leave));
        let home = self
            .world
            .find(&self.host_id)
            .and_then(|h| h.displays.first().copied());
        let (x, y) = home
            .map(|d| (d.x + d.w / 2.0, d.y + d.h / 2.0))
            .unwrap_or((0.0, 0.0));
        self.enter_local(x, y);
    }

    /// 处理一个本地输入事件。返回 `true` 表示事件已被转发，应在主机上吞掉。
    pub fn handle(&mut self, ev: InputEvent) -> bool {
        match ev {
            InputEvent::Move { x, y, dx, dy } => self.on_move(x, y, dx, dy),
            InputEvent::Button { button, down } => self.on_button(button, down),
            InputEvent::Key { usage, down } => self.on_key(usage, down),
            InputEvent::WheelLines { dx, dy } => {
                self.forward(Outbound::Stream(StreamMsg::WheelLines { dx, dy }))
            }
            InputEvent::WheelPixels { dx, dy } => {
                self.forward(Outbound::Stream(StreamMsg::WheelPixels { dx, dy }))
            }
        }
    }

    // ------------------------------------------------------------ 鼠标

    fn on_move(&mut self, x: f64, y: f64, dx: f64, dy: f64) -> bool {
        match self.focus.clone() {
            Focus::Local => self.move_local(x, y, dx, dy),
            Focus::Remote { id, cursor } => {
                self.move_remote(id, (cursor.0 + dx, cursor.1 + dy));
                true
            }
        }
    }

    fn move_local(&mut self, x: f64, y: f64, dx: f64, dy: f64) -> bool {
        if self.paused || !self.buttons_down.is_empty() {
            return false; // 暂停或拖拽中不切屏
        }
        let Some(host) = self.world.find(&self.host_id) else {
            return false;
        };
        let (gx, gy) = host.to_global(x, y);
        // 只有光标已经顶在边缘、还在往外推时才切屏
        let probe = (gx + dx.signum() * EDGE_PROBE, gy + dy.signum() * EDGE_PROBE);
        if host.contains_global(probe.0, probe.1) {
            return false;
        }
        let Some((target, _)) = self.world.device_near(probe.0, probe.1, &self.host_id) else {
            return false;
        };
        if !self.peers.contains_key(&target.id) {
            return false;
        }
        let id = target.id.clone();
        let entry = target.clamp_global(gx + dx, gy + dy);
        self.cursor.set_locked(true);
        self.cursor.hide();
        self.enter_remote(id, entry);
        true
    }

    fn move_remote(&mut self, id: DeviceId, (cx, cy): (f64, f64)) {
        let Some(current) = self.world.find(&id) else {
            self.return_home();
            return;
        };
        if current.contains_global(cx, cy) {
            self.focus = Focus::Remote {
                id: id.clone(),
                cursor: (cx, cy),
            };
            self.send_abs(&id);
            return;
        }

        // 离开了当前设备：拖拽中不切，其余情况找相邻设备
        let next = if self.buttons_down.is_empty() {
            self.world
                .device_near(cx, cy, &id)
                .map(|(d, p)| (d.id.clone(), p))
        } else {
            None
        };
        match next {
            Some((next_id, (nx, ny))) if next_id == self.host_id => {
                self.send(&id, Outbound::Stream(StreamMsg::Leave));
                let host = self.world.find(&self.host_id).expect("host in world");
                let (lx, ly) = host.to_local(nx, ny);
                self.enter_local(lx, ly);
            }
            Some((next_id, entry)) if self.peers.contains_key(&next_id) => {
                self.send(&id, Outbound::Stream(StreamMsg::Leave));
                self.remote_keys.clear();
                self.enter_remote(next_id, entry);
            }
            _ => {
                let clamped = current.clamp_global(cx, cy);
                self.focus = Focus::Remote {
                    id: id.clone(),
                    cursor: clamped,
                };
                self.send_abs(&id);
            }
        }
    }

    fn enter_remote(&mut self, id: DeviceId, cursor: (f64, f64)) {
        let (x, y) = self.local_of(&id, cursor);
        self.focus = Focus::Remote {
            id: id.clone(),
            cursor,
        };
        self.send(
            &id,
            Outbound::Stream(StreamMsg::Enter {
                x: x as f32,
                y: y as f32,
            }),
        );
    }

    fn enter_local(&mut self, x: f64, y: f64) {
        self.focus = Focus::Local;
        self.remote_keys.clear();
        self.cursor.warp(x, y);
        self.cursor.set_locked(false);
        self.cursor.show();
    }

    fn send_abs(&mut self, id: &str) {
        let Focus::Remote { cursor, .. } = &self.focus else {
            return;
        };
        let (x, y) = self.local_of(id, *cursor);
        self.mouse_seq = self.mouse_seq.wrapping_add(1);
        let msg = Datagram::MouseAbs {
            seq: self.mouse_seq,
            x: x as f32,
            y: y as f32,
        };
        self.send(id, Outbound::Datagram(msg));
    }

    fn local_of(&self, id: &str, (gx, gy): (f64, f64)) -> (f64, f64) {
        self.world
            .find(id)
            .map(|d| d.to_local(gx, gy))
            .unwrap_or((gx, gy))
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
            let (x, y) = self.local_of(&id, cursor);
            self.send(
                &id,
                Outbound::Stream(StreamMsg::MouseTo {
                    x: x as f32,
                    y: y as f32,
                }),
            );
        }
        self.send(&id, Outbound::Stream(StreamMsg::Button { button, down }));
        true
    }

    // ------------------------------------------------------------ 键盘

    fn on_key(&mut self, usage: u16, down: bool) -> bool {
        if down && usage == hid::ESCAPE && self.escape_chord_held() {
            if self.focused().is_some() {
                self.return_home();
                return true;
            }
            return false;
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
        self.send(
            &id,
            Outbound::Stream(StreamMsg::Key {
                usage: mapped,
                down,
            }),
        );
        true
    }

    /// 逃生热键 Ctrl + Option + Cmd + Esc。
    fn escape_chord_held(&self) -> bool {
        let held = |a: u16, b: u16| {
            [a, b]
                .iter()
                .any(|k| self.local_keys.contains(k) || self.remote_keys.contains(k))
        };
        held(hid::LEFT_CTRL, hid::RIGHT_CTRL)
            && held(hid::LEFT_ALT, hid::RIGHT_ALT)
            && held(hid::LEFT_GUI, hid::RIGHT_GUI)
    }

    fn map_key(&self, id: &str, usage: u16) -> u16 {
        let to = self.peers.get(id).map(|p| p.os).unwrap_or(Os::Other);
        remap_for_target(usage, self.host_os, to, self.swap_cmd_ctrl)
    }

    // ------------------------------------------------------------ 发送

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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use roam_proto::Rect;
    use tokio::sync::mpsc::{self, UnboundedReceiver};

    use super::*;
    use crate::{config::Point, layout::DeviceGeom};

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

    /// Mac（1000×800）在左，Windows（1920×1080）紧贴在右。
    fn setup() -> (Router, MockCursor, UnboundedReceiver<Outbound>) {
        let cursor = MockCursor::default();
        let mut router = Router::new("mac".into(), Os::MacOs, Box::new(cursor.clone()));
        router.set_world(World {
            devices: vec![
                DeviceGeom {
                    id: "mac".into(),
                    displays: vec![Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 1000.0,
                        h: 800.0,
                    }],
                    pos: Point { x: 0.0, y: 0.0 },
                },
                DeviceGeom {
                    id: "win".into(),
                    displays: vec![Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 1920.0,
                        h: 1080.0,
                    }],
                    pos: Point { x: 1000.0, y: 0.0 },
                },
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
        (router, cursor, rx)
    }

    fn drain(rx: &mut UnboundedReceiver<Outbound>) -> Vec<Outbound> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn mv(x: f64, y: f64, dx: f64, dy: f64) -> InputEvent {
        InputEvent::Move { x, y, dx, dy }
    }

    #[test]
    fn stays_local_away_from_edge() {
        let (mut r, _, mut rx) = setup();
        assert!(!r.handle(mv(500.0, 400.0, 30.0, 0.0)));
        assert!(drain(&mut rx).is_empty());
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn crosses_right_edge_and_back() {
        let (mut r, cursor, mut rx) = setup();
        // 顶在右边缘继续往右推
        assert!(r.handle(mv(999.0, 400.0, 5.0, 0.0)));
        assert_eq!(r.focused(), Some("win"));
        assert_eq!(
            drain(&mut rx),
            vec![Outbound::Stream(StreamMsg::Enter { x: 4.0, y: 400.0 })]
        );
        assert_eq!(
            *cursor.0.lock().unwrap(),
            vec![CursorCall::Locked(true), CursorCall::Hide]
        );

        // 在 Windows 上移动：发绝对坐标
        assert!(r.handle(mv(999.0, 400.0, 100.0, 50.0)));
        assert_eq!(
            drain(&mut rx),
            vec![Outbound::Datagram(Datagram::MouseAbs {
                seq: 1,
                x: 104.0,
                y: 450.0
            })]
        );

        // 往左移出 Windows 左边缘 → 回到 Mac，光标放在对应位置
        cursor.0.lock().unwrap().clear();
        assert!(r.handle(mv(999.0, 400.0, -110.0, 0.0)));
        assert_eq!(r.focused(), None);
        assert_eq!(drain(&mut rx), vec![Outbound::Stream(StreamMsg::Leave)]);
        assert_eq!(
            *cursor.0.lock().unwrap(),
            vec![
                CursorCall::Warp(994.0, 450.0),
                CursorCall::Locked(false),
                CursorCall::Show
            ]
        );
    }

    #[test]
    fn clamps_at_outer_edges() {
        let (mut r, _, mut rx) = setup();
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        drain(&mut rx);
        // 往右推出 Windows 外侧：没有设备，钳制在最右
        r.handle(mv(0.0, 0.0, 5000.0, 0.0));
        assert_eq!(r.focused(), Some("win"));
        assert_eq!(
            drain(&mut rx),
            vec![Outbound::Datagram(Datagram::MouseAbs {
                seq: 1,
                x: 1919.0,
                y: 400.0
            })]
        );
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
        // 在 Mac 上按住 Shift 后切到 Windows
        assert!(!r.handle(InputEvent::Key {
            usage: hid::LEFT_SHIFT,
            down: true
        }));
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        drain(&mut rx);

        // Cmd+C 在 Windows 上变成 Ctrl+C
        assert!(r.handle(InputEvent::Key {
            usage: hid::LEFT_GUI,
            down: true
        }));
        assert!(r.handle(InputEvent::Key {
            usage: 0x06,
            down: true
        }));
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

        // Shift 抬起交还给 Mac（Mac 看到过按下）
        assert!(!r.handle(InputEvent::Key {
            usage: hid::LEFT_SHIFT,
            down: false
        }));

        // 心跳同步按键状态
        r.tick();
        let Outbound::Stream(StreamMsg::KeyState { mut pressed }) = drain(&mut rx).remove(0) else {
            panic!("expected KeyState");
        };
        pressed.sort();
        assert_eq!(pressed, vec![0x06, hid::LEFT_CTRL]);
    }

    #[test]
    fn escape_chord_returns_home() {
        let (mut r, cursor, mut rx) = setup();
        r.handle(mv(999.0, 400.0, 5.0, 0.0));
        for usage in [hid::LEFT_CTRL, hid::LEFT_ALT, hid::LEFT_GUI] {
            r.handle(InputEvent::Key { usage, down: true });
        }
        drain(&mut rx);
        cursor.0.lock().unwrap().clear();

        assert!(r.handle(InputEvent::Key {
            usage: hid::ESCAPE,
            down: true
        }));
        assert_eq!(r.focused(), None);
        assert_eq!(drain(&mut rx), vec![Outbound::Stream(StreamMsg::Leave)]);
        assert_eq!(cursor.0.lock().unwrap()[0], CursorCall::Warp(500.0, 400.0));
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
