// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_mac::hsdpa::Feedback;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidEvidenceLimits;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckRejection {
    NonFinite,
    NoCandidate,
    Correlation,
    Margin,
    RelativePower,
    PilotFraction,
    PilotDisturbance,
    NonAckWord,
}

impl AckRejection {
    pub const ALL: [Self; 8] = [
        Self::NonFinite,
        Self::NoCandidate,
        Self::Correlation,
        Self::Margin,
        Self::RelativePower,
        Self::PilotFraction,
        Self::PilotDisturbance,
        Self::NonAckWord,
    ];
}

#[derive(Clone, Copy, Debug)]
pub struct EvidenceLimits {
    minimum_correlation: f64,
    minimum_margin: f64,
    minimum_pilot_fraction: f64,
    maximum_relative_disturbance: f64,
    minimum_relative_soft_power: f64,
}

impl EvidenceLimits {
    // Soft symbols are divided by the interpolated pilot channel, not measured in ADC units.
    pub fn new(
        minimum_correlation: f64,
        minimum_margin: f64,
        minimum_pilot_fraction: f64,
        maximum_relative_disturbance: f64,
        minimum_relative_soft_power: f64,
    ) -> Result<Self, InvalidEvidenceLimits> {
        if ![
            minimum_correlation,
            minimum_margin,
            minimum_pilot_fraction,
            maximum_relative_disturbance,
            minimum_relative_soft_power,
        ]
        .iter()
        .all(|v| v.is_finite())
            || !(0.0..=1.0).contains(&minimum_correlation)
            || minimum_correlation == 0.0
            || !(0.0..=2.0).contains(&minimum_margin)
            || minimum_margin == 0.0
            || !(0.0..=1.0).contains(&minimum_pilot_fraction)
            || minimum_pilot_fraction == 0.0
            || maximum_relative_disturbance < 0.0
            || minimum_relative_soft_power <= 0.0
        {
            return Err(InvalidEvidenceLimits);
        }
        Ok(Self {
            minimum_correlation,
            minimum_margin,
            minimum_pilot_fraction,
            maximum_relative_disturbance,
            minimum_relative_soft_power,
        })
    }

    pub fn accepts<T>(&self, observation: &Observation<T>) -> bool {
        self.rejection(observation).is_none()
    }

