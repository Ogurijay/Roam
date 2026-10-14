//! Roam 线协议。
//!
//! 一条 QUIC 连接上分多个通道（见 `docs/design.md` §3）：
//! - [`Datagram`]：不可靠数据报，用于鼠标移动和测量，丢了就丢了，只关心最新值。
//! - [`StreamMsg`]：可靠有序流，用于按键、点击、控制信令。
//!
//! 所有消息用 postcard 编码。按键一律传 USB HID Usage（物理位置），不传字符。
//! 坐标一律是被控设备自己的"逻辑点"（与 [`DisplayInfo::rect`] 同一坐标系）。

use serde::{Deserialize, Serialize};

/// ALPN，协议不兼容时递增版本号。
pub const ALPN: &[u8] = b"roam/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

/// 逻辑点坐标下的矩形。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// 一块显示器。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// 逻辑点坐标（macOS 的 point；Windows 为像素 / 缩放比例）。
    pub rect: Rect,
    /// 缩放比例，物理像素 = 逻辑点 × scale。
    pub scale: f64,
}

/// 连接建立后双方各发一次。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    pub os: Os,
    pub displays: Vec<DisplayInfo>,
}

/// 鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// 走数据报通道的消息：可以丢，不能阻塞。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Datagram {
    /// 绝对位置（A 模式）。`seq` 用于丢弃乱序到达的旧包。
    MouseAbs {
        seq: u32,
        x: f32,
        y: f32,
    },
    /// 相对位移（B 模式、游戏模式）。
    MouseRel {
        dx: i32,
        dy: i32,
    },
    /// 测量往返时延；对端原样回 [`Datagram::Pong`]。
    Ping {
        seq: u64,
        sent_us: u64,
    },
    Pong {
        seq: u64,
        sent_us: u64,
    },
}

/// 走可靠流的消息：不能丢，必须有序。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamMsg {
    Hello(Hello),
    /// 光标进入本设备，位置为逻辑点。
    Enter {
        x: f32,
        y: f32,
    },
    /// 光标离开本设备：被控端应释放所有按键和按钮。
    Leave,
    /// 可靠地移动到某处（点击前发送，保证点击位置正确）。
    MouseTo {
        x: f32,
        y: f32,
    },
    /// USB HID Usage Page 0x07 的 usage id。
    Key {
        usage: u16,
        down: bool,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// 行滚动，120 = 一格（与 Windows WHEEL_DELTA 一致），正值为向上 / 向右。
    WheelLines {
        dx: i32,
        dy: i32,
    },
    /// 像素滚动（macOS 触控板等高精度设备），正值为向上 / 向右。
    WheelPixels {
        dx: f32,
        dy: f32,
    },
    /// 控制端当前认为按下的全部按键，被控端据此纠正状态，防止卡键。
    KeyState {
        pressed: Vec<u16>,
    },
    /// 释放所有按键和鼠标按钮。
    ReleaseAll,
    Ping {
        seq: u64,
        sent_us: u64,
    },
    Pong {
        seq: u64,
        sent_us: u64,
    },
}

/// 消息编解码错误。
pub type Error = postcard::Error;

impl Datagram {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        postcard::to_stdvec(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        postcard::from_bytes(bytes)
    }
}

impl StreamMsg {
    /// 编码为带长度前缀的帧（u16 大端），便于在流上切分。
    pub fn encode_frame(&self) -> Result<Vec<u8>, Error> {
        let body = postcard::to_stdvec(self)?;
        let len = u16::try_from(body.len()).map_err(|_| Error::SerializeBufferFull)?;
        let mut frame = Vec::with_capacity(2 + body.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// 解码一帧的消息体（不含长度前缀）。
    pub fn decode_body(body: &[u8]) -> Result<Self, Error> {
        postcard::from_bytes(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_roundtrip() {
        let msgs = [
            Datagram::MouseRel { dx: -3, dy: 7 },
            Datagram::MouseAbs {
                seq: 9,
                x: 1920.5,
                y: 1080.0,
            },
            Datagram::Ping {
                seq: 1,
                sent_us: 123_456,
            },
        ];
        for msg in msgs {
            assert_eq!(Datagram::decode(&msg.encode().unwrap()).unwrap(), msg);
        }
    }

    #[test]
    fn mouse_abs_is_small() {
        let bytes = Datagram::MouseAbs {
            seq: 70_000,
            x: 1234.5,
            y: 987.25,
        }
        .encode()
        .unwrap();
        assert!(bytes.len() <= 12, "len = {}", bytes.len());
    }

    #[test]
    fn stream_frame_roundtrip() {
        let msg = StreamMsg::Hello(Hello {
            name: "mac".into(),
            os: Os::MacOs,
            displays: vec![DisplayInfo {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1512.0,
                    h: 982.0,
                },
                scale: 2.0,
            }],
        });
        let frame = msg.encode_frame().unwrap();
        let len = u16::from_be_bytes([frame[0], frame[1]]) as usize;
        assert_eq!(len, frame.len() - 2);
        assert_eq!(StreamMsg::decode_body(&frame[2..]).unwrap(), msg);
    }
}
