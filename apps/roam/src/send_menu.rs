//! 文件管理器入口。路径只经本机权限保护的请求文件交给已运行的 Roam，
//! 浏览器只能用随机凭据选择目标，不能通过 HTTP 指定任意本地文件。

use anyhow::{Context, Result, ensure};
use roam_core::config::Store;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const MENU: &str = "Roam - 发送到其他电脑";
const TTL: u64 = 60 * 60;

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub paths: Vec<PathBuf>,
    created: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn request_dir(store: &Store) -> PathBuf {
    store.dir().join("send-requests")
}

fn request_path(store: &Store, token: &str) -> Result<PathBuf> {
    ensure!(
        token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "无效的发送请求"
    );
    Ok(request_dir(store).join(format!("{token}.json")))
}

pub fn read(store: &Store, token: &str) -> Result<Request> {
    let request: Request = serde_json::from_slice(
        &fs::read(request_path(store, token)?)
            .context("发送请求不存在或已经处理，请重新选择文件")?,
    )?;
    ensure!(
        now().saturating_sub(request.created) < TTL,
        "发送请求已过期，请重新选择文件"
    );
    ensure!(!request.paths.is_empty(), "没有选择文件");
    Ok(request)
}

pub fn claim(store: &Store, token: &str) -> Result<Request> {
    let request = read(store, token)?;
    let path = request_path(store, token)?;
    fs::rename(&path, path.with_extension("sending")).context("请求已被处理，请勿重复发送")?;
    Ok(request)
}

pub fn finish(store: &Store, token: &str, success: bool) -> Result<()> {
    let path = request_path(store, token)?;
    if success {
        fs::remove_file(path.with_extension("sending"))?;
    } else {
        fs::rename(path.with_extension("sending"), path)?;
    }
    Ok(())
}

pub fn prepare(store: &Store, paths: &[PathBuf]) -> Result<String> {
    ensure!(!paths.is_empty(), "没有选择文件");
    let mut selected = Vec::new();
    for path in paths {
        let meta =
            fs::symlink_metadata(path).with_context(|| format!("无法读取 {}", path.display()))?;
        ensure!(
            meta.is_file() || meta.is_dir(),
            "请选择普通文件或文件夹：{}",
            path.display()
        );
        let path = fs::canonicalize(path)?;
        if !selected.contains(&path) {
            selected.push(path);
        }
    }
    let dir = request_dir(store);
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    // 只清理本功能创建、超过一小时的请求，不触碰源文件。
    for entry in fs::read_dir(&dir)?.flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > TTL)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    let token = iroh::SecretKey::generate().public().to_string();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(request_path(store, &token)?)?;
    file.write_all(&serde_json::to_vec(&Request {
        paths: selected,
        created: now(),
    })?)?;
    Ok(token)
}

pub fn open(store: &Store, port: u16, paths: &[PathBuf]) -> Result<()> {
    // 主程序通常由 iroh 初始化；右键入口在建立 iroh 端点之前就返回。
    let _ = rustls::crypto::ring::default_provider().install_default();
    tokio::runtime::Runtime::new()?.block_on(async {
        let state: serde_json::Value = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()?
            .get(format!("http://127.0.0.1:{port}/api/state"))
            .send()
            .await
            .context("请先启动本机 Roam，再右键发送文件")?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            crate::files::supports_direct(state["version"].as_str().unwrap_or("")),
            "正在运行的是旧版 Roam，请先启动 v0.3.3 或更新版本"
        );
        anyhow::Ok(())
    })?;
    let token = prepare(store, paths)?;
    crate::open_browser(&format!("http://127.0.0.1:{port}/#send={token}"));
    Ok(())
}

fn menu_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    return Ok(dirs::home_dir()
        .context("找不到用户目录")?
        .join("Library/Services")
        .join(format!("{MENU}.workflow")));
    #[cfg(windows)]
    return Ok(dirs::data_dir()
        .context("找不到 AppData")?
        .join("Microsoft/Windows/SendTo")
        .join(format!("{MENU}.lnk")));
    #[cfg(not(any(target_os = "macos", windows)))]
    anyhow::bail!("右键菜单目前支持 macOS 和 Windows");
}

pub fn enabled() -> bool {
    menu_path().is_ok_and(|p| p.exists())
}

