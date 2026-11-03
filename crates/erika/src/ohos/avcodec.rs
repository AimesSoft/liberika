use super::pixel_layout::VideoPixelFormat;
use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const AV_ERR_OK: i32 = 0;
const AV_PIXEL_FORMAT_NV12: i32 = 2;
const AV_PIXEL_FORMAT_SURFACE_FORMAT: i32 = 4;
const AVCODEC_BUFFER_FLAGS_EOS: u32 = 1 << 0;
const AVCODEC_BUFFER_FLAGS_SYNC_FRAME: u32 = 1 << 1;
const DEFAULT_MAX_INPUT_SIZE: usize = 4 * 1024 * 1024;
const NATIVE_ERROR_NO_BUFFER: i32 = 40_601_000;
const NATIVEBUFFER_USAGE_HW_TEXTURE: u64 = 1 << 9;

#[repr(C)]
struct OH_AVCodec {
    _private: [u8; 0],
}

#[repr(C)]
struct OH_AVFormat {
    _private: [u8; 0],
}

#[repr(C)]
struct OH_AVBuffer {
    _private: [u8; 0],
}

#[repr(C)]
struct OH_NativeImage {
    _private: [u8; 0],
}

#[repr(C)]
struct OHNativeWindow {
    _private: [u8; 0],
}

#[repr(C)]
struct OHNativeWindowBuffer {
    _private: [u8; 0],
}

