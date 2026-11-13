// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    num::{NonZeroU8, NonZeroUsize},
    sync::Arc,
};

use umts_dsp::{
    DspError,
    bits::Bit,
    coding::{DecodeProgress as CodingProgress, DecodeWork},
    hsupa::{ControlInformation, TransportDecoder, TransportPlan},
    transport_blocks::BlockOutcome,
};
use umts_mac::hsupa::{Attempt, HarqEntity, HarqError, Reception, SoftBufferAction};
use umts_types::{
    UeKey,
    hsupa::{Etfci, Tti},
};

mod delivery;
pub use delivery::{Publication, ReceivePort, ReceiveProgress, Report, ReportRef};
mod packets;
pub use packets::{PacketAdmission, PacketDelivery, PacketError, PacketPdu};
mod feedback;
pub use feedback::{FeedbackAssociation, FeedbackError, FeedbackRequest};
mod ingress;
pub use ingress::{
    GrantActivity, Ingress, IngressCounters, IngressError, IngressInputs, IngressProgress,
    IngressSettings, IngressStep, SchedulingReport,
};
mod tracked;
pub use tracked::{
    ControlAdmission, ControlObservation, DecodeTiming, DecodedTti, TrackedFormat, TrackedReceiver,
    TrackedSettings,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveError {
    NotReady,
    Format,
    Budget,
    Harq(HarqError),
    Dsp(DspError),
}

struct Format {
    identity: Etfci,
    plan: Arc<TransportPlan>,
}

pub struct Received<'a> {
    pub feedback: Reception,
    pub outcome: BlockOutcome,
    /// Whether this attempt was soft-combined with earlier attempts on its HARQ process.
    pub combined: bool,
    pub reset_reason: Option<umts_mac::hsupa::ResetReason>,
    pub payload: Option<&'a [Bit]>,
}

pub struct HarqReceiver {
    ue: UeKey,
    interval: Tti,
    rv_zero_only: bool,
    entity: HarqEntity,
    formats: Vec<Format>,
    decoder: TransportDecoder,
    combined: Vec<Vec<f32>>,
    evidence: Vec<f32>,
    payload: Vec<Bit>,
    pending: Option<PendingDecode>,
}

#[derive(Clone, Copy)]
struct PendingDecode {
    attempt: Attempt,
    format: usize,
    outcome: Option<BlockOutcome>,
    combined: bool,
    reset_reason: Option<umts_mac::hsupa::ResetReason>,
}

