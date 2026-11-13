// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_mac::hsdpa::{EntityError, Feedback, HarqEntity, ScheduledTransmission, TransmissionKey};
use umts_types::time::ChipInstant;
mod feedback;
pub use feedback::FeedbackError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlightError {
    Harq(EntityError),
    Identity,
    Time,
    State,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resolution<T> {
    Delivered(T),
    Retry,
    NotTransmitted,
}

enum Transmit {
    Pending,
    Submitted,
    AwaitingFeedback,
    Lost,
    Resolved,
}

#[derive(Clone, Copy)]
enum Receive {
    Reported(Feedback),
    Missing(ChipInstant),
}

pub struct Flight {
    transmission: ScheduledTransmission,
    feedback_end: ChipInstant,
    feedback_repetitions: std::num::NonZeroU8,
    feedback_request: Option<crate::hsdpa_rx::RequestId>,
    transmit: Transmit,
    receive: Option<Receive>,
}

pub struct RejectedCompletion {
    pub error: FlightError,
    pub completion: crate::hsdpa_tx::CompletedTransmission,
}

impl Flight {
    pub(super) fn transmit_result_received(&self) -> bool {
        !matches!(self.transmit, Transmit::Pending)
    }

    pub fn complete_transmit(
        &mut self,
        completion: crate::hsdpa_tx::CompletedTransmission,
    ) -> Result<crate::hsdpa_tx::WaveformBuffer, RejectedCompletion> {
        if completion.transmission.transmission() != self.transmission {
            return Err(RejectedCompletion {
                error: FlightError::Identity,
                completion,
            });
        }
        if let Err(error) = self.record_transmit(
            completion.transmission.transmission().key,
            completion.submitted,
        ) {
            return Err(RejectedCompletion { error, completion });
        }
        Ok(completion.transmission.recycle())
    }

    pub fn new<T>(
        transmission: ScheduledTransmission,
        harq: &HarqEntity<T>,
    ) -> Result<Self, FlightError> {
        if harq
            .reservation(transmission.key.process())
            .map_err(FlightError::Harq)?
            != Some(transmission)
        {
            return Err(FlightError::Identity);
        }
        Ok(Self {
            transmission,
            feedback_end: harq
                .feedback_end(transmission.key)
                .map_err(FlightError::Harq)?,
            feedback_repetitions: harq.ack_repetitions(),
            feedback_request: None,
            transmit: Transmit::Pending,
            receive: None,
        })
    }

    pub fn transmission(&self) -> ScheduledTransmission {
        self.transmission
    }

    pub(super) fn has_feedback(&self) -> bool {
        self.receive.is_some()
    }

    pub fn record_transmit(
        &mut self,
        key: TransmissionKey,
        submitted: bool,
    ) -> Result<(), FlightError> {
        self.check_key(key)?;
        if !matches!(self.transmit, Transmit::Pending) {
            return Err(FlightError::State);
        }
        self.transmit = if submitted {
            Transmit::Submitted
        } else {
            Transmit::Lost
        };
        Ok(())
    }

    /// Feedback must already satisfy the PHY acceptance policy, not merely rank first in a codebook.
    pub fn record_feedback(
        &mut self,
        key: TransmissionKey,
        observed: ChipInstant,
        feedback: Feedback,
    ) -> Result<(), FlightError> {
        self.check_receive(key)?;
        if observed != self.transmission.expected_ack {
            return Err(FlightError::Time);
        }
        self.receive = Some(Receive::Reported(feedback));
        Ok(())
    }

    pub fn record_missing(
        &mut self,
        key: TransmissionKey,
        processed_until: ChipInstant,
    ) -> Result<(), FlightError> {
        self.check_receive(key)?;
        if processed_until.epoch != self.feedback_end.epoch
            || processed_until.index < self.feedback_end.index
        {
            return Err(FlightError::Time);
        }
        self.receive = Some(Receive::Missing(processed_until));
        Ok(())
    }

    pub fn resolve<T>(
        &mut self,
        harq: &mut HarqEntity<T>,
    ) -> Result<Option<Resolution<T>>, FlightError> {
        let key = self.transmission.key;
        match self.transmit {
            Transmit::Pending => return Ok(None),
            Transmit::Resolved => return Err(FlightError::State),
            Transmit::Lost => {
                // A queued feedback request still owns this process identity until its report returns.
                if self.feedback_request.is_some() && self.receive.is_none() {
                    return Ok(None);
                }
                harq.lost(key).map_err(FlightError::Harq)?;
                self.transmit = Transmit::Resolved;
                return Ok(Some(Resolution::NotTransmitted));
            }
            Transmit::Submitted => {
                harq.transmitted(key).map_err(FlightError::Harq)?;
                self.transmit = Transmit::AwaitingFeedback;
            }
            Transmit::AwaitingFeedback => (),
        }
        let Some(receive) = self.receive else {
            return Ok(None);
        };
        let result = match receive {
            Receive::Reported(feedback) => match harq
                .feedback(key, self.transmission.expected_ack, feedback)
                .map_err(FlightError::Harq)?
            {
                Some(payload) => Resolution::Delivered(payload),
                None => Resolution::Retry,
            },
            Receive::Missing(through) => {
                harq.feedback_missing(key, through)
                    .map_err(FlightError::Harq)?;
                Resolution::Retry
            }
        };
        self.transmit = Transmit::Resolved;
        Ok(Some(result))
    }

    fn check_key(&self, key: TransmissionKey) -> Result<(), FlightError> {
        if key == self.transmission.key {
            Ok(())
        } else {
            Err(FlightError::Identity)
        }
    }

    fn check_receive(&self, key: TransmissionKey) -> Result<(), FlightError> {
        self.check_key(key)?;
        if self.receive.is_some() || matches!(self.transmit, Transmit::Resolved) {
            return Err(FlightError::State);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    mod feedback;
    use super::*;
    use crate::test_support::{at, count};
    use std::num::{NonZeroU8, NonZeroU64};
    use umts_mac::hsdpa::ProcessId;
    use umts_types::{UeKey, hsdpa::Category};

    fn fixture(repetitions: u8) -> (HarqEntity<u32>, Flight, ScheduledTransmission) {
        let mut harq = HarqEntity::new(
            UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            NonZeroU64::MIN,
            Category::new(10).unwrap(),
            at(0),
            &[count(28_800); 6],
            NonZeroU8::new(repetitions).unwrap(),
        )
        .unwrap();
        let id = ProcessId::new(0).unwrap();
        harq.insert(id, 123).unwrap();
        let transmission = harq.schedule(id, at(0), at(30_000)).unwrap();
        let flight = Flight::new(transmission, &harq).unwrap();
        (harq, flight, transmission)
    }

    #[test]
    fn ack_and_transmit_resolution_can_arrive_in_either_order() {
        for tx_first in [false, true] {
            let (mut harq, mut flight, transmission) = fixture(1);
            if tx_first {
                flight.record_transmit(transmission.key, true).unwrap();
            } else {
                flight
                    .record_feedback(transmission.key, transmission.expected_ack, Feedback::Ack)
                    .unwrap();
            }
            assert_eq!(flight.resolve(&mut harq), Ok(None));
            assert_eq!(
                harq.payload(transmission.key.process()).unwrap(),
                Some(&123)
            );
            if tx_first {
                flight
                    .record_feedback(transmission.key, transmission.expected_ack, Feedback::Ack)
                    .unwrap();
            } else {
                flight.record_transmit(transmission.key, true).unwrap();
            }
            assert_eq!(
                flight.resolve(&mut harq),
                Ok(Some(Resolution::Delivered(123)))
            );
            assert_eq!(flight.resolve(&mut harq), Err(FlightError::State));
            assert!(harq.payload(transmission.key.process()).unwrap().is_none());
        }
    }

    #[test]
    fn ack_cannot_turn_known_tx_loss_into_delivery() {
        let (mut harq, mut flight, transmission) = fixture(1);
        flight
            .record_feedback(transmission.key, transmission.expected_ack, Feedback::Ack)
            .unwrap();
        flight.record_transmit(transmission.key, false).unwrap();
        assert_eq!(
            flight.resolve(&mut harq),
            Ok(Some(Resolution::NotTransmitted))
        );
        assert_eq!(
            harq.payload(transmission.key.process()).unwrap(),
            Some(&123)
        );
        let next = harq
            .schedule(transmission.key.process(), at(38_400), at(68_400))
            .unwrap();
        let mut next_flight = Flight::new(next, &harq).unwrap();
        assert_eq!(
            next_flight.record_transmit(transmission.key, true),
            Err(FlightError::Identity)
        );
        assert_eq!(
            next_flight.record_feedback(transmission.key, next.expected_ack, Feedback::Ack),
            Err(FlightError::Identity)
        );
        assert_eq!(next_flight.resolve(&mut harq), Ok(None));
    }

    #[test]
    fn missing_feedback_requires_the_full_repeated_rx_window() {
        let (mut harq, mut flight, transmission) = fixture(3);
        let end = harq.feedback_end(transmission.key).unwrap();
        assert_eq!(
            flight.record_missing(transmission.key, at(end.index - 1)),
            Err(FlightError::Time)
        );
        flight.record_missing(transmission.key, end).unwrap();
        assert_eq!(flight.resolve(&mut harq), Ok(None));
        flight.record_transmit(transmission.key, true).unwrap();
        assert_eq!(flight.resolve(&mut harq), Ok(Some(Resolution::Retry)));
        assert_eq!(
            harq.payload(transmission.key.process()).unwrap(),
            Some(&123)
        );
    }
}
