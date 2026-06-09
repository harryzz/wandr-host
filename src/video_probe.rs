//! `wandr-host --probe-video` — task-93 spike: prove camera → HW VP8 encode works
//! end-to-end under `--no-art`, with minimal code, via the NDK **Surface path**.
//!
//! Approach (see `tasks/93-video-call.md`): the camera writes frames straight
//! into the encoder's input surface (`AMediaCodec_createInputSurface`), so there
//! are no manual YUV buffer copies — one window is both the capture-session output
//! and the codec input. We use the NDK C APIs (`libcamera2ndk` / `libmediandk`),
//! NOT rsbinder + vendored AIDL: the NDK libs are themselves the binder clients to
//! `cameraserver` / `media.codec` (via `libbinder_ndk`, already linked), so no AIDL
//! vendoring is needed for the spike.
//!
//! It answers the two task-93 risks:
//!   1. Does `ACameraManager_openCamera` SUCCEED under `--no-art`? (cameraserver is
//!      up, but `open()` may hit a permission/AppOps path like the audio path did.)
//!   2. Can the HW VP8 encoder (`OMX.qcom.video.encoder.vp8`) take camera frames and
//!      produce a bitstream — at what fps / first-frame latency?
//!
//! Output is a plain report to stdout. Android-only; off-android is a stub.

#[cfg(target_os = "android")]
pub fn probe_video() {
    android::run();
}

