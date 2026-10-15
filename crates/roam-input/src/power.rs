//! 别让系统把 Roam 当成闲置的后台程序。
//!
//! Roam 平时只有菜单栏 / 托盘图标，窗口隐藏。macOS 的 App Nap、Windows 的后台节能（EcoQoS）
//! 会推迟这类程序的定时器、降低优先级，网络保活跟着被推迟，表现为控制间歇性失灵。
//! 另外光标在别的电脑上时，本机的键鼠事件都被 Roam 拦下了，系统可能以为没人在用而息屏、睡眠。

/// 进程启动时调用一次。
pub fn keep_responsive() {
    #[cfg(target_os = "macos")]
    mac::keep_responsive();
    #[cfg(windows)]
    win::keep_responsive();
}

/// 正在用本机键鼠控制别的电脑：告诉系统用户是活跃的，别息屏、睡眠。
/// 每次调用都会重置系统的空闲计时，调用方几秒一次即可。
pub fn declare_user_active() {
    #[cfg(target_os = "macos")]
    mac::declare_user_active();
    #[cfg(windows)]
    win::declare_user_active();
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::atomic::{AtomicU32, Ordering};

    use core_foundation::{
        base::TCFType,
        string::{CFString, CFStringRef},
    };
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionDeclareUserActivity(
            name: CFStringRef,
            user_type: u32,
            assertion: *mut u32,
        ) -> i32;
    }

    /// kIOPMUserActiveLocal：本地用户在操作。
    const USER_ACTIVE_LOCAL: u32 = 0;

    static ASSERTION: AtomicU32 = AtomicU32::new(0);

    pub fn keep_responsive() {
        // 关掉 App Nap，但允许系统在真正空闲时睡眠
        let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::LatencyCritical;
        let reason = NSString::from_str("Roam 键鼠共享需要低延迟");
        let activity =
            NSProcessInfo::processInfo().beginActivityWithOptions_reason(options, &reason);
        // 在整个进程生命周期内有效
        std::mem::forget(activity);
        tracing::info!("已关闭 App Nap");
    }

    pub fn declare_user_active() {
        let name = CFString::new("Roam：正在用本机键鼠控制其他电脑");
        // 复用同一个断言，系统只会刷新它的时间
        let mut id = ASSERTION.load(Ordering::Relaxed);
        let result = unsafe {
            IOPMAssertionDeclareUserActivity(name.as_concrete_TypeRef(), USER_ACTIVE_LOCAL, &mut id)
        };
        if result != 0 {
            tracing::debug!("IOPMAssertionDeclareUserActivity 失败: {result:#x}");
        }
        ASSERTION.store(id, Ordering::Relaxed);
    }
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::System::{
        Power::{ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState},
        Threading::{
            GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            ProcessPowerThrottling, SetProcessInformation,
        },
    };

    pub fn keep_responsive() {
        // ControlMask 里的项由我们自己决定，StateMask 为 0 表示全部不节流
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let ok = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if ok == 0 {
            tracing::debug!("关闭后台节能失败: {}", std::io::Error::last_os_error());
        } else {
            tracing::info!("已关闭后台节能（EcoQoS）");
        }
    }

    pub fn declare_user_active() {
        // 不带 ES_CONTINUOUS：只重置一次空闲计时
        unsafe { SetThreadExecutionState(ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED) };
    }
}
