//! CPU-only H.264 encoding of desktop frames.
//!
//! GLV1 packets contain one Annex B access unit. RGBA8 is converted to limited
//! range BT.601 YUV420, so the stream is lossy, including chroma subsampling.

use anyhow::{Context, Result, ensure};
use gliss_core::{Damage, Frame};
use openh264::{OpenH264API, nal_units};
use openh264_sys2::{
    API, CAMERA_VIDEO_REAL_TIME, CONSTANT_ID, DEBLOCKING_IDC_1, ENCODER_OPTION_BITRATE,
    ENCODER_OPTION_DATAFORMAT, ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_TRACE_LEVEL, ISVCEncoder,
    LOW_COMPLEXITY, PRO_BASELINE, RC_BITRATE_MODE, SBitrateInfo, SEncParamExt, SFrameBSInfo,
    SM_SINGLE_SLICE, SPATIAL_LAYER_0, SPATIAL_LAYER_ALL, SSourcePicture, UNSPECIFIED_BIT_RATE,
    WELS_LOG_QUIET, videoFormatI420, videoFrameTypeIDR, videoFrameTypeSkip,
};

/// Minimum and maximum H.264 QP bounds for OpenH264 rate control.
///
/// A 28 floor avoids spending encoder and decoder work on sub-perceptual fine
/// detail on 8 Mbit/s links. With `DEBLOCKING_IDC_1`, capping QP at 46 lets
/// rate control fit scrolling 1080p desktops within 1 Mbit/s WAN links without
/// triggering the QP 47..51 un-deblocked reference degradation cycle.
const MIN_QP: i32 = 28;
const MAX_QP: i32 = 46;

/// Planar I420 backing store updated directly in-place on dirty tile spans.
///
/// Uses the exact limited-range BT.601 integer math of OpenH264's
/// `YUVBuffer::read_rgba8`:
/// - `Y = ((66 * R + 129 * G + 25 * B) >> 8) + 16`
/// - `avg = (p00 + p01 + p10 + p11 + 2) >> 2`
/// - `U = ((-38 * R_avg - 74 * G_avg + 112 * B_avg) >> 8) + 128`
/// - `V = ((112 * R_avg - 94 * G_avg - 18 * B_avg) >> 8) + 128`
struct YuvPlanes {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    initialized: bool,
}

impl YuvPlanes {
    fn new(width: usize, height: usize) -> Self {
        let luma = width * height;
        let chroma = (width / 2) * (height / 2);
        Self {
            width,
            height,
            y: vec![0; luma],
            u: vec![0; chroma],
            v: vec![0; chroma],
            initialized: false,
        }
    }

    fn update_rgba8(&mut self, rgba: &[u8], damage: &Damage) {
        let w = self.width;
        let h = self.height;
        let force_full =
            !self.initialized || damage.is_full() || damage.dimensions() != (w as u16, h as u16);
        if force_full {
            let rect = Rect {
                x0: 0,
                y0: 0,
                bw: w,
                bh: h,
            };
            convert_rgba_rect_to_i420(rgba, w, rect, &mut self.y, &mut self.u, &mut self.v);
            self.initialized = true;
            return;
        }
        let y_plane = &mut self.y;
        let u_plane = &mut self.u;
        let v_plane = &mut self.v;
        damage.for_each_span(|x0, y0, bw, bh| {
            let rect = Rect { x0, y0, bw, bh };
            convert_rgba_rect_to_i420(rgba, w, rect, y_plane, u_plane, v_plane);
        });
    }

    #[cfg(test)]
    fn y(&self) -> &[u8] {
        &self.y
    }

    #[cfg(test)]
    fn u(&self) -> &[u8] {
        &self.u
    }

    #[cfg(test)]
    fn v(&self) -> &[u8] {
        &self.v
    }
}

