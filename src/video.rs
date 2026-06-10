//! `wandr:video` NDK backend (task 93 Phase 1) — camera capture + HW
//! `AMediaCodec` VP8 encode, and HW VP8/VP9 decode (decode-to-buffer; the
//! decode-to-surface + arbiter `Role::Video` compositing lands in Phase 4).
//!
//! Promoted from the `--probe-video` spike: `video_probe.rs` keeps the
//! standalone diagnostic CLI on top of this module's `ndk` FFI. The NDK C
//! APIs (`libcamera2ndk` / `libmediandk`) are themselves the binder clients to
//! `cameraserver` / `media.codec`, so no AIDL vendoring is needed — but they
//! ride C++ `libbinder`, which needs a running threadpool in OUR process (see
//! `ensure_binder_threadpool`).
//!
//! ‼️ Teardown discipline (the spike's cameraserver-wedge gotcha): camera +
//! codec must release cleanly on call end AND on guest death — `Drop` here
//! does the full ordered teardown and tolerates partially-initialized state,
//! so an `open` failure or a dropped WIT resource both unwind safely. Never
//! SIGKILL the process mid-transaction if avoidable.

/// Backend-side mirror of `wandr:video` `video-error` (converted in
/// `video_host_impl.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoError {
    UnsupportedCodec,
    NoHwCodec,
    CodecInitFailed,
    BadFrame,
    QueueFull,
    SurfaceUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Vp8,
    Vp9,
}

impl Codec {
    fn mime(self) -> &'static str {
        match self {
            Codec::Vp8 => "video/x-vnd.on2.vp8",
            Codec::Vp9 => "video/x-vnd.on2.vp9",
        }
    }
}

/// An on-screen rect in the owning surface's pixel space (see wit/video.wit
/// `video-rect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl VideoRect {
    fn visible(&self) -> bool {
        self.w > 0 && self.h > 0
    }
}

pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub bitrate_bps: u32,
    pub framerate: u32,
    /// front camera (the call self-view) vs back; falls back to the first
    /// enumerated camera if the requested facing doesn't exist.
    pub facing_front: bool,
    /// PiP self-view rect: the camera streams to a second (preview) surface
    /// composited at this rect. `None` = no self-view.
    pub preview: Option<VideoRect>,
}

pub struct DecoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Decode-to-surface compositing rect (task 93 Phase 4). `None` (or an
    /// empty rect) = decode-to-buffer: frames are counted + dropped — the
    /// Phase-1 diagnostic mode.
    pub rect: Option<VideoRect>,
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    /// 90 kHz RTP timestamp (wrapping), converted from the codec's µs PTS.
    pub timestamp: u32,
    pub keyframe: bool,
}

#[cfg(target_os = "android")]
pub use android::{ensure_binder_threadpool, VideoDecoder, VideoEncoder};

#[cfg(not(target_os = "android"))]
pub use desktop::{ensure_binder_threadpool, VideoDecoder, VideoEncoder};

// ── desktop stubs (the host also builds for JIT dev; camera/codec are
//    android-only, so open just fails) ─────────────────────────────────────
#[cfg(not(target_os = "android"))]
mod desktop {
    use super::*;

    pub fn ensure_binder_threadpool() -> bool {
        false
    }

    pub struct VideoEncoder;
    impl VideoEncoder {
        pub fn open(_config: &EncoderConfig) -> Result<Self, VideoError> {
            log::warn!("video: wandr:video is android-only (no camera/MediaCodec on desktop)");
            Err(VideoError::CodecInitFailed)
        }
        pub fn next_frame(&mut self) -> Option<EncodedFrame> {
            None
        }
        pub fn request_keyframe(&mut self) {}
        pub fn set_bitrate(&mut self, _bps: u32) {}
        pub fn set_preview_rect(&mut self, _rect: super::VideoRect) {}
        pub fn set_preview_visible(&mut self, _visible: bool) {}
    }

    pub struct VideoDecoder;
    impl VideoDecoder {
        pub fn open(_config: &DecoderConfig) -> Result<Self, VideoError> {
            log::warn!("video: wandr:video is android-only (no MediaCodec on desktop)");
            Err(VideoError::CodecInitFailed)
        }
        pub fn submit(&mut self, _data: &[u8], _timestamp: u32) -> Result<(), VideoError> {
            Err(VideoError::BadFrame)
        }
        pub fn decoded_frames(&self) -> u64 {
            0
        }
        pub fn set_rect(&mut self, _rect: super::VideoRect) {}
        pub fn set_visible(&mut self, _visible: bool) {}
    }
}

