// sf_surface — task 33 Step 1/2: the reusable libgui surface shim.
//
// sf_probe.cpp's proven SurfaceFlinger path, as an `extern "C"` shared
// library (libsf_surface.so). The wandr-host standalone runtime dlopen()s it
// and calls sf_create_fullscreen_surface() to obtain a fullscreen
// ANativeWindow* with no NativeActivity, then drives EGL/Skia on it.
//
// Built IN-TREE as a soong cc_library_shared (see sf_surface.bp) — libgui's
// headers cannot be consumed out-of-tree. See memory
// project-boot-model-libgui-build and tasks/33-boot-model-bringup.md.

#include <gui/SurfaceComposerClient.h>
#include <gui/SurfaceControl.h>
#include <gui/Surface.h>
#include <gui/BLASTBufferQueue.h>
#include <gui/LayerState.h>
#include <gui/WindowInfo.h>
#include <binder/ProcessState.h>
#include <binder/IServiceManager.h>
#include <binder/Binder.h>
#include <binder/Parcel.h>
#include <android/os/IInputFlinger.h>
#include <android/gui/FocusRequest.h>
#include <input/Input.h>
#include <input/InputConsumer.h>
#include <input/InputTransport.h>
// Task 80 — standalone InputReader for ART-less input. Headers from
// frameworks/native/services/inputflinger/{include,reader/include} (Android.bp
// include_dirs). EventHub reads /dev/input/* directly; no system_server.
#include "InputReaderBase.h"
#include "InputReaderFactory.h"
#include "InputListener.h"
#include <input/DisplayViewport.h>
#include <deque>
#include <mutex>
#include <cstdlib>
#include <unistd.h>
#include <ui/PixelFormat.h>
#include <ui/DisplayId.h>
#include <ui/DisplayMode.h>
#include <ui/LogicalDisplayId.h>
#include <ui/Rect.h>
#include <ui/Region.h>
#include <ui/Rotation.h>
#include <ui/Size.h>
#include <utils/String8.h>
#include <utils/Timers.h>

#include <android/native_window.h>
#include <android/input.h>
#include <android/log.h>
#include <system/window.h>

#include <algorithm>
#include <cstdint>
#include <cstdlib>
#include <memory>
#include <vector>
#include <poll.h>

using namespace android;

// POD input event handed back across the C ABI by sf_input_poll(). Mirrored
// in sf_surface.h and in the Rust side (src/sf_surface.rs) — keep in sync.
struct SfInputEvent {
    int32_t kind;        // 0=down 1=up 2=move 3=scroll  10=key-down 11=key-up
    int32_t pointer_id;  // multi-touch pointer id (0..N); 0 for key events
    float   x;
    float   y;
    float   pressure;    // 0.0..1.0; 0 for key events
    int32_t key_code;    // AKEYCODE_* for key events; 0 otherwise
    int32_t meta_state;  // AMETA_* shift/alt/ctrl bitmask for key events; 0 otherwise
};

namespace {
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO,  "sf_surface", __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, "sf_surface", __VA_ARGS__)

// Panel dimensions in PORTRAIT orientation (width <= height). Populated
// by `init_panel_dims()` from `SurfaceComposerClient::getActiveDisplayMode`
// after the display token is resolved. Defaults to taimen (Pixel 2 XL)
// so the legacy hardcoded behavior still works if the query somehow
// fails — but every fresh process path passes through init_panel_dims
// and overrides these. Task 48.
uint32_t PANEL_W = 1440;
uint32_t PANEL_H = 2880;

// Query the active display mode and update PANEL_W / PANEL_H. The
// resolution is normalized to portrait (`min(w,h)` → width,
// `max(w,h)` → height) so the rest of the shim's portrait-coord
// assumptions hold on any display the runtime targets. Falls back to
// the taimen defaults on a failed query with a warning logged.
void init_panel_dims(const sp<IBinder>& display) {
    ui::DisplayMode mode;
    status_t st = SurfaceComposerClient::getActiveDisplayMode(display, &mode);
    if (st != OK) {
        LOGE("init_panel_dims: getActiveDisplayMode failed (%d) — "
             "keeping defaults %ux%u", st, PANEL_W, PANEL_H);
        return;
    }
    int32_t w = mode.resolution.width;
    int32_t h = mode.resolution.height;
    if (w <= 0 || h <= 0 || w > 8192 || h > 8192) {
        LOGE("init_panel_dims: implausible resolution %dx%d — "
             "keeping defaults %ux%u", w, h, PANEL_W, PANEL_H);
        return;
    }
    // Normalize to portrait. The shim's setDisplayProjection(ROTATION_0,
    // Rect(PW, PH), ...) treats PW as the short side; landscape-native
    // displays (taimen, some tablets) report (long, short) here.
    uint32_t pw = static_cast<uint32_t>(std::min(w, h));
    uint32_t ph = static_cast<uint32_t>(std::max(w, h));
    PANEL_W = pw;
    PANEL_H = ph;
    LOGI("init_panel_dims: panel resolution %dx%d → portrait %ux%u",
         w, h, PANEL_W, PANEL_H);
}

// Keep these alive for the process lifetime — dropping any of them
// invalidates the ANativeWindow* handed back to the caller.
sp<SurfaceComposerClient> g_client;
sp<SurfaceControl>        g_control;
sp<BLASTBufferQueue>      g_bbq;
sp<Surface>               g_surface;
sp<IBinder>               g_display;

// Task 47 step 3c — overlay parent container. Mirrors AOSP's
// BlastInputSurface pattern (frameworks/native/libs/gui/tests/
// EndToEndNativeInputTest.cpp): the buffer-state surface (g_control)
// is parented to a container-only SurfaceControl, and positioning /
// layering / cropping are applied to the PARENT. Setting position
// directly on the BBQ-backed buffer-state child does not stick;
// the parent inherits geometry, the child draws the buffer.
sp<SurfaceControl>        g_overlay_parent;

// Input plumbing (task 33 Step 3) — an InputFlinger input channel registered
// for the wandr layer so touches in our window are dispatched to us.
std::shared_ptr<InputChannel>  g_input_channel;
std::unique_ptr<InputConsumer> g_input_consumer;
sp<gui::WindowInfoHandle>      g_window_info;

// Task 47 step 3c — overlay surfaces are positioned at (0, PANEL_H - H)
// on the panel, so InputDispatcher delivers display-coord events in
// the range [Y, PANEL_H] in Y. The guest expects surface-local coords
// (range [0, H]), so sf_input_poll subtracts this offset from motion
// events. 0 = fullscreen surface (no subtraction).
int32_t g_overlay_y_offset = 0;

// Task 80 Step 2 — per-host input-region routing. With our own InputReader, every
// host receives every touch; each host drops touches outside the visible region of
// its surface so a tap on a chrome strip doesn't leak to the app underneath (and
// vice-versa). The region is in GLOBAL display coords and is checked BEFORE the
// g_overlay_y_offset local-mapping. Overlays self-set it to their strip at create;
// the fullscreen app sets it to its content rect (panel minus chrome insets) via
// sf_set_input_rect when the arbiter pushes geometry. Inactive = accept all (the
// pre-routing behavior + the inputflinger path are unaffected).
bool    g_input_filter_active = false;
int32_t g_input_fx = 0, g_input_fy = 0, g_input_fw = 0, g_input_fh = 0;

static bool input_accepts(float gx, float gy) {
    if (!g_input_filter_active) return true;
    return gx >= static_cast<float>(g_input_fx) && gx < static_cast<float>(g_input_fx + g_input_fw) &&
           gy >= static_cast<float>(g_input_fy) && gy < static_cast<float>(g_input_fy + g_input_fh);
}

