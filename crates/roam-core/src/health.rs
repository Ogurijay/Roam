//! 应用层心跳：判断对方是否还在响应，统计中断次数。
//!
//! 主机每隔 [`HEARTBEAT`] 发一次心跳，被控端回复。任何一端长时间收不到对方的数据，
//! 说明网络或对方进程出了问题；中断的时间和时长会写进日志，方便事后排查。

use std::time::{Duration, Instant};

/// 心跳间隔。
pub const HEARTBEAT: Duration = Duration::from_millis(250);
/// 超过这么久没收到对方的数据，记为一次中断。
pub const DOWN_AFTER: Duration = Duration::from_secs(1);
/// 中断超过这么久，认为对方暂时不可用：主机把光标收回本机，被控端松开所有按键。
pub const STALL_AFTER: Duration = Duration::from_secs(3);
/// 定时器比预定时间晚这么多，说明本机进程被系统节流了（例如 macOS 的 App Nap）。
pub const TIMER_LATE: Duration = Duration::from_millis(500);

/// 链路状态的变化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEvent {
    /// 开始收不到对方的数据。
    Down,
    /// 中断已持续 [`STALL_AFTER`]。
    Stalled,
}

#[derive(Debug, Default, Clone)]
pub struct LinkHealth {
    last: Option<Instant>,
    down: bool,
    stalled: bool,
    /// 已恢复的中断次数。
    pub drops: u32,
    /// 最长的一次中断。
    pub longest: Duration,
}

impl LinkHealth {
    /// 收到了对方的数据。此前处于中断状态时返回中断时长。
    pub fn heard(&mut self, now: Instant) -> Option<Duration> {
        let outage = match self.last {
            Some(last) if self.down => Some(now.saturating_duration_since(last)),
            _ => None,
        };
        if let Some(outage) = outage {
            self.drops += 1;
            self.longest = self.longest.max(outage);
        }
        self.last = Some(now);
        self.down = false;
        self.stalled = false;
        outage
    }

    /// 定期调用，状态变化时返回事件。
    /// 还没收到过对方的数据时不做判断（老版本的对方不回心跳）。
    pub fn check(&mut self, now: Instant) -> Option<LinkEvent> {
        let gap = self.silent_for(now)?;
        if !self.stalled && gap >= STALL_AFTER {
            self.down = true;
            self.stalled = true;
            return Some(LinkEvent::Stalled);
        }
        if !self.down && gap >= DOWN_AFTER {
            self.down = true;
            return Some(LinkEvent::Down);
        }
        None
    }

    /// 距离上次收到对方数据过了多久。
    pub fn silent_for(&self, now: Instant) -> Option<Duration> {
        self.last.map(|last| now.saturating_duration_since(last))
    }

    /// 对方是否已经长时间没有响应。
    pub fn stalled(&self) -> bool {
        self.stalled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_peer_goes_down_then_stalls_then_recovers() {
        let t0 = Instant::now();
        let mut h = LinkHealth::default();
        // 还没收到过数据：不判断
        assert_eq!(h.check(t0 + Duration::from_secs(10)), None);

        h.heard(t0);
        assert_eq!(h.check(t0 + Duration::from_millis(500)), None);
        assert_eq!(
            h.check(t0 + Duration::from_millis(1100)),
            Some(LinkEvent::Down)
        );
        assert_eq!(h.check(t0 + Duration::from_millis(1500)), None);
        assert!(!h.stalled());
        assert_eq!(
            h.check(t0 + Duration::from_millis(3100)),
            Some(LinkEvent::Stalled)
        );
        assert!(h.stalled());
        assert_eq!(h.check(t0 + Duration::from_secs(5)), None);

        let outage = h.heard(t0 + Duration::from_secs(6));
        assert_eq!(outage, Some(Duration::from_secs(6)));
        assert!(!h.stalled());
        assert_eq!((h.drops, h.longest), (1, Duration::from_secs(6)));
        // 正常收发不算中断
        assert_eq!(h.heard(t0 + Duration::from_millis(6250)), None);
        assert_eq!(h.drops, 1);
    }

    #[test]
    fn long_gap_jumps_straight_to_stalled() {
        // 定时器被推迟时，一次检查可能直接跨过两个阈值
        let t0 = Instant::now();
        let mut h = LinkHealth::default();
        h.heard(t0);
        assert_eq!(
            h.check(t0 + Duration::from_secs(4)),
            Some(LinkEvent::Stalled)
        );
        assert_eq!(h.check(t0 + Duration::from_secs(5)), None);
        assert_eq!(
            h.heard(t0 + Duration::from_secs(5)),
            Some(Duration::from_secs(5))
        );
    }
}
