use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::fs::File;
use std::num::NonZeroUsize;
use std::os::fd::FromRawFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Arc, Mutex, MutexGuard, Once, OnceLock};
use std::thread::{self, ThreadId};

use erika::source::{AndroidOwnedFdRegistration, register_android_owned_fd};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{
    JNI_ERR, JNI_VERSION_1_6, JavaVM, jboolean, jbyteArray, jdouble, jfloat, jint, jlong, jstring,
};
use ndk::native_window::NativeWindow;
use serde_json::{Map, Value, json};

use super::*;

struct AndroidPresenter {
    handle: *mut ErikaPresenterHandle,
    native_window: Option<NativeWindow>,
    latest_stats: ErikaPresenterStats,
}

/// Owns a boxed `AndroidPresenter` without making its non-`Send` fields part of
/// the process-wide registry's type. The address is only dereferenced or freed
/// by `with_registered_presenter`/`destroy_registered_presenter` after the
/// creator-thread check and while the corresponding entry mutex is held.
struct OwnedAndroidPresenterAddress {
    address: NonZeroUsize,
}

impl OwnedAndroidPresenterAddress {
    fn new(presenter: AndroidPresenter) -> Self {
        let address = Box::into_raw(Box::new(presenter)) as usize;
        Self {
            address: NonZeroUsize::new(address).expect("Box::into_raw returned a null address"),
        }
    }

    unsafe fn as_mut(&mut self) -> &mut AndroidPresenter {
        // SAFETY: the caller holds the entry mutex, has verified the creator
        // thread, and this owned address has not been taken for destruction.
        unsafe { &mut *(self.address.get() as *mut AndroidPresenter) }
    }

    unsafe fn destroy(self) {
        // SAFETY: `self` is taken exactly once from the entry state after that
        // entry has been removed from the registry and all earlier calls have
        // released the entry mutex.
        drop(unsafe { Box::from_raw(self.address.get() as *mut AndroidPresenter) });
    }
}

struct AndroidPresenterEntry {
    owner_thread: ThreadId,
    presenter: Mutex<Option<OwnedAndroidPresenterAddress>>,
}

impl AndroidPresenterEntry {
    fn new(presenter: AndroidPresenter) -> Self {
        Self {
            owner_thread: thread::current().id(),
            presenter: Mutex::new(Some(OwnedAndroidPresenterAddress::new(presenter))),
        }
    }

    fn ensure_owner_thread(&self, id: jlong, operation: &str) -> Result<(), PresenterIdError> {
        let current_thread = thread::current().id();
        if current_thread == self.owner_thread {
            return Ok(());
        }
        Err(PresenterIdError::WrongThread {
            id,
            operation: operation.to_string(),
            owner_thread: self.owner_thread,
            current_thread,
        })
    }
}

#[derive(Debug)]
struct PresenterIdAllocator {
    next_id: Option<jlong>,
}

impl Default for PresenterIdAllocator {
    fn default() -> Self {
        Self { next_id: Some(1) }
    }
}

impl PresenterIdAllocator {
    fn allocate(&mut self) -> Result<jlong, PresenterIdError> {
        let id = self.next_id.ok_or(PresenterIdError::IdSpaceExhausted)?;
        debug_assert!(id > 0);
        self.next_id = id.checked_add(1);
        Ok(id)
    }

    fn was_issued(&self, id: jlong) -> bool {
        id > 0 && self.next_id.is_none_or(|next_id| id < next_id)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PresenterIdError {
    Invalid(jlong),
    Unknown(jlong),
    AlreadyDestroyed(jlong),
    WrongThread {
        id: jlong,
        operation: String,
        owner_thread: ThreadId,
        current_thread: ThreadId,
    },
    IdSpaceExhausted,
}

impl PresenterIdError {
    fn kind(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_id",
            Self::Unknown(_) => "unknown_id",
            Self::AlreadyDestroyed(_) => "already_destroyed",
            Self::WrongThread { .. } => "wrong_thread",
            Self::IdSpaceExhausted => "id_space_exhausted",
        }
    }
}

impl std::fmt::Display for PresenterIdError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(id) => write!(
                formatter,
                "invalid Erika Android presenter ID {id}; IDs must be positive"
            ),
            Self::Unknown(id) => write!(formatter, "unknown Erika Android presenter ID {id}"),
            Self::AlreadyDestroyed(id) => write!(
                formatter,
                "Erika Android presenter ID {id} has already been destroyed"
            ),
            Self::WrongThread {
                id,
                operation,
                owner_thread,
                current_thread,
            } => write!(
                formatter,
                "Erika Android presenter ID {id} cannot run {operation} on thread {current_thread:?}; creator thread is {owner_thread:?}"
            ),
            Self::IdSpaceExhausted => {
                formatter.write_str("Erika Android presenter ID space is exhausted")
            }
        }
    }
}

#[derive(Default)]
struct PresenterRegistry {
    ids: PresenterIdAllocator,
    entries: HashMap<jlong, Arc<AndroidPresenterEntry>>,
}

impl PresenterRegistry {
    fn register(
        &mut self,
        entry: Arc<AndroidPresenterEntry>,
    ) -> Result<jlong, (PresenterIdError, Arc<AndroidPresenterEntry>)> {
        let id = match self.ids.allocate() {
            Ok(id) => id,
            Err(error) => return Err((error, entry)),
        };
        let previous = self.entries.insert(id, entry);
        debug_assert!(previous.is_none(), "monotonic presenter ID was reused");
        Ok(id)
    }

    fn get(&self, id: jlong) -> Result<Arc<AndroidPresenterEntry>, PresenterIdError> {
        self.validate_id(id)?;
        self.entries
            .get(&id)
            .cloned()
            .ok_or_else(|| self.missing_id_error(id))
    }

    fn remove_for_destroy(
        &mut self,
        id: jlong,
    ) -> Result<Arc<AndroidPresenterEntry>, PresenterIdError> {
        self.validate_id(id)?;
        let entry = self
            .entries
            .get(&id)
            .cloned()
            .ok_or_else(|| self.missing_id_error(id))?;
        entry.ensure_owner_thread(id, "destroy")?;
        Ok(self
            .entries
            .remove(&id)
            .expect("presenter entry disappeared while registry lock was held"))
    }

    fn validate_id(&self, id: jlong) -> Result<(), PresenterIdError> {
        if id <= 0 {
            Err(PresenterIdError::Invalid(id))
        } else {
            Ok(())
        }
    }

    fn missing_id_error(&self, id: jlong) -> PresenterIdError {
        if self.ids.was_issued(id) {
            PresenterIdError::AlreadyDestroyed(id)
        } else {
            PresenterIdError::Unknown(id)
        }
    }
}

fn presenter_registry() -> &'static Mutex<PresenterRegistry> {
    static REGISTRY: OnceLock<Mutex<PresenterRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(PresenterRegistry::default()))
}

struct OwnedFdCallGuard {
    fd: i32,
    file: Option<File>,
    _registration: Option<AndroidOwnedFdRegistration>,
}

impl OwnedFdCallGuard {
    unsafe fn from_transferred_fd(fd: i32) -> Self {
        Self {
            fd,
            // SAFETY: Kotlin detached this descriptor specifically for this JNI call.
            file: Some(unsafe { File::from_raw_fd(fd) }),
            _registration: None,
        }
    }

    fn validate_invocation(&self, method: &str, arguments: &Value) -> Result<(), String> {
        if !matches!(
            method,
            "open" | "addExternalSubtitle" | "loadDanmakuFile" | "addDanmakuTrackFile"
        ) {
            return Err(format!(
                "method {method} does not accept transferred fd {}",
                self.fd
            ));
        }
        let Some(uri) = arguments
            .as_object()
            .and_then(|args| args.get("uri"))
            .and_then(Value::as_str)
        else {
            return Err(format!(
                "method {method} is missing the URI for transferred fd {}",
                self.fd
            ));
        };
        let fd = owned_fd_from_uri(uri)
            .ok_or_else(|| format!("invalid owned fd URI for {method}: {uri}"))?;
        if fd != self.fd {
            return Err(format!(
                "owned fd URI mismatch for {method}: expected {} but received {fd}",
                self.fd
            ));
        }
        Ok(())
    }

