//! Basic Audio Profile: what the headphones can do, and what we tell them to do.
//!
//! This is the layer the whole project exists for. PACS says which LC3
//! configurations a device accepts; ASCS is where we pick one. Every parameter
//! the Microsoft driver decides silently is an explicit choice here.

/// Type-Length-Value element, the encoding BAP uses throughout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ltv {
    pub kind: u8,
    pub value: Vec<u8>,
}

impl Ltv {
    pub fn new(kind: u8, value: impl Into<Vec<u8>>) -> Self {
        Self { kind, value: value.into() }
    }

    pub fn encode(items: &[Ltv]) -> Vec<u8> {
        let mut out = Vec::new();
        for item in items {
            out.push((item.value.len() + 1) as u8);
            out.push(item.kind);
            out.extend_from_slice(&item.value);
        }
        out
    }

    /// Walks a concatenated LTV buffer, stopping cleanly on a truncated tail.
    pub fn decode(buffer: &[u8]) -> Vec<Ltv> {
        let mut items = Vec::new();
        let mut offset = 0;

        while offset < buffer.len() {
            let length = buffer[offset] as usize;
            if length == 0 || offset + 1 + length > buffer.len() {
                break;
            }
            items.push(Ltv {
                kind: buffer[offset + 1],
                value: buffer[offset + 2..offset + 1 + length].to_vec(),
            });
            offset += 1 + length;
        }

        items
    }

    fn as_u16(&self) -> Option<u16> {
        (self.value.len() >= 2).then(|| u16::from_le_bytes([self.value[0], self.value[1]]))
    }
}

/// Sampling frequency. The wire uses a small enum, not the value in hertz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplingFrequency(pub u8);

impl SamplingFrequency {
    pub const HZ_8000: Self = Self(0x01);
    pub const HZ_16000: Self = Self(0x03);
    pub const HZ_24000: Self = Self(0x05);
    pub const HZ_32000: Self = Self(0x06);
    pub const HZ_44100: Self = Self(0x07);
    pub const HZ_48000: Self = Self(0x08);

    /// The code for a rate in hertz, for a value someone typed in.
    ///
    /// Only the rates BAP actually defines; anything else has no code to send
    /// and returning `None` is the honest answer rather than the nearest match.
    pub fn from_hz(hz: u32) -> Option<Self> {
        Some(match hz {
            8_000 => Self::HZ_8000,
            16_000 => Self::HZ_16000,
            24_000 => Self::HZ_24000,
            32_000 => Self::HZ_32000,
            44_100 => Self::HZ_44100,
            48_000 => Self::HZ_48000,
            _ => return None,
        })
    }

    pub fn hz(&self) -> Option<u32> {
        Some(match self.0 {
            0x01 => 8_000,
            0x02 => 11_025,
            0x03 => 16_000,
            0x04 => 22_050,
            0x05 => 24_000,
            0x06 => 32_000,
            0x07 => 44_100,
            0x08 => 48_000,
            0x09 => 88_200,
            0x0A => 96_000,
            0x0B => 176_400,
            0x0C => 192_000,
            0x0D => 384_000,
            _ => return None,
        })
    }

    /// The capabilities bitfield uses bit positions, the configuration a value.
    pub fn from_capability_bit(bit: u32) -> Option<Self> {
        (bit < 13).then(|| Self((bit + 1) as u8))
    }
}

/// LC3 frame duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDuration {
    Ms7_5,
    Ms10,
}

impl FrameDuration {
    pub fn encoded(&self) -> u8 {
        match self {
            FrameDuration::Ms7_5 => 0x00,
            FrameDuration::Ms10 => 0x01,
        }
    }

    pub fn microseconds(&self) -> u32 {
        match self {
            FrameDuration::Ms7_5 => 7_500,
            FrameDuration::Ms10 => 10_000,
        }
    }
}

/// Audio channel allocation bits.
pub const LOCATION_FRONT_LEFT: u32 = 1 << 0;
pub const LOCATION_FRONT_RIGHT: u32 = 1 << 1;
/// Both channels in one stream - the layout that avoids needing two CIS.
pub const LOCATION_STEREO: u32 = LOCATION_FRONT_LEFT | LOCATION_FRONT_RIGHT;

/// No location at all, which is how BAP spells "mono".
///
/// A stream that claims neither ear is rendered to whatever the device has,
/// rather than to one side - which is exactly what a single mono stream wants.
pub const LOCATION_MONO: u32 = 0;

/// Codec Specific Capabilities from one PAC record.
#[derive(Debug, Clone, Default)]
pub struct CodecCapabilities {
    pub sampling_frequencies: Vec<SamplingFrequency>,
    pub supports_7_5ms: bool,
    pub supports_10ms: bool,
    pub channel_counts: Vec<u8>,
    pub min_octets_per_frame: Option<u16>,
    pub max_octets_per_frame: Option<u16>,
    pub max_frames_per_sdu: u8,
}

impl CodecCapabilities {
    pub fn parse(buffer: &[u8]) -> Self {
        let mut caps = Self { max_frames_per_sdu: 1, ..Default::default() };

        for ltv in Ltv::decode(buffer) {
            match ltv.kind {
                0x01 => {
                    if let Some(bits) = ltv.as_u16() {
                        for bit in 0..13u32 {
                            if bits & (1 << bit) != 0 {
                                if let Some(freq) = SamplingFrequency::from_capability_bit(bit) {
                                    caps.sampling_frequencies.push(freq);
                                }
                            }
                        }
                    }
                }
                0x02 => {
                    if let Some(&bits) = ltv.value.first() {
                        caps.supports_7_5ms = bits & 0x01 != 0;
                        caps.supports_10ms = bits & 0x02 != 0;
                    }
                }
                0x03 => {
                    if let Some(&bits) = ltv.value.first() {
                        for i in 0..8u8 {
                            if bits & (1 << i) != 0 {
                                caps.channel_counts.push(i + 1);
                            }
                        }
                    }
                }
                0x04 => {
                    if ltv.value.len() >= 4 {
                        caps.min_octets_per_frame =
                            Some(u16::from_le_bytes([ltv.value[0], ltv.value[1]]));
                        caps.max_octets_per_frame =
                            Some(u16::from_le_bytes([ltv.value[2], ltv.value[3]]));
                    }
                }
                0x05 => {
                    if let Some(&count) = ltv.value.first() {
                        caps.max_frames_per_sdu = count;
                    }
                }
                _ => {}
            }
        }

        // Omitted channel count means single channel only.
        if caps.channel_counts.is_empty() {
            caps.channel_counts.push(1);
        }
        caps
    }

    /// Whether this device can carry stereo on one stream, which lets us use a
    /// single CIS instead of two - fewer moving parts during setup.
    pub fn supports_stereo_in_one_stream(&self) -> bool {
        self.channel_counts.contains(&2) && self.max_frames_per_sdu >= 2
    }

