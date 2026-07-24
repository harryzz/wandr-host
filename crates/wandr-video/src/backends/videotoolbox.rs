//! macOS hardware video decode via **VideoToolbox** (`VTDecompressionSession`) —
//! the Apple peer of DXVA2 (Windows), VAAPI (Linux) and MediaCodec (Android).
//! VideoToolbox is a system framework, so this links no vendored/system codec
//! library; it registers HW-first (priority 10) and declines via
//! `VTIsHardwareDecodeSupported` so the software backends stay the fallback.
//!
//! ‼️ TWO SHAPE DIFFERENCES from the DXVA/VAAPI backends, both because VT is a
//! higher-level API:
//!   * BITSTREAM IS AVCC, not Annex-B. Each slice NAL is prefixed with a 4-byte
//!     big-endian length (NALUnitHeaderLength = 4); the SPS/PPS go into the
//!     `CMFormatDescription` separately, NOT into the sample buffer. This is the
//!     exact opposite of the DXVA fix (which needed Annex-B start codes).
//!   * VT OWNS THE DPB. With `kVTDecodeFrame_EnableTemporalProcessing` the decoder
//!     reorders internally and the output callback fires in DISPLAY order — so
//!     there is no cros-codecs DPB to hand-drive here (the bulk of d3d11.rs).
//!
//! First cut is the CPU-readback lane: the decoded `CVPixelBuffer` (NV12) is
//! copied to tightly-packed I420, exactly like the software backends. The
//! IOSurface → GL zero-copy import (the peer of the dma-buf / D3D11 paths in
//! `video_gl.rs`) is a follow-up; this proves the decode is correct first.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr::{null, null_mut};

use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{kCFAllocatorDefault, CFAllocatorRef};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;

use crate::{
    BackendKind, Codec, CodecBackend, CodecError, Decoder, DecoderParams, Encoder, EncoderParams,
    Frame, I420Ref,
};

// ── FFI: opaque handles ──────────────────────────────────────────────────────
type CMFormatDescriptionRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CVImageBufferRef = *mut c_void; // == CVPixelBufferRef for our output
type VTDecompressionSessionRef = *mut c_void;
type OSStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}
const CM_TIME_FLAG_VALID: u32 = 1;
impl CMTime {
    fn us(v: i64) -> Self {
        CMTime { value: v, timescale: 1_000_000, flags: CM_TIME_FLAG_VALID, epoch: 0 }
    }
    fn invalid() -> Self {
        CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_ts: CMTime,
    decode_ts: CMTime,
}

type VtOutputCallback = extern "C" fn(
    *mut c_void, // decompressionOutputRefCon
    *mut c_void, // sourceFrameRefCon
    OSStatus,
    u32,             // infoFlags
    CVImageBufferRef, // imageBuffer (may be null on error)
    CMTime,          // presentationTimeStamp
    CMTime,          // presentationDuration
);

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    callback: VtOutputCallback,
    refcon: *mut c_void,
}

// FourCCs + flags.
const PIXEL_FORMAT_420V: i32 = 0x3432_3076; // '420v' NV12, video-range
const CODEC_TYPE_H264: i32 = 0x6176_6331; // 'avc1'
const CODEC_TYPE_HEVC: i32 = 0x6876_6331; // 'hvc1'
const CV_LOCK_READ_ONLY: u64 = 1;
const BLOCK_BUFFER_ASSURE_MEMORY_NOW: u32 = 1 << 0;

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: c_int,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: c_int,
        extensions: CFDictionaryRef,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVImageBufferRef, lock_flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVImageBufferRef, unlock_flags: u64) -> i32;
    fn CVPixelBufferGetWidth(pixel_buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(pixel_buffer: CVImageBufferRef, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer: CVImageBufferRef, plane: usize) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: VTDecompressionSessionRef) -> OSStatus;
    fn VTDecompressionSessionFinishDelayedFrames(session: VTDecompressionSessionRef) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    fn VTIsHardwareDecodeSupported(codec_type: i32) -> u8; // Boolean
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

// ── backend descriptor ───────────────────────────────────────────────────────
pub struct VideoToolboxBackend;

