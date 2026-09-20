//! ISO buffer credits, following AOSP btm_iso_impl.h. Stereo admission is atomic:
//! when the controller is full, drop the entire audio frame, never just one ear.
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
            handles[..i].contains(h) || !self.outstanding.iter().any(|(known, _)| known == h)
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
