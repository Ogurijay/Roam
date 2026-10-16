//! Windows 锁屏控制。
//!
//! Windows 的锁屏界面（和 UAC 弹窗）在单独的"安全桌面"上，普通程序注入的键鼠会被系统丢弃。
//! 做法与 RustDesk 等远程控制软件相同：
//!
//! 1. 安装一个以 SYSTEM 身份运行的系统服务 `RoamService`（需要一次管理员授权）；
//! 2. 服务在当前登录的会话里启动「输入助手」（同样是 SYSTEM 身份），助手能切换到锁屏桌面注入键鼠；
//! 3. 被控端平时自己注入；检测到当前是安全桌面时，把键鼠通过命名管道转交给助手。
//!
//! 安全限制：助手**只在会话处于锁定状态时**注入（不处理 UAC 弹窗，否则任何本地程序都能借它
//! 绕过管理员确认），并且只接受 Roam 程序本身的连接。
//!
//! 可选：「允许控制以管理员身份运行的程序」。Windows 不允许普通程序向管理员窗口注入键鼠（UIPI），
//! 光标移到这种窗口上就像断开了一样。打开后，目标是管理员程序时也交给助手注入。开关存在
//! `HKLM\SOFTWARE\Roam`，只有管理员能改（打开要确认一次 UAC）；助手注入前会自己再核实一遍
//! 开关、当前是普通桌面（UAC 弹窗所在的安全桌面仍然不处理）、目标确实是管理员程序。

use std::{
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, bail};
use roam_proto::MouseButton;
use serde::{Deserialize, Serialize};
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE, LocalFree, POINT,
    },
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, DuplicateTokenEx,
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, SECURITY_ATTRIBUTES,
        SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel, TokenPrimary, TokenSessionId,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
            PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
        Registry::{
            HKEY_LOCAL_MACHINE, REG_DWORD, RRF_RT_REG_DWORD, RegDeleteKeyValueW, RegGetValueW,
            RegSetKeyValueW,
        },
        RemoteDesktop::{
            ProcessIdToSessionId, WTS_SESSIONSTATE_LOCK, WTSFreeMemory,
            WTSGetActiveConsoleSessionId, WTSINFOEXW, WTSQuerySessionInformationW,
            WTSSessionInfoEx,
        },
        StationsAndDesktops::{
            CloseDesktop, GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop,
            UOI_NAME,
        },
        Threading::{
            CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId,
            GetExitCodeProcess, INFINITE, OpenProcess, OpenProcessToken, PROCESS_INFORMATION,
            PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, STARTUPINFOW,
            TerminateProcess, WaitForSingleObject,
        },
    },
    UI::{
        Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::{
            GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, WindowFromPoint,
        },
    },
};

pub const SERVICE_NAME: &str = "RoamService";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const PIPE_ACCESS_DUPLEX: u32 = 3;
const GENERIC_ALL: u32 = 0x1000_0000;
const WAIT_TIMEOUT: u32 = 258;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- 服务管理（界面里调用）

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatusView {
    NotInstalled,
    Stopped,
    Running,
}

pub fn status() -> ServiceStatusView {
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return ServiceStatusView::NotInstalled;
    };
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => match service.query_status() {
            Ok(s) if s.current_state == ServiceState::Running => ServiceStatusView::Running,
            _ => ServiceStatusView::Stopped,
        },
        Err(_) => ServiceStatusView::NotInstalled,
    }
}

/// 以管理员身份运行 `roam.exe service <action>`（会弹出 UAC 确认），等它结束。
pub fn run_elevated(action: &str, log_dir: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let verb = wide("runas");
    let file = wide_os(exe.as_os_str());
    let params = wide(&format!(
        "service {action} --log-dir \"{}\"",
        log_dir.display()
    ));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        bail!("没有获得管理员授权");
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code != 0 {
        bail!("操作失败（详见日志文件夹里的 service.log）");
    }
    Ok(())
}