impl CodecBackend for VideoToolboxBackend {
    fn name(&self) -> &'static str {
        "videotoolbox"
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Hardware
    }
    fn priority(&self) -> u32 {
        10
    }
    fn supports_decode(&self, codec: Codec) -> bool {
        // The authoritative probe — false on a machine/OS without a HW block for
        // this codec, so the registry falls through to software.
        let ct = match codec {
            Codec::H264 => CODEC_TYPE_H264,
            Codec::H265 => CODEC_TYPE_HEVC,
            _ => return false,
        };
        unsafe { VTIsHardwareDecodeSupported(ct) != 0 }
    }
    fn supports_encode(&self, _codec: Codec) -> bool {
        false
    }
    fn open_decoder(&self, p: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        if !self.supports_decode(p.codec) {
            return Err(CodecError::Unsupported);
        }
        Ok(Box::new(VtDecoder::new(p.codec)))
    }
    fn open_encoder(&self, _p: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
        Err(CodecError::Unsupported)
    }
}

// ── decoder ──────────────────────────────────────────────────────────────────
struct DecodedFrame {
    buf: Vec<u8>, // tightly-packed I420
    w: u32,
    h: u32,
    pts_us: i64,
}

/// The output callback writes decoded frames here. Boxed so its address is stable
/// (it is the session's `refcon`), and only ever touched from the decode thread
/// (synchronous: `WaitForAsynchronousFrames` blocks that thread until the
/// callbacks for a decode have run).
struct FrameSink {
    frames: std::sync::Mutex<VecDeque<DecodedFrame>>,
}

pub struct VtDecoder {
    codec: Codec,
    session: VTDecompressionSessionRef,
    format: CMFormatDescriptionRef,
    vps: Option<Vec<u8>>, // HEVC only
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    sink: Box<FrameSink>,
    current: Option<DecodedFrame>,
}

// SAFETY: single-threaded use, like every other backend. The raw VT/CM handles
// and the `Box<FrameSink>` are owned by one `VtDecoder`; the host drives it from
// a wasmtime store's thread. VT's async callback runs while this thread is parked
// in `WaitForAsynchronousFrames`, so there is no concurrent access to the sink.
unsafe impl Send for VtDecoder {}

extern "C" fn output_callback(
    refcon: *mut c_void,
    _src: *mut c_void,
    status: OSStatus,
    _info: u32,
    image: CVImageBufferRef,
    pts: CMTime,
    _dur: CMTime,
) {
    if status != 0 || image.is_null() {
        return;
    }
    let sink = unsafe { &mut *(refcon as *mut FrameSink) };
    unsafe {
        if CVPixelBufferLockBaseAddress(image, CV_LOCK_READ_ONLY) != 0 {
            return;
        }
        let w = CVPixelBufferGetWidth(image);
        let h = CVPixelBufferGetHeight(image);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y_base = CVPixelBufferGetBaseAddressOfPlane(image, 0) as *const u8;
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
        let uv_base = CVPixelBufferGetBaseAddressOfPlane(image, 1) as *const u8;
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 1);

        let mut buf = Vec::with_capacity(w * h + 2 * cw * ch);
        for row in 0..h {
            let src = std::slice::from_raw_parts(y_base.add(row * y_stride), w);
            buf.extend_from_slice(src);
        }
        // NV12 CbCr plane -> planar U (Cb) then V (Cr).
        let mut u = Vec::with_capacity(cw * ch);
        let mut v = Vec::with_capacity(cw * ch);
        for row in 0..ch {
            let src = std::slice::from_raw_parts(uv_base.add(row * uv_stride), cw * 2);
            for x in 0..cw {
                u.push(src[2 * x]);
                v.push(src[2 * x + 1]);
            }
        }
        buf.extend_from_slice(&u);
        buf.extend_from_slice(&v);
        CVPixelBufferUnlockBaseAddress(image, CV_LOCK_READ_ONLY);

        let pts_us = if pts.timescale != 0 {
            pts.value * 1_000_000 / pts.timescale as i64
        } else {
            0
        };
        sink.frames.lock().unwrap().push_back(DecodedFrame { buf, w: w as u32, h: h as u32, pts_us });
    }
}

impl VtDecoder {
    fn new(codec: Codec) -> Self {
        VtDecoder {
            codec,
            session: null_mut(),
            format: null_mut(),
            vps: None,
            sps: None,
            pps: None,
            sink: Box::new(FrameSink { frames: std::sync::Mutex::new(VecDeque::new()) }),
            current: None,
        }
    }

