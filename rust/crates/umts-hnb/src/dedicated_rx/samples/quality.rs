// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_dsp::pilots::PilotEstimate;

const MINIMUM_COHERENCE: f64 = 0.5;
const CONFIRMING_SLOTS: u8 = 3;
const MAXIMUM_UNCONFIRMED_FRAMES: u8 = 2;

#[derive(Default)]
pub(super) struct TrackingQuality {
    consecutive: u8,
    confirmed: bool,
    unconfirmed_frames: u8,
}

impl TrackingQuality {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn begin_frame(&mut self) {
        self.consecutive = 0;
        self.confirmed = false;
    }

    pub(super) fn confirmed(&self) -> bool {
        self.confirmed
    }

    pub(super) fn observe(&mut self, pilot: PilotEstimate) -> bool {
        let signal = pilot.channel.norm_sqr();
        if signal.is_finite()
            && pilot.observed_power.is_finite()
            && pilot.observed_power > 0.0
            && signal / pilot.observed_power >= MINIMUM_COHERENCE
        {
            self.consecutive = self.consecutive.saturating_add(1);
            self.confirmed |= self.consecutive >= CONFIRMING_SLOTS;
        } else {
            self.consecutive = 0;
        }
        self.consecutive >= CONFIRMING_SLOTS
    }

    pub(super) fn finish_frame(&mut self, was_tracking: bool) -> bool {
        if self.confirmed {
            self.unconfirmed_frames = 0;
            true
        } else {
            self.unconfirmed_frames = self.unconfirmed_frames.saturating_add(1);
            was_tracking && self.unconfirmed_frames <= MAXIMUM_UNCONFIRMED_FRAMES
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex64;

    #[test]
    fn carrier_confirmation_requires_current_consecutive_coherent_slots() {
        let mut quality = TrackingQuality::default();
        let coherent = PilotEstimate {
            channel: Complex64::new(1.0, 0.0),
            observed_power: 1.1,
            residual_power: 0.1,
        };
        assert!(!quality.observe(coherent));
        assert!(!quality.observe(coherent));
        assert!(quality.observe(coherent));
        assert!(!quality.observe(PilotEstimate::default()));
        assert!(!quality.observe(coherent));
        assert!(!quality.observe(coherent));
        assert!(quality.observe(coherent));
        quality.begin_frame();
        assert!(!quality.observe(coherent));
    }

    #[test]
    fn prediction_requires_prior_lock_and_expires_after_two_unconfirmed_frames() {
        let mut quality = TrackingQuality::default();
        quality.begin_frame();
        assert!(!quality.finish_frame(false));
        quality.reset();
        for _ in 0..CONFIRMING_SLOTS {
            quality.observe(PilotEstimate {
                channel: Complex64::new(1.0, 0.0),
                observed_power: 1.1,
                residual_power: 0.1,
            });
        }
        assert!(quality.finish_frame(false));
        for _ in 0..MAXIMUM_UNCONFIRMED_FRAMES {
            quality.begin_frame();
            assert!(quality.finish_frame(true));
        }
        quality.begin_frame();
        assert!(!quality.finish_frame(true));
        quality.reset();
        quality.begin_frame();
        assert!(!quality.finish_frame(false));
    }
}
