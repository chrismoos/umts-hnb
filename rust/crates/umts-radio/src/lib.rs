// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod clock_model;
pub mod pacing;
pub mod transmit;

use std::num::NonZeroU64;

use umts_types::RadioEpoch;
use umts_types::time::{SampleClock, SampleInstant, SampleRate, TimeError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Discontinuity {
    pub reported_overflow: bool,
    pub tick_delta: i128,
}

#[derive(Clone, Copy, Debug)]
pub struct RxObservation {
    pub first: SampleInstant,
    pub tick: u64,
    pub samples: usize,
    // Only discarded_prefix..samples may advance the receiver's existing timeline.
    pub discarded_prefix: usize,
    pub discontinuity: Option<Discontinuity>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxOverlapCounters {
    pub blocks: u64,
    pub samples: u64,
    pub last: Option<RxOverlap>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxOverlap {
    pub expected_tick: u64,
    pub received_tick: u64,
    pub received_samples: usize,
    pub discarded_prefix: usize,
}

pub struct RxTimeline {
    epoch: RadioEpoch,
    rate: SampleRate,
    tick_rate: NonZeroU64,
    segment: Option<SampleClock>,
    next_sample: u64,
    pending_overflow: bool,
    overlaps: RxOverlapCounters,
}

impl RxTimeline {
    pub fn from_hal(epoch: RadioEpoch, rx: &dyn sdr_hal::RadioRx) -> Result<Self, TimeError> {
        Ok(Self {
            epoch,
            rate: SampleRate::from_hz(rx.rx_sample_rate_hz())?,
            tick_rate: NonZeroU64::new(rx.tick_rate()).ok_or(TimeError::InvalidRate)?,
            segment: None,
            next_sample: 0,
            pending_overflow: false,
            overlaps: RxOverlapCounters::default(),
        })
    }

    pub fn overlaps(&self) -> RxOverlapCounters {
        self.overlaps
    }

    pub fn observe(
        &mut self,
        result: sdr_hal::RxReadResult,
    ) -> Result<Option<RxObservation>, TimeError> {
        self.pending_overflow |= result.overflow;
        if result.samples_read == 0 {
            return Ok(None);
        }
        let expected_first = SampleInstant {
            epoch: self.epoch,
            index: self.next_sample,
        };
        let expected = self
            .segment
            .map(|clock| clock.tick_at(expected_first))
            .transpose()?;
        let tick_delta =
            expected.map_or(0, |tick| i128::from(result.time_ticks) - i128::from(tick));
        let first = match self.segment {
            Some(clock) => clock.sample_at_tick(result.time_ticks)?,
            None => expected_first,
        };
        let discarded_prefix = usize::try_from(
            self.next_sample
                .saturating_sub(first.index)
                .min(u64::try_from(result.samples_read).map_err(|_| TimeError::Overflow)?),
        )
        .map_err(|_| TimeError::Overflow)?;
        if discarded_prefix != 0 {
            self.overlaps.blocks = self.overlaps.blocks.saturating_add(1);
            self.overlaps.samples = self
                .overlaps
                .samples
                .saturating_add(discarded_prefix as u64);
            self.overlaps.last = Some(RxOverlap {
                expected_tick: expected.ok_or(TimeError::BeforeOrigin)?,
                received_tick: result.time_ticks,
                received_samples: result.samples_read,
                discarded_prefix,
            });
            if discarded_prefix == result.samples_read {
                return Ok(None);
            }
        }
        let next_sample = first
            .index
            .checked_add(u64::try_from(result.samples_read).map_err(|_| TimeError::Overflow)?)
            .ok_or(TimeError::Overflow)?;
        let retained_first = first.index + discarded_prefix as u64;
        let discontinuity = (self.pending_overflow || retained_first != expected_first.index)
            .then_some(Discontinuity {
                reported_overflow: self.pending_overflow,
                tick_delta,
            });
        if self.segment.is_none() {
            self.segment = Some(SampleClock::new(
                first,
                result.time_ticks,
                self.rate,
                self.tick_rate,
            ));
        }
        self.next_sample = next_sample;
        self.pending_overflow = false;
        Ok(Some(RxObservation {
            first,
            tick: result.time_ticks,
            samples: result.samples_read,
            discarded_prefix,
            discontinuity,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use num_complex::Complex32;
    use sdr_hal::{
        Radio,
        memory::{IqBlock, MemoryRadio},
    };

    use super::*;

    #[test]
    fn hal_partial_reads_preserve_valid_prefix_clock_and_gap_evidence() {
        let (mut radio, peer) = MemoryRadio::new(2);
        radio.set_tx_sample_rate(2_000_000.0).unwrap();
        radio
            .setup_rx(0, "", 2_000_000.0, 2_000_000.0, None)
            .unwrap();
        let (_tx, rx) = Box::new(radio).split().unwrap();
        let mut rx = rx.unwrap();
        rx.rx_activate(None).unwrap();
        let mut timeline = RxTimeline::from_hal(RadioEpoch::new(NonZeroU64::MIN), &*rx).unwrap();
        peer.input
            .send(IqBlock {
                samples: vec![Complex32::new(0.25, -0.5); 5],
                time_ticks: Some(10_000),
                discontinuity: false,
            })
            .unwrap();
        let mut samples = [Complex32::new(99.0, 99.0); 3];
        let first = timeline
            .observe(rx.rx_read(&mut samples, 1_000).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.samples, first.tick, first.first.index),
            (3, 10_000, 0)
        );
        assert_eq!(first.discontinuity, None);
        let second = timeline
            .observe(rx.rx_read(&mut samples, 1_000).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            (second.samples, second.tick, second.first.index),
            (2, 11_500, 3)
        );
        assert_eq!(second.discontinuity, None);
        peer.input
            .send(IqBlock {
                samples: vec![Complex32::new(0.1, 0.2)],
                time_ticks: Some(14_000),
                discontinuity: true,
            })
            .unwrap();
        let gap = timeline
            .observe(rx.rx_read(&mut samples, 1_000).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            gap.discontinuity,
            Some(Discontinuity {
                reported_overflow: true,
                tick_delta: 1_500
            })
        );
        assert_eq!(gap.samples, 1);
        assert_eq!(gap.first.index, 8);
        assert!(
            timeline
                .observe(sdr_hal::RxReadResult {
                    samples_read: 1,
                    time_ticks: 14_000,
                    overflow: false,
                })
                .unwrap()
                .is_none()
        );
        assert_eq!(timeline.overlaps().samples, 1);
        rx.stop().unwrap();
    }

    #[test]
    fn quantized_timestamps_do_not_create_gaps_and_empty_overflow_is_not_lost() {
        let (mut radio, _peer) = MemoryRadio::new(1);
        radio
            .setup_rx(0, "", 7_680_000.0, 5_000_000.0, None)
            .unwrap();
        let (_tx, rx) = Box::new(radio).split().unwrap();
        let mut timeline =
            RxTimeline::from_hal(RadioEpoch::new(NonZeroU64::MIN), &*rx.unwrap()).unwrap();
        let first = timeline
            .observe(sdr_hal::RxReadResult {
                samples_read: 3,
                time_ticks: 0,
                overflow: false,
            })
            .unwrap()
            .unwrap();
        assert_eq!(first.discontinuity, None);
        let quantized = timeline
            .observe(sdr_hal::RxReadResult {
                samples_read: 3,
                time_ticks: 390,
                overflow: false,
            })
            .unwrap()
            .unwrap();
        assert_eq!(quantized.first.index, 3);
        assert_eq!(quantized.discontinuity, None);
        assert!(
            timeline
                .observe(sdr_hal::RxReadResult {
                    samples_read: 0,
                    time_ticks: 0,
                    overflow: true,
                })
                .unwrap()
                .is_none()
        );
        let resumed = timeline
            .observe(sdr_hal::RxReadResult {
                samples_read: 2,
                time_ticks: 781,
                overflow: false,
            })
            .unwrap()
            .unwrap();
        assert_eq!(resumed.first.index, 6);
        assert!(resumed.discontinuity.unwrap().reported_overflow);
    }

    #[test]
    fn overlapping_packets_trim_only_duplicates_and_keep_the_original_frontier() {
        let (mut radio, _peer) = MemoryRadio::new(1);
        radio
            .setup_rx(0, "", 1_000_000.0, 1_000_000.0, None)
            .unwrap();
        let (_, rx) = Box::new(radio).split().unwrap();
        let mut timeline =
            RxTimeline::from_hal(RadioEpoch::new(NonZeroU64::MIN), &*rx.unwrap()).unwrap();
        let read = |tick, samples_read, overflow| sdr_hal::RxReadResult {
            time_ticks: tick,
            samples_read,
            overflow,
        };
        let first = timeline.observe(read(10_000, 8, false)).unwrap().unwrap();
        assert_eq!(first.discarded_prefix, 0);
        let partial = timeline.observe(read(15_000, 8, false)).unwrap().unwrap();
        assert_eq!(
            (
                partial.first.index,
                partial.samples,
                partial.discarded_prefix
            ),
            (5, 8, 3)
        );
        assert!(partial.discontinuity.is_none());
        assert!(timeline.observe(read(15_000, 8, true)).unwrap().is_none());
        let next = timeline.observe(read(23_000, 2, false)).unwrap().unwrap();
        assert_eq!((next.first.index, next.discarded_prefix), (13, 0));
        assert!(next.discontinuity.unwrap().reported_overflow);
        assert_eq!(
            timeline.overlaps(),
            RxOverlapCounters {
                blocks: 2,
                samples: 11,
                last: Some(RxOverlap {
                    expected_tick: 23_000,
                    received_tick: 15_000,
                    received_samples: 8,
                    discarded_prefix: 8
                }),
            }
        );
        assert!(timeline.observe(read(9_000, 1, false)).is_err());
    }
}
