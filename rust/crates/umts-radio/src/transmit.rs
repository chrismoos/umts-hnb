// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::{NonZeroU64, NonZeroUsize};

use num_complex::Complex32;
use sdr_hal::{DeviceTime, RadioTx, Schedule, TxCapability, TxMetadata};
use umts_types::time::{SampleClock, SampleInstant, SampleRate, TimeError};

#[derive(Debug)]
pub enum PrepareError {
    Time(TimeError),
    Driver(sdr_hal::error::Error),
    TimedSamplesRequired,
    Lead(crate::pacing::LeadError),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubmitFailureKind {
    NotActive,
    UnknownProgress,
    BlockTooLarge,
    InvalidTime(TimeError),
    Overlap,
    NoProgress,
    CallBudget,
    Driver(sdr_hal::error::ErrorKind),
}

#[derive(Debug)]
pub struct SubmitFailure {
    pub kind: SubmitFailureKind,
    pub accepted_samples: Option<usize>,
    pub source: Option<sdr_hal::error::Error>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Prepared,
    Active,
    Uncertain,
    Stopped,
}

pub struct TxSubmission {
    uncertain_end: Option<SampleInstant>,
    tick_rate: NonZeroU64,
    tx: Box<dyn RadioTx>,
    clock: SampleClock,
    max_samples: NonZeroUsize,
    max_calls: NonZeroUsize,
    next: SampleInstant,
    state: State,
}

impl TxSubmission {
    /// origin_tick is the nominal TX-clock time before the HAL calibration correction.
    pub fn prepare(
        mut tx: Box<dyn RadioTx>,
        origin: SampleInstant,
        origin_tick: u64,
        max_samples: NonZeroUsize,
        max_calls: NonZeroUsize,
    ) -> Result<Self, PrepareError> {
        if !tx.capabilities().supports(TxCapability::TimedSamples) {
            return Err(PrepareError::TimedSamplesRequired);
        }
        let tick_rate =
            NonZeroU64::new(tx.tick_rate()).ok_or(PrepareError::Time(TimeError::InvalidRate))?;
        let clock = SampleClock::new(
            origin,
            origin_tick,
            SampleRate::from_hz(tx.tx_sample_rate_hz()).map_err(PrepareError::Time)?,
            tick_rate,
        )
        .with_correction_ns(tx.rx_to_tx_delay_ns().unwrap_or(0))
        .map_err(PrepareError::Time)?;
        tx.prepare_transmit(max_samples.get())
            .map_err(PrepareError::Driver)?;
        Ok(Self {
            uncertain_end: None,
            tick_rate,
            tx,
            clock,
            max_samples,
            max_calls,
            next: origin,
            state: State::Prepared,
        })
    }

    pub fn start(&mut self) -> Result<(), sdr_hal::error::Error> {
        if self.state != State::Prepared {
            return Err(sdr_hal::error::Error::invalid_setting(
                "TX owner is not prepared",
            ));
        }
        self.tx.enable_transmit(true)?;
        self.state = State::Active;
        Ok(())
    }

    pub fn tick_at(&self, sample: SampleInstant) -> Result<u64, TimeError> {
        self.clock.tick_at(sample)
    }

    pub fn hardware_time(&self) -> Result<u64, sdr_hal::error::Error> {
        self.tx.get_hardware_time()
    }

    pub fn clock_reader(&self) -> Option<Box<dyn sdr_hal::RadioClock>> {
        self.tx.clock_reader()
    }

    pub fn health(&mut self) -> Result<sdr_hal::TxRadioHealth, sdr_hal::error::Error> {
        self.tx.tx_health()
    }

    pub fn tick_rate(&self) -> NonZeroU64 {
        self.tick_rate
    }

    pub fn is_active(&self) -> bool {
        self.state == State::Active
    }

    // Discard the entire uncertain call interval before another timed write, never replay its possible prefix.
    pub fn discard_unknown(&mut self, end: SampleInstant) -> bool {
        if self.state != State::Uncertain || self.uncertain_end != Some(end) {
            return false;
        }
        self.next = end;
        self.uncertain_end = None;
        self.state = State::Active;
        true
    }

