// sf_probe — task 33 Step 1 keystone de-risk (M5: isolate first).
//
// A standalone, pure-C++ binary — NOT a NativeActivity, NOT linked into
// wandr-host. It proves the single biggest unproven assumption of the
// post-ART display path: can a non-Activity process get a display surface
// from SurfaceFlinger and render to it?
//
// Sequence: ProcessState threadpool -> SurfaceComposerClient -> createSurface
// -> Transaction(setLayer top, show) -> SurfaceControl::getSurface()
// -> ANativeWindow* -> EGL -> glClear -> eglSwapBuffers.
//
// Run on the rooted Pixel 2 XL ("taimen", Android 15) via:
//   adb shell su -c '/data/local/tmp/sf_probe'
// Expect: a solid blue frame on the physical panel for ~10 s.
//
// If this fails, the post-ART display showstopper is found at the cheapest
// possible point. See tasks/33-boot-model-bringup.md.

#include <gui/SurfaceComposerClient.h>
#include <gui/SurfaceControl.h>
#include <gui/Surface.h>
#include <binder/ProcessState.h>
#include <ui/PixelFormat.h>
#include <ui/DisplayId.h>
#include <utils/String8.h>

#include <android/native_window.h>
#include <EGL/egl.h>
#include <GLES2/gl2.h>

#include <cstdio>
#include <unistd.h>
#include <vector>

using namespace android;

#define CHECK(cond, msg) \
    do { if (!(cond)) { printf("sf_probe FAIL: %s\n", msg); return 1; } } while (0)

int main() {
    printf("sf_probe: start\n");

    // Binder threadpool — SurfaceComposerClient talks to SF over binder.
    ProcessState::self()->startThreadPool();

    sp<SurfaceComposerClient> client = new SurfaceComposerClient();
    status_t err = client->initCheck();
    printf("sf_probe: SurfaceComposerClient initCheck = %d\n", err);
    CHECK(err == NO_ERROR, "SurfaceComposerClient initCheck");

    std::vector<PhysicalDisplayId> ids =
        SurfaceComposerClient::getPhysicalDisplayIds();
    printf("sf_probe: %zu physical display(s)\n", ids.size());
    CHECK(!ids.empty(), "no physical displays");

    // M3 — avoid the drift-prone display-info structs (ui::DisplayMode,
    // gui::StaticDisplayInfo). The taimen panel is 1440x2880; hardcode it
    // for the spike. Step 2 reads real geometry from rsbinder / dumpsys.
    const uint32_t W = 1440, H = 2880;

    sp<SurfaceControl> sc = client->createSurface(
        String8("wandr-probe"), W, H, PIXEL_FORMAT_RGBA_8888, 0);
    CHECK(sc != nullptr && sc->isValid(), "createSurface");
    printf("sf_probe: createSurface ok\n");

    SurfaceComposerClient::Transaction()
        .setLayer(sc, 0x7fffffff)
        .show(sc)
        .apply();
    printf("sf_probe: transaction applied (layer shown, top z-order)\n");

    sp<Surface> surface = sc->getSurface();
    CHECK(surface != nullptr, "getSurface");
    ANativeWindow* win = surface.get();

    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(dpy != EGL_NO_DISPLAY, "eglGetDisplay");
    CHECK(eglInitialize(dpy, nullptr, nullptr), "eglInitialize");

    const EGLint cfgAttrs[] = {
        EGL_SURFACE_TYPE,    EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig cfg;
    EGLint numCfg = 0;
    CHECK(eglChooseConfig(dpy, cfgAttrs, &cfg, 1, &numCfg) && numCfg > 0,
          "eglChooseConfig");

    EGLSurface esurf = eglCreateWindowSurface(dpy, cfg, win, nullptr);
    CHECK(esurf != EGL_NO_SURFACE, "eglCreateWindowSurface");

    const EGLint ctxAttrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxAttrs);
    CHECK(ctx != EGL_NO_CONTEXT, "eglCreateContext");
    CHECK(eglMakeCurrent(dpy, esurf, esurf, ctx), "eglMakeCurrent");

    glClearColor(0.0f, 0.4f, 0.8f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    CHECK(eglSwapBuffers(dpy, esurf), "eglSwapBuffers");
    printf("sf_probe: frame swapped — holding 10s\n");

    sleep(10);

    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroySurface(dpy, esurf);
    eglDestroyContext(dpy, ctx);
    printf("sf_probe: done\n");
    return 0;
}