#[cfg(not(target_os = "android"))]
pub fn probe_video() {
    println!("--probe-video: android-only");
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int, c_void};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};
    use std::time::Instant;

    /// `println!` is block-buffered to a pipe, so a hang/crash loses all output.
    /// `p!` writes + flushes each line so probe progress is always visible.
    macro_rules! p {
        ($($arg:tt)*) => {{
            use std::io::Write;
            let mut o = std::io::stdout();
            let _ = writeln!(o, $($arg)*);
            let _ = o.flush();
        }};
    }

    // ── Opaque NDK types (used only behind pointers) ─────────────────────────
    #[repr(C)] pub struct ACameraManager { _p: [u8; 0] }
    #[repr(C)] pub struct ACameraDevice { _p: [u8; 0] }
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
    struct ACameraIdList {
        num_cameras: c_int,
        camera_ids: *mut *const c_char,
    }

    #[repr(C)]
    struct ACameraDeviceStateCallbacks {
        context: *mut c_void,
        on_disconnected: extern "C" fn(*mut c_void, *mut ACameraDevice),
        on_error: extern "C" fn(*mut c_void, *mut ACameraDevice, c_int),
    }

    #[repr(C)]
    struct ACameraCaptureSessionStateCallbacks {
        context: *mut c_void,
        on_closed: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
        on_ready: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
        on_active: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
    }

    #[repr(C)]
    struct AMediaCodecBufferInfo {
        offset: i32,
        size: i32,
        presentation_time_us: i64,
        flags: u32,
    }

    // camera_status_t / media_status_t: 0 == OK.
    const ACAMERA_OK: c_int = 0;
    const AMEDIA_OK: c_int = 0;
    // ACameraDevice_request_template
    const TEMPLATE_RECORD: c_int = 3;
    // TEMPLATE_PREVIEW (=1) avoids the qcom HAL's video-stabilization (EIS) path,
    // which needs the gyro via android.frameworks.sensorservice.ISensorManager
    // (gone under --no-art → HAL SIGABRT at startChannelLocked). task 93.
    const TEMPLATE_PREVIEW: c_int = 1;
    // MediaCodec
    const COLOR_FORMAT_SURFACE: i32 = 0x7F00_0789; // COLOR_FormatSurface
    const CONFIGURE_FLAG_ENCODE: u32 = 1;
    const BUFFER_FLAG_KEY_FRAME: u32 = 1;
    const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;

    #[link(name = "camera2ndk")]
    extern "C" {
        fn ACameraManager_create() -> *mut ACameraManager;
        fn ACameraManager_delete(mgr: *mut ACameraManager);
        fn ACameraManager_getCameraIdList(mgr: *mut ACameraManager, out: *mut *mut ACameraIdList) -> c_int;
        fn ACameraManager_deleteCameraIdList(list: *mut ACameraIdList);
        fn ACameraManager_openCamera(mgr: *mut ACameraManager, id: *const c_char,
            cbs: *const ACameraDeviceStateCallbacks, out: *mut *mut ACameraDevice) -> c_int;
        fn ACameraDevice_close(dev: *mut ACameraDevice) -> c_int;
        fn ACameraDevice_createCaptureRequest(dev: *mut ACameraDevice, template: c_int,
            out: *mut *mut ACaptureRequest) -> c_int;
        fn ACameraDevice_createCaptureSession(dev: *mut ACameraDevice,
            outputs: *const ACaptureSessionOutputContainer,
            cbs: *const ACameraCaptureSessionStateCallbacks,
            out: *mut *mut ACameraCaptureSession) -> c_int;
        fn ACaptureSessionOutputContainer_create(out: *mut *mut ACaptureSessionOutputContainer) -> c_int;
        fn ACaptureSessionOutputContainer_free(c: *mut ACaptureSessionOutputContainer);
        fn ACaptureSessionOutput_create(win: *mut ANativeWindow, out: *mut *mut ACaptureSessionOutput) -> c_int;
        fn ACaptureSessionOutput_free(o: *mut ACaptureSessionOutput);
        fn ACaptureSessionOutputContainer_add(c: *mut ACaptureSessionOutputContainer, o: *const ACaptureSessionOutput) -> c_int;
        fn ACameraOutputTarget_create(win: *mut ANativeWindow, out: *mut *mut ACameraOutputTarget) -> c_int;
        fn ACameraOutputTarget_free(t: *mut ACameraOutputTarget);
        fn ACaptureRequest_addTarget(req: *mut ACaptureRequest, t: *const ACameraOutputTarget) -> c_int;
        fn ACaptureRequest_free(req: *mut ACaptureRequest);
        fn ACameraCaptureSession_setRepeatingRequest(s: *mut ACameraCaptureSession,
            cbs: *const c_void, num: c_int, reqs: *mut *mut ACaptureRequest, seq: *mut c_int) -> c_int;
        fn ACameraCaptureSession_stopRepeating(s: *mut ACameraCaptureSession) -> c_int;
        fn ACameraCaptureSession_close(s: *mut ACameraCaptureSession);
    }

    #[link(name = "mediandk")]
    extern "C" {
        fn AMediaCodec_createEncoderByType(mime: *const c_char) -> *mut AMediaCodec;
        fn AMediaCodec_createCodecByName(name: *const c_char) -> *mut AMediaCodec;
        fn AMediaCodec_configure(c: *mut AMediaCodec, fmt: *const AMediaFormat,
            surface: *mut ANativeWindow, crypto: *mut c_void, flags: u32) -> c_int;
        fn AMediaCodec_createInputSurface(c: *mut AMediaCodec, out: *mut *mut ANativeWindow) -> c_int;
        fn AMediaCodec_start(c: *mut AMediaCodec) -> c_int;
        fn AMediaCodec_stop(c: *mut AMediaCodec) -> c_int;
        fn AMediaCodec_delete(c: *mut AMediaCodec) -> c_int;
        fn AMediaCodec_signalEndOfInputStream(c: *mut AMediaCodec) -> c_int;
        fn AMediaCodec_dequeueOutputBuffer(c: *mut AMediaCodec, info: *mut AMediaCodecBufferInfo, timeout_us: i64) -> isize;
        fn AMediaCodec_releaseOutputBuffer(c: *mut AMediaCodec, idx: usize, render: bool) -> c_int;
        fn AMediaFormat_new() -> *mut AMediaFormat;
        fn AMediaFormat_delete(f: *mut AMediaFormat) -> c_int;
        fn AMediaFormat_setString(f: *mut AMediaFormat, key: *const c_char, val: *const c_char);
        fn AMediaFormat_setInt32(f: *mut AMediaFormat, key: *const c_char, val: i32);
        // AImageReader (also in libmediandk) — a camera target with EXPLICIT
        // dimensions, to prove the camera delivers frames under --no-art (decisive
        // test, sidestepping the encoder input-surface's 0x0 geometry).
        fn AImageReader_new(width: i32, height: i32, format: i32, max_images: i32,
            out: *mut *mut AImageReader) -> c_int;
        fn AImageReader_getWindow(r: *mut AImageReader, out: *mut *mut ANativeWindow) -> c_int;
        fn AImageReader_acquireLatestImage(r: *mut AImageReader, out: *mut *mut AImage) -> c_int;
        fn AImageReader_delete(r: *mut AImageReader);
        fn AImage_getWidth(img: *mut AImage, out: *mut i32) -> c_int;
        fn AImage_getHeight(img: *mut AImage, out: *mut i32) -> c_int;
        fn AImage_delete(img: *mut AImage);
    }

    #[link(name = "android")]
    extern "C" {
        fn ANativeWindow_release(win: *mut ANativeWindow);
        fn ANativeWindow_getFormat(win: *mut ANativeWindow) -> i32;
        fn ANativeWindow_setBuffersGeometry(win: *mut ANativeWindow, w: i32, h: i32, format: i32) -> i32;
    }

    // The NDK Camera2 path talks to cameraserver over C++ libbinder and needs a
    // running binder threadpool in OUR process to receive its callbacks + link-to-
    // death (else "Thread Pool max thread count is 0" → camera ops hang). The NDK
    // stub `libbinder_ndk` does NOT export `ABinderProcess_*`, and rsbinder's
    // threadpool is a separate context that doesn't service C++ libbinder. So we
    // start the C++ libbinder threadpool through the task-33 shim (which links the
    // device's `libbinder`): `sf_start_binder_threadpool` → `ProcessState::self()
    // ->startThreadPool()`. dlopen'd by name (resolved via LD_LIBRARY_PATH, same as
    // the host's own shim load).
    unsafe fn start_binder_threadpool() -> bool {
        let lib = CString::new("libsf_surface.so").unwrap();
        let h = libc::dlopen(lib.as_ptr(), libc::RTLD_NOW);
        if h.is_null() {
            p!("WARN: dlopen(libsf_surface.so) failed — binder threadpool not started");
            return false;
        }
        let sym = CString::new("sf_start_binder_threadpool").unwrap();
        let f = libc::dlsym(h, sym.as_ptr());
        if f.is_null() {
            p!("WARN: sf_start_binder_threadpool not found in shim (rebuild libsf_surface.so)");
            return false;
        }
        let func: extern "C" fn() = std::mem::transmute(f);
        func();
        true
    }

    static CAM_ERROR: AtomicBool = AtomicBool::new(false);
    static CAM_ERR_CODE: AtomicI32 = AtomicI32::new(0);

    extern "C" fn on_disconnected(_c: *mut c_void, _d: *mut ACameraDevice) {}
    extern "C" fn on_error(_c: *mut c_void, _d: *mut ACameraDevice, err: c_int) {
        CAM_ERROR.store(true, Relaxed);
        CAM_ERR_CODE.store(err, Relaxed);
    }
    extern "C" fn on_session_noop(_c: *mut c_void, _s: *mut ACameraCaptureSession) {}

    unsafe fn fmt_set_str(f: *mut AMediaFormat, k: &str, v: &str) {
        let ck = CString::new(k).unwrap();
        let cv = CString::new(v).unwrap();
        AMediaFormat_setString(f, ck.as_ptr(), cv.as_ptr());
    }
    unsafe fn fmt_set_i32(f: *mut AMediaFormat, k: &str, v: i32) {
        let ck = CString::new(k).unwrap();
        AMediaFormat_setInt32(f, ck.as_ptr(), v);
    }

    pub fn run() {
        const W: i32 = 640;
        const H: i32 = 480;
        const VP8: &str = "video/x-vnd.on2.vp8";
        // Optional explicit encoder name as the arg after `--probe-video`
        // (e.g. `c2.android.vp8.encoder`/`OMX.google.vp8.encoder` for SW,
        // `OMX.qcom.video.encoder.vp8` for HW). Default = createEncoderByType(VP8).
        let codec_name: Option<String> = std::env::args()
            .skip_while(|a| a != "--probe-video")
            .nth(1)
            .filter(|s| !s.starts_with("--"));
        // `--probe-video imagereader` → camera → AImageReader(YUV) frame-count test
        // (proves camera delivery under --no-art with explicit dims).
        if codec_name.as_deref() == Some("imagereader") {
            p!("=== wandr-host --probe-video — camera → AImageReader YUV ({W}x{H}) ===");
            unsafe { run_imagereader(W, H) }
            return;
        }
        match &codec_name {
            Some(n) => p!("=== wandr-host --probe-video — camera → encode via '{n}' ({W}x{H}) ==="),
            None => p!("=== wandr-host --probe-video — camera → HW VP8 encode ({W}x{H}) ==="),
        }
        unsafe { run_inner(W, H, VP8, codec_name.as_deref()) }
    }

    const AIMAGE_FORMAT_YUV_420_888: i32 = 0x23;

    // Decisive camera-delivery test: camera → AImageReader (explicit dims, no
    // encoder). If frames arrive, camera capture works end-to-end under --no-art
    // and the encoder-surface 0x0 was the only gap.
    unsafe fn run_imagereader(w: i32, h: i32) {
        let tp = start_binder_threadpool();
        p!("binder threadpool started: {tp}");
        let mgr = ACameraManager_create();
        if mgr.is_null() { p!("FAIL: ACameraManager_create -> null"); return; }
        let mut id_list: *mut ACameraIdList = ptr::null_mut();
        if ACameraManager_getCameraIdList(mgr, &mut id_list) != ACAMERA_OK || id_list.is_null() {
            p!("FAIL: getCameraIdList"); ACameraManager_delete(mgr); return;
        }
        let n = (*id_list).num_cameras;
        p!("cameras visible: {n}");
        if n <= 0 { cleanup_mgr(mgr, id_list); return; }
        let cam_id_ptr = *(*id_list).camera_ids.add(0);
        let cam_id = CStr::from_ptr(cam_id_ptr).to_string_lossy().into_owned();

        p!("opening camera id={cam_id} …");
        let dev_cbs = ACameraDeviceStateCallbacks { context: ptr::null_mut(), on_disconnected, on_error };
        let mut device: *mut ACameraDevice = ptr::null_mut();
        let ost = ACameraManager_openCamera(mgr, cam_id_ptr, &dev_cbs, &mut device);
        if ost != ACAMERA_OK || device.is_null() {
            p!("FAIL: openCamera status={ost}"); cleanup_mgr(mgr, id_list); return;
        }
        p!("camera OPENED ✓");

        // AImageReader with EXPLICIT dims → its window has 640x480 (no 0x0).
        let mut reader: *mut AImageReader = ptr::null_mut();
        let rs = AImageReader_new(w, h, AIMAGE_FORMAT_YUV_420_888, 4, &mut reader);
        if rs != AMEDIA_OK || reader.is_null() {
            p!("FAIL: AImageReader_new status={rs}");
            ACameraDevice_close(device); cleanup_mgr(mgr, id_list); return;
        }
        let mut win: *mut ANativeWindow = ptr::null_mut();
        AImageReader_getWindow(reader, &mut win);
        p!("AImageReader {w}x{h} YUV ready");

        // Capture session → the ImageReader window.
        let mut out: *mut ACaptureSessionOutput = ptr::null_mut();
        let mut container: *mut ACaptureSessionOutputContainer = ptr::null_mut();
        ACaptureSessionOutput_create(win, &mut out);
        ACaptureSessionOutputContainer_create(&mut container);
        ACaptureSessionOutputContainer_add(container, out);
        let sess_cbs = ACameraCaptureSessionStateCallbacks {
            context: ptr::null_mut(),
            on_closed: on_session_noop, on_ready: on_session_noop, on_active: on_session_noop,
        };
        let mut session: *mut ACameraCaptureSession = ptr::null_mut();
        let sst = ACameraDevice_createCaptureSession(device, container, &sess_cbs, &mut session);
        if sst != ACAMERA_OK || session.is_null() {
            p!("FAIL: createCaptureSession status={sst}");
            AImageReader_delete(reader); ACameraDevice_close(device); cleanup_mgr(mgr, id_list); return;
        }
        let mut req: *mut ACaptureRequest = ptr::null_mut();
        ACameraDevice_createCaptureRequest(device, TEMPLATE_RECORD, &mut req);
        let mut target: *mut ACameraOutputTarget = ptr::null_mut();
        ACameraOutputTarget_create(win, &mut target);
        ACaptureRequest_addTarget(req, target);
        let mut seq: c_int = 0;
        let rst = ACameraCaptureSession_setRepeatingRequest(session, ptr::null(), 1, &mut req, &mut seq);
        p!("repeating capture set (status={rst}); draining ImageReader ~5s …");

        let start = Instant::now();
        let mut frames = 0u64;
        let mut first_ms: i128 = -1;
        let mut dims = (0i32, 0i32);
        while start.elapsed().as_secs() < 5 {
            let mut img: *mut AImage = ptr::null_mut();
            if AImageReader_acquireLatestImage(reader, &mut img) == AMEDIA_OK && !img.is_null() {
                if first_ms < 0 { first_ms = start.elapsed().as_millis() as i128; }
                if dims.0 == 0 { AImage_getWidth(img, &mut dims.0); AImage_getHeight(img, &mut dims.1); }
                frames += 1;
                AImage_delete(img);
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let secs = start.elapsed().as_secs_f64();
        p!("──────── RESULT ────────");
        p!("camera-open    : OK ({n} cameras, id={cam_id})");
        p!("captured frames: {frames} in {secs:.1}s = {:.1} fps", frames as f64 / secs);
        p!("frame dims     : {}x{}", dims.0, dims.1);
        p!("first-frame    : {first_ms} ms");
        if frames > 0 {
            p!("VERDICT: camera DELIVERS frames under --no-art ✓ — capture works end-to-end");
        } else {
            p!("VERDICT: still 0 frames even with explicit-dim ImageReader — deeper camera/session issue");
        }
        if CAM_ERROR.load(Relaxed) { p!("camera onError: code={}", CAM_ERR_CODE.load(Relaxed)); }

        ACameraCaptureSession_stopRepeating(session);
        ACameraCaptureSession_close(session);
        ACaptureRequest_free(req);
        ACameraOutputTarget_free(target);
        ACaptureSessionOutputContainer_free(container);
        ACaptureSessionOutput_free(out);
        AImageReader_delete(reader);
        ACameraDevice_close(device);
        cleanup_mgr(mgr, id_list);
    }

    unsafe fn run_inner(w: i32, h: i32, vp8: &str, codec_name: Option<&str>) {
        // 0. Start the (C++ libbinder) binder threadpool BEFORE touching the camera
        //    — the NDK Camera2 client needs it to service cameraserver callbacks;
        //    without it open/createCaptureSession hang ("Thread Pool max thread
        //    count is 0").
        let tp = start_binder_threadpool();
        p!("binder threadpool started: {tp}");

        // 1. Camera manager + enumerate.
        let mgr = ACameraManager_create();
        if mgr.is_null() { p!("FAIL: ACameraManager_create -> null"); return; }
        let mut id_list: *mut ACameraIdList = ptr::null_mut();
        let st = ACameraManager_getCameraIdList(mgr, &mut id_list);
        if st != ACAMERA_OK || id_list.is_null() {
            p!("FAIL: getCameraIdList status={st}");
            ACameraManager_delete(mgr);
            return;
        }
        let n = (*id_list).num_cameras;
        p!("cameras visible: {n}");
        if n <= 0 {
            p!("FAIL: cameraserver returned 0 cameras (HAL not enumerated under --no-art?)");
            ACameraManager_deleteCameraIdList(id_list);
            ACameraManager_delete(mgr);
            return;
        }
        let cam_id_ptr = *(*id_list).camera_ids.add(0);
        let cam_id = CStr::from_ptr(cam_id_ptr).to_string_lossy().into_owned();

        // 2. Open the camera FIRST (reordered) — isolates the camera-open
        //    permission path (risk #1; the permission_checker stub) from the codec
        //    configure. If open blocks here, it's the camera privacy gate; if it
        //    succeeds and the encoder step blocks, that's a separate codec dependency.
        p!("opening camera id={cam_id} …");
        let dev_cbs = ACameraDeviceStateCallbacks {
            context: ptr::null_mut(), on_disconnected, on_error,
        };
        let mut device: *mut ACameraDevice = ptr::null_mut();
        let ost = ACameraManager_openCamera(mgr, cam_id_ptr, &dev_cbs, &mut device);
        if ost != ACAMERA_OK || device.is_null() {
            p!("FAIL: openCamera(id={cam_id}) status={ost} \
                — camera open BLOCKED under this runtime (permission/AppOps?)");
            cleanup_mgr(mgr, id_list);
            return;
        }
        p!("camera OPENED id={cam_id} (status={ost}) ✓  — open works under --no-art");

        // 3. VP8 encoder + input surface (color-format = Surface).
        let codec = match codec_name {
            Some(name) => {
                p!("creating encoder by name '{name}' …");
                let cn = CString::new(name).unwrap();
                AMediaCodec_createCodecByName(cn.as_ptr())
            }
            None => {
                p!("creating VP8 encoder by type …");
                let cmime = CString::new(vp8).unwrap();
                AMediaCodec_createEncoderByType(cmime.as_ptr())
            }
        };
        if codec.is_null() {
            p!("FAIL: encoder create -> null (no such VP8 encoder?)");
            ACameraDevice_close(device); cleanup_mgr(mgr, id_list);
            return;
        }
        p!("encoder created; configuring {w}x{h} …");
        let fmt = AMediaFormat_new();
        fmt_set_str(fmt, "mime", vp8);
        fmt_set_i32(fmt, "width", w);
        fmt_set_i32(fmt, "height", h);
        fmt_set_i32(fmt, "color-format", COLOR_FORMAT_SURFACE);
        fmt_set_i32(fmt, "bitrate", 1_000_000);
        fmt_set_i32(fmt, "frame-rate", 30);
        fmt_set_i32(fmt, "i-frame-interval", 1);
        let cfg = AMediaCodec_configure(codec, fmt, ptr::null_mut(), ptr::null_mut(), CONFIGURE_FLAG_ENCODE);
        if cfg != AMEDIA_OK {
            p!("FAIL: AMediaCodec_configure status={cfg}");
            AMediaFormat_delete(fmt); AMediaCodec_delete(codec); ACameraDevice_close(device); cleanup_mgr(mgr, id_list);
            return;
        }
        p!("configured; creating input surface …");
        let mut win: *mut ANativeWindow = ptr::null_mut();
        let isc = AMediaCodec_createInputSurface(codec, &mut win);
        if isc != AMEDIA_OK || win.is_null() {
            p!("FAIL: createInputSurface status={isc}");
            AMediaFormat_delete(fmt); AMediaCodec_delete(codec); ACameraDevice_close(device); cleanup_mgr(mgr, id_list);
            return;
        }
        // The MediaCodec input surface's BufferQueue defaults to 0x0, so the camera
        // configures a 0x0 stream and the HAL tears it down ("width 0 height 0" →
        // DEL_STREAM). Give it concrete geometry (keep the codec's negotiated format)
        // before the camera targets it, so the capture stream is sized correctly.
        let fmt_px = ANativeWindow_getFormat(win);
        let sg = ANativeWindow_setBuffersGeometry(win, w, h, fmt_px);
        p!("input surface geometry set {w}x{h} fmt={fmt_px} -> {sg}; starting encoder …");
        if AMediaCodec_start(codec) != AMEDIA_OK {
            p!("FAIL: AMediaCodec_start");
            ANativeWindow_release(win); AMediaFormat_delete(fmt); AMediaCodec_delete(codec); ACameraDevice_close(device); cleanup_mgr(mgr, id_list);
            return;
        }
        p!("VP8 encoder configured + started; input surface ready");

        // 4. Capture session → the encoder's input surface.
        let mut out: *mut ACaptureSessionOutput = ptr::null_mut();
        let mut container: *mut ACaptureSessionOutputContainer = ptr::null_mut();
        ACaptureSessionOutput_create(win, &mut out);
        ACaptureSessionOutputContainer_create(&mut container);
        ACaptureSessionOutputContainer_add(container, out);
        let sess_cbs = ACameraCaptureSessionStateCallbacks {
            context: ptr::null_mut(),
            on_closed: on_session_noop, on_ready: on_session_noop, on_active: on_session_noop,
        };
        let mut session: *mut ACameraCaptureSession = ptr::null_mut();
        let sst = ACameraDevice_createCaptureSession(device, container, &sess_cbs, &mut session);
        if sst != ACAMERA_OK || session.is_null() {
            p!("FAIL: createCaptureSession status={sst}");
            ACameraDevice_close(device); ANativeWindow_release(win);
            AMediaCodec_stop(codec); AMediaCodec_delete(codec); AMediaFormat_delete(fmt);
            cleanup_mgr(mgr, id_list);
            return;
        }

        // 5. Repeating RECORD request targeting the same surface.
        let mut req: *mut ACaptureRequest = ptr::null_mut();
        ACameraDevice_createCaptureRequest(device, TEMPLATE_PREVIEW, &mut req);
        let mut target: *mut ACameraOutputTarget = ptr::null_mut();
        ACameraOutputTarget_create(win, &mut target);
        ACaptureRequest_addTarget(req, target);
        let mut seq: c_int = 0;
        let rst = ACameraCaptureSession_setRepeatingRequest(session, ptr::null(), 1, &mut req, &mut seq);
        if rst != ACAMERA_OK {
            p!("WARN: setRepeatingRequest status={rst} (continuing to drain)");
        }
        p!("repeating capture set; draining encoder output ~5s …");

        // 6. Drain the encoder ~5 s.
        let start = Instant::now();
        let mut frames = 0u64;
        let mut bytes = 0u64;
        let mut keyframes = 0u64;
        let mut first_ms: i128 = -1;
        let mut fmt_changed = false;
        let mut info = AMediaCodecBufferInfo { offset: 0, size: 0, presentation_time_us: 0, flags: 0 };
        while start.elapsed().as_secs() < 5 {
            let idx = AMediaCodec_dequeueOutputBuffer(codec, &mut info, 100_000);
            if idx >= 0 {
                if first_ms < 0 { first_ms = start.elapsed().as_millis() as i128; }
                frames += 1;
                bytes += info.size.max(0) as u64;
                if info.flags & BUFFER_FLAG_KEY_FRAME != 0 { keyframes += 1; }
                AMediaCodec_releaseOutputBuffer(codec, idx as usize, false);
            } else if idx == INFO_OUTPUT_FORMAT_CHANGED {
                fmt_changed = true;
            }
            // -1 (try-again) / -3 (buffers-changed): keep polling.
        }
        let secs = start.elapsed().as_secs_f64();

        p!("──────── RESULT ────────");
        p!("camera-open      : OK (id={cam_id}, {n} cameras)");
        p!("encoded frames   : {frames} in {secs:.1}s = {:.1} fps", frames as f64 / secs);
        p!("avg frame bytes  : {}", if frames > 0 { bytes / frames } else { 0 });
        p!("keyframes        : {keyframes}");
        p!("first-frame      : {first_ms} ms");
        p!("output-format-set: {fmt_changed}");
        if frames == 0 {
            p!("VERDICT: camera opened but NO VP8 frames — camera not delivering to the \
                surface or encoder stalled (check camera onError below).");
        } else {
            p!("VERDICT: camera → HW VP8 encode WORKS under --no-art ✓");
        }
        if CAM_ERROR.load(Relaxed) {
            p!("camera onError fired: code={}", CAM_ERR_CODE.load(Relaxed));
        }

        // 7. Best-effort teardown.
        let _ = AMediaCodec_signalEndOfInputStream(codec);
        ACameraCaptureSession_stopRepeating(session);
        ACameraCaptureSession_close(session);
        ACaptureRequest_free(req);
        ACameraOutputTarget_free(target);
        ACaptureSessionOutputContainer_free(container);
        ACaptureSessionOutput_free(out);
        ACameraDevice_close(device);
        AMediaCodec_stop(codec);
        AMediaCodec_delete(codec);
        AMediaFormat_delete(fmt);
        ANativeWindow_release(win);
        cleanup_mgr(mgr, id_list);
    }

    unsafe fn cleanup_mgr(mgr: *mut ACameraManager, id_list: *mut ACameraIdList) {
        ACameraManager_deleteCameraIdList(id_list);
        ACameraManager_delete(mgr);
    }
}
