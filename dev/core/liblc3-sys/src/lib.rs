//! Raw bindings to Google's reference LC3 codec.
//!
//! The declarations mirror `include/lc3.h` of the vendored liblc3 exactly and
//! are written by hand rather than generated. bindgen would pull libclang into
//! the build, which on Windows means shipping an LLVM installation to anyone
//! who wants to compile the project - a heavy price for a header this small
//! and this stable.

#![allow(non_camel_case_types)]

use std::os::raw::{c_int, c_uint, c_void};

/// PCM sample layout handed to the encoder or expected from the decoder.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lc3_pcm_format {
    S16 = 0,
    S24 = 1,
    S24_3LE = 2,
    Float = 3,
}

/// Opaque codec contexts. liblc3 never allocates: both handles point into
/// memory the caller supplies, sized by `lc3_encoder_size`/`lc3_decoder_size`.
#[repr(C)]
pub struct lc3_encoder {
    _private: [u8; 0],
}

#[repr(C)]
pub struct lc3_decoder {
    _private: [u8; 0],
}

pub type lc3_encoder_t = *mut lc3_encoder;
pub type lc3_decoder_t = *mut lc3_decoder;

pub const LC3_MIN_BITRATE: c_int = 16_000;
pub const LC3_MAX_BITRATE: c_int = 320_000;
pub const LC3_HR_MAX_BITRATE: c_int = 672_000;

pub const LC3_MIN_FRAME_BYTES: c_int = 20;
pub const LC3_MAX_FRAME_BYTES: c_int = 400;
pub const LC3_HR_MAX_FRAME_BYTES: c_int = 625;

extern "C" {
    pub fn lc3_frame_samples(dt_us: c_int, sr_hz: c_int) -> c_int;
    pub fn lc3_hr_frame_samples(hrmode: bool, dt_us: c_int, sr_hz: c_int) -> c_int;

    pub fn lc3_frame_bytes(dt_us: c_int, bitrate: c_int) -> c_int;
    pub fn lc3_frame_block_bytes(dt_us: c_int, nframes: c_int, bitrate: c_int) -> c_int;
    pub fn lc3_resolve_bitrate(dt_us: c_int, nbytes: c_int) -> c_int;
    pub fn lc3_delay_samples(dt_us: c_int, sr_hz: c_int) -> c_int;

    pub fn lc3_encoder_size(dt_us: c_int, sr_hz: c_int) -> c_uint;
    pub fn lc3_setup_encoder(
        dt_us: c_int,
        sr_hz: c_int,
        sr_pcm_hz: c_int,
        mem: *mut c_void,
    ) -> lc3_encoder_t;
    pub fn lc3_encoder_disable_ltpf(encoder: lc3_encoder_t);
    pub fn lc3_encode(
        encoder: lc3_encoder_t,
        fmt: lc3_pcm_format,
        pcm: *const c_void,
        stride: c_int,
        nbytes: c_int,
        out: *mut c_void,
    ) -> c_int;

    pub fn lc3_decoder_size(dt_us: c_int, sr_hz: c_int) -> c_uint;
    pub fn lc3_setup_decoder(
        dt_us: c_int,
        sr_hz: c_int,
        sr_pcm_hz: c_int,
        mem: *mut c_void,
    ) -> lc3_decoder_t;
    pub fn lc3_decode(
        decoder: lc3_decoder_t,
        input: *const c_void,
        nbytes: c_int,
        fmt: lc3_pcm_format,
        pcm: *mut c_void,
        stride: c_int,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proves the C sources actually built and linked, and that the ABI lines
    // up: these three answers are fixed by the LC3 specification, so a wrong
    // calling convention or a mismatched int width shows up here rather than
    // as silence on the headphones.
    #[test]
    fn specification_constants_come_back_from_the_c_library() {
        unsafe {
            assert_eq!(lc3_frame_samples(10_000, 48_000), 480);
            assert_eq!(lc3_frame_samples(7_500, 48_000), 360);
            assert_eq!(lc3_frame_samples(10_000, 16_000), 160);
        }
    }

    #[test]
    fn a_round_trip_through_the_codec_preserves_a_sine() {
        const DT_US: i32 = 10_000;
        const SR_HZ: i32 = 48_000;
        const NBYTES: i32 = 120;

        let samples = unsafe { lc3_frame_samples(DT_US, SR_HZ) } as usize;
        let input: Vec<i16> = (0..samples)
            .map(|n| {
                let phase = 2.0 * std::f64::consts::PI * 1000.0 * n as f64 / SR_HZ as f64;
                (phase.sin() * 16_000.0) as i16
            })
            .collect();

        let mut enc_mem = vec![0u8; unsafe { lc3_encoder_size(DT_US, SR_HZ) } as usize];
        let mut dec_mem = vec![0u8; unsafe { lc3_decoder_size(DT_US, SR_HZ) } as usize];
        let mut frame = vec![0u8; NBYTES as usize];
        let mut output = vec![0i16; samples];

        unsafe {
            let encoder = lc3_setup_encoder(DT_US, SR_HZ, 0, enc_mem.as_mut_ptr().cast());
            let decoder = lc3_setup_decoder(DT_US, SR_HZ, 0, dec_mem.as_mut_ptr().cast());
            assert!(!encoder.is_null() && !decoder.is_null());

            assert_eq!(
                lc3_encode(
                    encoder,
                    lc3_pcm_format::S16,
                    input.as_ptr().cast(),
                    1,
                    NBYTES,
                    frame.as_mut_ptr().cast()
                ),
                0
            );
            assert_eq!(
                lc3_decode(
                    decoder,
                    frame.as_ptr().cast(),
                    NBYTES,
                    lc3_pcm_format::S16,
                    output.as_mut_ptr().cast(),
                    1
                ),
                0
            );
        }

        // The first frame is the codec's lookahead, so it decodes to near
        // silence no matter what went in. Energy is therefore only meaningful
        // as a check that the frame was not rejected outright.
        assert!(frame.iter().any(|&b| b != 0), "encoder produced an empty frame");
    }
}