// ── NDK FFI (shared with video_probe.rs) ──────────────────────────────────
#[cfg(target_os = "android")]
pub mod ndk {
    #![allow(non_camel_case_types, dead_code)]
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_void};

    // Opaque NDK types (used only behind pointers).
    #[repr(C)] pub struct ACameraManager { _p: [u8; 0] }
    #[repr(C)] pub struct ACameraDevice { _p: [u8; 0] }
    #[repr(C)] pub struct ACameraMetadata { _p: [u8; 0] }
    #[repr(C)] pub struct ACameraCaptureSession { _p: [u8; 0] }
    #[repr(C)] pub struct ACaptureRequest { _p: [u8; 0] }
    #[repr(C)] pub struct ACameraOutputTarget { _p: [u8; 0] }
    #[repr(C)] pub struct ACaptureSessionOutput { _p: [u8; 0] }
    #[repr(C)] pub struct ACaptureSessionOutputContainer { _p: [u8; 0] }
    #[repr(C)] pub struct AMediaCodec { _p: [u8; 0] }
    #[repr(C)] pub struct AMediaFormat { _p: [u8; 0] }
    #[repr(C)] pub struct ANativeWindow { _p: [u8; 0] }
    #[repr(C)] pub struct AImageReader { _p: [u8; 0] }
    #[repr(C)] pub struct AImage { _p: [u8; 0] }

    #[repr(C)]
    pub struct ACameraIdList {
        pub num_cameras: c_int,
        pub camera_ids: *mut *const c_char,
    }

    #[repr(C)]
    pub struct ACameraDeviceStateCallbacks {
        pub context: *mut c_void,
        pub on_disconnected: extern "C" fn(*mut c_void, *mut ACameraDevice),
        pub on_error: extern "C" fn(*mut c_void, *mut ACameraDevice, c_int),
    }

    #[repr(C)]
    pub struct ACameraCaptureSessionStateCallbacks {
        pub context: *mut c_void,
        pub on_closed: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
        pub on_ready: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
        pub on_active: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
    }

    /// NdkCameraMetadata.h `ACameraMetadata_const_entry` (the union of const
    /// pointers collapses to one pointer slot; cast per `type`).
    #[repr(C)]
    pub struct ACameraMetadata_const_entry {
        pub tag: u32,
        pub r#type: u8,
        pub count: u32,
        pub data: *const u8,
    }

    #[repr(C)]
    pub struct AMediaCodecBufferInfo {
        pub offset: i32,
        pub size: i32,
        pub presentation_time_us: i64,
        pub flags: u32,
    }

    // camera_status_t / media_status_t: 0 == OK.
    pub const ACAMERA_OK: c_int = 0;
    pub const AMEDIA_OK: c_int = 0;
    // ACameraDevice_request_template
    pub const TEMPLATE_RECORD: c_int = 3;
    // TEMPLATE_PREVIEW (=1) avoids the qcom HAL's video-stabilization (EIS)
    // path, which needs the gyro via android.frameworks.sensorservice
    // .ISensorManager (task 93/95 — wandr-sensormanager provides it now, but
    // PREVIEW stays the proven default).
    pub const TEMPLATE_PREVIEW: c_int = 1;
    // MediaCodec
    pub const COLOR_FORMAT_SURFACE: i32 = 0x7F00_0789; // COLOR_FormatSurface
    pub const CONFIGURE_FLAG_ENCODE: u32 = 1;
    pub const BUFFER_FLAG_KEY_FRAME: u32 = 1;
    pub const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
    pub const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
    pub const INFO_OUTPUT_BUFFERS_CHANGED: isize = -3;
    pub const AIMAGE_FORMAT_YUV_420_888: i32 = 0x23;
    // NdkCameraMetadataTags.h (value cross-checked against the vendored AOSP
    // camera metadata AIDL: ANDROID_LENS_FACING = 524293).
    pub const ACAMERA_LENS_FACING: u32 = 524293; // (8 << 16) + 5
    pub const ACAMERA_LENS_FACING_FRONT: u8 = 0;
    pub const ACAMERA_LENS_FACING_BACK: u8 = 1;

    #[link(name = "camera2ndk")]
    extern "C" {
        pub fn ACameraManager_create() -> *mut ACameraManager;
        pub fn ACameraManager_delete(mgr: *mut ACameraManager);
        pub fn ACameraManager_getCameraIdList(mgr: *mut ACameraManager, out: *mut *mut ACameraIdList) -> c_int;
        pub fn ACameraManager_deleteCameraIdList(list: *mut ACameraIdList);
        pub fn ACameraManager_getCameraCharacteristics(mgr: *mut ACameraManager, id: *const c_char,
            out: *mut *mut ACameraMetadata) -> c_int;
        pub fn ACameraMetadata_getConstEntry(meta: *const ACameraMetadata, tag: u32,
            out: *mut ACameraMetadata_const_entry) -> c_int;
        pub fn ACameraMetadata_free(meta: *mut ACameraMetadata);
        pub fn ACameraManager_openCamera(mgr: *mut ACameraManager, id: *const c_char,
            cbs: *const ACameraDeviceStateCallbacks, out: *mut *mut ACameraDevice) -> c_int;
        pub fn ACameraDevice_close(dev: *mut ACameraDevice) -> c_int;
        pub fn ACameraDevice_createCaptureRequest(dev: *mut ACameraDevice, template: c_int,
            out: *mut *mut ACaptureRequest) -> c_int;
        pub fn ACameraDevice_createCaptureSession(dev: *mut ACameraDevice,
            outputs: *const ACaptureSessionOutputContainer,
            cbs: *const ACameraCaptureSessionStateCallbacks,
            out: *mut *mut ACameraCaptureSession) -> c_int;
        pub fn ACaptureSessionOutputContainer_create(out: *mut *mut ACaptureSessionOutputContainer) -> c_int;
        pub fn ACaptureSessionOutputContainer_free(c: *mut ACaptureSessionOutputContainer);
        pub fn ACaptureSessionOutput_create(win: *mut ANativeWindow, out: *mut *mut ACaptureSessionOutput) -> c_int;
        pub fn ACaptureSessionOutput_free(o: *mut ACaptureSessionOutput);
        pub fn ACaptureSessionOutputContainer_add(c: *mut ACaptureSessionOutputContainer, o: *const ACaptureSessionOutput) -> c_int;
        pub fn ACameraOutputTarget_create(win: *mut ANativeWindow, out: *mut *mut ACameraOutputTarget) -> c_int;
        pub fn ACameraOutputTarget_free(t: *mut ACameraOutputTarget);
        pub fn ACaptureRequest_addTarget(req: *mut ACaptureRequest, t: *const ACameraOutputTarget) -> c_int;
        pub fn ACaptureRequest_free(req: *mut ACaptureRequest);
        pub fn ACameraCaptureSession_setRepeatingRequest(s: *mut ACameraCaptureSession,
            cbs: *const c_void, num: c_int, reqs: *mut *mut ACaptureRequest, seq: *mut c_int) -> c_int;
        pub fn ACameraCaptureSession_stopRepeating(s: *mut ACameraCaptureSession) -> c_int;
        pub fn ACameraCaptureSession_close(s: *mut ACameraCaptureSession);
    }

    #[link(name = "mediandk")]
    extern "C" {
        pub fn AMediaCodec_createEncoderByType(mime: *const c_char) -> *mut AMediaCodec;
        pub fn AMediaCodec_createDecoderByType(mime: *const c_char) -> *mut AMediaCodec;
        pub fn AMediaCodec_createCodecByName(name: *const c_char) -> *mut AMediaCodec;
        pub fn AMediaCodec_configure(c: *mut AMediaCodec, fmt: *const AMediaFormat,
            surface: *mut ANativeWindow, crypto: *mut c_void, flags: u32) -> c_int;
        pub fn AMediaCodec_createInputSurface(c: *mut AMediaCodec, out: *mut *mut ANativeWindow) -> c_int;
        pub fn AMediaCodec_start(c: *mut AMediaCodec) -> c_int;
        pub fn AMediaCodec_stop(c: *mut AMediaCodec) -> c_int;
        pub fn AMediaCodec_delete(c: *mut AMediaCodec) -> c_int;
        pub fn AMediaCodec_signalEndOfInputStream(c: *mut AMediaCodec) -> c_int;
        pub fn AMediaCodec_setParameters(c: *mut AMediaCodec, params: *const AMediaFormat) -> c_int;
        pub fn AMediaCodec_dequeueOutputBuffer(c: *mut AMediaCodec, info: *mut AMediaCodecBufferInfo, timeout_us: i64) -> isize;
        pub fn AMediaCodec_releaseOutputBuffer(c: *mut AMediaCodec, idx: usize, render: bool) -> c_int;
        pub fn AMediaCodec_getOutputBuffer(c: *mut AMediaCodec, idx: usize, out_size: *mut usize) -> *mut u8;
        pub fn AMediaCodec_getInputBuffer(c: *mut AMediaCodec, idx: usize, out_size: *mut usize) -> *mut u8;
        pub fn AMediaCodec_dequeueInputBuffer(c: *mut AMediaCodec, timeout_us: i64) -> isize;
        pub fn AMediaCodec_queueInputBuffer(c: *mut AMediaCodec, idx: usize, offset: usize,
            size: usize, time_us: u64, flags: u32) -> c_int;
        pub fn AMediaFormat_new() -> *mut AMediaFormat;
        pub fn AMediaFormat_delete(f: *mut AMediaFormat) -> c_int;
        pub fn AMediaFormat_setString(f: *mut AMediaFormat, key: *const c_char, val: *const c_char);
        pub fn AMediaFormat_setInt32(f: *mut AMediaFormat, key: *const c_char, val: i32);
        pub fn AImageReader_new(width: i32, height: i32, format: i32, max_images: i32,
            out: *mut *mut AImageReader) -> c_int;
        pub fn AImageReader_getWindow(r: *mut AImageReader, out: *mut *mut ANativeWindow) -> c_int;
        pub fn AImageReader_acquireLatestImage(r: *mut AImageReader, out: *mut *mut AImage) -> c_int;
        pub fn AImageReader_delete(r: *mut AImageReader);
        pub fn AImage_getWidth(img: *mut AImage, out: *mut i32) -> c_int;
        pub fn AImage_getHeight(img: *mut AImage, out: *mut i32) -> c_int;
        pub fn AImage_delete(img: *mut AImage);
    }

    #[link(name = "android")]
    extern "C" {
        pub fn ANativeWindow_release(win: *mut ANativeWindow);
        pub fn ANativeWindow_getFormat(win: *mut ANativeWindow) -> i32;
        pub fn ANativeWindow_setBuffersGeometry(win: *mut ANativeWindow, w: i32, h: i32, format: i32) -> i32;
    }

    pub unsafe fn fmt_set_str(f: *mut AMediaFormat, k: &str, v: &str) {
        let ck = CString::new(k).unwrap();
        let cv = CString::new(v).unwrap();
        AMediaFormat_setString(f, ck.as_ptr(), cv.as_ptr());
    }
    pub unsafe fn fmt_set_i32(f: *mut AMediaFormat, k: &str, v: i32) {
        let ck = CString::new(k).unwrap();
        AMediaFormat_setInt32(f, ck.as_ptr(), v);
    }

    /// The NDK camera/codec paths talk to cameraserver / media.codec over C++
    /// `libbinder` (not `libbinder_ndk`) and need a running C++ binder
    /// threadpool in OUR process for callbacks + link-to-death (else "Thread
    /// Pool max thread count is 0" → every camera/codec call hangs). The NDK
    /// stub doesn't export `ABinderProcess_*` and rsbinder's threadpool is a
    /// separate context, so we go through the task-33 shim:
    /// `sf_start_binder_threadpool` → `ProcessState::self()->startThreadPool()`.
    pub unsafe fn start_binder_threadpool() -> bool {
        let lib = CString::new("libsf_surface.so").unwrap();
        let h = libc::dlopen(lib.as_ptr(), libc::RTLD_NOW);
        if h.is_null() {
            log::warn!("video: dlopen(libsf_surface.so) failed — binder threadpool not started");
            return false;
        }
        let sym = CString::new("sf_start_binder_threadpool").unwrap();
        let f = libc::dlsym(h, sym.as_ptr());
        if f.is_null() {
            log::warn!("video: sf_start_binder_threadpool not found in shim (rebuild libsf_surface.so)");
            return false;
        }
        let func: extern "C" fn() = std::mem::transmute(f);
        func();
        true
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::ndk::*;
    use super::{Codec, DecoderConfig, EncodedFrame, EncoderConfig, VideoError};
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_int, c_void};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};
    use std::sync::Once;

    /// Idempotent threadpool start. Deliberately LAZY (first encoder/decoder
    /// `open`), never at host init: app processes fork from the zygote and
    /// threads don't survive fork — codec opens only happen post-fork at call
    /// time, so the pool is always started in the process that uses it.
    pub fn ensure_binder_threadpool() -> bool {
        static ONCE: Once = Once::new();
        static OK: AtomicBool = AtomicBool::new(false);
        ONCE.call_once(|| {
            let ok = unsafe { start_binder_threadpool() };
            OK.store(ok, Relaxed);
            log::info!("video: C++ libbinder threadpool started: {ok}");
        });
        OK.load(Relaxed)
    }

    /// Child z-order for media surfaces relative to the app's own UI buffer
    /// (the SurfaceView model — negative composites BELOW the buffer; the UI
    /// punches a transparent hole). PiP self-view sits above remote video.
    const Z_REMOTE_VIDEO: i32 = -2;
    const Z_PIP_PREVIEW: i32 = -1;

    /// `sf_media_*` — the libgui shim's media-surface API (task 93 Phase 4):
    /// SurfaceControl subtrees whose producer `ANativeWindow*` feeds the
    /// decoder (decode-to-surface) or the camera (self-view preview). Child of
    /// the app's surface when one exists; top-level in a headless process.
    pub(super) mod media {
        use super::super::ndk::ANativeWindow;
        use super::super::VideoRect;
        use std::ffi::CString;
        use std::sync::OnceLock;

        struct MediaFns {
            create: unsafe extern "C" fn(i32, i32, i32, *mut *mut std::ffi::c_void) -> i32,
            set_rect: unsafe extern "C" fn(i32, i32, i32, i32, i32) -> i32,
            set_visible: unsafe extern "C" fn(i32, i32) -> i32,
            destroy: unsafe extern "C" fn(i32),
            set_opaque: unsafe extern "C" fn(i32) -> i32,
        }
        // Function pointers into libsf_surface.so, which is dlopen'd once and
        // never unloaded — safe to share across threads.
        unsafe impl Send for MediaFns {}
        unsafe impl Sync for MediaFns {}

        fn fns() -> Option<&'static MediaFns> {
            static FNS: OnceLock<Option<MediaFns>> = OnceLock::new();
            FNS.get_or_init(|| unsafe {
                let lib = CString::new("libsf_surface.so").unwrap();
                let h = libc::dlopen(lib.as_ptr(), libc::RTLD_NOW);
                if h.is_null() {
                    log::warn!("video: dlopen(libsf_surface.so) failed — no media surfaces");
                    return None;
                }
                let sym = |name: &str| {
                    let c = CString::new(name).unwrap();
                    let p = libc::dlsym(h, c.as_ptr());
                    if p.is_null() {
                        log::warn!("video: {name} missing from shim (rebuild libsf_surface.so)");
                    }
                    p
                };
                let (c, r, v, d, o) = (
                    sym("sf_media_create"),
                    sym("sf_media_set_rect"),
                    sym("sf_media_set_visible"),
                    sym("sf_media_destroy"),
                    sym("sf_set_opaque"),
                );
                if c.is_null() || r.is_null() || v.is_null() || d.is_null() || o.is_null() {
                    return None;
                }
                Some(MediaFns {
                    create: std::mem::transmute(c),
                    set_rect: std::mem::transmute(r),
                    set_visible: std::mem::transmute(v),
                    destroy: std::mem::transmute(d),
                    set_opaque: std::mem::transmute(o),
                })
            })
            .as_ref()
        }

        /// Create a media surface for a `buf_w`×`buf_h` producer at child z
        /// `z`. Returns `(slot, producer window)`; the window stays valid
        /// until `destroy(slot)`.
        pub fn create(buf_w: i32, buf_h: i32, z: i32) -> Option<(i32, *mut ANativeWindow)> {
            let f = fns()?;
            let mut win: *mut std::ffi::c_void = std::ptr::null_mut();
            let slot = unsafe { (f.create)(buf_w, buf_h, z, &mut win) };
            if slot < 0 || win.is_null() {
                log::warn!("video: sf_media_create({buf_w}x{buf_h}, z={z}) failed");
                return None;
            }
            Some((slot, win as *mut ANativeWindow))
        }

        pub fn set_rect(slot: i32, r: VideoRect) {
            if let Some(f) = fns() {
                unsafe { (f.set_rect)(slot, r.x, r.y, r.w, r.h) };
            }
        }

        pub fn set_visible(slot: i32, visible: bool) {
            if let Some(f) = fns() {
                unsafe { (f.set_visible)(slot, visible as i32) };
            }
        }

        pub fn destroy(slot: i32) {
            if let Some(f) = fns() {
                unsafe { (f.destroy)(slot) };
            }
        }

        /// Toggle the app layer's opaque flag (cleared while a behind-the-UI
        /// video surface is up so the guest's transparent hole blends).
        /// Returns false when this process has no main surface (headless).
        pub fn set_opaque(opaque: bool) -> bool {
            match fns() {
                Some(f) => unsafe { (f.set_opaque)(opaque as i32) == 0 },
                None => false,
            }
        }
    }

    /// Per-device camera error flags, written by the NDK state callbacks.
    /// Boxed so the context pointer stays stable for the device's lifetime.
    struct CamCbCtx {
        error: AtomicBool,
        code: AtomicI32,
    }

    extern "C" fn on_disconnected(_c: *mut c_void, _d: *mut ACameraDevice) {}
    extern "C" fn on_error(ctx: *mut c_void, _d: *mut ACameraDevice, err: c_int) {
        if !ctx.is_null() {
            let flags = unsafe { &*(ctx as *const CamCbCtx) };
            flags.error.store(true, Relaxed);
            flags.code.store(err, Relaxed);
        }
    }
    extern "C" fn on_session_noop(_c: *mut c_void, _s: *mut ACameraCaptureSession) {}

    /// µs PTS → wrapping 90 kHz RTP timestamp.
    fn pts_us_to_90khz(us: i64) -> u32 {
        ((us as u64).wrapping_mul(9) / 100) as u32
    }
    fn ts_90khz_to_us(ts: u32) -> u64 {
        (ts as u64).wrapping_mul(100) / 9
    }

    // ── encoder: camera → input surface → HW VP8 ──────────────────────────

    pub struct VideoEncoder {
        mgr: *mut ACameraManager,
        device: *mut ACameraDevice,
        session: *mut ACameraCaptureSession,
        req: *mut ACaptureRequest,
        target: *mut ACameraOutputTarget,
        container: *mut ACaptureSessionOutputContainer,
        out: *mut ACaptureSessionOutput,
        win: *mut ANativeWindow,
        codec: *mut AMediaCodec,
        fmt: *mut AMediaFormat,
        started: bool,
        cb_ctx: Box<CamCbCtx>,
        // PiP self-view (task 93 Phase 4): the camera streams to a second
        // (media-surface) output target; nothing crosses the WIT boundary.
        preview_slot: Option<i32>,
        preview_out: *mut ACaptureSessionOutput,
        preview_target: *mut ACameraOutputTarget,
    }

    // The NDK camera and AMediaCodec handles are documented thread-safe; all
    // access is serialized through the owning wasmtime Store in any case —
    // the raw pointers are what block the auto-impl.
    unsafe impl Send for VideoEncoder {}

    impl VideoEncoder {
        pub fn open(config: &EncoderConfig) -> Result<Self, VideoError> {
            if config.codec != Codec::Vp8 {
                // VP9/H264 HW *encode* don't exist on this class of SoC and
                // VP8 is what Signal/WebRTC negotiate — see wit/video.wit.
                return Err(VideoError::UnsupportedCodec);
            }
            if config.width == 0 || config.height == 0 || config.framerate == 0 {
                return Err(VideoError::CodecInitFailed);
            }
            ensure_binder_threadpool();

            let mut enc = VideoEncoder {
                mgr: ptr::null_mut(),
                device: ptr::null_mut(),
                session: ptr::null_mut(),
                req: ptr::null_mut(),
                target: ptr::null_mut(),
                container: ptr::null_mut(),
                out: ptr::null_mut(),
                win: ptr::null_mut(),
                codec: ptr::null_mut(),
                fmt: ptr::null_mut(),
                started: false,
                cb_ctx: Box::new(CamCbCtx { error: AtomicBool::new(false), code: AtomicI32::new(0) }),
                preview_slot: None,
                preview_out: ptr::null_mut(),
                preview_target: ptr::null_mut(),
            };
            // From here every early return goes through Drop, which tolerates
            // the partially-filled struct — the ordered-teardown guarantee.
            unsafe { enc.open_inner(config) }?;
            Ok(enc)
        }

        unsafe fn open_inner(&mut self, config: &EncoderConfig) -> Result<(), VideoError> {
            self.mgr = ACameraManager_create();
            if self.mgr.is_null() {
                log::warn!("video: ACameraManager_create -> null");
                return Err(VideoError::CodecInitFailed);
            }
            let cam_id = pick_camera(self.mgr, config.facing_front)?;
            log::info!("video: opening camera id={} (front={}) …", cam_id.to_string_lossy(), config.facing_front);

            let dev_cbs = ACameraDeviceStateCallbacks {
                context: &*self.cb_ctx as *const CamCbCtx as *mut c_void,
                on_disconnected,
                on_error,
            };
            let ost = ACameraManager_openCamera(self.mgr, cam_id.as_ptr(), &dev_cbs, &mut self.device);
            if ost != ACAMERA_OK || self.device.is_null() {
                log::warn!("video: openCamera status={ost} (permission stubs running?)");
                return Err(VideoError::CodecInitFailed);
            }

            // HW VP8 encoder, COLOR_FormatSurface, geometry from the guest's config.
            let cmime = CString::new(config.codec.mime()).unwrap();
            self.codec = AMediaCodec_createEncoderByType(cmime.as_ptr());
            if self.codec.is_null() {
                log::warn!("video: no encoder for {}", config.codec.mime());
                return Err(VideoError::NoHwCodec);
            }
            self.fmt = AMediaFormat_new();
            fmt_set_str(self.fmt, "mime", config.codec.mime());
            fmt_set_i32(self.fmt, "width", config.width as i32);
            fmt_set_i32(self.fmt, "height", config.height as i32);
            fmt_set_i32(self.fmt, "color-format", COLOR_FORMAT_SURFACE);
            fmt_set_i32(self.fmt, "bitrate", config.bitrate_bps as i32);
            fmt_set_i32(self.fmt, "frame-rate", config.framerate as i32);
            fmt_set_i32(self.fmt, "i-frame-interval", 1);
            let cfg = AMediaCodec_configure(self.codec, self.fmt, ptr::null_mut(), ptr::null_mut(), CONFIGURE_FLAG_ENCODE);
            if cfg != AMEDIA_OK {
                log::warn!("video: encoder configure status={cfg}");
                return Err(VideoError::CodecInitFailed);
            }
            let isc = AMediaCodec_createInputSurface(self.codec, &mut self.win);
            if isc != AMEDIA_OK || self.win.is_null() {
                log::warn!("video: createInputSurface status={isc}");
                return Err(VideoError::SurfaceUnavailable);
            }
            // The input surface's BufferQueue defaults to 0×0 — the camera
            // would configure a 0×0 stream and the HAL tears it down. Give it
            // the real geometry (keeping the codec's negotiated pixel format).
            let fmt_px = ANativeWindow_getFormat(self.win);
            ANativeWindow_setBuffersGeometry(self.win, config.width as i32, config.height as i32, fmt_px);
            if AMediaCodec_start(self.codec) != AMEDIA_OK {
                log::warn!("video: encoder start failed");
                return Err(VideoError::CodecInitFailed);
            }
            self.started = true;

            // Capture session → the encoder's input surface, repeating PREVIEW.
            ACaptureSessionOutput_create(self.win, &mut self.out);
            ACaptureSessionOutputContainer_create(&mut self.container);
            ACaptureSessionOutputContainer_add(self.container, self.out);
            // PiP self-view: a second camera output target onto a media
            // surface. Same stream size as the encode stream (a real,
            // supported camera config — the on-screen rect scales it).
            let mut preview_win: *mut ANativeWindow = ptr::null_mut();
            if let Some(rect) = config.preview.filter(|r| r.visible()) {
                match media::create(config.width as i32, config.height as i32, Z_PIP_PREVIEW) {
                    Some((slot, win)) => {
                        self.preview_slot = Some(slot);
                        preview_win = win;
                        media::set_rect(slot, rect);
                        media::set_visible(slot, true);
                        ACaptureSessionOutput_create(preview_win, &mut self.preview_out);
                        ACaptureSessionOutputContainer_add(self.container, self.preview_out);
                    }
                    None => {
                        // Self-view is cosmetic — log + carry on encoding.
                        log::warn!("video: preview media surface unavailable — no self-view");
                    }
                }
            }
            let sess_cbs = ACameraCaptureSessionStateCallbacks {
                context: ptr::null_mut(),
                on_closed: on_session_noop,
                on_ready: on_session_noop,
                on_active: on_session_noop,
            };
            let sst = ACameraDevice_createCaptureSession(self.device, self.container, &sess_cbs, &mut self.session);
            if sst != ACAMERA_OK || self.session.is_null() {
                log::warn!("video: createCaptureSession status={sst}");
                return Err(VideoError::CodecInitFailed);
            }
            ACameraDevice_createCaptureRequest(self.device, TEMPLATE_PREVIEW, &mut self.req);
            if self.req.is_null() {
                return Err(VideoError::CodecInitFailed);
            }
            ACameraOutputTarget_create(self.win, &mut self.target);
            ACaptureRequest_addTarget(self.req, self.target);
            if !preview_win.is_null() {
                ACameraOutputTarget_create(preview_win, &mut self.preview_target);
                ACaptureRequest_addTarget(self.req, self.preview_target);
            }
            let mut seq: c_int = 0;
            let rst = ACameraCaptureSession_setRepeatingRequest(self.session, ptr::null(), 1, &mut self.req, &mut seq);
            if rst != ACAMERA_OK {
                log::warn!("video: setRepeatingRequest status={rst}");
                return Err(VideoError::CodecInitFailed);
            }
            log::info!("video: encoder live — {}x{} @ {} fps, {} bps",
                config.width, config.height, config.framerate, config.bitrate_bps);
            Ok(())
        }

        /// Non-blocking pull of the next encoded frame (the guest polls at
        /// frame cadence; see the WIT note re a future callback alternative).
        pub fn next_frame(&mut self) -> Option<EncodedFrame> {
            if self.cb_ctx.error.swap(false, Relaxed) {
                log::warn!("video: camera onError code={}", self.cb_ctx.code.load(Relaxed));
            }
            let mut info = AMediaCodecBufferInfo { offset: 0, size: 0, presentation_time_us: 0, flags: 0 };
            // A couple of retries so format/buffers-changed notifications and
            // codec-config buffers don't cost the guest a whole poll interval.
            for _ in 0..4 {
                let idx = unsafe { AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, 0) };
                if idx >= 0 {
                    let mut osz: usize = 0;
                    let obuf = unsafe { AMediaCodec_getOutputBuffer(self.codec, idx as usize, &mut osz) };
                    let mut data = Vec::new();
                    if !obuf.is_null() && info.size > 0 {
                        let off = info.offset.max(0) as usize;
                        let n = (info.size as usize).min(osz.saturating_sub(off));
                        data = unsafe { std::slice::from_raw_parts(obuf.add(off), n) }.to_vec();
                    }
                    let keyframe = info.flags & BUFFER_FLAG_KEY_FRAME != 0;
                    let codec_config = info.flags & BUFFER_FLAG_CODEC_CONFIG != 0;
                    let ts = pts_us_to_90khz(info.presentation_time_us);
                    unsafe { AMediaCodec_releaseOutputBuffer(self.codec, idx as usize, false) };
                    if data.is_empty() || codec_config {
                        continue; // CSD/empty buffers never reach the RTP layer
                    }
                    return Some(EncodedFrame { data, timestamp: ts, keyframe });
                } else if idx == INFO_OUTPUT_FORMAT_CHANGED || idx == INFO_OUTPUT_BUFFERS_CHANGED {
                    continue;
                } else {
                    return None; // try-again-later
                }
            }
            None
        }

        /// Force a sync frame (receiver's RTCP PLI/FIR).
        pub fn request_keyframe(&mut self) {
            unsafe {
                let f = AMediaFormat_new();
                fmt_set_i32(f, "request-sync", 0); // AMEDIACODEC_KEY_REQUEST_SYNC_FRAME
                let st = AMediaCodec_setParameters(self.codec, f);
                AMediaFormat_delete(f);
                if st != AMEDIA_OK {
                    log::warn!("video: request-sync setParameters status={st}");
                }
            }
        }

        /// Adapt to congestion (guest drives this from REMB/TWCC).
        pub fn set_bitrate(&mut self, bps: u32) {
            unsafe {
                let f = AMediaFormat_new();
                fmt_set_i32(f, "video-bitrate", bps as i32); // AMEDIACODEC_KEY_VIDEO_BITRATE
                let st = AMediaCodec_setParameters(self.codec, f);
                AMediaFormat_delete(f);
                if st != AMEDIA_OK {
                    log::warn!("video: video-bitrate setParameters status={st}");
                }
            }
        }

        /// Move/resize the PiP self-view (no-op without a preview surface).
        pub fn set_preview_rect(&mut self, rect: super::VideoRect) {
            if let Some(slot) = self.preview_slot {
                media::set_rect(slot, rect);
            }
        }

        /// Show/hide the PiP self-view (no-op without a preview surface).
        pub fn set_preview_visible(&mut self, visible: bool) {
            if let Some(slot) = self.preview_slot {
                media::set_visible(slot, visible);
            }
        }
    }

    impl Drop for VideoEncoder {
        // The probe's proven teardown order, null-tolerant so a failed open
        // unwinds the same way. Skipping/reordering this wedges cameraserver.
        fn drop(&mut self) {
            unsafe {
                if self.started {
                    let _ = AMediaCodec_signalEndOfInputStream(self.codec);
                }
                if !self.session.is_null() {
                    ACameraCaptureSession_stopRepeating(self.session);
                    ACameraCaptureSession_close(self.session);
                }
                if !self.req.is_null() { ACaptureRequest_free(self.req); }
                if !self.target.is_null() { ACameraOutputTarget_free(self.target); }
                if !self.preview_target.is_null() { ACameraOutputTarget_free(self.preview_target); }
                if !self.container.is_null() { ACaptureSessionOutputContainer_free(self.container); }
                if !self.out.is_null() { ACaptureSessionOutput_free(self.out); }
                if !self.preview_out.is_null() { ACaptureSessionOutput_free(self.preview_out); }
                if !self.device.is_null() { ACameraDevice_close(self.device); }
                if !self.codec.is_null() {
                    if self.started { AMediaCodec_stop(self.codec); }
                    AMediaCodec_delete(self.codec);
                }
                if !self.fmt.is_null() { AMediaFormat_delete(self.fmt); }
                if !self.win.is_null() { ANativeWindow_release(self.win); }
                if !self.mgr.is_null() { ACameraManager_delete(self.mgr); }
            }
            // After the camera (producer) is closed — the surface can go.
            if let Some(slot) = self.preview_slot.take() {
                media::destroy(slot);
            }
            log::info!("video: encoder torn down");
        }
    }

    /// Pick the camera whose ACAMERA_LENS_FACING matches; fall back to the
    /// first enumerated id (the probe's behavior) if no match / query fails.
    unsafe fn pick_camera(mgr: *mut ACameraManager, front: bool) -> Result<CString, VideoError> {
        let mut id_list: *mut ACameraIdList = ptr::null_mut();
        if ACameraManager_getCameraIdList(mgr, &mut id_list) != ACAMERA_OK || id_list.is_null() {
            log::warn!("video: getCameraIdList failed");
            return Err(VideoError::CodecInitFailed);
        }
        let n = (*id_list).num_cameras;
        if n <= 0 {
            log::warn!("video: 0 cameras enumerated");
            ACameraManager_deleteCameraIdList(id_list);
            return Err(VideoError::CodecInitFailed);
        }
        let want = if front { ACAMERA_LENS_FACING_FRONT } else { ACAMERA_LENS_FACING_BACK };
        let mut chosen: Option<CString> = None;
        for i in 0..n {
            let id_ptr = *(*id_list).camera_ids.add(i as usize);
            let mut meta: *mut ACameraMetadata = ptr::null_mut();
            if ACameraManager_getCameraCharacteristics(mgr, id_ptr, &mut meta) == ACAMERA_OK && !meta.is_null() {
                let mut entry = ACameraMetadata_const_entry { tag: 0, r#type: 0, count: 0, data: ptr::null() };
                let got = ACameraMetadata_getConstEntry(meta, ACAMERA_LENS_FACING, &mut entry);
                let facing = if got == ACAMERA_OK && entry.count > 0 && !entry.data.is_null() {
                    Some(*entry.data)
                } else {
                    None
                };
                ACameraMetadata_free(meta);
                if facing == Some(want) {
                    chosen = Some(CStr::from_ptr(id_ptr).to_owned());
                    break;
                }
            }
        }
        let id = chosen.unwrap_or_else(|| {
            let first = CStr::from_ptr(*(*id_list).camera_ids.add(0)).to_owned();
            log::info!("video: no {} camera found — falling back to id={}",
                if front { "front" } else { "back" }, first.to_string_lossy());
            first
        });
        ACameraManager_deleteCameraIdList(id_list);
        Ok(id)
    }

    // ── decoder: guest pushes encoded frames → HW decode ──────────────────
    // Decode-to-SURFACE when the config carries a rect (task 93 Phase 4): the
    // codec renders straight into a media surface composited below the app's
    // (hole-punched) UI — decoded pixels never re-enter the guest. Without a
    // rect it falls back to decode-to-buffer (count + drop; diagnostics).

    pub struct VideoDecoder {
        codec: *mut AMediaCodec,
        fmt: *mut AMediaFormat,
        started: bool,
        decoded: u64,
        slot: Option<i32>,
        /// True when this decoder cleared the app layer's opaque flag (so it
        /// must restore it on teardown).
        opaque_cleared: bool,
    }

    // Same justification as VideoEncoder: AMediaCodec is thread-safe and
    // access is store-serialized; only the raw pointer blocks the auto-impl.
    unsafe impl Send for VideoDecoder {}

    impl VideoDecoder {
        pub fn open(config: &DecoderConfig) -> Result<Self, VideoError> {
            ensure_binder_threadpool();
            let mut dec = VideoDecoder {
                codec: ptr::null_mut(),
                fmt: ptr::null_mut(),
                started: false,
                decoded: 0,
                slot: None,
                opaque_cleared: false,
            };
            // Decode-to-surface: allocate the compositing surface first (its
            // window is the codec's render target). Failure here is fatal —
            // the caller asked for on-screen video.
            let mut surface: *mut ANativeWindow = ptr::null_mut();
            if let Some(rect) = config.rect.filter(|r| r.visible()) {
                let (slot, win) = media::create(
                    config.width.max(1) as i32,
                    config.height.max(1) as i32,
                    Z_REMOTE_VIDEO,
                )
                .ok_or(VideoError::SurfaceUnavailable)?;
                dec.slot = Some(slot);
                surface = win;
                media::set_rect(slot, rect);
                media::set_visible(slot, true);
                // Let the guest's transparent hole blend (no-op headless).
                dec.opaque_cleared = media::set_opaque(false);
            }
            unsafe {
                let dmime = CString::new(config.codec.mime()).unwrap();
                dec.codec = AMediaCodec_createDecoderByType(dmime.as_ptr());
                if dec.codec.is_null() {
                    log::warn!("video: no decoder for {}", config.codec.mime());
                    return Err(VideoError::NoHwCodec);
                }
                dec.fmt = AMediaFormat_new();
                fmt_set_str(dec.fmt, "mime", config.codec.mime());
                fmt_set_i32(dec.fmt, "width", config.width.max(1) as i32);
                fmt_set_i32(dec.fmt, "height", config.height.max(1) as i32);
                let dcfg = AMediaCodec_configure(dec.codec, dec.fmt, surface, ptr::null_mut(), 0);
                if dcfg != AMEDIA_OK {
                    log::warn!("video: decoder configure status={dcfg}");
                    return Err(VideoError::CodecInitFailed);
                }
                if AMediaCodec_start(dec.codec) != AMEDIA_OK {
                    log::warn!("video: decoder start failed");
                    return Err(VideoError::CodecInitFailed);
                }
            }
            dec.started = true;
            log::info!("video: decoder live — {} {}x{} ({})",
                config.codec.mime(), config.width, config.height,
                if dec.slot.is_some() { "decode-to-surface" } else { "decode-to-buffer" });
            Ok(dec)
        }

        /// Push one reassembled encoded frame; drains decoder output as a side
        /// effect (decode-to-buffer: decoded frames are counted + dropped
        /// until Phase 4 renders them).
        pub fn submit(&mut self, data: &[u8], timestamp: u32) -> Result<(), VideoError> {
            if data.is_empty() {
                return Err(VideoError::BadFrame);
            }
            unsafe {
                self.drain();
                let di = AMediaCodec_dequeueInputBuffer(self.codec, 10_000);
                if di < 0 {
                    self.drain();
                    return Err(VideoError::QueueFull);
                }
                let mut isz: usize = 0;
                let ibuf = AMediaCodec_getInputBuffer(self.codec, di as usize, &mut isz);
                if ibuf.is_null() || data.len() > isz {
                    log::warn!("video: input buffer null or frame too large ({} > {isz})", data.len());
                    return Err(VideoError::BadFrame);
                }
                ptr::copy_nonoverlapping(data.as_ptr(), ibuf, data.len());
                AMediaCodec_queueInputBuffer(self.codec, di as usize, 0, data.len(), ts_90khz_to_us(timestamp), 0);
                self.drain();
            }
            Ok(())
        }

        pub fn decoded_frames(&self) -> u64 {
            self.decoded
        }

        /// Move/resize the video surface (no-op in decode-to-buffer mode).
        pub fn set_rect(&mut self, rect: super::VideoRect) {
            if let Some(slot) = self.slot {
                media::set_rect(slot, rect);
            }
        }

        /// Show/hide the video surface (no-op in decode-to-buffer mode).
        pub fn set_visible(&mut self, visible: bool) {
            if let Some(slot) = self.slot {
                media::set_visible(slot, visible);
            }
        }

        unsafe fn drain(&mut self) {
            // render=true sends the decoded buffer to the media surface
            // (decode-to-surface); with no surface it just recycles.
            let render = self.slot.is_some();
            let mut info = AMediaCodecBufferInfo { offset: 0, size: 0, presentation_time_us: 0, flags: 0 };
            for _ in 0..16 {
                let idx = AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, 0);
                if idx >= 0 {
                    self.decoded += 1;
                    AMediaCodec_releaseOutputBuffer(self.codec, idx as usize, render);
                } else if idx == INFO_OUTPUT_FORMAT_CHANGED || idx == INFO_OUTPUT_BUFFERS_CHANGED {
                    continue;
                } else {
                    break;
                }
            }
        }
    }

    impl Drop for VideoDecoder {
        fn drop(&mut self) {
            unsafe {
                if !self.codec.is_null() {
                    if self.started { AMediaCodec_stop(self.codec); }
                    AMediaCodec_delete(self.codec);
                }
                if !self.fmt.is_null() { AMediaFormat_delete(self.fmt); }
            }
            // After the codec (producer) is gone — release the surface and
            // restore the app layer's opacity.
            if let Some(slot) = self.slot.take() {
                media::destroy(slot);
            }
            if self.opaque_cleared {
                media::set_opaque(true);
            }
            log::info!("video: decoder torn down ({} frames decoded)", self.decoded);
        }
    }
}
