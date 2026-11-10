//! Linux video decoder implementation using FFmpeg FFI.

use zenlink_core::api::Codec;

enum ZlVideoOpaque {}

extern "C" {
    fn zl_video_codec_available(is_hevc: libc::c_int) -> libc::c_int;
    fn zl_video_open(is_hevc: libc::c_int) -> *mut ZlVideoOpaque;
    fn zl_video_close(v: *mut ZlVideoOpaque);
    fn zl_video_decode(
        v: *mut ZlVideoOpaque,
        data: *const u8,
        len: libc::c_int,
        out_bgra: *mut *mut u8,
        out_w: *mut libc::c_int,
        out_h: *mut libc::c_int,
    ) -> libc::c_int;
}

pub fn media_foundation_present() -> bool {
    // Always true on Linux as FFmpeg replaces Media Foundation
    true
}

pub fn decoder_available(codec: &Codec) -> bool {
    let is_hevc = match codec {
        Codec::H264 => 0,
        Codec::Hevc => 1,
    };
    unsafe { zl_video_codec_available(is_hevc) == 1 }
}

pub struct LinuxDecoder {
    handle: *mut ZlVideoOpaque,
}

unsafe impl Send for LinuxDecoder {}

impl LinuxDecoder {
    pub fn new(codec: &Codec) -> Result<Self, String> {
        let is_hevc = match codec {
            Codec::H264 => 0,
            Codec::Hevc => 1,
        };
        let handle = unsafe { zl_video_open(is_hevc) };
        if handle.is_null() {
            return Err("failed to open FFmpeg video decoder".into());
        }
        Ok(Self { handle })
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<(u32, u32, &[u8])>, String> {
        let mut out_bgra: *mut u8 = std::ptr::null_mut();
        let mut out_w: libc::c_int = 0;
        let mut out_h: libc::c_int = 0;

        let ret = unsafe {
            zl_video_decode(
                self.handle,
                data.as_ptr(),
                data.len() as libc::c_int,
                &mut out_bgra,
                &mut out_w,
                &mut out_h,
            )
        };

        if ret < 0 {
            return Err("video decode error".into());
        }
        if ret == 0 || out_bgra.is_null() || out_w <= 0 || out_h <= 0 {
            return Ok(None);
        }

        let slice = unsafe {
            std::slice::from_raw_parts(out_bgra, (out_w as usize) * (out_h as usize) * 4)
        };
        Ok(Some((out_w as u32, out_h as u32, slice)))
    }
}

impl Drop for LinuxDecoder {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zl_video_close(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}