    /// Checks a configuration against what the device actually advertised.
    pub fn accepts(&self, config: &CodecConfiguration) -> Result<(), ConfigRejection> {
        if !self.sampling_frequencies.contains(&config.sampling_frequency) {
            return Err(ConfigRejection::SamplingFrequency);
        }

        let duration_ok = match config.frame_duration {
            FrameDuration::Ms7_5 => self.supports_7_5ms,
            FrameDuration::Ms10 => self.supports_10ms,
        };
        if !duration_ok {
            return Err(ConfigRejection::FrameDuration);
        }

        if let (Some(min), Some(max)) = (self.min_octets_per_frame, self.max_octets_per_frame) {
            if config.octets_per_frame < min || config.octets_per_frame > max {
                return Err(ConfigRejection::OctetsOutOfRange { min, max });
            }
        }

        let channels = config.channel_allocation.count_ones() as u8;
        if channels > 0 && !self.channel_counts.contains(&channels) {
            return Err(ConfigRejection::ChannelCount);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigRejection {
    #[error("device does not support that sampling frequency")]
    SamplingFrequency,

    #[error("device does not support that frame duration")]
    FrameDuration,

    #[error("octets per frame must be between {min} and {max}")]
    OctetsOutOfRange { min: u16, max: u16 },

    #[error("device does not support that channel count")]
    ChannelCount,
}

/// One PAC record: a codec plus the configurations it accepts.
#[derive(Debug, Clone)]
pub struct PacRecord {
    pub coding_format: u8,
    pub company_id: u16,
    pub vendor_codec_id: u16,
    pub capabilities: CodecCapabilities,
    /// The capability bytes exactly as the device sent them.
    ///
    /// Kept so a conclusion drawn from the parse can be checked against the
    /// source. Every design decision here rests on what these records say, and
    /// a parser bug in them is invisible from anywhere else in the stack.
    pub raw: Vec<u8>,
}

impl PacRecord {
    pub const CODING_FORMAT_LC3: u8 = 0x06;

    pub fn is_lc3(&self) -> bool {
        self.coding_format == Self::CODING_FORMAT_LC3
    }

    /// Parses a Sink PAC or Source PAC characteristic value.
    pub fn parse_characteristic(value: &[u8]) -> Vec<PacRecord> {
        let mut records = Vec::new();
        if value.is_empty() {
            return records;
        }

        let count = value[0] as usize;
        let mut offset = 1;

        for _ in 0..count {
            if offset + 6 > value.len() {
                break;
            }

            let coding_format = value[offset];
            let company_id = u16::from_le_bytes([value[offset + 1], value[offset + 2]]);
            let vendor_codec_id = u16::from_le_bytes([value[offset + 3], value[offset + 4]]);
            let caps_len = value[offset + 5] as usize;
            offset += 6;

            if offset + caps_len > value.len() {
                break;
            }
            let raw = value[offset..offset + caps_len].to_vec();
            let capabilities = CodecCapabilities::parse(&raw);
            offset += caps_len;

            if offset >= value.len() {
                break;
            }
            let metadata_len = value[offset] as usize;
            offset += 1 + metadata_len;

            records.push(PacRecord {
                coding_format,
                company_id,
                vendor_codec_id,
                capabilities,
                raw,
            });

            if offset > value.len() {
                break;
            }
        }

        records
    }
}

/// A concrete LC3 stream configuration - what actually gets sent to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecConfiguration {
    pub sampling_frequency: SamplingFrequency,
    pub frame_duration: FrameDuration,
    pub channel_allocation: u32,
    /// The bitrate knob. Bitrate = octets * 8 / frame duration.
    pub octets_per_frame: u16,
    pub frames_per_sdu: u8,
}

impl CodecConfiguration {
    /// Bits per second for a single channel.
    pub fn bitrate_per_channel(&self) -> u32 {
        (self.octets_per_frame as u32 * 8 * 1_000_000) / self.frame_duration.microseconds()
    }

    pub fn channel_count(&self) -> u32 {
        self.channel_allocation.count_ones().max(1)
    }

    /// SDU size the CIG must carry: every channel's frame in one interval.
    pub fn sdu_size(&self) -> u16 {
        self.octets_per_frame * self.channel_count() as u16 * self.frames_per_sdu.max(1) as u16
    }

    /// Encodes the Codec Specific Configuration LTV block.
    pub fn encode(&self) -> Vec<u8> {
        let mut items = vec![
            Ltv::new(0x01, vec![self.sampling_frequency.0]),
            Ltv::new(0x02, vec![self.frame_duration.encoded()]),
            Ltv::new(0x03, self.channel_allocation.to_le_bytes().to_vec()),
            Ltv::new(0x04, self.octets_per_frame.to_le_bytes().to_vec()),
        ];

        if self.frames_per_sdu > 1 {
            items.push(Ltv::new(0x05, vec![self.frames_per_sdu]));
        }

        Ltv::encode(&items)
    }

    /// The BAP-named settings, so a preset can say "48_4" rather than raw numbers.
    pub fn preset_name(&self) -> Option<&'static str> {
        let hz = self.sampling_frequency.hz()?;
        let ms = self.frame_duration;
        let octets = self.octets_per_frame;

        Some(match (hz, ms, octets) {
            (16_000, FrameDuration::Ms10, 40) => "16_2",
            (24_000, FrameDuration::Ms10, 60) => "24_2",
            (32_000, FrameDuration::Ms10, 80) => "32_2",
            (48_000, FrameDuration::Ms7_5, 75) => "48_1",
            (48_000, FrameDuration::Ms10, 100) => "48_2",
            (48_000, FrameDuration::Ms7_5, 90) => "48_3",
            (48_000, FrameDuration::Ms10, 120) => "48_4",
            (48_000, FrameDuration::Ms7_5, 117) => "48_5",
            (48_000, FrameDuration::Ms10, 155) => "48_6",
            _ => return None,
        })
    }
}

/// Quality of service for the isochronous link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QosConfiguration {
    pub sdu_interval_us: u32,
    pub framing: u8,
    pub phy: u8,
    pub max_sdu: u16,
    pub retransmission_number: u8,
    pub max_transport_latency_ms: u16,
    pub presentation_delay_us: u32,
}

impl QosConfiguration {
    /// End-to-end audio delay this configuration implies, in milliseconds.
    ///
    /// The frame must be captured, sent, and held until the presentation instant,
    /// so latency is roughly the frame duration plus transport plus presentation.
    pub fn estimated_latency_ms(&self, codec: &CodecConfiguration) -> u32 {
        codec.frame_duration.microseconds() / 1000
            + self.max_transport_latency_ms as u32
            + self.presentation_delay_us / 1000
    }
}

/// Named starting points. "Default" mirrors what Windows typically negotiates,
/// so the stack begins from a known-good state rather than something exotic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// 48 kHz, 10 ms, 100 octets - 80 kbps per channel.
    WindowsDefault,
    /// 48 kHz, 7.5 ms, 75 octets - shortest frame, lowest delay.
    LowLatency,
    /// 48 kHz, 10 ms, 155 octets - 124 kbps per channel, the LC3 ceiling.
    HighQuality,
    /// 24 kHz, 10 ms, 60 octets - survives a weak link.
    Robust,
}

