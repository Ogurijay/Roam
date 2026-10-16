//! 文件传输：复制文件后切屏，文件就传到对方的「下载/Roam」文件夹，
//! 传完后对方剪贴板里就是这些文件，直接粘贴即可。

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, ReadError, RecvStream, SendStream, VarInt};
use roam_proto::{
    DirectFileOffer, FileCommand, FileDataHeader, FileEntry, FileOffer, FileTarget, encode_side,
    side,
};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

const CHUNK: usize = 256 * 1024;

pub fn supports_direct(version: &str) -> bool {
    semver::Version::parse(version).is_ok_and(|v| v >= semver::Version::new(0, 3, 3))
}

/// 有文件读不了（例如 macOS 的隐私保护）。重试也没用，等剪贴板换了内容再发。
#[derive(Debug)]
pub struct Unreadable(String);

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreadable {}

/// 发送前确认每个文件都能打开，免得对方收到一半才失败。
fn check_readable(entries: &[FileEntry], sources: &[PathBuf]) -> Result<()> {
    for (entry, src) in entries.iter().zip(sources) {
        if entry.is_dir || entry.size == 0 {
            continue;
        }
        if let Err(err) = std::fs::File::open(src) {
            // 查文件大小不受限制，读内容会被拦下
            let hint = if cfg!(target_os = "macos")
                && err.kind() == std::io::ErrorKind::PermissionDenied
            {
                "。macOS 不允许 Roam 读取这个位置：在「系统设置 → 隐私与安全性 → 完全磁盘访问权限」里加入 Roam（从终端运行时加入终端），然后重试"
            } else {
                ""
            };
            return Err(Unreadable(format!("无法读取「{}」: {err}{hint}", src.display())).into());
        }
    }
    Ok(())
}

/// 一次传输的进度，显示在页面和托盘里。
#[derive(Debug, Clone, Serialize)]
pub struct TransferInfo {
    pub id: u64,
    /// "send" / "receive"
    pub direction: &'static str,
    pub peer: String,
    /// 第一个文件或文件夹的名字，多个时显示"等 N 项"。
    pub title: String,
    pub files: usize,
    pub total: u64,
    pub done: u64,
    /// "running" / "done" / "failed"
    pub state: &'static str,
    pub error: Option<String>,
    /// 接收完成后文件所在的位置。
    pub location: Option<String>,
}

/// 所有传输的进度记录（最近的在前，最多保留 20 条）。
#[derive(Clone, Default)]
pub struct Transfers {
    list: Arc<Mutex<Vec<TransferInfo>>>,
    receipts: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Result<(), String>>>>>,
    queries: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Vec<FileTarget>>>>>,
}

impl Transfers {
    pub fn command(&self, command: FileCommand) {
        match command {
            FileCommand::Result { id, error } => {
                if let Some(tx) = self.receipts.lock().unwrap().remove(&id) {
                    let _ = tx.send(error.map_or(Ok(()), Err));
                }
            }
            FileCommand::Targets { request, targets } => {
                if let Some(tx) = self.queries.lock().unwrap().remove(&request) {
                    let _ = tx.send(targets);
                }
            }
            FileCommand::List { .. } => {}
        }
    }

    pub async fn targets(&self, conn: &Connection) -> Result<Vec<FileTarget>> {
        let request = new_id();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.queries.lock().unwrap().insert(request, tx);
        let result = async {
            crate::clipboard::send_side(conn, side::FILE_COMMAND, &FileCommand::List { request })
                .await?;
            Ok(tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .context("主机没有响应，请升级主机 Roam 后重试")??)
        }
        .await;
        self.queries.lock().unwrap().remove(&request);
        result
    }

    pub fn list(&self) -> Vec<TransferInfo> {
        self.list.lock().unwrap().clone()
    }

    /// 正在进行的传输（托盘状态用）。
    pub fn active(&self) -> Option<TransferInfo> {
        self.list
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.state == "running")
            .cloned()
    }

    fn start(&self, info: TransferInfo) {
        let mut list = self.list.lock().unwrap();
        list.insert(0, info);
        list.truncate(20);
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut TransferInfo)) {
        if let Some(t) = self.list.lock().unwrap().iter_mut().find(|t| t.id == id) {
            f(t);
        }
    }
}