    fn arm_for_source(&mut self, uri: &str) -> Result<(), String> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        let fd = owned_fd_from_uri(uri)
            .ok_or_else(|| format!("owned fd URI changed before transfer: {uri}"))?;
        if fd != self.fd {
            return Err(format!(
                "owned fd URI changed before transfer: expected {} but received {fd}",
                self.fd
            ));
        }
        self._registration = Some(
            register_android_owned_fd(file)
                .map_err(|error| format!("failed to register owned fd {}: {error}", self.fd))?,
        );
        Ok(())
    }
}

/// Registers Android's Java VM before any presenter or decoder can be created.
/// FFmpeg's MediaCodec backend attaches its playback worker threads through
/// this pointer; returning JNI_ERR prevents a misleading software-only load.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn JNI_OnLoad(java_vm: *mut JavaVM, _reserved: *mut c_void) -> jint {
    install_android_panic_hook();
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        erika::ffmpeg::register_android_java_vm(java_vm.cast::<c_void>())
    })) {
        Ok(Ok(())) => JNI_VERSION_1_6,
        Ok(Err(error)) => {
            android_jni_log_error(&format!(
                "JNI_OnLoad failed to register FFmpeg JavaVM: {error}"
            ));
            JNI_ERR
        }
        Err(_) => {
            android_jni_log_error("panic while registering FFmpeg JavaVM in JNI_OnLoad");
            JNI_ERR
        }
    }
}

impl AndroidPresenter {
    fn new(handle: *mut ErikaPresenterHandle) -> Self {
        Self {
            handle,
            native_window: None,
            latest_stats: ErikaPresenterStats::default(),
        }
    }

    unsafe fn detach_surface(&mut self) -> ErikaStatus {
        let status = unsafe { erika_presenter_detach_surface(self.handle) };
        if matches!(status, ErikaStatus::Ok) {
            self.native_window = None;
        }
        status
    }
}

