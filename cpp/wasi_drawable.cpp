#include "wasi_drawable.h"

#include "include/core/SkCanvas.h"
#include "include/core/SkClipOp.h"
#include "include/core/SkPaint.h"
#include "include/core/SkPoint.h"
#include "include/core/SkColor.h"

#include <algorithm>
#include <cmath>

WasiDrawable::WasiDrawable() : fBounds(SkRect::MakeEmpty()) {}

sk_sp<WasiDrawable> WasiDrawable::Make() {
    return sk_sp<WasiDrawable>(new WasiDrawable());
}

void WasiDrawable::setInner(sk_sp<SkDrawable> inner) {
    fInner = std::move(inner);
    notifyDrawingChanged();
}

void WasiDrawable::setBounds(const SkRect& r) {
    fBounds = r;
}

void WasiDrawable::setTransform(
    SkScalar layerX, SkScalar layerY,
    SkScalar translationX, SkScalar translationY,
    SkScalar scaleX, SkScalar scaleY,
    SkScalar rotationZ,
    SkScalar pivotX, SkScalar pivotY,
    SkScalar alpha) {
    fLayerX = layerX; fLayerY = layerY;
    fTranslationX = translationX; fTranslationY = translationY;
    fScaleX = scaleX; fScaleY = scaleY;
    fRotationZ = rotationZ;
    fPivotX = pivotX; fPivotY = pivotY;
    fAlpha = alpha;
}

void WasiDrawable::setClipRect(const SkRect& r, bool antialias) {
    fClipKind = ClipKind::Rect;
    fClipRect = r;
    fClipAA = antialias;
}

void WasiDrawable::setClipRRect(const SkRRect& rr, bool antialias) {
    fClipKind = ClipKind::RRect;
    fClipRRect = rr;
    fClipAA = antialias;
}

void WasiDrawable::clearClip() {
    fClipKind = ClipKind::None;
}

void WasiDrawable::setShadowElevation(SkScalar e) {
    fShadowElevation = e;
}

SkRect WasiDrawable::onGetBounds() {
    return fBounds;
}

void WasiDrawable::onDraw(SkCanvas* canvas) {
    if (!fInner) return;
    canvas->save();

    // 1. Position into parent space.
    if (fLayerX != 0 || fLayerY != 0)
        canvas->translate(fLayerX, fLayerY);

    // 2. Compose-model translation (post-position, pre-scale/rotate).
    if (fTranslationX != 0 || fTranslationY != 0)
        canvas->translate(fTranslationX, fTranslationY);

    // 3. Scale + rotate around pivot (T(pivot) · S · R · T(-pivot)).
    bool hasTransform = fScaleX != 1 || fScaleY != 1 || fRotationZ != 0;
    if (hasTransform) {
        if (fPivotX != 0 || fPivotY != 0) canvas->translate(fPivotX, fPivotY);
        if (fScaleX != 1 || fScaleY != 1) canvas->scale(fScaleX, fScaleY);
        if (fRotationZ != 0) canvas->rotate(fRotationZ);
        if (fPivotX != 0 || fPivotY != 0) canvas->translate(-fPivotX, -fPivotY);
    }

    // 4. Coarse shadow (before clip — shadow can extend past the layer).
    if (fShadowElevation > 0 && fClipKind != ClipKind::None) {
        SkScalar dy = fShadowElevation * 0.8f;
        int alphaByte = (int)std::lround(fShadowElevation * 12.0f);
        if (alphaByte > 64) alphaByte = 64;
        if (alphaByte < 0) alphaByte = 0;
        SkPaint sp;
        sp.setColor(SkColorSetARGB((U8CPU)alphaByte, 0, 0, 0));
        canvas->save();
        canvas->translate(0, dy);
        switch (fClipKind) {
            case ClipKind::Rect:  canvas->drawRect(fClipRect, sp);   break;
            case ClipKind::RRect: canvas->drawRRect(fClipRRect, sp); break;
            default: break;
        }
        canvas->restore();
    }

    // 5. Clip.
    switch (fClipKind) {
        case ClipKind::Rect:
            canvas->clipRect(fClipRect, SkClipOp::kIntersect, fClipAA);
            break;
        case ClipKind::RRect:
            canvas->clipRRect(fClipRRect, SkClipOp::kIntersect, fClipAA);
            break;
        default: break;
    }

    // 6. Alpha-only saveLayer (for layerPaint/colorFilter/imageFilter the
    // caller still uses parent-recording-captured canvas.saveLayer; those
    // change less often and aren't part of the scroll/transform hot path).
    bool needsAlphaLayer = fAlpha < 1.0f;
    if (needsAlphaLayer) {
        SkPaint p;
        p.setAlphaf(fAlpha);
        SkRect layerBounds = SkRect::MakeWH(fBounds.width(), fBounds.height());
        canvas->saveLayer(&layerBounds, &p);
    }

    canvas->drawDrawable(fInner.get(), nullptr);

    if (needsAlphaLayer) canvas->restore();
    canvas->restore();
}