#[repr(C)]
struct OH_NativeBuffer {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct OhosNativeBufferConfig {
    pub width: i32,
    pub height: i32,
    pub format: i32,
    pub usage: i32,
    pub stride: i32,
}

type OhOnFrameAvailable = unsafe extern "C" fn(*mut c_void);

#[repr(C)]
#[derive(Clone, Copy)]
struct OH_OnFrameAvailableListener {
    context: *mut c_void,
    on_frame_available: Option<OhOnFrameAvailable>,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct OH_AVCodecBufferAttr {
    pts: i64,
    size: i32,
    offset: i32,
    flags: u32,
}

type OhAvCodecOnError = unsafe extern "C" fn(*mut OH_AVCodec, i32, *mut c_void);
type OhAvCodecOnStreamChanged =
    unsafe extern "C" fn(*mut OH_AVCodec, *mut OH_AVFormat, *mut c_void);
type OhAvCodecOnNeedInputBuffer =
    unsafe extern "C" fn(*mut OH_AVCodec, u32, *mut OH_AVBuffer, *mut c_void);
type OhAvCodecOnNewOutputBuffer =
    unsafe extern "C" fn(*mut OH_AVCodec, u32, *mut OH_AVBuffer, *mut c_void);

#[repr(C)]
#[derive(Clone, Copy)]
struct OH_AVCodecCallback {
    on_error: Option<OhAvCodecOnError>,
    on_stream_changed: Option<OhAvCodecOnStreamChanged>,
    on_need_input_buffer: Option<OhAvCodecOnNeedInputBuffer>,
    on_new_output_buffer: Option<OhAvCodecOnNewOutputBuffer>,
}

#[link(name = "native_media_vdec")]
unsafe extern "C" {
    fn OH_VideoDecoder_CreateByMime(mime: *const c_char) -> *mut OH_AVCodec;
    fn OH_VideoDecoder_Destroy(codec: *mut OH_AVCodec) -> i32;
    fn OH_VideoDecoder_RegisterCallback(
        codec: *mut OH_AVCodec,
        callback: OH_AVCodecCallback,
        user_data: *mut c_void,
    ) -> i32;
    fn OH_VideoDecoder_Configure(codec: *mut OH_AVCodec, format: *mut OH_AVFormat) -> i32;
    fn OH_VideoDecoder_Prepare(codec: *mut OH_AVCodec) -> i32;
    fn OH_VideoDecoder_Start(codec: *mut OH_AVCodec) -> i32;
    fn OH_VideoDecoder_Stop(codec: *mut OH_AVCodec) -> i32;
    fn OH_VideoDecoder_Flush(codec: *mut OH_AVCodec) -> i32;
    fn OH_VideoDecoder_PushInputBuffer(codec: *mut OH_AVCodec, index: u32) -> i32;
    fn OH_VideoDecoder_FreeOutputBuffer(codec: *mut OH_AVCodec, index: u32) -> i32;
    fn OH_VideoDecoder_SetSurface(codec: *mut OH_AVCodec, window: *mut OHNativeWindow) -> i32;
    fn OH_VideoDecoder_RenderOutputBuffer(codec: *mut OH_AVCodec, index: u32) -> i32;
}

#[link(name = "native_media_core")]
unsafe extern "C" {
    fn OH_AVFormat_CreateVideoFormat(
        mime_type: *const c_char,
        width: i32,
        height: i32,
    ) -> *mut OH_AVFormat;
    fn OH_AVFormat_Destroy(format: *mut OH_AVFormat);
    fn OH_AVFormat_SetIntValue(format: *mut OH_AVFormat, key: *const c_char, value: i32) -> bool;
    fn OH_AVFormat_SetBuffer(
        format: *mut OH_AVFormat,
        key: *const c_char,
        addr: *const u8,
        size: usize,
    ) -> bool;
    fn OH_AVFormat_GetIntValue(
        format: *mut OH_AVFormat,
        key: *const c_char,
        value: *mut i32,
    ) -> bool;
    fn OH_AVBuffer_GetBufferAttr(buffer: *mut OH_AVBuffer, attr: *mut OH_AVCodecBufferAttr) -> i32;
    fn OH_AVBuffer_SetBufferAttr(
        buffer: *mut OH_AVBuffer,
        attr: *const OH_AVCodecBufferAttr,
    ) -> i32;
    fn OH_AVBuffer_GetAddr(buffer: *mut OH_AVBuffer) -> *mut u8;
    fn OH_AVBuffer_GetCapacity(buffer: *mut OH_AVBuffer) -> i32;
    fn OH_AVBuffer_GetNativeBuffer(buffer: *mut OH_AVBuffer) -> *mut OH_NativeBuffer;
}

#[link(name = "native_image")]
unsafe extern "C" {
    fn OH_NativeImage_Create(texture_id: u32, texture_target: u32) -> *mut OH_NativeImage;
    fn OH_ConsumerSurface_Create() -> *mut OH_NativeImage;
    fn OH_NativeImage_AttachContext(image: *mut OH_NativeImage, texture_id: u32) -> i32;
    fn OH_ConsumerSurface_SetDefaultUsage(image: *mut OH_NativeImage, usage: u64) -> i32;
    fn OH_NativeImage_AcquireNativeWindow(image: *mut OH_NativeImage) -> *mut OHNativeWindow;
    fn OH_NativeImage_SetOnFrameAvailableListener(
        image: *mut OH_NativeImage,
        listener: OH_OnFrameAvailableListener,
    ) -> i32;
    fn OH_NativeImage_UnsetOnFrameAvailableListener(image: *mut OH_NativeImage) -> i32;
    fn OH_NativeImage_UpdateSurfaceImage(image: *mut OH_NativeImage) -> i32;
    fn OH_NativeImage_GetTransformMatrixV2(image: *mut OH_NativeImage, matrix: *mut f32) -> i32;
    fn OH_NativeImage_AcquireNativeWindowBuffer(
        image: *mut OH_NativeImage,
        native_window_buffer: *mut *mut OHNativeWindowBuffer,
        fence_fd: *mut i32,
    ) -> i32;
    fn OH_NativeImage_ReleaseNativeWindowBuffer(
        image: *mut OH_NativeImage,
        native_window_buffer: *mut OHNativeWindowBuffer,
        fence_fd: i32,
    ) -> i32;
    fn OH_NativeImage_Destroy(image: *mut *mut OH_NativeImage);
}

#[link(name = "native_buffer")]
unsafe extern "C" {
    fn OH_NativeBuffer_FromNativeWindowBuffer(
        native_window_buffer: *mut OHNativeWindowBuffer,
        buffer: *mut *mut OH_NativeBuffer,
    ) -> i32;
    fn OH_NativeBuffer_GetConfig(buffer: *mut OH_NativeBuffer, config: *mut OhosNativeBufferConfig);
    fn OH_NativeBuffer_Unreference(buffer: *mut OH_NativeBuffer) -> i32;
}

#[link(name = "native_window")]
unsafe extern "C" {
    fn OH_NativeWindow_DestroyNativeWindowBuffer(buffer: *mut OHNativeWindowBuffer);
}

#[link(name = "native_media_codecbase")]
unsafe extern "C" {
    static OH_MD_KEY_MAX_INPUT_SIZE: *const c_char;
    static OH_MD_KEY_PIXEL_FORMAT: *const c_char;
    static OH_MD_KEY_CODEC_CONFIG: *const c_char;
    static OH_MD_KEY_WIDTH: *const c_char;
    static OH_MD_KEY_HEIGHT: *const c_char;
    static OH_MD_KEY_VIDEO_STRIDE: *const c_char;
    static OH_MD_KEY_VIDEO_SLICE_HEIGHT: *const c_char;
    static OH_MD_KEY_VIDEO_PIC_WIDTH: *const c_char;
    static OH_MD_KEY_VIDEO_PIC_HEIGHT: *const c_char;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OhosVideoCodec {
    Avc,
    Hevc,
}

impl OhosVideoCodec {
    fn mime(self) -> &'static [u8] {
        match self {
            Self::Avc => b"video/avc\0",
            Self::Hevc => b"video/hevc\0",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Avc => "h264",
            Self::Hevc => "hevc",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DecodedVideoFrameView<'a> {
    pub pixel_format: VideoPixelFormat,
    pub luma: &'a [u8],
    pub chroma: &'a [u8],
    pub luma_stride: usize,
    pub chroma_stride: usize,
    pub width: u32,
    pub height: u32,
    pub pts_micros: i64,
}

pub enum OhosDecoderOutput<T> {
    NeedMoreInput,
    Frame(T),
    EndOfStream,
}

#[derive(Clone)]
pub struct DecodedSurfaceFrame {
    pub image: Arc<OhosNativeBufferImage>,
    pub width: u32,
    pub height: u32,
    pub pts_micros: i64,
}

struct SurfaceAvailability {
    pending: usize,
    shutting_down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OhosAvCodecSurfaceMode {
    ExternalTexture,
    NativeBuffer,
}

pub struct OhosAvCodecSurface {
    image: *mut OH_NativeImage,
    window: *mut OHNativeWindow,
    mode: OhosAvCodecSurfaceMode,
    availability: Mutex<SurfaceAvailability>,
    available: Condvar,
    consumer: Mutex<()>,
    discarded_external_frames: AtomicUsize,
}

unsafe impl Send for OhosAvCodecSurface {}
unsafe impl Sync for OhosAvCodecSurface {}

fn wait_acquire_fence(fence_fd: i32, timeout: Duration) -> Result<(), String> {
    if fence_fd < 0 {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for OHOS NativeBuffer acquire fence".to_string());
        }
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: fence_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result > 0 {
            unsafe { libc::close(fence_fd) };
            return Ok(());
        }
        if result == 0 {
            return Err("timed out waiting for OHOS NativeBuffer acquire fence".to_string());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!(
                "failed waiting for OHOS NativeBuffer acquire fence: {error}"
            ));
        }
    }
}

impl OhosAvCodecSurface {
    pub fn new_external_texture(texture_id: u32, texture_target: u32) -> Result<Arc<Self>, String> {
        let image = unsafe { OH_NativeImage_Create(texture_id, texture_target) };
        if image.is_null() {
            return Err(format!(
                "OH_NativeImage_Create returned null for texture {texture_id} target 0x{texture_target:x}"
            ));
        }
        let attach_code = unsafe { OH_NativeImage_AttachContext(image, texture_id) };
        if attach_code != AV_ERR_OK {
            let mut image = image;
            unsafe { OH_NativeImage_Destroy(&mut image) };
            return Err(format!(
                "OH_NativeImage_AttachContext failed with {attach_code}"
            ));
        }
        let usage_code =
            unsafe { OH_ConsumerSurface_SetDefaultUsage(image, NATIVEBUFFER_USAGE_HW_TEXTURE) };
        if usage_code != AV_ERR_OK {
            let mut image = image;
            unsafe { OH_NativeImage_Destroy(&mut image) };
            return Err(format!(
                "OH_ConsumerSurface_SetDefaultUsage(HW_TEXTURE) failed with {usage_code}"
            ));
        }
        Self::from_image(image, OhosAvCodecSurfaceMode::ExternalTexture)
    }

    pub fn new_native_buffer() -> Result<Arc<Self>, String> {
        let image = unsafe { OH_ConsumerSurface_Create() };
        if image.is_null() {
            return Err("OH_ConsumerSurface_Create returned null".to_string());
        }
        let usage_code =
            unsafe { OH_ConsumerSurface_SetDefaultUsage(image, NATIVEBUFFER_USAGE_HW_TEXTURE) };
        if usage_code != AV_ERR_OK {
            let mut image = image;
            unsafe { OH_NativeImage_Destroy(&mut image) };
            return Err(format!(
                "OH_ConsumerSurface_SetDefaultUsage(HW_TEXTURE) failed with {usage_code}"
            ));
        }
        Self::from_image(image, OhosAvCodecSurfaceMode::NativeBuffer)
    }

    fn from_image(
        image: *mut OH_NativeImage,
        mode: OhosAvCodecSurfaceMode,
    ) -> Result<Arc<Self>, String> {
        let window = unsafe { OH_NativeImage_AcquireNativeWindow(image) };
        if window.is_null() {
            let mut image = image;
            unsafe { OH_NativeImage_Destroy(&mut image) };
            return Err("OH_NativeImage_AcquireNativeWindow returned null".to_string());
        }
        let surface = Arc::new(Self {
            image,
            window,
            mode,
            availability: Mutex::new(SurfaceAvailability {
                pending: 0,
                shutting_down: false,
            }),
            available: Condvar::new(),
            consumer: Mutex::new(()),
            discarded_external_frames: AtomicUsize::new(0),
        });
        let listener = OH_OnFrameAvailableListener {
            context: Arc::as_ptr(&surface).cast_mut().cast(),
            on_frame_available: Some(on_surface_frame_available),
        };
        let code = unsafe { OH_NativeImage_SetOnFrameAvailableListener(image, listener) };
        if code != AV_ERR_OK {
            return Err(format!(
                "OH_NativeImage_SetOnFrameAvailableListener failed with {code}"
            ));
        }
        Ok(surface)
    }

    fn window(&self) -> *mut OHNativeWindow {
        self.window
    }

    pub fn acquire_frame(
        self: &Arc<Self>,
        timeout: Duration,
    ) -> Result<Arc<OhosNativeBufferImage>, String> {
        let deadline = Instant::now() + timeout;
        let mut availability = self
            .availability
            .lock()
            .map_err(|_| "OHOS surface availability lock was poisoned".to_string())?;
        while availability.pending == 0 && !availability.shutting_down {
            let now = Instant::now();
            if now >= deadline {
                return Err("timed out waiting for decoded Surface buffer".to_string());
            }
            let wait = deadline.saturating_duration_since(now);
            let (next, result) = self
                .available
                .wait_timeout(availability, wait)
                .map_err(|_| "OHOS surface availability wait was poisoned".to_string())?;
            availability = next;
            if result.timed_out() && availability.pending == 0 {
                return Err("timed out waiting for decoded Surface buffer".to_string());
            }
        }
        if availability.shutting_down {
            return Err("decoded Surface is shutting down".to_string());
        }
        availability.pending = availability.pending.saturating_sub(1);
        drop(availability);
        let payload = match self.mode {
            OhosAvCodecSurfaceMode::ExternalTexture => {
                NativeBufferImagePayload::ExternalTexture { pending: true }
            }
            OhosAvCodecSurfaceMode::NativeBuffer => {
                let _consumer = self
                    .consumer
                    .lock()
                    .map_err(|_| "OHOS NativeImage acquire lock was poisoned".to_string())?;
                let mut native_window_buffer = ptr::null_mut();
                let mut fence_fd = -1;
                let acquire_code = unsafe {
                    OH_NativeImage_AcquireNativeWindowBuffer(
                        self.image,
                        &mut native_window_buffer,
                        &mut fence_fd,
                    )
                };
                if acquire_code != AV_ERR_OK || native_window_buffer.is_null() {
                    if fence_fd >= 0 {
                        unsafe { libc::close(fence_fd) };
                    }
                    return Err(format!(
                        "OH_NativeImage_AcquireNativeWindowBuffer failed with {acquire_code}"
                    ));
                }
                if let Err(error) = wait_acquire_fence(fence_fd, timeout) {
                    let _ = unsafe {
                        OH_NativeImage_ReleaseNativeWindowBuffer(
                            self.image,
                            native_window_buffer,
                            fence_fd,
                        )
                    };
                    return Err(error);
                }
                let mut native_buffer = ptr::null_mut();
                let convert_code = unsafe {
                    OH_NativeBuffer_FromNativeWindowBuffer(native_window_buffer, &mut native_buffer)
                };
                if convert_code != AV_ERR_OK || native_buffer.is_null() {
                    let _ = unsafe {
                        OH_NativeImage_ReleaseNativeWindowBuffer(
                            self.image,
                            native_window_buffer,
                            -1,
                        )
                    };
                    return Err(format!(
                        "OH_NativeBuffer_FromNativeWindowBuffer failed with {convert_code}"
                    ));
                }
                let mut config = OhosNativeBufferConfig::default();
                unsafe { OH_NativeBuffer_GetConfig(native_buffer, &mut config) };
                if config.width <= 0 || config.height <= 0 {
                    let _ = unsafe {
                        OH_NativeImage_ReleaseNativeWindowBuffer(
                            self.image,
                            native_window_buffer,
                            -1,
                        )
                    };
                    return Err(format!("invalid OH_NativeBuffer config {config:?}"));
                }
                NativeBufferImagePayload::NativeBuffer {
                    native_window_buffer,
                    native_buffer,
                    config,
                }
            }
        };
        Ok(Arc::new(OhosNativeBufferImage {
            source: Arc::clone(self),
            state: Mutex::new(NativeBufferImageState {
                payload: Some(payload),
            }),
        }))
    }

    fn update_external_texture(&self) -> Result<Option<[f32; 16]>, String> {
        self.drain_discarded_external_frames()?;
        let _consumer = self
            .consumer
            .lock()
            .map_err(|_| "OHOS NativeImage update lock was poisoned".to_string())?;
        let update_code = unsafe { OH_NativeImage_UpdateSurfaceImage(self.image) };
        if update_code == NATIVE_ERROR_NO_BUFFER {
            return Ok(None);
        }
        check_avcodec(update_code, "OH_NativeImage_UpdateSurfaceImage")?;
        let mut transform = [0.0; 16];
        check_avcodec(
            unsafe { OH_NativeImage_GetTransformMatrixV2(self.image, transform.as_mut_ptr()) },
            "OH_NativeImage_GetTransformMatrixV2",
        )?;
        Ok(Some(transform))
    }

    pub(crate) fn drain_discarded_external_frames(&self) -> Result<usize, String> {
        let discarded = self.discarded_external_frames.swap(0, Ordering::AcqRel);
        if discarded == 0 {
            return Ok(0);
        }
        let _consumer = self
            .consumer
            .lock()
            .map_err(|_| "OHOS NativeImage update lock was poisoned".to_string())?;
        let mut drained = 0;
        for index in 0..discarded {
            let update_code = unsafe { OH_NativeImage_UpdateSurfaceImage(self.image) };
            if update_code == NATIVE_ERROR_NO_BUFFER {
                self.discarded_external_frames
                    .fetch_add(discarded - index, Ordering::AcqRel);
                break;
            }
            check_avcodec(update_code, "OH_NativeImage_UpdateSurfaceImage(discarded)")?;
            drained += 1;
        }
        Ok(drained)
    }

    fn mark_external_frame_discarded(&self) {
        self.discarded_external_frames
            .fetch_add(1, Ordering::AcqRel);
    }

    fn reset_pending_callbacks_after_flush(&self) -> Result<(), String> {
        let mut availability = self
            .availability
            .lock()
            .map_err(|_| "OHOS surface availability lock was poisoned".to_string())?;
        availability.pending = 0;
        Ok(())
    }

    fn prepare_for_decoder_attachment(&self) -> Result<(), String> {
        self.reset_pending_callbacks_after_flush()?;
        self.discarded_external_frames.store(0, Ordering::Release);
        Ok(())
    }

    fn release_native_buffer(
        &self,
        native_window_buffer: *mut OHNativeWindowBuffer,
    ) -> Result<(), String> {
        let _consumer = self
            .consumer
            .lock()
            .map_err(|_| "OHOS NativeImage release lock was poisoned".to_string())?;
        let code = unsafe {
            OH_NativeImage_ReleaseNativeWindowBuffer(self.image, native_window_buffer, -1)
        };
        if code == AV_ERR_OK {
            Ok(())
        } else {
            unsafe { OH_NativeWindow_DestroyNativeWindowBuffer(native_window_buffer) };
            Err(format!(
                "OH_NativeImage_ReleaseNativeWindowBuffer failed with {code}"
            ))
        }
    }
}

impl Drop for OhosAvCodecSurface {
    fn drop(&mut self) {
        if let Ok(mut availability) = self.availability.lock() {
            availability.shutting_down = true;
            self.available.notify_all();
        }
        if !self.image.is_null() {
            let _ = unsafe { OH_NativeImage_UnsetOnFrameAvailableListener(self.image) };
            unsafe { OH_NativeImage_Destroy(&mut self.image) };
        }
        self.window = ptr::null_mut();
    }
}

enum NativeBufferImagePayload {
    ExternalTexture {
        pending: bool,
    },
    NativeBuffer {
        native_window_buffer: *mut OHNativeWindowBuffer,
        native_buffer: *mut OH_NativeBuffer,
        config: OhosNativeBufferConfig,
    },
}

struct NativeBufferImageState {
    payload: Option<NativeBufferImagePayload>,
}

pub struct OhosNativeBufferImage {
    source: Arc<OhosAvCodecSurface>,
    state: Mutex<NativeBufferImageState>,
}

unsafe impl Send for OhosNativeBufferImage {}
unsafe impl Sync for OhosNativeBufferImage {}

impl OhosNativeBufferImage {
    pub fn update_external_texture(&self) -> Result<Option<[f32; 16]>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "OHOS native image frame lock was poisoned".to_string())?;
        match state.payload.as_mut() {
            Some(NativeBufferImagePayload::ExternalTexture { pending }) if *pending => {
                let transform = self.source.update_external_texture()?;
                *pending = false;
                Ok(transform)
            }
            Some(NativeBufferImagePayload::ExternalTexture { .. }) => {
                Err("OHOS external texture frame was already updated".to_string())
            }
            Some(NativeBufferImagePayload::NativeBuffer { .. }) => {
                Err("OHOS NativeBuffer frame cannot update an external OES texture".to_string())
            }
            None => Err("OHOS native image frame was already released".to_string()),
        }
    }