fn title_of(entries: &[FileEntry]) -> String {
    let tops: Vec<&str> = entries
        .iter()
        .map(|e| e.path.split('/').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .into_iter()
        .fold(Vec::new(), |mut acc, t| {
            if !acc.contains(&t) {
                acc.push(t);
            }
            acc
        });
    match tops.as_slice() {
        [] => "（空）".into(),
        [one] => one.to_string(),
        [first, rest @ ..] => format!("{first} 等 {} 项", rest.len() + 1),
    }
}

/// 剪贴板里文件列表的摘要：路径 + 大小 + 修改时间，用于判断是否已经同步过。
pub fn digest(paths: &[PathBuf]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "files".hash(&mut h);
    for p in paths {
        p.hash(&mut h);
        if let Ok(meta) = std::fs::metadata(p) {
            meta.len().hash(&mut h);
            meta.modified().ok().hash(&mut h);
        }
    }
    h.finish()
}

pub(crate) fn new_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (COUNTER.fetch_add(1, Ordering::Relaxed) << 48)
}

// ---------------------------------------------------------------- 发送

/// 宣告用的概况：顶层名字、文件数、总大小。会遍历文件夹，阻塞调用。
pub fn summarize(paths: &[PathBuf]) -> Result<(Vec<String>, u32, u64)> {
    let (entries, _) = collect(paths)?;
    let names = paths
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let files = entries.iter().filter(|e| !e.is_dir).count() as u32;
    let total = entries.iter().map(|e| e.size).sum();
    Ok((names, files, total))
}

/// 把选中的文件和文件夹展开成清单，同时记下每个条目的源路径。
fn collect(paths: &[PathBuf]) -> Result<(Vec<FileEntry>, Vec<PathBuf>)> {
    fn walk(
        src: &Path,
        rel: String,
        entries: &mut Vec<FileEntry>,
        sources: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let meta = std::fs::symlink_metadata(src)?;
        if meta.file_type().is_symlink() {
            return Ok(()); // 不跟随符号链接，避免循环和意外泄露
        }
        if meta.is_dir() {
            entries.push(FileEntry {
                path: rel.clone(),
                size: 0,
                is_dir: true,
            });
            sources.push(src.to_path_buf());
            let mut children: Vec<_> = std::fs::read_dir(src)?.collect::<std::io::Result<_>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let name = child.file_name().to_string_lossy().into_owned();
                walk(&child.path(), format!("{rel}/{name}"), entries, sources)?;
            }
        } else if meta.is_file() {
            entries.push(FileEntry {
                path: rel,
                size: meta.len(),
                is_dir: false,
            });
            sources.push(src.to_path_buf());
        } else {
            bail!("不支持的文件类型：{}", src.display());
        }
        Ok(())
    }

    let mut entries = Vec::new();
    let mut sources = Vec::new();
    let mut names = HashSet::new();
    for p in paths {
        let name = p
            .file_name()
            .context("无效的文件路径")?
            .to_string_lossy()
            .into_owned();
        anyhow::ensure!(
            names.insert(name.clone()),
            "选中的项目重名：{name}，请分开发送"
        );
        walk(p, name, &mut entries, &mut sources)?;
    }
    anyhow::ensure!(
        !entries.is_empty(),
        "没有可发送的普通文件或文件夹（不跟随符号链接）"
    );
    Ok((entries, sources))
}

/// 发送复制的文件。`peer` 只用于显示。
pub async fn send(
    conn: &Connection,
    paths: Vec<PathBuf>,
    peer: &str,
    max_bytes: u64,
    transfers: &Transfers,
) -> Result<()> {
    send_inner(conn, paths, peer, max_bytes, transfers, None).await
}

pub async fn send_direct(
    conn: &Connection,
    paths: Vec<PathBuf>,
    peer: &str,
    max_bytes: u64,
    transfers: &Transfers,
    target: Option<String>,
) -> Result<()> {
    send_inner(conn, paths, peer, max_bytes, transfers, Some(target)).await
}