    unsafe fn teardown(&mut self) {
        if !self.session.is_null() {
            VTDecompressionSessionInvalidate(self.session);
            CFRelease(self.session as *const c_void);
            self.session = null_mut();
        }
        if !self.format.is_null() {
            CFRelease(self.format as *const c_void);
            self.format = null_mut();
        }
    }

    /// Build the `CMFormatDescription` from the buffered parameter sets and create
    /// the session. H.264 = SPS+PPS; HEVC = VPS+SPS+PPS. Both use a 4-byte length
    /// prefix (AVCC / hvcC).
    unsafe fn build_session(&mut self) -> Result<(), CodecError> {
        let mut fmt: CMFormatDescriptionRef = null_mut();
        let st = match self.codec {
            Codec::H264 => {
                let (sps, pps) = (self.sps.as_ref().unwrap(), self.pps.as_ref().unwrap());
                let ptrs: [*const u8; 2] = [sps.as_ptr(), pps.as_ptr()];
                let sizes: [usize; 2] = [sps.len(), pps.len()];
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    kCFAllocatorDefault, 2, ptrs.as_ptr(), sizes.as_ptr(), 4, &mut fmt,
                )
            }
            Codec::H265 => {
                let vps = self.vps.as_ref().unwrap();
                let (sps, pps) = (self.sps.as_ref().unwrap(), self.pps.as_ref().unwrap());
                let ptrs: [*const u8; 3] = [vps.as_ptr(), sps.as_ptr(), pps.as_ptr()];
                let sizes: [usize; 3] = [vps.len(), sps.len(), pps.len()];
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    kCFAllocatorDefault, 3, ptrs.as_ptr(), sizes.as_ptr(), 4, null(), &mut fmt,
                )
            }
            _ => return Err(CodecError::Unsupported),
        };
        if st != 0 || fmt.is_null() {
            return Err(CodecError::InitFailed);
        }
        self.format = fmt;

        // Ask for NV12 (video-range) output so the readback assumes two planes.
        let fmt_key = CFString::wrap_under_get_rule(kCVPixelBufferPixelFormatTypeKey);
        let attrs = CFDictionary::from_CFType_pairs(&[(
            fmt_key.as_CFType(),
            CFNumber::from(PIXEL_FORMAT_420V).as_CFType(),
        )]);

        let cb = VTDecompressionOutputCallbackRecord {
            callback: output_callback,
            refcon: &mut *self.sink as *mut FrameSink as *mut c_void,
        };
        let mut sess: VTDecompressionSessionRef = null_mut();
        let st = VTDecompressionSessionCreate(
            kCFAllocatorDefault,
            fmt,
            null(), // no decoder spec: HW already gated by supports_decode
            attrs.as_concrete_TypeRef(),
            &cb,
            &mut sess,
        );
        if st != 0 || sess.is_null() {
            log::warn!("wandr-video: VTDecompressionSessionCreate failed ({st})");
            return Err(CodecError::InitFailed);
        }
        self.session = sess;
        Ok(())
    }

    unsafe fn decode_avcc(&mut self, avcc: &[u8], pts_us: i64) -> Result<(), CodecError> {
        // Block buffer that OWNS a copy (VT keeps it past this call for reorder).
        let mut bbuf: CMBlockBufferRef = null_mut();
        let st = CMBlockBufferCreateWithMemoryBlock(
            kCFAllocatorDefault,
            null_mut(),
            avcc.len(),
            kCFAllocatorDefault,
            null(),
            0,
            avcc.len(),
            BLOCK_BUFFER_ASSURE_MEMORY_NOW,
            &mut bbuf,
        );
        if st != 0 || bbuf.is_null() {
            return Err(CodecError::BadFrame);
        }
        let st = CMBlockBufferReplaceDataBytes(avcc.as_ptr() as *const c_void, bbuf, 0, avcc.len());
        if st != 0 {
            CFRelease(bbuf as *const c_void);
            return Err(CodecError::BadFrame);
        }

        let timing = CMSampleTimingInfo {
            duration: CMTime::invalid(),
            presentation_ts: CMTime::us(pts_us),
            decode_ts: CMTime::invalid(),
        };
        let size = avcc.len();
        let mut sbuf: CMSampleBufferRef = null_mut();
        let st = CMSampleBufferCreateReady(
            kCFAllocatorDefault,
            bbuf,
            self.format,
            1,
            1,
            &timing,
            1,
            &size,
            &mut sbuf,
        );
        CFRelease(bbuf as *const c_void); // sample buffer retains it
        if st != 0 || sbuf.is_null() {
            return Err(CodecError::BadFrame);
        }

        // Synchronous decode: the callback fires inline, in DECODE order.
        // VideoToolbox does NOT reorder to display order; the host does that by
        // sorting on the PTS each frame carries (video_desktop::queue_decoded).
        let flags = 0u32;
        let mut info: u32 = 0;
        let st = VTDecompressionSessionDecodeFrame(self.session, sbuf, flags, null_mut(), &mut info);
        CFRelease(sbuf as *const c_void);
        if st != 0 {
            log::warn!("wandr-video: VTDecompressionSessionDecodeFrame failed ({st})");
            return Err(CodecError::BadFrame);
        }
        Ok(())
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        unsafe { self.teardown() }
    }
}