// ── Task 80: ART-less input via a standalone Android InputReader ──────────────
// When WANDR_EVDEV_INPUT is set, source input from a private InputReader (its
// EventHub reads /dev/input/* directly) instead of the system_server InputFlinger
// channel — so input survives with the Java framework stopped. sf_input_poll
// drains the queue our listener fills; the SfInputEvent contract to the host is
// unchanged. Validated by the task-80 spike (createInputReader runs standalone;
// MT touch decodes 1:1). Routing (which surface) stays the arbiter's job; this
// first cut feeds the foreground host its events.
bool                                  g_evdev_mode = false;
std::unique_ptr<InputReaderInterface> g_input_reader;
std::deque<SfInputEvent>              g_evdev_queue;
std::mutex                            g_evdev_mutex;

class WandrInputListener : public InputListenerInterface {
public:
    void notifyInputDevicesChanged(const NotifyInputDevicesChangedArgs&) override {}
    void notifyKey(const NotifyKeyArgs& a) override {
        SfInputEvent e{};
        if (a.action == AKEY_EVENT_ACTION_DOWN) e.kind = 10;
        else if (a.action == AKEY_EVENT_ACTION_UP) e.kind = 11;
        else return;
        e.key_code = a.keyCode;
        e.meta_state = a.metaState;
        std::lock_guard<std::mutex> lk(g_evdev_mutex);
        g_evdev_queue.push_back(e);
    }
    void notifyMotion(const NotifyMotionArgs& a) override {
        const int32_t masked = a.action & AMOTION_EVENT_ACTION_MASK;
        const size_t count = a.pointerCoords.size();
        auto emit = [&](size_t idx, int32_t kind) {
            if (idx >= count) return;
            const float gx = a.pointerCoords[idx].getX();
            const float gy = a.pointerCoords[idx].getY();
            // Step 2 routing — drop touches outside this surface's visible region
            // (global coords), so chrome/app input don't leak into each other.
            if (!input_accepts(gx, gy)) return;
            SfInputEvent e{};
            e.kind = kind;
            e.pointer_id = a.pointerProperties[idx].id;
            e.x = gx;
            // InputReader gives display-global coords; guest wants surface-local.
            e.y = gy - static_cast<float>(g_overlay_y_offset);
            e.pressure = a.pointerCoords[idx].getAxisValue(AMOTION_EVENT_AXIS_PRESSURE);
            std::lock_guard<std::mutex> lk(g_evdev_mutex);
            g_evdev_queue.push_back(e);
        };
        const int32_t pidx = (a.action & AMOTION_EVENT_ACTION_POINTER_INDEX_MASK)
                             >> AMOTION_EVENT_ACTION_POINTER_INDEX_SHIFT;
        switch (masked) {
            case AMOTION_EVENT_ACTION_DOWN:
            case AMOTION_EVENT_ACTION_POINTER_DOWN:
                emit(pidx, 0);
                break;
            case AMOTION_EVENT_ACTION_UP:
            case AMOTION_EVENT_ACTION_POINTER_UP:
            case AMOTION_EVENT_ACTION_CANCEL:
                emit(pidx, 1);
                break;
            case AMOTION_EVENT_ACTION_MOVE:
                for (size_t i = 0; i < count; i++) emit(i, 2);
                break;
            default:
                break;
        }
    }
    void notifySwitch(const NotifySwitchArgs&) override {}
    void notifySensor(const NotifySensorArgs&) override {}
    void notifyVibratorState(const NotifyVibratorStateArgs&) override {}
    void notifyDeviceReset(const NotifyDeviceResetArgs&) override {}
    void notifyPointerCaptureChanged(const NotifyPointerCaptureChangedArgs&) override {}
};

class WandrInputPolicy : public InputReaderPolicyInterface {
public:
    void getReaderConfiguration(InputReaderConfiguration* outConfig) override {
        // Associate the touchscreen with the internal display so InputReader
        // configures it + maps coords. 1:1 viewport, ROT_0 (the host applies its
        // own content rotation; the standalone loop inverse-maps input to match).
        const int32_t pw = static_cast<int32_t>(PANEL_W);
        const int32_t ph = static_cast<int32_t>(PANEL_H);
        DisplayViewport v;
        v.displayId = ui::LogicalDisplayId::DEFAULT;
        v.orientation = ui::ROTATION_0;
        v.logicalRight = pw;  v.logicalBottom = ph;
        v.physicalRight = pw; v.physicalBottom = ph;
        v.deviceWidth = pw;   v.deviceHeight = ph;
        v.isActive = true;
        v.uniqueId = "local:0";
        v.type = ViewportType::INTERNAL;
        outConfig->setDisplayViewports({v});
    }
    void notifyInputDevicesChanged(const std::vector<InputDeviceInfo>&) override {}
    void notifyTouchpadHardwareState(const SelfContainedHardwareState&, int32_t) override {}
    void notifyTouchpadGestureInfo(GestureType, int32_t) override {}
    void notifyTouchpadThreeFingerTap() override {}
    std::shared_ptr<KeyCharacterMap> getKeyboardLayoutOverlay(
            const InputDeviceIdentifier&, const std::optional<KeyboardLayoutInfo>) override {
        return nullptr;
    }
    std::string getDeviceAlias(const InputDeviceIdentifier&) override { return ""; }
    TouchAffineTransformation getTouchAffineTransformation(
            const std::string&, ui::Rotation) override {
        return TouchAffineTransformation();
    }
    void notifyStylusGestureStarted(int32_t, nsecs_t) override {}
    bool isInputMethodConnectionActive() override { return false; }
    std::optional<DisplayViewport> getPointerViewportForAssociatedDisplay(
            ui::LogicalDisplayId) override {
        return std::nullopt;
    }
};

sp<WandrInputPolicy> g_evdev_policy;
WandrInputListener   g_evdev_listener;

// Start the private InputReader once. Idempotent.
void start_evdev_input() {
    if (g_input_reader != nullptr) {
        return;
    }
    g_evdev_policy = sp<WandrInputPolicy>::make();
    g_input_reader = createInputReader(g_evdev_policy, g_evdev_listener);
    if (g_input_reader == nullptr) {
        LOGE("evdev: createInputReader returned null — input disabled");
        return;
    }
    if (g_input_reader->start() != OK) {
        LOGE("evdev: InputReader start failed — input disabled");
        g_input_reader.reset();
        return;
    }
    LOGI("evdev: standalone InputReader started (ART-less input, panel %ux%u)",
         PANEL_W, PANEL_H);
}

// Task 84 — register this host's input-channel token with wandr-inputflinger's
// "wandr.windowreg" service (transaction code must match WandrWindowReg::TX_REGISTER
// = FIRST_CALL_TRANSACTION). The token only exists in our process + wandr-
// inputflinger (which minted it at createInputChannel); the arbiter authors the
// window list by pid and wandr-inputflinger joins pid→token here. Fire-and-forget
// (oneway). No-op under normal ART — checkService returns null when the standalone
// inputflinger isn't the one serving input.
void register_window_token_artless() {
    if (g_input_channel == nullptr) {
        return;
    }
    sp<IBinder> reg =
        defaultServiceManager()->checkService(String16("wandr.windowreg"));
    if (reg == nullptr) {
        return; // normal ART — system InputDispatcher owns windows via SF
    }
    static constexpr uint32_t TX_REGISTER = IBinder::FIRST_CALL_TRANSACTION;
    Parcel data, reply;
    data.writeInt32(static_cast<int32_t>(getpid()));
    data.writeStrongBinder(g_input_channel->getConnectionToken());
    status_t st = reg->transact(TX_REGISTER, data, &reply, IBinder::FLAG_ONEWAY);
    LOGI("registered window token with wandr.windowreg (pid=%d) → %d",
         getpid(), static_cast<int>(st));
}