#[inline]
fn convert_rgba_pair_scalar(
    row0: &[u8],
    row1: &[u8],
    y0: &mut [u8],
    y1: &mut [u8],
    u_out: &mut [u8],
    v_out: &mut [u8],
) {
    let pairs = u_out.len();
    for cx in 0..pairs {
        let px0 = cx * 8;
        let r00 = i32::from(row0[px0]);
        let g00 = i32::from(row0[px0 + 1]);
        let b00 = i32::from(row0[px0 + 2]);
        let r01 = i32::from(row0[px0 + 4]);
        let g01 = i32::from(row0[px0 + 5]);
        let b01 = i32::from(row0[px0 + 6]);

        let r10 = i32::from(row1[px0]);
        let g10 = i32::from(row1[px0 + 1]);
        let b10 = i32::from(row1[px0 + 2]);
        let r11 = i32::from(row1[px0 + 4]);
        let g11 = i32::from(row1[px0 + 5]);
        let b11 = i32::from(row1[px0 + 6]);

        let lx = cx * 2;
        y0[lx] = (((66 * r00 + 129 * g00 + 25 * b00) >> 8) + 16) as u8;
        y0[lx + 1] = (((66 * r01 + 129 * g01 + 25 * b01) >> 8) + 16) as u8;
        y1[lx] = (((66 * r10 + 129 * g10 + 25 * b10) >> 8) + 16) as u8;
        y1[lx + 1] = (((66 * r11 + 129 * g11 + 25 * b11) >> 8) + 16) as u8;

        let ra = (r00 + r01 + r10 + r11 + 2) >> 2;
        let ga = (g00 + g01 + g10 + g11 + 2) >> 2;
        let ba = (b00 + b01 + b10 + b11 + 2) >> 2;

        u_out[cx] = (((-38 * ra - 74 * ga + 112 * ba) >> 8) + 128) as u8;
        v_out[cx] = (((112 * ra - 94 * ga - 18 * ba) >> 8) + 128) as u8;
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x0: usize,
    y0: usize,
    bw: usize,
    bh: usize,
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn convert_rgba_rect_to_i420_avx2(
    rgba: &[u8],
    stride_w: usize,
    rect: Rect,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    use std::arch::x86_64::*;

    // Each RGBA quad produces two partial sums: 66R + 129G and 25B.
    // Centering the input lets maddubs take unsigned coefficients and signed
    // pixels. Neither partial sum saturates, and their sum fits in i16.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn luma(first: __m256i, second: __m256i) -> __m256i {
        let center = _mm256_set1_epi8(i8::MIN);
        let coefficients = _mm256_set1_epi32(i32::from_le_bytes([66, 129, 25, 0]));
        let first = _mm256_maddubs_epi16(coefficients, _mm256_xor_si256(first, center));
        let second = _mm256_maddubs_epi16(coefficients, _mm256_xor_si256(second, center));
        let sums = _mm256_hadd_epi16(first, second);
        // Undo centering, add the limited-range offset, then take the high byte.
        // 128 * (66 + 129 + 25) + (16 << 8) = 126 << 8.
        _mm256_srli_epi16::<8>(_mm256_add_epi16(sums, _mm256_set1_epi16(126 << 8)))
    }

    // Sum vertically adjacent bytes, then horizontally adjacent RGBA pixels.
    // The four u16 channel sums fit in ten bits. Round just once per 2x2 block.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn chroma_averages(top: __m256i, bottom: __m256i) -> __m256i {
        let ones = _mm256_set1_epi8(1);
        let lo = _mm256_maddubs_epi16(_mm256_unpacklo_epi8(top, bottom), ones);
        let hi = _mm256_maddubs_epi16(_mm256_unpackhi_epi8(top, bottom), ones);
        let even = _mm256_unpacklo_epi64(lo, hi);
        let odd = _mm256_unpackhi_epi64(lo, hi);
        let sums = _mm256_add_epi16(even, odd);
        _mm256_srli_epi16::<2>(_mm256_add_epi16(sums, _mm256_set1_epi16(2)))
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn chroma(first: __m256i, second: __m256i) -> __m128i {
        let averages = _mm256_packus_epi16(first, second);
        let u = _mm256_set1_epi32(i32::from_le_bytes([-38i8 as u8, -74i8 as u8, 112, 0]));
        let v = _mm256_set1_epi32(i32::from_le_bytes([112, -94i8 as u8, -18i8 as u8, 0]));
        let sums = _mm256_hadd_epi16(
            _mm256_maddubs_epi16(averages, u),
            _mm256_maddubs_epi16(averages, v),
        );
        // Adding 128 << 8 shifts each signed result into the u16 range.
        // Each lane now holds pairs of U samples followed by pairs of V.
        let biased = _mm256_add_epi16(sums, _mm256_set1_epi16(i16::MIN));
        let high_bytes = _mm256_broadcastsi128_si256(_mm_setr_epi8(
            1, 3, 5, 7, 9, 11, 13, 15, -1, -1, -1, -1, -1, -1, -1, -1,
        ));
        let uv = _mm256_shuffle_epi8(biased, high_bytes);
        _mm_unpacklo_epi16(
            _mm256_castsi256_si128(uv),
            _mm256_extracti128_si256::<1>(uv),
        )
    }

    let Rect { x0, y0, bw, bh } = rect;
    let simd_width = bw & !15;
    for y in (y0..y0 + bh).step_by(2) {
        let pos = y * stride_w + x0;
        let uv_pos = (y / 2) * (stride_w / 2) + x0 / 2;
        let top = &rgba[pos * 4..(pos + bw) * 4];
        let bottom = &rgba[(pos + stride_w) * 4..(pos + stride_w + bw) * 4];
        let (rows, next) = y_plane[pos..pos + stride_w + bw].split_at_mut(stride_w);
        let y_top = &mut rows[..bw];
        let y_bottom = &mut next[..bw];
        let u = &mut u_plane[uv_pos..uv_pos + bw / 2];
        let v = &mut v_plane[uv_pos..uv_pos + bw / 2];

        for x in (0..simd_width).step_by(16) {
            // SAFETY: each input row has bw * 4 bytes. x + 16 <= bw, covering
            // both unaligned 32-byte loads, sixteen Y and eight U/V stores.
            unsafe {
                let t = top.as_ptr().add(x * 4);
                let b = bottom.as_ptr().add(x * 4);
                let t0 = _mm256_loadu_si256(t.cast());
                let t1 = _mm256_loadu_si256(t.add(32).cast());
                let b0 = _mm256_loadu_si256(b.cast());
                let b1 = _mm256_loadu_si256(b.add(32).cast());
                let ys = _mm256_packus_epi16(luma(t0, t1), luma(b0, b1));
                // Each 128-bit lane holds groups of four top and bottom Y.
                let lo = _mm256_castsi256_si128(ys);
                let hi = _mm256_extracti128_si256::<1>(ys);
                _mm_storeu_si128(y_top.as_mut_ptr().add(x).cast(), _mm_unpacklo_epi32(lo, hi));
                _mm_storeu_si128(
                    y_bottom.as_mut_ptr().add(x).cast(),
                    _mm_unpackhi_epi32(lo, hi),
                );
                let uv = chroma(chroma_averages(t0, b0), chroma_averages(t1, b1));
                _mm_storel_epi64(u.as_mut_ptr().add(x / 2).cast(), uv);
                _mm_storel_epi64(v.as_mut_ptr().add(x / 2).cast(), _mm_srli_si128::<8>(uv));
            }
        }
        if simd_width < bw {
            convert_rgba_pair_scalar(
                &top[simd_width * 4..],
                &bottom[simd_width * 4..],
                &mut y_top[simd_width..],
                &mut y_bottom[simd_width..],
                &mut u[simd_width / 2..],
                &mut v[simd_width / 2..],
            );
        }
    }
}

fn convert_rgba_rect_to_i420(
    rgba: &[u8],
    stride_w: usize,
    rect: Rect,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    let Rect { x0, y0, bw, bh } = rect;
    if bw == 0 || bh == 0 {
        return;
    }
    let height = y_plane.len() / stride_w;
    assert!(
        x0.is_multiple_of(2)
            && y0.is_multiple_of(2)
            && bw.is_multiple_of(2)
            && bh.is_multiple_of(2)
            && stride_w.is_multiple_of(2)
            && height.is_multiple_of(2)
            && x0 + bw <= stride_w
            && y0 + bh <= height
            && rgba.len() >= height * stride_w * 4
            && u_plane.len() == (stride_w / 2) * (height / 2)
            && v_plane.len() == (stride_w / 2) * (height / 2)
    );

    #[cfg(target_arch = "x86_64")]
    if bw >= 16 && std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 support checked above and bounds asserted above.
        unsafe {
            convert_rgba_rect_to_i420_avx2(rgba, stride_w, rect, y_plane, u_plane, v_plane);
        }
        return;
    }

    let row_rgba = stride_w * 4;
    let uv_w = stride_w / 2;
    for cy in (y0 / 2)..((y0 + bh) / 2) {
        let r0_base = (cy * 2) * row_rgba + x0 * 4;
        let r1_base = (cy * 2 + 1) * row_rgba + x0 * 4;
        let y0_base = (cy * 2) * stride_w + x0;
        let y1_base = (cy * 2 + 1) * stride_w + x0;
        let uv_base = cy * uv_w + x0 / 2;
        let (y_row0, y_row1) = y_plane[y0_base..y1_base + bw].split_at_mut(stride_w);
        convert_rgba_pair_scalar(
            &rgba[r0_base..r0_base + bw * 4],
            &rgba[r1_base..r1_base + bw * 4],
            &mut y_row0[..bw],
            &mut y_row1[..bw],
            &mut u_plane[uv_base..uv_base + bw / 2],
            &mut v_plane[uv_base..uv_base + bw / 2],
        );
    }
}

