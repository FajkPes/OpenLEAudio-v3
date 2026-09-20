//! Read-back validation for the ASCS transaction shared by automatic and custom audio.
use crate::bap::{ase, CodecConfiguration, QosConfiguration};

pub fn codec_response(
    id: u8,
    expected: &CodecConfiguration,
    value: &[u8],
) -> Result<ase::PreferredQos, String> {
    if value.first() != Some(&id) {
        return Err(format!("ASE {id}: response belongs to another endpoint"));
    }
    let actual = ase::parse_configured_codec(value)
        .ok_or_else(|| format!("ASE {id}: Codec Configured has not been confirmed"))?;
    // Only LC3 with the standard codec identifier is supported by our encoder.
    if value.get(19..24) != Some(&[6, 0, 0, 0, 0][..]) || actual != *expected {
        return Err(format!(
            "ASE {id}: codec differs from request; requested {expected:?}, received {actual:?}"
        ));
    }
    let qos = ase::parse_preferred_qos(value)
        .ok_or_else(|| format!("ASE {id}: missing QoS preferences"))?;
    if qos.presentation_delay_min_us > qos.presentation_delay_max_us {
        return Err(format!("ASE {id}: invalid presentation delay range"));
    }
    Ok(qos)
}

pub fn qos_response(
    id: u8,
    cig: u8,
    cis: u8,
    expected: &QosConfiguration,
    value: &[u8],
) -> Result<(), String> {
    let actual = ase::parse_qos_configured(value)
        .ok_or_else(|| format!("ASE {id}: QoS Configured has not been confirmed"))?;
    let expected = ase::QosConfiguredState {
        cig_id: cig,
        cis_id: cis,
        sdu_interval_us: expected.sdu_interval_us,
        framing: expected.framing,
        phy: expected.phy,
        max_sdu: expected.max_sdu,
        retransmission_number: expected.retransmission_number,
        max_transport_latency_ms: expected.max_transport_latency_ms,
        presentation_delay_us: expected.presentation_delay_us,
    };
    if value.first() != Some(&id) || actual != expected {
        return Err(format!(
            "ASE {id}: QoS/CIS mapping differs; requested {expected:?}, received {actual:?}"
        ));
    }
    Ok(())
}

pub fn streaming_response(id: u8, cig: u8, cis: u8, value: &[u8]) -> Result<(), String> {
    let actual = ase::parse_transient(value)
        .ok_or_else(|| format!("ASE {id}: no complete Streaming confirmation"))?;
    if value.first() != Some(&id)
        || value.get(1) != Some(&ase::STATE_STREAMING)
        || actual.cig_id != cig
        || actual.cis_id != cis
    {
        return Err(format!("ASE {id}: not Streaming on CIG {cig}, CIS {cis}"));
    }
    Ok(())
}

