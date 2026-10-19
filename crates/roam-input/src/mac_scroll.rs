//! 本机滚动过滤。按系统提供的离散/连续滚动分类，普通滚轮与触摸板分别反转。
//! 连续滚动鼠标（如 Magic Mouse）也属于连续组，不能仅凭该事件字段识别硬件型号。

use core_foundation::base::CFRelease;
use core_graphics::event::{CGEvent, EventField};
use foreign_types::ForeignType;
use roam_core::settings::MacScrollSettings;
use std::{
    ffi::c_void,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

static MOUSE: AtomicBool = AtomicBool::new(false);
static TRACKPAD: AtomicBool = AtomicBool::new(false);
static LAST: AtomicU8 = AtomicU8::new(0);
static STATUS: Mutex<Result<(), String>> = Mutex::new(Err(String::new()));

pub fn configure(settings: MacScrollSettings) {
    MOUSE.store(settings.reverse_mouse, Ordering::Relaxed);
    TRACKPAD.store(settings.reverse_trackpad, Ordering::Relaxed);
}

pub fn set_status(status: Result<(), String>) {
    *STATUS.lock().unwrap() = status;
}
pub fn status() -> (bool, Option<String>, &'static str) {
    let state = STATUS.lock().unwrap();
    (
        state.is_ok(),
        state.as_ref().err().filter(|s| !s.is_empty()).cloned(),
        match LAST.load(Ordering::Relaxed) {
            1 => "mouse",
            2 => "continuous",
            _ => "none",
        },
    )
}

pub fn transform(event: &CGEvent) {
    let continuous =
        event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0;
    LAST.store(if continuous { 2 } else { 1 }, Ordering::Relaxed);
    reverse(
        event,
        MacScrollSettings {
            reverse_mouse: MOUSE.load(Ordering::Relaxed),
            reverse_trackpad: TRACKPAD.load(Ordering::Relaxed),
        },
    );
}

fn reverse(event: &CGEvent, settings: MacScrollSettings) {
    if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
        == crate::mac::INJECTED_MARK
    {
        return;
    }
    let continuous =
        event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0;
    if !(if continuous {
        settings.reverse_trackpad
    } else {
        settings.reverse_mouse
    }) {
        return;
    }
    // 先保存各字段；写入离散 delta 时 macOS 会同步修改另外两种单位的值。
    let axes = [(11, 93, 96), (12, 94, 97)];
    let values = axes.map(|(line, fixed, point)| {
        (
            event.get_integer_value_field(line),
            event.get_double_value_field(fixed),
            event.get_integer_value_field(point),
        )
    });
    reverse_hid(event);
    for ((line, fixed, point), (a, b, c)) in axes.into_iter().zip(values) {
        event.set_integer_value_field(line, a.saturating_neg());
        event.set_double_value_field(fixed, -b);
        event.set_integer_value_field(point, c.saturating_neg());
    }
}

// 新版应用可能直接读取关联的 IOHID 滚动值；可选 SPI 不存在时保留 CGEvent 路径。
type CopyHid = unsafe extern "C" fn(*const c_void) -> *const c_void;
type GetFloat = unsafe extern "C" fn(*const c_void, u32) -> f64;
type SetFloat = unsafe extern "C" fn(*const c_void, u32, f64);

fn reverse_hid(event: &CGEvent) {
    static API: OnceLock<Option<(CopyHid, GetFloat, SetFloat)>> = OnceLock::new();
    let Some((copy, get, set)) = API.get_or_init(|| {
        Some(unsafe {
            (
                std::mem::transmute::<*mut c_void, CopyHid>(crate::mac::dlsym(
                    c"CGEventCopyIOHIDEvent",
                )?),
                std::mem::transmute::<*mut c_void, GetFloat>(crate::mac::dlsym(
                    c"IOHIDEventGetFloatValue",
                )?),
                std::mem::transmute::<*mut c_void, SetFloat>(crate::mac::dlsym(
                    c"IOHIDEventSetFloatValue",
                )?),
            )
        })
    }) else {
        return;
    };
    unsafe {
        let hid = copy(event.as_ptr().cast());
        if !hid.is_null() {
            // kIOHIDEventTypeScroll = 6；X/Y 字段依次位于该类型的首两个字段。
            for field in [6 << 16, (6 << 16) + 1] {
                set(hid, field, -get(hid, field));
            }
            CFRelease(hid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::{
        event::ScrollEventUnit,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    #[test]
    fn groups_and_injected_events_are_independent() {
        for continuous in [false, true] {
            for mouse in [false, true] {
                for pad in [false, true] {
                    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
                    let event =
                        CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, 2, 16, -8, 0)
                            .unwrap();
                    event.set_integer_value_field(
                        EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS,
                        continuous as i64,
                    );
                    let before = event.get_integer_value_field(96);
                    reverse(
                        &event,
                        MacScrollSettings {
                            reverse_mouse: mouse,
                            reverse_trackpad: pad,
                        },
                    );
                    assert_eq!(
                        event.get_integer_value_field(96),
                        if if continuous { pad } else { mouse } {
                            -before
                        } else {
                            before
                        }
                    );
                    event.set_integer_value_field(
                        EventField::EVENT_SOURCE_USER_DATA,
                        crate::mac::INJECTED_MARK,
                    );
                    let before = event.get_integer_value_field(96);
                    reverse(
                        &event,
                        MacScrollSettings {
                            reverse_mouse: true,
                            reverse_trackpad: true,
                        },
                    );
                    assert_eq!(event.get_integer_value_field(96), before);
                }
            }
        }
    }
}