    pub fn native_buffer(&self) -> Result<(NonNull<c_void>, OhosNativeBufferConfig), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "OHOS native buffer image lock was poisoned".to_string())?;
        match state.payload.as_ref() {
            Some(NativeBufferImagePayload::NativeBuffer {
                native_buffer,
                config,
                ..
            }) => NonNull::new((*native_buffer).cast())
                .map(|buffer| (buffer, *config))
                .ok_or_else(|| "OHOS NativeBuffer pointer is null".to_string()),
            Some(NativeBufferImagePayload::ExternalTexture { .. }) => {
                Err("OHOS external OES frame has no acquired NativeBuffer".to_string())
            }
            None => Err("OHOS NativeBuffer frame was already released".to_string()),
        }
    }

    pub fn release_to_surface(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "OHOS native buffer image lock was poisoned".to_string())?;
        match state.payload.take() {
            Some(NativeBufferImagePayload::ExternalTexture { pending: true }) => {
                self.source.mark_external_frame_discarded();
                Ok(())
            }
            Some(NativeBufferImagePayload::ExternalTexture { pending: false }) | None => Ok(()),
            Some(NativeBufferImagePayload::NativeBuffer {
                native_window_buffer,
                ..
            }) => self.source.release_native_buffer(native_window_buffer),
        }
    }
}

