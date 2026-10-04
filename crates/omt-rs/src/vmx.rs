//! FFI bindings to the VMX codec (vendored at `vendor/libvmx`, MIT license).
//! VMX is OMT's intra-frame video codec: 4:2:2(:4 with alpha), with NEON and
//! AVX2 paths. Compiled from source for every target by build.rs.

use std::os::raw::{c_int, c_uchar, c_void};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VmxSize {
    pub width: c_int,
    pub height: c_int,
}

/// VMX_PROFILE values from vmxcodec.h.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxProfile {
    Default = 0,
    Lq = 33,
    Sq = 66,
    Hq = 99,
    OmtLq = 133,
    OmtSq = 166,
    OmtHq = 199,
}

/// VMX_COLORSPACE values from vmxcodec.h.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxColorSpace {
    Undefined = 0,
    Bt601 = 601,
    Bt709 = 709,
}

/// VMX_ERR values from vmxcodec.h.
pub const VMX_ERR_OK: c_int = 0;

unsafe extern "C" {
    fn VMX_Create(dimensions: VmxSize, profile: c_int, color_space: c_int) -> *mut c_void;
    fn VMX_Destroy(instance: *mut c_void);
    fn VMX_LoadFrom(instance: *mut c_void, data: *mut c_uchar, data_len: c_int) -> c_int;
    fn VMX_SaveTo(instance: *mut c_void, dst: *mut c_uchar, max_len: c_int) -> c_int;
    fn VMX_DecodeBGRA(instance: *mut c_void, dst: *mut c_uchar, stride: c_int) -> c_int;
    fn VMX_DecodeUYVY(instance: *mut c_void, dst: *mut c_uchar, stride: c_int) -> c_int;
    fn VMX_DecodeUYVA(instance: *mut c_void, dst: *mut c_uchar, stride: c_int) -> c_int;
    fn VMX_EncodeBGRA(
        instance: *mut c_void,
        src: *mut c_uchar,
        stride: c_int,
        interlaced: c_int,
    ) -> c_int;
    fn VMX_EncodeUYVY(
        instance: *mut c_void,
        src: *mut c_uchar,
        stride: c_int,
        interlaced: c_int,
    ) -> c_int;
    fn VMX_EncodeUYVA(
        instance: *mut c_void,
        src: *mut c_uchar,
        stride: c_int,
        interlaced: c_int,
    ) -> c_int;
    fn VMX_SetQuality(instance: *mut c_void, q: c_int);
    fn VMX_SetThreads(instance: *mut c_void, num_threads: c_int);
}

/// Safe wrapper owning a VMX codec instance.
pub struct VmxInstance {
    ptr: *mut c_void,
}

// The VMX instance is only touched through &mut self.
unsafe impl Send for VmxInstance {}

impl VmxInstance {
    pub fn new(
        width: i32,
        height: i32,
        profile: VmxProfile,
        color_space: VmxColorSpace,
    ) -> Option<Self> {
        let ptr = unsafe {
            VMX_Create(
                VmxSize { width, height },
                profile as c_int,
                color_space as c_int,
            )
        };
        if ptr.is_null() {
            return None;
        }
        let mut inst = Self { ptr };
        // VMX hardcodes its thread count to 2 at ≤1080p (only 4K→8 and 8K→16
        // get more; see VMX_BITRATE_TABLE), leaving most of a modern device's
        // cores idle and capping real-time 1080p at well under 30 fps.
        //
        // But an application commonly runs a decoder and an encoder at once
        // (a receive-process-send pipeline), so giving each all cores
        // oversubscribes (measured: encode 8.5 ms alone → 45 ms when both use 8
        // threads on 8 cores). Use half the cores per instance so a concurrent
        // decode+encode pair sums to the core count; a lone encoder still gets
        // enough threads to stay well under a 33 ms/30 fps budget. Override
        // with `set_threads`.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let threads = (cores / 2).clamp(2, 8) as i32;
        inst.set_threads(threads);
        log::info!("VMX {width}x{height} instance using {threads} threads ({cores} cores)");
        Some(inst)
    }

    pub fn set_quality(&mut self, q: i32) {
        unsafe { VMX_SetQuality(self.ptr, q) }
    }

    pub fn set_threads(&mut self, n: i32) {
        unsafe { VMX_SetThreads(self.ptr, n) }
    }

