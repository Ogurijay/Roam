//! 应用内更新。
//!
//! - 新版本由 GitHub Actions 构建并发布到 GitHub Releases（见 `.github/workflows/release.yml`）
//! - 主机定期检查，提示后一键更新：主机下载各平台安装包，通过更新通道
//!   （[`UPDATE_ALPN`]，永远兼容）推送给被控端，最后更新自己
//! - 被控端也可自行从 GitHub 检查更新；主机推送仍要求授权，所有下载均校验 SHA256

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, EndpointAddr, endpoint::Connection};
use roam_proto::{Os, UPDATE_ALPN, UpdateOffer, UpdateReply, VERSION};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

/// 发布新版本的仓库。
pub const REPO: &str = "Ogurijay/Roam";

/// 各平台安装包的文件名（与发布脚本保持一致）。
pub fn asset_name(os: Os) -> Option<&'static str> {
    match os {
        Os::MacOs => Some("roam-macos-universal"),
        Os::Windows => Some("roam-windows-x86_64.exe"),
        _ => None,
    }
}

const CHECKSUMS: &str = "SHA256SUMS";

#[derive(Debug, Clone, Serialize)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub url: String,
    #[serde(skip)]
    assets: Vec<Asset>,
}

#[derive(Debug, Clone)]
struct Asset {
    name: String,
    url: String,
    size: u64,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

impl From<GhRelease> for Release {
    fn from(r: GhRelease) -> Self {
        Self {
            version: r.tag_name.trim_start_matches('v').to_string(),
            notes: r.body.unwrap_or_default(),
            url: r.html_url,
            assets: r
                .assets
                .into_iter()
                .map(|a| Asset {
                    name: a.name,
                    url: a.browser_download_url,
                    size: a.size,
                })
                .collect(),
        }
    }
}

/// 页面上显示的更新状态。
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateState {
    pub current: String,
    pub latest: Option<Release>,
    /// 有比当前更新的版本。
    pub available: bool,
    /// 正在进行的步骤说明，例如"下载中 45%"。
    pub phase: Option<String>,
    pub error: Option<String>,
    /// 上次检查的时间（Unix 秒）。
    pub last_checked: Option<u64>,
    /// 每台设备的推送结果。
    pub devices: Vec<(String, String)>,
}

pub struct Updater {
    client: reqwest::Client,
    cache: PathBuf,
    pub state: Mutex<UpdateState>,
}

/// 比较版本号：`a` 是否比 `b` 新。
pub fn is_newer(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

/// 单独判断每台设备，主机已是最新版时仍能补齐旧被控端；不降级更新。
pub fn needs_update(current: Option<&str>, target: &str) -> bool {
    match current.and_then(|v| semver::Version::parse(v).ok()) {
        Some(current) => semver::Version::parse(target).is_ok_and(|target| target > current),
        None => true,
    }
}

impl Updater {
    pub fn new(endpoint: &Endpoint, cache: PathBuf) -> Result<Self> {
        // 复用 iroh 的 TLS 配置（含根证书），不额外引入一套 TLS 实现
        let mut tls = endpoint.tls_config().clone();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .user_agent(format!("roam/{VERSION}"))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()?;
        let devices = std::fs::read(cache.join("last-update.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Ok(Self {
            client,
            cache,
            state: Mutex::new(UpdateState {
                current: VERSION.into(),
                devices,
                ..Default::default()
            }),
        })
    }

    fn api_base() -> String {
        std::env::var("ROAM_UPDATE_API")
            .unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}"))
    }

    pub fn set_phase(&self, phase: Option<String>) {
        self.state.lock().unwrap().phase = phase;
    }

    pub fn set_error(&self, error: Option<String>) {
        self.state.lock().unwrap().error = error;
    }

    /// 重启后仍能看到哪些被控端更新失败，避免只剩普通连接错误。
    pub fn save_results(&self) {
        let result = (|| -> Result<()> {
            std::fs::create_dir_all(&self.cache)?;
            let devices = self.state.lock().unwrap().devices.clone();
            std::fs::write(
                self.cache.join("last-update.json"),
                serde_json::to_vec(&devices)?,
            )?;
            Ok(())
        })();
        if let Err(err) = result {
            tracing::warn!("保存设备更新结果失败：{err:#}");
        }
    }

    async fn get_release(&self, path: &str) -> Result<Release> {
        let url = format!("{}/{path}", Self::api_base());
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("无法连接 GitHub 更新接口；请检查系统代理或网络设置")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("还没有发布任何版本");
        }
        let gh: GhRelease = resp.error_for_status()?.json().await?;
        if gh.draft || gh.prerelease {
            bail!("最新版本还未正式发布");
        }
        Ok(gh.into())
    }