/// 安装并启动服务（需要在管理员进程里调用）。
pub fn install(log_dir: &Path) -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    // 已经装过就先删掉重装（程序路径可能变了）
    if let Ok(old) = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        let _ = old.stop();
        let _ = old.delete();
        drop(old);
        std::thread::sleep(Duration::from_secs(2));
    }
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("Roam 锁屏控制"),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![
            OsString::from("service"),
            OsString::from("run"),
            OsString::from("--log-dir"),
            log_dir.as_os_str().to_owned(),
        ],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager.create_service(
        &info,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    service.set_description("让 Roam 在 Windows 锁屏界面也能接收远程键鼠（只在锁屏时生效）")?;
    service.start::<&str>(&[])?;
    tracing::info!("已安装并启动 {SERVICE_NAME}");
    Ok(())
}

/// 停止并删除服务（需要在管理员进程里调用）。
pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
    }
    service.delete()?;
    let _ = set_allow_elevated(false);
    tracing::info!("已卸载 {SERVICE_NAME}");
    Ok(())
}

// ---------------------------------------------------------------- 控制管理员程序（可选）

const REG_KEY: &str = r"SOFTWARE\Roam";
const REG_ALLOW_ELEVATED: &str = "AllowElevated";
/// 完整性级别：管理员程序是 High（0x3000）。
const HIGH_INTEGRITY: u32 = 0x3000;

/// 是否允许输入助手向以管理员身份运行的程序注入键鼠。
pub fn allow_elevated() -> bool {
    let key = wide(REG_KEY);
    let name = wide(REG_ALLOW_ELEVATED);
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    result == 0 && data != 0
}

/// 打开 / 关闭「允许控制管理员程序」（需要在管理员进程里调用）。
pub fn set_allow_elevated(on: bool) -> Result<()> {
    let key = wide(REG_KEY);
    let name = wide(REG_ALLOW_ELEVATED);
    let result = if on {
        let data = 1u32;
        unsafe {
            RegSetKeyValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                REG_DWORD,
                (&data as *const u32).cast(),
                size_of::<u32>() as u32,
            )
        }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr()) }
    };
    if result != 0 && !(result == ERROR_FILE_NOT_FOUND && !on) {
        bail!(
            "写注册表失败: {}",
            std::io::Error::from_raw_os_error(result as i32)
        );
    }
    tracing::info!(
        "{}控制以管理员身份运行的程序",
        if on { "已允许" } else { "已禁止" }
    );
    Ok(())
}

/// 进程的完整性级别（普通程序 0x2000，管理员 0x3000，SYSTEM 0x4000）。
/// 进程打不开（受保护的系统进程等）返回 None；能打开但读不了令牌，说明权限比我们高，返回 `u32::MAX`。
fn integrity_level(pid: u32) -> Option<u32> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        CloseHandle(process);
        if opened == 0 {
            return Some(u32::MAX);
        }
        let mut buf = [0u64; 16];
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buf.as_mut_ptr().cast(),
            size_of_val(&buf) as u32,
            &mut len,
        );
        CloseHandle(token);
        if ok == 0 {
            return Some(u32::MAX);
        }
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(label.Label.Sid, (count - 1) as u32))
    }
}