    pub fn submit(
        &mut self,
        first: SampleInstant,
        samples: &[Complex32],
    ) -> Result<usize, SubmitFailure> {
        let fail = |kind, accepted_samples, source| SubmitFailure {
            kind,
            accepted_samples,
            source,
        };
        if self.state == State::Uncertain {
            return Err(fail(SubmitFailureKind::UnknownProgress, None, None));
        }
        if self.state != State::Active {
            return Err(fail(SubmitFailureKind::NotActive, Some(0), None));
        }
        if samples.len() > self.max_samples.get() {
            return Err(fail(SubmitFailureKind::BlockTooLarge, Some(0), None));
        }
        let first_tick = self
            .clock
            .tick_at(first)
            .map_err(|e| fail(SubmitFailureKind::InvalidTime(e), Some(0), None))?;
        if first.index < self.next.index {
            return Err(fail(SubmitFailureKind::Overlap, Some(0), None));
        }
        let end = first
            .index
            .checked_add(samples.len() as u64)
            .ok_or_else(|| {
                fail(
                    SubmitFailureKind::InvalidTime(TimeError::Overflow),
                    Some(0),
                    None,
                )
            })?;
        self.clock
            .tick_at(SampleInstant {
                index: end,
                ..first
            })
            .map_err(|e| fail(SubmitFailureKind::InvalidTime(e), Some(0), None))?;
        if samples.is_empty() {
            return Ok(0);
        }
        let mut accepted = 0;
        for _ in 0..self.max_calls.get() {
            let position = SampleInstant {
                index: first.index + accepted as u64,
                ..first
            };
            let tick = if accepted == 0 {
                first_tick
            } else {
                self.clock
                    .tick_at(position)
                    .expect("complete interval already validated")
            };
            let written = match self.tx.write(
                &samples[accepted..],
                TxMetadata {
                    schedule: Schedule::At(DeviceTime::from_ticks(tick)),
                    end_of_burst: false,
                },
            ) {
                Ok(n) if n <= samples.len() - accepted => n,
                Ok(_) => {
                    self.state = State::Uncertain;
                    return Err(fail(SubmitFailureKind::UnknownProgress, None, None));
                }
                Err(error) => {
                    let kind = error.kind();
                    let progress = error
                        .accepted_samples()
                        .filter(|&n| n <= samples.len() - accepted);
                    if let Some(progress) = progress {
                        self.next = SampleInstant {
                            index: position.index + progress as u64,
                            ..first
                        };
                        return Err(fail(
                            SubmitFailureKind::Driver(kind),
                            Some(accepted + progress),
                            Some(error),
                        ));
                    }
                    self.state = State::Uncertain;
                    self.uncertain_end = matches!(
                        kind,
                        sdr_hal::error::ErrorKind::Late
                            | sdr_hal::error::ErrorKind::Timeout
                            | sdr_hal::error::ErrorKind::IncompleteWrite
                    )
                    .then_some(SampleInstant {
                        index: end,
                        ..first
                    });
                    return Err(fail(SubmitFailureKind::Driver(kind), None, Some(error)));
                }
            };
            if written == 0 {
                return Err(fail(SubmitFailureKind::NoProgress, Some(accepted), None));
            }
            accepted += written;
            self.next = SampleInstant {
                index: first.index + accepted as u64,
                ..first
            };
            if accepted == samples.len() {
                return Ok(accepted);
            }
        }
        Err(fail(SubmitFailureKind::CallBudget, Some(accepted), None))
    }

    pub fn stop(&mut self) -> Result<(), sdr_hal::error::Error> {
        if self.state == State::Stopped {
            return Ok(());
        }
        self.tx.stop()?;
        self.state = State::Stopped;
        Ok(())
    }
}

impl Drop for TxSubmission {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Writes = Arc<Mutex<Vec<(u64, usize, bool)>>>;
    use sdr_hal::error::{Error, ErrorKind};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    use umts_types::RadioEpoch;

    struct ScriptedTx {
        results: VecDeque<Result<usize, Error>>,
        writes: Writes,
    }

    impl RadioTx for ScriptedTx {
        fn supports_timed_tx(&self) -> bool {
            true
        }
        fn tx_sample_rate_hz(&self) -> f64 {
            7_680_000.0
        }
        fn tick_rate(&self) -> u64 {
            30_720_000
        }
        fn get_hardware_time(&self) -> Result<u64, Error> {
            Ok(0)
        }
        fn transmit(&mut self, _: &[Complex32]) -> Result<(), Error> {
            panic!("must use timed write")
        }
        fn enable_transmit(&mut self, _: bool) -> Result<(), Error> {
            Ok(())
        }
        fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
            let Schedule::At(time) = metadata.schedule else {
                panic!("missing timestamp")
            };
            self.writes
                .lock()
                .unwrap()
                .push((time.ticks(), samples.len(), metadata.end_of_burst));
            self.results.pop_front().unwrap_or(Ok(samples.len()))
        }
    }

