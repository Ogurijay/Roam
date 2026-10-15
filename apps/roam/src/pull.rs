//! 按需传文件：复制文件后切屏只「宣告」（文件名和大小），到另一台电脑上按粘贴时才真正传输。
//!
//! 该不该拦下一次粘贴，看系统剪贴板的变化计数：收到宣告时记下计数，按粘贴时计数没变，
//! 说明这之后本机没复制过别的东西，最近一次复制的就是对方的文件；变了就照常粘贴。

use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

use roam_core::keymap::hid;
use roam_proto::{FileAnnounce, Os};

/// 按下粘贴后这么久内传完，就自动替用户粘贴；更慢的传完后提示用户再按一次。
pub const AUTO_PASTE_WITHIN: Duration = Duration::from_secs(5);
/// 同一批文件正在取的时候，重复按粘贴不重复请求（超过这个时间认为上次失败了，允许重试）。
const PULL_RETRY_AFTER: Duration = Duration::from_secs(30);
/// 记住最近宣告过的几批文件。
const OUTBOX_LEN: usize = 16;

/// 宣告出去的文件在哪里。
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// 本机的文件。
    Local(Vec<PathBuf>),
    /// 某台被控设备上的文件，由主机中转：设备 ID 和它给的凭据。
    Remote { peer: String, token: u64 },
}

/// 别处复制的、可以在本机按粘贴时去取的文件。
#[derive(Debug, Clone)]
pub struct Pending {
    pub announce: FileAnnounce,
    /// 文件在哪台设备上（主机用来知道向谁要；被控端总是向主机要）。
    pub peer: Option<String>,
    seq: u64,
}

#[derive(Debug, Clone, Copy)]
struct Pulling {
    token: u64,
    started: Instant,
    auto_paste: bool,
}

/// 一台设备上按需传文件的状态。
#[derive(Default)]
pub struct Shelf {
    pending: Mutex<Option<Pending>>,
    pulling: Mutex<Option<Pulling>>,
    outbox: Mutex<VecDeque<(u64, Source)>>,
}

impl Shelf {
    /// 收到宣告：记下来，连同此刻本机剪贴板的变化计数。
    pub fn arm(&self, announce: FileAnnounce, peer: Option<String>) {
        let Some(seq) = roam_input::clipboard_seq() else {
            return;
        };
        *self.pending.lock().unwrap() = Some(Pending {
            announce,
            peer,
            seq,
        });
    }

    /// 宣告之后本机剪贴板没变过时，返回待取的文件；本机复制过别的东西就作废。
    pub fn current(&self) -> Option<Pending> {
        let mut pending = self.pending.lock().unwrap();
        let p = pending.as_ref()?;
        if roam_input::clipboard_seq() == Some(p.seq) {
            Some(p.clone())
        } else {
            *pending = None;
            None
        }
    }

    /// 用户按了粘贴，要去取 `token` 对应的文件。返回 false 表示同一批正在取，不用重复请求。
    pub fn begin_pull(&self, token: u64) -> bool {
        let mut pulling = self.pulling.lock().unwrap();
        if let Some(p) = &mut *pulling
            && p.token == token
            && p.started.elapsed() < PULL_RETRY_AFTER
        {
            return false;
        }
        *pulling = Some(Pulling {
            token,
            started: Instant::now(),
            auto_paste: true,
        });
        true
    }

    /// 光标离开了这台电脑：文件到了也不要自动粘贴，免得粘到别处。
    pub fn cancel_auto_paste(&self) {
        if let Some(p) = &mut *self.pulling.lock().unwrap() {
            p.auto_paste = false;
        }
    }

    /// 收到了一批文件。是本机粘贴触发的就返回 Some（值表示要不要自动粘贴），否则返回 None。
    pub fn pull_done(&self) -> Option<bool> {
        let p = self.pulling.lock().unwrap().take()?;
        *self.pending.lock().unwrap() = None;
        Some(p.auto_paste && p.started.elapsed() < AUTO_PASTE_WITHIN)
    }

    /// 记下本机宣告出去的文件，返回对方来取时要带的凭据。
    pub fn register(&self, source: Source) -> u64 {
        let token = crate::files::new_id();
        let mut outbox = self.outbox.lock().unwrap();
        outbox.push_back((token, source));
        while outbox.len() > OUTBOX_LEN {
            outbox.pop_front();
        }
        token
    }

    pub fn lookup(&self, token: u64) -> Option<Source> {
        self.outbox
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| *t == token)
            .map(|(_, s)| s.clone())
    }
}

/// 是不是目标系统上的"粘贴"：Windows 是 Ctrl+V，macOS 是 Cmd+V（不管还按着别的修饰键）。
pub fn is_paste(os: Os, usage: u16, held: impl Fn(u16) -> bool) -> bool {
    usage == hid::V
        && match os {
            Os::MacOs => held(hid::LEFT_GUI) || held(hid::RIGHT_GUI),
            _ => held(hid::LEFT_CTRL) || held(hid::RIGHT_CTRL),
        }
}

