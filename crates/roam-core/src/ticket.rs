//! 连接码：把 [`EndpointAddr`]（设备公钥 + 直连地址 + 中继）编码成一行可复制的文本。

use anyhow::{Context, Result, bail};
use data_encoding::BASE32_NOPAD;
use iroh::EndpointAddr;

const PREFIX: &str = "roam1";

pub fn encode(addr: &EndpointAddr) -> Result<String> {
    let bytes = postcard::to_stdvec(addr)?;
    Ok(format!(
        "{PREFIX}{}",
        BASE32_NOPAD.encode(&bytes).to_lowercase()
    ))
}

pub fn decode(ticket: &str) -> Result<EndpointAddr> {
    let Some(body) = ticket.trim().strip_prefix(PREFIX) else {
        bail!("连接码格式不对，应以 {PREFIX} 开头");
    };
    let bytes = BASE32_NOPAD
        .decode(body.to_uppercase().as_bytes())
        .context("连接码解码失败")?;
    Ok(postcard::from_bytes(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = iroh::SecretKey::generate();
        let addr =
            EndpointAddr::new(key.public()).with_ip_addr("192.168.1.2:4433".parse().unwrap());
        assert_eq!(decode(&encode(&addr).unwrap()).unwrap(), addr);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("hello").is_err());
        assert!(decode("roam1!!!").is_err());
    }
}