impl Preset {
    pub fn codec(&self, stereo_in_one_stream: bool) -> CodecConfiguration {
        let allocation = if stereo_in_one_stream {
            LOCATION_STEREO
        } else {
            LOCATION_FRONT_LEFT
        };
        let frames_per_sdu = 1;

        let (frequency, duration, octets) = match self {
            // Exactly what the Windows LE Audio driver asks for, read out of a
            // WPP trace of it streaming to these headphones:
            //
            //   SDU interval 0x00001D4C = 7500 us, Max SDU 90, sequential,
            //   unframed, 2M PHY, retransmissions 13, max latency 0x004B = 75 ms
            //
            // Not a guess and not a rounding of the BAP presets. The retransmission
            // count and the latency ceiling are the striking part: this stack was
            // asking for 2 retransmissions inside 20 ms, which leaves the
            // controller almost no room to schedule two channels - and it was the
            // second channel that kept failing to be established.
            Preset::WindowsDefault => (SamplingFrequency::HZ_48000, FrameDuration::Ms7_5, 90),
            Preset::LowLatency => (SamplingFrequency::HZ_48000, FrameDuration::Ms7_5, 75),
            Preset::HighQuality => (SamplingFrequency::HZ_48000, FrameDuration::Ms10, 155),
            Preset::Robust => (SamplingFrequency::HZ_24000, FrameDuration::Ms10, 60),
        };

        CodecConfiguration {
            sampling_frequency: frequency,
            frame_duration: duration,
            channel_allocation: allocation,
            octets_per_frame: octets,
            frames_per_sdu,
        }
    }

    pub fn qos(&self, codec: &CodecConfiguration) -> QosConfiguration {
        let (retransmissions, transport_latency, presentation_delay) = match self {
            Preset::WindowsDefault => (13, 75, 40_000),
            Preset::LowLatency => (2, 8, 20_000),
            Preset::HighQuality => (4, 40, 40_000),
            Preset::Robust => (5, 60, 40_000),
        };

        QosConfiguration {
            sdu_interval_us: codec.frame_duration.microseconds(),
            framing: 0x00, // unframed
            phy: 0x02,     // 2M
            max_sdu: codec.sdu_size(),
            retransmission_number: retransmissions,
            max_transport_latency_ms: transport_latency,
            presentation_delay_us: presentation_delay,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Preset::WindowsDefault => "Default (Windows-compatible)",
            Preset::LowLatency => "Nizka latence",
            Preset::HighQuality => "Vysoka kvalita",
            Preset::Robust => "Stabilni spojeni",
        }
    }
}

/// What the device says back about an ASE.
///
/// Everything so far has been us talking. The control point is a write, and a
/// write response only means the bytes arrived - not that the device accepted
/// what was in them. The device's actual answer arrives later, as a
/// notification, and a stack that never subscribes to those is configuring
/// blind: a rejected parameter looks exactly like a successful one.
pub mod ase {
    /// ASE states, in the order the state machine walks them.
    pub const STATE_IDLE: u8 = 0x00;
    pub const STATE_CODEC_CONFIGURED: u8 = 0x01;
    pub const STATE_QOS_CONFIGURED: u8 = 0x02;
    pub const STATE_ENABLING: u8 = 0x03;
    pub const STATE_STREAMING: u8 = 0x04;
    pub const STATE_DISABLING: u8 = 0x05;
    pub const STATE_RELEASING: u8 = 0x06;