pub fn set_enabled(store: &Store, port: u16, enabled: bool) -> Result<()> {
    let path = menu_path()?;
    if !enabled {
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else if path.exists() {
            fs::remove_file(&path)?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    install(&path, &exe, store.dir(), port)
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn install(path: &Path, exe: &Path, config: &Path, port: u16) -> Result<()> {
    use serde_json::json;
    let contents = path.join("Contents");
    fs::create_dir_all(&contents)?;
    let script = format!(
        "exec {} --config-dir {} --ui-port {port} send -- \"$@\"",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&config.to_string_lossy())
    );
    let types = json!({"Container":"List", "Types":["com.apple.cocoa.string"]});
    let workflow = json!({
        "AMApplicationBuild":"534", "AMApplicationVersion":"2.10", "AMDocumentVersion":"2",
        "actions":[{"action":{
            "AMAccepts":types, "AMProvides":types,
            "AMActionVersion":"2.0.3", "AMApplication":["Automator"],
            "ActionBundlePath":"/System/Library/Automator/Run Shell Script.action",
            "ActionName":"Run Shell Script", "BundleIdentifier":"com.apple.RunShellScript",
            "Class Name":"RunShellScriptAction", "UUID":"8DD9A931-EE28-4C39-9810-CEFA98D12AA3",
            "ActionParameters":{"COMMAND_STRING":script, "CheckedForUserDefaultShell":true, "inputMethod":1, "shell":"/bin/zsh", "source":""}
        }}],
        "connectors":{},
        "workflowMetaData":{
            "serviceApplicationBundleID":"com.apple.finder", "applicationBundleID":"com.apple.finder",
            "inputTypeIdentifier":"com.apple.Automator.fileSystemObject",
            "serviceInputTypeIdentifier":"com.apple.Automator.fileSystemObject",
            "outputTypeIdentifier":"com.apple.Automator.nothing", "serviceOutputTypeIdentifier":"com.apple.Automator.nothing",
            "presentationMode":15, "processesInput":false, "serviceProcessesInput":false,
            "useAutomaticInputType":false, "workflowTypeIdentifier":"com.apple.Automator.servicesMenu"
        }
    });
    let info = json!({"NSServices":[{"NSMenuItem":{"default":MENU}, "NSMessage":"runWorkflowAsService", "NSSendFileTypes":["public.item"], "NSRequiredContext":{"NSApplicationIdentifier":"com.apple.finder"}}]});
    for (name, value) in [("Info.plist", info), ("document.wflow", workflow)] {
        let file = contents.join(name);
        fs::write(&file, serde_json::to_vec(&value)?)?;
        ensure!(
            Command::new("/usr/bin/plutil")
                .args(["-convert", "xml1"])
                .arg(&file)
                .status()?
                .success(),
            "无法生成 Finder 快速操作"
        );
    }
    // 请求系统重新扫描服务，不重启 Finder。
    let _ = Command::new("/System/Library/CoreServices/pbs")
        .arg("-update")
        .status();
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_quote(s: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in s.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            out.push_str(&"\\".repeat(slashes));
        }
        out.push(c);
        slashes = 0;
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

#[cfg(windows)]
fn install(path: &Path, exe: &Path, config: &Path, port: u16) -> Result<()> {
    fs::create_dir_all(path.parent().context("无效的发送到目录")?)?;
    let args = format!(
        "--config-dir {} --ui-port {port} send --",
        windows_quote(&config.to_string_lossy())
    );
    let result = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command",
        "$ErrorActionPreference='Stop'; $s=New-Object -ComObject WScript.Shell; $l=$s.CreateShortcut($env:ROAM_MENU_LINK); $l.TargetPath=$env:ROAM_MENU_EXE; $l.Arguments=$env:ROAM_MENU_ARGS; $l.Description='通过 Roam 发送文件到其他电脑'; $l.Save()"])
        .env("ROAM_MENU_LINK", path).env("ROAM_MENU_EXE", exe).env("ROAM_MENU_ARGS", args).output()?;
    ensure!(
        result.status.success(),
        "无法安装发送到菜单：{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn install(_: &Path, _: &Path, _: &Path, _: u16) -> Result<()> {
    anyhow::bail!("不支持当前系统")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_scoped_single_use_and_retryable() {
        let dir = std::env::temp_dir().join(format!("roam-menu-{}", crate::files::new_id()));
        let store = Store::open(Some(dir.clone())).unwrap();
        let file = dir.join("中文 空格 ' $().txt");
        fs::write(&file, b"hello").unwrap();
        let token = prepare(&store, &[file.clone(), file.clone()]).unwrap();
        assert!(read(&store, "../../config").is_err());
        assert_eq!(
            read(&store, &token).unwrap().paths,
            vec![fs::canonicalize(file).unwrap()]
        );
        claim(&store, &token).unwrap();
        assert!(claim(&store, &token).is_err());
        finish(&store, &token, false).unwrap();
        claim(&store, &token).unwrap();
        finish(&store, &token, true).unwrap();
        assert!(read(&store, &token).is_err());
        let expired = "a".repeat(64);
        fs::write(
            request_path(&store, &expired).unwrap(),
            serde_json::to_vec(&Request {
                paths: vec![dir.clone()],
                created: 1,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(read(&store, &expired).is_err());
        #[cfg(target_os = "macos")]
        assert_eq!(shell_quote("a'b $()"), "'a'\\''b $()'");
        assert_eq!(windows_quote("C:\\space dir\\"), "\"C:\\space dir\\\\\"");
        fs::remove_dir_all(dir).unwrap();
    }
}
