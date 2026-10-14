//! 连接相关的公共逻辑：端点、帧读写。

use std::time::Duration;

use anyhow::{Context, Result};
use iroh::{
    Endpoint,
    endpoint::{RecvStream, SendStream, presets},
};
use roam_core::config::Store;
use roam_proto::{ALPN, StreamMsg, UPDATE_ALPN};

/// 被控端固定使用的 UDP 端口：重启（比如更新后）地址不变，主机能立刻重连，也方便配置防火墙。
pub const CLIENT_PORT: u16 = 47700;

/// 用本机固定密钥绑定端点，并尽量等到连上中继（用于跨网打洞）。
/// `port` 为 Some 时优先绑定这个端口，被占用就退回随机端口。
pub async fn bind(store: &Store, port: Option<u16>) -> Result<Endpoint> {
    let builder = || -> Result<_> {
        Ok(Endpoint::builder(presets::N0)
            .secret_key(store.secret_key()?)
            .alpns(vec![ALPN.to_vec(), UPDATE_ALPN.to_vec()]))
    };
    let fixed = match port {
        Some(port) => {
            let attempt = async {
                builder()?
                    .clear_ip_transports()
                    .bind_addr(format!("0.0.0.0:{port}").as_str())?
                    .bind_addr(format!("[::]:{port}").as_str())?
                    .bind()
                    .await
                    .map_err(anyhow::Error::from)
            };
            match attempt.await {
                Ok(endpoint) => Some(endpoint),
                Err(err) => {
                    tracing::warn!("端口 {port} 不可用，改用随机端口: {err:#}");
                    None
                }
            }
        }
        None => None,
    };
    let endpoint = match fixed {
        Some(endpoint) => endpoint,
        None => builder()?.bind().await.context("绑定网络端点失败")?,
    };
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