impl Drop for OhosNativeBufferImage {
    fn drop(&mut self) {
        let _ = self.release_to_surface();
    }
}

#[derive(Debug, Clone, Copy)]
struct InputBuffer {
    index: u32,
    buffer: usize,
}

#[derive(Debug, Clone, Copy)]
struct OutputBuffer {
    index: u32,
    buffer: usize,
    attr: OH_AVCodecBufferAttr,
}

#[derive(Debug, Clone, Copy)]
struct OutputLayout {
    width: u32,
    height: u32,
    stride: usize,
    slice_height: usize,
    pixel_format: i32,
}

impl OutputLayout {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            stride: width as usize,
            slice_height: height as usize,
            pixel_format: AV_PIXEL_FORMAT_NV12,
        }
    }
}

#[derive(Debug)]
struct CallbackState {
    inputs: VecDeque<InputBuffer>,
    outputs: VecDeque<OutputBuffer>,
    layout: OutputLayout,
    errors: VecDeque<i32>,
}

struct CallbackContext {
    state: Mutex<CallbackState>,
}

pub struct OhosVideoDecoder {
    codec: *mut OH_AVCodec,
    callback_context: Box<CallbackContext>,
    codec_kind: OhosVideoCodec,
    nal_length_size: Option<usize>,
    parameter_sets: Vec<u8>,
    parameter_sets_sent: bool,
    surface: Option<Arc<OhosAvCodecSurface>>,
    started: bool,
    last_output_pixel_format: Option<VideoPixelFormat>,
}