impl Drop for AndroidPresenter {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            if self.native_window.is_some() {
                let _ = unsafe { self.detach_surface() };
            }
            unsafe { erika_presenter_destroy(self.handle) };
            self.handle = ptr::null_mut();
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeCreate(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    output_mode: jint,
    edr_headroom: jfloat,
    upscaler: jint,
    video_alpha_mode: jint,
) -> jlong {
    catch_unwind(AssertUnwindSafe(|| {
        let handle = erika_presenter_create_with_config(ErikaPresenterConfig {
            output_mode,
            edr_headroom,
            luma_upscaler: upscaler,
            video_alpha_mode,
        });
        if handle.is_null() {
            let reason = last_error_message(ErikaStatus::PlayerError);
            android_jni_log_error(
                &json!({
                    "event": "presenter_create",
                    "stage": "failed",
                    "reason": reason,
                })
                .to_string(),
            );
            return 0;
        }
        let presenter = AndroidPresenter::new(handle);
        let entry = Arc::new(AndroidPresenterEntry::new(presenter));
        // Allocation and insertion are atomic under the registry lock. The ID
        // cannot be observed as missing by another JNI call before create
        // returns it to Kotlin.
        let registration = { lock_registry().register(entry) };
        match registration {
            Ok(id) => id,
            Err((error, entry)) => {
                let reason = error.to_string();
                set_last_error(&reason);
                log_presenter_registry_error(0, "create", &error);
                // Registration returned ownership on failure, so cleanup runs
                // after the global registry guard has been released.
                if let Err(cleanup_error) = destroy_presenter_entry(&entry, 0, "createCleanup") {
                    log_presenter_registry_error(0, "createCleanup", &cleanup_error);
                }
                0
            }
        }
    }))
    .unwrap_or_else(|_| {
        let reason = "panic while creating Erika Android presenter";
        set_last_error(reason);
        android_jni_log_error(
            &json!({
                "event": "presenter_create",
                "stage": "panic",
                "reason": reason,
            })
            .to_string(),
        );
        0
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeLastError(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    catch_unwind(AssertUnwindSafe(|| {
        new_java_string(&mut env, last_error_message(ErikaStatus::PlayerError))
    }))
    .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeDestroy(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    match catch_unwind(AssertUnwindSafe(|| destroy_registered_presenter(handle))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            set_last_error(error.to_string());
            log_presenter_registry_error(handle, "destroy", &error);
        }
        Err(payload) => {
            let reason = panic_payload_message(payload);
            set_last_error(format!(
                "panic while destroying Erika Android presenter ID {handle}: {reason}"
            ));
            android_jni_log_error(
                &json!({
                    "event": "presenter_registry",
                    "operation": "destroy",
                    "playerId": handle,
                    "errorKind": "panic",
                    "reason": reason,
                })
                .to_string(),
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeInvoke(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    method: JString<'_>,
    arguments_json: JString<'_>,
    transferred_fd: jint,
) -> jstring {
    let mut owned_fd = (transferred_fd >= 0)
        .then(|| unsafe { OwnedFdCallGuard::from_transferred_fd(transferred_fd) });
    let invalid_transferred_fd = transferred_fd < -1;
    let response = catch_unwind(AssertUnwindSafe(|| {
        if invalid_transferred_fd {
            return Err(format!("invalid transferred fd {transferred_fd}"));
        }
        let method: String = env
            .get_string(&method)
            .map(|value| value.into())
            .map_err(|error| format!("invalid method string: {error}"))?;
        let arguments_json: String = env
            .get_string(&arguments_json)
            .map(|value| value.into())
            .map_err(|error| format!("invalid arguments JSON string: {error}"))?;
        let arguments = if arguments_json.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&arguments_json)
                .map_err(|error| format!("invalid arguments JSON: {error}"))?
        };
        validate_owned_fd_invocation(&owned_fd, &method, &arguments)?;
        with_registered_presenter(handle, &method, |presenter| unsafe {
            invoke_presenter(presenter, &method, &arguments, &mut owned_fd)
        })
    }));

    let response = match response {
        Ok(Ok(value)) => success_response(value),
        Ok(Err(error)) => error_response(ErikaStatus::PlayerError, error),
        Err(_) => error_response(ErikaStatus::Panic, "panic in Erika Android JNI bridge"),
    };
    new_java_string(&mut env, response.to_string())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeRegisterSubtitleMemoryFont(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    data: JByteArray<'_>,
) -> jstring {
    let response = catch_unwind(AssertUnwindSafe(|| {
        let bytes = env
            .convert_byte_array(&data)
            .map_err(|error| format!("invalid subtitle memory font byte array: {error}"))?;
        with_registered_presenter(handle, "registerSubtitleMemoryFont", |presenter| {
            let mut font_id = 0;
            call_status(unsafe {
                erika_presenter_register_subtitle_memory_font(
                    presenter.handle,
                    bytes.as_ptr(),
                    bytes.len(),
                    &mut font_id,
                )
            })?;
            Ok(json!(font_id))
        })
    }));
    response_to_jstring(&mut env, response)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeAttachSurface(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    surface: JObject<'_>,
    width: jint,
    height: jint,
    scale: jdouble,
    extended_linear: jboolean,
    direct_composition: jboolean,
    desired_headroom: jfloat,
    fallback_reason: jint,
) -> jstring {
    let response = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "attachSurface", |presenter| {
            if surface.is_null() {
                return Err("Android Surface is null".to_string());
            }
            if presenter.native_window.is_some() {
                call_status(unsafe { presenter.detach_surface() })?;
            }
            let native_window =
                unsafe { NativeWindow::from_surface(env.get_native_interface(), surface.as_raw()) }
                    .ok_or_else(|| "ANativeWindow_fromSurface returned null".to_string())?;
            let scale = normalized_scale(scale);
            let status = unsafe {
                erika_presenter_attach_wgpu_surface_with_output_capabilities(
                    presenter.handle,
                    ErikaWgpuSurfaceKind::AndroidNativeWindow,
                    native_window.ptr().as_ptr() as usize as u64,
                    0,
                    physical_dimension(width),
                    physical_dimension(height),
                    scale,
                    ErikaSurfaceOutputCapabilities {
                        extended_linear: extended_linear != 0,
                        direct_composition: direct_composition != 0,
                        desired_headroom,
                        fallback_reason,
                    },
                )
            };
            call_status(status)?;
            presenter.native_window = Some(native_window);
            Ok(Value::Null)
        })
    }));
    response_to_jstring(&mut env, response)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeResizeSurface(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    width: jint,
    height: jint,
    scale: jdouble,
) -> jstring {
    let response = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "resizeSurface", |presenter| {
            let scale = normalized_scale(scale);
            call_status(unsafe {
                erika_presenter_resize_surface(
                    presenter.handle,
                    physical_dimension(width),
                    physical_dimension(height),
                    scale,
                )
            })?;
            Ok(Value::Null)
        })
    }));
    response_to_jstring(&mut env, response)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeDetachSurface(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    let response = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "detachSurface", |presenter| {
            if presenter.native_window.is_some() {
                call_status(unsafe { presenter.detach_surface() })?;
            }
            Ok(Value::Null)
        })
    }));
    response_to_jstring(&mut env, response)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeRenderTick(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    time_seconds: jdouble,
) -> jstring {
    let response = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "renderTick", |presenter| {
            let mut stats = ErikaPresenterStats::default();
            call_status(unsafe {
                erika_presenter_render_tick(presenter.handle, time_seconds, &mut stats)
            })?;
            presenter.latest_stats = stats;
            Ok(stats_to_json(stats))
        })
    }));
    response_to_jstring(&mut env, response)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeAudioOnlyTick(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    let response = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "audioOnlyTick", |presenter| {
            let mut stats = ErikaPresenterStats::default();
            call_status(unsafe { erika_presenter_audio_only_tick(presenter.handle, &mut stats) })?;
            presenter.latest_stats = stats;
            Ok(stats_to_json(stats))
        })
    }));
    response_to_jstring(&mut env, response)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativePollEvent(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    let response = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "pollEvent", |presenter| {
            let mut event = ErikaEvent::default();
            let status = unsafe { erika_presenter_poll_event(presenter.handle, &mut event) };
            if matches!(status, ErikaStatus::NoEvent) {
                return Ok(Value::Null);
            }
            call_status(status)?;
            let mut value = event_to_json(event);
            if matches!(event.kind, ErikaEventKind::Error) {
                if let Value::Object(map) = &mut value {
                    map.insert(
                        "error".to_string(),
                        Value::String(last_error_message(event.status)),
                    );
                }
            } else if matches!(event.kind, ErikaEventKind::VideoDecoderChanged) {
                if let Value::Object(map) = &mut value {
                    let message = last_error_message(event.status);
                    if let Ok(decoder) = serde_json::from_str::<Value>(&message) {
                        map.insert("decoder".to_string(), decoder);
                    }
                    map.insert("message".to_string(), Value::String(message));
                }
            } else if matches!(event.kind, ErikaEventKind::AudioOutputChanged)
                && let Value::Object(map) = &mut value
            {
                let message = last_error_message(event.status);
                if let Ok(audio) = serde_json::from_str::<Value>(&message) {
                    map.insert("audio".to_string(), audio);
                }
                map.insert("message".to_string(), Value::String(message));
            }
            if matches!(
                event.kind,
                ErikaEventKind::TracksChanged | ErikaEventKind::TrackSelectionChanged
            ) {
                if let Value::Object(map) = &mut value {
                    map.insert("trackList".to_string(), unsafe {
                        presenter_tracks_json(presenter.handle)?
                    });
                    map.insert("trackSelection".to_string(), unsafe {
                        presenter_track_selection_json(presenter.handle)?
                    });
                }
            }
            Ok(value)
        })
    }));
    response_to_jstring(&mut env, response)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativePlaybackState(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    let state = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "playbackState", |presenter| {
            let handle = unsafe { presenter.handle.as_ref() }
                .ok_or_else(|| "Android presenter handle is null".to_string())?;
            Ok(state_to_c(handle.presenter.player().state()) as jint)
        })
    }));
    match state {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            set_last_error(error);
            -1
        }
        Err(_) => {
            set_last_error("panic while reading Erika Android playback state");
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativePlaybackIntentState(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    let state = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "playbackIntentState", |presenter| {
            let handle = unsafe { presenter.handle.as_ref() }
                .ok_or_else(|| "Android presenter handle is null".to_string())?;
            Ok(state_to_c(handle.presenter.player().playback_intent_state()) as jint)
        })
    }));
    match state {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            set_last_error(error);
            -1
        }
        Err(_) => {
            set_last_error("panic while reading Erika Android playback intent state");
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aimesoft_erika_1flutter_ErikaNative_nativeCaptureFrame(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    width: jint,
    height: jint,
) -> jbyteArray {
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_registered_presenter(handle, "captureFrame", |presenter| {
            let width = physical_dimension(width);
            let height = physical_dimension(height);
            let len = (width as usize)
                .checked_mul(height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or_else(|| "Android screenshot dimensions overflowed usize".to_string())?;
            if len == 0 {
                return Err("Android screenshot dimensions must be non-zero".to_string());
            }
            let handle = unsafe { presenter.handle.as_mut() }
                .ok_or_else(|| "Android screenshot presenter handle is null".to_string())?;
            capture_presenter_frame_rgba(handle, width, height)
        })
    }));

    match result {
        Ok(Ok(Some(rgba))) => env
            .byte_array_from_slice(&rgba)
            .map(|array| array.into_raw())
            .unwrap_or(ptr::null_mut()),
        Ok(Ok(None)) => ptr::null_mut(),
        Ok(Err(error)) => {
            let _ = env.throw_new("java/lang/IllegalStateException", error);
            ptr::null_mut()
        }
        Err(_) => {
            let _ = env.throw_new(
                "java/lang/IllegalStateException",
                "panic in Erika Android screenshot capture",
            );
            ptr::null_mut()
        }
    }
}

