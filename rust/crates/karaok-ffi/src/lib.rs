//! Narrow, panic-safe, versioned C ABI bridging the Rust core to macOS SwiftUI (§4.3, ADR-0001).
//!
//! Safety invariants:
//! - Every `extern "C"` function wraps its body in `std::panic::catch_unwind`.
//! - Opaque handles own `Arc<AppController>` cleanly with explicit destroy.
//! - Real-time snapshot reads (`karaok_controller_read_transport` / `read_meters`) perform zero JSON
//!   formatting and zero heap allocations.

use karaok_controller::{AppController, ControllerCommand, ControllerResponse};
use karaok_library::StoragePaths;
use karaok_types::{ErrorCode, KaraokError, FFI_ABI_VERSION};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::Arc;

pub struct KaraokControllerHandle {
    inner: Arc<AppController>,
}

#[repr(C)]
pub struct KaraokTransportSnapshotC {
    pub generation: u64,
    pub state: u8,
    pub presentation_quality: u8,
    pub using_stems: u8,
    pub stems_available: u8,
    pub source_position_s: f64,
    pub duration_s: f64,
    pub monotonic_host_ns: u64,
    pub playback_ratio: f64,
    pub presentation_latency_ms: f64,
}

#[repr(C)]
pub struct KaraokMeterSnapshotC {
    pub input_pre_peak: f32,
    pub input_pre_rms: f32,
    pub input_clipped: u8,
    pub output_clipped: u8,
    pub voiced: u8,
    pub active_mic_capture: u8,
    pub voice_post_peak: f32,
    pub voice_post_rms: f32,
    pub output_peak: f32,
    pub detected_f0_hz: f32,
    pub target_f0_hz: f32,
    pub pitch_confidence: f32,
    pub pitch_correction_cents: f32,
}

fn to_c_string_ptr(s: String) -> *mut c_char {
    let clean = s.replace('\0', " ");
    CString::new(clean)
        .map(CString::into_raw)
        .unwrap_or(null_mut())
}

fn response_error_ptr(code: ErrorCode, msg: &str) -> *mut c_char {
    let resp = ControllerResponse::Error {
        error: KaraokError::new(code, msg),
    };
    to_c_string_ptr(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string()))
}

#[no_mangle]
pub extern "C" fn karaok_abi_version() -> u32 {
    FFI_ABI_VERSION
}

/// Creates an opaque `KaraokControllerHandle`.
///
/// # Safety
/// - `app_support_dir` and `cache_dir` must either both be null or point to valid NUL-terminated UTF-8 C strings.
/// - `out_error_json`, if non-null, must point to writable memory for a `*mut c_char`.
/// - Returned error string (if any) must be freed with `karaok_string_free`, and returned handle with `karaok_controller_destroy`.
#[no_mangle]
pub unsafe extern "C" fn karaok_controller_create(
    app_support_dir: *const c_char,
    cache_dir: *const c_char,
    out_error_json: *mut *mut c_char,
) -> *mut KaraokControllerHandle {
    if !out_error_json.is_null() {
        *out_error_json = null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let paths = if !app_support_dir.is_null() && !cache_dir.is_null() {
            let app_s = CStr::from_ptr(app_support_dir).to_string_lossy().into_owned();
            let cache_s = CStr::from_ptr(cache_dir).to_string_lossy().into_owned();
            StoragePaths::from_roots(PathBuf::from(app_s), PathBuf::from(cache_s))
        } else {
            StoragePaths::macos_default()
        };

        let paths = match paths {
            Ok(p) => p,
            Err(e) => return Err(e),
        };

        #[cfg(target_os = "macos")]
        let hal = karaok_audio_device::default_system_backend();
        #[cfg(not(target_os = "macos"))]
        let hal = Arc::new(karaok_audio_device::SimulatedHalBackend::new(
            karaok_audio_device::SimulatedHalBackend::fixture_with_nothing_usb_and_builtin(),
        ));

        AppController::new(paths, hal)
    }));

    match result {
        Ok(Ok(ctrl)) => Box::into_raw(Box::new(KaraokControllerHandle { inner: ctrl })),
        Ok(Err(err)) => {
            if !out_error_json.is_null() {
                *out_error_json = to_c_string_ptr(
                    serde_json::to_string(&err).unwrap_or_else(|_| "{}".to_string()),
                );
            }
            null_mut()
        }
        Err(_) => {
            if !out_error_json.is_null() {
                // Bare KaraokError JSON matching Ok(Err) branch (Nit 1)
                let err = KaraokError::new(
                    ErrorCode::InternalError,
                    "Panique interceptée lors de l'initialisation FFI.",
                );
                *out_error_json = to_c_string_ptr(
                    serde_json::to_string(&err).unwrap_or_else(|_| "{}".to_string()),
                );
            }
            null_mut()
        }
    }
}