/// 前台窗口和鼠标下面的窗口所属的进程。
fn target_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        let under_cursor = if GetCursorPos(&mut point) != 0 {
            WindowFromPoint(point)
        } else {
            std::ptr::null_mut()
        };
        for hwnd in [GetForegroundWindow(), under_cursor] {
            if hwnd.is_null() {
                continue;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != 0 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

fn process_name(pid: u32) -> String {
    process_path(pid)
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| format!("PID {pid}"))
}

/// 被控端用：前台窗口或鼠标下的窗口属于权限比本程序高的程序（以管理员身份运行）时，返回程序名。
/// 本程序注入的键鼠到不了这种窗口。
pub fn elevated_target() -> Option<String> {
    let me = integrity_level(unsafe { GetCurrentProcessId() }).unwrap_or(0x2000);
    target_pids()
        .into_iter()
        .find(|&pid| integrity_level(pid).is_some_and(|level| level > me))
        .map(process_name)
}

/// 当前输入桌面的名字：平时是 "Default"，锁屏 / UAC 弹窗时是 "Winlogon"。
fn input_desktop_name() -> Option<String> {
    unsafe {
        let desk = OpenInputDesktop(0, 0, 0x0001); // DESKTOP_READOBJECTS
        if desk.is_null() {
            return None;
        }
        let mut buf = [0u16; 64];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            desk,
            UOI_NAME,
            buf.as_mut_ptr().cast(),
            size_of_val(&buf) as u32,
            &mut needed,
        );
        CloseDesktop(desk);
        if ok == 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// 助手用：开关已打开、当前是普通桌面（不是 UAC 弹窗），而且前台窗口或鼠标下的窗口属于管理员程序。
fn elevated_target_allowed() -> bool {
    allow_elevated()
        && input_desktop_name().is_some_and(|name| name.eq_ignore_ascii_case("Default"))
        && target_pids()
            .into_iter()
            .any(|pid| integrity_level(pid).is_some_and(|level| level >= HIGH_INTEGRITY))
}

// ---------------------------------------------------------------- 服务主循环（SYSTEM，会话 0）

define_windows_service!(ffi_service_main, service_main);

/// 由服务控制管理器启动时调用，阻塞直到服务停止。
pub fn run_service() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(err) = service_loop() {
        tracing::error!("服务异常退出: {err:#}");
    }
}

fn service_loop() -> Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |event| match event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let set = |state, accept| {
        status.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    )?;
    tracing::info!("服务已启动");

    let exe = std::env::current_exe()?;
    let log_dir = crate::logging::dir().unwrap_or_else(std::env::temp_dir);
    let mut helper: Option<Helper> = None;
    loop {
        // 保证当前登录的会话里有一个输入助手；会话切换、助手退出或程序更新后重启它
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let modified = std::fs::metadata(&exe).and_then(|m| m.modified()).ok();
        let healthy = helper
            .as_ref()
            .is_some_and(|h| h.session == session && h.alive() && h.exe_modified == modified);
        if !healthy {
            if let Some(old) = helper.take() {
                old.kill();
            }
            if session != u32::MAX {
                match Helper::launch(&exe, session, &log_dir, modified) {
                    Ok(h) => {
                        tracing::info!("已在会话 {session} 启动输入助手");
                        helper = Some(h);
                    }
                    Err(err) => tracing::warn!("启动输入助手失败: {err:#}"),
                }
            }
        }
        match stop_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    if let Some(h) = helper.take() {
        h.kill();
    }
    set(ServiceState::Stopped, ServiceControlAccept::empty())?;
    Ok(())
}

struct Helper {
    process: HANDLE,
    session: u32,
    exe_modified: Option<SystemTime>,
}

impl Helper {
    /// 用服务自己的 SYSTEM 令牌，在指定会话里启动输入助手。
    fn launch(
        exe: &Path,
        session: u32,
        log_dir: &Path,
        exe_modified: Option<SystemTime>,
    ) -> Result<Self> {
        unsafe {
            let mut own: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) == 0 {
                bail!("OpenProcessToken 失败");
            }
            let mut token: HANDLE = std::ptr::null_mut();
            let ok = DuplicateTokenEx(
                own,
                TOKEN_ALL_ACCESS,
                std::ptr::null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut token,
            );
            CloseHandle(own);
            if ok == 0 {
                bail!("DuplicateTokenEx 失败");
            }
            let mut session_id = session;
            if SetTokenInformation(
                token,
                TokenSessionId,
                (&mut session_id as *mut u32).cast(),
                size_of::<u32>() as u32,
            ) == 0
            {
                CloseHandle(token);
                bail!("SetTokenInformation 失败");
            }
            let app = wide_os(exe.as_os_str());
            let mut cmd = wide(&format!(
                "\"{}\" input-helper --log-dir \"{}\"",
                exe.display(),
                log_dir.display()
            ));
            let mut desktop = wide("winsta0\\default");
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desktop.as_mut_ptr();
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessAsUserW(
                token,
                app.as_ptr(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NO_WINDOW,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            );
            CloseHandle(token);
            if ok == 0 {
                bail!(
                    "CreateProcessAsUserW 失败: {}",
                    std::io::Error::last_os_error()
                );
            }
            CloseHandle(pi.hThread);
            Ok(Self {
                process: pi.hProcess,
                session,
                exe_modified,
            })
        }
    }

    fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    fn kill(self) {
        unsafe {
            TerminateProcess(self.process, 0);
            CloseHandle(self.process);
        }
    }
}

// ---------------------------------------------------------------- 输入助手（SYSTEM，用户会话）

/// 被控端发给助手的消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HelperMsg {
    MoveTo { x: f32, y: f32 },
    Key { usage: u16, down: bool },
    Button { button: MouseButton, down: bool },
    WheelLines { dx: i32, dy: i32 },
    WheelPixels { dx: f32, dy: f32 },
    ReleaseAll,
}

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\roam-input-{session}")
}