    /// 检查最新版本，返回是否有更新。
    pub async fn check(&self) -> Result<bool> {
        let result = self.get_release("releases/latest").await;
        let mut state = self.state.lock().unwrap();
        state.last_checked = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
        match result {
            Ok(release) => {
                state.available = is_newer(&release.version, VERSION);
                state.latest = Some(release);
                state.error = None;
                Ok(state.available)
            }
            Err(err) => {
                state.error = Some(format!("检查更新失败：{err:#}"));
                tracing::warn!("检查更新失败：{err:#}");
                Err(err)
            }
        }
    }

    /// 指定版本的发布信息（同步旧设备到主机当前版本时用）。
    pub async fn release_for(&self, version: &str) -> Result<Release> {
        self.get_release(&format!("releases/tags/v{version}")).await
    }

    /// 仅准备本机的新版本，不依赖主机连接，也不降级本机。
    pub async fn prepare_self_update(&self, mirror: Option<&str>) -> Result<Option<PathBuf>> {
        self.set_error(None);
        self.set_phase(Some("正在检查更新…".into()));
        if !self.check().await? {
            return Ok(None);
        }
        let release = self
            .state
            .lock()
            .unwrap()
            .latest
            .clone()
            .context("没有可用的版本")?;
        self.fetch(&release, Os::current(), mirror).await.map(Some)
    }

    /// 下载某个平台的安装包并校验 SHA256，已经下载过就直接用缓存。
    pub async fn fetch(&self, release: &Release, os: Os, mirror: Option<&str>) -> Result<PathBuf> {
        let name = asset_name(os).context("这个平台没有安装包")?;
        self.set_phase(Some(format!("正在获取 {name} 的校验和…")));
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == name)
            .with_context(|| format!("v{} 里没有 {name}", release.version))?;
        let expected = self.expected_hash(release, name).await?;

        let dir = self.cache.join(&release.version);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        if path.exists() && sha256_file(&path)? == expected {
            return Ok(path);
        }

        let url = with_mirror(&asset.url, mirror);
        self.set_phase(Some(format!("正在连接下载 {name}…")));
        let mut resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("下载 {name} 失败"))?
            .error_for_status()?;
        let total = resp.content_length().unwrap_or(asset.size).max(1);
        let tmp = dir.join(format!("{name}.download"));
        let mut file = std::fs::File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut done = 0u64;
        let mut last_pct = u64::MAX;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk)?;
            hasher.update(&chunk);
            done += chunk.len() as u64;
            let pct = done * 100 / total;
            if pct != last_pct {
                last_pct = pct;
                self.set_phase(Some(format!("下载 {name} {pct}%")));
            }
        }
        file.flush()?;
        drop(file);
        let actual: [u8; 32] = hasher.finalize().into();
        if actual != expected {
            let _ = std::fs::remove_file(&tmp);
            bail!("{name} 校验失败，文件可能被篡改或下载不完整");
        }
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// 校验和总是直接从 GitHub 获取（文件很小），只有安装包走镜像：
    /// 这样即使镜像不可信，也无法同时伪造安装包和校验和。
    async fn expected_hash(&self, release: &Release, name: &str) -> Result<[u8; 32]> {
        let sums = release
            .assets
            .iter()
            .find(|a| a.name == CHECKSUMS)
            .context("发布里缺少 SHA256SUMS，为安全起见不更新")?;
        let text = self
            .client
            .get(&sums.url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .context("无法从 GitHub 获取校验和")?
            .error_for_status()?
            .text()
            .await?;
        parse_checksum(&text, name).with_context(|| format!("SHA256SUMS 里没有 {name}"))
    }
}

fn with_mirror(url: &str, mirror: Option<&str>) -> String {
    match mirror.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("{}/{url}", m.trim_end_matches('/')),
        None => url.to_string(),
    }
}

/// 解析 `sha256sum` 格式：`<hex>  <文件名>`。
fn parse_checksum(text: &str, name: &str) -> Option<[u8; 32]> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hex = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name).then(|| hex_to_bytes(hex)).flatten()
    })
}