unsafe impl Send for OhosVideoDecoder {}

impl OhosVideoDecoder {
    pub fn new(
        codec_kind: OhosVideoCodec,
        width: u32,
        height: u32,
        codec_config: &[u8],
        surface: Option<Arc<OhosAvCodecSurface>>,
    ) -> Result<Self, String> {
        if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err(format!("invalid video dimensions {width}x{height}"));
        }

        let (codec_config, nal_length_size, parameter_sets) =
            normalize_codec_config(codec_kind, codec_config)?;
        let surface = surface.and_then(|surface| {
            surface
                .prepare_for_decoder_attachment()
                .ok()
                .map(|()| surface)
        });
        let mime = codec_kind.mime();
        let codec = unsafe { OH_VideoDecoder_CreateByMime(mime.as_ptr().cast()) };
        if codec.is_null() {
            return Err(format!(
                "OH_VideoDecoder_CreateByMime returned null for {}",
                codec_kind.as_str()
            ));
        }
        let callback_context = Box::new(CallbackContext {
            state: Mutex::new(CallbackState {
                inputs: VecDeque::new(),
                outputs: VecDeque::new(),
                layout: OutputLayout::new(width, height),
                errors: VecDeque::new(),
            }),
        });
        let mut decoder = Self {
            codec,
            callback_context,
            codec_kind,
            nal_length_size,
            parameter_sets,
            parameter_sets_sent: false,
            surface,
            started: false,
            last_output_pixel_format: None,
        };
        decoder.initialize(width, height, &codec_config)?;
        Ok(decoder)
    }

    fn initialize(&mut self, width: u32, height: u32, codec_config: &[u8]) -> Result<(), String> {
        let callback = OH_AVCodecCallback {
            on_error: Some(on_error),
            on_stream_changed: Some(on_stream_changed),
            on_need_input_buffer: Some(on_need_input_buffer),
            on_new_output_buffer: Some(on_new_output_buffer),
        };
        let user_data = (&mut *self.callback_context as *mut CallbackContext).cast();
        check_avcodec(
            unsafe { OH_VideoDecoder_RegisterCallback(self.codec, callback, user_data) },
            "OH_VideoDecoder_RegisterCallback",
        )?;

        let mime = self.codec_kind.mime();
        let format = unsafe {
            OH_AVFormat_CreateVideoFormat(mime.as_ptr().cast(), width as i32, height as i32)
        };
        if format.is_null() {
            return Err("OH_AVFormat_CreateVideoFormat returned null".to_string());
        }

        let max_input_size = (width as usize)
            .saturating_mul(height as usize)
            .max(DEFAULT_MAX_INPUT_SIZE)
            .min(i32::MAX as usize) as i32;
        let format_result = (|| {
            set_format_int(
                format,
                unsafe { OH_MD_KEY_MAX_INPUT_SIZE },
                max_input_size,
                "OH_MD_KEY_MAX_INPUT_SIZE",
            )?;
            set_format_int(
                format,
                unsafe { OH_MD_KEY_PIXEL_FORMAT },
                if self.surface.is_some() {
                    AV_PIXEL_FORMAT_SURFACE_FORMAT
                } else {
                    AV_PIXEL_FORMAT_NV12
                },
                "OH_MD_KEY_PIXEL_FORMAT",
            )?;
            if !codec_config.is_empty()
                && !unsafe {
                    OH_AVFormat_SetBuffer(
                        format,
                        OH_MD_KEY_CODEC_CONFIG,
                        codec_config.as_ptr(),
                        codec_config.len(),
                    )
                }
            {
                return Err("OH_AVFormat_SetBuffer(OH_MD_KEY_CODEC_CONFIG) failed".to_string());
            }
            check_avcodec(
                unsafe { OH_VideoDecoder_Configure(self.codec, format) },
                "OH_VideoDecoder_Configure",
            )
        })();
        unsafe { OH_AVFormat_Destroy(format) };
        format_result?;

        if let Some(surface) = self.surface.as_ref() {
            check_avcodec(
                unsafe { OH_VideoDecoder_SetSurface(self.codec, surface.window()) },
                "OH_VideoDecoder_SetSurface",
            )?;
        }
        check_avcodec(
            unsafe { OH_VideoDecoder_Prepare(self.codec) },
            "OH_VideoDecoder_Prepare",
        )?;
        check_avcodec(
            unsafe { OH_VideoDecoder_Start(self.codec) },
            "OH_VideoDecoder_Start",
        )?;
        self.started = true;
        Ok(())
    }

    pub fn send_packet(
        &mut self,
        data: &[u8],
        pts_micros: i64,
        is_key: bool,
    ) -> Result<bool, String> {
        self.check_callback_error()?;
        let normalized_data;
        let packet_data = if let Some(nal_length_size) = self.nal_length_size {
            normalized_data = length_prefixed_packet_to_annex_b(data, nal_length_size)?;
            normalized_data.as_slice()
        } else {
            data
        };
        let prepended_data;
        let includes_parameter_sets =
            is_key && !self.parameter_sets_sent && !self.parameter_sets.is_empty();
        let data = if includes_parameter_sets {
            prepended_data = {
                let mut combined =
                    Vec::with_capacity(self.parameter_sets.len() + packet_data.len());
                combined.extend_from_slice(&self.parameter_sets);
                combined.extend_from_slice(packet_data);
                combined
            };
            prepended_data.as_slice()
        } else {
            packet_data
        };
        let input = {
            let mut state = self.state()?;
            state.inputs.pop_front()
        };
        let Some(input) = input else {
            return Ok(false);
        };

        let buffer = input.buffer as *mut OH_AVBuffer;
        let capacity = unsafe { OH_AVBuffer_GetCapacity(buffer) };
        let address = unsafe { OH_AVBuffer_GetAddr(buffer) };
        if capacity < 0 || address.is_null() {
            return Err("OH_AVBuffer input storage is unavailable".to_string());
        }
        if data.len() > capacity as usize || data.len() > i32::MAX as usize {
            self.state()?.inputs.push_front(input);
            return Err(format!(
                "compressed packet is {} bytes but AVCodec input capacity is {} bytes",
                data.len(),
                capacity
            ));
        }

        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), address, data.len()) };
        let attr = OH_AVCodecBufferAttr {
            pts: pts_micros,
            size: data.len() as i32,
            offset: 0,
            flags: if is_key {
                AVCODEC_BUFFER_FLAGS_SYNC_FRAME
            } else {
                0
            },
        };
        check_avcodec(
            unsafe { OH_AVBuffer_SetBufferAttr(buffer, &attr) },
            "OH_AVBuffer_SetBufferAttr(input)",
        )?;
        check_avcodec(
            unsafe { OH_VideoDecoder_PushInputBuffer(self.codec, input.index) },
            "OH_VideoDecoder_PushInputBuffer",
        )?;
        if includes_parameter_sets {
            self.parameter_sets_sent = true;
        }
        Ok(true)
    }

    pub fn send_eof(&mut self) -> Result<bool, String> {
        self.check_callback_error()?;
        let input = {
            let mut state = self.state()?;
            state.inputs.pop_front()
        };
        let Some(input) = input else {
            return Ok(false);
        };
        let buffer = input.buffer as *mut OH_AVBuffer;
        let attr = OH_AVCodecBufferAttr {
            flags: AVCODEC_BUFFER_FLAGS_EOS,
            ..OH_AVCodecBufferAttr::default()
        };
        check_avcodec(
            unsafe { OH_AVBuffer_SetBufferAttr(buffer, &attr) },
            "OH_AVBuffer_SetBufferAttr(eof)",
        )?;
        check_avcodec(
            unsafe { OH_VideoDecoder_PushInputBuffer(self.codec, input.index) },
            "OH_VideoDecoder_PushInputBuffer(eof)",
        )?;
        Ok(true)
    }

    pub fn receive_frame<T, F>(&mut self, consume: F) -> Result<OhosDecoderOutput<T>, String>
    where
        F: for<'buffer> FnOnce(DecodedVideoFrameView<'buffer>) -> Result<T, String>,
    {
        self.check_callback_error()?;
        let (output, layout) = {
            let mut state = self.state()?;
            let Some(output) = state.outputs.pop_front() else {
                return Ok(OhosDecoderOutput::NeedMoreInput);
            };
            (output, state.layout)
        };

        if output.attr.flags & AVCODEC_BUFFER_FLAGS_EOS != 0 && output.attr.size <= 0 {
            check_avcodec(
                unsafe { OH_VideoDecoder_FreeOutputBuffer(self.codec, output.index) },
                "OH_VideoDecoder_FreeOutputBuffer(eof)",
            )?;
            return Ok(OhosDecoderOutput::EndOfStream);
        }

        // The AVCodec-owned buffer remains valid until FreeOutputBuffer. Consume
        // it synchronously so the caller can copy straight into its final frame
        // storage without allocating and filling an intermediate full-frame Vec.
        // SAFETY: AVCodec owns the output storage and guarantees that it stays
        // valid until OH_VideoDecoder_FreeOutputBuffer below. The higher-ranked
        // callback bound prevents a borrowed view from escaping `consume`.
        let consume_result = unsafe { output_frame_view(output, layout) }.and_then(|decoded| {
            if self.last_output_pixel_format != Some(decoded.pixel_format) {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "ohos_avcodec_output_layout",
                        "pixelFormat": decoded.pixel_format.as_str(),
                        "bytesPerSample": decoded.pixel_format.bytes_per_sample(),
                        "width": decoded.width,
                        "height": decoded.height,
                        "stride": decoded.luma_stride,
                    })
                    .to_string(),
                );
                self.last_output_pixel_format = Some(decoded.pixel_format);
            }
            consume(decoded)
        });
        let release_result = check_avcodec(
            unsafe { OH_VideoDecoder_FreeOutputBuffer(self.codec, output.index) },
            "OH_VideoDecoder_FreeOutputBuffer",
        );
        match (consume_result, release_result) {
            (Ok(frame), Ok(())) => Ok(OhosDecoderOutput::Frame(frame)),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    pub fn receive_surface_frame(
        &mut self,
    ) -> Result<OhosDecoderOutput<DecodedSurfaceFrame>, String> {
        self.check_callback_error()?;
        let Some(surface) = self.surface.as_ref().cloned() else {
            return Err("AVCodec decoder is not configured for Surface output".to_string());
        };
        let (output, layout) = {
            let mut state = self.state()?;
            let Some(output) = state.outputs.pop_front() else {
                return Ok(OhosDecoderOutput::NeedMoreInput);
            };
            (output, state.layout)
        };
        if output.attr.flags & AVCODEC_BUFFER_FLAGS_EOS != 0 && output.attr.size <= 0 {
            check_avcodec(
                unsafe { OH_VideoDecoder_FreeOutputBuffer(self.codec, output.index) },
                "OH_VideoDecoder_FreeOutputBuffer(surface eof)",
            )?;
            return Ok(OhosDecoderOutput::EndOfStream);
        }
        check_avcodec(
            unsafe { OH_VideoDecoder_RenderOutputBuffer(self.codec, output.index) },
            "OH_VideoDecoder_RenderOutputBuffer",
        )?;
        let image = surface.acquire_frame(Duration::from_millis(100))?;
        Ok(OhosDecoderOutput::Frame(DecodedSurfaceFrame {
            image,
            width: layout.width,
            height: layout.height,
            pts_micros: output.attr.pts,
        }))
    }

    pub fn uses_surface(&self) -> bool {
        self.surface.is_some()
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.check_callback_error()?;
        check_avcodec(
            unsafe { OH_VideoDecoder_Flush(self.codec) },
            "OH_VideoDecoder_Flush",
        )?;
        if let Some(surface) = &self.surface {
            surface.reset_pending_callbacks_after_flush()?;
        }
        {
            let mut state = self.state()?;
            state.inputs.clear();
            state.outputs.clear();
            state.errors.clear();
        }
        check_avcodec(
            unsafe { OH_VideoDecoder_Start(self.codec) },
            "OH_VideoDecoder_Start(after flush)",
        )?;
        self.parameter_sets_sent = false;
        Ok(())
    }

    fn check_callback_error(&self) -> Result<(), String> {
        let error = self.state()?.errors.pop_front();
        match error {
            Some(code) => Err(format!("HarmonyOS AVCodec callback error {code}")),
            None => Ok(()),
        }
    }

    fn state(&self) -> Result<MutexGuard<'_, CallbackState>, String> {
        self.callback_context
            .state
            .lock()
            .map_err(|_| "HarmonyOS AVCodec callback state lock was poisoned".to_string())
    }
}