unsafe fn invoke_presenter(
    presenter: &mut AndroidPresenter,
    method: &str,
    arguments: &Value,
    owned_fd: &mut Option<OwnedFdCallGuard>,
) -> Result<Value, String> {
    let args = arguments
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;
    let handle = presenter.handle;
    match method {
        "open" => {
            let uri = required_string(args, "uri")?;
            let uri_c = c_string(uri, "uri")?;
            arm_owned_fd_for_source(owned_fd, uri)?;
            let headers = c_http_headers(args)?;
            let options = ErikaOpenOptions {
                headers: headers.as_ptr(),
                header_count: headers.len(),
                http_read_ahead_bytes: optional_open_byte_count(args, "httpReadAheadBytes")?,
                http_back_buffer_bytes: optional_open_byte_count(args, "httpBackBufferBytes")?,
                reserved: [0; 2],
            };
            let status =
                unsafe { erika_presenter_open_with_options(handle, uri_c.as_ptr(), &options) };
            call_status(status)?;
            Ok(Value::Null)
        }
        "play" => status_value(unsafe { erika_presenter_play(handle) }),
        "pause" => status_value(unsafe { erika_presenter_pause(handle) }),
        "stop" => status_value(unsafe { erika_presenter_stop(handle) }),
        "close" => status_value(unsafe { erika_presenter_close(handle) }),
        "seek" => {
            let position = required_u64(args, "positionMicros")?;
            status_value(unsafe { erika_presenter_seek(handle, position) })
        }
        "setPlaybackRate" => {
            let rate = required_f64(args, "rate")?;
            status_value(unsafe { erika_presenter_set_playback_rate(handle, rate) })
        }
        "setVolume" => {
            let volume = required_f64(args, "volume")?;
            status_value(unsafe { erika_presenter_set_volume(handle, volume) })
        }
        "setUpscaler" => {
            let mode = required_i64(args, "mode")? as i32;
            status_value(unsafe { erika_presenter_set_upscaler(handle, mode) })
        }
        "setSubtitleScale" => {
            let scale = required_f64(args, "scale")?;
            status_value(unsafe { erika_presenter_set_subtitle_scale(handle, scale) })
        }
        "setSubtitleStyle" => {
            let family = optional_c_string(args, "fontFamily")?;
            let file_path = optional_c_string(args, "fontFilePath")?;
            let mut style = ErikaSubtitleStyle::default();
            style.font_family = optional_c_string_ptr(&family);
            style.font_file_path = optional_c_string_ptr(&file_path);
            update_u32(args, "primaryColorRgba", &mut style.primary_color_rgba);
            update_u32(args, "outlineColorRgba", &mut style.outline_color_rgba);
            update_f64(args, "fontSize", &mut style.font_size);
            update_f64(args, "outlineWidth", &mut style.outline_width);
            update_bool(args, "bold", &mut style.bold);
            update_bool(args, "italic", &mut style.italic);
            update_bool(args, "underline", &mut style.underline);
            update_bool(args, "strikeOut", &mut style.strike_out);
            update_f64(args, "spacing", &mut style.spacing);
            update_f64(args, "scaleXPercent", &mut style.scale_x_percent);
            update_f64(args, "scaleYPercent", &mut style.scale_y_percent);
            update_i32(args, "borderStyle", &mut style.border_style);
            update_f64(args, "shadowDepth", &mut style.shadow_depth);
            update_f64(args, "blur", &mut style.blur);
            update_i32(args, "alignment", &mut style.alignment);
            update_i32(args, "marginLeft", &mut style.margin_left);
            update_i32(args, "marginRight", &mut style.margin_right);
            update_i32(args, "marginVertical", &mut style.margin_vertical);
            update_u32(args, "overrideMask", &mut style.override_mask);
            call_status(unsafe { erika_presenter_set_subtitle_style(handle, style) })?;
            Ok(Value::Null)
        }
        "selectSubtitleMemoryFonts" => {
            let ids = required_u64_array(args, "fontIds")?;
            status_value(unsafe {
                erika_presenter_select_subtitle_memory_fonts(handle, ids.as_ptr(), ids.len())
            })
        }
        "clearSubtitleMemoryFonts" => {
            status_value(unsafe { erika_presenter_clear_subtitle_memory_fonts(handle) })
        }
        "getSubtitleMemoryFontStatus" => unsafe { subtitle_memory_font_status_json(handle) },
        "getSubtitleMemoryFontInfo" => {
            let font_id = required_u64(args, "fontId")?;
            unsafe { subtitle_memory_font_info_json(handle, font_id) }
        }
        "setOutputHeadroom" => {
            let headroom = required_f64(args, "headroom")? as f32;
            let known =
                optional_bool(args, "known").ok_or_else(|| "known is required".to_string())?;
            status_value(unsafe { erika_presenter_set_output_headroom(handle, headroom, known) })
        }
        "getUpscalerStatus" => {
            let mut status_value = ErikaUpscalerStatus::default();
            call_status(unsafe { erika_presenter_get_upscaler_status(handle, &mut status_value) })?;
            Ok(upscaler_status_to_json(status_value))
        }
        "getOutputStatus" => {
            let mut status_value = ErikaOutputStatus::default();
            call_status(unsafe { erika_presenter_get_output_status(handle, &mut status_value) })?;
            Ok(output_status_to_json(status_value))
        }
        "getPresenterStats" => Ok(stats_to_json(presenter.latest_stats)),
        "getResourceStatus" => {
            let mut status_value = ErikaPresenterResourceStatus::default();
            call_status(unsafe { erika_presenter_get_resource_status(handle, &mut status_value) })?;
            Ok(resource_status_to_json(status_value))
        }
        "setDebugHudEnabled" => {
            let enabled =
                optional_bool(args, "enabled").ok_or_else(|| "enabled is required".to_string())?;
            status_value(unsafe { erika_presenter_set_debug_hud_enabled(handle, enabled) })
        }
        "addExternalSubtitle" => {
            let uri = required_string(args, "uri")?;
            let uri_c = c_string(uri, "uri")?;
            let mut track_id = -1i64;
            arm_owned_fd_for_source(owned_fd, uri)?;
            let status = unsafe {
                erika_presenter_add_external_subtitle(handle, uri_c.as_ptr(), &mut track_id)
            };
            call_status(status)?;
            Ok(json!(track_id))
        }
        "removeSubtitleTrack" => {
            let track_id = required_i64(args, "trackId")?;
            status_value(unsafe { erika_presenter_remove_subtitle_track(handle, track_id) })
        }
        "selectAudioTrack" => {
            let track_id = optional_i64(args, "trackId").unwrap_or(-1);
            status_value(unsafe { erika_presenter_select_audio_track(handle, track_id) })
        }
        "selectSubtitleTrack" => {
            let track_id = optional_i64(args, "trackId").unwrap_or(-1);
            status_value(unsafe { erika_presenter_select_subtitle_track(handle, track_id) })
        }
        "tracks" => unsafe { presenter_tracks_json(handle) },
        "loadDanmakuFile" => {
            let uri = required_string(args, "uri")?;
            let uri_c = c_string(uri, "uri")?;
            arm_owned_fd_for_source(owned_fd, uri)?;
            let status = unsafe { erika_presenter_load_danmaku_file(handle, uri_c.as_ptr()) };
            call_status(status)?;
            Ok(Value::Null)
        }
        "loadDanmakuJson" => {
            let source = required_string(args, "json")?;
            let source_c = c_string(source, "json")?;
            status_value(unsafe { erika_presenter_load_danmaku_json(handle, source_c.as_ptr()) })
        }
        "addDanmakuTrackFile" => {
            let uri = required_string(args, "uri")?;
            let uri_c = c_string(uri, "uri")?;
            let name = optional_c_string(args, "name")?;
            let mut track_id = 0u64;
            arm_owned_fd_for_source(owned_fd, uri)?;
            let status = unsafe {
                erika_presenter_add_danmaku_track_file(
                    handle,
                    uri_c.as_ptr(),
                    optional_c_string_ptr(&name),
                    optional_i64(args, "offsetMicros").unwrap_or(0),
                    &mut track_id,
                )
            };
            call_status(status)?;
            Ok(json!(track_id))
        }
        "addDanmakuTrackJson" => {
            let source = required_string(args, "json")?;
            let source_c = c_string(source, "json")?;
            let name = optional_c_string(args, "name")?;
            let mut track_id = 0u64;
            call_status(unsafe {
                erika_presenter_add_danmaku_track_json(
                    handle,
                    source_c.as_ptr(),
                    optional_c_string_ptr(&name),
                    optional_i64(args, "offsetMicros").unwrap_or(0),
                    &mut track_id,
                )
            })?;
            Ok(json!(track_id))
        }
        "removeDanmakuTrack" => {
            let track_id = required_u64(args, "trackId")?;
            status_value(unsafe { erika_presenter_remove_danmaku_track(handle, track_id) })
        }
        "setDanmakuTrackEnabled" => {
            let track_id = required_u64(args, "trackId")?;
            let enabled = optional_bool(args, "enabled").unwrap_or(true);
            status_value(unsafe {
                erika_presenter_set_danmaku_track_enabled(handle, track_id, enabled)
            })
        }
        "setDanmakuTrackOffset" => {
            let track_id = required_u64(args, "trackId")?;
            let offset = optional_i64(args, "offsetMicros").unwrap_or(0);
            status_value(unsafe {
                erika_presenter_set_danmaku_track_offset(handle, track_id, offset)
            })
        }
        "setDanmakuGlobalOffset" => status_value(unsafe {
            erika_presenter_set_danmaku_global_offset(
                handle,
                optional_i64(args, "offsetMicros").unwrap_or(0),
            )
        }),
        "danmakuTracks" => unsafe { presenter_danmaku_tracks_json(handle) },
        "clearDanmaku" => status_value(unsafe { erika_presenter_clear_danmaku(handle) }),
        "setDanmakuEnabled" => status_value(unsafe {
            erika_presenter_set_danmaku_enabled(
                handle,
                optional_bool(args, "enabled").unwrap_or(true),
            )
        }),
        "setDanmakuConfig" => unsafe { set_danmaku_config(handle, args) },
        // Remaining names go to the shared pass-through table (diagnostics,
        // logs, listMethods, invokeJson, …); unknown names still fail here.
        method if crate::presenter_json::json_method_spec(method).is_some() => unsafe {
            crate::presenter_json::invoke(handle, method, arguments)
        },
        method => Err(format!("unsupported Erika Android method: {method}")),
    }
}

