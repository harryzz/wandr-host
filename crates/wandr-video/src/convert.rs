//! Pixel-format plumbing — the `libswscale` replacement.
//!
//! FFmpeg's swscale did colorspace conversion AND resize in one `Scaler::run`.
//! Here they are split: `fast_image_resize` (pure Rust SIMD) resizes in RGB, then
//! the `yuv` crate (pure Rust SIMD) converts. Resizing before converting matches
//! swscale's effective ordering and keeps chroma siting trivial.
//!
//! ‼️ COLORSPACE IS LOAD-BEARING. swscale's default for RGB24↔YUV420P is
//! BT.601 with LIMITED (studio) range, and that is also VP8/VP9's default
//! `color_range`. Getting either wrong does not error — it produces washed-out or
//! crushed video that looks exactly like a broken encoder. Both directions below
//! MUST agree, which is why the constants are defined once here and never inline.

use yuv::{
    rgb_to_yuv420, yuv420_to_rgba, BufferStoreMut, YuvChromaSubsampling, YuvConversionMode,
    YuvPlanarImage, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
};

use crate::{I420Ref, CodecError};

/// The one source of truth for colorspace. See the module note — these must match
/// on both the encode and decode paths.
/// Encoder-side default. Our own encoder produces BT.601 limited, and the RGB->
/// YUV direction has no stream to read a matrix from.
const RANGE: YuvRange = YuvRange::Limited;
const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt601;

/// Map a decoded frame's signalled colour onto the converter's vocabulary, so
/// the CPU lane converts with the SAME matrix the GPU sampler is told to use.
fn yuv_params(color: crate::ColorInfo) -> (YuvRange, YuvStandardMatrix) {
    let range = if color.full_range { YuvRange::Full } else { YuvRange::Limited };
    let matrix = match color.matrix {
        crate::ColorMatrix::Bt601 => YuvStandardMatrix::Bt601,
        crate::ColorMatrix::Bt709 => YuvStandardMatrix::Bt709,
        crate::ColorMatrix::Bt2020 => YuvStandardMatrix::Bt2020,
    };
    (range, matrix)
}
/// `Balanced` is the crate default: still very fast (SIMD), but materially more
/// precise than the `Fast` tier, which is only the libyuv-equivalent approximation
/// and sits behind a `fast_mode` feature. At call resolutions the extra precision
/// is free in practice, so there is no reason to take the approximation.
const MODE: YuvConversionMode = YuvConversionMode::Balanced;

/// A borrowed, tightly-packed RGB24 frame (the shape nokhwa's `RgbFormat` decode
/// produces, and what the Android PiP path hands over).
pub struct Rgb24Frame<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
}

impl<'a> Rgb24Frame<'a> {
    pub fn new(data: &'a [u8], width: u32, height: u32) -> Self {
        Self { data, width, height }
    }
}