/// Split an Annex-B buffer into NAL bodies (start codes removed). A leading zero
/// of a 4-byte start code is left as a harmless trailing byte on the previous
/// NAL — VT ignores rbsp trailing bytes.
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut bodies = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            bodies.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(bodies.len());
    for k in 0..bodies.len() {
        let s = bodies[k];
        let e = if k + 1 < bodies.len() { bodies[k + 1] - 3 } else { data.len() };
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

impl Decoder for VtDecoder {
    fn decode(&mut self, chunk: crate::Chunk<'_>) -> Result<(), CodecError> {
        let hevc = self.codec == Codec::H265;
        let mut avcc: Vec<u8> = Vec::new();
        for nal in split_nals(chunk.data) {
            if nal.is_empty() {
                continue;
            }
            // Parameter sets go into the format description; VCL slices become AVCC
            // (4-byte big-endian length prefix + NAL). H.264 NAL header is 1 byte
            // (type = low 5 bits); HEVC is 2 bytes (type = bits 1..6 of byte 0).
            let is_slice = if hevc {
                match (nal[0] >> 1) & 0x3f {
                    32 => {
                        self.vps = Some(nal.to_vec());
                        false
                    }
                    33 => {
                        self.sps = Some(nal.to_vec());
                        false
                    }
                    34 => {
                        self.pps = Some(nal.to_vec());
                        false
                    }
                    0..=31 => true, // VCL
                    _ => false,
                }
            } else {
                match nal[0] & 0x1f {
                    7 => {
                        self.sps = Some(nal.to_vec());
                        false
                    }
                    8 => {
                        self.pps = Some(nal.to_vec());
                        false
                    }
                    1 | 5 => true,
                    _ => false,
                }
            };
            if is_slice {
                avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                avcc.extend_from_slice(nal);
            }
        }

        if self.session.is_null() {
            let have = if hevc {
                self.vps.is_some() && self.sps.is_some() && self.pps.is_some()
            } else {
                self.sps.is_some() && self.pps.is_some()
            };
            if have {
                unsafe { self.build_session()? };
            }
        }
        if self.session.is_null() || avcc.is_empty() {
            return Ok(()); // config-only AU, or no session yet
        }
        unsafe { self.decode_avcc(&avcc, chunk.timestamp_us) }
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        if !self.session.is_null() {
            unsafe {
                VTDecompressionSessionFinishDelayedFrames(self.session);
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            }
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        unsafe { self.teardown() };
        self.vps = None;
        self.sps = None;
        self.pps = None;
        self.sink.frames.lock().unwrap().clear();
        self.current = None;
        Ok(())
    }

    fn next_frame(&mut self) -> Option<Frame<'_>> {
        // VT emits in display order (temporal processing), so FIFO is display order.
        self.current = self.sink.frames.lock().unwrap().pop_front();
        let f = self.current.as_ref()?;
        let (w, h) = (f.w, f.h);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let yl = (w * h) as usize;
        let cl = (cw * ch) as usize;
        Some(Frame::cpu(I420Ref {
            y: &f.buf[..yl],
            y_stride: w,
            u: &f.buf[yl..yl + cl],
            u_stride: cw,
            v: &f.buf[yl + cl..yl + 2 * cl],
            v_stride: cw,
            width: w,
            height: h,
            timestamp_us: f.pts_us,
        }))
    }
}