unsafe fn set_danmaku_config(
    handle: *mut ErikaPresenterHandle,
    args: &Map<String, Value>,
) -> Result<Value, String> {
    let mut config = ErikaDanmakuConfig::default();
    call_status(unsafe { erika_presenter_get_danmaku_config(handle, &mut config) })?;
    update_bool(args, "enabled", &mut config.enabled);
    update_f32(args, "fontSize", &mut config.font_size);
    update_f32(args, "opacity", &mut config.opacity);
    update_f32(args, "displayArea", &mut config.display_area);
    update_f32(
        args,
        "scrollDurationSeconds",
        &mut config.scroll_duration_seconds,
    );
    update_f32(args, "scrollSpeedFactor", &mut config.scroll_speed_factor);
    update_f32(args, "trackGapRatio", &mut config.track_gap_ratio);
    update_f32(args, "outlineWidth", &mut config.outline_width);
    update_f32(args, "shadowOffsetX", &mut config.shadow_offset_x);
    update_f32(args, "shadowOffsetY", &mut config.shadow_offset_y);
    update_bool(args, "mergeDuplicates", &mut config.merge_duplicates);
    update_bool(args, "allowStacking", &mut config.allow_stacking);
    update_bool(
        args,
        "allowScrollOverwrite",
        &mut config.allow_scroll_overwrite,
    );
    update_u32(args, "maxQuantity", &mut config.max_quantity);
    update_u32(args, "maxLinesPerMode", &mut config.max_lines_per_mode);
    update_bool(args, "blockTop", &mut config.block_top);
    update_bool(args, "blockBottom", &mut config.block_bottom);
    update_bool(args, "blockScroll", &mut config.block_scroll);
    if let Some(value) = args.get("shadowStyle").and_then(Value::as_i64) {
        config.shadow_style = value as i32;
    }
    call_status(unsafe { erika_presenter_set_danmaku_config(handle, config) })?;

    if args.contains_key("customFontFamily") || args.contains_key("customFontFilePath") {
        let family = optional_c_string(args, "customFontFamily")?;
        let file_path = optional_c_string(args, "customFontFilePath")?;
        call_status(unsafe {
            erika_presenter_set_danmaku_font(
                handle,
                optional_c_string_ptr(&family),
                optional_c_string_ptr(&file_path),
            )
        })?;
    }
    if let Some(block_words) = args.get("blockWordsJson").and_then(Value::as_str) {
        let block_words = c_string(block_words, "blockWordsJson")?;
        call_status(unsafe {
            erika_presenter_set_danmaku_block_words_json(handle, block_words.as_ptr())
        })?;
    }
    Ok(Value::Null)
}

unsafe fn presenter_tracks_json(handle: *mut ErikaPresenterHandle) -> Result<Value, String> {
    let mut len = 0usize;
    call_status(unsafe { erika_presenter_tracks(handle, ptr::null_mut(), 0, &mut len) })?;
    let mut tracks = vec![ErikaTrackInfo::default(); len];
    if len > 0 {
        call_status(unsafe {
            erika_presenter_tracks(handle, tracks.as_mut_ptr(), tracks.len(), &mut len)
        })?;
    }
    let values = tracks
        .iter_mut()
        .take(len)
        .map(|track| {
            let value = json!({
                "id": track.id,
                "kind": track.kind as i32,
                "source": track.source as i32,
                "selected": track.selected,
                "canRemove": track.can_remove,
                "title": unsafe { borrowed_c_string(track.title) },
                "language": unsafe { borrowed_c_string(track.language) },
                "codec": unsafe { borrowed_c_string(track.codec) },
                "width": track.width,
                "height": track.height,
                "sampleRate": track.sample_rate,
                "channels": track.channels,
                "pixelFormat": unsafe { borrowed_c_string(track.pixel_format) },
                "sampleFormat": unsafe { borrowed_c_string(track.sample_format) },
                "profile": unsafe { borrowed_c_string(track.profile) },
                "level": track.level,
                "bitRate": track.bit_rate,
                "frameRateNumerator": track.frame_rate_numerator,
                "frameRateDenominator": track.frame_rate_denominator,
            });
            unsafe { erika_track_info_free(track) };
            value
        })
        .collect();
    Ok(Value::Array(values))
}

unsafe fn presenter_track_selection_json(
    handle: *mut ErikaPresenterHandle,
) -> Result<Value, String> {
    let mut selection = ErikaTrackSelection::default();
    call_status(unsafe { erika_presenter_track_selection(handle, &mut selection) })?;
    Ok(json!({
        "video": selection.video,
        "audio": selection.audio,
        "subtitle": selection.subtitle,
    }))
}

unsafe fn subtitle_memory_font_status_json(
    handle: *mut ErikaPresenterHandle,
) -> Result<Value, String> {
    let mut status = ErikaSubtitleMemoryFontStatus::default();
    call_status(unsafe { erika_presenter_get_subtitle_memory_font_status(handle, &mut status) })?;
    let selected_ids = if status.selected_count == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(status.selected_ids, status.selected_count) }.to_vec()
    };
    let value = json!({
        "registeredCount": status.registered_count,
        "registeredBytes": status.registered_bytes,
        "selectedCount": status.selected_count,
        "generation": status.generation,
        "selectedIds": selected_ids,
    });
    unsafe { erika_subtitle_memory_font_status_free(&mut status) };
    Ok(value)
}

