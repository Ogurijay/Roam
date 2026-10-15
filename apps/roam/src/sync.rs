//! 切屏时的剪贴板同步，主机和被控端共用。
//!
//! 文本和图片直接发过去；文件只宣告，对方按粘贴时再传（见 [`crate::pull`]）。

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use iroh::endpoint::Connection;
use roam_proto::{
    ClipboardData, ClipboardPolicy, FileAnnounce, FileDataHeader, FileOffer, FilePull, side,
};

use crate::{
    clipboard,
    files::{self, Receiver, Transfers, human_bytes},
    pull::{Shelf, Source},
};

/// 本机剪贴板里的内容。
enum Local {
    Files(Vec<PathBuf>),
    Data(ClipboardData),
}

impl Local {
    fn digest(&self) -> u64 {
        match self {
            Local::Files(paths) => files::digest(paths),
            Local::Data(data) => data.digest(),
        }
    }
}

/// 读取本机剪贴板：复制的是文件时优先当作文件（避免只把文件名当文本发过去）。
fn read_local(policy: &ClipboardPolicy) -> Option<Local> {
    let files = clipboard::with_clipboard(|cb| cb.get().file_list().ok())
        .ok()
        .flatten()
        .filter(|list| !list.is_empty());
    match files {
        Some(paths) if policy.files => Some(Local::Files(paths)),
        Some(_) => None,
        None => clipboard::read(policy).map(Local::Data),
    }
}

/// 最近一次同步（收到或发出）的内容摘要，没变就不重复发送。
pub type LastDigest = Arc<Mutex<Option<u64>>>;

/// 把本机剪贴板发给对方：文本和图片直接发，文件只宣告。`peer` 是对方的名字，`me` 是本机的。
pub async fn push(
    conn: &Connection,
    policy: ClipboardPolicy,
    last: &LastDigest,
    peer: &str,
    me: &str,
    shelf: &Shelf,
) -> Result<()> {
    if !policy.enabled {
        return Ok(());
    }
    // 收到别处的文件宣告后本机没复制过别的：最近一次复制在别处，本机剪贴板里是旧内容，不能发
    if shelf.current().is_some() {
        return Ok(());
    }
    let Some(local) = tokio::task::spawn_blocking(move || read_local(&policy)).await? else {
        return Ok(());
    };
    let digest = local.digest();
    {
        // 先记下摘要：大文件传输期间来回切屏不会重复发送
        let mut last = last.lock().unwrap();
        if *last == Some(digest) {
            return Ok(());
        }
        *last = Some(digest);
    }
    match local {
        Local::Data(data) => {
            let size = data.size();
            clipboard::send_side(conn, side::CLIPBOARD, &data).await?;
            tracing::info!("剪贴板 → {peer}（{size} 字节）");
        }
        Local::Files(paths) => {
            let summary = {
                let paths = paths.clone();
                tokio::task::spawn_blocking(move || files::summarize(&paths)).await?
            };
            let (names, files, total) = match summary {
                Ok(s) => s,
                Err(err) => {
                    *last.lock().unwrap() = None;
                    return Err(err);
                }
            };
            let token = shelf.register(Source::Local(paths));
            let announce = FileAnnounce {
                token,
                from: me.to_string(),
                names,
                files,
                total,
            };
            if let Err(err) = send_announce(conn, &announce, peer).await {
                *last.lock().unwrap() = None;
                return Err(err);
            }
        }
    }
    Ok(())
}

/// 宣告复制了文件（不传内容）。
pub async fn send_announce(conn: &Connection, announce: &FileAnnounce, peer: &str) -> Result<()> {
    clipboard::send_side(conn, side::FILE_ANNOUNCE, announce).await?;
    tracing::info!(
        "文件 → {peer}：{}（{} 个文件，{}），对方粘贴时再传",
        announce.names.join("、"),
        announce.files,
        human_bytes(announce.total)
    );
    Ok(())
}

/// 用户在本机按了粘贴：请对方把宣告过的文件发过来。
pub async fn request_files(conn: &Connection, token: u64) -> Result<()> {
    clipboard::send_side(conn, side::FILE_PULL, &FilePull { token }).await
}

/// 文件宣告的一句话说明（日志用）。
pub fn describe(announce: &FileAnnounce) -> String {
    format!(
        "{}（{} 个文件，{}）",
        announce.names.join("、"),
        announce.files,
        human_bytes(announce.total)
    )
}

/// 收到的文件放进剪贴板之后的回调，见 [`Inbound::on_files`]。
pub type OnFiles = Arc<dyn Fn(&[PathBuf], bool) -> bool + Send + Sync>;

