//! Portable LC3 playback subset of AOSP android-16.0.0_r4.
//! Source: system/bta/le_audio/audio_set_scenarios.json in packages/modules/Bluetooth.
//! OEM overrides and duplex/telephony configurations are not implied by these lists.
//!
//! Everything else in this stack decides the stream configuration first and
//! then checks the headphones will accept it. Android works the other way
//! round: it holds an ordered list of complete configurations and walks it,
//! taking the first one the device's published capabilities allow. The list
//! lives in `audio_set_configurations.json` and `audio_set_scenarios.json`
//! inside `packages/modules/Bluetooth`, and the order in it is the order
//! transcribed below.
//!
//! This matters beyond tidiness. The preset this program shipped with was read
//! out of a WPP trace of the *Windows* LE Audio driver, which asks for 48 kHz
//! at 7.5 ms with 90 octets - the setting BAP calls 48_3. Android asks for
//! 48_4 first: 48 kHz at 10 ms with 120 octets. Both carry 96 kbps, so the
//! difference is not bandwidth. A 10 ms frame gives the codec 480 samples to
//! work with instead of 360, which is finer frequency resolution for the same
//! bits - and 48_3 is only Android's fifth choice, not its first.
//!
//! That is the concrete reason the same headphones sound different from a
//! phone. Not the codec, which measures clean across the whole band: the
//! configuration asked for.

use crate::bap::{
    ase::PreferredQos, CodecConfiguration, FrameDuration, PacRecord, QosConfiguration,
    SamplingFrequency,
};

/// One of the codec settings BAP defines in its Table 3.5.
///
/// Named the way the specification and Android name them, so a configuration
/// seen in a log can be found here without translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BapSetting {
    pub name: &'static str,
    pub sampling_frequency: SamplingFrequency,
    pub frame_duration: FrameDuration,
    pub octets_per_frame: u16,
}

impl BapSetting {
    pub const LC3_16_1: Self = Self {
        name: "16_1",
        sampling_frequency: SamplingFrequency::HZ_16000,
        frame_duration: FrameDuration::Ms7_5,
        octets_per_frame: 30,
    };
    pub const LC3_16_2: Self = Self {
        name: "16_2",
        sampling_frequency: SamplingFrequency::HZ_16000,
        frame_duration: FrameDuration::Ms10,
        octets_per_frame: 40,
    };
    pub const LC3_24_2: Self = Self {
        name: "24_2",
        sampling_frequency: SamplingFrequency::HZ_24000,
        frame_duration: FrameDuration::Ms10,
        octets_per_frame: 60,
    };
    pub const LC3_32_2: Self = Self {
        name: "32_2",
        sampling_frequency: SamplingFrequency::HZ_32000,
        frame_duration: FrameDuration::Ms10,
        octets_per_frame: 80,
    };
    pub const LC3_48_1: Self = Self {
        name: "48_1",
        sampling_frequency: SamplingFrequency::HZ_48000,
        frame_duration: FrameDuration::Ms7_5,
        octets_per_frame: 75,
    };
    pub const LC3_48_2: Self = Self {
        name: "48_2",
        sampling_frequency: SamplingFrequency::HZ_48000,
        frame_duration: FrameDuration::Ms10,
        octets_per_frame: 100,
    };
    pub const LC3_48_3: Self = Self {
        name: "48_3",
        sampling_frequency: SamplingFrequency::HZ_48000,
        frame_duration: FrameDuration::Ms7_5,
        octets_per_frame: 90,
    };
    pub const LC3_48_4: Self = Self {
        name: "48_4",
        sampling_frequency: SamplingFrequency::HZ_48000,
        frame_duration: FrameDuration::Ms10,
        octets_per_frame: 120,
    };
    pub const LC3_48_5: Self = Self {
        name: "48_5",
        sampling_frequency: SamplingFrequency::HZ_48000,
        frame_duration: FrameDuration::Ms7_5,
        octets_per_frame: 117,
    };
    pub const LC3_48_6: Self = Self {
        name: "48_6",
        sampling_frequency: SamplingFrequency::HZ_48000,
        frame_duration: FrameDuration::Ms10,
        octets_per_frame: 155,
    };

    /// Bits per second for one channel at this setting.
    pub fn bitrate(&self) -> u32 {
        self.octets_per_frame as u32 * 8 * 1_000_000 / self.frame_duration.microseconds()
    }
}