unsafe fn subtitle_memory_font_info_json(
    handle: *mut ErikaPresenterHandle,
    font_id: u64,
) -> Result<Value, String> {
    let mut info = ErikaSubtitleMemoryFontInfo::default();
    call_status(unsafe {
        erika_presenter_get_subtitle_memory_font_info(handle, font_id, &mut info)
    })?;
    let faces = if info.face_count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(info.faces, info.face_count) }
    };
    let faces = faces
        .iter()
        .map(|face| {
            let families_json = unsafe { borrowed_c_string(face.families_json) };
            let families = families_json
                .as_deref()
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .unwrap_or_else(|| Value::Array(Vec::new()));
            json!({
                "index": face.index,
                "families": families,
                "postScriptName": unsafe { borrowed_c_string(face.post_script_name) },
                "weight": face.weight,
                "italic": face.italic,
                "monospaced": face.monospaced,
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "id": info.id,
        "byteLen": info.byte_len,
        "faces": faces,
    });
    unsafe { erika_subtitle_memory_font_info_free(&mut info) };
    Ok(value)
}

unsafe fn presenter_danmaku_tracks_json(
    handle: *mut ErikaPresenterHandle,
) -> Result<Value, String> {
    let mut len = 0usize;
    call_status(unsafe { erika_presenter_danmaku_tracks(handle, ptr::null_mut(), 0, &mut len) })?;
    let mut tracks = vec![ErikaDanmakuTrackInfo::default(); len];
    if len > 0 {
        call_status(unsafe {
            erika_presenter_danmaku_tracks(handle, tracks.as_mut_ptr(), tracks.len(), &mut len)
        })?;
    }
    let values = tracks
        .iter_mut()
        .take(len)
        .map(|track| {
            let value = json!({
                "id": track.id,
                "enabled": track.enabled,
                "offsetMicros": track.offset_micros,
                "itemCount": track.item_count,
                "name": unsafe { borrowed_c_string(track.name) },
                "source": unsafe { borrowed_c_string(track.source) },
            });
            unsafe { erika_danmaku_track_info_free(track) };
            value
        })
        .collect();
    Ok(Value::Array(values))
}

fn event_to_json(event: ErikaEvent) -> Value {
    json!({
        "kind": event.kind as i32,
        "status": event.status as i32,
        "state": event.state as i32,
        "durationMicros": event.duration_micros,
        "positionMicros": event.position_micros,
        "buffering": event.buffering,
        "video": {
            "width": event.video.width,
            "height": event.video.height,
            "primaries": event.video.primaries,
            "transfer": event.video.transfer,
        },
        "tracks": {
            "video": event.tracks.video,
            "audio": event.tracks.audio,
            "subtitle": event.tracks.subtitle,
        },
    })
}

fn stats_to_json(stats: ErikaPresenterStats) -> Value {
    json!({
        "decodedVideoFrames": stats.decoded_video_frames,
        "renderedVideoFrames": stats.rendered_video_frames,
        "renderedTestFrames": stats.rendered_test_frames,
        "pushedAudioFrames": stats.pushed_audio_frames,
        "overlayFrames": stats.overlay_frames,
        "danmakuFrames": stats.danmaku_frames,
        "danmakuItems": stats.danmaku_items,
        "importFailures": stats.import_failures,
        "renderFailures": stats.render_failures,
        "audioFailures": stats.audio_failures,
        "softwareVideoFrames": stats.software_video_frames,
        "hardwareVideoFrames": stats.hardware_video_frames,
        "zeroCopyVideoFrames": stats.zero_copy_video_frames,
        "cpuVideoFrameFallbacks": stats.cpu_video_frame_fallbacks,
        "lastRenderMicros": stats.last_render_micros,
        "lastRenderCurrentMicros": stats.last_render_current_micros,
        "audioClockReadFrames": stats.audio_clock_read_frames,
        "audioClockQueuedFrames": stats.audio_clock_queued_frames,
        "audioClockUnderflowFrames": stats.audio_clock_underflow_frames,
        "audioRecoveryState": stats.audio_recovery_state,
        "audioLastErrorCode": stats.audio_last_error_code,
        "audioRecoveryAttempts": stats.audio_recovery_attempts,
        "audioRecoveryCount": stats.audio_recovery_count,
        "audioRecoveryFailures": stats.audio_recovery_failures,
        "directZeroCopyVideoFrames": stats.direct_zero_copy_video_frames,
        "sharedHandleVideoFrames": stats.shared_handle_video_frames,
        "hdrSourceFrames": stats.hdr_source_frames,
        "hdr10OutputFrames": stats.hdr10_output_frames,
        "sdrTonemapFrames": stats.sdr_tonemap_frames,
        "hdr10MetadataUpdates": stats.hdr10_metadata_updates,
        "hdr10MetadataFailures": stats.hdr10_metadata_failures,
        "hdr10OutputFailures": stats.hdr10_output_failures,
        "hdr10OutputActive": stats.hdr10_output_active,
        "videoFrameBackpressureDrops": stats.video_frame_backpressure_drops,
    })
}

fn upscaler_status_to_json(status: ErikaUpscalerStatus) -> Value {
    json!({
        "requestedMode": status.requested_mode,
        "activeBackend": status.active_backend,
        "fallbackCount": status.fallback_count,
        "upscaledFrames": status.upscaled_frames,
        "lastEncodeMicros": status.last_encode_micros,
        "lastGpuMicros": status.last_gpu_micros,
    })
}

fn output_status_to_json(status: ErikaOutputStatus) -> Value {
    json!({
        "requestedMode": status.requested_mode,
        "activeEncoding": status.active_encoding,
        "surfaceFormat": status.surface_format,
        "nativeDataSpace": status.native_data_space,
        "requestedHeadroom": status.requested_headroom,
        "activeHeadroom": status.active_headroom,
        "activeHeadroomKnown": status.active_headroom_known,
        "extendedLinearActive": status.extended_linear_active,
        "fallbackReason": status.fallback_reason,
        "fallbackCount": status.fallback_count,
        "dataSpaceFailures": status.data_space_failures,
        "headroomUpdates": status.headroom_updates,
        "extendedLinearFrames": status.extended_linear_frames,
    })
}

fn resource_status_to_json(status: ErikaPresenterResourceStatus) -> Value {
    json!({
        "deviceCurrentAllocatedBytes": status.device_current_allocated_bytes,
        "deviceRecommendedWorkingSetBytes": status.device_recommended_working_set_bytes,
        "drawableEstimatedBytes": status.drawable_estimated_bytes,
        "videoFrameBytes": status.video_frame_bytes,
        "overlayAtlasBytes": status.overlay_atlas_bytes,
        "danmakuAtlasBytes": status.danmaku_atlas_bytes,
        "danmakuVertexBufferBytes": status.danmaku_vertex_buffer_bytes,
        "upscalerBytes": status.upscaler_bytes,
        "rendererTrackedBytes": status.renderer_tracked_bytes,
        "presenterCpuDanmakuAtlasBytes": status.presenter_cpu_danmaku_atlas_bytes,
        "drawableCount": status.drawable_count,
        "outputModeSwitches": status.output_mode_switches,
    })
}

fn lock_registry() -> MutexGuard<'static, PresenterRegistry> {
    match presenter_registry().lock() {
        Ok(registry) => registry,
        Err(poisoned) => {
            android_jni_log_error(
                &json!({
                    "event": "presenter_registry",
                    "operation": "lock",
                    "errorKind": "poisoned_registry_lock",
                    "reason": "recovering poisoned Android presenter registry lock",
                })
                .to_string(),
            );
            poisoned.into_inner()
        }
    }
}

fn lock_presenter_entry<'a>(
    entry: &'a AndroidPresenterEntry,
    id: jlong,
    operation: &str,
) -> MutexGuard<'a, Option<OwnedAndroidPresenterAddress>> {
    match entry.presenter.lock() {
        Ok(presenter) => presenter,
        Err(poisoned) => {
            android_jni_log_error(
                &json!({
                    "event": "presenter_registry",
                    "operation": operation,
                    "playerId": id,
                    "errorKind": "poisoned_player_lock",
                    "reason": "recovering poisoned Android presenter operation lock",
                })
                .to_string(),
            );
            poisoned.into_inner()
        }
    }
}

fn with_registered_presenter<R>(
    id: jlong,
    operation: &str,
    invoke: impl for<'presenter> FnOnce(&'presenter mut AndroidPresenter) -> Result<R, String>,
) -> Result<R, String> {
    let entry = {
        let registry = lock_registry();
        match registry.get(id) {
            Ok(entry) => entry,
            Err(error) => return Err(report_presenter_registry_error(id, operation, error)),
        }
    };
    if let Err(error) = entry.ensure_owner_thread(id, operation) {
        return Err(report_presenter_registry_error(id, operation, error));
    }

    let mut slot = lock_presenter_entry(&entry, id, operation);
    let Some(address) = slot.as_mut() else {
        return Err(report_presenter_registry_error(
            id,
            operation,
            PresenterIdError::AlreadyDestroyed(id),
        ));
    };
    // SAFETY: this closure cannot return a borrow of the presenter, and it runs
    // only on the creator thread while this entry's operation mutex is held.
    invoke(unsafe { address.as_mut() })
}

fn destroy_registered_presenter(id: jlong) -> Result<(), PresenterIdError> {
    let entry = { lock_registry().remove_for_destroy(id)? };
    // Removal prevents new lookups. Taking this lock waits for every call that
    // cloned the entry before removal; late callers then observe the empty slot.
    destroy_presenter_entry(&entry, id, "destroy")
}

fn destroy_presenter_entry(
    entry: &AndroidPresenterEntry,
    id: jlong,
    operation: &str,
) -> Result<(), PresenterIdError> {
    entry.ensure_owner_thread(id, operation)?;
    let mut slot = lock_presenter_entry(entry, id, operation);
    let presenter = slot.take().ok_or(PresenterIdError::AlreadyDestroyed(id))?;
    // SAFETY: the creator thread was verified above, and the owned address was
    // taken exactly once while the per-player mutex is held.
    unsafe { presenter.destroy() };
    Ok(())
}

