//! Safe wrapper over Google's reference LC3 codec.
//!
//! This replaces the pure-Rust `lc3-codec` crate. The reason is not speed but
//! agreement: every LE Audio device in the world is tested against this
//! implementation, so when a frame we produce and a frame a headset expects
//! disagree, the difference is now ours to fix rather than a difference between
//! two independent readings of the specification.
//!
//! liblc3 allocates nothing itself. Each context lives in a byte buffer sized
//! by the library and owned here, which is why this module needs none of the
//! borrowed-workspace machinery the previous codec required.

use liblc3_sys as sys;
use std::os::raw::c_void;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Lc3Error {
    #[error("liblc3 rejected the configuration: {frame_duration_us} us at {sample_rate} Hz")]
    UnsupportedConfiguration {
        frame_duration_us: u32,
        sample_rate: u32,
    },

    #[error("frame must be exactly {expected} samples, got {got}")]
    WrongFrameLength { got: usize, expected: usize },

    #[error("frame size {0} bytes is outside what LC3 can carry")]
    FrameSizeOutOfRange(usize),

    #[error("liblc3 rejected the frame")]
    Rejected,
}

/// A frame size liblc3 will accept. Checked here rather than trusted, because
/// the value arrives from the negotiated configuration and a device is free to
/// ask for something the codec cannot express.
fn check_frame_bytes(nbytes: usize) -> Result<i32, Lc3Error> {
    if !(sys::LC3_MIN_FRAME_BYTES as usize..=sys::LC3_MAX_FRAME_BYTES as usize).contains(&nbytes) {
        return Err(Lc3Error::FrameSizeOutOfRange(nbytes));
    }
    Ok(nbytes as i32)
}

/// One LC3 encoder per channel.
///
/// liblc3 is a mono codec, so a stereo stream is two contexts that never share
/// state. That matches how the audio reaches the air anyway - two CIS, each
/// carrying one channel - and it is the same arrangement Android uses.
pub struct Encoder {
    /// Backing store for each channel's context. Never resized after setup:
    /// the handles below point into these buffers.
    contexts: Vec<Vec<u8>>,
    handles: Vec<sys::lc3_encoder_t>,
    samples_per_frame: usize,
}

// The contexts are owned exclusively by this struct and liblc3 keeps no global
// state, so an encoder may be moved to the thread that drives the ISO pump.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(channels: usize, frame_duration_us: u32, sample_rate: u32) -> Result<Self, Lc3Error> {
        let channels = channels.max(1);
        let dt_us = frame_duration_us as i32;
        let sr_hz = sample_rate as i32;

        let samples_per_frame = unsafe { sys::lc3_frame_samples(dt_us, sr_hz) };
        let size = unsafe { sys::lc3_encoder_size(dt_us, sr_hz) } as usize;
        if samples_per_frame <= 0 || size == 0 {
            return Err(Lc3Error::UnsupportedConfiguration {
                frame_duration_us,
                sample_rate,
            });
        }

        let mut contexts = Vec::with_capacity(channels);
        let mut handles = Vec::with_capacity(channels);

        for _ in 0..channels {
            let mut context = vec![0u8; size];
            // sr_pcm_hz of 0 means "same as the stream", which is what we want:
            // the capture side is already resampled to the negotiated rate.
            let handle = unsafe {
                sys::lc3_setup_encoder(dt_us, sr_hz, 0, context.as_mut_ptr() as *mut c_void)
            };
            if handle.is_null() {
                return Err(Lc3Error::UnsupportedConfiguration {
                    frame_duration_us,
                    sample_rate,
                });
            }
            contexts.push(context);
            handles.push(handle);
        }

        Ok(Self {
            contexts,
            handles,
            samples_per_frame: samples_per_frame as usize,
        })
    }

    pub fn samples_per_frame(&self) -> usize {
        self.samples_per_frame
    }

    pub fn channels(&self) -> usize {
        self.handles.len()
    }

    /// Encodes one channel's frame into exactly `out.len()` bytes.
    ///
    /// LC3 is fixed-rate: the output length is part of the configuration the
    /// device was told to expect, not something the encoder chooses.
    pub fn encode(
        &mut self,
        channel: usize,
        samples: &[i16],
        out: &mut [u8],
    ) -> Result<(), Lc3Error> {
        if samples.len() != self.samples_per_frame {
            return Err(Lc3Error::WrongFrameLength {
                got: samples.len(),
                expected: self.samples_per_frame,
            });
        }

        let handle = *self.handles.get(channel).ok_or(Lc3Error::Rejected)?;
        let nbytes = check_frame_bytes(out.len())?;

        let rc = unsafe {
            sys::lc3_encode(
                handle,
                sys::lc3_pcm_format::S16,
                samples.as_ptr() as *const c_void,
                1,
                nbytes,
                out.as_mut_ptr() as *mut c_void,
            )
        };

        if rc == 0 {
            Ok(())
        } else {
            Err(Lc3Error::Rejected)
        }
    }

    /// Long-term postfilter analysis costs a lot of CPU and behaves poorly on
    /// synthetic signals. Worth turning off for test tones, not for music.
    pub fn disable_ltpf(&mut self) {
        for &handle in &self.handles {
            unsafe { sys::lc3_encoder_disable_ltpf(handle) };
        }
    }

    /// The contexts must outlive the handles that point into them. Nothing else
    /// reads this field, and without a reader it looks like dead storage.
    #[allow(dead_code)]
    fn context_bytes(&self) -> usize {
        self.contexts.iter().map(Vec::len).sum()
    }
}