/// How Android asks for the transport half of the configuration.
///
/// The three named profiles carry a target latency and nothing else: Android
/// writes zero for the retransmission count and the transport latency, and
/// fills them in afterwards from what the device published in its Codec
/// Configured state. So does this - see [`QosProfile::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QosProfile {
    LowLatency,
    BalancedReliability,
    HighReliability,
    /// A profile that names its own numbers, as Android's `QoS_Config_*_2`
    /// entries do.
    Explicit {
        retransmission_number: u8,
        max_transport_latency_ms: u16,
    },
}

impl QosProfile {
    /// The Target_Latency byte sent with Config Codec.
    pub fn target_latency(&self) -> u8 {
        use crate::bap::ascs;
        match self {
            QosProfile::LowLatency => ascs::LATENCY_LOW,
            QosProfile::BalancedReliability => ascs::LATENCY_BALANCED,
            QosProfile::HighReliability => ascs::LATENCY_HIGH_RELIABILITY,
            // An explicit profile is still a reliability-flavoured one; Android
            // pairs these with the balanced target latency.
            QosProfile::Explicit { .. } => ascs::LATENCY_BALANCED,
        }
    }

    /// Turns the profile into the numbers Config QoS carries.
    ///
    /// `preferred` is what the device published when it answered Config Codec.
    /// Taking its values rather than imposing ours is the whole point of the
    /// named profiles: the device knows what its own radio can hold, and a host
    /// that overrides that is guessing against the firmware. When the device
    /// said nothing, fall back to numbers that are known to work rather than to
    /// zero, which no controller will schedule.
    pub fn resolve(
        &self,
        codec: &CodecConfiguration,
        preferred: Option<&PreferredQos>,
    ) -> QosConfiguration {
        const FALLBACK_RETRANSMISSIONS: u8 = 13;
        const FALLBACK_LATENCY_MS: u16 = 95;
        const FALLBACK_PRESENTATION_DELAY_US: u32 = 40_000;

        let (retransmission_number, max_transport_latency_ms) = match self {
            QosProfile::Explicit {
                retransmission_number,
                max_transport_latency_ms,
            } => (*retransmission_number, *max_transport_latency_ms),
            _ => match preferred {
                Some(qos) if qos.retransmission_preference > 0 => (
                    qos.retransmission_preference,
                    if qos.max_transport_latency_ms > 0 {
                        qos.max_transport_latency_ms
                    } else {
                        FALLBACK_LATENCY_MS
                    },
                ),
                _ => (FALLBACK_RETRANSMISSIONS, FALLBACK_LATENCY_MS),
            },
        };

        let presentation_delay_us = preferred
            .map(|qos| qos.choose_presentation_delay(FALLBACK_PRESENTATION_DELAY_US))
            .unwrap_or(FALLBACK_PRESENTATION_DELAY_US);

        QosConfiguration {
            sdu_interval_us: codec.frame_duration.microseconds(),
            framing: 0x00, // unframed
            phy: 0x02,     // 2M
            max_sdu: codec.sdu_size(),
            retransmission_number,
            max_transport_latency_ms,
            presentation_delay_us,
        }
    }
}

/// One entry of a scenario's priority list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub setting: BapSetting,
    pub qos: QosProfile,
}

const fn candidate(setting: BapSetting, qos: QosProfile) -> Candidate {
    Candidate { setting, qos }
}

/// The Media scenario, in Android's order.
///
/// Transcribed from `audio_set_scenarios.json`, keeping the two-ASE sink
/// entries - one channel per endpoint, which is the stereo layout this stack
/// uses. The pattern repeats per setting: the reliability-profile entry first,
/// then the variant with explicit numbers.
pub const MEDIA_ORDER: &[Candidate] = &[
    candidate(BapSetting::LC3_48_4, QosProfile::HighReliability),
    candidate(
        BapSetting::LC3_48_4,
        QosProfile::Explicit {
            retransmission_number: 13,
            max_transport_latency_ms: 100,
        },
    ),
    candidate(BapSetting::LC3_48_2, QosProfile::HighReliability),
    candidate(
        BapSetting::LC3_48_2,
        QosProfile::Explicit {
            retransmission_number: 13,
            max_transport_latency_ms: 95,
        },
    ),
    candidate(BapSetting::LC3_48_3, QosProfile::HighReliability),
    candidate(
        BapSetting::LC3_48_3,
        QosProfile::Explicit {
            retransmission_number: 13,
            max_transport_latency_ms: 75,
        },
    ),
    candidate(BapSetting::LC3_48_1, QosProfile::HighReliability),
    candidate(
        BapSetting::LC3_48_1,
        QosProfile::Explicit {
            retransmission_number: 13,
            max_transport_latency_ms: 75,
        },
    ),
    candidate(BapSetting::LC3_24_2, QosProfile::BalancedReliability),
    candidate(
        BapSetting::LC3_24_2,
        QosProfile::Explicit {
            retransmission_number: 13,
            max_transport_latency_ms: 95,
        },
    ),
    candidate(BapSetting::LC3_16_2, QosProfile::BalancedReliability),
    candidate(
        BapSetting::LC3_16_2,
        QosProfile::Explicit {
            retransmission_number: 13,
            max_transport_latency_ms: 95,
        },
    ),
    candidate(BapSetting::LC3_16_1, QosProfile::BalancedReliability),
    candidate(
        BapSetting::LC3_16_1,
        QosProfile::Explicit {
            retransmission_number: 13,
            max_transport_latency_ms: 75,
        },
    ),
];