/// Destroys a handle created by `karaok_controller_create`. Safe to call with null.
///
/// # Safety
/// - `handle` must be null or a valid pointer previously returned by `karaok_controller_create` and not yet freed.
/// - No other thread may concurrently access `handle` once destruction begins.
#[no_mangle]
pub unsafe extern "C" fn karaok_controller_destroy(handle: *mut KaraokControllerHandle) {
    if handle.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let boxed = Box::from_raw(handle);
        // Explicitly cancel workers, stop streams, and join hotplug thread cleanly before dropping Arc (B5)
        boxed.inner.shutdown();
        drop(boxed);
    }));
}

/// Dispatches a JSON `ControllerCommand` and returns a newly allocated UTF-8 JSON `ControllerResponse`.
///
/// # Safety
/// - `handle` must be a valid pointer from `karaok_controller_create` (or null, returning a structured error).
/// - `command_json` must be null or point to a valid NUL-terminated UTF-8 C string.
/// - Caller must free the returned pointer with `karaok_string_free`.
#[no_mangle]
pub unsafe extern "C" fn karaok_controller_dispatch_json(
    handle: *mut KaraokControllerHandle,
    command_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() || command_json.is_null() {
        return response_error_ptr(ErrorCode::InvalidParameter, "Pointeur FFI nul.");
    }
    let res = catch_unwind(AssertUnwindSafe(|| {
        let ctrl = &(*handle).inner;
        let raw = CStr::from_ptr(command_json).to_string_lossy();
        match serde_json::from_str::<ControllerCommand>(&raw) {
            Ok(cmd) => {
                let response = ctrl.dispatch(cmd);
                to_c_string_ptr(
                    serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string()),
                )
            }
            Err(e) => response_error_ptr(
                ErrorCode::InvalidParameter,
                &format!("Commande JSON invalide : {e}"),
            ),
        }
    }));

    match res {
        Ok(ptr) => ptr,
        Err(_) => response_error_ptr(
            ErrorCode::InternalError,
            "Panique Rust interceptée à la frontière FFI.",
        ),
    }
}

/// Drains queued `AppEvent` entries as a newly allocated UTF-8 JSON array.
///
/// # Safety
/// - `handle` must be null or a valid pointer from `karaok_controller_create`.
/// - Caller must free the returned pointer with `karaok_string_free`.
#[no_mangle]
pub unsafe extern "C" fn karaok_controller_poll_events_json(
    handle: *mut KaraokControllerHandle,
) -> *mut c_char {
    if handle.is_null() {
        return to_c_string_ptr("[]".to_string());
    }
    let res = catch_unwind(AssertUnwindSafe(|| {
        let events = (*handle).inner.poll_events();
        to_c_string_ptr(serde_json::to_string(&events).unwrap_or_else(|_| "[]".to_string()))
    }));
    res.unwrap_or_else(|_| to_c_string_ptr("[]".to_string()))
}

/// Reads the latest lock-free `KaraokTransportSnapshotC` from the real-time engine.
///
/// # Safety
/// - `handle` must be null or a valid pointer from `karaok_controller_create`.
/// - `out_snapshot` must be null or point to valid writable memory for `KaraokTransportSnapshotC`.
#[no_mangle]
pub unsafe extern "C" fn karaok_controller_read_transport(
    handle: *mut KaraokControllerHandle,
    out_snapshot: *mut KaraokTransportSnapshotC,
) -> i32 {
    if handle.is_null() || out_snapshot.is_null() {
        return 0;
    }
    let res = catch_unwind(AssertUnwindSafe(|| {
        let s = (*handle).inner.read_transport_snapshot();
        *out_snapshot = KaraokTransportSnapshotC {
            generation: s.generation,
            state: s.state as u8,
            presentation_quality: s.quality as u8,
            using_stems: u8::from(s.using_stems),
            stems_available: u8::from(s.stems_available),
            source_position_s: s.source_position_s,
            duration_s: s.duration_s,
            monotonic_host_ns: s.monotonic_host_ns,
            playback_ratio: s.playback_ratio,
            presentation_latency_ms: s.presentation_latency_ms,
        };
        1
    }));
    res.unwrap_or(0)
}

