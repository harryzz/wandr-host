//! GPU zero-copy interop shared by the GStreamer backend.
//!
//! These pieces used to live inside the hand-written `d3d11` / `videotoolbox`
//! decoders. Those decoders are retired, but the GStreamer backend still needs the
//! per-OS glue to hand a decoded GPU frame to the host compositor, so it lives here,
//! gated on the `gstreamer` feature (the only consumer):
//!
//! - Windows: the ANGLE `ID3D11Device` handoff (host → decoder, so the decoded texture
//!   is a same-device alias ANGLE can import) + an NV12 D3D11-texture readback (the
//!   import fallback).
//! - macOS: a `CVPixelBuffer` → I420 readback (the IOSurface import fallback).
//!
//! Linux (dma-buf) needs no such glue — `gstreamer-allocators` + the host's dma-buf
//! EGL import cover it directly.

/// NV12 (Y plane + interleaved CbCr) → tightly-packed I420 (Y, then U, then V).
#[cfg(target_os = "windows")]
pub(crate) unsafe fn pack_nv12_i420(base: *const u8, stride: usize, width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let uv = base.add(stride * h);
    let mut out = vec![0u8; w * h + 2 * cw * ch];
    for y in 0..h {
        let src = std::slice::from_raw_parts(base.add(y * stride), w);
        out[y * w..y * w + w].copy_from_slice(src);
    }
    let (u_off, v_off) = (w * h, w * h + cw * ch);
    for y in 0..ch {
        let row = std::slice::from_raw_parts(uv.add(y * stride), cw * 2);
        for x in 0..cw {
            out[u_off + y * cw + x] = row[2 * x];
            out[v_off + y * cw + x] = row[2 * x + 1];
        }
    }
    out
}

// ── Windows: ANGLE device handoff + D3D11 NV12 readback ──────────────────────
#[cfg(target_os = "windows")]
mod windows_interop {
    use std::cell::Cell;
    use std::ffi::c_void;

    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

    // The host extracts ANGLE's ID3D11Device (eglQueryDeviceAttribEXT(EGL_D3D11_DEVICE
    // _ANGLE)) and sets it here, on the GL thread, before opening a decoder — so decode
    // lands on ANGLE's device and its NV12 texture imports as a plain same-device alias.
    thread_local! {
        static ANGLE_D3D11_DEVICE: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
    }

    /// Set (or clear, with null) the `ID3D11Device` the decoder should decode on.
    /// Pass `ID3D11Device::as_raw()`; call on the GL thread before opening a decoder.
    pub fn set_angle_d3d11_device(device: *mut c_void) {
        ANGLE_D3D11_DEVICE.with(|c| c.set(device));
    }

    pub(crate) fn angle_d3d11_device() -> Option<*mut c_void> {
        ANGLE_D3D11_DEVICE.with(|c| {
            let p = c.get();
            (!p.is_null()).then_some(p)
        })
    }

    /// Copy an NV12 D3D11 texture to a staging texture and pack it to tight I420 — the
    /// CPU fallback when the host cannot import the texture. `None` on any failure.
    pub(crate) unsafe fn readback_nv12_texture(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>> {
        let sdesc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        device.CreateTexture2D(&sdesc, None, Some(&mut staging)).ok()?;
        let staging: ID3D11Texture2D = staging?;
        context.CopyResource(&staging, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
        let out = super::pack_nv12_i420(m.pData as *const u8, m.RowPitch as usize, width, height);
        context.Unmap(&staging, 0);
        Some(out)
    }
}

#[cfg(target_os = "windows")]
pub(crate) use windows_interop::{angle_d3d11_device, readback_nv12_texture};
#[cfg(target_os = "windows")]
pub use windows_interop::set_angle_d3d11_device;

// ── macOS: CVPixelBuffer → I420 readback ─────────────────────────────────────
#[cfg(target_os = "macos")]
mod macos_interop {
    use std::ffi::c_void;

    use crate::CodecError;

    const CV_LOCK_READ_ONLY: u64 = 1;

    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        fn CVPixelBufferLockBaseAddress(pb: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(pb: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferGetWidth(pb: *mut c_void) -> usize;
        fn CVPixelBufferGetHeight(pb: *mut c_void) -> usize;
        fn CVPixelBufferGetBaseAddressOfPlane(pb: *mut c_void, plane: usize) -> *mut c_void;
        fn CVPixelBufferGetBytesPerRowOfPlane(pb: *mut c_void, plane: usize) -> usize;
    }

    /// Lock an NV12 `CVPixelBuffer` and pack it to tight I420 — the CPU fallback when
    /// the host cannot import its IOSurface.
    pub(crate) unsafe fn pixel_buffer_to_i420(image: *mut c_void, out: &mut Vec<u8>) -> Result<(), CodecError> {
        if CVPixelBufferLockBaseAddress(image, CV_LOCK_READ_ONLY) != 0 {
            return Err(CodecError::BadFrame);
        }
        let w = CVPixelBufferGetWidth(image);
        let h = CVPixelBufferGetHeight(image);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y_base = CVPixelBufferGetBaseAddressOfPlane(image, 0) as *const u8;
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
        let uv_base = CVPixelBufferGetBaseAddressOfPlane(image, 1) as *const u8;
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 1);
        out.clear();
        out.reserve(w * h + 2 * cw * ch);
        for row in 0..h {
            out.extend_from_slice(std::slice::from_raw_parts(y_base.add(row * y_stride), w));
        }
        let mut u = Vec::with_capacity(cw * ch);
        let mut v = Vec::with_capacity(cw * ch);
        for row in 0..ch {
            let src = std::slice::from_raw_parts(uv_base.add(row * uv_stride), cw * 2);
            for x in 0..cw {
                u.push(src[2 * x]);
                v.push(src[2 * x + 1]);
            }
        }
        out.extend_from_slice(&u);
        out.extend_from_slice(&v);
        CVPixelBufferUnlockBaseAddress(image, CV_LOCK_READ_ONLY);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos_interop::pixel_buffer_to_i420;
