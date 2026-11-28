//! Diagnostics C ABI: the process-wide log export and the one-call presenter
//! snapshot.
//!
//! New diagnostic data is returned as JSON documents instead of new fields on
//! existing structs. Apple hosts mirror the C structs by hand, so growing a
//! struct would require a lock-step dylib/plugin upgrade; a new optional symbol
//! returning JSON degrades cleanly when an older library lacks it.

use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, Location, catch_unwind};
use std::ptr;

use erika::diagnostics::{LogLevel, LogQuery, export_logs};

use super::*;

/// Log ring severities for `erika_log_read_json`'s `min_level`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaLogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

fn json_to_c(value: &serde_json::Value, out_json: *mut *mut c_char) -> ErikaStatus {
    match CString::new(value.to_string()) {
        Ok(text) => {
            unsafe { *out_json = text.into_raw() };
            ErikaStatus::Ok
        }
        Err(_) => player_error("diagnostics JSON contains an interior NUL"),
    }
}

pub(crate) fn log_query(limit: u64, min_level: Option<LogLevel>, since_seq: u64) -> LogQuery {
    LogQuery {
        limit: usize::try_from(limit).unwrap_or(usize::MAX),
        min_level,
        since_seq,
    }
}

/// Exports the process-wide log ring as JSON. Works without any handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_log_read_json(
    limit: u32,
    min_level: i32,
    since_seq: u64,
    out_json: *mut *mut c_char,
) -> ErikaStatus {
    if out_json.is_null() {
        set_last_error("log output pointer is null");
        return ErikaStatus::NullPointer;
    }
    unsafe { *out_json = ptr::null_mut() };
    let min_level = if min_level < 0 {
        None
    } else {
        match LogLevel::from_raw(min_level) {
            Some(level) => Some(level),
            None => {
                return finalize_status(player_error(format!(
                    "unsupported log level {min_level}; use 0..=4 or -1 for every level"
                )));
            }
        }
    };
    match catch_unwind(AssertUnwindSafe(|| {
        let exported = export_logs(&log_query(u64::from(limit), min_level, since_seq));
        json_to_c(&exported, out_json)
    })) {
        Ok(status) => finalize_status(status),
        Err(payload) => {
            set_last_error(report_capi_panic(
                "Erika log export",
                Location::caller(),
                payload,
            ));
            ErikaStatus::Panic
        }
    }
}

/// Writes a presenter's diagnostics snapshot (schema 1) as JSON.
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_diagnostics_json(
    handle: *mut ErikaPresenterHandle,
    out_json: *mut *mut c_char,
) -> ErikaStatus {
    if out_json.is_null() {
        set_last_error("diagnostics output pointer is null");
        return ErikaStatus::NullPointer;
    }
    unsafe { *out_json = ptr::null_mut() };
    with_presenter_mut(handle, |handle| {
        json_to_c(&handle.presenter.diagnostics_snapshot(), out_json)
    })
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_diagnostics_json(
    _handle: *mut std::ffi::c_void,
    out_json: *mut *mut c_char,
) -> ErikaStatus {
    if out_json.is_null() {
        return ErikaStatus::NullPointer;
    }
    unsafe { *out_json = ptr::null_mut() };
    finalize_status(player_error(
        "ErikaPresenterHandle is not available on this target",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    fn take_json(pointer: *mut c_char) -> serde_json::Value {
        assert!(!pointer.is_null());
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { erika_string_free(pointer) };
        serde_json::from_str(&text).unwrap()
    }

    fn last_error_text() -> String {
        let pointer = erika_last_error_message();
        assert!(!pointer.is_null());
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        unsafe { erika_string_free(pointer) };
        text
    }

    #[test]
    fn log_read_validates_arguments() {
        assert_eq!(
            unsafe { erika_log_read_json(10, -1, 0, ptr::null_mut()) },
            ErikaStatus::NullPointer
        );
        let mut out = ptr::null_mut();
        assert_eq!(
            unsafe { erika_log_read_json(10, 9, 0, &mut out) },
            ErikaStatus::PlayerError
        );
        assert!(out.is_null());
        assert!(last_error_text().contains("unsupported log level 9"));
        assert_eq!(ErikaLogLevel::Warn as i32, LogLevel::Warn as i32);
        assert_eq!(ErikaLogLevel::Error as i32, LogLevel::Error as i32);
    }

    #[test]
    fn log_read_returns_a_filtered_redacted_document() {
        // Any diagnostic reaches the ring; a structured warning is enough.
        let marker = "capi-log-marker-91aa";
        let mut out = ptr::null_mut();
        assert_eq!(
            unsafe { erika_log_read_json(0, ErikaLogLevel::Warn as i32, 0, &mut out) },
            ErikaStatus::Ok
        );
        let before = take_json(out);
        assert_eq!(before["schema"], 1);
        assert_eq!(before["redacted"], true);
        assert!(
            before["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["level"] == "warn" || entry["level"] == "error")
        );
        let since = before["nextSeq"].as_u64().unwrap();
        // A failed open of a missing file emits an operational diagnostic
        // through the ring with the path redacted on export.
        let handle = erika_create();
        let uri = CString::new(format!("/Users/someone/{marker}.mkv?token=abc")).unwrap();
        let _ = unsafe { erika_open(handle, uri.as_ptr()) };
        unsafe { erika_destroy(handle) };
        let mut out = ptr::null_mut();
        assert_eq!(
            unsafe { erika_log_read_json(50, -1, since, &mut out) },
            ErikaStatus::Ok
        );
        let after = take_json(out);
        for entry in after["entries"].as_array().unwrap() {
            assert!(entry["seq"].as_u64().unwrap() >= since);
            let text = entry.to_string();
            assert!(!text.contains("/Users/someone"), "{text}");
            assert!(!text.contains("token=abc"), "{text}");
        }
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn presenter_diagnostics_json_has_the_documented_sections() {
        let mut out = ptr::null_mut();
        assert_eq!(
            unsafe { erika_presenter_get_diagnostics_json(ptr::null_mut(), &mut out) },
            ErikaStatus::NullPointer
        );
        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        assert_eq!(
            unsafe { erika_presenter_get_diagnostics_json(handle, ptr::null_mut()) },
            ErikaStatus::NullPointer
        );
        assert_eq!(
            unsafe { erika_presenter_get_diagnostics_json(handle, &mut out) },
            ErikaStatus::Ok
        );
        let snapshot = take_json(out);
        assert_eq!(snapshot["schema"], 1);
        for section in [
            "playback",
            "media",
            "decoder",
            "video",
            "presenter",
            "output",
            "hdr",
            "resources",
            "upscaler",
            "audio",
            "buffers",
            "timing",
        ] {
            assert!(snapshot.get(section).is_some(), "missing {section}");
        }
        for timing in ["scheduler", "presentation", "clock"] {
            assert!(
                snapshot["timing"][timing].is_object(),
                "missing timing.{timing}"
            );
        }
        unsafe { erika_presenter_destroy(handle) };
    }
}