async fn send_inner(
    conn: &Connection,
    paths: Vec<PathBuf>,
    peer: &str,
    max_bytes: u64,
    transfers: &Transfers,
    direct: Option<Option<String>>,
) -> Result<()> {
    let (entries, sources) = tokio::task::spawn_blocking(move || {
        let (entries, sources) = collect(&paths)?;
        check_readable(&entries, &sources)?;
        anyhow::Ok((entries, sources))
    })
    .await??;
    let offer = FileOffer {
        transfer_id: new_id(),
        entries,
    };
    let total = offer.total_bytes();
    if total > max_bytes {
        bail!(
            "文件总大小 {} 超过上限 {}，未发送",
            human_bytes(total),
            human_bytes(max_bytes)
        );
    }
    let id = offer.transfer_id;
    let receipt = direct.as_ref().map(|_| {
        let (tx, rx) = tokio::sync::oneshot::channel();
        transfers.receipts.lock().unwrap().insert(id, tx);
        rx
    });
    transfers.start(TransferInfo {
        id,
        direction: "send",
        peer: peer.to_string(),
        title: title_of(&offer.entries),
        files: offer.file_count(),
        total,
        done: 0,
        state: "running",
        error: None,
        location: None,
    });

    let result = async {
        match direct {
            Some(target) => {
                crate::clipboard::send_side(
                    conn,
                    side::DIRECT_FILE_OFFER,
                    &DirectFileOffer {
                        offer: offer.clone(),
                        target,
                    },
                )
                .await?
            }
            None => crate::clipboard::send_side(conn, side::FILE_OFFER, &offer).await?,
        }
        for (index, (entry, src)) in offer.entries.iter().zip(&sources).enumerate() {
            if entry.is_dir || entry.size == 0 {
                continue;
            }
            let mut stream = conn.open_uni().await?;
            let header = FileDataHeader {
                transfer_id: id,
                index: index as u32,
            };
            let sent = send_one(&mut stream, &header, entry, src, transfers).await;
            if sent.is_err() {
                // 显式中止：只是丢掉的话流会被当作正常结束，对方只会看到文件不完整
                let _ = stream.reset(VarInt::from_u32(1));
            }
            sent?;
        }
        if let Some(receipt) = receipt {
            tokio::select! {
                result = tokio::time::timeout(std::time::Duration::from_secs(30 * 60), receipt) => {
                    result.context("等待接收端保存文件超时")??.map_err(anyhow::Error::msg)?;
                }
                _ = conn.closed() => bail!("接收电脑已断开连接，未确认文件保存完成"),
            }
        }
        anyhow::Ok(())
    }
    .await;

    transfers.receipts.lock().unwrap().remove(&id);
    transfers.update(id, |t| match &result {
        Ok(()) => t.state = "done",
        Err(err) => {
            t.state = "failed";
            t.error = Some(format!("{err:#}"));
        }
    });
    result
}

/// 在一条单向流上发送一个文件：头部 + 内容。
async fn send_one(
    stream: &mut SendStream,
    header: &FileDataHeader,
    entry: &FileEntry,
    src: &Path,
    transfers: &Transfers,
) -> Result<()> {
    stream
        .write_all(&encode_side(side::FILE_DATA, header)?)
        .await?;
    let mut file = tokio::fs::File::open(src)
        .await
        .with_context(|| format!("无法读取 {}", src.display()))?;
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut file, &mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n]).await?;
        sent += n as u64;
        transfers.update(header.transfer_id, |t| t.done += n as u64);
    }
    if sent != entry.size {
        bail!("{} 在发送过程中被修改了", entry.path);
    }
    stream.finish()?;
    Ok(())
}

// ---------------------------------------------------------------- 接收

struct Incoming {
    offer: FileOffer,
    /// 每个条目在本机的目标路径。
    targets: Vec<PathBuf>,
    /// 复制时选中的顶层文件 / 文件夹（写进剪贴板的就是它们）。
    tops: Vec<PathBuf>,
    pending: HashSet<u32>,
}

/// 接收方：处理清单和文件内容，全部完成后把文件放进剪贴板。
#[derive(Clone)]
pub struct Receiver {
    base: PathBuf,
    transfers: Transfers,
    incoming: Arc<Mutex<HashMap<u64, Incoming>>>,
    /// 全部完成时回调：顶层路径。
    on_complete: Arc<dyn Fn(u64, Vec<PathBuf>) + Send + Sync>,
}

/// 默认接收目录：下载/Roam。
pub fn default_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("Roam")
}

impl Receiver {
    pub fn new(
        transfers: Transfers,
        base: PathBuf,
        on_complete: impl Fn(u64, Vec<PathBuf>) + Send + Sync + 'static,
    ) -> Self {
        Self {
            base,
            transfers,
            incoming: Arc::new(Mutex::new(HashMap::new())),
            on_complete: Arc::new(on_complete),
        }
    }