/// 接收对方发来的单向流的上下文。
pub struct Inbound {
    pub peer: String,
    /// 当前的同步策略（主机按设置计算，被控端用主机下发的）。
    pub policy: Arc<dyn Fn() -> ClipboardPolicy + Send + Sync>,
    /// 被控端收到主机下发的策略时调用；主机为 None。
    pub on_policy: Option<Arc<dyn Fn(ClipboardPolicy) + Send + Sync>>,
    pub last: LastDigest,
    pub transfers: Transfers,
    /// 剪贴板被对方的文本 / 图片更新后调用（主机用来转发给当前所在的另一台设备）。
    pub on_updated: Arc<dyn Fn() + Send + Sync>,
    /// 对方宣告复制了文件。
    pub on_announce: Arc<dyn Fn(FileAnnounce) + Send + Sync>,
    /// 对方来取本机宣告过的文件。
    pub on_pull: Arc<dyn Fn(u64) + Send + Sync>,
    /// 收到的文件已经放进剪贴板（参数表示是否成功放进去）。返回 true 表示已经处理
    /// （例如自动粘贴了），不用再提醒用户。在普通线程里调用。
    pub on_files: OnFiles,
}

pub async fn receive_loop(conn: Connection, ctx: Inbound) {
    let ctx = Arc::new(ctx);
    let receiver = {
        let ctx = ctx.clone();
        Receiver::new(ctx.transfers.clone(), move |tops: Vec<PathBuf>| {
            *ctx.last.lock().unwrap() = Some(files::digest(&tops));
            let names: Vec<String> = tops
                .iter()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect();
            tracing::info!("文件 ← {}：{}", ctx.peer, names.join("、"));
            let on_files = ctx.on_files.clone();
            std::thread::spawn(move || {
                let ok = clipboard::with_clipboard(|cb| cb.set().file_list(&tops).is_ok())
                    .unwrap_or(false);
                if !on_files(&tops, ok) {
                    notify_files_ready(&names, ok);
                }
            });
        })
    };

    while let Ok(mut stream) = conn.accept_uni().await {
        let policy = (ctx.policy)();
        let (kind, body) =
            match files::read_header(&mut stream, policy.max_bytes.max(1 << 20)).await {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!("接收失败: {err:#}");
                    continue;
                }
            };
        match kind {
            side::CLIPBOARD_POLICY => {
                if let (Some(on_policy), Ok(p)) =
                    (&ctx.on_policy, clipboard::decode::<ClipboardPolicy>(&body))
                {
                    on_policy(p);
                }
            }
            side::CLIPBOARD if policy.enabled => {
                let Ok(data) = clipboard::decode::<ClipboardData>(&body) else {
                    continue;
                };
                let digest = data.digest();
                tracing::info!("剪贴板 ← {}（{} 字节）", ctx.peer, data.size());
                let written = tokio::task::spawn_blocking(move || clipboard::write(&data)).await;
                if matches!(written, Ok(Ok(()))) {
                    *ctx.last.lock().unwrap() = Some(digest);
                    (ctx.on_updated)();
                }
            }
            side::FILE_OFFER if policy.enabled && policy.files => {
                let Ok(offer) = clipboard::decode::<FileOffer>(&body) else {
                    continue;
                };
                if let Err(err) = receiver.on_offer(offer, &ctx.peer, policy.max_file_bytes) {
                    tracing::warn!("拒收文件: {err:#}");
                }
            }
            side::FILE_ANNOUNCE if policy.enabled && policy.files => {
                if let Ok(announce) = clipboard::decode::<FileAnnounce>(&body) {
                    (ctx.on_announce)(announce);
                }
            }
            side::FILE_PULL => {
                if let Ok(pull) = clipboard::decode::<FilePull>(&body) {
                    (ctx.on_pull)(pull.token);
                }
            }
            side::FILE_DATA => {
                let Ok(header) = clipboard::decode::<FileDataHeader>(&body) else {
                    continue;
                };
                let receiver = receiver.clone();
                tokio::spawn(async move {
                    if let Err(err) = receiver.on_data(header, &mut stream).await {
                        tracing::warn!("接收文件失败: {err:#}");
                    }
                });
            }
            _ => {}
        }
    }
}

/// 文件收完后提醒用户可以粘贴了（Windows 弹出系统通知；macOS 在页面和菜单栏里显示）。
fn notify_files_ready(names: &[String], clipboard_ok: bool) {
    #[cfg(windows)]
    {
        use tauri_winrt_notification::Toast;
        let what = match names {
            [one] => format!("「{one}」"),
            [first, rest @ ..] => format!("「{first}」等 {} 项", rest.len() + 1),
            [] => "文件".into(),
        };
        let text = if clipboard_ok {
            "已放进剪贴板，可以直接粘贴".to_string()
        } else {
            format!("已保存到 {}", files::default_dir().display())
        };
        let _ = Toast::new(Toast::POWERSHELL_APP_ID)
            .title(&format!("Roam · 已收到{what}"))
            .text1(&text)
            .show();
    }
    #[cfg(not(windows))]
    let _ = (names, clipboard_ok);
}