/// All endpoints must accept the same presentation delay. Max(per-endpoint
/// clamp) is insufficient: it can exceed another endpoint's upper bound.
pub fn presentation_delay(
    preferences: &[ase::PreferredQos],
    wanted: u32,
    prefer_device: bool,
) -> Result<u32, String> {
    if preferences.is_empty() {
        return Err("no confirmed ASE preferences".into());
    }
    let min = preferences
        .iter()
        .map(|p| p.presentation_delay_min_us)
        .max()
        .unwrap();
    let max = preferences
        .iter()
        .map(|p| p.presentation_delay_max_us)
        .min()
        .unwrap();
    if min > max {
        return Err("the channels have no common presentation delay".into());
    }
    let mut preferred_min = min;
    let mut preferred_max = max;
    for p in preferences {
        if p.preferred_delay_min_us != 0 && p.preferred_delay_min_us != 0xFF_FFFF {
            preferred_min = preferred_min.max(p.preferred_delay_min_us);
        }
        if p.preferred_delay_max_us != 0 && p.preferred_delay_max_us != 0xFF_FFFF {
            preferred_max = preferred_max.min(p.preferred_delay_max_us);
        }
    }
    Ok(if prefer_device && preferred_min <= preferred_max {
        wanted.clamp(preferred_min, preferred_max)
    } else {
        wanted.clamp(min, max)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bap::{ascs, Preset, LOCATION_FRONT_RIGHT};
    fn preference(min: u32, max: u32) -> ase::PreferredQos {
        ase::PreferredQos {
            framing: 0,
            phy_preference: 2,
            retransmission_preference: 2,
            max_transport_latency_ms: 95,
            presentation_delay_min_us: min,
            presentation_delay_max_us: max,
            preferred_delay_min_us: min,
            preferred_delay_max_us: max,
        }
    }
    fn codec_value(id: u8, codec: CodecConfiguration) -> Vec<u8> {
        let mut value = vec![id, ase::STATE_CODEC_CONFIGURED, 0, 2, 2, 95, 0];
        for _ in 0..4 {
            value.extend_from_slice(&[0x40, 0x9c, 0]);
        }
        value.extend_from_slice(&[6, 0, 0, 0, 0]);
        let encoded = codec.encode();
        value.push(encoded.len() as u8);
        value.extend(encoded);
        value
    }
    #[test]
    fn rejects_stale_codec_and_wrong_ear_after_reconnect() {
        let codec = Preset::WindowsDefault.codec(false);
        let value = codec_value(1, codec);
        assert!(codec_response(1, &codec, &value).is_ok());
        let mut changed = codec;
        changed.octets_per_frame = 80;
        assert!(codec_response(1, &changed, &value).is_err());
        changed = codec;
        changed.channel_allocation = LOCATION_FRONT_RIGHT;
        assert!(codec_response(1, &changed, &value).is_err());
        assert!(codec_response(2, &codec, &value).is_err());
    }
    #[test]
    fn rejects_qos_or_cis_mapping_left_from_another_attempt() {
        let codec = Preset::WindowsDefault.codec(false);
        let qos = Preset::WindowsDefault.qos(&codec);
        let command = ascs::config_qos(2, 1, 1, &qos);
        let mut value = vec![2, ase::STATE_QOS_CONFIGURED];
        value.extend_from_slice(&command[3..]);
        assert!(qos_response(2, 1, 1, &qos, &value).is_ok());
        assert!(qos_response(2, 1, 0, &qos, &value).is_err());
        let mut changed = qos;
        changed.max_transport_latency_ms += 1;
        assert!(qos_response(2, 1, 1, &changed, &value).is_err());
        assert!(qos_response(2, 1, 1, &qos, &value[..5]).is_err());
    }
    #[test]
    fn streaming_must_confirm_endpoint_and_group_not_just_state_byte() {
        assert!(streaming_response(2, 1, 1, &[2, 4, 1, 1, 0]).is_ok());
        for value in [
            &[2, 4, 1, 0, 0][..],
            &[1, 4, 1, 1, 0],
            &[2, 3, 1, 1, 0],
            &[2, 4],
        ] {
            assert!(streaming_response(2, 1, 1, value).is_err());
        }
    }
    #[test]
    fn stereo_delay_requires_intersection_and_preserves_valid_custom_choice() {
        assert!(presentation_delay(
            &[preference(20_000, 30_000), preference(40_000, 50_000)],
            25_000,
            false
        )
        .is_err());
        assert_eq!(
            presentation_delay(
                &[preference(20_000, 40_000), preference(30_000, 50_000)],
                25_000,
                false
            )
            .unwrap(),
            30_000
        );
        let mut p = preference(20_000, 60_000);
        p.preferred_delay_min_us = 40_000;
        p.preferred_delay_max_us = 40_000;
        assert_eq!(presentation_delay(&[p], 25_000, false).unwrap(), 25_000);
        assert_eq!(presentation_delay(&[p], 25_000, true).unwrap(), 40_000);
        assert_eq!(
            presentation_delay(&[preference(40_000, 40_000)], 0, false).unwrap(),
            40_000
        );
    }
}