    fn rejection<T>(&self, observation: &Observation<T>) -> Option<AckRejection> {
        let evidence = &observation.evidence;
        if ![
            evidence.normalized_correlation,
            evidence.normalized_margin,
            evidence.mean_soft_power,
            observation.minimum_pilot_fraction,
            observation.mean_relative_pilot_disturbance,
        ]
        .iter()
        .all(|v| v.is_finite())
        {
            Some(AckRejection::NonFinite)
        } else if evidence.candidate.is_none() {
            Some(AckRejection::NoCandidate)
        } else if evidence.normalized_correlation < self.minimum_correlation {
            Some(AckRejection::Correlation)
        } else if evidence.normalized_margin < self.minimum_margin {
            Some(AckRejection::Margin)
        } else if evidence.mean_soft_power < self.minimum_relative_soft_power {
            Some(AckRejection::RelativePower)
        } else if observation.minimum_pilot_fraction < self.minimum_pilot_fraction {
            Some(AckRejection::PilotFraction)
        } else if observation.mean_relative_pilot_disturbance < 0.0
            || observation.mean_relative_pilot_disturbance > self.maximum_relative_disturbance
        {
            Some(AckRejection::PilotDisturbance)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FeedbackPolicy {
    pub acknowledgement: EvidenceLimits,
    pub cqi: EvidenceLimits,
}

impl FeedbackPolicy {
    pub fn classify_ack_cells(
        &self,
        observation: &Observation<CarrierAcknowledgements>,
    ) -> [Feedback; 2] {
        if !self.acknowledgement.accepts(observation) {
            return [Feedback::Dtx; 2];
        }
        observation
            .evidence
            .candidate
            .map_or([Feedback::Dtx; 2], |word| {
                word.cells().map(|cell| match cell {
                    Some(umts_dsp::hsdpa::CellAck::Ack) => Feedback::Ack,
                    Some(umts_dsp::hsdpa::CellAck::Nack) => Feedback::Nack,
                    None => Feedback::Dtx,
                })
            })
    }

    pub fn classify_cqi_cells(&self, observation: &Observation<CarrierCqi>) -> [Option<Cqi>; 2] {
        if !self.cqi.accepts(observation) {
            return [None; 2];
        }
        observation
            .evidence
            .candidate
            .map_or([None; 2], CarrierCqi::cells)
    }

    pub fn classify_ack(&self, observation: &Observation<HarqAck>) -> Feedback {
        if !self.acknowledgement.accepts(observation) {
            return Feedback::Dtx;
        }
        match observation.evidence.candidate {
            Some(HarqAck::Ack) => Feedback::Ack,
            Some(HarqAck::Nack) => Feedback::Nack,
            _ => Feedback::Dtx,
        }
    }

    pub fn classify_ack_with_reason(
        &self,
        observation: &Observation<HarqAck>,
    ) -> (Feedback, Option<AckRejection>) {
        let feedback = self.classify_ack(observation);
        let rejection = (feedback == Feedback::Dtx).then(|| {
            self.acknowledgement
                .rejection(observation)
                .unwrap_or(AckRejection::NonAckWord)
        });
        (feedback, rejection)
    }

    pub fn classify_cqi(&self, observation: &Observation<Cqi>) -> Option<Cqi> {
        self.cqi
            .accepts(observation)
            .then_some(observation.evidence.candidate)
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_word_is_not_ack_without_pilot_and_relative_power_evidence() {
        let limits = EvidenceLimits::new(0.8, 0.2, 0.7, 0.1, 0.01).unwrap();
        let policy = FeedbackPolicy {
            acknowledgement: limits,
            cqi: limits,
        };
        let good = Observation {
            evidence: FeedbackEvidence {
                candidate: Some(HarqAck::Ack),
                normalized_correlation: 1.0,
                normalized_margin: 0.8,
                mean_soft_power: 0.1,
            },
            minimum_pilot_fraction: 0.9,
            mean_relative_pilot_disturbance: 0.01,
        };
        assert_eq!(policy.classify_ack(&good), Feedback::Ack);
        assert_eq!(
            policy.classify_ack_with_reason(&good),
            (Feedback::Ack, None)
        );
        for (bad, reason) in [
            Observation {
                minimum_pilot_fraction: 0.1,
                ..good
            },
            Observation {
                mean_relative_pilot_disturbance: 1.0,
                ..good
            },
            Observation {
                evidence: FeedbackEvidence {
                    mean_soft_power: 0.0,
                    ..good.evidence
                },
                ..good
            },
            Observation {
                evidence: FeedbackEvidence {
                    normalized_margin: f64::NAN,
                    ..good.evidence
                },
                ..good
            },
            Observation {
                evidence: FeedbackEvidence {
                    candidate: Some(HarqAck::Preamble),
                    ..good.evidence
                },
                ..good
            },
        ]
        .into_iter()
        .zip([
            AckRejection::PilotFraction,
            AckRejection::PilotDisturbance,
            AckRejection::RelativePower,
            AckRejection::NonFinite,
            AckRejection::NonAckWord,
        ]) {
            assert_eq!(policy.classify_ack(&bad), Feedback::Dtx);
            assert_eq!(
                policy.classify_ack_with_reason(&bad),
                (Feedback::Dtx, Some(reason))
            );
        }
        assert!(EvidenceLimits::new(0.0, 0.2, 0.7, 0.1, 0.01).is_err());
        assert!(EvidenceLimits::new(0.8, 0.2, 0.7, f64::INFINITY, 0.01).is_err());
    }
}
