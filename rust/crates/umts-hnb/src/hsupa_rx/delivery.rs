// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{DecodedTti, ReceiveError};
use crate::dedicated_rx::ReceiveIdentity;
use std::{
    num::NonZeroUsize,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use umts_dsp::{bits::Bit, hsupa::ControlDecision, transport_blocks::BlockOutcome};
use umts_mac::hsupa::Reception;
use umts_runtime::mailbox::{self, Receiver, Sender};
use umts_types::time::ChipInstant;

const MAX_REPORTS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub timing: Option<super::DecodeTiming>,
    pub identity: ReceiveIdentity,
    pub start: ChipInstant,
    pub absolute_tti: u64,
    pub control: ControlDecision,
    pub feedback: Reception,
    pub outcome: BlockOutcome,
    pub combined: bool,
    pub reset_reason: Option<umts_mac::hsupa::ResetReason>,
    pub payload_bits: usize,
}

pub struct ReportRef<'a> {
    pub report: Report,
    pub payload: &'a [Bit],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publication {
    Idle,
    Pending,
    Published,
    Backpressure,
    Closed,
}

struct Packet {
    report: Option<Report>,
    payload: Vec<Bit>,
    published_at: Option<Instant>,
}
struct Link {
    producer: AtomicBool,
    consumer: AtomicBool,
    outstanding: AtomicUsize,
    progress: WorkerProgress,
    control_loss: OnceLock<ControlLoss>,
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct ControlLoss {
    pub last_admitted: super::ControlObservation,
    pub rejected: super::ControlObservation,
    pub consecutive_rejections: u64,
}

#[derive(Clone, Copy, Debug, Default)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct ReceiveProgress {
    pub sample_searches: u64,
    pub sample_acquired_frames: u64,
    pub sample_rejected_frames: u64,
    pub sample_history_lost_frames: u64,
    pub last_sample_search_chip: u64,
    pub control_loss: Option<ControlLoss>,
    pub collected: u64,
    pub last_collected_chip: u64,
    pub no_control: u64,
    pub rejected_control: u64,
    pub admitted_control: u64,
    pub dropped_intervals: u64,
    pub output_backpressure: u64,
}

#[derive(Default)]
struct WorkerProgress {
    sample_searches: AtomicU64,
    sample_acquired_frames: AtomicU64,
    sample_rejected_frames: AtomicU64,
    sample_history_lost_frames: AtomicU64,
    last_sample_search_chip: AtomicU64,
    collected: AtomicU64,
    last_collected_chip: AtomicU64,
    no_control: AtomicU64,
    rejected_control: AtomicU64,
    admitted_control: AtomicU64,
    dropped_intervals: AtomicU64,
    output_backpressure: AtomicU64,
}

