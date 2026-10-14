//! 本地配置与设备密钥。
//!
//! 目录：Windows `%APPDATA%\roam`，macOS `~/Library/Application Support/roam`，
//! 可用 `--config-dir` 覆盖（同一台机器上同时跑主机和被控端测试时需要）。

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use roam_proto::{DisplayInfo, Os};
use serde::{Deserialize, Serialize};

use crate::{
    layout::Cell,
    settings::{ClipboardSettings, DeviceSettings, SwitchSettings},
};

/// 本机的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 用本机键鼠控制其他电脑。
    Host,
    /// 被其他电脑控制。
    Client,
}

/// 主机记录的一台被控设备。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// EndpointId 的字符串形式。
    pub id: String,
    pub ticket: String,
    pub name: String,
    /// 最近一次连上时上报的信息，离线时也能在布局里显示。
    #[serde(default)]
    pub os: Option<Os>,
    #[serde(default)]
    pub displays: Vec<DisplayInfo>,
    #[serde(default)]
    pub settings: DeviceSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    /// 主机：要控制的设备。
    #[serde(default)]
    pub peers: Vec<Peer>,
    /// 被控端：允许控制本机的主机 ID。
    #[serde(default)]
    pub trusted_hosts: Vec<String>,
    /// 被控端：已授权主机的名字（显示用）。
    #[serde(default)]
    pub host_names: BTreeMap<String, String>,
    /// 本机角色；未设置时 macOS 默认主机，其他系统默认被控端。
    #[serde(default)]
    pub role: Option<Role>,
    /// 主机：每台设备（含主机自己）在布局网格中的格子。
    #[serde(default)]
    pub grid: BTreeMap<String, Cell>,
    /// 控制 Windows 时把 Cmd 和 Ctrl 对调。
    #[serde(default = "default_true")]
    pub swap_cmd_ctrl: bool,
    /// 切屏手感。
    #[serde(default)]
    pub switching: SwitchSettings,
    /// 剪贴板同步。
    #[serde(default)]
    pub clipboard: ClipboardSettings,
    /// 下载更新时加在 GitHub 地址前的镜像前缀（国内加速用），例如 `https://ghfast.top/`。
    #[serde(default)]
    pub update_mirror: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Config {
    fn new_default() -> Self {
        Self {
            name: default_device_name(),
            peers: Vec::new(),
            trusted_hosts: Vec::new(),
            host_names: BTreeMap::new(),
            role: None,
            grid: BTreeMap::new(),
            swap_cmd_ctrl: true,
            switching: SwitchSettings::default(),
            clipboard: ClipboardSettings::default(),
            update_mirror: None,
        }
    }

    pub fn role(&self) -> Role {
        self.role.unwrap_or(if cfg!(target_os = "macos") {
            Role::Host
        } else {
            Role::Client
        })
    }

    pub fn peer_mut(&mut self, id: &str) -> Option<&mut Peer> {
        self.peers.iter_mut().find(|p| p.id == id)
    }
}

/// 配置目录。
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: Option<PathBuf>) -> Result<Self> {
        let dir = match dir {
            Some(dir) => dir,
            None => dirs::config_dir()
                .context("找不到系统配置目录")?
                .join("roam"),
        };
        fs::create_dir_all(&dir).with_context(|| format!("无法创建 {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    pub fn load(&self) -> Result<Config> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(Config::new_default());
        }
        let text = fs::read_to_string(&path)?;
        serde_json::from_str(&text).with_context(|| format!("配置文件损坏: {}", path.display()))
    }

    /// 先写临时文件再改名，避免写一半断电导致配置损坏。
    pub fn save(&self, config: &Config) -> Result<()> {
        let tmp = self.dir.join("config.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(config)?)?;
        fs::rename(&tmp, self.config_path())?;
        Ok(())
    }

    #[cfg(feature = "net")]
    /// 设备密钥：首次运行生成并保存，之后保持不变，连接码因此也固定。
    pub fn secret_key(&self) -> Result<iroh::SecretKey> {
        let path = self.dir.join("device.key");
        if let Ok(bytes) = fs::read(&path) {
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("密钥文件损坏: {}", path.display()))?;
            return Ok(iroh::SecretKey::from_bytes(&bytes));
        }
        let key = iroh::SecretKey::generate();
        fs::write(&path, key.to_bytes())?;
        Ok(key)
    }
}

fn default_device_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_end_matches(".local").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "我的电脑".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "net")]
    #[test]
    fn key_is_stable_and_config_roundtrips() {
        let dir = std::env::temp_dir().join(format!("roam-test-{}", std::process::id()));
        let store = Store::open(Some(dir.clone())).unwrap();
        let k1 = store.secret_key().unwrap();
        let k2 = store.secret_key().unwrap();
        assert_eq!(k1.public(), k2.public());

        let mut cfg = store.load().unwrap();
        cfg.grid.insert("a".into(), Cell { col: 1, row: 2 });
        store.save(&cfg).unwrap();
        assert_eq!(store.load().unwrap().grid["a"], Cell { col: 1, row: 2 });
        fs::remove_dir_all(dir).ok();
    }
}