    pub fn on_offer(&self, offer: FileOffer, peer: &str, max_bytes: u64) -> Result<()> {
        self.on_offer_in(offer, peer, max_bytes, &self.base)
    }

    pub fn on_offer_in(
        &self,
        offer: FileOffer,
        peer: &str,
        max_bytes: u64,
        base: &Path,
    ) -> Result<()> {
        let total = offer.total_bytes();
        if total > max_bytes {
            bail!("对方发来的文件 {} 超过上限，已拒收", human_bytes(total));
        }
        anyhow::ensure!(!offer.entries.is_empty(), "文件清单为空");
        anyhow::ensure!(
            !self
                .incoming
                .lock()
                .unwrap()
                .contains_key(&offer.transfer_id),
            "重复的传输编号"
        );
        std::fs::create_dir_all(base)?;

        // 顶层名字和本地已有文件重名时自动改名：报告.pdf → 报告 (1).pdf
        let mut renamed: HashMap<String, String> = HashMap::new();
        let mut tops = Vec::new();
        for entry in &offer.entries {
            let top = entry.path.split('/').next().unwrap_or_default().to_string();
            anyhow::ensure!(!top.is_empty(), "非法的文件路径");
            if renamed.contains_key(&top) {
                continue;
            }
            let sanitized = sanitize(&top);
            anyhow::ensure!(!sanitized.is_empty(), "非法的文件名");
            // 先独占顶层目标，防止两个并行传输选中同一个名字后互相覆盖。
            let unique = loop {
                let candidate = unique_name(base, &sanitized);
                let path = base.join(&candidate);
                let reserved = if entry.is_dir {
                    std::fs::create_dir(&path)
                } else {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map(|_| ())
                };
                match reserved {
                    Ok(()) => break candidate,
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(err) => return Err(err.into()),
                }
            };
            tops.push(base.join(&unique));
            renamed.insert(top, unique);
        }

        let mut targets = Vec::with_capacity(offer.entries.len());
        let mut pending = HashSet::new();
        let mut used_paths = HashSet::new();
        for (index, entry) in offer.entries.iter().enumerate() {
            let mut parts = entry.path.split('/');
            let top = parts.next().unwrap_or_default();
            let mut path = base.join(&renamed[top]);
            for part in parts {
                let part = sanitize(part);
                if part.is_empty() || part == "." || part == ".." {
                    bail!("非法路径：{}", entry.path);
                }
                path.push(part);
            }
            anyhow::ensure!(
                used_paths.insert(path.to_string_lossy().to_lowercase()),
                "清单中的文件名重复：{}",
                entry.path
            );
            if entry.is_dir {
                std::fs::create_dir_all(&path)?;
            } else {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if path != base.join(&renamed[top]) {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)?;
                }
                if entry.size != 0 {
                    pending.insert(index as u32);
                }
            }
            targets.push(path);
        }

        let id = offer.transfer_id;
        self.transfers.start(TransferInfo {
            id,
            direction: "receive",
            peer: peer.to_string(),
            title: title_of(&offer.entries),
            files: offer.file_count(),
            total,
            done: 0,
            state: "running",
            error: None,
            location: Some(base.display().to_string()),
        });
        let done_now = pending.is_empty();
        self.incoming.lock().unwrap().insert(
            id,
            Incoming {
                offer,
                targets,
                tops: tops.clone(),
                pending,
            },
        );
        if done_now {
            self.finish(id);
        }
        Ok(())
    }

    /// 接收一个文件的内容（单向流里头部之后的部分）。
    pub async fn on_data(&self, header: FileDataHeader, stream: &mut RecvStream) -> Result<()> {
        let id = header.transfer_id;
        let (target, size) = {
            let incoming = self.incoming.lock().unwrap();
            let t = incoming.get(&id).context("未知的传输")?;
            let entry = t
                .offer
                .entries
                .get(header.index as usize)
                .context("非法的文件序号")?;
            (t.targets[header.index as usize].clone(), entry.size)
        };

        let part = target.with_file_name(format!(".roam-{id:016x}-{:08x}.part", header.index));
        let mut created = false;
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part)
                .await?;
            created = true;
            let mut received = 0u64;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(Some(n)) => n,
                    Ok(None) => break,
                    Err(ReadError::Reset(_)) => {
                        bail!("对方中止了发送（{received}/{size} 字节），原因见对方的日志")
                    }
                    Err(err) => return Err(err.into()),
                };
                received += n as u64;
                if received > size {
                    bail!("收到的数据比声明的多");
                }
                file.write_all(&buf[..n]).await?;
                self.transfers.update(id, |t| t.done += n as u64);
            }
            file.flush().await?;
            drop(file);
            if received != size {
                bail!("传输中断（{received}/{size} 字节）");
            }
            tokio::fs::rename(&part, &target).await?;
            anyhow::Ok(())
        }
        .await;

        if let Err(err) = result {
            if created {
                let _ = tokio::fs::remove_file(&part).await;
            }
            self.fail(id, &err);
            return Err(err);
        }
        let finished = {
            let mut incoming = self.incoming.lock().unwrap();
            let t = incoming.get_mut(&id).context("未知的传输")?;
            t.pending.remove(&header.index);
            t.pending.is_empty()
        };
        if finished {
            self.finish(id);
        }
        Ok(())
    }

    fn finish(&self, id: u64) {
        let Some(incoming) = self.incoming.lock().unwrap().remove(&id) else {
            return;
        };
        self.transfers.update(id, |t| t.state = "done");
        (self.on_complete)(id, incoming.tops);
    }

    fn fail(&self, id: u64, err: &anyhow::Error) {
        self.incoming.lock().unwrap().remove(&id);
        self.transfers.update(id, |t| {
            t.state = "failed";
            t.error = Some(format!("{err:#}"));
        });
    }
}

