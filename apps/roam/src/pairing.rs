//! 配对校验码：主机与被控端各自根据双方 ID 算出同一个 6 位数字，
//! 用户核对两边一致后再允许，避免连错设备。

pub fn verification_code(host_id: &str, client_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"roam-pairing-v1");
    hasher.update(host_id.as_bytes());
    hasher.update(client_id.as_bytes());
    let bytes = hasher.finalize();
    let n = u32::from_le_bytes(bytes.as_bytes()[..4].try_into().unwrap()) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_order_sensitive() {
        let a = verification_code("host", "client");
        assert_eq!(a, verification_code("host", "client"));
        assert_ne!(a, verification_code("client", "host"));
        assert_eq!(a.len(), 7);
    }
}
