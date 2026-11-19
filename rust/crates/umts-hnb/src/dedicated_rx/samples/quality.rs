// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_dsp::pilots::PilotEstimate;

const MINIMUM_COHERENCE: f64 = 0.5;
const CONFIRMING_SLOTS: u8 = 3;
const MAXIMUM_UNCONFIRMED_FRAMES: u8 = 10;
const TPC_MINIMUM_COHERENCE: f64 = 0.25;

fn coherence(pilot: PilotEstimate) -> Option<f64> {
    let signal = pilot.channel.norm_sqr();
    (signal.is_finite() && pilot.observed_power.is_finite() && pilot.observed_power > 0.0)
        .then(|| signal / pilot.observed_power)
}

pub(super) fn tpc_pilot_usable(pilot: PilotEstimate) -> bool {
    coherence(pilot).is_some_and(|coherence| coherence >= TPC_MINIMUM_COHERENCE)
}

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
        if coherence(pilot).is_some_and(|coherence| coherence >= MINIMUM_COHERENCE) {
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
    fn tpc_needs_a_correlated_pilot() {
        let pilot = |coherence: f64| PilotEstimate {
            channel: Complex64::new(coherence.sqrt(), 0.0),
            observed_power: 1.0,
            residual_power: 1.0 - coherence,
        };
        assert!(tpc_pilot_usable(pilot(0.7)));
        assert!(tpc_pilot_usable(pilot(TPC_MINIMUM_COHERENCE)));
        assert!(!tpc_pilot_usable(pilot(0.1)));
        assert!(!tpc_pilot_usable(PilotEstimate::default()));
        assert!(!tpc_pilot_usable(PilotEstimate {
            channel: Complex64::new(f64::NAN, 0.0),
            observed_power: 1.0,
            residual_power: 0.0,
        }));
    }

    #[test]
    fn prediction_requires_prior_lock_and_expires_after_the_hold_window() {
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

    #[test]
    fn prediction_bridges_a_short_interference_step_without_reacquisition() {
        let coherent = PilotEstimate {
            channel: Complex64::new(1.0, 0.0),
            observed_power: 1.1,
            residual_power: 0.1,
        };
        let interfered = PilotEstimate {
            channel: Complex64::new(0.2_f64.sqrt(), 0.0),
            observed_power: 1.0,
            residual_power: 0.8,
        };
        let mut quality = TrackingQuality::default();
        for _ in 0..3 {
            quality.observe(coherent);
        }
        assert!(quality.finish_frame(false));
        for _ in 0..6 {
            quality.begin_frame();
            for _ in 0..15 {
                quality.observe(interfered);
            }
            assert!(!quality.confirmed());
            assert!(quality.finish_frame(true));
        }
        quality.begin_frame();
        for _ in 0..3 {
            quality.observe(coherent);
        }
        assert!(quality.confirmed());
        assert!(quality.finish_frame(true));
    }

    #[test]
    fn prediction_does_not_outlive_100_ms_without_pilot_evidence() {
        let mut quality = TrackingQuality::default();
        for _ in 0..3 {
            quality.observe(PilotEstimate {
                channel: Complex64::new(1.0, 0.0),
                observed_power: 1.1,
                residual_power: 0.1,
            });
        }
        assert!(quality.finish_frame(false));
        for _ in 0..10 {
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