fn current_session() -> u32 {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    session
}

/// 本会话是否处于锁屏状态。
fn session_locked(session: u32) -> bool {
    unsafe {
        let mut buf: *mut u16 = std::ptr::null_mut();
        let mut len = 0u32;
        if WTSQuerySessionInformationW(
            std::ptr::null_mut(),
            session,
            WTSSessionInfoEx,
            &mut buf,
            &mut len,
        ) == 0
            || buf.is_null()
        {
            return false;
        }
        let info = &*(buf as *const WTSINFOEXW);
        let locked = info.Level == 1
            && info.Data.WTSInfoExLevel1.SessionFlags as u32 == WTS_SESSIONSTATE_LOCK;
        WTSFreeMemory(buf.cast());
        locked
    }
}

fn process_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
    norm(a) == norm(b)
}

/// 输入助手主循环：在命名管道上等待被控端，把收到的键鼠注入到当前的输入桌面。
pub fn run_helper() -> Result<()> {
    roam_input::windows::init_dpi();
    let session = current_session();
    let me = std::env::current_exe()?;
    let name = wide(&pipe_name(session));
    // 只允许 SYSTEM 和交互登录的用户访问管道
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GRGW;;;IU)");
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        bail!("无法创建管道安全描述符");
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    tracing::info!("输入助手已启动（会话 {session}）");
    let mut injector = roam_input::windows::Injector::new();
    let mut desktop: HDESK = std::ptr::null_mut();

    loop {
        let pipe = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                &attrs,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            unsafe { LocalFree(sd) };
            bail!("无法创建命名管道: {}", std::io::Error::last_os_error());
        }
        if unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } == 0 {
            // 客户端在 Create 与 Connect 之间已经连上时也会返回 0（ERROR_PIPE_CONNECTED），照常处理
            const ERROR_PIPE_CONNECTED: i32 = 535;
            if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_PIPE_CONNECTED) {
                unsafe { CloseHandle(pipe) };
                continue;
            }
        }
        // 只接受 Roam 程序自己的连接
        let mut pid = 0u32;
        unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) };
        let trusted = process_path(pid).is_some_and(|p| same_path(&p, &me));
        if !trusted {
            tracing::warn!("拒绝了来自其他程序（PID {pid}）的连接");
            unsafe {
                DisconnectNamedPipe(pipe);
                CloseHandle(pipe);
            }
            continue;
        }
        let mut file = unsafe { File::from_raw_handle(pipe) };
        let mut len = [0u8; 2];
        while file.read_exact(&mut len).is_ok() {
            let mut body = vec![0u8; u16::from_le_bytes(len) as usize];
            if file.read_exact(&mut body).is_err() {
                break;
            }
            let Ok(msg) = postcard::from_bytes::<HelperMsg>(&body) else {
                continue;
            };
            // 切到当前的输入桌面（锁屏界面在 Winlogon 桌面上），后面判断窗口也要在这个桌面上做
            let input = unsafe { OpenInputDesktop(0, 0, GENERIC_ALL) };
            if !input.is_null() {
                if unsafe { SetThreadDesktop(input) } != 0 {
                    if !desktop.is_null() {
                        unsafe { CloseDesktop(desktop) };
                    }
                    desktop = input;
                } else {
                    unsafe { CloseDesktop(input) };
                }
            }
            // 只在锁屏时、或者（用户打开了开关）目标是管理员程序时注入；其余由被控端自己注入，
            // UAC 弹窗一律不处理
            if !session_locked(session) && !elevated_target_allowed() {
                if matches!(msg, HelperMsg::ReleaseAll) {
                    injector.release_all();
                }
                continue;
            }
            match msg {
                HelperMsg::MoveTo { x, y } => injector.move_to(x, y),
                HelperMsg::Key { usage, down } => injector.key(usage, down),
                HelperMsg::Button { button, down } => injector.button(button, down),
                HelperMsg::WheelLines { dx, dy } => injector.wheel_lines(dx, dy),
                HelperMsg::WheelPixels { dx, dy } => injector.wheel_pixels(dx, dy),
                HelperMsg::ReleaseAll => injector.release_all(),
            }
        }
        injector.release_all();
        // File 在这里关闭句柄
        drop(file);
    }
}

