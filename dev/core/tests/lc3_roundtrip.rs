//! Does the LC3 path actually carry the whole audio band?
//!
//! The stream reaches the headphones and plays, but it sounds thin - no bass and
//! no air, as if something band-limited it. Everything on the radio side reports
//! success, so guessing is expensive: each attempt costs a connection, a listen
//! and a report back.
//!
//! This measures it instead, with no hardware involved. A signal with known
//! tones goes through our encoder and back through the matching decoder, and the
//! energy at each tone is compared before and after. If the low and high tones
//! come back missing, the codec path is the problem and the radio is innocent.

use olea_core::lc3::Decoder as Lc3Decoder;
use olea_core::bap::{CodecConfiguration, FrameDuration as BapFrameDuration, SamplingFrequency as BapRate};
use olea_core::stream::AudioEncoder;

const RATE: u32 = 48_000;
const SAMPLES_PER_FRAME: usize = 480; // 10 ms at 48 kHz
const SAMPLES_PER_7MS5: usize = 360; // 7.5 ms at 48 kHz, what Windows uses

/// Energy at one frequency, by the Goertzel algorithm.
///
/// A whole FFT would tell us more than we need. This answers exactly one
/// question - "how much of this tone is present" - and is short enough to read.
fn tone_energy(samples: &[i16], frequency: f32) -> f32 {
    let k = frequency / RATE as f32;
    let coefficient = 2.0 * (2.0 * std::f32::consts::PI * k).cos();

    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &sample in samples {
        let s0 = sample as f32 / 32768.0 + coefficient * s1 - s2;
        s2 = s1;
        s1 = s0;
    }

    (s1 * s1 + s2 * s2 - coefficient * s1 * s2).sqrt() / samples.len() as f32
}