    pub fn state_name(state: u8) -> &'static str {
        match state {
            STATE_IDLE => "Idle",
            STATE_CODEC_CONFIGURED => "Codec Configured",
            STATE_QOS_CONFIGURED => "QoS Configured",
            STATE_ENABLING => "Enabling",
            STATE_STREAMING => "Streaming",
            STATE_DISABLING => "Disabling",
            STATE_RELEASING => "Releasing",
            _ => "unknown state",
        }
    }

    /// One ASE's reported state.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AseState {
        pub ase_id: u8,
        pub state: u8,
    }

    /// What the device says it wants, sent with the Codec Configured state.
    ///
    /// This is the half of the conversation the stack has been ignoring. The
    /// device answers Config Codec by publishing the QoS it prefers - including
    /// the presentation delay range it can actually work in - and a host that
    /// never reads it is left guessing values that may simply be outside what
    /// the device supports. The device then accepts the configuration and never
    /// starts the stream, which looks like a routing fault rather than a
    /// parameter the headphones quietly could not meet.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PreferredQos {
        pub framing: u8,
        pub phy_preference: u8,
        pub retransmission_preference: u8,
        pub max_transport_latency_ms: u16,
        pub presentation_delay_min_us: u32,
        pub presentation_delay_max_us: u32,
        /// Zero when the device states no preference inside the supported range.
        pub preferred_delay_min_us: u32,
        pub preferred_delay_max_us: u32,
    }

    impl PreferredQos {
        /// A presentation delay the device will accept, preferring its own.
        pub fn choose_presentation_delay(&self, wanted_us: u32) -> u32 {
            // The device's own preference wins when it states one: it is the
            // value its firmware was tuned around.
            if self.preferred_delay_min_us > 0 {
                return self.preferred_delay_min_us;
            }

            wanted_us.clamp(self.presentation_delay_min_us, self.presentation_delay_max_us)
        }
    }

    /// Reads the parameters carried by a Codec Configured state notification.
    ///
    /// Layout after ASE_ID and State: Framing, PHY, RTN, Max_Transport_Latency
    /// (2), Presentation_Delay_Min (3), Presentation_Delay_Max (3),
    /// Preferred_Presentation_Delay_Min (3), Preferred_Presentation_Delay_Max
    /// (3), then the codec configuration.
    pub fn parse_preferred_qos(value: &[u8]) -> Option<PreferredQos> {
        let body = value.get(2..)?;
        // Framing, PHY, RTN, latency (2), then four three-byte delays: 17 bytes.
        if body.len() < 17 || value[1] != STATE_CODEC_CONFIGURED {
            return None;
        }

        let u24 = |at: usize| {
            u32::from_le_bytes([body[at], body[at + 1], body[at + 2], 0])
        };

        Some(PreferredQos {
            framing: body[0],
            phy_preference: body[1],
            retransmission_preference: body[2],
            max_transport_latency_ms: u16::from_le_bytes([body[3], body[4]]),
            presentation_delay_min_us: u24(5),
            presentation_delay_max_us: u24(8),
            preferred_delay_min_us: u24(11),
            preferred_delay_max_us: u24(14),
        })
    }

    /// The codec configuration the device actually applied.
    ///
    /// Read back rather than assumed. A device answers Config Codec by
    /// publishing what it settled on, and it does not have to be what was
    /// asked for - it may clamp a value, ignore one, or keep a previous
    /// configuration. Nothing else in the stack can tell the difference, which
    /// is why changing the frame length or the bitrate can leave the sound
    /// identical: the request changed and the stream did not.
    ///
    /// Sits after the QoS preferences: Codec_ID (5), configuration length (1),
    /// then the same LTVs that went out in Config Codec.
    pub fn parse_configured_codec(value: &[u8]) -> Option<super::CodecConfiguration> {
        use super::{CodecConfiguration, FrameDuration, Ltv, SamplingFrequency};

        let body = value.get(2..)?;
        if value.get(1) != Some(&STATE_CODEC_CONFIGURED) || body.len() < 17 + 6 {
            return None;
        }

        let length = body[17 + 5] as usize;
        let ltvs = body.get(17 + 6..17 + 6 + length)?;

        let mut config = CodecConfiguration {
            sampling_frequency: SamplingFrequency(0),
            frame_duration: FrameDuration::Ms10,
            channel_allocation: 0,
            octets_per_frame: 0,
            frames_per_sdu: 1,
        };

        for ltv in Ltv::decode(ltvs) {
            match ltv.kind {
                0x01 => {
                    if let Some(&code) = ltv.value.first() {
                        config.sampling_frequency = SamplingFrequency(code);
                    }
                }
                0x02 => {
                    if let Some(&code) = ltv.value.first() {
                        config.frame_duration =
                            if code == 0x00 { FrameDuration::Ms7_5 } else { FrameDuration::Ms10 };
                    }
                }
                0x03 => {
                    if ltv.value.len() >= 4 {
                        config.channel_allocation = u32::from_le_bytes([
                            ltv.value[0],
                            ltv.value[1],
                            ltv.value[2],
                            ltv.value[3],
                        ]);
                    }
                }
                0x04 => {
                    if ltv.value.len() >= 2 {
                        config.octets_per_frame =
                            u16::from_le_bytes([ltv.value[0], ltv.value[1]]);
                    }
                }
                0x05 => {
                    if let Some(&count) = ltv.value.first() {
                        config.frames_per_sdu = count;
                    }
                }
                _ => {}
            }
        }

        Some(config)
    }

    /// Reads a Sink ASE characteristic value or notification.
    pub fn parse_state(value: &[u8]) -> Option<AseState> {
        let &[ase_id, state, ..] = value else {
            return None;
        };
        Some(AseState { ase_id, state })
    }

    /// How the device answered one operation on the control point.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AseResponse {
        pub ase_id: u8,
        pub response_code: u8,
        pub reason: u8,
    }

    impl AseResponse {
        pub fn accepted(&self) -> bool {
            self.response_code == 0x00
        }

        /// What went wrong, in words, including which parameter was at fault.
        pub fn explain(&self) -> String {
            if self.accepted() {
                return "accepted".into();
            }

            let what = match self.response_code {
                0x01 => "unsupported opcode",
                0x02 => "invalid length",
                0x03 => "invalid ASE ID",
                0x04 => "invalid state transition",
                0x05 => "invalid ASE direction",
                0x06 => "unsupported codec capability",
                0x07 => "unsupported parameter value",
                0x08 => "rejected parameter value",
                0x09 => "invalid parameter value",
                0x0A => "unsupported metadata",
                0x0B => "rejected metadata",
                0x0C => "invalid metadata",
                0x0D => "insufficient resources",
                0x0E => "unspecified error",
                _ => "unknown response",
            };

            // The reason names the offending parameter, which is the difference
            // between "it said no" and knowing what to change - so getting this
            // table wrong is worse than having none at all. It was wrong: the
            // codes were shifted, and the field was read as if it always meant
            // the same thing. It does not. ASCS reuses the Reason byte for two
            // different tables, and which one applies is decided by the response
            // code above it. Both tables and the choice between them are taken
            // from Android's client_parser.cc, which is what these devices are
            // tested against.
            //
            // The old table turned a complaint about the SDU interval into the
            // words "sample rate", which is a different parameter set by a
            // different part of the stack.
            let configuration_reason = |reason: u8| match reason {
                0x01 => " (Codec ID)",
                0x02 => " (codec specific configuration)",
                0x03 => " (SDU interval)",
                0x04 => " (framing)",
                0x05 => " (PHY)",
                0x06 => " (maximum SDU size)",
                0x07 => " (retransmission number)",
                0x08 => " (max transport latency)",
                0x09 => " (presentation delay)",
                0x0A => " (invalid ASE CIS mapping)",
                _ => "",
            };

            let metadata_reason = |reason: u8| match reason {
                0x01 => " (preferred audio contexts)",
                0x02 => " (streaming audio contexts)",
                0x03 => " (program info)",
                0x04 => " (language)",
                0x05 => " (CCID list)",
                0x06 => " (parental rating)",
                0x07 => " (program info URI)",
                0xFE => " (extended metadata)",
                0xFF => " (vendor specific)",
                _ => "",
            };

            let which = match self.response_code {
                // Unsupported, Rejected and Invalid Configuration Parameter Value.
                0x07 | 0x08 | 0x09 => configuration_reason(self.reason),
                // Unsupported, Rejected and Invalid Metadata.
                0x0A | 0x0B | 0x0C => metadata_reason(self.reason),
                // Every other response code leaves Reason unused, so naming a
                // parameter here would be inventing one.
                _ => "",
            };

            format!("{what}{which}")
        }
    }

    /// The QoS the device confirms it applied, reported with the QoS
    /// Configured state.
    ///
    /// Worth reading rather than assuming. Config QoS is a request, and the
    /// answer carries the values the device settled on - including which CIS it
    /// bound the endpoint to. A host that never reads this cannot tell a
    /// configuration that was applied from one that was quietly clamped, and it
    /// cannot tell which earpiece is on which CIS except by guessing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct QosConfiguredState {
        pub cig_id: u8,
        pub cis_id: u8,
        pub sdu_interval_us: u32,
        pub framing: u8,
        pub phy: u8,
        pub max_sdu: u16,
        pub retransmission_number: u8,
        pub max_transport_latency_ms: u16,
        pub presentation_delay_us: u32,
    }

    /// Field order and lengths follow Android's
    /// `ParseAseStatusQosConfiguredStateParams`.
    pub fn parse_qos_configured(value: &[u8]) -> Option<QosConfiguredState> {
        if value.get(1) != Some(&STATE_QOS_CONFIGURED) {
            return None;
        }
        let body = value.get(2..)?;

        // CIG, CIS, SDU interval (3), framing, PHY, max SDU (2), RTN,
        // latency (2), presentation delay (3).
        const LEN: usize = 15;
        if body.len() != LEN {
            return None;
        }

        let u24 = |at: usize| u32::from_le_bytes([body[at], body[at + 1], body[at + 2], 0]);

        Some(QosConfiguredState {
            cig_id: body[0],
            cis_id: body[1],
            sdu_interval_us: u24(2),
            framing: body[5],
            phy: body[6],
            max_sdu: u16::from_le_bytes([body[7], body[8]]),
            retransmission_number: body[9],
            max_transport_latency_ms: u16::from_le_bytes([body[10], body[11]]),
            presentation_delay_us: u24(12),
        })
    }

    /// What Enabling, Streaming and Disabling carry.
    ///
    /// The CIS identifier here is the device's own answer to which stream feeds
    /// which endpoint. With two CIS in play that is the only readable statement
    /// of the left/right mapping; everything else is inference.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TransientState {
        pub cig_id: u8,
        pub cis_id: u8,
        pub metadata: Vec<u8>,
    }

    /// Field order and lengths follow Android's
    /// `ParseAseStatusTransientStateParams`.
    pub fn parse_transient(value: &[u8]) -> Option<TransientState> {
        match value.get(1) {
            Some(&STATE_ENABLING) | Some(&STATE_STREAMING) | Some(&STATE_DISABLING) => {}
            _ => return None,
        }
        let body = value.get(2..)?;

        // CIG, CIS, metadata length, then exactly that much metadata. Android
        // rejects a length that does not match rather than taking what fits,
        // and so does this: a mismatch means the notification was truncated,
        // and truncated metadata read as if complete is how a stream ends up
        // configured for the wrong audio context.
        let &[cig_id, cis_id, metadata_len, ref metadata @ ..] = body else {
            return None;
        };
        if metadata.len() != metadata_len as usize {
            return None;
        }

        Some(TransientState {
            cig_id,
            cis_id,
            metadata: metadata.to_vec(),
        })
    }

    /// Reads a notification from the ASE Control Point.
    pub fn parse_control_point_response(value: &[u8]) -> Option<(u8, Vec<AseResponse>)> {
        let &[opcode, count, ref rest @ ..] = value else {
            return None;
        };

        let responses = rest
            .chunks_exact(3)
            .take(count as usize)
            .map(|c| AseResponse { ase_id: c[0], response_code: c[1], reason: c[2] })
            .collect();

        Some((opcode, responses))
    }
}

