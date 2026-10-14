//! 剪贴板读写与传输。
//!
//! 切屏时同步：主机切到某台设备时把主机剪贴板发过去；被控端在光标离开时
//! 把自己的剪贴板发回主机。内容没变就不发。

use std::{borrow::Cow, io::Cursor, time::Duration};

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, RecvStream};
use roam_proto::{ClipboardData, ClipboardPolicy, encode_side};
use serde::de::DeserializeOwned;

/// 读取剪贴板。有文本时只取文本（和富文本），没有文本时才取图片。阻塞调用。
pub fn read(policy: &ClipboardPolicy) -> Option<ClipboardData> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let text = cb.get_text().ok().filter(|t| !t.is_empty());
    let data = if text.is_some() {
        ClipboardData {
            html: cb.get().html().ok().filter(|h| !h.is_empty()),
            text,
            png: None,
        }
    } else if policy.images {
        let image = cb.get_image().ok()?;
        ClipboardData {
            text: None,
            html: None,
            png: Some(encode_png(image.width as u32, image.height as u32, &image.bytes).ok()?),
        }
    } else {
        return None;
    };
    if data.is_empty() || data.size() as u64 > policy.max_bytes {
        return None;
    }
    Some(data)
}

/// 写入剪贴板。阻塞调用。
pub fn write(data: &ClipboardData) -> Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    if let Some(png) = &data.png {
        let (width, height, rgba) = decode_png(png)?;
        cb.set_image(arboard::ImageData {
            width,
            height,
            bytes: Cow::Owned(rgba),
        })?;
    } else if let Some(html) = &data.html {
        cb.set_html(html.as_str(), data.text.as_deref())?;
    } else if let Some(text) = &data.text {
        cb.set_text(text)?;
    }
    Ok(())
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header()?.write_image_data(rgba)?;
    Ok(out)
}

fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("图片太大")?];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width as usize, info.height as usize);
    // 统一转成 RGBA
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("不支持的 PNG 格式"),
    };
    Ok((w, h, rgba))
}

/// 通过单向流发送一块数据（剪贴板、策略等）。
pub async fn send_side(conn: &Connection, kind: u8, body: &impl serde::Serialize) -> Result<()> {
    let frame = encode_side(kind, body)?;
    let mut stream = conn.open_uni().await?;
    // 大图片在慢速网络上可能要几秒，给足时间但不无限等待
    tokio::time::timeout(Duration::from_secs(60), stream.write_all(&frame))
        .await
        .context("发送剪贴板超时")??;
    stream.finish()?;
    Ok(())
}

/// 读取单向流：返回类型和原始内容。
pub async fn read_side(stream: &mut RecvStream, max_bytes: u64) -> Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    stream.read_exact(&mut head).await?;
    let len = u32::from_be_bytes(head[1..5].try_into().unwrap()) as u64;
    if len > max_bytes {
        bail!("数据过大（{len} 字节），已忽略");
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await?;
    Ok((head[0], body))
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    Ok(postcard::from_bytes(body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip() {
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect();
        let png = encode_png(3, 2, &rgba).unwrap();
        let (w, h, back) = decode_png(&png).unwrap();
        assert_eq!((w, h), (3, 2));
        assert_eq!(back, rgba);
    }
}