/// A signal with one tone in the bass, one in the middle and one up top.
fn probe_signal(frames: usize) -> Vec<i16> {
    let mut samples = Vec::with_capacity(frames * SAMPLES_PER_FRAME);

    for n in 0..frames * SAMPLES_PER_FRAME {
        let t = n as f32 / RATE as f32;
        let value = (2.0 * std::f32::consts::PI * 60.0 * t).sin() * 0.25
            + (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.25
            + (2.0 * std::f32::consts::PI * 10_000.0 * t).sin() * 0.25;

        samples.push((value * 26_000.0) as i16);
    }

    samples
}

fn configuration(octets: u16) -> CodecConfiguration {
    CodecConfiguration {
        sampling_frequency: BapRate::HZ_48000,
        frame_duration: BapFrameDuration::Ms10,
        channel_allocation: olea_core::bap::LOCATION_FRONT_LEFT,
        octets_per_frame: octets,
        frames_per_sdu: 1,
    }
}

/// Runs the probe signal through encode and decode, returning the output.
fn round_trip(octets: u16) -> Vec<i16> {
    const FRAMES: usize = 40; // 400 ms, long past any codec warm-up

    let mut encoder = AudioEncoder::with_channels(configuration(octets), 1);

    let mut decoder = Lc3Decoder::new(10_000, RATE).expect("48 kHz at 10 ms");

    let input = probe_signal(FRAMES);
    let mut output = Vec::with_capacity(input.len());

    for frame in input.chunks_exact(SAMPLES_PER_FRAME) {
        let encoded = encoder.encode_channel(0, frame).expect("encode failed");

        let mut decoded = [0i16; SAMPLES_PER_FRAME];
        decoder
            .decode(&encoded, &mut decoded)
            .expect("decode failed");

        output.extend_from_slice(&decoded);
    }

    // Drop the first few frames: the codec has an inherent delay and the very
    // start is genuinely quiet, which would look like a missing tone.
    output.split_off(SAMPLES_PER_FRAME * 4)
}

/// Prints what survived, so a failure says which end of the band was lost.
fn report(label: &str, input: &[i16], output: &[i16]) -> Vec<(f32, f32)> {
    let mut ratios = Vec::new();

    println!("\n{label}");
    for frequency in [60.0f32, 1_000.0, 10_000.0] {
        let before = tone_energy(input, frequency);
        let after = tone_energy(output, frequency);
        let ratio = if before > 0.0 { after / before } else { 0.0 };

        println!(
            "  {frequency:>8.0} Hz: {:.4} -> {:.4}  ({:.0} % retained)",
            before,
            after,
            ratio * 100.0
        );

        ratios.push((frequency, ratio));
    }

    ratios
}

#[test]
fn the_codec_carries_bass_middle_and_treble() {
    let input = probe_signal(40);
    let input = &input[SAMPLES_PER_FRAME * 4..];

    let output = round_trip(100);
    let ratios = report("48 kHz, 10 ms, 100 octets (default preset)", input, &output);

    for (frequency, ratio) in ratios {
        assert!(
            ratio > 0.5,
            "at {frequency} Hz only {:.0} % of the tone survived the codec - \
             this is the band limiting we hear",
            ratio * 100.0
        );
    }
}

#[test]
fn more_octets_do_not_lose_the_band() {
    let input = probe_signal(40);
    let input = &input[SAMPLES_PER_FRAME * 4..];

    let output = round_trip(155);
    let ratios = report("48 kHz, 10 ms, 155 octets (high quality)", input, &output);

    for (frequency, ratio) in ratios {
        assert!(
            ratio > 0.5,
            "at {frequency} Hz only {:.0} % survived even at the LC3 ceiling",
            ratio * 100.0
        );
    }
}

/// Encodes two different signals on the two channels of one encoder and decodes
/// both, which is exactly what the dual-CIS path does and what nothing tested.
///
/// The encoder is created once with two channels and shares its working buffers
/// between them. If those buffers overlap, or if channel one's state is not
/// really separate, the second channel comes out wrong - and the symptom on the
/// hardware is the right earpiece playing nothing while the left plays
/// everything, which looks exactly like a routing problem on the radio.
#[test]
fn both_channels_of_a_stereo_encoder_carry_their_own_audio() {
    const FRAMES: usize = 20;
    const OCTETS: u16 = 90;

    let mut encoder = AudioEncoder::with_channels(
        CodecConfiguration { frame_duration: BapFrameDuration::Ms7_5, ..configuration(OCTETS) },
        2,
    );

    let mut decoders: Vec<Lc3Decoder> = (0..2)
        .map(|_| Lc3Decoder::new(7_500, RATE).expect("48 kHz at 7.5 ms"))
        .collect();

    // Two tones far apart, so a channel carrying the wrong one is unmistakable.
    let tone = |hz: f32, n: usize| {
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                ((2.0 * std::f32::consts::PI * hz * t).sin() * 24_000.0) as i16
            })
            .collect::<Vec<i16>>()
    };

    let samples = SAMPLES_PER_7MS5 * FRAMES;
    let left_in = tone(500.0, samples);
    let right_in = tone(4_000.0, samples);

    let mut left_out = Vec::new();
    let mut right_out = Vec::new();

    for frame in 0..FRAMES {
        let range = frame * SAMPLES_PER_7MS5..(frame + 1) * SAMPLES_PER_7MS5;

        for (channel, input, output) in [
            (0usize, &left_in, &mut left_out),
            (1usize, &right_in, &mut right_out),
        ] {
            let encoded = encoder
                .encode_channel(channel, &input[range.clone()])
                .expect("encode failed");

            let mut decoded = [0i16; SAMPLES_PER_7MS5];
            decoders[channel]
                .decode(&encoded, &mut decoded)
                .expect("decode failed");

            output.extend_from_slice(&decoded);
        }
    }

    // Skip the codec's start-up delay before measuring.
    let settled = SAMPLES_PER_7MS5 * 4;
    let left_out = &left_out[settled..];
    let right_out = &right_out[settled..];

    let left_500 = tone_energy(left_out, 500.0);
    let left_4k = tone_energy(left_out, 4_000.0);
    let right_500 = tone_energy(right_out, 500.0);
    let right_4k = tone_energy(right_out, 4_000.0);

    println!("
channel 0: 500 Hz {left_500:.4}, 4 kHz {left_4k:.4}");
    println!("channel 1: 500 Hz {right_500:.4}, 4 kHz {right_4k:.4}");

    assert!(
        left_500 > left_4k * 4.0,
        "channel 0 should carry its own 500 Hz tone, not the other channel's"
    );
    assert!(
        right_4k > right_500 * 4.0,
        "channel 1 came back carrying the wrong audio - the right earpiece          would play nothing recognisable"
    );
}