pub(super) enum ProgressEvent {
    Collected(ChipInstant),
    NoControl,
    RejectedControl,
    AdmittedControl,
    DroppedInterval,
    OutputBackpressure,
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn progress_is_visible_without_publishing_payload_or_consuming_capacity() {
        let (delivery, mut port) = Delivery::new(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(128).unwrap(),
            NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        let at = ChipInstant {
            epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
            index: 2560,
        };
        delivery.observe(ProgressEvent::Collected(at));
        delivery.observe(ProgressEvent::NoControl);
        delivery.observe(ProgressEvent::RejectedControl);
        delivery.observe(ProgressEvent::AdmittedControl);
        delivery.observe(ProgressEvent::DroppedInterval);
        delivery.observe(ProgressEvent::OutputBackpressure);
        let mut samples = crate::dedicated_rx::SampleReceiveCounters {
            searches: 5,
            acquired_frames: 2,
            rejected_frames: 3,
            history_lost_frames: 4,
            sampled_slots: 30,
            ..Default::default()
        };
        delivery.observe_samples(samples, Some(at));
        samples.history_lost_frames = 6;
        delivery.observe_samples(samples, None);
        let progress = port.progress();
        assert_eq!(progress.sample_searches, 5);
        assert_eq!(progress.sample_acquired_frames, 2);
        assert_eq!(progress.sample_rejected_frames, 3);
        assert_eq!(progress.sample_history_lost_frames, 6);
        assert_eq!(progress.last_sample_search_chip, at.index);
        assert_eq!(progress.collected, 1);
        assert_eq!(progress.last_collected_chip, at.index);
        assert_eq!(progress.no_control, 1);
        assert_eq!(progress.rejected_control, 1);
        assert_eq!(progress.admitted_control, 1);
        assert_eq!(progress.dropped_intervals, 1);
        assert_eq!(progress.output_backpressure, 1);
        assert!(port.peek().is_none());
        assert_eq!(delivery.outstanding(), 0);
    }
}

pub struct ReceivePort {
    ready: Receiver<Packet>,
    free: Sender<Packet>,
    pending: Option<Packet>,
    link: Arc<Link>,
}

impl ReceivePort {
    // Independently sampled diagnostics are not a coherent protocol-state snapshot.
    pub fn progress(&self) -> ReceiveProgress {
        let p = &self.link.progress;
        ReceiveProgress {
            sample_searches: p.sample_searches.load(Ordering::Relaxed),
            sample_acquired_frames: p.sample_acquired_frames.load(Ordering::Relaxed),
            sample_rejected_frames: p.sample_rejected_frames.load(Ordering::Relaxed),
            sample_history_lost_frames: p.sample_history_lost_frames.load(Ordering::Relaxed),
            last_sample_search_chip: p.last_sample_search_chip.load(Ordering::Relaxed),
            control_loss: self.link.control_loss.get().copied(),
            collected: p.collected.load(Ordering::Relaxed),
            last_collected_chip: p.last_collected_chip.load(Ordering::Relaxed),
            no_control: p.no_control.load(Ordering::Relaxed),
            rejected_control: p.rejected_control.load(Ordering::Relaxed),
            admitted_control: p.admitted_control.load(Ordering::Relaxed),
            dropped_intervals: p.dropped_intervals.load(Ordering::Relaxed),
            output_backpressure: p.output_backpressure.load(Ordering::Relaxed),
        }
    }
    pub fn peek_with_age(&mut self) -> Option<(ReportRef<'_>, Duration)> {
        self.peek()?;
        let packet = self.pending.as_ref()?;
        let report = packet.report.expect("published HSUPA report");
        Some((
            ReportRef {
                report,
                payload: &packet.payload[..report.payload_bits],
            },
            packet
                .published_at
                .expect("published report timestamp")
                .elapsed(),
        ))
    }

    #[cfg(test)]
    pub fn pending_age(&self) -> Option<Duration> {
        self.pending.as_ref()?.published_at.map(|at| at.elapsed())
    }

    pub fn producer_available(&self) -> bool {
        self.link.producer.load(Ordering::Acquire)
    }

    pub fn closed_and_empty(&mut self) -> bool {
        // The close acquire precedes the final queue read so the last publication cannot be missed.
        !self.producer_available() && self.peek().is_none()
    }

    pub fn peek(&mut self) -> Option<ReportRef<'_>> {
        if self.pending.is_none() {
            self.pending = self.ready.try_recv();
        }
        let packet = self.pending.as_ref()?;
        let report = packet.report.expect("published HSUPA report");
        Some(ReportRef {
            report,
            payload: &packet.payload[..report.payload_bits],
        })
    }

    pub fn release(&mut self) -> bool {
        let Some(mut packet) = self.pending.take() else {
            return false;
        };
        packet.report = None;
        packet.published_at = None;
        self.link.outstanding.fetch_sub(1, Ordering::AcqRel);
        assert!(
            self.free.try_send(packet).is_ok(),
            "owned HSUPA output slot"
        );
        true
    }
}

impl Drop for ReceivePort {
    fn drop(&mut self) {
        self.link.consumer.store(false, Ordering::Release);
    }
}

pub(super) struct Delivery {
    free: Receiver<Packet>,
    ready: Sender<Packet>,
    reserved: Option<Packet>,
    link: Arc<Link>,
    bytes: usize,
}