// ---------------------------------------------------------------- 被控端：转交给助手

/// 当前输入桌面是不是安全桌面（锁屏 / UAC）。普通程序打不开安全桌面，以此判断。
pub fn secure_desktop_active() -> bool {
    let desk = unsafe { OpenInputDesktop(0, 0, 0x0100) }; // DESKTOP_READOBJECTS
    if desk.is_null() {
        true
    } else {
        unsafe { CloseDesktop(desk) };
        false
    }
}

/// 被控端到输入助手的连接（按需连接，断了自动重连）。
pub struct HelperLink {
    pipe: Option<File>,
    last_attempt: Option<Instant>,
}

impl Default for HelperLink {
    fn default() -> Self {
        Self::new()
    }
}

impl HelperLink {
    pub fn new() -> Self {
        Self {
            pipe: None,
            last_attempt: None,
        }
    }

    pub fn send(&mut self, msg: &HelperMsg) -> bool {
        if self.pipe.is_none() {
            // 助手不在时不要每个事件都去尝试连接
            if self
                .last_attempt
                .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
            {
                return false;
            }
            self.last_attempt = Some(Instant::now());
            self.pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(pipe_name(current_session()))
                .ok();
        }
        let Some(pipe) = &mut self.pipe else {
            return false;
        };
        let Ok(body) = postcard::to_stdvec(msg) else {
            return false;
        };
        let mut frame = (body.len() as u16).to_le_bytes().to_vec();
        frame.extend_from_slice(&body);
        if pipe.write_all(&frame).is_err() {
            self.pipe = None;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 列出所有窗口程序的完整性级别（只读，手动运行）：
    /// `cargo test -p roam integrity_of_running_windows -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn integrity_of_running_windows() {
        let me = integrity_level(unsafe { GetCurrentProcessId() });
        println!("本进程: {me:x?}");
        for p in std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-Process | Where-Object MainWindowHandle -ne 0 | ForEach-Object Id",
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
        {
            println!("{:>24} {:x?}", process_name(p), integrity_level(p));
        }
        println!("允许控制管理员程序: {}", allow_elevated());
    }
}