fn report_presenter_registry_error(id: jlong, operation: &str, error: PresenterIdError) -> String {
    let reason = error.to_string();
    set_last_error(&reason);
    log_presenter_registry_error(id, operation, &error);
    reason
}

fn log_presenter_registry_error(id: jlong, operation: &str, error: &PresenterIdError) {
    android_jni_log_error(
        &json!({
            "event": "presenter_registry",
            "operation": operation,
            "playerId": id,
            "errorKind": error.kind(),
            "reason": error.to_string(),
        })
        .to_string(),
    );
}

fn call_status(status: ErikaStatus) -> Result<(), String> {
    if matches!(status, ErikaStatus::Ok) {
        Ok(())
    } else {
        Err(last_error_message(status))
    }
}

fn status_value(status: ErikaStatus) -> Result<Value, String> {
    call_status(status)?;
    Ok(Value::Null)
}

fn last_error_message(status: ErikaStatus) -> String {
    let raw = erika_last_error_message();
    if raw.is_null() {
        return format!("Erika C ABI returned {status:?}");
    }
    let message = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    unsafe { erika_string_free(raw) };
    message
}

fn success_response(value: Value) -> Value {
    json!({
        "ok": true,
        "status": ErikaStatus::Ok as i32,
        "value": value,
    })
}

fn error_response(status: ErikaStatus, error: impl Into<String>) -> Value {
    json!({
        "ok": false,
        "status": status as i32,
        "error": error.into(),
    })
}

fn response_to_jstring(
    env: &mut JNIEnv<'_>,
    response: std::thread::Result<Result<Value, String>>,
) -> jstring {
    let value = match response {
        Ok(Ok(value)) => success_response(value),
        Ok(Err(error)) => error_response(ErikaStatus::PlayerError, error),
        Err(payload) => {
            let reason = panic_payload_message(payload);
            android_jni_log_error(
                &json!({
                    "event": "jni_panic",
                    "reason": reason,
                })
                .to_string(),
            );
            error_response(
                ErikaStatus::Panic,
                format!("panic in Erika Android JNI bridge: {reason}"),
            )
        }
    };
    new_java_string(env, value.to_string())
}

fn new_java_string(env: &mut JNIEnv<'_>, value: String) -> jstring {
    env.new_string(value)
        .map(|value| value.into_raw())
        .unwrap_or(ptr::null_mut())
}

fn install_android_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let reason = panic_payload_ref_message(info.payload()).replace(['\r', '\n'], " ");
            let (file, line, column) = info
                .location()
                .map(|location| (location.file(), location.line(), location.column()))
                .unwrap_or(("<unknown>", 0, 0));
            android_jni_log_error(
                &json!({
                    "event": "rust_panic",
                    "file": file,
                    "line": line,
                    "column": column,
                    "reason": reason,
                })
                .to_string(),
            );
            previous(info);
        }));
    });
}

pub(super) fn android_jni_log_error(message: &str) {
    const ANDROID_LOG_ERROR: jint = 6;
    const TAG: &[u8] = b"Erika\0";

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(priority: jint, tag: *const c_char, text: *const c_char) -> jint;
    }

    let Ok(message) = CString::new(message.replace('\0', "\\0")) else {
        return;
    };
    unsafe {
        let _ = __android_log_write(
            ANDROID_LOG_ERROR,
            TAG.as_ptr().cast::<c_char>(),
            message.as_ptr(),
        );
    }
}

fn required_string<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn required_i64(args: &Map<String, Value>, name: &str) -> Result<i64, String> {
    args.get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{name} is required"))
}

fn required_u64(args: &Map<String, Value>, name: &str) -> Result<u64, String> {
    args.get(name)
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|v| v.try_into().ok()))
        })
        .ok_or_else(|| format!("{name} is required"))
}

fn required_u64_array(args: &Map<String, Value>, name: &str) -> Result<Vec<u64>, String> {
    args.get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{name} is required"))?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|value| value.try_into().ok()))
                .ok_or_else(|| format!("{name} must contain unsigned integers"))
        })
        .collect()
}

fn required_f64(args: &Map<String, Value>, name: &str) -> Result<f64, String> {
    args.get(name)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("{name} is required"))
}

fn optional_i64(args: &Map<String, Value>, name: &str) -> Option<i64> {
    args.get(name).and_then(Value::as_i64)
}

fn optional_bool(args: &Map<String, Value>, name: &str) -> Option<bool> {
    args.get(name).and_then(Value::as_bool)
}

/// Parses an optional byte-count open argument (`httpReadAheadBytes`,
/// `httpBackBufferBytes`). 0 / missing keeps the default resolution
/// (environment override where one exists, then the engine default); negative
/// or fractional values are rejected because they cannot be a byte count.
fn optional_open_byte_count(args: &Map<String, Value>, name: &str) -> Result<u64, String> {
    let Some(value) = args.get(name) else {
        return Ok(0);
    };
    match value {
        Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| format!("{name} must be a non-negative integer")),
        Value::Null => Ok(0),
        _ => Err(format!("{name} must be a non-negative integer")),
    }
}

fn c_string(value: &str, name: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| format!("{name} contains an embedded NUL byte"))
}

/// Owns the `CString` storage the `ErikaHttpHeader` pointers refer to. Keeping
/// the strings and the header array in one value means the pointers stay valid
/// for exactly as long as the caller holds the storage, instead of dangling the
/// moment the builder returns.
#[derive(Default)]
struct HttpHeaderStorage {
    _names: Vec<CString>,
    _values: Vec<CString>,
    headers: Vec<ErikaHttpHeader>,
}

impl HttpHeaderStorage {
    fn as_ptr(&self) -> *const ErikaHttpHeader {
        if self.headers.is_empty() {
            ptr::null()
        } else {
            self.headers.as_ptr()
        }
    }

    fn len(&self) -> usize {
        self.headers.len()
    }
}

fn c_http_headers(args: &Map<String, Value>) -> Result<HttpHeaderStorage, String> {
    let Some(value) = args.get("httpHeaders") else {
        return Ok(HttpHeaderStorage::default());
    };
    let object = value
        .as_object()
        .ok_or_else(|| "httpHeaders must be a dictionary".to_string())?;
    let mut names = Vec::with_capacity(object.len());
    let mut values = Vec::with_capacity(object.len());
    for (name, value) in object {
        let value = value
            .as_str()
            .ok_or_else(|| format!("httpHeaders value for {name} must be a string"))?;
        if let Some(error) = http_header_error(name, value) {
            return Err(error);
        }
        names.push(c_string(name, "httpHeaders name")?);
        values.push(c_string(value, "httpHeaders value")?);
    }
    let headers = names
        .iter()
        .zip(values.iter())
        .map(|(name, value)| ErikaHttpHeader {
            name: name.as_ptr(),
            value: value.as_ptr(),
        })
        .collect();
    Ok(HttpHeaderStorage {
        _names: names,
        _values: values,
        headers,
    })
}

fn optional_c_string(args: &Map<String, Value>, name: &str) -> Result<Option<CString>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.is_empty() => Ok(None),
        Some(Value::String(value)) => c_string(value, name).map(Some),
        Some(_) => Err(format!("{name} must be a string or null")),
    }
}

fn optional_c_string_ptr(value: &Option<CString>) -> *const c_char {
    value.as_ref().map_or(ptr::null(), |value| value.as_ptr())
}

fn validate_owned_fd_invocation(
    owned_fd: &Option<OwnedFdCallGuard>,
    method: &str,
    arguments: &Value,
) -> Result<(), String> {
    if let Some(guard) = owned_fd {
        return guard.validate_invocation(method, arguments);
    }
    if !matches!(
        method,
        "open" | "addExternalSubtitle" | "loadDanmakuFile" | "addDanmakuTrackFile"
    ) {
        return Ok(());
    }
    let uri = arguments
        .as_object()
        .and_then(|args| args.get("uri"))
        .and_then(Value::as_str);
    if uri.is_some_and(|uri| uri.starts_with("fd://")) {
        return Err(format!(
            "owned fd URI for {method} requires an explicit transferred fd"
        ));
    }
    Ok(())
}