impl Delivery {
    pub(super) fn observe_samples(
        &self,
        counters: crate::dedicated_rx::SampleReceiveCounters,
        search: Option<ChipInstant>,
    ) {
        let p = &self.link.progress;
        p.sample_searches
            .store(counters.searches, Ordering::Relaxed);
        p.sample_acquired_frames
            .store(counters.acquired_frames, Ordering::Relaxed);
        p.sample_rejected_frames
            .store(counters.rejected_frames, Ordering::Relaxed);
        p.sample_history_lost_frames
            .store(counters.history_lost_frames, Ordering::Relaxed);
        if let Some(search) = search {
            p.last_sample_search_chip
                .store(search.index, Ordering::Relaxed);
        }
    }
    pub fn new(
        capacity: NonZeroUsize,
        bits: NonZeroUsize,
        maximum_bytes: NonZeroUsize,
    ) -> Result<(Self, ReceivePort), ReceiveError> {
        if capacity.get() > MAX_REPORTS {
            return Err(ReceiveError::Budget);
        }
        // Include both bounded queue slots and each exclusively owned payload buffer.
        let bytes = bits
            .get()
            .checked_mul(std::mem::size_of::<Bit>())
            .and_then(|bytes| bytes.checked_mul(capacity.get()))
            .and_then(|bytes| {
                bytes.checked_add(
                    mailbox::retained_storage_bound::<Packet>(capacity)?.checked_mul(2)?,
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    std::mem::size_of::<Self>()
                        + std::mem::size_of::<ReceivePort>()
                        + std::mem::size_of::<Link>()
                        + 2 * std::mem::size_of::<usize>(),
                )
            })
            .ok_or(ReceiveError::Budget)?;
        if bytes > maximum_bytes.get() {
            return Err(ReceiveError::Budget);
        }
        let (ready, receive) = mailbox::bounded(capacity);
        let (recycle, free) = mailbox::bounded(capacity);
        for _ in 0..capacity.get() {
            assert!(
                recycle
                    .try_send(Packet {
                        report: None,
                        payload: vec![Bit::Zero; bits.get()],
                        published_at: None
                    })
                    .is_ok(),
                "new HSUPA output pool"
            );
        }
        let link = Arc::new(Link {
            producer: AtomicBool::new(true),
            consumer: AtomicBool::new(true),
            outstanding: AtomicUsize::new(0),
            progress: WorkerProgress::default(),
            control_loss: OnceLock::new(),
        });
        Ok((
            Self {
                free,
                ready,
                reserved: None,
                link: Arc::clone(&link),
                bytes,
            },
            ReceivePort {
                ready: receive,
                free: recycle,
                pending: None,
                link,
            },
        ))
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub(super) fn observe(&self, event: ProgressEvent) {
        let p = &self.link.progress;
        let counter = match event {
            ProgressEvent::Collected(at) => {
                p.last_collected_chip.store(at.index, Ordering::Relaxed);
                &p.collected
            }
            ProgressEvent::NoControl => &p.no_control,
            ProgressEvent::RejectedControl => &p.rejected_control,
            ProgressEvent::AdmittedControl => &p.admitted_control,
            ProgressEvent::DroppedInterval => &p.dropped_intervals,
            ProgressEvent::OutputBackpressure => &p.output_backpressure,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_control_loss(&self, evidence: ControlLoss) {
        // Only the receive owner writes this cell, so initialization cannot contend.
        let _ = self.link.control_loss.set(evidence);
    }
    pub fn outstanding(&self) -> usize {
        self.link.outstanding.load(Ordering::Acquire)
    }
    pub fn available(&self) -> bool {
        self.link.consumer.load(Ordering::Acquire)
    }
    pub fn reserve(&mut self) -> Publication {
        if !self.link.consumer.load(Ordering::Acquire) {
            return Publication::Closed;
        }
        if self.reserved.is_none() {
            self.reserved = self.free.try_recv();
        }
        if self.reserved.is_some() {
            Publication::Pending
        } else {
            Publication::Backpressure
        }
    }

    pub fn publish(&mut self, decoded: DecodedTti<'_>) {
        let mut packet = self
            .reserved
            .take()
            .expect("output reserved before decoding");
        let payload = decoded.received.payload.unwrap_or(&[]);
        packet.payload[..payload.len()].copy_from_slice(payload);
        packet.report = Some(Report {
            timing: decoded.timing,
            identity: decoded.identity,
            start: decoded.start,
            absolute_tti: decoded.absolute_tti,
            control: decoded.control,
            feedback: decoded.received.feedback,
            outcome: decoded.received.outcome,
            combined: decoded.received.combined,
            reset_reason: decoded.received.reset_reason,
            payload_bits: payload.len(),
        });
        // Reserving one of the fixed pool slots guarantees room in the equally sized ready queue.
        self.link.outstanding.fetch_add(1, Ordering::AcqRel);
        packet.published_at = Some(Instant::now());
        assert!(
            self.ready.try_send(packet).is_ok(),
            "reserved HSUPA output capacity"
        );
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        self.link.producer.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use umts_dsp::hsupa::ControlInformation;
    use umts_types::{
        RadioEpoch, UeKey,
        hsupa::{Etfci, Rsn},
    };

    #[test]
    fn reserved_pool_preserves_payload_under_backpressure_and_closure() {
        let identity = ReceiveIdentity {
            ue: UeKey {
                epoch: RadioEpoch::new(NonZeroU64::MIN),
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::MIN,
        };
        let bits = [Bit::One, Bit::Zero, Bit::One];
        let decoded = || DecodedTti {
            timing: None,
            identity,
            start: ChipInstant {
                epoch: identity.ue.epoch,
                index: 0,
            },
            absolute_tti: 0,
            control: ControlDecision {
                information: ControlInformation {
                    format: Etfci::new(1).unwrap(),
                    sequence: Rsn::new(0).unwrap(),
                    happy: true,
                },
                normalized_score: 1.0,
                normalized_margin: Some(0.5),
            },
            received: super::super::Received {
                feedback: Reception::Ack { deliver: true },
                outcome: BlockOutcome::Decoded,
                combined: false,
                reset_reason: None,
                payload: Some(&bits),
            },
        };
        let (mut delivery, mut port) = Delivery::new(
            NonZeroUsize::MIN,
            NonZeroUsize::new(8).unwrap(),
            NonZeroUsize::new(100_000).unwrap(),
        )
        .unwrap();
        assert_eq!(delivery.reserve(), Publication::Pending);
        delivery.publish(decoded());
        assert_eq!(delivery.outstanding(), 1);
        assert_eq!(delivery.reserve(), Publication::Backpressure);
        for _ in 0..2 {
            let report = port.peek().unwrap();
            assert_eq!(report.report.identity, identity);
            assert_eq!(report.payload, &bits);
        }
        assert!(port.pending_age().is_some());
        assert!(port.release());
        assert!(port.pending_age().is_none());
        assert_eq!(delivery.outstanding(), 0);
        assert!(!port.release());
        assert_eq!(delivery.reserve(), Publication::Pending);
        delivery.publish(decoded());
        drop(delivery);
        assert!(!port.producer_available());
        assert!(!port.closed_and_empty());
        assert_eq!(port.peek().unwrap().payload, &bits);
        assert!(port.release());
        assert!(port.peek().is_none());
        assert!(port.closed_and_empty());
        let (mut delivery, port) = Delivery::new(
            NonZeroUsize::MIN,
            NonZeroUsize::MIN,
            NonZeroUsize::new(100_000).unwrap(),
        )
        .unwrap();
        drop(port);
        assert_eq!(delivery.reserve(), Publication::Closed);
        assert!(
            Delivery::new(
                NonZeroUsize::new(MAX_REPORTS + 1).unwrap(),
                NonZeroUsize::MIN,
                NonZeroUsize::new(100_000).unwrap()
            )
            .is_err()
        );
        assert!(Delivery::new(NonZeroUsize::MIN, NonZeroUsize::MIN, NonZeroUsize::MIN).is_err());
    }
}
