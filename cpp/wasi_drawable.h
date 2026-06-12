// WasiDrawable — SkDrawable subclass with a *swappable* inner drawable
// AND live transform/clip/alpha/shadow attrs applied at onDraw time. The
// transform attrs are the wasi analogue of an Android RenderNode's
// hardware-layer properties: setting them is a non-recordable host-state
// mutation, so a child layer can reposition (e.g. during scroll) without
// invalidating any parent's recording. See cpp/wasi_drawable.cpp::onDraw.
#pragma once

#include "include/core/SkDrawable.h"
#include "include/core/SkMatrix.h"
#include "include/core/SkPath.h"
#include "include/core/SkRect.h"
#include "include/core/SkRRect.h"
#include "include/core/SkRefCnt.h"

class SkCanvas;
class SkPictureRecorder;

class WasiDrawable final : public SkDrawable {
public:
    static sk_sp<WasiDrawable> Make();

    /// Replace the inner drawable. `inner` may be null to clear.
    /// Bumps `inner`'s refcount; caller still owns its own ref.
    void setInner(sk_sp<SkDrawable> inner);
    void setBounds(const SkRect& r);

    void setTransform(
        SkScalar layerX, SkScalar layerY,
        SkScalar translationX, SkScalar translationY,
        SkScalar scaleX, SkScalar scaleY,
        SkScalar rotationZ,
        SkScalar pivotX, SkScalar pivotY,
        SkScalar alpha);

    /// scene 0.0.2 path: an EXPLICIT 3x3 replaces the property-derived
    /// transform (last setter wins; alpha separate via setAlpha).
    void setMatrix(const SkMatrix& m);
    void setAlpha(SkScalar alpha);

    enum class ClipKind { None, Rect, RRect, Path };
    void setClipRect(const SkRect& r, bool antialias);
    void setClipRRect(const SkRRect& rr, bool antialias);
    void setClipPath(const SkPath& p, bool antialias);
    void clearClip();

    void setShadowElevation(SkScalar e);

protected:
    SkRect onGetBounds() override;
    void onDraw(SkCanvas* canvas) override;

private:
    WasiDrawable();
    sk_sp<SkDrawable> fInner;
    SkRect fBounds;

    // Transform attrs (applied each onDraw).
    SkScalar fLayerX = 0, fLayerY = 0;
    SkScalar fTranslationX = 0, fTranslationY = 0;
    SkScalar fScaleX = 1, fScaleY = 1;
    SkScalar fRotationZ = 0;
    SkScalar fPivotX = 0, fPivotY = 0;
    SkScalar fAlpha = 1.0f;

    // scene 0.0.2: explicit matrix mode (supersedes the property
    // pipeline when set).
    bool fHasMatrix = false;
    SkMatrix fMatrix = SkMatrix::I();

    // Clip applied AFTER transforms.
    ClipKind fClipKind = ClipKind::None;
    SkRect fClipRect = SkRect::MakeEmpty();
    SkRRect fClipRRect;
    SkPath fClipPath;
    bool fClipAA = true;

    // Coarse shadow: drop a translucent fill of the clip shape offset
    // downward, BEFORE applying clip. 0 = no shadow.
    SkScalar fShadowElevation = 0;
};

extern "C" {
    SkDrawable* wasi_drawable_create();
    void wasi_drawable_set_inner(SkDrawable* outer, SkDrawable* inner);
    void wasi_drawable_set_bounds(SkDrawable* d,
                                  float l, float t, float r, float b);
    void wasi_drawable_set_transform(SkDrawable* d,
                                     float layer_x, float layer_y,
                                     float translation_x, float translation_y,
                                     float scale_x, float scale_y,
                                     float rotation_z,
                                     float pivot_x, float pivot_y,
                                     float alpha);
    void wasi_drawable_set_clip_rect(SkDrawable* d,
                                     float l, float t, float r, float b,
                                     bool antialias);
    /// `radii` is an 8-element array of (x,y) radii for each of the 4
    /// corners, in upper-left → upper-right → lower-right → lower-left
    /// order (matching SkRRect::setRectRadii).
    void wasi_drawable_set_clip_rrect(SkDrawable* d,
                                      float l, float t, float r, float b,
                                      const float* radii_xy_4_corners,
                                      bool antialias);
    void wasi_drawable_clear_clip(SkDrawable* d);
    void wasi_drawable_set_shadow_elevation(SkDrawable* d, float elevation);

    // scene 0.0.2 entries.
    void wasi_drawable_set_matrix(SkDrawable* d,
                                  float m00, float m01, float m02,
                                  float m10, float m11, float m12,
                                  float m20, float m21, float m22);
    void wasi_drawable_set_alpha(SkDrawable* d, float alpha);
    /// `path` is an SkPath* (copied; caller keeps ownership).
    void wasi_drawable_set_clip_path(SkDrawable* d, const void* path,
                                     bool antialias);

    void wasi_drawable_ref(SkDrawable* d);
    void wasi_drawable_unref(SkDrawable* d);
    void wasi_canvas_draw_drawable(SkCanvas* canvas, SkDrawable* d);
}
