//! 连接相关的公共逻辑：端点、帧读写。

use std::time::Duration;

use anyhow::{Context, Result};
use iroh::{
    Endpoint,
    endpoint::{RecvStream, SendStream, presets},
};
use roam_core::config::Store;
use roam_proto::{ALPN, StreamMsg};

/// 用本机固定密钥绑定端点，并尽量等到连上中继（用于跨网打洞）。
pub async fn bind(store: &Store) -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(store.secret_key()?)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("绑定网络端点失败")?;
    if tokio::time::timeout(Duration::from_secs(8), endpoint.online())
        .await
        .is_err()
    {
        eprintln!("⚠ 未能连上中继服务器：跨网络连接可能失败，局域网不受影响。");
    }
    Ok(endpoint)
}

pub async fn read_msg(recv: &mut RecvStream) -> Result<StreamMsg> {
    let mut len = [0u8; 2];
    recv.read_exact(&mut len).await?;
    let mut body = vec![0u8; u16::from_be_bytes(len) as usize];
    recv.read_exact(&mut body).await?;
    Ok(StreamMsg::decode_body(&body)?)
}

pub async fn write_msg(send: &mut SendStream, msg: &StreamMsg) -> Result<()> {
    send.write_all(&msg.encode_frame()?).await?;
    Ok(())
}
