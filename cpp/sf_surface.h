// sf_surface — C ABI of the task-33 libgui surface shim (libsf_surface.so).
// wandr-host dlopen()s the .so and dlsym()s these symbols; this header is
// the contract (and documentation for the Rust mirror in src/sf_surface.rs).
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

struct ANativeWindow;

// Allocate a fullscreen top-z-order SurfaceControl from SurfaceFlinger and
// return its ANativeWindow* (drive EGL on it). Writes the portrait logical
// dimensions to out_w/out_h and the SurfaceFlinger display rotation
// (ui::Rotation, 0..3) to out_transform, all if non-null. Returns NULL on
// failure.
struct ANativeWindow* sf_create_fullscreen_surface(int32_t* out_w,
                                                   int32_t* out_h,
                                                   uint32_t* out_transform);

// POD input event drained by sf_input_poll(). Mirrored in sf_surface.cpp and
// in the Rust side (src/sf_surface.rs) — keep all three in sync.
struct SfInputEvent {
    int32_t kind;        // 0=down 1=up 2=move 3=scroll  10=key-down 11=key-up
    int32_t pointer_id;  // multi-touch pointer id (0..N); 0 for key events
    float   x;
    float   y;
    float   pressure;    // 0.0..1.0; 0 for key events
    int32_t key_code;    // AKEYCODE_* for key events; 0 otherwise
    int32_t meta_state;  // shift/alt/ctrl bits (AMETA_*) for key events; 0 otherwise
};

// Drain pending InputFlinger events into `out` (capacity `max`); returns the
// count written. Non-blocking — call once per frame. Returns 0 if the input
// channel was never set up (e.g. inputflinger unavailable).
int32_t sf_input_poll(struct SfInputEvent* out, int32_t max);

// Re-request input focus for the wandr window. The standalone runtime has no
// Activity, so any activity-backed window AMS resumes (com.android.launcher3,
// Messaging, …) steals InputDispatcher focus even though wandr owns the z-top
// SurfaceFlinger layer. Call periodically from the host loop to keep key
// events flowing. Returns 0 on success, -1 on failure.
int32_t sf_request_focus(void);

// Query the live Android producer transform hint (NATIVE_WINDOW_TRANSFORM_HINT,
// a 0..7 bitmask: FLIP_H=1, FLIP_V=2, ROT_90=4). Call only AFTER the host's
// EGL producer has connected — the hint is unpopulated before then. Returns 0
// if the surface is down or the query fails.
uint32_t sf_query_transform_hint(void);

// Reposition the wandr layer on the SurfaceFlinger z-axis (task 46 step 4/5).
// `z` is an int32 — higher values are drawn on top. The default at creation
// is INT32_MAX (top of everything except system overlays). Background apps
// in the arbiter's policy should drop to 0; foreground returns to INT32_MAX.
// Returns 0 on success, -1 if the surface is down.
int32_t sf_set_layer(int32_t z);

// Toggle wandr-layer visibility (task 46 step 4/5). `visible` non-zero shows
// the layer; zero hides it. Cheaper than re-creating the SF surface for
// "background" semantics — the layer stays allocated, its BBQ keeps the
// last frame, and re-showing is one Transaction round-trip. Returns 0 on
// success, -1 if the surface is down.
int32_t sf_set_visible(int32_t visible);

// Task 47 step 3c — allocate a bottom-strip OVERLAY SurfaceControl of
// `height_px` pixels (panel-width × height_px), positioned at
// `(0, PANEL_H - height_px)`. The input window is registered for that
// same bottom rect; sf_input_poll subtracts the Y offset so motion
// events arrive in surface-local coords.
//
// Starts INVISIBLE — the arbiter promotes the IME to fg + flips
// visible only when an editor focuses (cmd_overlay or auto-tied from
// cmd_attach_editor). Returns NULL on bad height or any libgui error.
// Geometry-parameterized overlay surface (task 47 IME + task 55 status
// bar + future bars). Conventions: w<=0/h<=0 → full panel width/height;
// y<0 → bottom-anchored. Status bar = (0,0,0,88); IME = (0,-1,0,1200).
struct ANativeWindow* sf_create_overlay_surface(int32_t x, int32_t y,
                                                int32_t w, int32_t h,
                                                int32_t* out_w,
                                                int32_t* out_h,
                                                uint32_t* out_transform);