    /// Loads an encoded VMX frame into the instance for decoding.
    pub fn load_from(&mut self, data: &mut [u8]) -> Result<(), i32> {
        let err = unsafe { VMX_LoadFrom(self.ptr, data.as_mut_ptr(), data.len() as c_int) };
        if err == VMX_ERR_OK { Ok(()) } else { Err(err) }
    }

    /// Writes the encoded frame to `dst`, returning the number of bytes written.
    pub fn save_to(&mut self, dst: &mut [u8]) -> i32 {
        unsafe { VMX_SaveTo(self.ptr, dst.as_mut_ptr(), dst.len() as c_int) }
    }

    pub fn decode_bgra(&mut self, dst: &mut [u8], stride: i32) -> Result<(), i32> {
        let err = unsafe { VMX_DecodeBGRA(self.ptr, dst.as_mut_ptr(), stride) };
        if err == VMX_ERR_OK { Ok(()) } else { Err(err) }
    }

    pub fn decode_uyvy(&mut self, dst: &mut [u8], stride: i32) -> Result<(), i32> {
        let err = unsafe { VMX_DecodeUYVY(self.ptr, dst.as_mut_ptr(), stride) };
        if err == VMX_ERR_OK { Ok(()) } else { Err(err) }
    }

    pub fn decode_uyva(&mut self, dst: &mut [u8], stride: i32) -> Result<(), i32> {
        let err = unsafe { VMX_DecodeUYVA(self.ptr, dst.as_mut_ptr(), stride) };
        if err == VMX_ERR_OK { Ok(()) } else { Err(err) }
    }

    pub fn encode_bgra(&mut self, src: &mut [u8], stride: i32) -> Result<(), i32> {
        let err = unsafe { VMX_EncodeBGRA(self.ptr, src.as_mut_ptr(), stride, 0) };
        if err == VMX_ERR_OK { Ok(()) } else { Err(err) }
    }

    pub fn encode_uyvy(&mut self, src: &mut [u8], stride: i32) -> Result<(), i32> {
        let err = unsafe { VMX_EncodeUYVY(self.ptr, src.as_mut_ptr(), stride, 0) };
        if err == VMX_ERR_OK { Ok(()) } else { Err(err) }
    }

    pub fn encode_uyva(&mut self, src: &mut [u8], stride: i32) -> Result<(), i32> {
        let err = unsafe { VMX_EncodeUYVA(self.ptr, src.as_mut_ptr(), stride, 0) };
        if err == VMX_ERR_OK { Ok(()) } else { Err(err) }
    }
}

impl Drop for VmxInstance {
    fn drop(&mut self) {
        unsafe { VMX_Destroy(self.ptr) }
    }
}

/// Output layouts for [`VmxDecoder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeFormat {
    /// 8-bit B, G, R, A per pixel; 4 bytes per pixel.
    Bgra,
    /// 4:2:2 U Y V Y; 2 bytes per pixel.
    Uyvy,
    /// UYVY then a full-resolution alpha plane; 3 bytes per pixel in total.
    Uyva,
}

impl DecodeFormat {
    /// Bytes for a `width` x `height` picture with tightly packed rows.
    pub fn frame_size(self, width: i32, height: i32) -> usize {
        let px = (width.max(0) * height.max(0)) as usize;
        match self {
            Self::Bgra => px * 4,
            Self::Uyvy => px * 2,
            Self::Uyva => px * 3,
        }
    }

    /// Bytes per row of the first plane, tightly packed.
    pub fn stride(self, width: i32) -> i32 {
        match self {
            Self::Bgra => width * 4,
            Self::Uyvy | Self::Uyva => width * 2,
        }
    }
}

/// Decodes a stream of VMX frames, recreating its codec instance when the
/// picture size changes.
#[derive(Default)]
pub struct VmxDecoder {
    instance: Option<(VmxInstance, i32, i32)>,
    scratch: Vec<u8>,
}