unsafe extern "C" fn on_surface_frame_available(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let surface = unsafe { &*context.cast::<OhosAvCodecSurface>() };
    if let Ok(mut availability) = surface.availability.lock() {
        availability.pending = availability.pending.saturating_add(1);
        surface.available.notify_one();
    }
}

impl Drop for OhosVideoDecoder {
    fn drop(&mut self) {
        if self.started {
            let _ = unsafe { OH_VideoDecoder_Stop(self.codec) };
            self.started = false;
        }
        if !self.codec.is_null() {
            let _ = unsafe { OH_VideoDecoder_Destroy(self.codec) };
            self.codec = ptr::null_mut();
        }
    }
}

fn normalize_codec_config(
    codec: OhosVideoCodec,
    codec_config: &[u8],
) -> Result<(Vec<u8>, Option<usize>, Vec<u8>), String> {
    if codec_config.is_empty() {
        return Ok((Vec::new(), None, Vec::new()));
    }
    if is_annex_b(codec_config) {
        return Ok((codec_config.to_vec(), None, codec_config.to_vec()));
    }
    let (parameter_sets, nal_length_size) = match codec {
        OhosVideoCodec::Avc => avcc_to_annex_b(codec_config),
        OhosVideoCodec::Hevc => hvcc_to_annex_b(codec_config),
    }?;
    Ok((codec_config.to_vec(), nal_length_size, parameter_sets))
}