// Register an InputFlinger input window for g_control so InputDispatcher
// routes touch events inside `rect` (in display coords) to our input
// channel. Recipe from
// frameworks/native/libs/gui/tests/EndToEndNativeInputTest.cpp. Non-fatal:
// on any failure input is simply disabled (g_input_consumer stays null).
//
// `rect` is in DISPLAY coordinates. The fullscreen path passes
// Rect(0,0,PW,PH); the overlay path (task 47 step 3c) passes
// Rect(0,Y,PW,PH) for a bottom strip — that rect determines which
// taps InputDispatcher routes to us; events arrive in display coords,
// and sf_input_poll subtracts g_overlay_y_offset to give the guest
// surface-local coords.
void register_input_window_at(const Rect& rect, const char* name) {
    // Task 80 — ART-less mode: skip the system_server InputFlinger channel and
    // run our own InputReader instead (idempotent across the fullscreen + overlay
    // call sites). Routing is the arbiter's job; this host consumes its events.
    if (getenv("WANDR_EVDEV_INPUT") != nullptr) {
        g_evdev_mode = true;
        start_evdev_input();
        return;
    }
    sp<IBinder> binder =
        defaultServiceManager()->waitForService(String16("inputflinger"));
    sp<os::IInputFlinger> inputFlinger = interface_cast<os::IInputFlinger>(binder);
    if (inputFlinger == nullptr) {
        LOGE("inputflinger service unavailable — input disabled");
        return;
    }

    os::InputChannelCore channelCore;
    binder::Status st =
        inputFlinger->createInputChannel("wandr input", &channelCore);
    if (!st.isOk()) {
        LOGE("createInputChannel failed — input disabled");
        return;
    }
    g_input_channel = std::shared_ptr<InputChannel>(
        InputChannel::create(std::move(channelCore)));
    g_input_consumer = std::make_unique<InputConsumer>(g_input_channel);

    g_window_info = sp<gui::WindowInfoHandle>::make();
    gui::WindowInfo* wi = g_window_info->editInfo();
    wi->token             = g_input_channel->getConnectionToken();
    wi->name              = name;
    wi->globalScaleFactor = 1.0f;
    wi->frame             = rect;
    wi->touchableRegion.orSelf(rect);
    wi->displayId         = ui::LogicalDisplayId::DEFAULT;
    wi->applicationInfo.token = sp<BBinder>::make();
    wi->applicationInfo.name  = name;
    wi->applicationInfo.dispatchingTimeoutMillis = 5000;

    SurfaceComposerClient::Transaction t;
    t.setInputWindowInfo(g_control, g_window_info);
    gui::FocusRequest fr;
    fr.token      = wi->token;
    fr.windowName = wi->name;
    fr.timestamp  = systemTime(SYSTEM_TIME_MONOTONIC);
    fr.displayId  = ui::LogicalDisplayId::DEFAULT.val();
    t.setFocusedWindow(fr);
    t.apply(/*synchronous=*/true);
    LOGI("input window '%s' registered at (%d,%d)-(%d,%d) (channel fd %d)",
         name, rect.left, rect.top, rect.right, rect.bottom,
         g_input_channel->getFd());

    // Task 84 — under ART-off, SurfaceFlinger can't deliver the WindowInfo above
    // to the standalone wandr-inputflinger dispatcher (it never re-bound
    // mInputFlinger after system_server died). Instead the wandr-arbiter authors
    // the window list by pid and wandr-inputflinger feeds the dispatcher — but it
    // needs our input-channel token keyed by pid, which only WE hold. Register it
    // directly with wandr-inputflinger's "wandr.windowreg" binder service (the
    // token's kernel identity round-trips intact). No-op under normal ART: the
    // service isn't published, so checkService returns null and we skip.
    register_window_token_artless();
}

// Back-compat wrapper for the fullscreen path. Same behavior as the
// original `register_input_window(PW, PH)` — registers a Rect(0,0,PW,PH)
// input window named "wandr". Kept so sf_create_fullscreen_surface's
// body is unchanged.
void register_input_window(uint32_t PW, uint32_t PH) {
    register_input_window_at(
        Rect(0, 0, static_cast<int32_t>(PW), static_cast<int32_t>(PH)),
        "wandr");
}

// Task 47 step 3c — update an EXISTING input window's bounds to
// `rect` (display coords). Used by sf_resize_overlay after the
// SurfaceControl is resized + repositioned. No-op if input wasn't
// registered (e.g. inputflinger was unavailable at create time).
void update_input_window_bounds(const Rect& rect) {
    if (g_window_info == nullptr) {
        return;
    }
    gui::WindowInfo* wi = g_window_info->editInfo();
    wi->frame = rect;
    wi->touchableRegion.clear();
    wi->touchableRegion.orSelf(rect);
    SurfaceComposerClient::Transaction t;
    t.setInputWindowInfo(g_control, g_window_info);
    t.apply(/*synchronous=*/true);
    LOGI("input window bounds updated to (%d,%d)-(%d,%d)",
         rect.left, rect.top, rect.right, rect.bottom);
}
}  // namespace