/// The band test again, at the frame length actually shipping.
///
/// The earlier measurement used 10 ms frames because that was the preset at the
/// time. The Windows trace then moved the default to 7.5 ms, and a codec is
/// perfectly entitled to behave differently at a different frame length - so
/// "LC3 is innocent" had to be re-established, not assumed.
#[test]
fn the_shipping_configuration_carries_bass_middle_and_treble() {
    const FRAMES: usize = 40;
    const OCTETS: u16 = 90;

    let mut encoder = AudioEncoder::with_channels(
        CodecConfiguration { frame_duration: BapFrameDuration::Ms7_5, ..configuration(OCTETS) },
        1,
    );

    let mut decoder = Lc3Decoder::new(7_500, RATE).expect("48 kHz at 7.5 ms");

    let samples = SAMPLES_PER_7MS5 * FRAMES;
    let mut input = Vec::with_capacity(samples);
    for n in 0..samples {
        let t = n as f32 / RATE as f32;
        let value = (2.0 * std::f32::consts::PI * 60.0 * t).sin() * 0.25
            + (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.25
            + (2.0 * std::f32::consts::PI * 10_000.0 * t).sin() * 0.25;
        input.push((value * 26_000.0) as i16);
    }

    let mut output = Vec::with_capacity(samples);
    for frame in input.chunks_exact(SAMPLES_PER_7MS5) {
        let encoded = encoder.encode_channel(0, frame).expect("encode failed");

        let mut decoded = [0i16; SAMPLES_PER_7MS5];
        decoder
            .decode(&encoded, &mut decoded)
            .expect("decode failed");

        output.extend_from_slice(&decoded);
    }

    let settled = SAMPLES_PER_7MS5 * 4;
    let ratios = report(
        "48 kHz, 7.5 ms, 90 octets (headphone stream configuration)",
        &input[settled..],
        &output[settled..],
    );

    for (frequency, ratio) in ratios {
        assert!(
            ratio > 0.5,
            "at {frequency} Hz only {:.0} % survived at the shipping configuration",
            ratio * 100.0
        );
    }
}


/// Where does the top end actually stop?
///
/// The other tests here go no higher than 10 kHz, so they cannot answer the
/// question that matters for "it has no treble". LC3 runs a bandwidth detector:
/// below a certain bitrate it decides the top of the spectrum is not worth the
/// bits and stops coding it, and nothing in the stream says it happened. The
/// stream still reports success, the level meters still move, and the top
/// octave is simply gone.
///
/// This measures one tone at a time - a sweep in one signal would let the loud
/// low tones mask the quiet high ones and blame the codec for masking that is
/// really in the test.
#[test]
fn where_the_top_end_stops_at_each_bitrate() {
    use olea_core::lc3::{Decoder, Encoder};

    const FRAMES: usize = 40;
    const TONES: [f32; 8] = [1_000.0, 8_000.0, 12_000.0, 14_000.0, 16_000.0, 18_000.0, 20_000.0, 22_000.0];

    // 7.5 ms is what the headphones negotiate. 90 octets is what ships today;
    // 155 is the most these headphones said they can take.
    const CONFIGURATIONS: [(u32, u16); 4] = [
        (7_500, 90),
        (7_500, 120),
        (7_500, 155),
        (10_000, 155),
    ];

    println!("\nTop end retained, by configuration (48 kHz)");
    println!("{:>12} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
             "config", "1 kHz", "8 kHz", "12 kHz", "14 kHz", "16 kHz", "18 kHz", "20 kHz", "22 kHz");

    for (frame_us, octets) in CONFIGURATIONS {
        let mut row = format!("{:>7} us {:>3}B", frame_us, octets);

        for tone_hz in TONES {
            let mut encoder = Encoder::new(1, frame_us, RATE).unwrap();
            let mut decoder = Decoder::new(frame_us, RATE).unwrap();
            let n = encoder.samples_per_frame();

            let input: Vec<i16> = (0..n * FRAMES)
                .map(|i| {
                    let t = i as f32 / RATE as f32;
                    ((2.0 * std::f32::consts::PI * tone_hz * t).sin() * 20_000.0) as i16
                })
                .collect();

            let mut frame = vec![0u8; octets as usize];
            let mut decoded = vec![0i16; n];
            let mut output = Vec::with_capacity(input.len());

            for block in input.chunks_exact(n) {
                encoder.encode(0, block, &mut frame).unwrap();
                decoder.decode(&frame, &mut decoded).unwrap();
                output.extend_from_slice(&decoded);
            }

            // Skip the codec's start-up delay before measuring.
            let settled = n * 4;
            let before = tone_energy(&input[settled..], tone_hz);
            let after = tone_energy(&output[settled..], tone_hz);
            let retained = if before > 0.0 { after / before } else { 0.0 };

            row.push_str(&format!("{:>8.0}%", retained * 100.0));
        }

        println!("{row}");
    }

    // 22 kHz sits above what LC3 codes at any Bluetooth bitrate, so it is here
    // as the floor of the measurement rather than as something to require.
    // What is asserted is the part a listener would notice: at the bitrate the
    // headphones actually accept, the presence band must survive.
    let mut encoder = Encoder::new(1, 7_500, RATE).unwrap();
    let mut decoder = Decoder::new(7_500, RATE).unwrap();
    let n = encoder.samples_per_frame();
    let input: Vec<i16> = (0..n * FRAMES)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            ((2.0 * std::f32::consts::PI * 8_000.0 * t).sin() * 20_000.0) as i16
        })
        .collect();
    let mut frame = vec![0u8; 90];
    let mut decoded = vec![0i16; n];
    let mut output = Vec::with_capacity(input.len());
    for block in input.chunks_exact(n) {
        encoder.encode(0, block, &mut frame).unwrap();
        decoder.decode(&frame, &mut decoded).unwrap();
        output.extend_from_slice(&decoded);
    }
    let settled = n * 4;
    let retained = tone_energy(&output[settled..], 8_000.0) / tone_energy(&input[settled..], 8_000.0);
    assert!(retained > 0.5, "8 kHz retained only {:.0}%", retained * 100.0);
}