/// Owned native OpenH264 `ISVCEncoder` configured at init time via `InitializeExt`.
struct RawEncoder {
    api: OpenH264API,
    ptr: *mut ISVCEncoder,
    initialized: bool,
}

// SAFETY: `RawEncoder` exclusively owns its single-threaded `ISVCEncoder`
// instance and never exposes shared aliases across threads.
unsafe impl Send for RawEncoder {}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr` was created by `WelsCreateSVCEncoder` and is destroyed once here.
            unsafe {
                if self.initialized
                    && !(*self.ptr).is_null()
                    && let Some(uninit) = (**self.ptr).Uninitialize
                {
                    uninit(self.ptr);
                }
                self.api.WelsDestroySVCEncoder(self.ptr);
            }
            self.ptr = std::ptr::null_mut();
        }
    }
}

impl RawEncoder {
    fn new(width: u16, height: u16, timing: VideoTiming, bitrate: u32) -> Result<Self> {
        let api = OpenH264API::from_source();
        let mut ptr: *mut ISVCEncoder = std::ptr::null_mut();
        // SAFETY: `WelsCreateSVCEncoder` writes a valid `ISVCEncoder` pointer on status 0.
        let create_status = unsafe { api.WelsCreateSVCEncoder(&raw mut ptr) };
        ensure!(
            create_status == 0 && !ptr.is_null(),
            "Create software OpenH264 encoder failed (status {create_status})"
        );
        let mut raw = Self {
            api,
            ptr,
            initialized: false,
        };
        // SAFETY: `raw.ptr` is non-null and valid until `Drop`.
        unsafe {
            ensure!(
                !(*raw.ptr).is_null(),
                "OpenH264 encoder returned null vtable pointer"
            );
            let vtbl = **raw.ptr;
            let mut trace = WELS_LOG_QUIET;
            let set_opt = vtbl
                .SetOption
                .context("OpenH264 missing SetOption vtable entry")?;
            let _ = set_opt(raw.ptr, ENCODER_OPTION_TRACE_LEVEL, (&raw mut trace).cast());

            let get_defaults = vtbl
                .GetDefaultParams
                .context("OpenH264 missing GetDefaultParams vtable entry")?;
            let mut param: SEncParamExt = std::mem::zeroed();
            let status = get_defaults(raw.ptr, &raw mut param);
            ensure!(status == 0, "OpenH264 GetDefaultParams failed ({status})");

            param.iUsageType = CAMERA_VIDEO_REAL_TIME;
            param.iPicWidth = i32::from(width);
            param.iPicHeight = i32::from(height);
            param.iTargetBitrate = bitrate as i32;
            param.iMaxBitrate = UNSPECIFIED_BIT_RATE as i32;
            param.fMaxFrameRate = timing.fps as f32;
            param.iRCMode = RC_BITRATE_MODE;
            param.iComplexityMode = LOW_COMPLEXITY;
            param.bEnableAdaptiveQuant = false;
            param.bEnableBackgroundDetection = false;
            param.bEnableSceneChangeDetect = false;
            param.bEnableDenoise = false;
            param.bEnableFrameSkip = false;
            param.bEnableLongTermReference = false;
            param.uiIntraPeriod = timing.intra_period;
            param.iNumRefFrame = 1;
            param.eSpsPpsIdStrategy = CONSTANT_ID;
            // Disabling the in-loop deblocking filter at init time avoids blurring
            // sharp desktop edges and cuts both software encode and decode CPU.
            param.iLoopFilterDisableIdc = DEBLOCKING_IDC_1;
            param.iMultipleThreadIdc = 1;
            param.iMinQp = MIN_QP;
            param.iMaxQp = MAX_QP;
            param.iEntropyCodingModeFlag = 0;
            param.iSpatialLayerNum = 1;
            param.sSpatialLayers[0].iVideoWidth = i32::from(width);
            param.sSpatialLayers[0].iVideoHeight = i32::from(height);
            param.sSpatialLayers[0].fFrameRate = timing.fps as f32;
            param.sSpatialLayers[0].iSpatialBitrate = bitrate as i32;
            param.sSpatialLayers[0].iMaxSpatialBitrate = bitrate as i32;
            param.sSpatialLayers[0].uiProfileIdc = PRO_BASELINE;
            param.sSpatialLayers[0].sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
            // Signal limited-range BT.601 to match our RGBA->I420 conversion.
            param.sSpatialLayers[0].bVideoSignalTypePresent = true;
            param.sSpatialLayers[0].uiVideoFormat = 5;
            param.sSpatialLayers[0].bFullRange = false;
            param.sSpatialLayers[0].bColorDescriptionPresent = true;
            param.sSpatialLayers[0].uiColorPrimaries = 6;
            param.sSpatialLayers[0].uiTransferCharacteristics = 6;
            param.sSpatialLayers[0].uiColorMatrix = 6;

            let init_ext = vtbl
                .InitializeExt
                .context("OpenH264 missing InitializeExt vtable entry")?;
            let status = init_ext(raw.ptr, &raw const param);
            ensure!(status == 0, "OpenH264 InitializeExt failed ({status})");
            raw.initialized = true;

            let mut format = videoFormatI420;
            let status = set_opt(raw.ptr, ENCODER_OPTION_DATAFORMAT, (&raw mut format).cast());
            ensure!(
                status == 0,
                "OpenH264 ENCODER_OPTION_DATAFORMAT failed ({status})"
            );
        }
        Ok(raw)
    }

    fn set_bitrate(&mut self, target: u32, applied: u32) -> Result<()> {
        // OpenH264's `WelsEncoderApplyBitRate` rescales spatial layer bitrates
        // through `f32`, which can round integers above 2^24 upward by 1-2 bps.
        // Ensuring the max bitrate ceiling covers that `f32` representation
        // prevents `WelsBitRateVerification` from rejecting valid targets up to
        // 50 Mbit/s.
        let max_target = target.max((target as f32) as u32);
        let order = if target < applied {
            [
                (ENCODER_OPTION_BITRATE, target as i32),
                (ENCODER_OPTION_MAX_BITRATE, max_target as i32),
            ]
        } else {
            [
                (ENCODER_OPTION_MAX_BITRATE, max_target as i32),
                (ENCODER_OPTION_BITRATE, target as i32),
            ]
        };
        // SAFETY: the encoder is initialized and both options take one `SBitrateInfo`.
        // Update both layer 0 (which controls `iMaxSpatialBitrate`) and `SPATIAL_LAYER_ALL`.
        unsafe {
            let set_opt = (**self.ptr)
                .SetOption
                .context("OpenH264 missing SetOption vtable entry")?;
            for layer in [SPATIAL_LAYER_0, SPATIAL_LAYER_ALL] {
                for (option, value) in order {
                    let mut info = SBitrateInfo {
                        iLayer: layer,
                        iBitrate: value,
                    };
                    let status = set_opt(self.ptr, option, (&raw mut info).cast());
                    ensure!(status == 0, "OpenH264 rejected bitrate {target}");
                }
            }
        }
        Ok(())
    }

    fn force_intra_frame(&mut self) -> Result<()> {
        // SAFETY: the encoder is initialized.
        unsafe {
            let force = (**self.ptr)
                .ForceIntraFrame
                .context("OpenH264 missing ForceIntraFrame vtable entry")?;
            let status = force(self.ptr, true);
            ensure!(status == 0, "OpenH264 ForceIntraFrame failed ({status})");
        }
        Ok(())
    }

    fn encode_frame(&mut self, yuv: &YuvPlanes, timestamp_ms: i64) -> Result<SFrameBSInfo> {
        let mut bs: SFrameBSInfo = unsafe { std::mem::zeroed() };
        let src = SSourcePicture {
            iColorFormat: videoFormatI420,
            iStride: [
                yuv.width as i32,
                (yuv.width / 2) as i32,
                (yuv.width / 2) as i32,
                0,
            ],
            pData: [
                yuv.y.as_ptr().cast_mut(),
                yuv.u.as_ptr().cast_mut(),
                yuv.v.as_ptr().cast_mut(),
                std::ptr::null_mut(),
            ],
            iPicWidth: yuv.width as i32,
            iPicHeight: yuv.height as i32,
            uiTimeStamp: timestamp_ms,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        };
        // SAFETY: `self.ptr` is initialized and `src` points into live `yuv` planes
        // for the full duration of `EncodeFrame`.
        let status = unsafe {
            let encode = (**self.ptr)
                .EncodeFrame
                .context("OpenH264 missing EncodeFrame vtable entry")?;
            encode(self.ptr, &raw const src, &raw mut bs)
        };
        ensure!(status == 0, "Encode H.264 frame failed ({status})");
        Ok(bs)
    }
}

pub const HEADER_LEN: usize = 24;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
pub const DEFAULT_BITRATE_BPS: u32 = 4_000_000;

/// Native rate-control rate and IDR cadence. Access units are timestamped at
/// the same rate. It describes encoder policy, not the cadence of frame capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTiming {
    fps: u32,
    intra_period: u32,
}

impl VideoTiming {
    pub fn new(fps: u32, intra_period: u32) -> Result<Self> {
        ensure!(
            (1..=60).contains(&fps),
            "OpenH264 fps must be within 1..=60"
        );
        ensure!(
            (1..=3600).contains(&intra_period),
            "H.264 intra period must be within 1..=3600 frames"
        );
        Ok(Self { fps, intra_period })
    }

    pub fn fps(self) -> u32 {
        self.fps
    }

    pub fn intra_period(self) -> u32 {
        self.intra_period
    }
}

/// One-thread software encoder; run encode on a bounded blocking worker.
pub struct VideoEncoder {
    encoder: RawEncoder,
    yuv: YuvPlanes,
    width: u16,
    height: u16,
    timing: VideoTiming,
    /// The bitrate the encoder was made with, and the most it may aim at.
    bitrate: u32,
    /// What it should aim at, and what the native encoder was last told.
    target: u32,
    applied: u32,
    encoded_frames: u64,
    force_keyframe: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

fn validate_dimensions(width: u16, height: u16) -> Result<()> {
    ensure!(
        width >= 16 && height >= 16,
        "H.264 dimensions must be at least 16x16"
    );
    ensure!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "H.264 YUV420 requires even width and height; no implicit crop or padding"
    );
    ensure!(
        width.max(height) <= 3840 && width.min(height) <= 2160,
        "OpenH264 dimensions exceed 3840x2160 / 2160x3840"
    );
    Ok(())
}

/// Whether a frame of this size can be encoded. A caller that checks first can
/// wait for a size that fits instead of failing on the encode.
pub fn encodable(width: u16, height: u16) -> bool {
    validate_dimensions(width, height).is_ok()
}

impl VideoEncoder {
    pub fn new(width: u16, height: u16, fps: u32) -> Result<Self> {
        Self::with_bitrate(width, height, fps, DEFAULT_BITRATE_BPS)
    }

    /// A keyframe every two seconds of frames at the given rate.
    pub fn with_bitrate(width: u16, height: u16, fps: u32, bitrate: u32) -> Result<Self> {
        ensure!(
            (1..=60).contains(&fps),
            "OpenH264 fps must be within 1..=60"
        );
        Self::with_timing(width, height, bitrate, VideoTiming::new(fps, fps * 2)?)
    }

    /// Change native timing without changing capture or input scheduling.
    pub fn with_timing(width: u16, height: u16, bitrate: u32, timing: VideoTiming) -> Result<Self> {
        validate_dimensions(width, height)?;
        ensure!(
            (1_000..=50_000_000).contains(&bitrate),
            "H.264 bitrate outside 1 kbit/s..50 Mbit/s"
        );
        Ok(Self {
            encoder: RawEncoder::new(width, height, timing, bitrate)?,
            yuv: YuvPlanes::new(usize::from(width), usize::from(height)),
            width,
            height,
            timing,
            bitrate,
            target: bitrate,
            applied: bitrate,
            encoded_frames: 0,
            force_keyframe: true,
            sps: Vec::new(),
            pps: Vec::new(),
        })
    }

    pub fn timing(&self) -> VideoTiming {
        self.timing
    }

    /// Aim at `bitrate` from the next frame on, for a link that carries less
    /// than the encoder was made for. More than that is taken as that.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.target = bitrate.clamp(1_000, self.bitrate);
    }

    /// Tell the native encoder about a target that moved by more than a tenth.
    /// Smaller steps would only unsettle its rate control. It exists once it
    /// has encoded a frame.
    fn apply_bitrate(&mut self) -> Result<()> {
        if self.encoded_frames == 0 || self.target.abs_diff(self.applied) <= self.applied / 10 {
            return Ok(());
        }
        self.encoder.set_bitrate(self.target, self.applied)?;
        self.applied = self.target;
        Ok(())
    }

    /// Request an independently decodable IDR access unit on the next encode.
    pub fn reset(&mut self) {
        self.force_keyframe = true;
    }

    /// Whether a frame has already been staged in the encoder's planar YUV buffer.
    pub fn has_prepared(&self) -> bool {
        self.yuv.initialized
    }

    /// Encode one complete RGBA frame; size changes rebuild native state.
    pub fn encode(&mut self, frame: &Frame, sequence: u32, input_id: u32) -> Result<Vec<u8>> {
        let full = Damage::full(frame.width, frame.height);
        self.encode_damaged(frame, &full, sequence, input_id)
    }

    /// Stage one RGBA frame into the encoder's planar I420 store, updating only
    /// the tiles marked in `damage`.
    ///
    /// Callers may drop `frame` immediately after this returns and before
    /// calling [`Self::encode_prepared`] so upstream buffers can be reused while
    /// OpenH264 encodes.
    pub fn prepare_damaged(&mut self, frame: &Frame, damage: &Damage) -> Result<()> {
        ensure!(frame.is_valid(), "Invalid RGBA frame length or dimensions");
        validate_dimensions(frame.width, frame.height)?;
        if (frame.width, frame.height) != (self.width, self.height) {
            // Construct first so a rejected resize leaves the old encoder intact.
            // The new one codes its first frame, a keyframe, at the bitrate it
            // is made with: the native encoder takes another target only once
            // it has coded a frame, and one made for less cannot be raised.
            let target = self.target;
            *self = Self::with_timing(frame.width, frame.height, self.bitrate, self.timing)?;
            self.target = target;
        }
        let effective_damage = if self.encoded_frames == 0 {
            Damage::full(frame.width, frame.height)
        } else {
            *damage
        };
        self.yuv.update_rgba8(&frame.pixels, &effective_damage);
        Ok(())
    }

    /// Encode the frame currently staged by [`Self::prepare_damaged`].
    pub fn encode_prepared(&mut self, sequence: u32, input_id: u32) -> Result<Vec<u8>> {
        ensure!(self.yuv.initialized, "No prepared frame to encode");
        self.apply_bitrate()?;
        if self.force_keyframe && self.encoded_frames > 0 {
            self.encoder.force_intra_frame()?;
        }
        let timestamp_ms =
            (self.encoded_frames.saturating_mul(1000) / u64::from(self.timing.fps)) as i64;
        let bs = self.encoder.encode_frame(&self.yuv, timestamp_ms)?;
        ensure!(
            bs.eFrameType != videoFrameTypeSkip,
            "Unexpected skipped encoder output"
        );
        let keyframe = bs.eFrameType == videoFrameTypeIDR;
        ensure!(
            !self.force_keyframe || keyframe,
            "Encoder did not provide the requested IDR frame"
        );

        let layer_count = (bs.iLayerNum.max(0) as usize).min(bs.sLayerInfo.len());
        let mut raw_len = 0usize;
        let mut has_sps = false;
        let mut has_pps = false;
        for layer in &bs.sLayerInfo[..layer_count] {
            let nal_count = layer.iNalCount.max(0) as usize;
            if nal_count == 0 || layer.pBsBuf.is_null() || layer.pNalLengthInByte.is_null() {
                continue;
            }
            let mut offset = 0usize;
            for nal_index in 0..nal_count {
                // SAFETY: OpenH264 populates `pNalLengthInByte[..iNalCount]` and
                // contiguous NAL bytes in `pBsBuf` valid until the next encode call.
                let nal = unsafe {
                    let len = (*layer.pNalLengthInByte.add(nal_index)).max(0) as usize;
                    let slice = std::slice::from_raw_parts(layer.pBsBuf.add(offset), len);
                    offset += len;
                    slice
                };
                raw_len += nal.len();
                match nal_type(nal) {
                    Some(7) => {
                        if self.sps.as_slice() != nal {
                            self.sps.clear();
                            self.sps.extend_from_slice(nal);
                        }
                        has_sps = true;
                    }
                    Some(8) => {
                        if self.pps.as_slice() != nal {
                            self.pps.clear();
                            self.pps.extend_from_slice(nal);
                        }
                        has_pps = true;
                    }
                    _ => {}
                }
            }
        }
        ensure!(raw_len > 0, "Encoder unexpectedly skipped a frame");
        let prepend_params = keyframe && (!has_sps || !has_pps);
        if prepend_params {
            ensure!(
                !self.sps.is_empty() && !self.pps.is_empty(),
                "IDR lacks SPS/PPS"
            );
        }
        let payload_len = if prepend_params {
            self.sps.len() + self.pps.len() + raw_len
        } else {
            raw_len
        };
        ensure!(
            payload_len <= MAX_PAYLOAD,
            "Encoded access unit exceeds packet limit"
        );
        let mut packet = Vec::with_capacity(HEADER_LEN + payload_len);
        packet.extend_from_slice(b"GLV1");
        packet.extend_from_slice(&sequence.to_le_bytes());
        packet.extend_from_slice(&input_id.to_le_bytes());
        packet.extend_from_slice(&self.width.to_le_bytes());
        packet.extend_from_slice(&self.height.to_le_bytes());
        packet.extend_from_slice(&0_u16.to_le_bytes());
        packet.extend_from_slice(&u16::from(keyframe).to_le_bytes());
        packet.extend_from_slice(&(payload_len as u32).to_le_bytes());
        if prepend_params {
            packet.extend_from_slice(&self.sps);
            packet.extend_from_slice(&self.pps);
        }
        for layer in &bs.sLayerInfo[..layer_count] {
            let nal_count = layer.iNalCount.max(0) as usize;
            if nal_count == 0 || layer.pBsBuf.is_null() || layer.pNalLengthInByte.is_null() {
                continue;
            }
            let mut layer_len = 0usize;
            for nal_index in 0..nal_count {
                // SAFETY: same as above; all NALs of a layer are contiguous in `pBsBuf`.
                layer_len += unsafe { (*layer.pNalLengthInByte.add(nal_index)).max(0) as usize };
            }
            if layer_len > 0 {
                // SAFETY: `pBsBuf` holds `layer_len` contiguous encoded bytes.
                let bytes = unsafe { std::slice::from_raw_parts(layer.pBsBuf, layer_len) };
                packet.extend_from_slice(bytes);
            }
        }
        self.encoded_frames += 1;
        self.force_keyframe = false;
        Ok(packet)
    }

    /// Encode one RGBA frame, updating only the tiles marked in `damage`.
    pub fn encode_damaged(
        &mut self,
        frame: &Frame,
        damage: &Damage,
        sequence: u32,
        input_id: u32,
    ) -> Result<Vec<u8>> {
        self.prepare_damaged(frame, damage)?;
        self.encode_prepared(sequence, input_id)
    }
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    let prefix = if nal.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };
    nal.get(prefix).map(|byte| byte & 31)
}

/// Validated packet framing, with borrowed native H.264 bytes.
#[derive(Debug)]
pub struct VideoPacket<'a> {
    pub sequence: u32,
    pub input_id: u32,
    pub width: u16,
    pub height: u16,
    pub keyframe: bool,
    pub payload: &'a [u8],
}

/// Checks framing and IDR/header declarations; not a full H.264 validator.
pub fn parse_packet(packet: &[u8]) -> Result<VideoPacket<'_>> {
    ensure!(
        packet.len() >= HEADER_LEN && packet.len() <= HEADER_LEN + MAX_PAYLOAD,
        "Invalid GLV1 packet size"
    );
    ensure!(&packet[..4] == b"GLV1", "Unknown video wire version");
    let u16_at = |offset| u16::from_le_bytes([packet[offset], packet[offset + 1]]);
    let u32_at = |offset| u32::from_le_bytes(packet[offset..offset + 4].try_into().unwrap());
    let width = u16_at(12);
    let height = u16_at(14);
    validate_dimensions(width, height)?;
    let flags = u16_at(18);
    ensure!(
        u16_at(16) == 0 && flags <= 1,
        "Invalid video flags/reserved field"
    );
    ensure!(
        u32_at(20) as usize == packet.len() - HEADER_LEN,
        "Video payload length mismatch"
    );
    let payload = &packet[HEADER_LEN..];
    ensure!(nal_type(payload).is_some(), "Video payload is not Annex B");
    let mut idr = false;
    let mut sps = false;
    let mut pps = false;
    let mut slice = false;
    for nal in nal_units(payload) {
        match nal_type(nal) {
            Some(5) => {
                idr = true;
                slice = true;
            }
            Some(1) => slice = true,
            Some(7) => sps = true,
            Some(8) => pps = true,
            _ => {}
        }
    }
    ensure!(
        slice && idr == (flags == 1) && (!idr || (sps && pps)),
        "Invalid IDR or parameter set declaration"
    );
    Ok(VideoPacket {
        sequence: u32_at(4),
        input_id: u32_at(8),
        width,
        height,
        keyframe: flags == 1,
        payload,
    })
}

#[cfg(test)]
mod fuzz;
#[cfg(test)]
mod tests;
