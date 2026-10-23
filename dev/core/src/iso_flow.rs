//! ISO buffer credits, following AOSP btm_iso_impl.h. Stereo admission is atomic:
//! when the controller is full, drop the entire audio frame, never just one ear.

/// LE Read Buffer Size v2 includes the sequence number and SDU length fields
/// in ISO_Data_Packet_Length. The outer four-byte HCI header is excluded.
pub fn buffer_limits(params: &[u8], sdu_size: u16, streams: usize) -> Result<(u16, u16), String> {
    if params.len() < 7 || params[0] != 0 {
        return Err("cannot read controller ISO buffer limits".into());
    }
    let packet_length = u16::from_le_bytes([params[4], params[5]]);
    let capacity = u16::from(params[6]);
    if streams > 0 && (streams > usize::from(capacity)
        || u32::from(sdu_size) + 4 > u32::from(packet_length)) {
        return Err(format!(
            "controller ISO buffers cannot carry {streams} packets with {sdu_size}-byte SDUs plus 4-byte ISO metadata: {capacity} buffers of {packet_length} bytes available"));
    }
    Ok((packet_length, capacity))
}
#[derive(Debug)]
pub struct IsoCredits {
    capacity: u16,
    available: u16,
    outstanding: Vec<(u16, u16)>,
}
impl IsoCredits {
    pub fn new(capacity: u16, handles: &[u16]) -> Self {
        Self {
            capacity,
            available: capacity,
            outstanding: handles.iter().map(|h| (*h, 0)).collect(),
        }
    }
    pub fn reserve_frame(&mut self, handles: &[u16]) -> bool {
        if handles.is_empty() || handles.len() > self.available as usize {
            return false;
        }
        if handles.iter().enumerate().any(|(i, h)| {
            handles[..i].contains(h) || !self.outstanding.iter().any(|(known, pending)| known == h && *pending < 4)
        }) {
            return false;
        }
        self.available -= handles.len() as u16;
        for handle in handles {
            self.outstanding
                .iter_mut()
                .find(|(h, _)| h == handle)
                .unwrap()
                .1 += 1;
        }
        true
    }
    /// Ignore unrelated/stale completions; they cannot increase capacity.
    pub fn complete(&mut self, handle: u16, count: u16) -> u16 {
        let Some((_, pending)) = self.outstanding.iter_mut().find(|(h, _)| *h == handle) else {
            return 0;
        };
        let completed = count.min(*pending);
        *pending -= completed;
        self.available = (self.available + completed).min(self.capacity);
        completed
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn buffer_size_includes_iso_metadata_and_rx_only_needs_no_tx_credits() {
        let limits = [0, 27, 0, 8, 104, 0, 2];
        assert_eq!(buffer_limits(&limits, 100, 2).unwrap(), (104, 2));
        assert!(buffer_limits(&limits, 101, 2).is_err());
        assert!(buffer_limits(&limits, 100, 3).is_err());
        assert!(buffer_limits(&limits[..6], 100, 2).is_err());
        assert!(buffer_limits(&[1, 27, 0, 8, 104, 0, 2], 100, 2).is_err());
        assert_eq!(buffer_limits(&[0; 7], 100, 0).unwrap(), (0, 0));
    }
    #[test]
    fn stereo_is_never_half_submitted_when_the_other_channel_holds_credits() {
        let mut credits = IsoCredits::new(2, &[23, 24]);
        assert!(credits.reserve_frame(&[23, 24]));
        assert_eq!(credits.complete(23, 1), 1);
        assert!(!credits.reserve_frame(&[23, 24]));
        assert_eq!(credits.complete(24, 1), 1);
        assert!(credits.reserve_frame(&[23, 24]));
    }
    #[test]
    fn unknown_duplicate_and_excess_completions_cannot_invent_credits() {
        let mut credits = IsoCredits::new(2, &[23, 24]);
        assert_eq!(credits.complete(23, 50), 0);
        assert!(!credits.reserve_frame(&[23, 23]));
        assert!(!credits.reserve_frame(&[23, 25]));
        assert!(credits.reserve_frame(&[23, 24]));
        assert_eq!(credits.complete(25, 50), 0);
        assert_eq!(credits.complete(23, 50), 1);
        assert_eq!(credits.complete(23, 50), 0);
        assert!(!credits.reserve_frame(&[23, 24]));
    }
}

#[cfg(test)]
mod backlog_regressions {
    use super::*;
    #[test]
    fn slow_ear_cannot_accumulate_an_entire_controller_queue() {
        let mut credits = IsoCredits::new(100, &[23,24]);
        for _ in 0..4 { assert!(credits.reserve_frame(&[23,24])); credits.complete(23,1); }
        assert!(!credits.reserve_frame(&[23,24]));
        credits.complete(24,1);
        assert!(credits.reserve_frame(&[23,24]));
    }
}