// Task 47 step 3c — resize an existing overlay SurfaceControl to
// `new_height_px` pixels tall. Re-positions to
// `(0, PANEL_H - new_height_px)`, updates the BLASTBufferQueue's
// buffer dimensions, re-registers the input window at the new rect,
// and updates the overlay Y offset so subsequent sf_input_poll calls
// translate motion-event Y values correctly. The Rust side calls
// ANativeWindow_setBuffersGeometry after this returns to flush
// EGL/Skia's view of the new dimensions. Returns 0 on success, -1 if
// the surface is down, -2 if `new_height_px` is out of range.
int32_t sf_resize_overlay(int32_t new_height_px);

// Task 62 — general overlay move+resize. Superset of sf_resize_overlay:
// repositions to (x,y) AND resizes to w×h, with the same conventions as
// sf_create_overlay_surface (w<=0/h<=0 → full panel dim; y<0 →
// bottom-anchored; x<=0 → 0). Used by the overlay-rotation path to flip
// a bottom strip into a vertical side strip on landscape. The Rust side
// calls ANativeWindow_setBuffersGeometry after this returns. Returns 0
// on success, -1 if the surface is down, -2 if the rect is out of range.
int32_t sf_set_overlay_geometry(int32_t x, int32_t y, int32_t w, int32_t h);

// Task 62 — report the panel's native (portrait) dimensions in pixels.
// The host needs PANEL_H to build a rotated side-strip rect. Either
// out-pointer may be NULL.
void sf_panel_dims(int32_t* out_w, int32_t* out_h);

// Task 80 Step 2 — set this host's input region (global display coords). With the
// ART-less InputReader path, every host sees every touch; touches outside this
// rect are dropped so chrome/app input don't leak. The fullscreen app sets its
// content rect (panel minus chrome insets) when the arbiter pushes geometry;
// overlays self-set their strip at create. Non-positive w/h clears (accept all).
// No-op for the inputflinger path.
void sf_set_input_rect(int32_t x, int32_t y, int32_t w, int32_t h);

// ── Task 93 Phase 4: media surfaces (video decode-to-surface + PiP self-view) ──
// A media surface = a SurfaceControl subtree whose producer ANativeWindow* is
// handed to AMediaCodec (decode-to-surface) or the camera (self-view preview).
// Child of this process's main surface when one exists (the SurfaceView model:
// negative `z` composites BELOW the app's buffer — pair with sf_set_opaque(0)
// and a transparent hole in the guest UI); top-level z=MAX in a surfaceless
// (headless diagnostic) process. `buf_w/buf_h` = producer buffer size (must be
// a real camera/codec size); the container scales it into the on-screen rect.
// Returns slot id >=0 (4 slots) or -1; window valid until sf_media_destroy.
int32_t sf_media_create(int32_t buf_w, int32_t buf_h, int32_t z, void** out_window);
// Rect in the parent surface's pixel space (panel pixels when top-level).
int32_t sf_media_set_rect(int32_t slot, int32_t x, int32_t y, int32_t w, int32_t h);
int32_t sf_media_set_visible(int32_t slot, int32_t visible);
// Rotate the buffer at composition (NATIVE_WINDOW transform: ROT_90=4, 180=3,
// 270=7). 90/270 swap the logical dims (BBQ + crop updated); call before set_rect.
int32_t sf_media_set_transform(int32_t slot, uint32_t transform);
// One-shot: rect (panel px) + buffer transform in one transaction; the
// transformed producer buffer scales ONCE into the rect (identity matrix).
int32_t sf_media_set_geometry(int32_t slot, int32_t x, int32_t y, int32_t w,
                              int32_t h, uint32_t transform);
void    sf_media_destroy(int32_t slot);
// Toggle the main layer's eLayerOpaque flag (clear while a behind-the-UI video
// surface is up so the guest's transparent hole blends; restore after).
int32_t sf_set_opaque(int32_t opaque);

// Release the surface/control/client and input plumbing.
void sf_destroy_surface(void);

#ifdef __cplusplus
}
#endif
