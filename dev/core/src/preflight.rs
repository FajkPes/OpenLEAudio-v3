//! One planning path for both preflight and real setup. No radio I/O here.
use crate::settings::Settings;
use crate::stream::{StreamPlan, MicrophoneQuality};
use crate::link::AudioCapabilities;

pub fn plan(settings: &Settings, capabilities: &AudioCapabilities, prefer_single_cis: bool) -> Result<(StreamPlan,String),String> {
    let microphone_enabled=settings.get("microphone_mode").unwrap_or("off")=="on";
        let gaming = settings.get("audio_context").unwrap_or("media") == "game";
        let requested_context = if gaming {
            crate::bap::ascs::CONTEXT_GAME
        } else {
            crate::bap::ascs::CONTEXT_MEDIA
        };
        if let Some(supported) = capabilities.supported_contexts {
            if supported & requested_context == 0 {
                return Err(format!(
                    "selected {} mode is not supported by these headphones (supported contexts {supported:#06x})",
                    if gaming { "gaming" } else { "media" }
                ));
            }
        }

        // Anything that is not "custom" means Android's list, so the stored value
        // from a version 1 profile - "windows", "robust" and the rest - lands
        // there too rather than selecting a preset that no longer exists.
        let chosen = settings.get("preset").unwrap_or("google").to_string();
        let custom_codec = custom_codec(settings);
        let custom_qos = custom_qos(settings);

        let (plan, preset_label) = if chosen == "custom" {
            let mut plan = StreamPlan::build_custom(
                &capabilities,
                custom_codec,
                custom_qos,
                prefer_single_cis,
            )
            .map_err(|e| format!("device rejected the custom configuration: {e}"))?;

            plan.context = requested_context;
            plan.target_latency = if gaming {
                crate::bap::ascs::LATENCY_LOW
            } else {
                crate::bap::ascs::LATENCY_HIGH_RELIABILITY
            };

            (plan, format!("custom / {}", if gaming { "gaming" } else { "media" }))
        } else {
            // Everything that is not a configuration the user typed comes from
            // Android's Media priority list. There is no preset to fall back
            // to on purpose: the old ones described what the Windows driver
            // asks for, and mixing the two made it impossible to tell from a
            // log which stack had produced a given stream.
            let plan = if gaming {
                StreamPlan::build_google_game(&capabilities, prefer_single_cis, None)
            } else {
                StreamPlan::build_google(&capabilities, prefer_single_cis, None)
            }
            .map_err(|e| format!("stream could not be scheduled: {e}"))?;

            let choice = if gaming {
                StreamPlan::google_game_choice(&capabilities, prefer_single_cis)
            } else {
                StreamPlan::google_choice(&capabilities, prefer_single_cis)
            };
            let scenario = if gaming { "Gaming" } else { "Media" };
            let label = match choice {
                Some(chosen) if chosen.rank == 0 => {
                    format!("Google/Android {scenario} - LC3 {}", chosen.setting.name)
                }
                Some(chosen) => format!(
                    "Google/Android {scenario} - LC3 {} ({}. volba)",
                    chosen.setting.name,
                    chosen.rank + 1
                ),
                None => format!("Google/Android {scenario}"),
            };

            (plan, label)
        };

        // The radio settings apply whichever way the plan was built. A preset
        // chooses the codec; how hard the radio works to deliver it is a
        // separate decision and the user is allowed to make it.
        let mut plan = plan;

        // Exactly what Windows does, and nothing else. The experimental
        // alternatives - swapped ears, other ASE pairs, interleaved packing -
        // were removed once the trace showed what the real driver sends. Their
        // values lingered in saved settings and quietly configured ASE 1 as the
        // right ear and ASE 4 as the left, which made every later measurement
        // meaningless. A setting with no control is a setting nobody can see.

        // Only when the user asked for custom. A preset carries radio settings
        // that were measured, not guessed, and letting a stale saved value
        // override them makes a fix look like it did nothing.
        if chosen == "custom" {
            if let Some(phy) = settings.get("phy") {
                plan.qos.phy = if phy == "1M" { 0x01 } else { 0x02 };
            }
            if let Some(rtn) = settings.number("retransmissions") {
                plan.qos.retransmission_number = rtn as u8;
            }
        }


        // Mono is a choice, not only a fallback: two isochronous channels are
        // where these headphones are unreliable, and one that always works can
        // be worth more than stereo that sometimes does.
        let mode = settings.get("audio_mode").unwrap_or("stereo").to_string();
        let mut plan = match mode.as_str() {
            "mono" => {
                plan.into_mono()
            }
            _ => plan,
        };
        if microphone_enabled {
            let quality = match settings.get("microphone_quality").unwrap_or("balanced") {
                "voice" => MicrophoneQuality::Voice,
                "high" => MicrophoneQuality::High,
                _ => MicrophoneQuality::Balanced,
            };
            plan = plan
                .with_microphone(&capabilities, quality)
                .map_err(|error| format!("headset microphone could not be configured: {error}"))?;
        }

    validate(&plan)?;
    Ok((plan,preset_label))
}
pub fn validate(plan: &StreamPlan) -> Result<(),String> {
    for (codec,qos) in std::iter::once((&plan.codec,&plan.qos)).chain(plan.microphone.iter().map(|m|(&m.codec,&m.qos))) {
        crate::safety::check_stream_parameters(codec.octets_per_frame,qos.sdu_interval_us,qos.retransmission_number,qos.max_transport_latency_ms).map_err(str::to_string)?;
        if !matches!(qos.phy,1|2) {return Err("only 1M and 2M are supported".into());}
        if qos.max_sdu!=codec.sdu_size() {return Err("SDU size does not match the selected codec".into());}
        if qos.presentation_delay_us>0xffffff {return Err("presentation delay is outside the protocol range".into());}
    }
    let mut writes=crate::safety::WritePolicy::default();writes.allow_ase_control_point(1);
    for pdu in plan.ascs_sequence() {writes.check_write(1,&pdu).map_err(|e|e.to_string())?;}
    Ok(())
}
    /// The codec configuration the user typed in, with the defaults filled in.
    fn custom_codec(settings: &Settings) -> crate::bap::CodecConfiguration {
        use crate::bap::{CodecConfiguration, FrameDuration, SamplingFrequency};

        let rate = settings.number("rate_hz").unwrap_or(48_000.0) as u32;
        let frame = settings.number("frame_ms").unwrap_or(10.0);

        CodecConfiguration {
            sampling_frequency: SamplingFrequency::from_hz(rate)
                .unwrap_or(SamplingFrequency::HZ_48000),
            frame_duration: if frame < 8.75 { FrameDuration::Ms7_5 } else { FrameDuration::Ms10 },
            channel_allocation: crate::bap::LOCATION_FRONT_LEFT,
            octets_per_frame: settings.number("octets").unwrap_or(100.0) as u16,
            frames_per_sdu: 1,
        }
    }

    fn custom_qos(settings: &Settings) -> crate::bap::QosConfiguration {
        use crate::bap::QosConfiguration;

        let codec = custom_codec(settings);

        QosConfiguration {
            sdu_interval_us: codec.frame_duration.microseconds(),
            framing: 0,
            // 1M takes more airtime; actual range depends on both radios.
            phy: match settings.get("phy") {
                Some("1M") => 0x01,
                _ => 0x02,
            },
            max_sdu: codec.octets_per_frame,
            retransmission_number: settings.number("retransmissions").unwrap_or(2.0) as u8,
            max_transport_latency_ms: settings.number("max_latency_ms").unwrap_or(20.0) as u16,
            presentation_delay_us: (settings.number("presentation_delay_ms").unwrap_or(40.0)
                * 1000.0) as u32,
        }
    }