extern "C" SkDrawable* wasi_drawable_create() {
    return WasiDrawable::Make().release();
}

extern "C" void wasi_drawable_set_inner(SkDrawable* outer, SkDrawable* inner) {
    static_cast<WasiDrawable*>(outer)->setInner(sk_ref_sp(inner));
}

extern "C" void wasi_drawable_set_bounds(SkDrawable* d,
                                         float l, float t, float r, float b) {
    static_cast<WasiDrawable*>(d)->setBounds(SkRect::MakeLTRB(l, t, r, b));
}

extern "C" void wasi_drawable_set_transform(SkDrawable* d,
                                            float layer_x, float layer_y,
                                            float translation_x, float translation_y,
                                            float scale_x, float scale_y,
                                            float rotation_z,
                                            float pivot_x, float pivot_y,
                                            float alpha) {
    static_cast<WasiDrawable*>(d)->setTransform(
        layer_x, layer_y, translation_x, translation_y,
        scale_x, scale_y, rotation_z, pivot_x, pivot_y, alpha);
}

extern "C" void wasi_drawable_set_clip_rect(SkDrawable* d,
                                            float l, float t, float r, float b,
                                            bool antialias) {
    static_cast<WasiDrawable*>(d)->setClipRect(SkRect::MakeLTRB(l, t, r, b), antialias);
}

extern "C" void wasi_drawable_set_clip_rrect(SkDrawable* d,
                                             float l, float t, float r, float b,
                                             const float* radii_xy_4_corners,
                                             bool antialias) {
    SkRRect rr;
    SkVector radii[4] = {
        { radii_xy_4_corners[0], radii_xy_4_corners[1] }, // UL
        { radii_xy_4_corners[2], radii_xy_4_corners[3] }, // UR
        { radii_xy_4_corners[4], radii_xy_4_corners[5] }, // LR
        { radii_xy_4_corners[6], radii_xy_4_corners[7] }, // LL
    };
    rr.setRectRadii(SkRect::MakeLTRB(l, t, r, b), radii);
    static_cast<WasiDrawable*>(d)->setClipRRect(rr, antialias);
}

extern "C" void wasi_drawable_clear_clip(SkDrawable* d) {
    static_cast<WasiDrawable*>(d)->clearClip();
}

extern "C" void wasi_drawable_set_shadow_elevation(SkDrawable* d, float elevation) {
    static_cast<WasiDrawable*>(d)->setShadowElevation(elevation);
}

extern "C" void wasi_drawable_ref(SkDrawable* d) {
    if (d) d->ref();
}

extern "C" void wasi_drawable_unref(SkDrawable* d) {
    if (d) d->unref();
}

extern "C" void wasi_canvas_draw_drawable(SkCanvas* canvas, SkDrawable* d) {
    canvas->drawDrawable(d, /*matrix=*/nullptr);
}
