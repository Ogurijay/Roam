//! 原生对话框（目前只有 Windows 被控端需要）。

/// 弹出"是 / 否"对话框，置顶显示，阻塞直到用户选择。
#[cfg(windows)]
pub fn confirm(title: &str, message: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
    };
    let wide = |s: &str| {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let (title, message) = (wide(title), wide(message));
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    result == IDYES
}

#[cfg(not(windows))]
pub fn confirm(_title: &str, message: &str) -> bool {
    println!("{message}\n允许吗？[y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    matches!(line.trim(), "y" | "Y" | "yes")
}

/// 提示框（只有"确定"）。
#[cfg(windows)]
pub fn alert(title: &str, message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    };
    let wide = |s: &str| {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let (title, message) = (wide(title), wide(message));
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

#[cfg(not(windows))]
pub fn alert(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}

/// 从资源管理器双击启动时没有控制台；从终端启动时挂到父进程的控制台上，保留命令行输出。
#[cfg(windows)]
pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
pub fn attach_parent_console() {}