/// The Game scenario. Shorter frames, because latency is the thing being
/// bought and quality is what pays for it.
pub const GAME_ORDER: &[Candidate] = &[
    candidate(BapSetting::LC3_48_2, QosProfile::LowLatency),
    candidate(BapSetting::LC3_48_3, QosProfile::LowLatency),
    candidate(BapSetting::LC3_48_1, QosProfile::LowLatency),
    candidate(BapSetting::LC3_32_2, QosProfile::LowLatency),
    candidate(BapSetting { name: "32_1", sampling_frequency: SamplingFrequency::HZ_32000, frame_duration: FrameDuration::Ms7_5, octets_per_frame: 60 }, QosProfile::LowLatency),
    candidate(BapSetting::LC3_24_2, QosProfile::LowLatency),
    candidate(BapSetting { name: "24_1", sampling_frequency: SamplingFrequency::HZ_24000, frame_duration: FrameDuration::Ms7_5, octets_per_frame: 45 }, QosProfile::LowLatency),
    candidate(BapSetting::LC3_16_2, QosProfile::LowLatency),
    candidate(BapSetting::LC3_16_1, QosProfile::LowLatency),
];

/// What the walk over a priority list settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub setting: BapSetting,
    pub qos: QosProfile,
    pub codec: CodecConfiguration,
    /// How far down the list this was found. Zero is Android's first choice;
    /// anything else is worth saying out loud, because it means the headphones
    /// turned down what a phone would have asked for.
    pub rank: usize,
}

