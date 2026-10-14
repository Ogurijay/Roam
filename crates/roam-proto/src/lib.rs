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
pub const ALPN: &[u8] = b"roam/2";

/// 更新通道的 ALPN。永远保持兼容：即使主协议升级了，主机也能用它把新版本推给旧客户端。
pub const UPDATE_ALPN: &[u8] = b"roam/update/1";

/// 本程序版本（工作区版本号）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    /// 程序版本，用于提示和同步更新。
    pub version: String,
    /// 被控端当前的连接码（地址可能变化，主机据此更新保存的连接码）；主机发送时为空。
    pub ticket: String,
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
    /// 锁定被控端的电脑（系统不认程序注入的 Win+L，所以单独发一条消息）。
    LockScreen,
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
            version: VERSION.into(),
            ticket: String::new(),
        });
        let frame = msg.encode_frame().unwrap();
        let len = u16::from_be_bytes([frame[0], frame[1]]) as usize;
        assert_eq!(len, frame.len() - 2);
        assert_eq!(StreamMsg::decode_body(&frame[2..]).unwrap(), msg);
    }
}

// ---------------------------------------------------------------- 单向流

/// 单向流的第一个字节表示内容类型，之后是 u32 大端长度和 postcard 编码的内容。
/// 大块数据（剪贴板图片等）走单向流，不会阻塞键鼠通道。
pub mod side {
    pub const CLIPBOARD: u8 = 1;
    pub const CLIPBOARD_POLICY: u8 = 2;
    /// 文件清单：[`FileOffer`](super::FileOffer)。
    pub const FILE_OFFER: u8 = 3;
    /// 单个文件：头部是 [`FileDataHeader`](super::FileDataHeader)，之后直到流结束都是文件内容。
    pub const FILE_DATA: u8 = 4;
}

/// 剪贴板内容。有文本时只同步文本（和富文本），没有文本时才同步图片。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipboardData {
    pub text: Option<String>,
    pub html: Option<String>,
    /// PNG 编码的图片。
    pub png: Option<Vec<u8>>,
}

impl ClipboardData {
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.html.is_none() && self.png.is_none()
    }

    pub fn size(&self) -> usize {
        self.text.as_ref().map_or(0, String::len)
            + self.html.as_ref().map_or(0, String::len)
            + self.png.as_ref().map_or(0, Vec::len)
    }

    /// 内容摘要，用于判断剪贴板是否变化（只在本进程内比较）。
    pub fn digest(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

/// 主机告诉被控端：要不要把剪贴板发过来、发什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClipboardPolicy {
    pub enabled: bool,
    pub images: bool,
    pub max_bytes: u64,
    /// 复制文件后切屏时传输文件。
    pub files: bool,
    /// 单次文件传输的总大小上限。
    pub max_file_bytes: u64,
}

/// 一次文件传输的清单（复制的所有文件和文件夹）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOffer {
    pub transfer_id: u64,
    pub entries: Vec<FileEntry>,
}

impl FileOffer {
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    pub fn file_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_dir).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对路径，用 `/` 分隔；第一段是复制时选中的文件或文件夹名。
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDataHeader {
    pub transfer_id: u64,
    /// 在 [`FileOffer::entries`] 中的下标。
    pub index: u32,
}

// ---------------------------------------------------------------- 更新通道

/// 主机推送新版本：先发这个头部，之后是完整的程序文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateOffer {
    pub version: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateReply {
    /// 已校验并替换，马上重启。
    Installed,
    Rejected(String),
}

pub fn encode_side(kind: u8, body: &impl Serialize) -> Result<Vec<u8>, Error> {
    let body = postcard::to_stdvec(body)?;
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(kind);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod side_tests {
    use super::*;

    #[test]
    fn side_frame_layout() {
        let data = ClipboardData {
            text: Some("你好".into()),
            html: None,
            png: None,
        };
        let frame = encode_side(side::CLIPBOARD, &data).unwrap();
        assert_eq!(frame[0], side::CLIPBOARD);
        let len = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 5);
        let back: ClipboardData = postcard::from_bytes(&frame[5..]).unwrap();
        assert_eq!(back, data);
        assert_eq!(back.digest(), data.digest());
    }
}