/// Resize (only if needed) and convert RGB24 → I420, writing directly into
/// caller-supplied plane slices — which are libvpx's own image planes, so there is
/// no intermediate buffer on the encode path.
///
/// `scratch` carries the resized RGB between calls so a steady-state encode does
/// no allocation.
#[allow(clippy::too_many_arguments)]
pub fn rgb24_into_i420(
    src: Rgb24Frame<'_>,
    dst_w: u32,
    dst_h: u32,
    y: &mut [u8],
    y_stride: u32,
    u: &mut [u8],
    u_stride: u32,
    v: &mut [u8],
    v_stride: u32,
    scratch: &mut Vec<u8>,
    resizer: &mut fast_image_resize::Resizer,
) -> Result<(), CodecError> {
    let expected = src.width as usize * src.height as usize * 3;
    if src.data.len() < expected {
        log::warn!(
            "wandr-video: short RGB frame: {} < {expected} ({}x{})",
            src.data.len(),
            src.width,
            src.height
        );
        return Err(CodecError::BadFrame);
    }

    // Skip the resize entirely when the camera already matches the encode size —
    // the common case (the call path is 640x480 on both sides). swscale used to
    // run unconditionally, so this is CPU back.
    let (rgb, rgb_stride): (&[u8], u32) = if src.width == dst_w && src.height == dst_h {
        (&src.data[..expected], src.width * 3)
    } else {
        use fast_image_resize::images::{Image, ImageRef};
        use fast_image_resize::PixelType;

        let src_img = ImageRef::new(src.width, src.height, &src.data[..expected], PixelType::U8x3)
            .map_err(|e| {
                log::warn!("wandr-video: resize src: {e:?}");
                CodecError::BadFrame
            })?;
        scratch.resize(dst_w as usize * dst_h as usize * 3, 0);
        let mut dst_img = Image::from_slice_u8(dst_w, dst_h, scratch, PixelType::U8x3)
            .map_err(|e| {
                log::warn!("wandr-video: resize dst: {e:?}");
                CodecError::BadFrame
            })?;
        resizer.resize(&src_img, &mut dst_img, None).map_err(|e| {
            log::warn!("wandr-video: resize: {e:?}");
            CodecError::BadFrame
        })?;
        (scratch.as_slice(), dst_w * 3)
    };

    let mut planar = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y),
        y_stride,
        u_plane: BufferStoreMut::Borrowed(u),
        u_stride,
        v_plane: BufferStoreMut::Borrowed(v),
        v_stride,
        width: dst_w,
        height: dst_h,
    };
    rgb_to_yuv420(&mut planar, rgb, rgb_stride, RANGE, MATRIX, MODE).map_err(|e| {
        log::warn!("wandr-video: rgb->i420: {e:?}");
        CodecError::BadFrame
    })
}

/// Convert a decoded I420 frame to tightly-packed RGBA8888 (what Skia's raster
/// upload wants). Called by the host ONLY in decode-to-surface mode.
///
/// Writes into `out`, resizing it to exactly `w * h * 4`.
/// I420 -> RGBA using the frame's own signalled colour. Kept as the default
/// entry point; `i420_to_rgba_with` takes an explicit `ColorInfo` for callers
/// that hold one (the GPU readback lane, which must match the sampler).
pub fn i420_to_rgba(frame: &I420Ref<'_>, out: &mut Vec<u8>) -> Result<(), CodecError> {
    i420_to_rgba_with(frame, crate::ColorInfo::for_resolution(frame.width, frame.height), out)
}

pub fn i420_to_rgba_with(
    frame: &I420Ref<'_>,
    color: crate::ColorInfo,
    out: &mut Vec<u8>,
) -> Result<(), CodecError> {
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 {
        return Err(CodecError::BadFrame);
    }
    out.resize(w as usize * h as usize * 4, 0);

    let planar = YuvPlanarImage {
        y_plane: frame.y,
        y_stride: frame.y_stride,
        u_plane: frame.u,
        u_stride: frame.u_stride,
        v_plane: frame.v,
        v_stride: frame.v_stride,
        width: w,
        height: h,
    };
    // The `yuv` crate writes a tightly-packed destination directly, which is why
    // the old manual stride-unpacking loop is gone.
    let (range, matrix) = yuv_params(color);
    yuv420_to_rgba(&planar, out, w * 4, range, matrix).map_err(|e| {
        log::warn!("wandr-video: i420->rgba: {e:?}");
        CodecError::BadFrame
    })
}

/// Chroma plane dimensions for I420 at `w`x`h` (round up — odd sizes are legal).
pub const fn chroma_dims(w: u32, h: u32) -> (u32, u32) {
    (w.div_ceil(2), h.div_ceil(2))
}

/// Allocate an owned I420 planar image — test/utility helper, not on the hot path.
pub fn alloc_i420(w: u32, h: u32) -> YuvPlanarImageMut<'static, u8> {
    YuvPlanarImageMut::alloc(w, h, YuvChromaSubsampling::Yuv420)
}
