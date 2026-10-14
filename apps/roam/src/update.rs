//! 应用内更新。
//!
//! - 新版本由 GitHub Actions 构建并发布到 GitHub Releases（见 `.github/workflows/release.yml`）
//! - 主机定期检查，提示后一键更新：主机下载各平台安装包，通过更新通道
//!   （[`UPDATE_ALPN`]，永远兼容）推送给被控端，最后更新自己
//! - 被控端只接受已授权主机推送的更新，并校验 SHA256

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
pub const REPO: &str = "Ogurijay/roam";

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

impl Updater {
    pub fn new(endpoint: &Endpoint, cache: PathBuf) -> Result<Self> {
        // 复用 iroh 的 TLS 配置（含根证书），不额外引入一套 TLS 实现
        let mut tls = endpoint.tls_config().clone();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .user_agent(format!("roam/{VERSION}"))
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            cache,
            state: Mutex::new(UpdateState {
                current: VERSION.into(),
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

    async fn get_release(&self, path: &str) -> Result<Release> {
        let url = format!("{}/{path}", Self::api_base());
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("无法连接 GitHub")?;
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
                Err(err)
            }
        }
    }

    /// 指定版本的发布信息（同步旧设备到主机当前版本时用）。
    pub async fn release_for(&self, version: &str) -> Result<Release> {
        self.get_release(&format!("releases/tags/v{version}")).await
    }

    /// 下载某个平台的安装包并校验 SHA256，已经下载过就直接用缓存。
    pub async fn fetch(&self, release: &Release, os: Os, mirror: Option<&str>) -> Result<PathBuf> {
        let name = asset_name(os).context("这个平台没有安装包")?;
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == name)
            .with_context(|| format!("v{} 里没有 {name}", release.version))?;
        let expected = self.expected_hash(release, name, mirror).await?;

        let dir = self.cache.join(&release.version);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        if path.exists() && sha256_file(&path)? == expected {
            return Ok(path);
        }

        let url = with_mirror(&asset.url, mirror);
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

    async fn expected_hash(
        &self,
        release: &Release,
        name: &str,
        mirror: Option<&str>,
    ) -> Result<[u8; 32]> {
        let sums = release
            .assets
            .iter()
            .find(|a| a.name == CHECKSUMS)
            .context("发布里缺少 SHA256SUMS，为安全起见不更新")?;
        let text = self
            .client
            .get(with_mirror(&sums.url, mirror))
            .send()
            .await?
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

/// 用新程序替换当前程序。运行中的程序不能覆盖，但可以改名，所以先把自己改名再放新文件。
pub fn install(new_bin: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("找不到当前程序路径")?;
    let old = old_path(&exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).context("无法替换当前程序（权限不足？）")?;
    if let Err(err) = std::fs::copy(new_bin, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(err).context("写入新程序失败，已还原");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(target_os = "macos")]
    {
        // 去掉隔离属性，避免 Gatekeeper 拦截
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&exe)
            .status();
    }
    Ok(())
}

/// 用同样的参数重新启动自己。macOS 上直接替换进程（保持同一个 PID，launchd 与终端都不受影响）。
pub fn restart() -> ! {
    let exe = std::env::current_exe().expect("current exe");
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).args(&args).exec();
        eprintln!("重启失败: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new(&exe).args(&args).spawn();
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
    if let Err(err) = install(&tmp) {
        write_frame(&mut send, &UpdateReply::Rejected(format!("{err:#}"))).await?;
        return Err(err);
    }
    let _ = std::fs::remove_file(&tmp);
    write_frame(&mut send, &UpdateReply::Installed).await?;
    send.finish()?;
    // 等回复送达再重启
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    println!("已更新到 v{}，正在重启…", offer.version);
    restart();
}

#[cfg(test)]
mod tests {
    use super::*;

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