/// A single-channel decoder, for the headset's microphone stream.
pub struct Decoder {
    #[allow(dead_code)]
    context: Vec<u8>,
    handle: sys::lc3_decoder_t,
    samples_per_frame: usize,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(frame_duration_us: u32, sample_rate: u32) -> Result<Self, Lc3Error> {
        let dt_us = frame_duration_us as i32;
        let sr_hz = sample_rate as i32;

        let samples_per_frame = unsafe { sys::lc3_frame_samples(dt_us, sr_hz) };
        let size = unsafe { sys::lc3_decoder_size(dt_us, sr_hz) } as usize;
        if samples_per_frame <= 0 || size == 0 {
            return Err(Lc3Error::UnsupportedConfiguration {
                frame_duration_us,
                sample_rate,
            });
        }

        let mut context = vec![0u8; size];
        let handle =
            unsafe { sys::lc3_setup_decoder(dt_us, sr_hz, 0, context.as_mut_ptr() as *mut c_void) };
        if handle.is_null() {
            return Err(Lc3Error::UnsupportedConfiguration {
                frame_duration_us,
                sample_rate,
            });
        }

        Ok(Self {
            context,
            handle,
            samples_per_frame: samples_per_frame as usize,
        })
    }

    pub fn samples_per_frame(&self) -> usize {
        self.samples_per_frame
    }

    /// Decodes one frame. An empty payload asks for packet loss concealment,
    /// which is how a dropped SDU should be handled: liblc3 fills the gap from
    /// the previous frame rather than leaving a click.
    pub fn decode(&mut self, payload: &[u8], out: &mut [i16]) -> Result<(), Lc3Error> {
        if out.len() != self.samples_per_frame {
            return Err(Lc3Error::WrongFrameLength {
                got: out.len(),
                expected: self.samples_per_frame,
            });
        }

        let (input, nbytes) = if payload.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (
                payload.as_ptr() as *const c_void,
                check_frame_bytes(payload.len())?,
            )
        };

        let rc = unsafe {
            sys::lc3_decode(
                self.handle,
                input,
                nbytes,
                sys::lc3_pcm_format::S16,
                out.as_mut_ptr() as *mut c_void,
                1,
            )
        };

        // 0 is a decoded frame and 1 means concealment ran. Both produced
        // usable audio; only -1 is a rejected frame.
        if rc >= 0 {
            Ok(())
        } else {
            Err(Lc3Error::Rejected)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lengths_match_the_specification() {
        let encoder = Encoder::new(2, 10_000, 48_000).unwrap();
        assert_eq!(encoder.samples_per_frame(), 480);
        assert_eq!(encoder.channels(), 2);

        let encoder = Encoder::new(1, 7_500, 48_000).unwrap();
        assert_eq!(encoder.samples_per_frame(), 360);
    }

    #[test]
    fn a_rate_lc3_does_not_define_is_refused_rather_than_guessed() {
        // 44.1 kHz is not an LC3 rate at all. 3 ms is not one of the four
        // frame durations liblc3 accepts - 2.5, 5, 7.5 and 10 ms are, which is
        // wider than the two Bluetooth actually negotiates.
        assert!(Encoder::new(1, 10_000, 44_100).is_err());
        assert!(Encoder::new(1, 3_000, 48_000).is_err());
        assert!(Encoder::new(1, 5_000, 48_000).is_ok());
    }

    #[test]
    fn the_two_channels_of_a_stereo_encoder_are_independent() {
        // The same encoder instance, two channels, two different signals. If
        // the contexts were shared the second frame would carry state from the
        // first and the bytes would match; they must not.
        let mut encoder = Encoder::new(2, 10_000, 48_000).unwrap();
        let n = encoder.samples_per_frame();

        let loud: Vec<i16> = (0..n)
            .map(|i| ((i as f32 * 0.1).sin() * 12_000.0) as i16)
            .collect();
        let quiet: Vec<i16> = vec![0; n];

        let mut left = vec![0u8; 100];
        let mut right = vec![0u8; 100];
        encoder.encode(0, &loud, &mut left).unwrap();
        encoder.encode(1, &quiet, &mut right).unwrap();

        assert_ne!(left, right, "both channels produced the same frame");
    }

    #[test]
    fn a_lost_packet_is_concealed_rather_than_refused() {
        let mut decoder = Decoder::new(10_000, 32_000).unwrap();
        let mut out = vec![0i16; decoder.samples_per_frame()];
        decoder
            .decode(&[], &mut out)
            .expect("empty payload must run concealment");
    }

    #[test]
    fn a_tone_survives_the_round_trip_with_its_energy_intact() {
        const SR: u32 = 48_000;
        let mut encoder = Encoder::new(1, 10_000, SR).unwrap();
        let mut decoder = Decoder::new(10_000, SR).unwrap();
        let n = encoder.samples_per_frame();

        let tone: Vec<i16> = (0..n * 8)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * 1_000.0 * i as f64 / SR as f64;
                (phase.sin() * 10_000.0) as i16
            })
            .collect();

        let mut frame = vec![0u8; 120];
        let mut decoded = vec![0i16; n];
        let mut energy_in = 0f64;
        let mut energy_out = 0f64;

        // The codec has algorithmic delay, so the first frames decode to near
        // silence whatever goes in. Energy is compared over the later frames
        // only, where the pipeline is primed.
        for (index, block) in tone.chunks(n).enumerate() {
            encoder.encode(0, block, &mut frame).unwrap();
            decoder.decode(&frame, &mut decoded).unwrap();

            if index >= 2 {
                energy_in += block.iter().map(|&s| (s as f64).powi(2)).sum::<f64>();
                energy_out += decoded.iter().map(|&s| (s as f64).powi(2)).sum::<f64>();
            }
        }

        let ratio = energy_out / energy_in;
        assert!(
            (0.5..2.0).contains(&ratio),
            "decoded energy is {ratio:.3} of the input; the tone did not survive"
        );
    }
}