/// Reads the latest lock-free `KaraokMeterSnapshotC` from the real-time engine.
///
/// # Safety
/// - `handle` must be null or a valid pointer from `karaok_controller_create`.
/// - `out_snapshot` must be null or point to valid writable memory for `KaraokMeterSnapshotC`.
#[no_mangle]
pub unsafe extern "C" fn karaok_controller_read_meters(
    handle: *mut KaraokControllerHandle,
    out_snapshot: *mut KaraokMeterSnapshotC,
) -> i32 {
    if handle.is_null() || out_snapshot.is_null() {
        return 0;
    }
    let res = catch_unwind(AssertUnwindSafe(|| {
        let m = (*handle).inner.read_meter_snapshot();
        *out_snapshot = KaraokMeterSnapshotC {
            input_pre_peak: m.input_pre_peak,
            input_pre_rms: m.input_pre_rms,
            input_clipped: u8::from(m.input_clipped),
            output_clipped: u8::from(m.output_clipped),
            voiced: u8::from(m.voiced),
            active_mic_capture: u8::from(m.active_mic_capture),
            voice_post_peak: m.voice_post_peak,
            voice_post_rms: m.voice_post_rms,
            output_peak: m.output_peak,
            detected_f0_hz: m.detected_f0_hz,
            target_f0_hz: m.target_f0_hz,
            pitch_confidence: m.pitch_confidence,
            pitch_correction_cents: m.pitch_correction_cents,
        };
        1
    }));
    res.unwrap_or(0)
}

/// Frees a C string previously allocated and returned by `karaok-ffi`.
///
/// # Safety
/// - `ptr` must be null or a pointer previously returned by `karaok_controller_*` and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn karaok_string_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(CString::from_raw(ptr));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null;
    use tempfile::tempdir;

    #[test]
    fn ffi_abi_lifecycle_null_safety_and_json_dispatch() {
        assert_eq!(karaok_abi_version(), FFI_ABI_VERSION);

        // Null safety checks
        unsafe {
            karaok_controller_destroy(null_mut());
            karaok_string_free(null_mut());
            let mut ts = std::mem::zeroed::<KaraokTransportSnapshotC>();
            assert_eq!(karaok_controller_read_transport(null_mut(), &mut ts), 0);
            let err_ptr = karaok_controller_dispatch_json(null_mut(), null());
            assert!(!err_ptr.is_null());
            let err_str = CStr::from_ptr(err_ptr).to_string_lossy().into_owned();
            karaok_string_free(err_ptr);
            assert!(err_str.contains("invalid_parameter"));
        }

        // Full isolated lifecycle test against real macOS Core Audio HAL + temp SQLite
        let dir = tempdir().unwrap();
        let app_c = CString::new(dir.path().join("app").to_str().unwrap()).unwrap();
        let cache_c = CString::new(dir.path().join("cache").to_str().unwrap()).unwrap();
        let mut err_out: *mut c_char = null_mut();

        unsafe {
            let handle = karaok_controller_create(app_c.as_ptr(), cache_c.as_ptr(), &mut err_out);
            assert!(err_out.is_null());
            assert!(!handle.is_null());

            // Dispatch valid command
            let cmd = CString::new(r#"{"cmd":"list_tracks","search":null,"favorites_only":false}"#).unwrap();
            let resp_ptr = karaok_controller_dispatch_json(handle, cmd.as_ptr());
            assert!(!resp_ptr.is_null());
            let resp_str = CStr::from_ptr(resp_ptr).to_string_lossy().into_owned();
            karaok_string_free(resp_ptr);
            assert!(resp_str.contains("\"ok\":\"success\""));

            // Dispatch malformed JSON -> clean structured error, no panic
            let bad = CString::new("{not valid json").unwrap();
            let bad_ptr = karaok_controller_dispatch_json(handle, bad.as_ptr());
            let bad_str = CStr::from_ptr(bad_ptr).to_string_lossy().into_owned();
            karaok_string_free(bad_ptr);
            assert!(bad_str.contains("invalid_parameter"));

            // Lock-free snapshots
            let mut ts = std::mem::zeroed::<KaraokTransportSnapshotC>();
            let mut ms = std::mem::zeroed::<KaraokMeterSnapshotC>();
            assert_eq!(karaok_controller_read_transport(handle, &mut ts), 1);
            assert_eq!(karaok_controller_read_meters(handle, &mut ms), 1);
            assert_eq!(ms.active_mic_capture, 0);

            karaok_controller_destroy(handle);
        }
    }
}