    fn at(index: u64) -> SampleInstant {
        SampleInstant {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            index,
        }
    }

    fn scripted(results: Vec<Result<usize, Error>>, max_calls: usize) -> (TxSubmission, Writes) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let tx = ScriptedTx {
            results: results.into(),
            writes: Arc::clone(&writes),
        };
        let mut submission = TxSubmission::prepare(
            Box::new(tx),
            at(0),
            100,
            NonZeroUsize::new(100).unwrap(),
            NonZeroUsize::new(max_calls).unwrap(),
        )
        .unwrap();
        submission.start().unwrap();
        (submission, writes)
    }

    #[test]
    fn accepted_prefixes_advance_device_ticks_without_per_slot_burst_end() {
        let (mut tx, writes) = scripted(vec![Ok(2), Ok(3)], 4);
        assert_eq!(tx.submit(at(0), &[Complex32::default(); 5]).unwrap(), 5);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![(100, 5, false), (108, 3, false)]
        );
        assert!(matches!(
            tx.submit(at(4), &[Complex32::default()]),
            Err(SubmitFailure {
                kind: SubmitFailureKind::Overlap,
                ..
            })
        ));
    }

    #[test]
    fn call_budget_is_bounded_and_reports_the_exact_retry_position() {
        let (mut tx, writes) = scripted(vec![Ok(1), Ok(1)], 2);
        let error = tx.submit(at(0), &[Complex32::default(); 3]).unwrap_err();
        assert_eq!(error.kind, SubmitFailureKind::CallBudget);
        assert_eq!(error.accepted_samples, Some(2));
        assert_eq!(writes.lock().unwrap().len(), 2);
        assert_eq!(tx.submit(at(2), &[Complex32::default()]).unwrap(), 1);
    }

    #[test]
    fn unknown_progress_poisoning_prevents_blind_resubmission() {
        let (mut tx, writes) = scripted(
            vec![Err(Error::new(ErrorKind::Disconnected, "injected"))],
            4,
        );
        let error = tx.submit(at(0), &[Complex32::default(); 3]).unwrap_err();
        assert_eq!(error.accepted_samples, None);
        let retry = tx.submit(at(0), &[Complex32::default(); 3]).unwrap_err();
        assert_eq!(retry.kind, SubmitFailureKind::UnknownProgress);
        assert!(!tx.discard_unknown(at(3)));
        assert_eq!(writes.lock().unwrap().len(), 1);
        tx.stop().unwrap();
    }

    #[test]
    fn known_native_failure_never_retries_an_accepted_prefix() {
        let error = Error::new(ErrorKind::IncompleteWrite, "injected").with_accepted_samples(2);
        let (mut tx, _) = scripted(vec![Err(error)], 4);
        assert_eq!(
            tx.submit(at(0), &[Complex32::default(); 3])
                .unwrap_err()
                .accepted_samples,
            Some(2)
        );
        assert_eq!(tx.submit(at(2), &[Complex32::default()]).unwrap(), 1);
    }

    #[test]
    fn late_unknown_prefix_can_only_resume_after_disposing_the_entire_attempted_interval() {
        let (mut tx, writes) = scripted(vec![Err(Error::new(ErrorKind::Late, "late"))], 1);
        let failure = tx.submit(at(0), &[Complex32::default(); 3]).unwrap_err();
        assert_eq!(failure.accepted_samples, None);
        assert!(!tx.discard_unknown(at(2)));
        assert!(!tx.discard_unknown(at(4)));
        assert!(tx.discard_unknown(at(3)));
        assert!(tx.is_active());
        assert!(matches!(
            tx.submit(at(2), &[Complex32::default()]),
            Err(SubmitFailure {
                kind: SubmitFailureKind::Overlap,
                ..
            })
        ));
        assert_eq!(tx.submit(at(3), &[Complex32::default()]).unwrap(), 1);
        assert_eq!(*writes.lock().unwrap(), [(100, 3, false), (112, 1, false)]);
    }
}