// ---- ASCS control point operations ----

pub mod ascs {
    use super::{CodecConfiguration, QosConfiguration};

    pub const OP_CONFIG_CODEC: u8 = 0x01;
    pub const OP_CONFIG_QOS: u8 = 0x02;
    pub const OP_ENABLE: u8 = 0x03;
    pub const OP_RECEIVER_START_READY: u8 = 0x04;
    pub const OP_DISABLE: u8 = 0x05;
    pub const OP_RELEASE: u8 = 0x08;

    /// Target latency hint sent with Config Codec.
    pub const LATENCY_LOW: u8 = 0x01;
    pub const LATENCY_BALANCED: u8 = 0x02;
    pub const LATENCY_HIGH_RELIABILITY: u8 = 0x03;

    /// Combines operations with the same opcode into one ASCS Control Point PDU.
    ///
    /// ASCS operations carry a Number_of_ASEs byte precisely so a stereo pair
    /// can be changed atomically. Windows uses that form for these headphones.
    /// Sending one write per ASE lets the first endpoint advance while the
    /// second is still in the previous state, which some firmware resolves by
    /// rendering the one live stream into both ears.
    pub fn batch(operations: &[Vec<u8>]) -> Vec<u8> {
        let Some(first) = operations.first() else {
            return Vec::new();
        };

        let opcode = first[0];
        let count: usize = operations
            .iter()
            .filter(|operation| operation.first() == Some(&opcode))
            .map(|operation| operation.get(1).copied().unwrap_or(0) as usize)
            .sum();

        let mut pdu = vec![opcode, count.min(u8::MAX as usize) as u8];
        for operation in operations {
            if operation.first() == Some(&opcode) && operation.len() >= 2 {
                pdu.extend_from_slice(&operation[2..]);
            }
        }
        pdu
    }

    /// Builds Config Codec: the operation that sets LC3 bitrate and sample rate.
    pub fn config_codec(ase_id: u8, target_latency: u8, config: &CodecConfiguration) -> Vec<u8> {
        config_codec_on_phy(ase_id, target_latency, 0x02, config)
    }

    pub fn config_codec_on_phy(ase_id: u8, target_latency: u8, target_phy: u8, config: &CodecConfiguration) -> Vec<u8> {
        let specific = config.encode();
        let mut pdu = vec![OP_CONFIG_CODEC, 0x01, ase_id, target_latency, target_phy];
        // Codec ID: LC3, no vendor fields.
        pdu.extend_from_slice(&[0x06, 0x00, 0x00, 0x00, 0x00]);
        pdu.push(specific.len() as u8);
        pdu.extend_from_slice(&specific);
        pdu
    }

    /// Builds Config QoS: retransmissions, transport latency, presentation delay.
    pub fn config_qos(ase_id: u8, cig_id: u8, cis_id: u8, qos: &QosConfiguration) -> Vec<u8> {
        let mut pdu = vec![OP_CONFIG_QOS, 0x01, ase_id, cig_id, cis_id];
        pdu.extend_from_slice(&qos.sdu_interval_us.to_le_bytes()[0..3]);
        pdu.push(qos.framing);
        pdu.push(qos.phy);
        pdu.extend_from_slice(&qos.max_sdu.to_le_bytes());
        pdu.push(qos.retransmission_number);
        pdu.extend_from_slice(&qos.max_transport_latency_ms.to_le_bytes());
        pdu.extend_from_slice(&qos.presentation_delay_us.to_le_bytes()[0..3]);
        pdu
    }

    /// Builds Enable with the streaming context metadata.
    pub fn enable(ase_id: u8, context: u16) -> Vec<u8> {
        // Metadata: one LTV, Streaming_Audio_Contexts.
        let metadata = [0x03u8, 0x02, context.to_le_bytes()[0], context.to_le_bytes()[1]];
        let mut pdu = vec![OP_ENABLE, 0x01, ase_id, metadata.len() as u8];
        pdu.extend_from_slice(&metadata);
        pdu
    }

    /// Tells a Source ASE that this client is ready to receive its audio.
    ///
    /// This must not be sent for the Sink ASEs used for playback: there the
    /// client is the Audio Source and the server in the headphones performs the
    /// transition autonomously.
    pub fn receiver_start_ready(ase_id: u8) -> Vec<u8> {
        vec![OP_RECEIVER_START_READY, 0x01, ase_id]
    }

    pub fn release(ase_id: u8) -> Vec<u8> {
        vec![OP_RELEASE, 0x01, ase_id]
    }

    /// Audio context values used in Enable metadata.
    pub const CONTEXT_MEDIA: u16 = 0x0004;
    pub const CONTEXT_GAME: u16 = 0x0008;
    pub const CONTEXT_CONVERSATIONAL: u16 = 0x0002;
}

#[cfg(test)]
mod tests {
    /// The read-back that would have shown the request and the stream differing.
    #[test]
    fn the_configuration_the_device_applied_is_read_back() {
        use super::ase::*;

        let mut value = vec![0x01, STATE_CODEC_CONFIGURED, 0x00, 0x02, 0x0D, 0x4B, 0x00];
        value.extend_from_slice(&[0x40, 0x9C, 0x00]); // delay min 40000
        value.extend_from_slice(&[0x40, 0x9C, 0x00]); // delay max
        value.extend_from_slice(&[0x40, 0x9C, 0x00]); // preferred min
        value.extend_from_slice(&[0x40, 0x9C, 0x00]); // preferred max
        value.extend_from_slice(&[0x06, 0x00, 0x00, 0x00, 0x00]); // LC3

        // The device says it applied 48 kHz, 7.5 ms, right ear, 90 octets.
        let ltvs: Vec<u8> = vec![
            0x02, 0x01, 0x08,
            0x02, 0x02, 0x00,
            0x05, 0x03, 0x02, 0x00, 0x00, 0x00,
            0x03, 0x04, 0x5A, 0x00,
        ];
        value.push(ltvs.len() as u8);
        value.extend_from_slice(&ltvs);

        let applied = parse_configured_codec(&value).unwrap();

        assert_eq!(applied.sampling_frequency, SamplingFrequency::HZ_48000);
        assert_eq!(applied.frame_duration, FrameDuration::Ms7_5);
        assert_eq!(applied.channel_allocation, LOCATION_FRONT_RIGHT);
        assert_eq!(applied.octets_per_frame, 90);
    }

    #[test]
    fn stereo_control_operations_are_batched_atomically() {
        let config = Preset::WindowsDefault.codec(false);
        let left = ascs::config_codec(1, ascs::LATENCY_BALANCED, &config);
        let right = ascs::config_codec(2, ascs::LATENCY_BALANCED, &config);
        let batched = ascs::batch(&[left.clone(), right.clone()]);

        assert_eq!(batched[0], ascs::OP_CONFIG_CODEC);
        assert_eq!(batched[1], 2);
        assert_eq!(batched.len(), left.len() + right.len() - 2);
        assert_eq!(batched[2], 1, "first ASE");
        assert_eq!(batched[2 + left.len() - 2], 2, "second ASE");
    }