fn hex_to_bytes(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

// ---------------------------------------------------------------- 安装与重启

fn old_path(exe: &Path) -> PathBuf {
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    exe.with_file_name(format!("{name}.old"))
}

/// 完整准备新程序后再替换；返回替换前保存的启动路径。
pub fn install(new_bin: &Path) -> Result<PathBuf> {
    // 必须在替换前保存路径：部分系统在运行文件改名后，current_exe 会指向旧文件。
    let exe = std::env::current_exe().context("找不到当前程序路径")?;
    install_at(new_bin, &exe)?;
    Ok(exe)
}

fn install_at(new_bin: &Path, exe: &Path) -> Result<()> {
    let prepared = exe.with_file_name(format!(
        "{}.update-{}",
        exe.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = prepare_install(new_bin, exe, &prepared);
    if let Err(err) = result {
        let _ = std::fs::remove_file(&prepared);
        return Err(err);
    }
    let old = old_path(exe);
    let result = replace_prepared(&prepared, exe, &old);
    let _ = std::fs::remove_file(&prepared);
    result
}

fn prepare_install(new_bin: &Path, exe: &Path, prepared: &Path) -> Result<()> {
    std::fs::copy(new_bin, prepared).context("无法写入新程序，请检查程序所在文件夹的写入权限")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(prepared, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(target_os = "macos")]
    {
        // 在替换之前验证发布包的签名，避免旧程序已经移走才发现新文件无法运行。
        let status = std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(prepared)
            .status()
            .context("无法检查 Mac 安装包签名")?;
        if !status.success() {
            bail!("Mac 安装包签名验证失败，已保留原程序");
        }
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(prepared)
            .status();
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(prepared)?
        .sync_all()?;
    // 在改动原程序前确认它存在，下载失败等情况不影响正在运行的程序。
    std::fs::metadata(exe).context("找不到待替换的原程序")?;
    Ok(())
}

fn replace_prepared(prepared: &Path, exe: &Path, old: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::copy(exe, old).context("无法备份当前程序，请检查文件夹写入权限")?;
        // 同目录 rename 原子替换，复制、权限或签名失败不会留下一半安装的程序。
        std::fs::rename(prepared, exe).context("无法替换当前程序，原程序保持不变")?;
    }
    #[cfg(not(unix))]
    {
        if old.exists() {
            std::fs::remove_file(old).context("旧程序仍被占用，请稍后重试更新")?;
        }
        std::fs::rename(exe, old).context("无法替换当前程序（权限不足？）")?;
        if let Err(err) = std::fs::rename(prepared, exe) {
            std::fs::rename(old, exe).context("写入新程序失败，而且无法还原原程序")?;
            return Err(err).context("写入新程序失败，已还原");
        }
    }
    Ok(())
}

/// 用同样的参数重新启动自己。macOS 上直接替换进程（保持同一个 PID，launchd 与终端都不受影响）。
pub fn restart_at(exe: &Path) -> ! {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(exe).args(&args).exec();
        eprintln!("重启失败: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        if let Err(err) = std::process::Command::new(exe).args(&args).spawn() {
            tracing::error!("新版已安装，但重启失败：{err}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
}

/// 用指定参数重新启动自己（切换角色时用）。
pub fn restart_with(args: &[String]) -> ! {
    let exe = std::env::current_exe().expect("current exe");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).args(args).exec();
        eprintln!("重启失败: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new(&exe).args(args).spawn();
        std::process::exit(0);
    }
}

/// 启动时清理上次更新留下的旧程序（Windows 上旧进程可能还没完全退出，稍后重试）。
pub fn cleanup_old() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let old = old_path(&exe);
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..20 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

// ---------------------------------------------------------------- 更新通道

async fn write_frame(send: &mut iroh::endpoint::SendStream, body: &impl Serialize) -> Result<()> {
    let body = postcard::to_stdvec(body)?;
    send.write_all(&(body.len() as u32).to_be_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    recv: &mut iroh::endpoint::RecvStream,
) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 64 * 1024 {
        bail!("消息过大");
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

/// 主机：把新版本推给一台被控端。
pub async fn push(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    bin: &Path,
    version: &str,
) -> Result<()> {
    let conn = tokio::time::timeout(Duration::from_secs(15), endpoint.connect(addr, UPDATE_ALPN))
        .await
        .context("连接超时（设备可能离线）")?
        .map_err(|e| {
            let text = format!("{e:#}");
            if text.contains("doesn't support any known protocol")
                || text.contains("no application protocol")
            {
                anyhow::anyhow!("这台设备的版本太旧，不支持应用内更新，需要手动更新一次")
            } else {
                anyhow::anyhow!(text)
            }
        })?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let size = std::fs::metadata(bin)?.len();
    let offer = UpdateOffer {
        version: version.to_string(),
        size,
        sha256: sha256_file(bin)?,
    };
    write_frame(&mut send, &offer).await?;
    let mut file = tokio::fs::File::open(bin).await?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
    }
    send.finish()?;
    let reply: UpdateReply = tokio::time::timeout(Duration::from_secs(60), read_frame(&mut recv))
        .await
        .context("等待对方安装超时")??;
    conn.close(0u32.into(), b"done");
    match reply {
        UpdateReply::Installed => Ok(()),
        UpdateReply::Rejected(reason) => bail!("对方拒绝：{reason}"),
    }
}

/// 被控端：接收主机推送的新版本，校验后替换并重启。
/// `trusted` 为发起连接的主机是否已授权。成功时不返回（进程会重启）。
pub async fn receive(conn: Connection, trusted: bool, staging: PathBuf) -> Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    let offer: UpdateOffer = read_frame(&mut recv).await?;
    if !trusted {
        write_frame(&mut send, &UpdateReply::Rejected("未授权的主机".into())).await?;
        send.finish()?;
        bail!("拒绝了未授权主机推送的更新");
    }
    if offer.size > 500 * 1024 * 1024 {
        write_frame(&mut send, &UpdateReply::Rejected("文件过大".into())).await?;
        bail!("更新文件过大");
    }

    let tmp = staging.join("roam-update.bin");
    let mut file = std::fs::File::create(&tmp)?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    while let Some(n) = recv.read(&mut buf).await? {
        received += n as u64;
        if received > offer.size {
            bail!("收到的数据比声明的多");
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
    }
    file.flush()?;
    drop(file);
    let actual: [u8; 32] = hasher.finalize().into();
    if received != offer.size || actual != offer.sha256 {
        let _ = std::fs::remove_file(&tmp);
        write_frame(&mut send, &UpdateReply::Rejected("校验失败".into())).await?;
        bail!("更新文件校验失败");
    }
    let exe = match install(&tmp) {
        Ok(exe) => exe,
        Err(err) => {
            write_frame(&mut send, &UpdateReply::Rejected(format!("{err:#}"))).await?;
            return Err(err);
        }
    };
    let _ = std::fs::remove_file(&tmp);
    write_frame(&mut send, &UpdateReply::Installed).await?;
    send.finish()?;
    // 等回复送达再重启
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    println!("已更新到 v{}，正在重启…", offer.version);
    restart_at(&exe);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bad_download_preserves_cache_and_valid_retry_succeeds() {
        use axum::{Router, routing::get};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let valid = Arc::new(AtomicBool::new(false));
        let payload = b"verified executable bytes";
        let hash = Sha256::digest(payload)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let os = Os::Windows;
        let name = asset_name(os).unwrap();
        let sums = format!("{hash}  {name}\n");
        let flag = valid.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/sums", get(move || async move { sums }))
                    .route(
                        "/bin",
                        get(move || {
                            let flag = flag.clone();
                            async move {
                                if flag.load(Ordering::Relaxed) {
                                    payload.as_slice()
                                } else {
                                    b"corrupt bytes".as_slice()
                                }
                            }
                        }),
                    ),
            )
            .await
            .unwrap();
        });
        let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let dir = TestDir::new();
        let updater = Updater::new(&endpoint, dir.0.clone()).unwrap();
        let release = Release {
            version: "9.0.0".into(),
            notes: String::new(),
            url: base.clone(),
            assets: vec![
                Asset {
                    name: name.into(),
                    url: format!("{base}/bin"),
                    size: payload.len() as u64,
                },
                Asset {
                    name: CHECKSUMS.into(),
                    url: format!("{base}/sums"),
                    size: 0,
                },
            ],
        };
        let cached = dir.0.join("9.0.0").join(name);
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"previous cached bytes").unwrap();
        assert!(updater.fetch(&release, os, None).await.is_err());
        assert_eq!(std::fs::read(&cached).unwrap(), b"previous cached bytes");
        assert!(!cached.with_file_name(format!("{name}.download")).exists());
        valid.store(true, Ordering::Relaxed);
        let fetched = updater.fetch(&release, os, None).await.unwrap();
        assert_eq!(std::fs::read(fetched).unwrap(), payload);
        endpoint.close().await;
        server.abort();
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "roam-install-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn latest_host_still_updates_an_older_peer_without_downgrading() {
        assert!(!needs_update(Some("0.3.0"), "0.3.0"));
        assert!(needs_update(Some("0.2.0"), "0.3.0"));
        assert!(!needs_update(Some("0.4.0"), "0.3.0"));
        assert!(needs_update(None, "0.3.0"));
        assert!(needs_update(Some("unknown"), "0.3.0"));
    }

    #[tokio::test]
    async fn pushes_over_update_channel_without_a_control_connection() {
        use iroh::endpoint::presets;

        let sender = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let receiver = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .alpns(vec![UPDATE_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let addr = EndpointAddr::new(receiver.id()).with_ip_addr(receiver.bound_sockets()[0]);
        let dir = TestDir::new();
        let bin = dir.0.join("release.bin");
        let payload = b"release bytes for an older peer";
        std::fs::write(&bin, payload).unwrap();
        let expected = sha256_file(&bin).unwrap();
        let server = tokio::spawn(async move {
            let conn = receiver.accept().await.unwrap().await.unwrap();
            assert_eq!(conn.alpn(), UPDATE_ALPN);
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let offer: UpdateOffer = read_frame(&mut recv).await.unwrap();
            assert_eq!(offer.version, "0.3.0");
            assert_eq!(offer.sha256, expected);
            assert_eq!(offer.size, payload.len() as u64);
            let bytes = recv.read_to_end(1024).await.unwrap();
            assert_eq!(bytes.as_slice(), payload);
            write_frame(&mut send, &UpdateReply::Installed)
                .await
                .unwrap();
            send.finish().unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
            receiver.close().await;
        });
        tokio::time::timeout(Duration::from_secs(15), push(&sender, addr, &bin, "0.3.0"))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        sender.close().await;
    }

    #[test]
    fn failed_preparation_leaves_original_executable_untouched() {
        let dir = TestDir::new();
        let exe = dir.0.join("roam");
        std::fs::write(&exe, b"original").unwrap();
        assert!(install_at(&dir.0.join("missing-download"), &exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"original");
        assert!(!old_path(&exe).exists());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn installation_keeps_backup_and_makes_the_new_file_executable() {
        let dir = TestDir::new();
        let exe = dir.0.join("roam");
        let download = dir.0.join("download");
        std::fs::write(&exe, b"old version").unwrap();
        std::fs::write(&download, b"new version").unwrap();
        install_at(&download, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new version");
        assert_eq!(std::fs::read(old_path(&exe)).unwrap(), b"old version");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111,
                0
            );
        }
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 3);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn invalid_mac_signature_keeps_the_original() {
        let dir = TestDir::new();
        let exe = dir.0.join("roam");
        let download = dir.0.join("download");
        std::fs::write(&exe, b"original").unwrap();
        std::fs::write(&download, b"not a signed Mach-O").unwrap();
        assert!(install_at(&download, &exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"original");
        assert!(!old_path(&exe).exists());
    }

    #[test]
    fn versions_compare() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
    }

    #[test]
    fn checksum_file_parsing() {
        let hash = "a".repeat(64);
        let text = format!(
            "{hash}  roam-windows-x86_64.exe\n{}  roam-macos-universal\n",
            "b".repeat(64)
        );
        assert_eq!(
            parse_checksum(&text, "roam-windows-x86_64.exe"),
            Some([0xaa; 32])
        );
        assert_eq!(
            parse_checksum(&text, "roam-macos-universal"),
            Some([0xbb; 32])
        );
        assert_eq!(parse_checksum(&text, "other"), None);
    }

    #[test]
    fn mirror_prefix() {
        let url = "https://github.com/a/b/releases/download/v1/x";
        assert_eq!(with_mirror(url, None), url);
        assert_eq!(
            with_mirror(url, Some("https://ghfast.top/")),
            format!("https://ghfast.top/{url}")
        );
    }
}