fn arm_owned_fd_for_source(
    owned_fd: &mut Option<OwnedFdCallGuard>,
    uri: &str,
) -> Result<(), String> {
    match (owned_fd.as_mut(), owned_fd_from_uri(uri)) {
        (Some(guard), Some(fd)) if fd == guard.fd => guard.arm_for_source(uri),
        (Some(guard), Some(fd)) => Err(format!(
            "owned fd URI changed before transfer: expected {} but received {fd}",
            guard.fd
        )),
        (Some(guard), None) => Err(format!(
            "owned fd URI changed before transfer: expected fd://{} but received {uri}",
            guard.fd
        )),
        (None, Some(fd)) => Err(format!(
            "owned fd URI fd://{fd} has no transferred descriptor"
        )),
        (None, None) => Ok(()),
    }
}

fn owned_fd_from_uri(uri: &str) -> Option<i32> {
    let body = uri.strip_prefix("fd://")?;
    let fd = body.split_once('?').map_or(body, |(fd, _)| fd);
    fd.parse().ok().filter(|fd| *fd >= 0)
}

unsafe fn borrowed_c_string(value: *const c_char) -> Option<String> {
    (!value.is_null()).then(|| {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    })
}

fn update_bool(args: &Map<String, Value>, name: &str, target: &mut bool) {
    if let Some(value) = args.get(name).and_then(Value::as_bool) {
        *target = value;
    }
}

fn update_f32(args: &Map<String, Value>, name: &str, target: &mut f32) {
    if let Some(value) = args.get(name).and_then(Value::as_f64) {
        if value.is_finite() {
            *target = value as f32;
        }
    }
}

fn update_f64(args: &Map<String, Value>, name: &str, target: &mut f64) {
    if let Some(value) = args.get(name).and_then(Value::as_f64) {
        if value.is_finite() {
            *target = value;
        }
    }
}

fn update_i32(args: &Map<String, Value>, name: &str, target: &mut i32) {
    if let Some(value) = args
        .get(name)
        .and_then(Value::as_i64)
        .and_then(|value| value.try_into().ok())
    {
        *target = value;
    }
}

fn update_u32(args: &Map<String, Value>, name: &str, target: &mut u32) {
    if let Some(value) = args.get(name).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_i64().and_then(|v| v.try_into().ok()))
    }) {
        *target = value.min(u32::MAX as u64) as u32;
    }
}

fn physical_dimension(value: jint) -> u32 {
    value.max(1) as u32
}

fn normalized_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale.clamp(0.25, 16.0)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_byte_count_options() {
        for name in ["httpReadAheadBytes", "httpBackBufferBytes"] {
            for (value, expected) in [
                (Value::Null, 0),
                (json!(0), 0),
                (json!(2_097_152), 2_097_152),
            ] {
                let args = Map::from_iter([(name.to_string(), value)]);
                assert_eq!(optional_open_byte_count(&args, name), Ok(expected));
            }

            for value in [json!(-1), json!(1.5), json!("2097152")] {
                let args = Map::from_iter([(name.to_string(), value)]);
                assert!(optional_open_byte_count(&args, name).is_err());
            }
        }
    }

    #[test]
    fn allocates_monotonic_nonzero_presenter_ids_without_reuse() {
        let mut allocator = PresenterIdAllocator::default();
        let first = allocator.allocate().unwrap();
        let second = allocator.allocate().unwrap();
        let third = allocator.allocate().unwrap();
        assert_eq!([first, second, third], [1, 2, 3]);

        let mut final_id = PresenterIdAllocator {
            next_id: Some(jlong::MAX),
        };
        assert_eq!(final_id.allocate(), Ok(jlong::MAX));
        assert_eq!(final_id.allocate(), Err(PresenterIdError::IdSpaceExhausted));
    }

    #[test]
    fn registry_rejects_invalid_unknown_and_destroyed_presenter_ids() {
        let mut registry = PresenterRegistry::default();
        assert!(matches!(registry.get(0), Err(PresenterIdError::Invalid(0))));
        assert!(matches!(
            registry.get(77),
            Err(PresenterIdError::Unknown(77))
        ));

        let issued = registry.ids.allocate().unwrap();
        assert!(matches!(
            registry.get(issued),
            Err(PresenterIdError::AlreadyDestroyed(id)) if id == issued
        ));
    }

    #[test]
    fn parses_owned_fd_uri_with_asset_metadata() {
        assert_eq!(owned_fd_from_uri("fd://42?offset=5&length=9"), Some(42));
        assert_eq!(owned_fd_from_uri("fd://bad?offset=0"), None);
        assert_eq!(owned_fd_from_uri("file:///tmp/video.mkv"), None);
    }

    #[test]
    fn parses_json_http_headers_into_c_headers() {
        let args = json!({
            "httpHeaders": {
                "Accept": "video/mp4",
                "X-Test": "two"
            }
        });
        let headers = c_http_headers(args.as_object().unwrap()).unwrap();
        let values = headers
            .headers
            .iter()
            .map(|header| unsafe {
                (
                    CStr::from_ptr(header.name).to_str().unwrap(),
                    CStr::from_ptr(header.value).to_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(values, vec![("Accept", "video/mp4"), ("X-Test", "two")]);
        assert_eq!(headers.len(), 2);
        assert!(!headers.as_ptr().is_null());
    }

    #[test]
    fn empty_json_http_headers_produce_a_null_header_pointer() {
        let args = json!({});
        let headers = c_http_headers(args.as_object().unwrap()).unwrap();

        assert_eq!(headers.len(), 0);
        assert!(headers.as_ptr().is_null());
    }

    #[test]
    fn rejects_reserved_and_malformed_json_http_headers() {
        for value in [
            json!({"Range": "bytes=0-10"}),
            json!({"host": "example.invalid"}),
            json!({"Content-Length": "10"}),
            json!({"Transfer-Encoding": "chunked"}),
            json!({"Connection": "close"}),
            json!({"X Test": "value"}),
            json!({"X-Test": "line\nbreak"}),
        ] {
            let args = json!({"httpHeaders": value});
            assert!(
                c_http_headers(args.as_object().unwrap()).is_err(),
                "{value} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_invalid_json_http_headers() {
        for value in [
            json!("not a dictionary"),
            json!({"X-Test": 1}),
            json!({"X\u{0000}-Test": "value"}),
            json!({"X-Test": "value\u{0000}"}),
        ] {
            let args = json!({"httpHeaders": value});
            assert!(c_http_headers(args.as_object().unwrap()).is_err());
        }

        let args = json!({});
        assert_eq!(c_http_headers(args.as_object().unwrap()).unwrap().len(), 0);
    }

    #[test]
    fn serializes_android_presenter_stats_with_dart_keys() {
        let stats = ErikaPresenterStats {
            rendered_video_frames: 12,
            audio_clock_queued_frames: 34,
            audio_recovery_state: 3,
            audio_last_error_code: -899,
            audio_recovery_count: 2,
            video_frame_backpressure_drops: 5,
            ..ErikaPresenterStats::default()
        };
        let value = stats_to_json(stats);
        assert_eq!(value["renderedVideoFrames"], 12);
        assert_eq!(value["audioClockQueuedFrames"], 34);
        assert_eq!(value["audioRecoveryState"], 3);
        assert_eq!(value["audioLastErrorCode"], -899);
        assert_eq!(value["audioRecoveryCount"], 2);
        assert_eq!(value["videoFrameBackpressureDrops"], 5);
    }

    #[test]
    fn parses_subtitle_memory_font_ids() {
        let args = json!({"fontIds": [1, 2, 3]});
        assert_eq!(
            required_u64_array(args.as_object().unwrap(), "fontIds").unwrap(),
            vec![1, 2, 3]
        );
        let invalid = json!({"fontIds": [1, -2]});
        assert!(required_u64_array(invalid.as_object().unwrap(), "fontIds").is_err());
    }
}