impl HarqReceiver {
    pub fn retained_bytes(&self) -> Result<usize, ReceiveError> {
        let base = std::mem::size_of::<Self>()
            .checked_add(self.formats.capacity() * std::mem::size_of::<Format>())
            .and_then(|bytes| {
                bytes.checked_add(self.combined.capacity() * std::mem::size_of::<Vec<f32>>())
            })
            .and_then(|bytes| {
                bytes.checked_add(self.evidence.capacity() * std::mem::size_of::<f32>())
            })
            .and_then(|bytes| {
                bytes.checked_add(self.payload.capacity() * std::mem::size_of::<Bit>())
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    self.decoder
                        .retained_bytes()?
                        .checked_sub(std::mem::size_of::<TransportDecoder>())?,
                )
            })
            .ok_or(ReceiveError::Budget)?;
        self.combined
            .iter()
            .try_fold(base, |bytes, values| {
                bytes.checked_add(values.capacity() * std::mem::size_of::<f32>())
            })
            .ok_or(ReceiveError::Budget)
    }
    // The budget covers retained soft bits and payload scratch. Decoder-plan admission is separate.
    pub fn new(
        ue: UeKey,
        interval: Tti,
        rv_zero_only: bool,
        formats: Vec<(Etfci, Arc<TransportPlan>)>,
        iterations: NonZeroU8,
        soft_buffer_bytes: NonZeroUsize,
    ) -> Result<Self, ReceiveError> {
        if formats.is_empty() || formats.len() > 128 {
            return Err(ReceiveError::Format);
        }
        let mut seen = [false; 128];
        let mut maximum_coded = 0;
        let mut maximum_payload = 0;
        for (identity, plan) in &formats {
            if plan.interval() != interval {
                return Err(ReceiveError::Format);
            }
            let index = usize::from(identity.value());
            if seen[index] {
                return Err(ReceiveError::Format);
            }
            seen[index] = true;
            maximum_coded = maximum_coded.max(plan.coded_bits());
            maximum_payload = maximum_payload.max(plan.payload_bits());
        }
        let processes = usize::from(interval.processes());
        let bytes = maximum_coded
            .checked_mul(processes + 1)
            .and_then(|n| n.checked_mul(std::mem::size_of::<f32>()))
            .and_then(|n| n.checked_add(maximum_payload.checked_mul(std::mem::size_of::<Bit>())?))
            .ok_or(ReceiveError::Budget)?;
        if bytes > soft_buffer_bytes.get() {
            return Err(ReceiveError::Budget);
        }
        let decoder = TransportDecoder::from_plans(
            formats.iter().map(|(_, plan)| Arc::clone(plan)).collect(),
            iterations,
        )
        .map_err(ReceiveError::Dsp)?;
        let formats = formats
            .into_iter()
            .map(|(identity, plan)| Format { identity, plan })
            .collect();
        Ok(Self {
            ue,
            interval,
            rv_zero_only,
            entity: HarqEntity::new(ue, interval),
            formats,
            decoder,
            combined: (0..processes).map(|_| vec![0.0; maximum_coded]).collect(),
            evidence: vec![0.0; maximum_coded],
            payload: vec![Bit::Zero; maximum_payload],
            pending: None,
        })
    }

    // Decoding must run off the radio reader and TX submission threads.
    // Input concatenates de-spread likelihoods for each E-DPDCH in channel-number order.
    #[cfg(test)]
    pub fn receive(
        &mut self,
        ue: UeKey,
        absolute_tti: u64,
        control: ControlInformation,
        physical: &[f32],
    ) -> Result<Received<'_>, ReceiveError> {
        self.begin(ue, absolute_tti, control, physical)?;
        self.service(&mut DecodeWork::new(NonZeroUsize::MAX))?
            .ok_or(ReceiveError::Budget)
    }

    pub fn begin(
        &mut self,
        ue: UeKey,
        absolute_tti: u64,
        control: ControlInformation,
        physical: &[f32],
    ) -> Result<(), ReceiveError> {
        if ue != self.ue {
            return Err(ReceiveError::Harq(HarqError::Identity));
        }
        if self.pending.is_some() {
            return Err(ReceiveError::Harq(HarqError::Pending));
        }
        let index = self
            .formats
            .iter()
            .position(|format| format.identity == control.format)
            .ok_or(ReceiveError::Format)?;
        let format = &self.formats[index];
        self.decoder.select(index).map_err(ReceiveError::Dsp)?;
        let coded_bits = format.plan.coded_bits();
        let evidence = &mut self.evidence[..coded_bits];
        // CFN wrap leaves RV phase unchanged: 1280/8 and 256/4 are multiples of four.
        let cycle_ttis = match self.interval {
            Tti::TwoMilliseconds => 1280,
            Tti::TenMilliseconds => 256,
        };
        let rv = control
            .sequence
            .redundancy_version(
                coded_bits / 3,
                format.plan.physical_bits(),
                self.interval,
                (absolute_tti % cycle_ttis) as u16,
                self.rv_zero_only,
            )
            .map_err(|_| ReceiveError::Format)?;
        self.decoder
            .unmatch(physical, rv, evidence)
            .map_err(ReceiveError::Dsp)?;
        let (attempt, action) = self
            .entity
            .begin(ue, absolute_tti, control.format, control.sequence)
            .map_err(ReceiveError::Harq)?;
        let combined = &mut self.combined[attempt.process()][..coded_bits];
        if action == SoftBufferAction::Reset {
            combined.fill(0.0);
        }
        if combined
            .iter()
            .zip(evidence.iter())
            .any(|(a, b)| !(*a + *b).is_finite())
        {
            combined.fill(0.0);
            self.entity.cancel(attempt).map_err(ReceiveError::Harq)?;
            return Err(ReceiveError::Dsp(DspError::NonFiniteEvidence));
        }
        for (sum, value) in combined.iter_mut().zip(evidence.iter()) {
            *sum += value;
        }
        let payload = &mut self.payload[..format.plan.payload_bits()];
        let outcome = match self.decoder.begin_combined(combined, payload) {
            Ok(CodingProgress::Complete(outcome)) => Some(outcome),
            Ok(CodingProgress::Pending) => None,
            Err(error) => {
                self.decoder.cancel();
                combined.fill(0.0);
                self.entity.cancel(attempt).map_err(ReceiveError::Harq)?;
                return Err(ReceiveError::Dsp(error));
            }
        };
        self.pending = Some(PendingDecode {
            attempt,
            format: index,
            outcome,
            combined: action == SoftBufferAction::Combine,
            reset_reason: self.entity.last_reset_reason(),
        });
        Ok(())
    }

    #[cfg(test)]
    pub fn pending_attempt(&self) -> Option<Attempt> {
        self.pending.map(|pending| pending.attempt)
    }

    pub fn expected_retransmission(&self, tti: u64) -> Option<(Etfci, umts_types::hsupa::Rsn)> {
        self.entity.expected_retransmission(tti)
    }

    pub fn cancel_pending(&mut self) -> Result<bool, ReceiveError> {
        let Some(pending) = self.pending else {
            return Ok(false);
        };
        self.entity
            .cancel(pending.attempt)
            .map_err(ReceiveError::Harq)?;
        self.decoder.cancel();
        self.combined[pending.attempt.process()].fill(0.0);
        self.pending = None;
        Ok(true)
    }

    pub fn service(&mut self, work: &mut DecodeWork) -> Result<Option<Received<'_>>, ReceiveError> {
        let Some(pending) = self.pending else {
            return Ok(None);
        };
        let plan = &self.formats[pending.format].plan;
        let coded_bits = plan.coded_bits();
        let payload_bits = plan.payload_bits();
        let outcome = match pending.outcome {
            Some(outcome) => outcome,
            None => match self.decoder.step_combined(
                &self.combined[pending.attempt.process()][..coded_bits],
                &mut self.payload[..payload_bits],
                work,
            ) {
                Ok(CodingProgress::Pending) => return Ok(None),
                Ok(CodingProgress::Complete(outcome)) => outcome,
                Err(error) => {
                    self.cancel_pending()?;
                    return Err(ReceiveError::Dsp(error));
                }
            },
        };
        let feedback = self
            .entity
            .complete(pending.attempt, outcome == BlockOutcome::Decoded)
            .map_err(ReceiveError::Harq)?;
        self.pending = None;
        Ok(Some(Received {
            feedback,
            outcome,
            combined: pending.combined,
            reset_reason: pending.reset_reason,
            payload: matches!(feedback, Reception::Ack { deliver: true })
                .then_some(&self.payload[..payload_bits]),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use umts_dsp::hsupa::TransportEncoder;
    use umts_types::{
        RadioEpoch,
        hsupa::{Rel6Category, Rsn},
    };

    #[test]
    fn bounded_decode_retains_attempt_until_completion_or_cancellation() {
        let nz = |value| NonZeroUsize::new(value).unwrap();
        let ue = UeKey {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            slot: 0,
            generation: NonZeroU64::MIN,
        };
        let plan = Arc::new(
            TransportPlan::new(
                nz(120),
                Tti::TwoMilliseconds,
                Rel6Category::new(6).unwrap(),
                &[16],
                nz(100_000),
            )
            .unwrap(),
        );
        let format = Etfci::new(10).unwrap();
        let control = ControlInformation {
            format,
            sequence: Rsn::new(0).unwrap(),
            happy: true,
        };
        let payload: Vec<_> = (0..120).map(|bit| Bit::from(bit % 3 == 0)).collect();
        let mut physical = vec![Bit::Zero; plan.physical_bits()];
        TransportEncoder::new(Arc::clone(&plan))
            .unwrap()
            .encode(&payload, 0, &mut physical)
            .unwrap();
        let evidence: Vec<_> = physical
            .iter()
            .map(|bit| if *bit == Bit::Zero { 5.0 } else { -5.0 })
            .collect();
        let mut receiver = HarqReceiver::new(
            ue,
            Tti::TwoMilliseconds,
            true,
            vec![(format, plan)],
            NonZeroU8::new(6).unwrap(),
            nz(100_000),
        )
        .unwrap();
        receiver.begin(ue, 0, control, &evidence).unwrap();
        let decode_quantum = nz(17);
        assert!(
            receiver
                .service(&mut DecodeWork::new(decode_quantum))
                .unwrap()
                .is_none()
        );
        let attempt = receiver.pending_attempt().unwrap();
        assert_eq!(attempt.tti(), 0);
        assert_eq!(
            receiver.begin(ue, 1, control, &evidence),
            Err(ReceiveError::Harq(HarqError::Pending))
        );
        assert_eq!(receiver.pending_attempt(), Some(attempt));
        assert!(receiver.cancel_pending().unwrap());
        assert!(!receiver.cancel_pending().unwrap());
        assert!(
            receiver
                .service(&mut DecodeWork::new(decode_quantum))
                .unwrap()
                .is_none()
        );
        receiver.begin(ue, 8, control, &evidence).unwrap();
        let mut completed = false;
        let mut steps = 0;
        for _ in 0..10_000 {
            let mut work = DecodeWork::new(decode_quantum);
            steps += 1;
            if let Some(result) = receiver.service(&mut work).unwrap() {
                assert_eq!(result.feedback, Reception::Ack { deliver: true });
                assert_eq!(result.payload, Some(payload.as_slice()));
                completed = true;
                break;
            }
            assert!(work.remaining() < 257);
        }
        assert!(completed && steps > 1);
        assert!(receiver.pending_attempt().is_none());
        assert!(
            receiver
                .service(&mut DecodeWork::new(nz(257)))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn soft_combining_recovers_a_block_that_no_single_attempt_decodes() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let ue = UeKey {
            epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
            slot: 0,
            generation: NonZeroU64::new(1).unwrap(),
        };
        let plan = Arc::new(
            TransportPlan::new(
                nz(5772),
                Tti::TwoMilliseconds,
                Rel6Category::new(6).unwrap(),
                &[2, 2, 4, 4],
                nz(200_000),
            )
            .unwrap(),
        );
        let format = Etfci::new(100).unwrap();
        let payload: Vec<_> = (0..5772)
            .map(|n| Bit::from(n % 7 == 0 || n % 5 == 1))
            .collect();
        let mut encoder = TransportEncoder::new(Arc::clone(&plan)).unwrap();
        let mut physical = vec![Bit::Zero; plan.physical_bits()];
        let fresh = |rv_zero_only| {
            HarqReceiver::new(
                ue,
                Tti::TwoMilliseconds,
                rv_zero_only,
                vec![(format, Arc::clone(&plan))],
                NonZeroU8::new(6).unwrap(),
                nz(800_000),
            )
            .unwrap()
        };
        let control = |rsn: u8| ControlInformation {
            format,
            sequence: Rsn::new(rsn).unwrap(),
            happy: true,
        };
        // Chase combining: every attempt is the same RV0 word, so the receiver's running sum
        // must equal an ideal sum of the same noisy copies decoded in one go.
        for (magnitude, amplitude) in [
            (5.0f32, 0.0f32),
            (5.0, 14.0),
            (5.0, 18.0),
            (5.0, 22.0),
            (5.0, 26.0),
        ] {
            let mut seed: u64 = 0x9E37_79B9_7F4A_7C15 ^ (amplitude.to_bits() as u64);
            let mut noise = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32 * amplitude
            };
            encoder.encode(&payload, 0, &mut physical).unwrap();
            let copies: Vec<Vec<f32>> = (0..4)
                .map(|_| {
                    physical
                        .iter()
                        .map(|bit| {
                            (if *bit == Bit::Zero {
                                magnitude
                            } else {
                                -magnitude
                            }) + noise()
                        })
                        .collect()
                })
                .collect();
            let mut receiver = fresh(true);
            let mut decoded_at = None;
            for (attempt, soft) in copies.iter().enumerate() {
                let result = receiver
                    .receive(ue, attempt as u64 * 8, control(attempt as u8), soft)
                    .unwrap();
                if result.feedback == (Reception::Ack { deliver: true }) {
                    assert_eq!(result.payload, Some(payload.as_slice()));
                    decoded_at = Some(attempt);
                    break;
                }
            }
            let mut ideal_at = None;
            for n in 1..=4 {
                let summed: Vec<f32> = (0..plan.physical_bits())
                    .map(|i| copies[..n].iter().map(|copy| copy[i]).sum())
                    .collect();
                let mut single = fresh(true);
                let result = single.receive(ue, 0, control(0), &summed).unwrap();
                if result.feedback == (Reception::Ack { deliver: true }) {
                    ideal_at = Some(n - 1);
                    break;
                }
            }
            assert_eq!(
                decoded_at, ideal_at,
                "magnitude {magnitude} noise {amplitude}: running soft combination must match an ideal sum"
            );
        }
    }

    #[test]
    fn transport_crc_drives_feedback_and_retransmissions_do_not_duplicate_payload() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let ue = UeKey {
            epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
            slot: 0,
            generation: NonZeroU64::new(1).unwrap(),
        };
        let plan = Arc::new(
            TransportPlan::new(
                nz(120),
                Tti::TwoMilliseconds,
                Rel6Category::new(6).unwrap(),
                &[16],
                nz(100_000),
            )
            .unwrap(),
        );
        let format = Etfci::new(10).unwrap();
        let mut receiver = HarqReceiver::new(
            ue,
            Tti::TwoMilliseconds,
            false,
            vec![(format, Arc::clone(&plan))],
            NonZeroU8::new(6).unwrap(),
            nz(100_000),
        )
        .unwrap();
        let mut encoder = TransportEncoder::new(Arc::clone(&plan)).unwrap();
        let payload: Vec<_> = (0..120).map(|n| Bit::from(n % 3 == 0)).collect();
        let mut physical = vec![Bit::Zero; plan.physical_bits()];
        let control = |rsn| ControlInformation {
            format,
            sequence: Rsn::new(rsn).unwrap(),
            happy: true,
        };
        let erased = vec![0.0; plan.physical_bits()];
        let result = receiver.receive(ue, 0, control(0), &erased).unwrap();
        assert_eq!(result.feedback, Reception::Nack);
        assert!(result.payload.is_none());
        for (tti, rsn, deliver) in [(8, 1, true), (16, 2, false), (24, 0, true)] {
            let control = control(rsn);
            let rv = control
                .sequence
                .redundancy_version(
                    plan.coded_bits() / 3,
                    plan.physical_bits(),
                    Tti::TwoMilliseconds,
                    tti as u16,
                    false,
                )
                .unwrap();
            encoder.encode(&payload, rv, &mut physical).unwrap();
            let soft: Vec<_> = physical
                .iter()
                .map(|bit| if *bit == Bit::Zero { 5.0 } else { -5.0 })
                .collect();
            let result = receiver.receive(ue, tti, control, &soft).unwrap();
            assert_eq!(result.feedback, Reception::Ack { deliver });
            assert_eq!(result.payload, deliver.then_some(payload.as_slice()));
        }
        assert!(matches!(
            receiver.receive(UeKey { slot: 1, ..ue }, 25, control(0), &erased),
            Err(ReceiveError::Harq(HarqError::Identity))
        ));
        assert!(matches!(
            receiver.receive(ue, 24, control(0), &erased),
            Err(ReceiveError::Harq(HarqError::Time))
        ));
    }
}