    #[test]
    fn a_state_without_a_configuration_reads_back_as_nothing() {
        use super::ase::*;

        assert_eq!(parse_configured_codec(&[0x01, STATE_STREAMING, 0x00]), None);
        assert_eq!(parse_configured_codec(&[0x01, STATE_CODEC_CONFIGURED, 0x00]), None);
    }

    #[test]
    fn the_devices_preferred_qos_is_read_from_the_codec_configured_state() {
        use super::ase::*;

        // ASE 1, Codec Configured, unframed, 2M, RTN 2, latency 100 ms,
        // presentation delay 20000-40000 us, preferred 30000-30000 us.
        let mut value = vec![0x01, STATE_CODEC_CONFIGURED, 0x00, 0x02, 0x02, 0x64, 0x00];
        value.extend_from_slice(&[0x20, 0x4E, 0x00]); // 20000
        value.extend_from_slice(&[0x40, 0x9C, 0x00]); // 40000
        value.extend_from_slice(&[0x30, 0x75, 0x00]); // 30000
        value.extend_from_slice(&[0x30, 0x75, 0x00]); // 30000

        let qos = parse_preferred_qos(&value).unwrap();

        assert_eq!(qos.max_transport_latency_ms, 100);
        assert_eq!(qos.presentation_delay_min_us, 20_000);
        assert_eq!(qos.presentation_delay_max_us, 40_000);
        assert_eq!(qos.preferred_delay_min_us, 30_000);

        // Our 40 ms is inside the range, but the device stated a preference.
        assert_eq!(qos.choose_presentation_delay(40_000), 30_000);
    }

    #[test]
    fn without_a_preference_our_value_is_clamped_into_range() {
        use super::ase::*;

        let mut value = vec![0x01, STATE_CODEC_CONFIGURED, 0x00, 0x02, 0x02, 0x64, 0x00];
        value.extend_from_slice(&[0x20, 0x4E, 0x00]); // min 20000
        value.extend_from_slice(&[0xB8, 0x88, 0x00]); // max 35000
        value.extend_from_slice(&[0x00, 0x00, 0x00]); // no preference
        value.extend_from_slice(&[0x00, 0x00, 0x00]);

        let qos = parse_preferred_qos(&value).unwrap();

        assert_eq!(qos.choose_presentation_delay(40_000), 35_000, "clamped down");
        assert_eq!(qos.choose_presentation_delay(10_000), 20_000, "clamped up");
        assert_eq!(qos.choose_presentation_delay(30_000), 30_000, "left alone");
    }

    #[test]
    fn a_state_that_is_not_codec_configured_carries_no_qos() {
        use super::ase::*;

        assert_eq!(parse_preferred_qos(&[0x01, STATE_STREAMING, 0x00]), None);
        assert_eq!(parse_preferred_qos(&[0x01, STATE_CODEC_CONFIGURED]), None);
    }

    /// Locks the preset to the values read out of the Windows driver's own
    /// trace. If someone changes them, the test says what they were and where
    /// they came from, rather than leaving the next person to rediscover it.
    #[test]
    fn the_windows_preset_matches_the_captured_windows_driver() {
        let codec = Preset::WindowsDefault.codec(false);
        let qos = Preset::WindowsDefault.qos(&codec);

        assert_eq!(codec.sampling_frequency, SamplingFrequency::HZ_48000);
        assert_eq!(codec.frame_duration, FrameDuration::Ms7_5);
        assert_eq!(codec.octets_per_frame, 90);

        // SDU interval 0x00001D4C in the trace.
        assert_eq!(qos.sdu_interval_us, 7_500);
        // Max Transport Latency 0x004B, Max Retransmission 13.
        assert_eq!(qos.max_transport_latency_ms, 75);
        assert_eq!(qos.retransmission_number, 13);
        assert_eq!(qos.phy, 0x02, "2M, as the trace shows");
        assert_eq!(qos.framing, 0, "unframed");
    }

    #[test]
    fn a_typed_in_rate_maps_to_its_code_or_to_nothing() {
        assert_eq!(SamplingFrequency::from_hz(48_000), Some(SamplingFrequency::HZ_48000));
        assert_eq!(SamplingFrequency::from_hz(16_000), Some(SamplingFrequency::HZ_16000));

        // Round trip through the pair, for every rate a person might enter.
        for hz in [8_000, 16_000, 24_000, 32_000, 44_100, 48_000] {
            assert_eq!(SamplingFrequency::from_hz(hz).unwrap().hz(), Some(hz));
        }

        // Not a rate BAP defines: no code exists, so no code is invented.
        assert_eq!(SamplingFrequency::from_hz(37_000), None);
    }

