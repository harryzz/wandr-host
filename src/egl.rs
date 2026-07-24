#[cfg(target_os = "android")]
pub mod android {
    use anyhow::{bail, Result};
    use std::ffi::c_void;

    type EGLDisplay  = *mut c_void;
    type EGLSurface  = *mut c_void;
    type EGLContext  = *mut c_void;
    type EGLConfig   = *mut c_void;
    type EGLint      = i32;
    type EGLBoolean  = u32;
    type EGLNativeWindowType = *mut c_void;

    const EGL_NONE:             EGLint = 0x3038;
    const EGL_SURFACE_TYPE:     EGLint = 0x3033;
    const EGL_WINDOW_BIT:       EGLint = 0x0004;
    const EGL_RENDERABLE_TYPE:  EGLint = 0x3040;
    const EGL_OPENGL_ES3_BIT:   EGLint = 0x0040;
    const EGL_ALPHA_SIZE:       EGLint = 0x3021;
    const EGL_BLUE_SIZE:        EGLint = 0x3022;
    const EGL_GREEN_SIZE:       EGLint = 0x3023;
    const EGL_RED_SIZE:         EGLint = 0x3024;
    const EGL_DEPTH_SIZE:       EGLint = 0x3025;
    const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
    const EGL_DEFAULT_DISPLAY:  EGLNativeWindowType = std::ptr::null_mut();
    const EGL_NO_DISPLAY:       EGLDisplay = std::ptr::null_mut();
    const EGL_NO_CONTEXT:       EGLContext = std::ptr::null_mut();
    const EGL_NO_SURFACE:       EGLSurface = std::ptr::null_mut();
    const EGL_TRUE:             EGLBoolean = 1;