extern "C" {

// Start the process-wide C++ libbinder threadpool. Needed by any in-process C++
// binder CLIENT that receives callbacks — e.g. the NDK Camera2 client
// (libcamera2ndk uses C++ libbinder, NOT libbinder_ndk; rsbinder's threadpool is a
// separate context and does not service it). Idempotent; safe to call before any
// camera/codec use (task 93). The surface entry points below already call this,
// but a non-surface caller (the video probe) needs it standalone.
void sf_start_binder_threadpool() {
    ProcessState::self()->startThreadPool();
}

// Allocate a fullscreen, top-z-order SurfaceControl from SurfaceFlinger and
// return its ANativeWindow* (a libgui Surface; the caller drives EGL on it).
// Writes the portrait logical dimensions to out_w/out_h and the SurfaceFlinger
// display rotation (ui::Rotation, 0..3) to out_transform, all if non-null.
// Returns nullptr on failure.
ANativeWindow* sf_create_fullscreen_surface(int32_t* out_w, int32_t* out_h,
                                            uint32_t* out_transform) {
    ProcessState::self()->startThreadPool();

    g_client = new SurfaceComposerClient();
    status_t err = g_client->initCheck();
    if (err != NO_ERROR) {
        LOGE("SurfaceComposerClient initCheck failed: %d", err);
        return nullptr;
    }

    std::vector<PhysicalDisplayId> ids =
        SurfaceComposerClient::getPhysicalDisplayIds();
    if (ids.empty()) {
        LOGE("no physical displays");
        return nullptr;
    }
    g_display = SurfaceComposerClient::getPhysicalDisplayToken(ids[0]);
    if (g_display == nullptr) {
        LOGE("getPhysicalDisplayToken returned null");
        return nullptr;
    }

    // Task 48 — populate PANEL_W/PANEL_H from the active display mode.
    // Pre-task 48 this was hardcoded to taimen's 1440x2880 portrait;
    // now any display the runtime targets works.
    init_panel_dims(g_display);
    const uint32_t PW = PANEL_W, PH = PANEL_H;  // local aliases for the rest of this fn

    // Step 1 — pin the display projection to portrait identity.
    //
    // ROTATION_0 with a portrait layer stack == the panel: this rotates
    // nothing, it just resets the projection in case a prior run left it
    // skewed (setDisplayProjection state persists across process exit). It
    // must NOT carry a rotation — that is a global display change that would
    // rotate the launcher / SystemUI too.
    {
        SurfaceComposerClient::Transaction t;
        t.setDisplayProjection(g_display, ui::ROTATION_0,
                               Rect(PW, PH), Rect(PW, PH));
        t.apply(/*synchronous=*/true);
    }

    // Step 2 — create the surface, PORTRAIT PWxPH (1440x2880).
    //
    // With the transform hint pinned to ROT_0 (Step 3, setFixedTransformHint)
    // there is no auto-prerotation, so the layer, the BLASTBufferQueue and the
    // EGL buffer are all the same portrait 1440x2880 — matching the portrait
    // panel and composition space 1:1, guest renders with an identity matrix.
    g_control = g_client->createSurface(
        String8("wandr"), PW, PH, PIXEL_FORMAT_RGBA_8888, 0);
    if (g_control == nullptr || !g_control->isValid()) {
        LOGE("createSurface failed");
        return nullptr;
    }

    // Step 3 — show it, top z-order, marked opaque; transform hint handling.
    //
    // eLayerOpaque tells SurfaceFlinger the layer fully covers its bounds, so
    // it does NOT blend whatever is behind it (the launcher) through pixels
    // the guest left transparent. It must be set via the transaction's
    // setFlags (a layer_state_t flag) — the createSurface `flags` parameter
    // uses a different, unrelated enum. The host pairs this by clearing the
    // surface to opaque black each frame (SkiaRenderer::begin_frame).
    //
    // Transform hint (task 33 orientation fix). The taimen panel is
    // physically landscape-native, so SurfaceFlinger hands this layer a
    // ROT_90 transform hint and EGL PRE-ROTATES — the producer's buffer is
    // transposed from the requested size. Rather than fight that, the host
    // now reads the real hint back via sf_query_transform_hint() and renders
    // pre-rotated to match it (the Android pre-rotation model). So by default
    // we do NOT pin the hint — SurfaceFlinger's natural hint flows through to
    // the producer and is queryable. WANDR_SF_HINT=<0..7>, if set, pins the
    // layer + client-cache hint to that value for on-device iteration:
    //   - setFixedTransformHint: SurfaceFlinger composites + reports it fixed.
    //   - g_control->setTransformHint: the client-side cache the BLASTBuffer
    //     queue forwards to the EGL producer (set ONCE at createSurface and
    //     never auto-updated by setFixedTransformHint), so it must be poked
    //     before the BBQ is constructed below.
    const char* pin_env = getenv("WANDR_SF_HINT");
    int pinned_hint = -1;
    if (pin_env != nullptr && pin_env[0] != '\0') {
        pinned_hint = atoi(pin_env);
    }
    {
        SurfaceComposerClient::Transaction t;
        t.setLayer(g_control, 0x7fffffff);
        if (pinned_hint >= 0) {
            t.setFixedTransformHint(g_control, pinned_hint);
        }
        t.setFlags(g_control, layer_state_t::eLayerOpaque,
                   layer_state_t::eLayerOpaque);
        t.show(g_control);
        t.apply(/*synchronous=*/true);
    }
    if (pinned_hint >= 0) {
        g_control->setTransformHint(pinned_hint);
        LOGI("transform hint pinned to %d (WANDR_SF_HINT)", pinned_hint);
    } else {
        LOGI("transform hint NOT pinned — SurfaceFlinger natural hint in use");
    }

    // Step 3b — attach a BLASTBufferQueue DIRECTLY to g_control.
    //
    // SurfaceControl::getSurface() would instead create an internal
    // "[BBQ] wandr" CHILD SurfaceControl and put the buffer there. That child
    // is clipped to g_control's bounds — a parent/child clip we avoid by
    // owning the BBQ ourselves (the same call getSurface() makes internally,
    // minus the child). One layer, no parent/child clip — the BBQ buffer
    // composites full-screen; the host pre-rotates content per the queried
    // transform hint so the guest UI lands upright.
    g_bbq = sp<BLASTBufferQueue>::make(
        "wandr", g_control, PW, PH, PIXEL_FORMAT_RGBA_8888);
    g_surface = g_bbq->getSurface(/*includeSurfaceControlHandle=*/true);
    if (g_surface == nullptr) {
        LOGE("BLASTBufferQueue getSurface returned null");
        return nullptr;
    }

    // Step 4 — register an InputFlinger input window (task 33 Step 3), or in
    // ART-less mode start our own InputReader (task 80; the InputReader display
    // viewport reads the global PANEL_W/PANEL_H set by init_panel_dims).
    register_input_window(PW, PH);

    // Report the portrait logical size. out_transform stays 0 here — the
    // real producer transform hint is only valid post-EGL-connect and is
    // read separately via sf_query_transform_hint().
    if (out_w) *out_w = static_cast<int32_t>(PW);
    if (out_h) *out_h = static_cast<int32_t>(PH);
    if (out_transform) *out_transform = 0;
    LOGI("surface created: portrait %ux%u logical (host reads the transform "
         "hint post-connect via sf_query_transform_hint)", PW, PH);
    return g_surface.get();
}

// Drain pending input events into `out` (capacity `max`); returns the count
// written. Non-blocking — call once per frame from the render loop. Each
// consumed InputFlinger event is decoded to the action pointer and finished.
// Returns 0 if input was never set up.
int32_t sf_input_poll(SfInputEvent* out, int32_t max) {
    if (out == nullptr || max <= 0) {
        return 0;
    }
    // Task 80 — ART-less mode: drain the queue our InputReader listener fills.
    if (g_evdev_mode) {
        int32_t n = 0;
        std::lock_guard<std::mutex> lk(g_evdev_mutex);
        while (n < max && !g_evdev_queue.empty()) {
            out[n++] = g_evdev_queue.front();
            g_evdev_queue.pop_front();
        }
        return n;
    }
    if (g_input_consumer == nullptr) {
        return 0;
    }
    static PreallocatedInputEventFactory factory;
    int32_t n = 0;
    while (n < max) {
        InputEvent* ev = nullptr;
        uint32_t seq = 0;
        status_t st = g_input_consumer->consume(
            &factory, /*consumeBatches=*/true, /*frameTime=*/-1, &seq, &ev);
        if (st != OK || ev == nullptr) {
            break;  // WOULD_BLOCK — nothing more pending
        }
        bool emitted = false;
        if (ev->getType() == InputEventType::MOTION) {
            MotionEvent* m = static_cast<MotionEvent*>(ev);
            size_t idx = 0;
            switch (m->getActionMasked()) {
                case AMOTION_EVENT_ACTION_DOWN:
                case AMOTION_EVENT_ACTION_POINTER_DOWN:
                    out[n].kind = 0; idx = m->getActionIndex(); emitted = true;
                    break;
                case AMOTION_EVENT_ACTION_UP:
                case AMOTION_EVENT_ACTION_POINTER_UP:
                case AMOTION_EVENT_ACTION_CANCEL:
                    out[n].kind = 1; idx = m->getActionIndex(); emitted = true;
                    break;
                case AMOTION_EVENT_ACTION_MOVE:
                    out[n].kind = 2; idx = 0; emitted = true;
                    break;
                case AMOTION_EVENT_ACTION_SCROLL:
                    out[n].kind = 3; idx = 0; emitted = true;
                    break;
                default:
                    break;
            }
            if (emitted) {
                out[n].pointer_id = m->getPointerId(idx);
                out[n].x          = m->getX(idx);
                // InputDispatcher delivers MotionEvent coordinates in
                // WINDOW-LOCAL space — the layer's display→window
                // transform (TRANSLATE 0,-Y for overlay layers) has
                // already been applied. So we pass m->getY through
                // verbatim; no offset subtraction needed.
                out[n].y          = m->getY(idx);
                out[n].pressure   = m->getPressure(idx);
                out[n].key_code   = 0;
                out[n].meta_state = 0;
            }
        } else if (ev->getType() == InputEventType::KEY) {
            KeyEvent* k = static_cast<KeyEvent*>(ev);
            switch (k->getAction()) {
                case AKEY_EVENT_ACTION_DOWN: out[n].kind = 10; emitted = true; break;
                case AKEY_EVENT_ACTION_UP:   out[n].kind = 11; emitted = true; break;
                // AKEY_EVENT_ACTION_MULTIPLE is deprecated; ignore.
                default: break;
            }
            if (emitted) {
                out[n].pointer_id = 0;
                out[n].x          = 0.0f;
                out[n].y          = 0.0f;
                out[n].pressure   = 0.0f;
                out[n].key_code   = k->getKeyCode();
                out[n].meta_state = k->getMetaState();
            }
        }
        g_input_consumer->sendFinishedSignal(seq, /*handled=*/true);
        if (emitted) {
            n++;
        }
    }
    return n;
}

// Re-request input focus for the wandr window. The standalone runtime has
// no Activity, so any activity-backed window (com.android.launcher3,
// Messaging, etc) that AMS resumes will steal focus from InputDispatcher's
// point of view, even though wandr owns the z-top SurfaceFlinger layer.
// Call this periodically from the host render loop to keep key events
// flowing to wandr. Returns 0 on success, -1 on failure.
int32_t sf_request_focus() {
    if (g_window_info == nullptr) {
        return -1;
    }
    gui::WindowInfo* wi = g_window_info->editInfo();
    gui::FocusRequest fr;
    fr.token      = wi->token;
    fr.windowName = wi->name;
    fr.timestamp  = systemTime(SYSTEM_TIME_MONOTONIC);
    fr.displayId  = ui::LogicalDisplayId::DEFAULT.val();
    SurfaceComposerClient::Transaction t;
    t.setFocusedWindow(fr);
    t.apply(/*synchronous=*/false);
    return 0;
}

// Query the live Android producer transform hint
// (NATIVE_WINDOW_TRANSFORM_HINT, a 0..7 bitmask: FLIP_H=1, FLIP_V=2,
// ROT_90=4). Must be called AFTER the host's EGL producer connects — the
// hint is not populated before then. Returns 0 if the surface is not up or
// the query fails. The host (canvas_impl.rs) maps its base transform from
// this value.
uint32_t sf_query_transform_hint() {
    if (g_surface == nullptr) {
        return 0;
    }
    int v = 0;
    status_t st = g_surface->query(NATIVE_WINDOW_TRANSFORM_HINT, &v);
    if (st != OK) {
        LOGE("query(NATIVE_WINDOW_TRANSFORM_HINT) failed: %d", st);
        return 0;
    }
    LOGI("transform hint queried: %d", v);
    return static_cast<uint32_t>(v);
}

// Reposition the wandr layer on the SurfaceFlinger z-axis (task 46 step 4/5).
// Higher z is drawn on top; default at creation is INT32_MAX. The arbiter
// pushes backgrounded apps to z=0 and pulls foreground to z=INT32_MAX —
// approximates AOSP's stacking with no shim source surface re-creation.
//
// Task 47 step 3c — for overlay surfaces, the geometry (position +
// layer + crop) lives on the PARENT container surface (BlastInputSurface
// pattern). Route setLayer to the parent so z-changes apply.
int32_t sf_set_layer(int32_t z) {
    sp<SurfaceControl> sc = g_overlay_parent != nullptr ? g_overlay_parent : g_control;
    if (sc == nullptr) {
        return -1;
    }
    SurfaceComposerClient::Transaction t;
    t.setLayer(sc, z);
    t.apply(/*synchronous=*/false);
    return 0;
}

// Toggle wandr-layer visibility (task 46 step 4/5). Cheaper than re-creating
// the surface: the layer stays allocated, its BBQ keeps the last frame, and
// re-showing is one Transaction round-trip. The arbiter prefers this over
// killing/relaunching the app for background transitions.
//
// Task 47 step 3c — for overlay surfaces, route show/hide to the
// PARENT container (which carries geometry). The buffer-state child
// remains shown — toggling visibility of the parent hides/shows the
// whole subtree.
int32_t sf_set_visible(int32_t visible) {
    sp<SurfaceControl> sc = g_overlay_parent != nullptr ? g_overlay_parent : g_control;
    if (sc == nullptr) {
        return -1;
    }
    SurfaceComposerClient::Transaction t;
    if (visible) {
        t.show(sc);
    } else {
        t.hide(sc);
    }
    t.apply(/*synchronous=*/false);
    return 0;
}

// Task 47 step 3c — allocate a BOTTOM-STRIP overlay SurfaceControl of
// height `height_px` pixels (panel-width × height_px), positioned at
// `(0, PANEL_H - height_px)`. The input window is registered for that
// same bottom rect; the rest of the panel keeps dispatching to whoever
// owns those z-layers (typically wandr-app's fullscreen surface).
//
// Starts INVISIBLE (`t.hide`) — the arbiter promotes to fg + flips
// visible explicitly via `sf_set_visible(1)` only when an editor
// focuses (cmd_overlay or auto-tied from cmd_attach_editor).
//
// Returns nullptr on bad height or any libgui error. `out_w` is set
// to PANEL_W; `out_h` to height_px; `out_transform` to 0 (host queries
// the live transform-hint via sf_query_transform_hint post-EGL-connect).
// Geometry-parameterized overlay surface. The shim is purpose-agnostic:
// the RUNTIME decides what each overlay is (status bar / IME / future
// bars) and passes a rectangle. Conventions (so callers needn't know the
// panel size): `w<=0` / `h<=0` → full panel width / height; `y<0` →
// bottom-anchored (shim computes PANEL_H - h). So the status bar passes
// (0,0,0,88), the IME passes (0,-1,0,1200). New bars are just new args —
// no new shim symbol, no new a-03 build.
ANativeWindow* sf_create_overlay_surface(int32_t x, int32_t y, int32_t w, int32_t h,
                                          int32_t* out_w, int32_t* out_h,
                                          uint32_t* out_transform) {
    if (h > static_cast<int32_t>(PANEL_H) || w > static_cast<int32_t>(PANEL_W)) {
        LOGE("[overlay] sf_create_overlay_surface: rect too big w=%d h=%d "
             "(panel %ux%u)", w, h, PANEL_W, PANEL_H);
        return nullptr;
    }

    ProcessState::self()->startThreadPool();

    g_client = new SurfaceComposerClient();
    status_t err = g_client->initCheck();
    if (err != NO_ERROR) {
        LOGE("[overlay] SurfaceComposerClient initCheck failed: %d", err);
        return nullptr;
    }

    std::vector<PhysicalDisplayId> ids =
        SurfaceComposerClient::getPhysicalDisplayIds();
    if (ids.empty()) {
        LOGE("[overlay] no physical displays");
        return nullptr;
    }
    g_display = SurfaceComposerClient::getPhysicalDisplayToken(ids[0]);
    if (g_display == nullptr) {
        LOGE("[overlay] getPhysicalDisplayToken returned null");
        return nullptr;
    }

    // Task 48 — query active display mode (idempotent; safe to call
    // even if sf_create_fullscreen_surface ran first).
    init_panel_dims(g_display);
    const uint32_t PW = PANEL_W;
    const uint32_t PH = PANEL_H;
    // Resolve the rect: w/h<=0 → full panel dimension; y<0 → bottom-anchored.
    const uint32_t W  = (w > 0) ? static_cast<uint32_t>(w) : PW;
    const uint32_t H  = (h > 0) ? static_cast<uint32_t>(h) : PH;
    const int32_t  X  = (x > 0) ? x : 0;
    const int32_t  Y  = (y >= 0) ? y : static_cast<int32_t>(PH - H);
    // Input translation: sf_input_poll subtracts this so the guest gets
    // surface-local Y. (X is 0 for the full-width horizontal strips we
    // support today; add an x-offset here if vertical/side bars land.)
    g_overlay_y_offset = Y;
    // Step 2 routing — this overlay only accepts touches inside its own strip
    // (global coords), so a tap on the app/another strip never reaches it.
    g_input_filter_active = true;
    g_input_fx = X; g_input_fy = Y;
    g_input_fw = static_cast<int32_t>(W); g_input_fh = static_cast<int32_t>(H);

    // Pin display projection to portrait identity — same reasoning as
    // sf_create_fullscreen_surface (clears any prior skew).
    {
        SurfaceComposerClient::Transaction t;
        t.setDisplayProjection(g_display, ui::ROTATION_0,
                               Rect(PW, PH), Rect(PW, PH));
        t.apply(/*synchronous=*/true);
    }

    // Parent container — AOSP BlastInputSurface pattern (test ref:
    // frameworks/native/libs/gui/tests/EndToEndNativeInputTest.cpp).
    // Position / layer / crop go on this PARENT; the BBQ-backed
    // buffer-state child inherits geometry. Setting position on the
    // buffer-state child directly was empirically a no-op on this
    // device.
    g_overlay_parent = g_client->createSurface(
        String8("wandr-overlay-parent"),
        /*w=*/0, /*h=*/0, PIXEL_FORMAT_RGBA_8888,
        ISurfaceComposerClient::eFXSurfaceContainer);
    if (g_overlay_parent == nullptr || !g_overlay_parent->isValid()) {
        LOGE("[overlay] createSurface(parent container) failed");
        return nullptr;
    }

    // Buffer-state child, parented to the container. PW × H buffer;
    // the parent supplies position + crop.
    g_control = g_client->createSurface(
        String8("wandr-overlay"), W, H, PIXEL_FORMAT_RGBA_8888,
        ISurfaceComposerClient::eFXSurfaceBufferState,
        g_overlay_parent->getHandle());
    if (g_control == nullptr || !g_control->isValid()) {
        LOGE("[overlay] createSurface(buffer child) failed");
        return nullptr;
    }

    // Same WANDR_SF_HINT env-var handling as fullscreen. Default: SF's
    // natural transform hint flows through to the producer.
    const char* pin_env = getenv("WANDR_SF_HINT");
    int pinned_hint = -1;
    if (pin_env != nullptr && pin_env[0] != '\0') {
        pinned_hint = atoi(pin_env);
    }
    {
        SurfaceComposerClient::Transaction t;
        // Parent: position + crop + layer (the geometry parts).
        // Initial z: just below i32::MAX. The arbiter's promote-to-
        // overlay calls sf_set_layer(i32::MAX) which we route to the
        // parent now (see sf_set_layer below).
        t.setLayer(g_overlay_parent, 0x7fffffff - 1);
        t.setPosition(g_overlay_parent, static_cast<float>(X), static_cast<float>(Y));
        t.setCrop(g_overlay_parent,
                  Rect(0, 0, static_cast<int32_t>(W), static_cast<int32_t>(H)));
        // Start HIDDEN — arbiter shows on attach-editor.
        t.hide(g_overlay_parent);

        // Child: flags + crop + show (the buffer parts).
        if (pinned_hint >= 0) {
            t.setFixedTransformHint(g_control, pinned_hint);
        }
        t.setFlags(g_control, layer_state_t::eLayerOpaque,
                   layer_state_t::eLayerOpaque);
        t.setCrop(g_control,
                  Rect(0, 0, static_cast<int32_t>(W), static_cast<int32_t>(H)));
        t.show(g_control);
        t.apply(/*synchronous=*/true);
    }
    if (pinned_hint >= 0) {
        g_control->setTransformHint(pinned_hint);
        LOGI("[overlay] transform hint pinned to %d (WANDR_SF_HINT)", pinned_hint);
    }

    g_bbq = sp<BLASTBufferQueue>::make(
        "wandr-overlay", g_control, W, H, PIXEL_FORMAT_RGBA_8888);
    g_surface = g_bbq->getSurface(/*includeSurfaceControlHandle=*/true);
    if (g_surface == nullptr) {
        LOGE("[overlay] BLASTBufferQueue getSurface returned null");
        return nullptr;
    }

    // Input window for the bottom strip only. Registered against the
    // child (BBQ buffer). The touchableRegion is in LAYER-LOCAL
    // coords: SurfaceFlinger adds the layer's position to convert to
    // display coords. With layer at (0, Y), passing Rect(0, 0, PW, H)
    // yields display-coord touchable region (0, Y) → (PW, Y + H) =
    // exactly the visible overlay strip.
    //
    // Bug found in initial smoke (task 49 step 2): passing
    // Rect(0, Y, PW, PH) yielded display coords (0, 2Y) → (PW, Y+PH)
    // = off-screen, so taps in the overlay area fell through to
    // wandr-app's fullscreen input window. The IME never saw any
    // touches; wandr-app's in-canvas keyboard got them.
    register_input_window_at(
        Rect(0, 0, static_cast<int32_t>(W), static_cast<int32_t>(H)),
        "wandr-overlay");

    if (out_w) *out_w = static_cast<int32_t>(W);
    if (out_h) *out_h = static_cast<int32_t>(H);
    if (out_transform) *out_transform = 0;
    LOGI("[overlay] surface created: %ux%u at (%d,%d), panel %ux%u",
         W, H, X, Y, PW, PH);
    return g_surface.get();
}

// Task 62 — general overlay move+resize. A strict superset of
// sf_resize_overlay: repositions the overlay to (X,Y) AND resizes it to
// W×H, using the SAME rect-resolution conventions as
// sf_create_overlay_surface (w<=0/h<=0 → full panel dim; y<0 →
// bottom-anchored; x<=0 → 0). Updates the parent's position + crop, the
// child's crop, the BLASTBufferQueue buffer dimensions, and the input
// window bounds. The Rust side calls `ANativeWindow_setBuffersGeometry`
// after this returns to flush EGL/Skia's view of the new buffer.
//
// This is what the overlay-rotation path (task 62) calls to flip a
// bottom strip into a vertical side strip on landscape: e.g.
// sf_set_overlay_geometry(PANEL_W - T, 0, T, PANEL_H) puts a T-wide
// keyboard down the physical right edge. Input still works verbatim —
// touchableRegion is layer-local Rect(0,0,W,H), SF adds the parent's
// position (incl. X), and InputDispatcher delivers window-local coords
// (so no g_overlay_y_offset / x-offset subtraction is needed; the host
// inverse-maps content rotation via base_matrix.invert()).
//
// Returns 0 on success, -1 if the surface isn't an overlay (or not yet
// created), -2 if the resolved rect is out of range.
int32_t sf_set_overlay_geometry(int32_t x, int32_t y, int32_t w, int32_t h) {
    if (g_control == nullptr || g_bbq == nullptr) {
        return -1;
    }
    const uint32_t PW = PANEL_W;
    const uint32_t PH = PANEL_H;
    const uint32_t W  = (w > 0) ? static_cast<uint32_t>(w) : PW;
    const uint32_t H  = (h > 0) ? static_cast<uint32_t>(h) : PH;
    if (W > PW || H > PH) {
        LOGE("[overlay] sf_set_overlay_geometry: rect too big w=%u h=%u "
             "(panel %ux%u)", W, H, PW, PH);
        return -2;
    }
    const int32_t  X  = (x > 0) ? x : 0;
    const int32_t  Y  = (y >= 0) ? y : static_cast<int32_t>(PH - H);
    g_overlay_y_offset = Y;  // parity only; sf_input_poll uses window-local coords

    {
        SurfaceComposerClient::Transaction t;
        // Position + crop go on the parent container.
        if (g_overlay_parent != nullptr) {
            t.setPosition(g_overlay_parent, static_cast<float>(X), static_cast<float>(Y));
            t.setCrop(g_overlay_parent,
                      Rect(0, 0, static_cast<int32_t>(W), static_cast<int32_t>(H)));
        }
        // Child crop also updates so the buffer-state surface knows its
        // bounded region matches the new W×H.
        t.setCrop(g_control,
                  Rect(0, 0, static_cast<int32_t>(W), static_cast<int32_t>(H)));
        t.apply(/*synchronous=*/true);
    }
    // Refresh the BBQ's notion of buffer dimensions. Without this it
    // keeps producing old-sized buffers that SF clips/distorts.
    g_bbq->update(g_control, W, H, PIXEL_FORMAT_RGBA_8888);

    // Re-register the input window at the new bounds. Layer-local coords —
    // SF adds the layer's position (X,Y) to convert to display coords.
    update_input_window_bounds(
        Rect(0, 0, static_cast<int32_t>(W), static_cast<int32_t>(H)));

    LOGI("[overlay] geometry set to %ux%u at (%d,%d)", W, H, X, Y);
    return 0;
}

// Task 62 — report the panel's native (portrait) dimensions. The host
// needs PANEL_H to build a rotated vertical side-strip rect (its buffer
// is only the strip-thickness tall, so the Rust side can't otherwise
// know the long edge). Populated by init_panel_dims at create time.
void sf_panel_dims(int32_t* out_w, int32_t* out_h) {
    if (out_w) *out_w = static_cast<int32_t>(PANEL_W);
    if (out_h) *out_h = static_cast<int32_t>(PANEL_H);
}

// Task 47 step 3c — resize the overlay SurfaceControl to `new_height_px`
// pixels tall (bottom-anchored, panel-width unchanged). Thin wrapper over
// sf_set_overlay_geometry so the IME `request-overlay-height` WIT path
// rides a single geometry codepath.
//
// Returns 0 on success, -1 if the surface isn't an overlay (or not yet
// created), -2 if `new_height_px` is out of range.
int32_t sf_resize_overlay(int32_t new_height_px) {
    if (new_height_px <= 0 || new_height_px > static_cast<int32_t>(PANEL_H)) {
        LOGE("[overlay] sf_resize_overlay: bad new_height_px=%d (panel H=%u)",
             new_height_px, PANEL_H);
        return -2;
    }
    // x=0, y=-1 (bottom-anchored), w=0 (full panel width), h=new_height_px.
    return sf_set_overlay_geometry(0, -1, 0, new_height_px);
}

// Task 80 Step 2 — set this host's input region (global display coords); touches
// outside it are dropped by our InputReader path. The fullscreen app calls this
// with its content rect (panel minus chrome insets) when the arbiter pushes
// geometry, so taps on the statusbar/taskbar strips don't leak to the app. A
// non-positive w or h clears the filter (accept all). No-op for the inputflinger
// path (which never consults the filter).
void sf_set_input_rect(int32_t x, int32_t y, int32_t w, int32_t h) {
    if (w <= 0 || h <= 0) {
        g_input_filter_active = false;
        return;
    }
    g_input_filter_active = true;
    g_input_fx = x; g_input_fy = y; g_input_fw = w; g_input_fh = h;
    LOGI("input region set to (%d,%d)-(%d,%d)", x, y, x + w, y + h);
}

// ── Task 93 Phase 4: media surfaces (video decode-to-surface + PiP self-view) ──
//
// Up to 4 slots. Each slot = a container SurfaceControl (carries geometry:
// position + scale matrix — the BlastInputSurface pattern, same reason as the
// overlay parent: geometry on a buffer-state child does not stick) + a
// buffer-state child + a BLASTBufferQueue sized to the PRODUCER's buffers
// (codec coded size / a real camera stream size like 640x480 — the camera
// derives its stream config from the consumer size, so this must be a
// supported size; the container's matrix scales it into the on-screen rect).
//
// Parenting — the SurfaceView model: when this process owns a main surface
// (g_control), the container is created as its CHILD, so it moves / hides /
// z-orders WITH the app automatically (role transitions need no new plumbing)
// and a NEGATIVE z composites below the app's own buffer — the app UI punches
// a transparent hole (see sf_set_opaque) and draws its controls around/over
// the video. In a surfaceless process (headless --run-once diagnostics) the
// container is a top-level layer at z=INT32_MAX instead.
struct MediaSlot {
    sp<SurfaceControl>   container;
    sp<SurfaceControl>   child;
    sp<BLASTBufferQueue> bbq;
    sp<Surface>          surface;
    // Current LOGICAL dims (post-transform; what set_rect scales from).
    int32_t              buf_w = 0;
    int32_t              buf_h = 0;
    // The producer's true buffer dims, as created (transform-independent).
    int32_t              producer_w = 0;
    int32_t              producer_h = 0;
};
static MediaSlot g_media[4];

// Lazily bring up a SurfaceComposerClient for a process that never created a
// main/overlay surface (the headless diagnostic path). Idempotent.
static bool ensure_media_client() {
    if (g_client != nullptr) return true;
    ProcessState::self()->startThreadPool();
    sp<SurfaceComposerClient> client = new SurfaceComposerClient();
    if (client->initCheck() != NO_ERROR) {
        LOGE("[media] SurfaceComposerClient initCheck failed");
        return false;
    }
    std::vector<PhysicalDisplayId> ids = SurfaceComposerClient::getPhysicalDisplayIds();
    if (ids.empty()) {
        LOGE("[media] no physical displays");
        return false;
    }
    g_display = SurfaceComposerClient::getPhysicalDisplayToken(ids[0]);
    if (g_display != nullptr) init_panel_dims(g_display);
    g_client = client;
    return true;
}

// Create a media surface. `buf_w`/`buf_h` = the producer's buffer size; `z` =
// sibling z relative to the app's own buffer (negative = behind it — remote
// video uses -2, the PiP self-view -1; ignored for the top-level fallback).
// Returns the slot id (>=0) and the producer ANativeWindow* via `out_window`
// (owned by the slot — valid until sf_media_destroy). Starts HIDDEN; call
// sf_media_set_rect + sf_media_set_visible(1).
int32_t sf_media_create(int32_t buf_w, int32_t buf_h, int32_t z, void** out_window) {
    if (out_window == nullptr || buf_w <= 0 || buf_h <= 0) return -1;
    if (!ensure_media_client()) return -1;
    int32_t slot = -1;
    for (int32_t i = 0; i < 4; i++) {
        if (g_media[i].container == nullptr) { slot = i; break; }
    }
    if (slot < 0) {
        LOGE("[media] no free media slot");
        return -1;
    }
    const bool top_level = (g_control == nullptr);
    char cname[40];
    snprintf(cname, sizeof(cname), "wandr-media-%d", slot);

    sp<SurfaceControl> container = g_client->createSurface(
        String8(cname), 0, 0, PIXEL_FORMAT_RGBA_8888,
        ISurfaceComposerClient::eFXSurfaceContainer,
        top_level ? nullptr : g_control->getHandle());
    if (container == nullptr || !container->isValid()) {
        LOGE("[media] createSurface(container) failed");
        return -1;
    }
    char bname[48];
    snprintf(bname, sizeof(bname), "wandr-media-buf-%d", slot);
    sp<SurfaceControl> child = g_client->createSurface(
        String8(bname), buf_w, buf_h, PIXEL_FORMAT_RGBA_8888,
        ISurfaceComposerClient::eFXSurfaceBufferState,
        container->getHandle());
    if (child == nullptr || !child->isValid()) {
        LOGE("[media] createSurface(buffer child) failed");
        return -1;
    }
    {
        SurfaceComposerClient::Transaction t;
        t.setLayer(container, top_level ? 0x7fffffff : z);
        t.hide(container);
        t.setCrop(child, Rect(0, 0, buf_w, buf_h));
        t.show(child);
        t.apply(/*synchronous=*/true);
    }
    sp<BLASTBufferQueue> bbq = sp<BLASTBufferQueue>::make(
        bname, child, static_cast<uint32_t>(buf_w), static_cast<uint32_t>(buf_h),
        PIXEL_FORMAT_RGBA_8888);
    sp<Surface> surface = bbq->getSurface(/*includeSurfaceControlHandle=*/true);
    if (surface == nullptr) {
        LOGE("[media] BLASTBufferQueue getSurface returned null");
        return -1;
    }
    g_media[slot].container = container;
    g_media[slot].child     = child;
    g_media[slot].bbq       = bbq;
    g_media[slot].surface   = surface;
    g_media[slot].buf_w      = buf_w;
    g_media[slot].buf_h      = buf_h;
    g_media[slot].producer_w = buf_w;
    g_media[slot].producer_h = buf_h;
    // The explicit upcast matters: Surface's ANativeWindow base subobject is
    // at a non-zero offset, and assigning straight into a void* skips the
    // pointer adjustment — the consumer (camera2ndk) then SIGSEGVs on a
    // garbage vtable. (The fullscreen path gets this implicitly from its
    // ANativeWindow* return type.)
    *out_window = static_cast<ANativeWindow*>(surface.get());
    LOGI("[media] slot %d created buf=%dx%d z=%d %s", slot, buf_w, buf_h, z,
         top_level ? "(top-level)" : "(child of app surface)");
    return slot;
}

// Position + size the media surface: `x,y,w,h` in the parent's coordinate
// space (= the app's surface pixels; panel pixels for the top-level fallback).
// The producer buffer (buf_w × buf_h) is scaled into the rect via the
// container's matrix.
int32_t sf_media_set_rect(int32_t slot, int32_t x, int32_t y, int32_t w, int32_t h) {
    if (slot < 0 || slot >= 4 || g_media[slot].container == nullptr) return -1;
    if (w <= 0 || h <= 0) return -1;
    const float sx = static_cast<float>(w) / static_cast<float>(g_media[slot].buf_w);
    const float sy = static_cast<float>(h) / static_cast<float>(g_media[slot].buf_h);
    SurfaceComposerClient::Transaction t;
    t.setPosition(g_media[slot].container, static_cast<float>(x), static_cast<float>(y));
    t.setMatrix(g_media[slot].container, sx, 0.0f, 0.0f, sy);
    t.apply(/*synchronous=*/false);
    return 0;
}

int32_t sf_media_set_visible(int32_t slot, int32_t visible) {
    if (slot < 0 || slot >= 4 || g_media[slot].container == nullptr) return -1;
    SurfaceComposerClient::Transaction t;
    if (visible) {
        t.show(g_media[slot].container);
    } else {
        t.hide(g_media[slot].container);
    }
    t.apply(/*synchronous=*/false);
    return 0;
}

// Rotate the media surface's BUFFER at composition (task 93 Phase 5 — camera
// sensor orientation for the PiP self-view; the decoder path rotates via
// MediaCodec "rotation-degrees" instead). `transform` is a NATIVE_WINDOW /
// HAL transform bitmask (ROT_90=4, ROT_180=3, ROT_270=7, 0=identity). For
// 90°/270° the buffer's logical dims swap, so the BBQ destination + crop are
// updated to the swapped size and the stored buf dims flip — `sf_media_set_rect`
// scaling then keeps the on-screen rect undistorted. Call BEFORE set_rect.
int32_t sf_media_set_transform(int32_t slot, uint32_t transform) {
    if (slot < 0 || slot >= 4 || g_media[slot].container == nullptr) return -1;
    // Logical (post-transform) dims derive from the producer's TRUE dims,
    // recorded at create — buf_w/buf_h hold the current logical size (used by
    // set_rect's scale math) and flip on a 90°/270° transform.
    const bool swaps = (transform & NATIVE_WINDOW_TRANSFORM_ROT_90) != 0;
    const int32_t pw = g_media[slot].producer_w;
    const int32_t ph = g_media[slot].producer_h;
    const int32_t nw = swaps ? ph : pw;
    const int32_t nh = swaps ? pw : ph;
    SurfaceComposerClient::Transaction t;
    t.setTransform(g_media[slot].child, transform);
    t.setCrop(g_media[slot].child, Rect(0, 0, nw, nh));
    t.apply(/*synchronous=*/true);
    g_media[slot].bbq->update(g_media[slot].child,
                              static_cast<uint32_t>(nw),
                              static_cast<uint32_t>(nh),
                              PIXEL_FORMAT_RGBA_8888);
    g_media[slot].buf_w = nw;
    g_media[slot].buf_h = nh;
    LOGI("[media] slot %d transform=%u logical=%dx%d", slot, transform, nw, nh);
    return 0;
}

// One-shot geometry (task 93 Phase 5 — supersedes the set_rect+set_transform
// pair for video): place the media surface at panel rect (x,y,w,h) with buffer
// `transform` (HAL bitmask), in ONE transaction. The BBQ destination + child
// crop become exactly (w,h) and the container matrix is identity, so the
// (transformed) producer buffer is scaled ONCE into the final on-screen rect —
// composing the old pair double-scaled when the transform swapped dims.
// Producer buffer size is independent (codec/camera set their own dims).
int32_t sf_media_set_geometry(int32_t slot, int32_t x, int32_t y, int32_t w,
                              int32_t h, uint32_t transform) {
    if (slot < 0 || slot >= 4 || g_media[slot].container == nullptr) return -1;
    if (w <= 0 || h <= 0) return -1;
    {
        SurfaceComposerClient::Transaction t;
        t.setTransform(g_media[slot].child, transform);
        t.setCrop(g_media[slot].child, Rect(0, 0, w, h));
        t.setPosition(g_media[slot].container, static_cast<float>(x), static_cast<float>(y));
        t.setMatrix(g_media[slot].container, 1.0f, 0.0f, 0.0f, 1.0f);
        t.apply(/*synchronous=*/false);
    }
    g_media[slot].bbq->update(g_media[slot].child,
                              static_cast<uint32_t>(w), static_cast<uint32_t>(h),
                              PIXEL_FORMAT_RGBA_8888);
    g_media[slot].buf_w = w;
    g_media[slot].buf_h = h;
    return 0;
}

// Release one media slot: hide synchronously (so the codec/camera producer is
// gone from the screen before its buffers die), then drop the refs — with no
// owner left, SurfaceFlinger removes the layers.
void sf_media_destroy(int32_t slot) {
    if (slot < 0 || slot >= 4 || g_media[slot].container == nullptr) return;
    {
        SurfaceComposerClient::Transaction t;
        t.hide(g_media[slot].container);
        t.apply(/*synchronous=*/true);
    }
    g_media[slot].surface.clear();
    g_media[slot].bbq.clear();
    g_media[slot].child.clear();
    g_media[slot].container.clear();
    g_media[slot].buf_w = g_media[slot].buf_h = 0;
    g_media[slot].producer_w = g_media[slot].producer_h = 0;
    LOGI("[media] slot %d destroyed", slot);
}

// Toggle the main layer's eLayerOpaque flag. The fullscreen surface is created
// opaque (SF skips blending). A behind-the-UI media surface (negative-z child)
// only shows through pixels the guest leaves transparent, which requires the
// layer to BLEND — so the host clears the flag while a decode-to-surface video
// is up and restores it after. Returns -1 if there is no main surface.
int32_t sf_set_opaque(int32_t opaque) {
    if (g_control == nullptr) return -1;
    SurfaceComposerClient::Transaction t;
    t.setFlags(g_control, opaque ? layer_state_t::eLayerOpaque : 0,
               layer_state_t::eLayerOpaque);
    t.apply(/*synchronous=*/false);
    return 0;
}

// Release the surface, control, client and input plumbing.
void sf_destroy_surface() {
    for (int32_t i = 0; i < 4; i++) sf_media_destroy(i);
    g_input_consumer.reset();
    g_input_channel.reset();
    g_window_info.clear();
    g_surface.clear();
    g_bbq.clear();
    g_control.clear();
    g_overlay_parent.clear();
    g_display.clear();
    g_client.clear();
    g_overlay_y_offset = 0;
}

}  // extern "C"