fn avcc_to_annex_b(config: &[u8]) -> Result<(Vec<u8>, Option<usize>), String> {
    if config.len() < 7 || config[0] != 1 {
        return Err("invalid AVCDecoderConfigurationRecord".to_string());
    }
    let nal_length_size = (config[4] & 0x03) as usize + 1;
    let mut cursor = 6;
    let mut output = Vec::with_capacity(config.len() + 16);
    let sequence_parameter_sets = (config[5] & 0x1f) as usize;
    for _ in 0..sequence_parameter_sets {
        append_config_nal(config, &mut cursor, &mut output)?;
    }
    let picture_parameter_sets = *config
        .get(cursor)
        .ok_or_else(|| "AVC configuration is missing PPS count".to_string())?
        as usize;
    cursor += 1;
    for _ in 0..picture_parameter_sets {
        append_config_nal(config, &mut cursor, &mut output)?;
    }
    if output.is_empty() {
        return Err("AVC configuration contains no SPS/PPS data".to_string());
    }
    Ok((output, Some(nal_length_size)))
}

fn hvcc_to_annex_b(config: &[u8]) -> Result<(Vec<u8>, Option<usize>), String> {
    if config.len() < 23 || config[0] != 1 {
        return Err("invalid HEVCDecoderConfigurationRecord".to_string());
    }
    let nal_length_size = (config[21] & 0x03) as usize + 1;
    let array_count = config[22] as usize;
    let mut cursor = 23usize;
    let mut output = Vec::with_capacity(config.len() + array_count * 4);
    for _ in 0..array_count {
        cursor = cursor
            .checked_add(1)
            .filter(|cursor| *cursor + 2 <= config.len())
            .ok_or_else(|| "truncated HEVC configuration array".to_string())?;
        let nal_count = u16::from_be_bytes([config[cursor], config[cursor + 1]]) as usize;
        cursor += 2;
        for _ in 0..nal_count {
            append_config_nal(config, &mut cursor, &mut output)?;
        }
    }
    if output.is_empty() {
        return Err("HEVC configuration contains no VPS/SPS/PPS data".to_string());
    }
    Ok((output, Some(nal_length_size)))
}

fn append_config_nal(
    config: &[u8],
    cursor: &mut usize,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    if *cursor + 2 > config.len() {
        return Err("truncated codec configuration NAL length".to_string());
    }
    let nal_size = u16::from_be_bytes([config[*cursor], config[*cursor + 1]]) as usize;
    *cursor += 2;
    let end = cursor
        .checked_add(nal_size)
        .filter(|end| *end <= config.len())
        .ok_or_else(|| "truncated codec configuration NAL data".to_string())?;
    output.extend_from_slice(&[0, 0, 0, 1]);
    output.extend_from_slice(&config[*cursor..end]);
    *cursor = end;
    Ok(())
}

fn length_prefixed_packet_to_annex_b(
    packet: &[u8],
    nal_length_size: usize,
) -> Result<Vec<u8>, String> {
    if !(1..=4).contains(&nal_length_size) {
        return Err(format!("invalid NAL length size {nal_length_size}"));
    }
    let mut cursor = 0usize;
    let mut output = Vec::with_capacity(packet.len().saturating_add(16));
    while cursor < packet.len() {
        if cursor + nal_length_size > packet.len() {
            return Err("truncated length-prefixed video packet".to_string());
        }
        let mut nal_size = 0usize;
        for byte in &packet[cursor..cursor + nal_length_size] {
            nal_size = nal_size
                .checked_shl(8)
                .and_then(|value| value.checked_add(*byte as usize))
                .ok_or_else(|| "video packet NAL size overflowed".to_string())?;
        }
        cursor += nal_length_size;
        if nal_size == 0 {
            continue;
        }
        let end = cursor
            .checked_add(nal_size)
            .filter(|end| *end <= packet.len())
            .ok_or_else(|| {
                format!(
                    "video packet NAL size {nal_size} exceeds remaining {} bytes",
                    packet.len().saturating_sub(cursor)
                )
            })?;
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&packet[cursor..end]);
        cursor = end;
    }
    if output.is_empty() && !packet.is_empty() {
        return Err("length-prefixed video packet contains no NAL data".to_string());
    }
    Ok(output)
}

fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