    #[link(name = "EGL")]
    extern "C" {
        fn eglGetDisplay(display_id: EGLNativeWindowType) -> EGLDisplay;
        fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
        fn eglChooseConfig(dpy: EGLDisplay, attribs: *const EGLint,
                           configs: *mut EGLConfig, config_size: EGLint,
                           num_config: *mut EGLint) -> EGLBoolean;
        fn eglCreateWindowSurface(dpy: EGLDisplay, config: EGLConfig,
                                   win: EGLNativeWindowType,
                                   attribs: *const EGLint) -> EGLSurface;
        fn eglCreateContext(dpy: EGLDisplay, config: EGLConfig,
                            share: EGLContext,
                            attribs: *const EGLint) -> EGLContext;
        fn eglMakeCurrent(dpy: EGLDisplay, draw: EGLSurface,
                          read: EGLSurface, ctx: EGLContext) -> EGLBoolean;
        fn eglSwapBuffers(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
        fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
        fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
        fn eglTerminate(dpy: EGLDisplay) -> EGLBoolean;
        fn eglGetProcAddress(name: *const u8) -> *const c_void;
    }

    pub struct EglContext {
        pub display: EGLDisplay,
        pub surface: EGLSurface,
        pub context: EGLContext,
        pub width:   i32,
        pub height:  i32,
    }

    impl EglContext {
        pub fn new(native_window: *mut c_void) -> Result<Self> {
            unsafe {
                let display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
                if display == EGL_NO_DISPLAY { bail!("eglGetDisplay failed"); }

                let mut major = 0i32;
                let mut minor = 0i32;
                if eglInitialize(display, &mut major, &mut minor) != EGL_TRUE {
                    bail!("eglInitialize failed");
                }
                log::info!("EGL {major}.{minor}");

                let attribs = [
                    EGL_SURFACE_TYPE,    EGL_WINDOW_BIT,
                    EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT,
                    EGL_RED_SIZE,   8,
                    EGL_GREEN_SIZE, 8,
                    EGL_BLUE_SIZE,  8,
                    // Request an alpha channel so a guest can render TRANSLUCENT
                    // pixels (Skia clear to 0x00000000). Without this, eglChooseConfig
                    // picks a no-alpha (RGBX) config and every transparent clear is
                    // stored opaque — which makes the behind-ui video hole-punch
                    // (decode-to-surface below a transparent app layer, task 117 M2
                    // Android) composite as opaque black over the video. The app SF
                    // layer is created eLayerOpaque by default, so an opaque-clearing
                    // guest is unaffected — SF ignores the alpha until a guest opts in
                    // via sf_set_opaque(false) (done when a behind-ui decoder opens).
                    EGL_ALPHA_SIZE, 8,
                    EGL_DEPTH_SIZE, 0,
                    EGL_NONE,
                ];
                let mut config: EGLConfig = std::ptr::null_mut();
                let mut num_config = 0i32;
                if eglChooseConfig(display, attribs.as_ptr(),
                                   &mut config, 1, &mut num_config) != EGL_TRUE
                    || num_config == 0
                {
                    bail!("eglChooseConfig failed");
                }

                let surface = eglCreateWindowSurface(
                    display, config, native_window, std::ptr::null());
                if surface == EGL_NO_SURFACE { bail!("eglCreateWindowSurface failed"); }

                let ctx_attribs = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
                let context = eglCreateContext(
                    display, config, EGL_NO_CONTEXT, ctx_attribs.as_ptr());
                if context == EGL_NO_CONTEXT { bail!("eglCreateContext failed"); }

                if eglMakeCurrent(display, surface, surface, context) != EGL_TRUE {
                    bail!("eglMakeCurrent failed");
                }
                log::info!("EGL context made current");

                let mut w = 0i32; let mut h = 0i32;
                extern "C" {
                    fn eglQuerySurface(dpy: EGLDisplay, surface: EGLSurface,
                                       attr: EGLint, val: *mut EGLint) -> EGLBoolean;
                }
                eglQuerySurface(display, surface, 0x3056 /* EGL_WIDTH  */, &mut w);
                eglQuerySurface(display, surface, 0x3057 /* EGL_HEIGHT */, &mut h);

                // The authoritative GL buffer geometry is the ANativeWindow's
                // — it is what GL actually renders into. eglQuerySurface
                // EGL_WIDTH/HEIGHT can disagree: on the taimen Adreno driver
                // it reports the *transposed* pre-rotation size (e.g.
                // 2880x1440 for a 1440x2880 buffer), which would make us build
                // a mismatched Skia surface and render rotated/clipped. Prefer
                // the ANativeWindow dims whenever they are valid.
                #[link(name = "android")]
                extern "C" {
                    fn ANativeWindow_getWidth(w: *mut c_void) -> i32;
                    fn ANativeWindow_getHeight(w: *mut c_void) -> i32;
                }
                let nw_w = ANativeWindow_getWidth(native_window);
                let nw_h = ANativeWindow_getHeight(native_window);
                log::info!(
                    "EGL surface dims — eglQuerySurface {w}x{h}, \
                     ANativeWindow {nw_w}x{nw_h}"
                );
                let (width, height) = if nw_w > 0 && nw_h > 0 {
                    (nw_w, nw_h)
                } else {
                    (w, h)
                };

                Ok(EglContext { display, surface, context, width, height })
            }
        }

        pub fn make_current(&self) {
            unsafe { eglMakeCurrent(self.display, self.surface, self.surface, self.context); }
        }

        pub fn swap(&self) {
            static LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::info!("eglSwapBuffers first call");
            }
            unsafe { eglSwapBuffers(self.display, self.surface); }
        }

        pub fn proc_resolver() -> impl Fn(&str) -> *const c_void {
            |name: &str| {
                let c = std::ffi::CString::new(name).unwrap();
                unsafe { eglGetProcAddress(c.as_ptr() as *const u8) }
            }
        }
    }

    impl Drop for EglContext {
        fn drop(&mut self) {
            // NOTE: do NOT call eglTerminate() — the display is a
            // process-singleton (EGL_DEFAULT_DISPLAY) and terminating it
            // would invalidate any other EglContext we're about to create
            // (warm-resume code path swaps the renderer in-place, which
            // means a new context briefly co-exists with the old one being
            // dropped). eglDestroyContext alone is sufficient to release
            // this context's resources.
            unsafe {
                eglMakeCurrent(
                    self.display,
                    EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
                eglDestroySurface(self.display, self.surface);
                eglDestroyContext(self.display, self.context);
            }
        }
    }
}