/// Walks a priority list and returns the first configuration the device allows.
///
/// This is Android's algorithm, and it is the opposite of deciding first and
/// validating afterwards. A device that cannot do 48 kHz is not an error here;
/// it simply falls through to the 24 and 16 kHz entries, which is why this
/// works on headphones nobody has tested against.
pub fn choose(
    order: &[Candidate],
    sink_records: &[PacRecord],
    stereo_in_one_stream: bool,
) -> Option<Chosen> {
    let lc3: Vec<&PacRecord> = sink_records.iter().filter(|r| r.is_lc3()).collect();
    if lc3.is_empty() {
        return None;
    }

    let allocation = if stereo_in_one_stream {
        crate::bap::LOCATION_STEREO
    } else {
        crate::bap::LOCATION_FRONT_LEFT
    };
    let frames_per_sdu = 1;

    order.iter().enumerate().find_map(|(rank, candidate)| {
        let codec = CodecConfiguration {
            sampling_frequency: candidate.setting.sampling_frequency,
            frame_duration: candidate.setting.frame_duration,
            channel_allocation: allocation,
            octets_per_frame: candidate.setting.octets_per_frame,
            frames_per_sdu,
        };

        lc3.iter()
            .any(|record| record.capabilities.accepts(&codec).is_ok())
            .then(|| Chosen {
                setting: candidate.setting,
                qos: candidate.qos,
                codec,
                rank,
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::synthetic_capabilities;

    #[test]
    fn media_starts_where_android_starts() {
        // 48_4 is the head of Android's Media list, and it is the setting a
        // phone asks for first. The preset this program shipped with was 48_3,
        // five places further down.
        assert_eq!(MEDIA_ORDER[0].setting, BapSetting::LC3_48_4);
        assert_eq!(MEDIA_ORDER[0].setting.frame_duration, FrameDuration::Ms10);
        assert_eq!(MEDIA_ORDER[0].setting.octets_per_frame, 120);
        assert_eq!(MEDIA_ORDER[0].qos, QosProfile::HighReliability);

        let position = MEDIA_ORDER
            .iter()
            .position(|c| c.setting == BapSetting::LC3_48_3)
            .expect("48_3 is on the list, just not at the top");
        assert_eq!(position, 4, "48_3 is Android's fifth entry");
    }

    #[test]
    fn the_two_settings_carry_the_same_bitrate() {
        // So the difference between them is frame length, not bandwidth. Worth
        // stating in a test, because "Android sounds better" invites the wrong
        // fix of simply raising the bitrate.
        assert_eq!(BapSetting::LC3_48_4.bitrate(), 96_000);
        assert_eq!(BapSetting::LC3_48_3.bitrate(), 96_000);
    }

    #[test]
    fn a_capable_device_gets_androids_first_choice() {
        let caps = synthetic_capabilities(false, 2);
        let chosen = choose(MEDIA_ORDER, &caps.sink_records, false).expect("LC3 sink present");

        assert_eq!(chosen.setting, BapSetting::LC3_48_4);
        assert_eq!(chosen.rank, 0);
        assert_eq!(chosen.codec.frame_duration, FrameDuration::Ms10);
        assert_eq!(chosen.codec.octets_per_frame, 120);
    }

    #[test]
    fn a_device_that_cannot_do_48_khz_falls_through_instead_of_failing() {
        // The universality case. Deciding first and validating afterwards turns
        // this device into an error; walking the list turns it into 24 or
        // 16 kHz audio, which is what a phone would give it.
        use crate::bap::{CodecCapabilities, PacRecord};

        let narrow = PacRecord {
            coding_format: PacRecord::CODING_FORMAT_LC3,
            company_id: 0,
            vendor_codec_id: 0,
            capabilities: CodecCapabilities {
                sampling_frequencies: vec![SamplingFrequency::HZ_16000],
                supports_7_5ms: true,
                supports_10ms: true,
                min_octets_per_frame: Some(26),
                max_octets_per_frame: Some(40),
                channel_counts: vec![1],
                ..Default::default()
            },
            raw: Vec::new(),
        };

        let chosen = choose(MEDIA_ORDER, &[narrow], false).expect("16 kHz is on the list");
        assert_eq!(chosen.setting.sampling_frequency, SamplingFrequency::HZ_16000);
        assert!(chosen.rank > 0, "not the first choice, and that is correct");
    }

    #[test]
    fn a_named_profile_takes_the_numbers_the_device_published() {
        let codec = CodecConfiguration {
            sampling_frequency: SamplingFrequency::HZ_48000,
            frame_duration: FrameDuration::Ms10,
            channel_allocation: crate::bap::LOCATION_FRONT_LEFT,
            octets_per_frame: 120,
            frames_per_sdu: 1,
        };

        let preferred = PreferredQos {
            framing: 0,
            phy_preference: 2,
            retransmission_preference: 13,
            max_transport_latency_ms: 75,
            presentation_delay_min_us: 40_000,
            presentation_delay_max_us: 40_000,
            preferred_delay_min_us: 40_000,
            preferred_delay_max_us: 40_000,
        };

        let qos = QosProfile::HighReliability.resolve(&codec, Some(&preferred));
        assert_eq!(qos.retransmission_number, 13, "the device's own preference");
        assert_eq!(qos.max_transport_latency_ms, 75);
        assert_eq!(qos.presentation_delay_us, 40_000);
        assert_eq!(qos.sdu_interval_us, 10_000);

        // An explicit profile overrides, because that is what makes it explicit.
        let explicit = QosProfile::Explicit {
            retransmission_number: 5,
            max_transport_latency_ms: 20,
        };
        let qos = explicit.resolve(&codec, Some(&preferred));
        assert_eq!(qos.retransmission_number, 5);
        assert_eq!(qos.max_transport_latency_ms, 20);
    }

    #[test]
    fn a_silent_device_still_gets_schedulable_numbers() {
        // Android writes zero and fills in from the device's answer. A device
        // that answers nothing must not leave zeros on the wire: no controller
        // will schedule a CIS with no retransmissions and no latency budget.
        let codec = CodecConfiguration {
            sampling_frequency: SamplingFrequency::HZ_48000,
            frame_duration: FrameDuration::Ms10,
            channel_allocation: crate::bap::LOCATION_FRONT_LEFT,
            octets_per_frame: 120,
            frames_per_sdu: 1,
        };

        let qos = QosProfile::HighReliability.resolve(&codec, None);
        assert!(qos.retransmission_number > 0);
        assert!(qos.max_transport_latency_ms > 0);
        assert!(qos.presentation_delay_us > 0);
    }

    #[test]
    fn target_latency_matches_the_profile() {
        use crate::bap::ascs;
        assert_eq!(
            QosProfile::HighReliability.target_latency(),
            ascs::LATENCY_HIGH_RELIABILITY
        );
        assert_eq!(QosProfile::LowLatency.target_latency(), ascs::LATENCY_LOW);
    }
}