/// 粘贴键的修饰键：Windows 是 Ctrl，macOS 是 Cmd。
pub fn paste_modifier(os: Os) -> (u16, u16) {
    match os {
        Os::MacOs => (hid::LEFT_GUI, hid::RIGHT_GUI),
        _ => (hid::LEFT_CTRL, hid::RIGHT_CTRL),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// 照常交给系统。
    Pass,
    /// 吞掉（被拦下的那次 V 的抬起）。
    Swallow,
    /// 吞掉，并去取文件。
    Pull,
}

/// 跟踪按着的键，识别要拦下的粘贴。拦下了 V 的按下，它的抬起也要一起拦下。
#[derive(Debug, Default)]
pub struct PasteKeys {
    held: HashSet<u16>,
    swallow_up: bool,
}

impl PasteKeys {
    /// `armed` 只在识别出粘贴时调用：返回 true 表示有待取的文件、要拦下这次粘贴。
    pub fn on_key(
        &mut self,
        os: Os,
        usage: u16,
        down: bool,
        armed: impl FnOnce() -> bool,
    ) -> KeyAction {
        if down {
            self.held.insert(usage);
        } else {
            self.held.remove(&usage);
        }
        if usage == hid::V && !down && self.swallow_up {
            self.swallow_up = false;
            return KeyAction::Swallow;
        }
        if down && is_paste(os, usage, |k| self.held.contains(&k)) && armed() {
            self.swallow_up = true;
            return KeyAction::Pull;
        }
        KeyAction::Pass
    }

    pub fn holds(&self, usage: u16) -> bool {
        self.held.contains(&usage)
    }

    /// 按主机的按键状态纠正：主机认为没按着的键都算松开。
    pub fn sync(&mut self, pressed: &[u16]) {
        self.held.retain(|k| pressed.contains(k));
    }

    /// 所有键都松开了（切屏、断线时）。
    pub fn reset(&mut self) {
        self.held.clear();
        self.swallow_up = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_chord_depends_on_target_os() {
        let ctrl = |k: u16| k == hid::LEFT_CTRL;
        let cmd = |k: u16| k == hid::LEFT_GUI;
        assert!(is_paste(Os::Windows, hid::V, ctrl));
        assert!(!is_paste(Os::Windows, hid::V, cmd));
        assert!(is_paste(Os::MacOs, hid::V, cmd));
        assert!(!is_paste(Os::MacOs, hid::V, ctrl));
        assert!(!is_paste(Os::Windows, 0x06, ctrl)); // Ctrl+C
    }

    #[test]
    fn swallows_the_paste_and_its_key_up_only_when_armed() {
        let mut keys = PasteKeys::default();
        let os = Os::Windows;
        assert_eq!(
            keys.on_key(os, hid::LEFT_CTRL, true, || true),
            KeyAction::Pass
        );
        // 没有待取的文件：照常粘贴
        assert_eq!(keys.on_key(os, hid::V, true, || false), KeyAction::Pass);
        assert_eq!(keys.on_key(os, hid::V, false, || false), KeyAction::Pass);
        // 有待取的文件：拦下按下和抬起
        assert_eq!(keys.on_key(os, hid::V, true, || true), KeyAction::Pull);
        assert_eq!(keys.on_key(os, hid::V, false, || true), KeyAction::Swallow);
        assert_eq!(
            keys.on_key(os, hid::LEFT_CTRL, false, || true),
            KeyAction::Pass
        );
        // 没按 Ctrl 的 V 只是打字
        assert_eq!(keys.on_key(os, hid::V, true, || true), KeyAction::Pass);
    }

    #[test]
    fn repeated_paste_does_not_request_twice() {
        let shelf = Shelf::default();
        let token = shelf.register(Source::Local(vec![PathBuf::from("a.txt")]));
        assert_eq!(
            shelf.lookup(token),
            Some(Source::Local(vec![PathBuf::from("a.txt")]))
        );
        assert!(shelf.begin_pull(token));
        assert!(!shelf.begin_pull(token));
        // 文件到了：自动粘贴
        assert_eq!(shelf.pull_done(), Some(true));
        // 不是本机粘贴触发的
        assert_eq!(shelf.pull_done(), None);

        assert!(shelf.begin_pull(token));
        shelf.cancel_auto_paste();
        assert_eq!(shelf.pull_done(), Some(false));
    }

    #[test]
    fn outbox_keeps_recent_entries() {
        let shelf = Shelf::default();
        let first = shelf.register(Source::Local(vec![]));
        for _ in 0..OUTBOX_LEN {
            shelf.register(Source::Local(vec![]));
        }
        assert_eq!(shelf.lookup(first), None);
    }
}