impl VmxDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one `width` x `height` VMX frame into `dst` (row stride
    /// `stride` bytes for the first plane).
    pub fn decode(
        &mut self,
        vmx: &[u8],
        width: i32,
        height: i32,
        format: DecodeFormat,
        dst: &mut [u8],
        stride: i32,
    ) -> Result<(), crate::Error> {
        let codec = |what: &str, e: i32| crate::Error::Codec(format!("VMX {what} failed ({e})"));
        if dst.len() < format.frame_size(width, height) {
            return Err(crate::Error::Codec(format!(
                "output buffer too small for {width}x{height}"
            )));
        }
        if !matches!(&self.instance, Some((_, w, h)) if *w == width && *h == height) {
            let inst =
                VmxInstance::new(width, height, VmxProfile::Default, VmxColorSpace::Undefined)
                    .ok_or_else(|| crate::Error::Codec("VMX_Create failed".into()))?;
            self.instance = Some((inst, width, height));
        }
        let (inst, _, _) = self.instance.as_mut().unwrap();
        // VMX_LoadFrom wants a mutable buffer.
        self.scratch.clear();
        self.scratch.extend_from_slice(vmx);
        inst.load_from(&mut self.scratch)
            .map_err(|e| codec("load", e))?;
        match format {
            DecodeFormat::Bgra => inst.decode_bgra(dst, stride),
            DecodeFormat::Uyvy => inst.decode_uyvy(dst, stride),
            DecodeFormat::Uyva => inst.decode_uyva(dst, stride),
        }
        .map_err(|e| codec("decode", e))
    }
}

/// Phase 0 smoke test: create and destroy a 1080p OMT-HQ codec instance.
pub fn probe() -> String {
    match VmxInstance::new(1920, 1080, VmxProfile::OmtHq, VmxColorSpace::Bt709) {
        Some(_instance) => "VMX 1080p instance OK ✓".to_string(),
        None => "VMX_Create returned null".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i32 = 256;
    const H: i32 = 128;
    const STRIDE: i32 = W * 4;

    /// A smooth BGRA gradient. Smooth on purpose: VMX is a lossy intra-frame
    /// DCT codec with 4:2:2 chroma, so a high-frequency pattern would be
    /// testing the quantizer rather than the round trip.
    fn gradient() -> Vec<u8> {
        let mut src = vec![0u8; (W * H * 4) as usize];
        for y in 0..H {
            for x in 0..W {
                let i = ((y * STRIDE) + x * 4) as usize;
                src[i] = (x / 2) as u8; // B
                src[i + 1] = (y) as u8; // G
                src[i + 2] = 128; // R
                src[i + 3] = 255; // A
            }
        }
        src
    }

    fn instance() -> VmxInstance {
        VmxInstance::new(W, H, VmxProfile::OmtHq, VmxColorSpace::Bt709)
            .expect("VMX_Create returned null")
    }

    /// Encode then decode a frame through the vendored codec. This is the only
    /// test that exercises the C++ side directly, so it is also the guard for
    /// the VMX_API linkage asymmetry patched in vendor/libvmx (see VENDORED.md):
    /// a C++-mangled entry point fails the link before this ever runs.
    #[test]
    fn bgra_round_trip_preserves_the_image() {
        let mut src = gradient();
        let mut enc = instance();
        enc.encode_bgra(&mut src, STRIDE)
            .expect("encode_bgra failed");

        let mut packet = vec![0u8; (W * H * 4) as usize];
        let n = enc.save_to(&mut packet);
        assert!(n > 0, "save_to wrote {n} bytes");
        assert!(
            n < W * H * 4,
            "encoded {n} bytes is no smaller than the {} byte source",
            W * H * 4
        );

        let mut dec = instance();
        dec.load_from(&mut packet[..n as usize])
            .expect("load_from failed");
        let mut out = vec![0u8; (W * H * 4) as usize];
        dec.decode_bgra(&mut out, STRIDE)
            .expect("decode_bgra failed");

        // Mean absolute error over the colour channels. Alpha is deliberately
        // excluded: encode_bgra feeds a 4:2:2 (no-alpha) profile, so what the
        // decoder puts in that byte is not part of the contract.
        let mut sum = 0u64;
        let mut count = 0u64;
        for px in 0..(W * H) as usize {
            for ch in 0..3 {
                let a = src[px * 4 + ch] as i32;
                let b = out[px * 4 + ch] as i32;
                sum += a.abs_diff(b) as u64;
                count += 1;
            }
        }
        let mae = sum as f64 / count as f64;
        assert!(mae < 3.0, "mean absolute error {mae:.2} is too high for HQ");
    }

    #[test]
    fn probe_reports_a_live_instance() {
        assert_eq!(probe(), "VMX 1080p instance OK ✓");
    }
}
