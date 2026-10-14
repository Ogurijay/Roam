//! 开机自启：Windows 写注册表 Run 项，macOS 写 LaunchAgent。

use anyhow::{Context, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode};

fn launcher(args: &[&str]) -> Result<AutoLaunch> {
    let exe = std::env::current_exe().context("找不到当前程序路径")?;
    Ok(AutoLaunchBuilder::new()
        .set_app_name("Roam")
        .set_app_path(&exe.to_string_lossy())
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        .set_args(args)
        .build()?)
}

pub fn is_enabled(args: &[&str]) -> bool {
    launcher(args)
        .and_then(|l| Ok(l.is_enabled()?))
        .unwrap_or(false)
}

pub fn set_enabled(args: &[&str], enabled: bool) -> Result<()> {
    let l = launcher(args)?;
    if enabled {
        l.enable()?;
    } else if l.is_enabled()? {
        l.disable()?;
    }
    Ok(())
}
