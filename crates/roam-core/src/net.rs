//! 连接路径分类：局域网 / 公网直连 / 中继。

use std::net::{IpAddr, SocketAddr};

use iroh::{TransportAddr, endpoint::Connection};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Lan,
    PublicV4,
    PublicV6,
    Relay,
    Unknown,
}

impl PathKind {
    pub fn label(self) -> &'static str {
        match self {
            PathKind::Lan => "局域网直连",
            PathKind::PublicV4 => "公网直连(IPv4)",
            PathKind::PublicV6 => "公网直连(IPv6)",
            PathKind::Relay => "中继",
            PathKind::Unknown => "未知",
        }
    }

    pub fn is_direct(self) -> bool {
        matches!(
            self,
            PathKind::Lan | PathKind::PublicV4 | PathKind::PublicV6
        )
    }
}

pub fn classify(addr: &TransportAddr) -> PathKind {
    match addr {
        TransportAddr::Relay(_) => PathKind::Relay,
        TransportAddr::Ip(sa) => classify_ip(sa),
        _ => PathKind::Unknown,
    }
}

pub fn classify_ip(sa: &SocketAddr) -> PathKind {
    match sa.ip() {
        IpAddr::V4(ip) if ip.is_private() || ip.is_loopback() || ip.is_link_local() => {
            PathKind::Lan
        }
        // 100.64.0.0/10：Tailscale 等组网工具的虚拟地址，按局域网看待
        IpAddr::V4(ip) if ip.octets()[0] == 100 && (ip.octets()[1] & 0xC0) == 64 => PathKind::Lan,
        // 198.18.0.0/15：Clash / Surge 等代理 TUN 模式的虚拟网卡，不是真正的公网地址
        IpAddr::V4(ip) if ip.octets()[0] == 198 && (ip.octets()[1] & 0xFE) == 18 => PathKind::Lan,
        IpAddr::V4(_) => PathKind::PublicV4,
        IpAddr::V6(ip)
            if ip.is_loopback() || ip.is_unicast_link_local() || ip.is_unique_local() =>
        {
            PathKind::Lan
        }
        IpAddr::V6(_) => PathKind::PublicV6,
    }
}

/// 当前被选中用于传输数据的路径：类型、地址、RTT。
pub fn selected_path(conn: &Connection) -> Option<(PathKind, String, std::time::Duration)> {
    conn.paths().iter().find(|p| p.is_selected()).map(|p| {
        let addr = p.remote_addr();
        let shown = match addr {
            TransportAddr::Ip(sa) => sa.to_string(),
            other => format!("{other:?}"),
        };
        (classify(addr), shown, p.rtt())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_addresses() {
        let k = |s: &str| classify_ip(&s.parse().unwrap());
        assert_eq!(k("192.168.1.2:1"), PathKind::Lan);
        assert_eq!(k("100.101.102.103:1"), PathKind::Lan);
        assert_eq!(k("203.0.113.7:1"), PathKind::PublicV4);
        assert_eq!(k("198.18.0.1:1"), PathKind::Lan);
        assert_eq!(k("[fe80::1]:1"), PathKind::Lan);
        assert_eq!(k("[240e::1]:1"), PathKind::PublicV6);
    }
}