    /// A rejection names the parameter the device objected to.
    ///
    /// This test used to expect reason 0x05 to mean "channel allocation". It
    /// does not. ASCS defines 0x05 as the PHY, and the value that means channel
    /// allocation lives in a different table altogether - the LTV types inside
    /// the codec specific configuration, which is a payload, not a reason code.
    /// The two had been merged into one list offset by two places, so a
    /// rejection was reported as the parameter two entries above the real one.
    ///
    /// That matters beyond tidiness: this expectation is the one that was read
    /// as explaining a mono stream. It was naming the wrong parameter, so
    /// whatever it explained, it was not that.
    #[test]
    fn a_rejection_names_the_parameter_ascs_actually_means() {
        use super::ase::*;

        // Config Codec, one ASE, rejected. Response 0x08 is "Rejected
        // Configuration Parameter Value", so the reason byte is read against
        // the configuration table, where 0x05 is the PHY.
        let (opcode, responses) =
            parse_control_point_response(&[0x01, 0x01, 0x02, 0x08, 0x05]).unwrap();

        assert_eq!(opcode, 0x01);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].ase_id, 0x02);
        assert!(!responses[0].accepted());
        assert_eq!(responses[0].explain(), "rejected parameter value (PHY)");
    }

    #[test]
    fn an_accepted_operation_says_so_plainly() {
        use super::ase::*;

        let (_, responses) =
            parse_control_point_response(&[0x03, 0x02, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00]).unwrap();

        assert_eq!(responses.len(), 2);
        assert!(responses.iter().all(|r| r.accepted()));
        assert_eq!(responses[0].explain(), "accepted");
    }

    #[test]
    fn ase_states_are_read_and_named() {
        use super::ase::*;

        let state = parse_state(&[0x01, STATE_STREAMING, 0xAA, 0xBB]).unwrap();

        assert_eq!(state.ase_id, 1);
        assert_eq!(state_name(state.state), "Streaming");
        assert_eq!(state_name(STATE_ENABLING), "Enabling");
        assert_eq!(parse_state(&[0x01]), None);
    }

    use super::*;

    /// Sink PAC as a headset typically reports it: LC3, 16-48 kHz, both frame
    /// durations, stereo capable, 26-155 octets.
    fn sample_sink_pac() -> Vec<u8> {
        let caps = Ltv::encode(&[
            Ltv::new(0x01, 0x00E4u16.to_le_bytes().to_vec()), // 16, 24, 32, 48 kHz
            Ltv::new(0x02, vec![0x03]),                       // 7.5 and 10 ms
            Ltv::new(0x03, vec![0x03]),                       // 1 or 2 channels
            Ltv::new(0x04, [26u16.to_le_bytes(), 155u16.to_le_bytes()].concat()),
            Ltv::new(0x05, vec![2]),
        ]);

        let mut record = vec![1u8]; // one record
        record.extend_from_slice(&[0x06, 0x00, 0x00, 0x00, 0x00]); // LC3
        record.push(caps.len() as u8);
        record.extend_from_slice(&caps);
        record.push(0); // no metadata
        record
    }

    #[test]
    fn pac_record_decodes_to_capabilities() {
        let records = PacRecord::parse_characteristic(&sample_sink_pac());
        assert_eq!(records.len(), 1);

        let record = &records[0];
        assert!(record.is_lc3());

        let caps = &record.capabilities;
        assert!(caps.sampling_frequencies.contains(&SamplingFrequency::HZ_48000));
        assert!(caps.sampling_frequencies.contains(&SamplingFrequency::HZ_16000));
        assert!(caps.supports_7_5ms && caps.supports_10ms);
        assert_eq!(caps.min_octets_per_frame, Some(26));
        assert_eq!(caps.max_octets_per_frame, Some(155));
        assert!(caps.supports_stereo_in_one_stream(), "one CIS should be possible");
    }

    #[test]
    fn bitrate_matches_the_named_presets() {
        // What Windows actually sends: 90 octets at 7.5 ms is 96 kbps. It is
        // not one of the named BAP presets, which is itself worth knowing.
        let config = Preset::WindowsDefault.codec(false);
        assert_eq!(config.bitrate_per_channel(), 96_000);

        // 48_1: 75 octets at 7.5 ms is also 80 kbps, but shorter frames.
        let low = Preset::LowLatency.codec(false);
        assert_eq!(low.bitrate_per_channel(), 80_000);
        assert_eq!(low.preset_name(), Some("48_1"));

        // 48_6: the LC3 ceiling.
        let high = Preset::HighQuality.codec(false);
        assert_eq!(high.bitrate_per_channel(), 124_000);
        assert_eq!(high.preset_name(), Some("48_6"));
    }

    #[test]
    fn capabilities_reject_configurations_the_device_cannot_do() {
        let caps = PacRecord::parse_characteristic(&sample_sink_pac())
            .remove(0)
            .capabilities;

        assert!(caps.accepts(&Preset::WindowsDefault.codec(false)).is_ok());
        assert!(caps.accepts(&Preset::HighQuality.codec(false)).is_ok());

        // Above the advertised maximum.
        let mut greedy = Preset::HighQuality.codec(false);
        greedy.octets_per_frame = 200;
        assert_eq!(
            caps.accepts(&greedy),
            Err(ConfigRejection::OctetsOutOfRange { min: 26, max: 155 })
        );

        // A rate the device never advertised.
        let mut exotic = Preset::WindowsDefault.codec(false);
        exotic.sampling_frequency = SamplingFrequency(0x0A); // 96 kHz
        assert_eq!(caps.accepts(&exotic), Err(ConfigRejection::SamplingFrequency));
    }

    #[test]
    fn stereo_on_one_stream_doubles_the_sdu() {
        let mono = Preset::WindowsDefault.codec(false);
        let stereo = Preset::WindowsDefault.codec(true);

        assert_eq!(mono.sdu_size(), 90);
        // One block containing two channels; channels must not be counted twice.
        assert_eq!(stereo.channel_count(), 2);
        assert_eq!(stereo.sdu_size(), 180);
    }

    #[test]
    fn config_codec_carries_the_octet_count() {
        let config = Preset::HighQuality.codec(false);
        let pdu = ascs::config_codec(0x01, ascs::LATENCY_BALANCED, &config);

        assert_eq!(pdu[0], ascs::OP_CONFIG_CODEC);
        assert_eq!(pdu[1], 0x01); // one ASE
        assert_eq!(pdu[2], 0x01); // ASE id
        assert_eq!(pdu[5], 0x06); // LC3

        // The configuration LTVs must round-trip.
        let specific_len = pdu[10] as usize;
        let decoded = Ltv::decode(&pdu[11..11 + specific_len]);

        let octets = decoded.iter().find(|l| l.kind == 0x04).expect("octets LTV present");
        assert_eq!(u16::from_le_bytes([octets.value[0], octets.value[1]]), 155);

        let frequency = decoded.iter().find(|l| l.kind == 0x01).unwrap();
        assert_eq!(frequency.value[0], SamplingFrequency::HZ_48000.0);
    }

    #[test]
    fn qos_encodes_24_bit_fields_correctly() {
        let codec = Preset::LowLatency.codec(false);
        let qos = Preset::LowLatency.qos(&codec);
        let pdu = ascs::config_qos(0x01, 0x00, 0x00, &qos);

        assert_eq!(pdu[0], ascs::OP_CONFIG_QOS);
        // SDU interval is 7500 us across three bytes.
        let interval = u32::from_le_bytes([pdu[5], pdu[6], pdu[7], 0]);
        assert_eq!(interval, 7_500);

        assert_eq!(pdu[9], 0x02); // 2M PHY
        assert_eq!(u16::from_le_bytes([pdu[10], pdu[11]]), 75); // max SDU
        assert_eq!(pdu[12], 2); // retransmissions

        // Presentation delay, also 24-bit.
        let delay = u32::from_le_bytes([pdu[15], pdu[16], pdu[17], 0]);
        assert_eq!(delay, 20_000);
    }

    #[test]
    fn latency_estimate_stays_under_the_target() {
        let codec = Preset::LowLatency.codec(false);
        let qos = Preset::LowLatency.qos(&codec);

        // 7.5 ms frame + 8 ms transport + 20 ms presentation.
        let latency = qos.estimated_latency_ms(&codec);
        assert_eq!(latency, 35);
        assert!(latency < 50, "low latency preset must stay well under 50 ms");
    }

    #[test]
    fn truncated_pac_does_not_panic() {
        assert!(PacRecord::parse_characteristic(&[]).is_empty());
        assert!(PacRecord::parse_characteristic(&[0x01]).is_empty());
        // Claims two records, carries one.
        let mut truncated = sample_sink_pac();
        truncated[0] = 2;
        assert_eq!(PacRecord::parse_characteristic(&truncated).len(), 1);
    }

    #[test]
    fn ltv_round_trips() {
        let items = vec![Ltv::new(0x01, vec![0x08]), Ltv::new(0x04, vec![0x64, 0x00])];
        let encoded = Ltv::encode(&items);
        assert_eq!(Ltv::decode(&encoded), items);

        // A length byte claiming more than the buffer holds stops the walk.
        assert!(Ltv::decode(&[0x20, 0x01, 0x02]).is_empty());
    }
}


#[cfg(test)]
mod google_agreement_tests {
    //! Checks that what goes on the wire matches Android's `client_parser.cc`.
    //!
    //! Android is what every LE Audio headset on the market is tested against,
    //! so its byte layout is the one devices are known to accept. These tests
    //! exist so the agreement cannot drift silently: a change to the encoders
    //! that moves a field breaks a test here rather than a stranger's headphones.

    use super::ase;
    use super::ascs;
    use super::{CodecConfiguration, FrameDuration, QosConfiguration, SamplingFrequency};