/// And where does the bottom end stop?
///
/// The counterpart to the treble sweep. Bass is where a codec is most often
/// accused of being thin, and where the accusation is hardest to check by ear
/// because small speakers roll off long before the signal reaching them does.
///
/// 20 Hz is the floor here on purpose. Below that there is nothing to measure
/// against: an LC3 frame at 7.5 ms is shorter than a single cycle of anything
/// under 133 Hz, so the very low end is carried across frames rather than
/// within one, and no headphone driver reproduces it anyway.
#[test]
fn where_the_bottom_end_stops_at_each_bitrate() {
    use olea_core::lc3::{Decoder, Encoder};

    const FRAMES: usize = 120; // Longer than the treble sweep: low tones need
                               // many cycles before the measurement settles.
    const TONES: [f32; 7] = [20.0, 30.0, 40.0, 60.0, 100.0, 200.0, 500.0];

    const CONFIGURATIONS: [(u32, u16); 3] = [
        (7_500, 90),  // what the headphones negotiate today
        (7_500, 155), // the most these headphones accept
        (10_000, 100),
    ];

    println!("\nBottom end retained, by configuration (48 kHz)");
    println!("{:>12} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
             "config", "20 Hz", "30 Hz", "40 Hz", "60 Hz", "100 Hz", "200 Hz", "500 Hz");

    for (frame_us, octets) in CONFIGURATIONS {
        let mut row = format!("{:>7} us {:>3}B", frame_us, octets);

        for tone_hz in TONES {
            let mut encoder = Encoder::new(1, frame_us, RATE).unwrap();
            let mut decoder = Decoder::new(frame_us, RATE).unwrap();
            let n = encoder.samples_per_frame();

            let input: Vec<i16> = (0..n * FRAMES)
                .map(|i| {
                    let t = i as f32 / RATE as f32;
                    ((2.0 * std::f32::consts::PI * tone_hz * t).sin() * 20_000.0) as i16
                })
                .collect();

            let mut frame = vec![0u8; octets as usize];
            let mut decoded = vec![0i16; n];
            let mut output = Vec::with_capacity(input.len());

            for block in input.chunks_exact(n) {
                encoder.encode(0, block, &mut frame).unwrap();
                decoder.decode(&frame, &mut decoded).unwrap();
                output.extend_from_slice(&decoded);
            }

            let settled = n * 8;
            let before = tone_energy(&input[settled..], tone_hz);
            let after = tone_energy(&output[settled..], tone_hz);
            let retained = if before > 0.0 { after / before } else { 0.0 };

            row.push_str(&format!("{:>8.0}%", retained * 100.0));
        }

        println!("{row}");
    }

    // What a listener would notice: the fundamental of a kick drum and of a
    // bass guitar have to arrive at the configuration actually in use.
    for tone_hz in [40.0f32, 60.0] {
        let mut encoder = Encoder::new(1, 7_500, RATE).unwrap();
        let mut decoder = Decoder::new(7_500, RATE).unwrap();
        let n = encoder.samples_per_frame();

        let input: Vec<i16> = (0..n * FRAMES)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                ((2.0 * std::f32::consts::PI * tone_hz * t).sin() * 20_000.0) as i16
            })
            .collect();

        let mut frame = vec![0u8; 90];
        let mut decoded = vec![0i16; n];
        let mut output = Vec::with_capacity(input.len());
        for block in input.chunks_exact(n) {
            encoder.encode(0, block, &mut frame).unwrap();
            decoder.decode(&frame, &mut decoded).unwrap();
            output.extend_from_slice(&decoded);
        }

        let settled = n * 8;
        let retained = tone_energy(&output[settled..], tone_hz) / tone_energy(&input[settled..], tone_hz);
        assert!(retained > 0.5, "{tone_hz} Hz retained only {:.0}%", retained * 100.0);
    }
}