/// 读取单向流的头部（类型 + 长度 + 内容），不读后面的文件数据。
pub async fn read_header(stream: &mut RecvStream, max_len: u64) -> Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    stream.read_exact(&mut head).await?;
    let len = u32::from_be_bytes(head[1..5].try_into().unwrap()) as usize;
    if len as u64 > max_len + 1024 {
        bail!("数据过大（{len} 字节），已忽略");
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok((head[0], body))
}

/// 去掉在 Windows / macOS 上不合法的文件名字符。
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim_end_matches(['.', ' '])
        .to_string()
}

fn unique_name(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|candidate| !dir.join(candidate).exists())
        .expect("infinite iterator")
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_walks_directories() {
        let dir = std::env::temp_dir().join(format!("roam-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("项目/子目录")).unwrap();
        std::fs::write(dir.join("项目/a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("项目/子目录/b.bin"), vec![7u8; 1000]).unwrap();
        std::fs::write(dir.join("单独.txt"), b"x").unwrap();

        let (entries, sources) = collect(&[dir.join("项目"), dir.join("单独.txt")]).unwrap();
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "项目",
                "项目/a.txt",
                "项目/子目录",
                "项目/子目录/b.bin",
                "单独.txt"
            ]
        );
        assert_eq!(entries.iter().map(|e| e.size).sum::<u64>(), 1006);
        assert_eq!(sources.len(), entries.len());
        assert_eq!(title_of(&entries), "项目 等 2 项");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn names_are_sanitized_and_unique() {
        assert_eq!(sanitize("a:b?.txt"), "a_b_.txt");
        assert_eq!(sanitize("trailing. "), "trailing");
        let dir = std::env::temp_dir().join(format!("roam-uniq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("报告.pdf"), b"").unwrap();
        std::fs::write(dir.join("报告 (1).pdf"), b"").unwrap();
        assert_eq!(unique_name(&dir, "报告.pdf"), "报告 (2).pdf");
        assert_eq!(unique_name(&dir, "新文件"), "新文件");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn human_readable_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn concurrent_offers_reserve_separate_destinations() {
        let dir = std::env::temp_dir().join(format!("roam-reserve-{}", new_id()));
        let a = Receiver::new(Transfers::default(), dir.clone(), |_, _| {});
        let b = Receiver::new(Transfers::default(), dir.clone(), |_, _| {});
        for (receiver, id) in [(&a, 1), (&b, 2)] {
            receiver
                .on_offer(
                    FileOffer {
                        transfer_id: id,
                        entries: vec![FileEntry {
                            path: "same.txt".into(),
                            size: 1,
                            is_dir: false,
                        }],
                    },
                    "peer",
                    1024,
                )
                .unwrap();
        }
        let left = a.incoming.lock().unwrap()[&1].tops[0].clone();
        let right = b.incoming.lock().unwrap()[&2].tops[0].clone();
        assert_ne!(left, right);
        assert!(left.exists() && right.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