unsafe fn output_frame_view<'a>(
    output: OutputBuffer,
    layout: OutputLayout,
) -> Result<DecodedVideoFrameView<'a>, String> {
    let buffer = output.buffer as *mut OH_AVBuffer;
    let native = unsafe { OH_AVBuffer_GetNativeBuffer(buffer) };
    let mut config = OhosNativeBufferConfig::default();
    let native_format = if native.is_null() {
        None
    } else {
        unsafe { OH_NativeBuffer_GetConfig(native, &mut config) };
        unsafe { OH_NativeBuffer_Unreference(native) };
        Some(config.format)
    };
    // AVCodec may label a P010 buffer as pixel_format=NV12. The native
    // buffer's real format distinguishes 8-bit bytes from 10-bit words.
    let pixel_format = VideoPixelFormat::from_native_buffer(layout.pixel_format, native_format)?;
    let stride = if config.stride > 0 {
        config.stride as usize
    } else {
        layout.stride
    };
    let width = layout.width as usize;
    let height = layout.height as usize;
    let luma_row_bytes = pixel_format.luma_row_bytes(width)?;
    let chroma_row_bytes = pixel_format.chroma_row_bytes(width)?;
    let chroma_rows = height.div_ceil(2);
    if layout.width == 0
        || layout.height == 0
        || stride < luma_row_bytes
        || stride < chroma_row_bytes
        || layout.slice_height < height
    {
        return Err(format!(
            "invalid AVCodec output layout {layout:?}, format={pixel_format:?}, stride={stride}"
        ));
    }
    let capacity = unsafe { OH_AVBuffer_GetCapacity(buffer) };
    if capacity < 0 {
        return Err("OH_AVBuffer output capacity is unavailable".to_string());
    }
    let offset = output.attr.offset.max(0) as usize;
    let luma_storage_size = stride
        .checked_mul(layout.slice_height)
        .ok_or_else(|| "AVCodec luma layout size overflowed".to_string())?;
    let visible_luma_size = stride
        .checked_mul(height)
        .ok_or_else(|| "AVCodec visible luma size overflowed".to_string())?;
    let chroma_size = stride
        .checked_mul(chroma_rows)
        .ok_or_else(|| "AVCodec chroma layout size overflowed".to_string())?;
    let required = luma_storage_size
        .checked_add(chroma_size)
        .and_then(|size| offset.checked_add(size))
        .ok_or_else(|| "AVCodec output layout size overflowed".to_string())?;
    if required > capacity as usize {
        return Err(format!(
            "AVCodec output layout needs {required} bytes but capacity is {capacity}"
        ));
    }
    let address = unsafe { OH_AVBuffer_GetAddr(buffer) };
    if address.is_null() {
        return Err("OH_AVBuffer output address is unavailable".to_string());
    }
    let source = unsafe { address.add(offset) };
    let source_chroma = unsafe { source.add(luma_storage_size) };
    Ok(DecodedVideoFrameView {
        pixel_format,
        luma: unsafe { slice::from_raw_parts(source, visible_luma_size) },
        chroma: unsafe { slice::from_raw_parts(source_chroma, chroma_size) },
        luma_stride: stride,
        chroma_stride: stride,
        width: layout.width,
        height: layout.height,
        pts_micros: output.attr.pts,
    })
}

fn set_format_int(
    format: *mut OH_AVFormat,
    key: *const c_char,
    value: i32,
    name: &'static str,
) -> Result<(), String> {
    if key.is_null() || !unsafe { OH_AVFormat_SetIntValue(format, key, value) } {
        return Err(format!("OH_AVFormat_SetIntValue({name}) failed"));
    }
    Ok(())
}

fn check_avcodec(code: i32, operation: &'static str) -> Result<(), String> {
    if code == AV_ERR_OK {
        Ok(())
    } else {
        Err(format!("{operation} failed with OH_AVErrCode {code}"))
    }
}

unsafe extern "C" fn on_error(_codec: *mut OH_AVCodec, error_code: i32, user_data: *mut c_void) {
    let Some(context) = callback_context(user_data) else {
        return;
    };
    if let Ok(mut state) = context.state.lock() {
        state.errors.push_back(error_code);
    }
}

unsafe extern "C" fn on_stream_changed(
    _codec: *mut OH_AVCodec,
    format: *mut OH_AVFormat,
    user_data: *mut c_void,
) {
    let Some(context) = callback_context(user_data) else {
        return;
    };
    if format.is_null() {
        return;
    }
    if let Ok(mut state) = context.state.lock() {
        let mut value = 0;
        if get_format_int(format, unsafe { OH_MD_KEY_VIDEO_PIC_WIDTH }, &mut value)
            || get_format_int(format, unsafe { OH_MD_KEY_WIDTH }, &mut value)
        {
            if value > 0 {
                state.layout.width = value as u32;
            }
        }
        if get_format_int(format, unsafe { OH_MD_KEY_VIDEO_PIC_HEIGHT }, &mut value)
            || get_format_int(format, unsafe { OH_MD_KEY_HEIGHT }, &mut value)
        {
            if value > 0 {
                state.layout.height = value as u32;
            }
        }
        if get_format_int(format, unsafe { OH_MD_KEY_VIDEO_STRIDE }, &mut value) && value > 0 {
            state.layout.stride = value as usize;
        } else {
            state.layout.stride = state.layout.width as usize;
        }
        if get_format_int(format, unsafe { OH_MD_KEY_VIDEO_SLICE_HEIGHT }, &mut value) && value > 0
        {
            state.layout.slice_height = value as usize;
        } else {
            state.layout.slice_height = state.layout.height as usize;
        }
        if get_format_int(format, unsafe { OH_MD_KEY_PIXEL_FORMAT }, &mut value) {
            state.layout.pixel_format = value;
        }
    }
}

unsafe extern "C" fn on_need_input_buffer(
    _codec: *mut OH_AVCodec,
    index: u32,
    buffer: *mut OH_AVBuffer,
    user_data: *mut c_void,
) {
    let Some(context) = callback_context(user_data) else {
        return;
    };
    if buffer.is_null() {
        if let Ok(mut state) = context.state.lock() {
            state.errors.push_back(-1);
        }
        return;
    }
    if let Ok(mut state) = context.state.lock() {
        state.inputs.push_back(InputBuffer {
            index,
            buffer: buffer as usize,
        });
    }
}

unsafe extern "C" fn on_new_output_buffer(
    _codec: *mut OH_AVCodec,
    index: u32,
    buffer: *mut OH_AVBuffer,
    user_data: *mut c_void,
) {
    let Some(context) = callback_context(user_data) else {
        return;
    };
    if buffer.is_null() {
        if let Ok(mut state) = context.state.lock() {
            state.errors.push_back(-2);
        }
        return;
    }
    let mut attr = OH_AVCodecBufferAttr::default();
    let attr_result = unsafe { OH_AVBuffer_GetBufferAttr(buffer, &mut attr) };
    if let Ok(mut state) = context.state.lock() {
        if attr_result != AV_ERR_OK {
            state.errors.push_back(attr_result);
            return;
        }
        state.outputs.push_back(OutputBuffer {
            index,
            buffer: buffer as usize,
            attr,
        });
    }
}

fn get_format_int(format: *mut OH_AVFormat, key: *const c_char, value: &mut i32) -> bool {
    !key.is_null() && unsafe { OH_AVFormat_GetIntValue(format, key, value) }
}

fn callback_context(user_data: *mut c_void) -> Option<&'static CallbackContext> {
    if user_data.is_null() {
        None
    } else {
        Some(unsafe { &*user_data.cast::<CallbackContext>() })
    }
}