    fn a_codec_configuration() -> CodecConfiguration {
        CodecConfiguration {
            sampling_frequency: SamplingFrequency(0x08), // 48 kHz
            frame_duration: FrameDuration::Ms7_5,
            channel_allocation: 0x0000_0001,
            octets_per_frame: 90,
            frames_per_sdu: 1,
        }
    }

    #[test]
    fn config_codec_lays_out_its_header_the_way_android_does() {
        let pdu = ascs::config_codec(1, ascs::LATENCY_BALANCED, &a_codec_configuration());

        // PrepareAseCtpCodecConfig writes: opcode, number of ASEs, then per ASE
        // the id, target latency, target PHY, and a five byte Codec ID made of
        // the coding format plus two 16-bit vendor fields. LC3 is coding format
        // 0x06 with both vendor fields zero.
        assert_eq!(pdu[0], ascs::OP_CONFIG_CODEC);
        assert_eq!(pdu[1], 1, "number of ASEs");
        assert_eq!(pdu[2], 1, "ASE id");
        assert_eq!(pdu[3], ascs::LATENCY_BALANCED);
        assert_eq!(pdu[4], 0x02, "target PHY: 2M");
        assert_eq!(&pdu[5..10], &[0x06, 0x00, 0x00, 0x00, 0x00], "Codec ID");

        // Then a length byte covering exactly the codec specific configuration.
        let length = pdu[10] as usize;
        assert_eq!(pdu.len(), 11 + length, "length byte must cover the remainder");
    }

    #[test]
    fn config_qos_lays_out_its_fields_the_way_android_does() {
        let qos = QosConfiguration {
            sdu_interval_us: 7_500,
            framing: 0,
            phy: 0x02,
            max_sdu: 90,
            retransmission_number: 13,
            max_transport_latency_ms: 75,
            presentation_delay_us: 40_000,
        };

        let pdu = ascs::config_qos(1, 0, 0, &qos);

        // PrepareAseCtpConfigQos: ase_id, cig, cis, sdu_interval (3), framing,
        // phy, max_sdu (2), retrans_nb, max_transport_latency (2),
        // pres_delay (3) - sixteen bytes per ASE, with opcode and count in
        // front of them.
        assert_eq!(pdu.len(), 2 + 16);
        assert_eq!(pdu[0], ascs::OP_CONFIG_QOS);
        assert_eq!(&pdu[2..5], &[1, 0, 0], "ASE id, CIG, CIS");
        assert_eq!(&pdu[5..8], &7_500u32.to_le_bytes()[..3], "SDU interval");
        assert_eq!(pdu[8], 0, "framing");
        assert_eq!(pdu[9], 0x02, "PHY");
        assert_eq!(&pdu[10..12], &90u16.to_le_bytes(), "max SDU");
        assert_eq!(pdu[12], 13, "retransmission number");
        assert_eq!(&pdu[13..15], &75u16.to_le_bytes(), "max transport latency");
        assert_eq!(&pdu[15..18], &40_000u32.to_le_bytes()[..3], "presentation delay");
    }

    #[test]
    fn enable_and_receiver_start_ready_match_android() {
        let pdu = ascs::enable(1, ascs::CONTEXT_MEDIA);
        // PrepareAseCtpEnable: opcode, count, then ase_id, metadata length,
        // metadata.
        assert_eq!(pdu[0], ascs::OP_ENABLE);
        assert_eq!(pdu[1], 1);
        assert_eq!(pdu[2], 1, "ASE id");
        assert_eq!(pdu[3] as usize, pdu.len() - 4, "metadata length");

        // PrepareAseCtpAudioReceiverStartReady: opcode, count, ids.
        assert_eq!(ascs::receiver_start_ready(5), vec![0x04, 0x01, 5]);
    }

    #[test]
    fn a_rejected_sdu_interval_is_named_as_one() {
        // This is the case the old table got wrong. Response code 0x07 is
        // "Unsupported Configuration Parameter Value" and reason 0x03 is the
        // SDU interval. The previous mapping called it "sample rate", which is
        // a parameter set somewhere else entirely.
        let response = ase::AseResponse {
            ase_id: 1,
            response_code: 0x07,
            reason: 0x03,
        };
        let explained = response.explain();
        assert!(explained.contains("SDU interval"), "got: {explained}");
        assert!(!explained.contains("sample rate"), "got: {explained}");
    }

    #[test]
    fn the_reason_byte_is_read_against_the_right_table() {
        // Same reason value, two response codes, two different meanings.
        let configuration = ase::AseResponse {
            ase_id: 1,
            response_code: 0x08, // Rejected Configuration Parameter Value
            reason: 0x02,
        };
        assert!(configuration.explain().contains("codec specific configuration"));

        let metadata = ase::AseResponse {
            ase_id: 1,
            response_code: 0x0B, // Rejected Metadata
            reason: 0x02,
        };
        assert!(metadata.explain().contains("streaming audio contexts"));

        // A response code that does not use the reason byte must not invent a
        // parameter for it.
        let transition = ase::AseResponse {
            ase_id: 1,
            response_code: 0x04, // Invalid ASE State Machine Transition
            reason: 0x02,
        };
        assert_eq!(transition.explain(), "invalid state transition");
    }

    #[test]
    fn the_qos_the_device_applied_is_read_back() {
        let mut value = vec![1u8, ase::STATE_QOS_CONFIGURED];
        value.extend_from_slice(&[0x00, 0x01]); // CIG 0, CIS 1
        value.extend_from_slice(&7_500u32.to_le_bytes()[..3]);
        value.push(0x00); // framing
        value.push(0x02); // PHY
        value.extend_from_slice(&90u16.to_le_bytes());
        value.push(13);
        value.extend_from_slice(&75u16.to_le_bytes());
        value.extend_from_slice(&40_000u32.to_le_bytes()[..3]);

        let parsed = ase::parse_qos_configured(&value).expect("well formed QoS state");
        assert_eq!(parsed.cis_id, 1);
        assert_eq!(parsed.sdu_interval_us, 7_500);
        assert_eq!(parsed.max_sdu, 90);
        assert_eq!(parsed.presentation_delay_us, 40_000);

        // Android rejects a length that does not match, and so does this.
        value.push(0xFF);
        assert!(ase::parse_qos_configured(&value).is_none());
    }

    #[test]
    fn a_streaming_notification_names_its_cis() {
        let value = vec![
            2,
            ase::STATE_STREAMING,
            0x00, // CIG
            0x01, // CIS
            0x04, // metadata length
            0x03,
            0x02,
            0x04,
            0x00,
        ];

        let parsed = ase::parse_transient(&value).expect("well formed streaming state");
        assert_eq!(parsed.cig_id, 0);
        assert_eq!(parsed.cis_id, 1);
        assert_eq!(parsed.metadata, vec![0x03, 0x02, 0x04, 0x00]);
    }

    #[test]
    fn truncated_metadata_is_refused_rather_than_half_read() {
        // The length byte claims four bytes and only two follow. Reading what
        // fits would hand the caller a metadata block that parses cleanly and
        // means something else.
        let value = vec![2, ase::STATE_ENABLING, 0x00, 0x01, 0x04, 0x03, 0x02];
        assert!(ase::parse_transient(&value).is_none());
    }
}
