// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    access::AccessObservation,
    access_rx::PrachWindowReceiver,
    test_support::{at, count, source},
};
use sdr_hal::{
    Radio, RxReadResult, Schedule, TxCapabilities, TxCapability, TxMetadata,
    error::{Error, ErrorKind},
    memory::{IqBlock, MemoryRadio},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use umts_dsp::{
    prach::{PrachCode, PreambleSignature},
    prach_search::PreambleSearchPlan,
    pulse_shaping::PulseShaper,
};

use super::*;

mod sch;

#[test]
fn actual_mapped_am_status_requires_driver_acceptance_of_the_entire_frame_and_pulse_tail() {
    use crate::common_am::CommonAmBearer;
    use umts_mac::{CommonHeader, CommonIdentity, CommonPdu, LogicalChannelId};
    use umts_rlc::acknowledged::{AmEntity, AmLimits, PublicationKind};
    use umts_rrc::establishment::SignallingProfile;
    use umts_types::{
        UeKey,
        bits::{BITS_PER_BYTE, Bits},
    };

    let mut source = source(false);
    let key = UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    };
    let (tx_size, rx_size) = CommonAmBearer::sizes(&source, 1).unwrap();
    let limits = AmLimits {
        maximum_sdu_bytes: count(1024),
        queued_sdus: count(4),
        maximum_buffer_bytes: 1024 * 1024,
    };
    let mut bearer = CommonAmBearer::new(
        &source,
        1,
        key,
        1,
        LogicalChannelId::new(2).unwrap(),
        crate::common_am::CommonAmSettings {
            config: SignallingProfile.network_am_configuration(tx_size, rx_size, 0, 0),
            limits,
            started_at: at(0),
        },
    )
    .unwrap();
    let mut sender = AmEntity::new(
        SignallingProfile.network_am_configuration(rx_size, tx_size, 0, 0),
        limits,
        at(0),
    )
    .unwrap();
    sender.enqueue(b"completed").unwrap();
    sender
        .publish_next(at(0), |publication| {
            bearer.receive(
                key,
                at(0),
                CommonPdu {
                    header: CommonHeader::Dedicated {
                        identity: CommonIdentity::Cell(1),
                        channel: LogicalChannelId::new(2).unwrap(),
                    },
                    payload: Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE)
                        .unwrap(),
                },
            )
        })
        .unwrap()
        .unwrap();
    let coverage = bearer
        .deliver_with(key, |delivery| {
            Ok::<_, ()>(delivery.covered_sdus().next().unwrap().1)
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        bearer.publish_frame_mapped(
            &mut source,
            at(0),
            at(0),
            |position| Ok::<_, ()>(position as u32),
            |_, _| Ok(())
        ),
        Ok(Some(PublicationKind::Status))
    );
    let status = bearer.status_submission(key).unwrap().unwrap().clone();
    assert!(status.coverage.covers(coverage));
    assert!(!status.transport.submitted());
    let (mut radio, peer) = MemoryRadio::new(1);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source,
        tx,
        TxSettings::offline(1000, count(1), count(1), count(2)),
    )
    .unwrap();
    pump.start().unwrap();
    for slot in 0..16 {
        preparation.prepare_one().unwrap().unwrap();
        assert!(!status.transport.submitted());
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        peer.output.recv().unwrap();
        assert_eq!(status.transport.submitted(), slot == 15);
    }
    assert_eq!(status.transport.air_end().epoch, key.epoch);
    assert!(status.transport.air_end().index > umts_types::time::CHIPS_PER_FRAME);
    pump.stop().unwrap();
    assert!(!status.transport.submitted());
}

#[test]
fn a_new_submission_owner_cannot_confirm_samples_generated_before_its_origin() {
    let mut source = source(false);
    let old = source.transport_submission(at(0)).unwrap();
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    source.generate_slot(&mut samples).unwrap();
    let tx = ScriptedTx {
        outcomes: VecDeque::new(),
        writes: Arc::new(Mutex::new(Vec::new())),
        calibration_ns: None,
    };
    let (mut preparation, mut pump) = prepare_tx(
        source,
        Box::new(tx),
        TxSettings::offline(1000, count(1), count(1), count(2)),
    )
    .unwrap();
    pump.start().unwrap();
    for _ in 0..16 {
        preparation.prepare_one().unwrap().unwrap();
        pump.cycle().unwrap();
    }
    assert!(!old.submitted());
}

#[test]
fn hal_memory_submission_recycles_a_fixed_pool_and_preserves_every_slot_timestamp() {
    let (mut radio, peer) = MemoryRadio::new(1);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut producer, mut pump) = prepare_tx(
        source(false),
        tx,
        TxSettings::offline(1_000_000_000, count(2), count(2), count(3)),
    )
    .unwrap();
    producer.prepare_one().unwrap().unwrap();
    producer.prepare_one().unwrap().unwrap();
    assert!(producer.prepare_one().unwrap().is_none());
    pump.start().unwrap();
    let mut total = 0;
    for slot in 0..60 {
        let PumpProgress::Submitted(window) = pump.cycle().unwrap() else {
            panic!("prepared slot not submitted");
        };
        let block = peer.output.recv().unwrap();
        let expected_tick =
            1_000_000_000 + ((slot as f64 * 5120.0 / 7_680_000.0) * 1e9).round() as u64;
        assert_eq!(block.time_ticks, Some(expected_tick));
        assert_eq!(block.samples.len(), 5120);
        assert_eq!(window.first_sample.index, slot * 5120);
        assert_eq!(window.next_sample.index, (slot + 1) * 5120);
        assert!(
            block
                .samples
                .iter()
                .all(|sample| sample.re.is_finite() && sample.im.is_finite())
        );
        total += block.samples.len();
        assert!(producer.prepare_one().unwrap().is_some());
    }
    assert_eq!(total, 60 * 5120);
    pump.stop().unwrap();
}

type Writes = Arc<Mutex<Vec<(u64, usize, bool)>>>;

#[test]
fn two_slot_batches_reject_a_source_between_aich_access_boundaries() {
    let mut source = source(false);
    let mut output = vec![Complex32::default(); source.samples_per_slot()];
    source.generate_slot(&mut output).unwrap();
    assert!(matches!(
        prepare_tx_batched(
            source,
            applied_tx(7_680_000.0),
            TxSettings::offline(1000, count(1), count(1), count(3)),
            TxBatch::TwoSlots,
        ),
        Err(PrepareError::Lead(LeadError::InvalidWindow))
    ));
}

struct ScriptedTx {
    outcomes: VecDeque<Result<usize, Error>>,
    writes: Writes,
    calibration_ns: Option<i64>,
}
impl RadioTx for ScriptedTx {
    fn capabilities(&self) -> TxCapabilities {
        TxCapabilities::from([TxCapability::TimedSamples])
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        7_680_000.0
    }
    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        self.calibration_ns
    }
    fn tick_rate(&self) -> u64 {
        30_720_000
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(0)
    }
    fn transmit(&mut self, _: &[Complex32]) -> Result<(), Error> {
        panic!("untimed submission");
    }
    fn enable_transmit(&mut self, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
        let Schedule::At(tick) = metadata.schedule else {
            panic!("untimed submission");
        };
        self.writes
            .lock()
            .unwrap()
            .push((tick.ticks(), samples.len(), metadata.end_of_burst));
        self.outcomes.pop_front().unwrap_or(Ok(samples.len()))
    }
}

fn scripted(
    outcomes: Vec<Result<usize, Error>>,
    attempts: usize,
) -> (TxPreparation, TxPump, Writes) {
    scripted_batch(outcomes, attempts, TxBatch::OneSlot)
}

fn scripted_batch(
    outcomes: Vec<Result<usize, Error>>,
    attempts: usize,
    batch: TxBatch,
) -> (TxPreparation, TxPump, Writes) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let tx = ScriptedTx {
        outcomes: outcomes.into(),
        writes: writes.clone(),
        calibration_ns: None,
    };
    let (mut preparation, mut pump) = prepare_tx_batched(
        source(true),
        Box::new(tx),
        TxSettings::offline(1000, count(1), count(1), count(attempts)),
        batch,
    )
    .unwrap();
    preparation.prepare_one().unwrap();
    pump.start().unwrap();
    (preparation, pump, writes)
}

#[test]
fn two_slot_partial_writes_retain_one_contiguous_batch_until_complete() {
    let (mut preparation, mut pump, writes) =
        scripted_batch(vec![Ok(100), Ok(200)], 4, TxBatch::TwoSlots);
    for _ in 0..2 {
        assert!(matches!(
            pump.cycle().unwrap(),
            PumpProgress::Partial { .. }
        ));
        assert!(preparation.prepare_one().unwrap().is_none());
    }
    let PumpProgress::Submitted(window) = pump.cycle().unwrap() else {
        panic!("complete two-slot batch")
    };
    assert_eq!(window.first_sample.index, 0);
    assert_eq!(window.next_sample.index, 10240);
    let writes = writes.lock().unwrap();
    assert_eq!(
        writes.iter().map(|value| value.1).collect::<Vec<_>>(),
        [10240, 10140, 9940]
    );
    for (write, offset) in writes.iter().zip([0, 100, 300]) {
        let sample = SampleInstant {
            index: offset,
            ..window.first_sample
        };
        assert_eq!(write.0, pump.submission.tick_at(sample).unwrap());
    }
}

#[test]
fn optional_wall_timing_counts_initial_queueing_once_across_partial_writes() {
    let (mut preparation, mut pump, _) = scripted(vec![Ok(100), Ok(200)], 4);
    assert!(preparation.timings().is_none());
    assert!(pump.timings().is_none());
    preparation.enable_timing();
    pump.enable_timing();
    for _ in 0..2 {
        assert!(matches!(
            pump.cycle().unwrap(),
            PumpProgress::Partial { .. }
        ));
    }
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    let timings = pump.timings().unwrap();
    assert_eq!(timings.cycle.calls, 3);
    assert_eq!(timings.write.calls, 3);
    assert_eq!(timings.pending.calls, 1);
    assert_eq!(timings.ready_queue.calls, 0);
    assert_eq!(timings.submitted, 1);
    assert!(preparation.prepare_one().unwrap().is_some());
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    assert_eq!(pump.timings().unwrap().ready_queue.calls, 1);
    assert_eq!(pump.timings().unwrap().pending.calls, 2);
    let timings = preparation.timings().unwrap();
    assert_eq!(timings.published, 1);
    assert_eq!(timings.synthesis.calls, 1);
    assert_eq!(timings.cycle.calls, 1);
    preparation.enable_timing();
    assert_eq!(preparation.timings(), Some(timings));
}

#[test]
fn wall_timing_is_retained_on_submission_failure_and_stays_disabled_by_default() {
    let (_preparation, mut pump, _) = scripted(
        vec![Err(Error::new(ErrorKind::Disconnected, "injected"))],
        3,
    );
    pump.enable_timing();
    assert!(matches!(pump.cycle(), Err(PumpError::Submission(_))));
    let timings = pump.timings().unwrap();
    assert_eq!(timings.write.calls, 1);
    assert_eq!(timings.cycle.calls, 1);
    assert_eq!(timings.submitted, 0);
    let (mut preparation, mut pump, _) = scripted(Vec::new(), 3);
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    preparation.prepare_one().unwrap().unwrap();
    assert!(preparation.timings().is_none());
    assert!(pump.timings().is_none());
}

#[test]
fn partial_submissions_retain_exact_prefixes_and_never_regenerate_or_replay_them() {
    let (mut preparation, mut pump, writes) =
        scripted(vec![Ok(100), Err(Error::incomplete_write(200, 5020))], 4);
    assert_eq!(
        pump.cycle().unwrap(),
        PumpProgress::Partial {
            accepted: 100,
            total: 5120
        }
    );
    assert!(preparation.prepare_one().unwrap().is_none());
    assert_eq!(
        pump.cycle().unwrap(),
        PumpProgress::Partial {
            accepted: 300,
            total: 5120
        }
    );
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    assert!(preparation.prepare_one().unwrap().is_some());
    assert_eq!(
        *writes.lock().unwrap(),
        vec![
            (1000, 5120, false),
            (1400, 5020, false),
            (2200, 4820, false)
        ]
    );
}

#[test]
fn disconnection_stops_but_retry_exhaustion_discards_only_the_affected_batch() {
    let (mut preparation, mut pump, writes) = scripted(
        vec![Err(Error::new(ErrorKind::Disconnected, "injected"))],
        3,
    );
    assert!(matches!(pump.cycle(), Err(PumpError::Submission(_))));
    assert!(matches!(pump.cycle(), Err(PumpError::Poisoned)));
    assert!(matches!(
        preparation.prepare_one(),
        Err(PreparationError::Stopped)
    ));
    assert_eq!(writes.lock().unwrap().len(), 1);
    let (mut preparation, mut pump, writes) = scripted(vec![Ok(0), Ok(0)], 2);
    assert_eq!(
        pump.cycle().unwrap(),
        PumpProgress::Partial {
            accepted: 0,
            total: 5120
        }
    );
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Dropped { .. })));
    assert!(!preparation.stop_token().is_stopping());
    assert_eq!(pump.recovery().retries_exhausted, 1);
    assert_eq!(writes.lock().unwrap().len(), 2);
    preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    assert_eq!(writes.lock().unwrap().len(), 3);
}

#[test]
fn late_and_timeout_driver_errors_discard_unknown_prefixes_without_restarting_the_stream() {
    for kind in [ErrorKind::Late, ErrorKind::Timeout] {
        let (mut preparation, mut pump, writes) = scripted_batch(
            vec![Err(Error::new(kind, "recoverable TX event"))],
            3,
            TxBatch::TwoSlots,
        );
        let proof = preparation.source().transport_submission(at(0)).unwrap();
        assert!(matches!(
            pump.cycle().unwrap(),
            PumpProgress::Dropped { .. }
        ));
        assert!(!preparation.stop_token().is_stopping());
        assert!(pump.submission.is_active());
        assert_eq!(pump.recovery().driver_dropped, 1);
        assert!(!proof.viable());
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        assert_eq!(
            writes
                .lock()
                .unwrap()
                .iter()
                .map(|write| write.1)
                .collect::<Vec<_>>(),
            [10240, 10240]
        );
        assert!(!proof.submitted());
    }
}

#[test]
fn dropped_pipeline_owner_cancels_its_peer() {
    let (mut preparation, pump, _) = scripted(Vec::new(), 3);
    drop(pump);
    assert!(preparation.stop_token().is_stopping());
    assert!(matches!(
        preparation.prepare_one(),
        Err(PreparationError::Stopped)
    ));
    let (preparation, mut pump, _) = scripted(Vec::new(), 3);
    drop(preparation);
    assert!(matches!(pump.cycle(), Err(PumpError::Stopped)));
    assert!(pump.start().is_err());
}

#[test]
fn on_air_origin_includes_pulse_delay_and_calibration_and_rejects_foreign_tick_rates() {
    let tx = ScriptedTx {
        outcomes: VecDeque::new(),
        writes: Arc::new(Mutex::new(Vec::new())),
        calibration_ns: Some(1000),
    };
    let (preparation, _pump) = prepare_tx(
        source(false),
        Box::new(tx),
        TxSettings::offline(1000, count(1), count(1), count(1)),
    )
    .unwrap();
    let origin = preparation.air_origin();
    assert_eq!(origin.chip, at(0));
    assert_eq!(origin.tick, 1000 + 31 + 128);
    assert_eq!(origin.tick_rate.get(), 30_720_000);
    let rx = ScriptedRx {
        outcomes: VecDeque::new(),
        active: false,
    };
    let settings = RxSettings {
        rate: SamplesPerChip::Two,
        span_chips: 32,
        phase: 0,
        maximum_samples: count(100),
        timeout_us: 1000,
    };
    let wrong = AirClockOrigin {
        tick_rate: NonZeroU64::new(1_000_000_000).unwrap(),
        ..origin
    };
    assert!(matches!(
        TimedRx::new(Box::new(rx), wrong, settings),
        Err(RxError::Time(TimeError::InvalidRate))
    ));
}

struct ScriptedRx {
    outcomes: VecDeque<Result<(Vec<Complex32>, RxReadResult), Error>>,
    active: bool,
}
impl RadioRx for ScriptedRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        7_680_000.0
    }
    fn tick_rate(&self) -> u64 {
        30_720_000
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(0)
    }
    fn rx_activate(&mut self, _: Option<u64>) -> Result<(), Error> {
        self.active = true;
        Ok(())
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.active = false;
        Ok(())
    }
    fn rx_read(&mut self, buffer: &mut [Complex32], _: i64) -> Result<RxReadResult, Error> {
        assert!(self.active);
        let (samples, result) = self.outcomes.pop_front().unwrap_or_else(|| {
            Ok((
                Vec::new(),
                RxReadResult {
                    samples_read: 0,
                    time_ticks: 0,
                    overflow: false,
                },
            ))
        })?;
        buffer[..samples.len()].copy_from_slice(&samples);
        Ok(result)
    }
}

struct AppliedRateTx {
    inner: ScriptedTx,
    rate_hz: f64,
}
impl RadioTx for AppliedRateTx {
    fn capabilities(&self) -> TxCapabilities {
        self.inner.capabilities()
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        self.rate_hz
    }
    fn tick_rate(&self) -> u64 {
        self.inner.tick_rate()
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.inner.get_hardware_time()
    }
    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.inner.transmit(samples)
    }
    fn enable_transmit(&mut self, enabled: bool) -> Result<(), Error> {
        self.inner.enable_transmit(enabled)
    }
    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
        self.inner.write(samples, metadata)
    }
}
struct AppliedRateRx {
    inner: ScriptedRx,
    rate_hz: f64,
    tick_rate: u64,
}
impl RadioRx for AppliedRateRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.rate_hz
    }
    fn tick_rate(&self) -> u64 {
        self.tick_rate
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.inner.get_hardware_time()
    }
    fn rx_activate(&mut self, time: Option<u64>) -> Result<(), Error> {
        self.inner.rx_activate(time)
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.inner.stop()
    }
    fn rx_read(&mut self, samples: &mut [Complex32], timeout: i64) -> Result<RxReadResult, Error> {
        self.inner.rx_read(samples, timeout)
    }
}
fn applied_tx(rate_hz: f64) -> Box<dyn RadioTx> {
    Box::new(AppliedRateTx {
        rate_hz,
        inner: ScriptedTx {
            outcomes: VecDeque::new(),
            writes: Arc::new(Mutex::new(Vec::new())),
            calibration_ns: None,
        },
    })
}
fn applied_rx(rate_hz: f64) -> Box<dyn RadioRx> {
    Box::new(AppliedRateRx {
        rate_hz,
        tick_rate: 30_720_000,
        inner: ScriptedRx {
            outcomes: VecDeque::new(),
            active: false,
        },
    })
}
fn fractional_rx_settings() -> RxSettings {
    RxSettings {
        rate: SamplesPerChip::Two,
        span_chips: 32,
        phase: 0,
        maximum_samples: count(100),
        timeout_us: 1000,
    }
}

#[test]
fn fractional_rates_preserve_explicit_geometry_and_one_clock_over_hours() {
    const MASTER_HZ: u64 = 30_720_000;
    const NOMINAL_SAMPLE_HZ: u64 = 7_680_000;
    for rate_hz in [7_680_000.005_983_839, 7_679_999.994_016_161] {
        let actual = SampleRate::from_hz(rate_hz).unwrap();
        let settings = TxSettings {
            origin_tick: 1000,
            prepared_slots: count(1),
            maximum_calls: count(1),
            maximum_attempts: count(1),
            timing: TxTiming::DeviceClock(LeadDurations {
                minimum_submission: std::time::Duration::from_micros(100),
                maximum_submission: std::time::Duration::from_millis(2),
                maximum_preparation: std::time::Duration::from_millis(3),
            }),
        };
        let (preparation, pump) = prepare_tx(source(false), applied_tx(rate_hz), settings).unwrap();
        let origin = preparation.air_origin();
        assert_eq!(
            origin.chip_rate,
            actual.divided_by(NonZeroU64::new(2).unwrap()).unwrap()
        );
        let sample = SampleInstant {
            epoch: at(0).epoch,
            index: NOMINAL_SAMPLE_HZ * 3 * 60 * 60,
        };
        let timestamp = pump.submission.tick_at(sample).unwrap();
        assert_eq!(
            preparation
                .pacing
                .as_ref()
                .unwrap()
                .clock
                .tick_at(sample)
                .unwrap(),
            timestamp
        );
        assert_eq!(
            timestamp,
            1000 + actual
                .samples_to_ticks(sample.index, origin.tick_rate)
                .unwrap()
        );
        let nominal = 1000 + sample.index * (MASTER_HZ / NOMINAL_SAMPLE_HZ);
        assert!(timestamp.abs_diff(nominal) > 200);
        let (clock, mut advance) = RadioClock::controlled(origin, origin.tick);
        let numerator = (rate_hz * 1_000_000.0).round() as u128;
        for tick in [timestamp - 1, timestamp, timestamp + 1] {
            advance(tick).unwrap();
            let expected = u64::try_from(
                u128::from(tick - origin.tick) * numerator
                    / (u128::from(MASTER_HZ) * 2 * 1_000_000),
            )
            .unwrap();
            assert_eq!(clock.read().unwrap().chip, Some(at(expected)));
        }
        let receiver = TimedRx::new(applied_rx(rate_hz), origin, fractional_rx_settings()).unwrap();
        assert_eq!(receiver.samples_per_chip(), 2);
        assert_eq!(receiver.sample_rate, actual);
    }
}

#[test]
fn near_nominal_but_different_rx_tx_rates_and_material_coercion_are_rejected() {
    let (preparation, _pump) = prepare_tx(
        source(false),
        applied_tx(7_680_000.005_983_839),
        TxSettings::offline(1000, count(1), count(1), count(1)),
    )
    .unwrap();
    for rate in [7_679_999.994_016_161, 7_680_000.1, 7_680_001.0, f64::NAN] {
        assert!(
            TimedRx::new(
                applied_rx(rate),
                preparation.air_origin(),
                fractional_rx_settings()
            )
            .is_err()
        );
    }
    for rate in [7_680_001.0, 7_679_999.0, f64::NAN, f64::INFINITY] {
        assert!(
            prepare_tx(
                source(false),
                applied_tx(rate),
                TxSettings::offline(1000, count(1), count(1), count(1))
            )
            .is_err()
        );
    }
}

#[test]
fn quantized_rx_timestamps_preserve_one_sample_grid_across_cell_origin_rounding() {
    const RATE_HZ: f64 = 7_680_000.005_983_839;
    const ORIGIN_TICK: u64 = 34_500_085;
    const BLOCK_SAMPLES: usize = 5120;
    const FIRST_TICK: u64 = 1_925_183_502;
    const NEXT_TICK: u64 = 1_925_203_981;
    let ticks = [
        FIRST_TICK,
        NEXT_TICK,
        NEXT_TICK + 20_480,
        NEXT_TICK + 61_440,
    ];
    let rx = AppliedRateRx {
        rate_hz: RATE_HZ,
        tick_rate: 30_720_000,
        inner: ScriptedRx {
            active: false,
            outcomes: ticks
                .into_iter()
                .map(|time_ticks| {
                    Ok((
                        vec![Complex32::new(0.1, 0.2); BLOCK_SAMPLES],
                        RxReadResult {
                            samples_read: BLOCK_SAMPLES,
                            time_ticks,
                            overflow: false,
                        },
                    ))
                })
                .collect(),
        },
    };
    let actual = SampleRate::from_hz(RATE_HZ).unwrap();
    let origin = AirClockOrigin {
        chip: at(0),
        tick: ORIGIN_TICK,
        tick_rate: NonZeroU64::new(30_720_000).unwrap(),
        chip_rate: actual.divided_by(NonZeroU64::new(2).unwrap()).unwrap(),
    };
    let absolute = SampleClock::new(
        SampleInstant {
            epoch: at(0).epoch,
            index: 0,
        },
        ORIGIN_TICK,
        actual,
        origin.tick_rate,
    );
    assert_eq!(
        absolute.sample_at_tick(NEXT_TICK).unwrap().index
            - absolute.sample_at_tick(FIRST_TICK).unwrap().index,
        BLOCK_SAMPLES as u64 - 1
    );
    let mut settings = fractional_rx_settings();
    settings.maximum_samples = count(BLOCK_SAMPLES);
    let mut receiver = TimedRx::new(Box::new(rx), origin, settings).unwrap();
    receiver.start(None).unwrap();
    for expected_generation in [1, 1, 1, 2] {
        let block = receiver.read_matched().unwrap().unwrap();
        assert_eq!(block.full.generation, expected_generation);
        assert_eq!(block.full.discontinuity, expected_generation == 2);
    }
    assert_eq!(receiver.counters().discontinuities, 1);
    assert_eq!(receiver.counters().overlaps.blocks, 0);
    receiver.stop().unwrap();
}

#[test]
fn fractional_rx_metadata_retains_long_horizon_continuity_and_real_gaps() {
    for rate_hz in [7_680_000.005_983_839, 7_679_999.994_016_161] {
        let actual = SampleRate::from_hz(rate_hz).unwrap();
        let tick_rate = NonZeroU64::new(30_720_000).unwrap();
        let first = 7_680_000_u64 * 3 * 60 * 60;
        let read = |index| {
            Ok((
                vec![Complex32::new(0.1, 0.2); 100],
                RxReadResult {
                    samples_read: 100,
                    time_ticks: actual.samples_to_ticks(index, tick_rate).unwrap(),
                    overflow: false,
                },
            ))
        };
        let rx = AppliedRateRx {
            rate_hz,
            tick_rate: 30_720_000,
            inner: ScriptedRx {
                active: false,
                outcomes: vec![
                    read(first),
                    read(first + 100),
                    read(first + 300),
                    read(first + 400),
                ]
                .into(),
            },
        };
        let origin = AirClockOrigin {
            chip: at(0),
            tick: 0,
            tick_rate,
            chip_rate: actual.divided_by(NonZeroU64::new(2).unwrap()).unwrap(),
        };
        let mut receiver = TimedRx::new(Box::new(rx), origin, fractional_rx_settings()).unwrap();
        receiver.start(None).unwrap();
        let block = receiver.read_filtered().unwrap().unwrap();
        assert_eq!(block.window.first_center.unwrap().index, first + 32);
        assert_eq!(block.generation, 1);
        assert!(!block.discontinuity);
        let block = receiver.read_filtered().unwrap().unwrap();
        assert_eq!(block.generation, 1);
        assert!(!block.discontinuity);
        let block = receiver.read_filtered().unwrap().unwrap();
        assert_eq!(block.window.first_center.unwrap().index, first + 332);
        assert_eq!(block.generation, 2);
        assert!(block.discontinuity);
        let block = receiver.read_filtered().unwrap().unwrap();
        assert_eq!(block.generation, 2);
        assert!(!block.discontinuity);
        assert_eq!(receiver.samples_per_chip(), 2);
        assert_eq!(receiver.counters().discontinuities, 1);
        receiver.stop().unwrap();
    }
}

#[test]
fn nanosecond_rx_timestamps_reset_the_filter_once_per_real_gap() {
    const RATE: u64 = 7_680_000;
    const TICKS: u64 = 1_000_000_000;
    const BLOCK: usize = 5120;
    const GAP: u64 = 128;
    let resumed = BLOCK as u64 + GAP;
    let starts = std::iter::once(0)
        .chain((0..6).map(|index| resumed + index * BLOCK as u64))
        .chain(std::iter::once(resumed + 6 * BLOCK as u64 + GAP));
    let outcomes = starts
        .enumerate()
        .map(|(index, sample)| {
            let tick = if index == 0 {
                0
            } else {
                u64::try_from(
                    ((2 * u128::from(sample) + 1) * u128::from(TICKS) + u128::from(RATE))
                        / (2 * u128::from(RATE)),
                )
                .unwrap()
            };
            Ok((
                vec![Complex32::new(0.1, 0.2); BLOCK],
                RxReadResult {
                    samples_read: BLOCK,
                    time_ticks: tick,
                    overflow: index == 1,
                },
            ))
        })
        .collect();
    let rx = AppliedRateRx {
        inner: ScriptedRx {
            outcomes,
            active: false,
        },
        rate_hz: RATE as f64,
        tick_rate: TICKS,
    };
    let origin = AirClockOrigin {
        chip: at(0),
        chip_rate: SampleRate::new(CHIPS_PER_SECOND, 1).unwrap(),
        tick: 0,
        tick_rate: NonZeroU64::new(TICKS).unwrap(),
    };
    let mut settings = fractional_rx_settings();
    settings.maximum_samples = count(BLOCK);
    let mut receiver = TimedRx::new(Box::new(rx), origin, settings).unwrap();
    receiver.start(None).unwrap();
    for index in 0..8 {
        let block = receiver.read_matched().unwrap().unwrap();
        let generation = match index {
            0 => 1,
            1..=6 => 2,
            _ => 3,
        };
        assert_eq!(block.full.generation, generation);
        assert_eq!(block.full.discontinuity, index == 1 || index == 7);
    }
    let counters = receiver.counters();
    assert_eq!(counters.discontinuities, 2);
    assert_eq!(counters.reported_overflows, 1);
    assert_eq!(counters.missing_samples, 2 * GAP);
    receiver.stop().unwrap();
}

struct RateRx {
    source: ScriptedRx,
    rate: SamplesPerChip,
}
impl RadioRx for RateRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        CHIPS_PER_SECOND as f64 * self.rate.get() as f64
    }
    fn tick_rate(&self) -> u64 {
        self.source.tick_rate()
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.source.get_hardware_time()
    }
    fn rx_activate(&mut self, time: Option<u64>) -> Result<(), Error> {
        self.source.rx_activate(time)
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.source.stop()
    }
    fn rx_read(&mut self, buffer: &mut [Complex32], timeout: i64) -> Result<RxReadResult, Error> {
        self.source.rx_read(buffer, timeout)
    }
}

#[test]
fn one_hal_matched_filter_retains_full_rate_and_exact_legacy_chip_phases() {
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for phase in 0..rate.get() {
            let input: Vec<_> = (0..303)
                .map(|i| Complex32::new((i % 7) as f32 * 0.01, (i % 13) as f32 * -0.01))
                .collect();
            let mut outcomes = VecDeque::new();
            let mut offset = 0;
            for length in [73, 119, 111] {
                outcomes.push_back(Ok((
                    input[offset..offset + length].to_vec(),
                    RxReadResult {
                        samples_read: length,
                        time_ticks: (1000 + offset as u64) * (8 / rate.get()) as u64,
                        overflow: false,
                    },
                )));
                offset += length;
            }
            let source = RateRx {
                source: ScriptedRx {
                    outcomes,
                    active: false,
                },
                rate,
            };
            let mut receiver = TimedRx::new(
                Box::new(source),
                AirClockOrigin {
                    chip: at(0),
                    chip_rate: SampleRate::new(CHIPS_PER_SECOND, 1).unwrap(),
                    tick: 0,
                    tick_rate: NonZeroU64::new(30_720_000).unwrap(),
                },
                RxSettings {
                    rate,
                    span_chips: 16,
                    phase,
                    maximum_samples: count(119),
                    timeout_us: 1000,
                },
            )
            .unwrap();
            let sample = |index| SampleInstant {
                epoch: at(0).epoch,
                index,
            };
            let mut reference = ReceiveFilter::new(rate, 16, phase, sample(1000)).unwrap();
            receiver.start(None).unwrap();
            offset = 0;
            for length in [73, 119, 111] {
                let mut expected =
                    vec![Complex32::default(); reference.output_length(length).unwrap()];
                let window = reference
                    .process(
                        sample(1000 + offset as u64),
                        &input[offset..offset + length],
                        &mut expected,
                    )
                    .unwrap();
                let actual = receiver.read_matched().unwrap().unwrap();
                assert_eq!(actual.common.chips, expected);
                assert_eq!(actual.common.window, window);
                assert_eq!(actual.full.window.sample_stride, 1);
                if let Some(first) = actual.common.window.first_center {
                    let full = actual.full.window.first_center.unwrap();
                    let start = (first.index - full.index) as usize;
                    assert_eq!(
                        actual.common.chips,
                        actual.full.samples[start..]
                            .iter()
                            .step_by(rate.get())
                            .copied()
                            .collect::<Vec<_>>()
                    );
                }
                offset += length;
            }
        }
    }
}

#[test]
fn receive_clock_overflow_timeout_and_gap_reset_preserve_physical_positions() {
    let read = |first: u64, count: usize, overflow: bool| {
        Ok((
            vec![Complex32::new(0.1, 0.2); count],
            RxReadResult {
                samples_read: count,
                time_ticks: first * 4,
                overflow,
            },
        ))
    };
    let outcomes = vec![
        read(1000, 100, false),
        Err(Error::new(ErrorKind::Timeout, "idle")),
        read(1100, 0, true),
        read(1200, 100, false),
        read(1300, 100, false),
    ];
    let rx = ScriptedRx {
        outcomes: outcomes.into(),
        active: false,
    };
    let mut receiver = TimedRx::new(
        Box::new(rx),
        AirClockOrigin {
            chip: at(0),
            chip_rate: SampleRate::new(CHIPS_PER_SECOND, 1).unwrap(),
            tick: 0,
            tick_rate: NonZeroU64::new(30_720_000).unwrap(),
        },
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: 32,
            phase: 0,
            maximum_samples: count(100),
            timeout_us: 1000,
        },
    )
    .unwrap();
    receiver.start(None).unwrap();
    let first = receiver.read_filtered().unwrap().unwrap();
    assert_eq!(first.generation, 1);
    assert_eq!(first.window.first_center.unwrap().index, 1032);
    assert!(!first.discontinuity);
    assert!(receiver.read_filtered().unwrap().is_none());
    assert!(receiver.read_filtered().unwrap().is_none());
    let next = receiver.read_filtered().unwrap().unwrap();
    assert_eq!(next.generation, 2);
    assert!(next.discontinuity);
    assert_eq!(next.window.first_center.unwrap().index, 1232);
    let following = receiver.read_filtered().unwrap().unwrap();
    assert_eq!(following.generation, 2);
    assert!(!following.discontinuity);
    assert_eq!(following.window.first_center.unwrap().index, 1268);
    assert_eq!(receiver.counters().discontinuities, 1);
    assert_eq!(receiver.counters().timeouts, 1);
    assert_eq!(receiver.counters().reported_overflows, 1);
    assert_eq!(receiver.counters().missing_samples, 100);
    assert_eq!(receiver.counters().maximum_gap_samples, 100);
    let gap = receiver.counters().last_discontinuity.unwrap();
    assert_eq!(gap.expected_sample.index, 1100);
    assert_eq!(gap.received_sample.index, 1200);
    assert_eq!(gap.raw_tick, 4800);
    assert_eq!(gap.driver.unwrap().tick_delta, 400);
    receiver.stop().unwrap();
    assert!(matches!(receiver.read_filtered(), Err(RxError::NotActive)));
}

#[test]
fn duplicate_rx_prefixes_preserve_full_rate_filter_and_common_chip_continuity() {
    let run = |chunks: &[(u64, usize)]| {
        let outcomes = chunks
            .iter()
            .map(|&(start, length)| {
                let samples = (start..start + length as u64)
                    .map(|index| {
                        Complex32::new((index % 17) as f32 / 100.0, (index % 29) as f32 / 100.0)
                    })
                    .collect();
                Ok((
                    samples,
                    RxReadResult {
                        time_ticks: start * 4,
                        samples_read: length,
                        overflow: false,
                    },
                ))
            })
            .collect();
        let mut receiver = TimedRx::new(
            Box::new(ScriptedRx {
                outcomes,
                active: false,
            }),
            AirClockOrigin {
                chip: at(0),
                chip_rate: SampleRate::new(CHIPS_PER_SECOND, 1).unwrap(),
                tick: 0,
                tick_rate: NonZeroU64::new(30_720_000).unwrap(),
            },
            RxSettings {
                rate: SamplesPerChip::Two,
                span_chips: 32,
                phase: 0,
                maximum_samples: count(100),
                timeout_us: 1000,
            },
        )
        .unwrap();
        receiver.start(None).unwrap();
        let mut full = Vec::new();
        let mut common = Vec::new();
        for _ in chunks {
            if let Some(batch) = receiver.read_matched().unwrap() {
                assert_eq!(batch.full.generation, 1);
                assert!(!batch.full.discontinuity);
                full.extend_from_slice(batch.full.samples);
                common.extend_from_slice(batch.common.chips);
            }
        }
        receiver.stop().unwrap();
        (full, common, receiver.counters())
    };
    let baseline = run(&[(1000, 100), (1100, 80), (1180, 100)]);
    let repeated = run(&[(1000, 100), (1080, 100), (1080, 100), (1180, 100)]);
    assert_eq!(baseline.0, repeated.0);
    assert_eq!(baseline.1, repeated.1);
    assert_eq!(baseline.2.output_chips, repeated.2.output_chips);
    assert_eq!(repeated.2.discontinuities, 0);
    assert_eq!(repeated.2.overlaps.blocks, 2);
    assert_eq!(repeated.2.overlaps.samples, 120);
    assert_eq!(repeated.2.input_samples, 400);
}

#[test]
fn malformed_rx_counts_and_nonfinite_samples_poison_the_stream_without_retry() {
    for invalid_samples in [false, true] {
        let mut samples = vec![Complex32::new(0.1, 0.2); 100];
        if invalid_samples {
            samples[99].im = f32::NAN;
        }
        let result = RxReadResult {
            samples_read: if invalid_samples { 100 } else { 101 },
            time_ticks: 4000,
            overflow: false,
        };
        let rx = ScriptedRx {
            outcomes: vec![Ok((samples, result))].into(),
            active: false,
        };
        let mut receiver = TimedRx::new(
            Box::new(rx),
            AirClockOrigin {
                chip: at(0),
                chip_rate: SampleRate::new(CHIPS_PER_SECOND, 1).unwrap(),
                tick: 0,
                tick_rate: NonZeroU64::new(30_720_000).unwrap(),
            },
            RxSettings {
                rate: SamplesPerChip::Two,
                span_chips: 32,
                phase: 0,
                maximum_samples: count(100),
                timeout_us: 1000,
            },
        )
        .unwrap();
        receiver.start(None).unwrap();
        assert!(receiver.read_filtered().is_err());
        assert!(matches!(receiver.read_filtered(), Err(RxError::Poisoned)));
        assert_eq!(receiver.counters().reads, 1);
    }
}

#[test]
fn sampled_preamble_flows_through_hal_filter_search_calendar_and_timed_aich_tx() {
    sampled_preamble_with_batch(TxBatch::OneSlot);
}

#[test]
fn two_slot_batches_preserve_exact_preamble_to_aich_waveform_and_access_alignment() {
    sampled_preamble_with_batch(TxBatch::TwoSlots);
}

fn sampled_preamble_with_batch(batch: TxBatch) {
    let code = PrachCode::new(7504).unwrap();
    let signature = PreambleSignature::new(13).unwrap();
    let search_plan = Arc::new(PreambleSearchPlan::new(code, 1 << 13, 0..4096, 100).unwrap());
    let span_chips = 32;
    let mut chips = vec![Complex32::default(); 23040 + search_plan.input_length() + span_chips];
    code.write_preamble(signature, &mut chips[23_094..23_094 + 4096])
        .unwrap();
    let mut tx_filter = PulseShaper::new(SamplesPerChip::Two, span_chips).unwrap();
    let mut samples = vec![Complex32::default(); chips.len() * 2];
    tx_filter.process(&chips, &mut samples).unwrap();
    let (mut radio, peer) = MemoryRadio::new(1);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    radio
        .setup_rx(0, "", 7_680_000.0, 5_000_000.0, None)
        .unwrap();
    let (tx, rx) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx_batched(
        source(false),
        tx,
        TxSettings::offline(1_000_000_000, count(2), count(2), count(2)),
        batch,
    )
    .unwrap();
    let sample_count = samples.len();
    peer.input
        .send(IqBlock {
            samples,
            time_ticks: Some(1_000_000_000),
            discontinuity: false,
        })
        .unwrap();
    let mut receiver = TimedRx::new(
        rx.unwrap(),
        preparation.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: 32,
            phase: 0,
            maximum_samples: count(997),
            timeout_us: 1000,
        },
    )
    .unwrap();
    receiver.start(None).unwrap();
    let mut search = PrachWindowReceiver::new(search_plan, SamplesPerChip::Two);
    search.arm(at(23040)).unwrap();
    let mut result = None;
    while receiver.counters().input_samples < sample_count as u64 {
        if let Some(input) = receiver.read_filtered().unwrap()
            && let Some(window) = search.feed(input).unwrap()
        {
            result = window.strongest(signature);
            break;
        }
    }
    assert!(
        result.is_some(),
        "remaining chips: {:?}",
        search.remaining_chips()
    );
    let result = result.unwrap();
    assert_eq!(result.correlation_start, at(23094));
    assert!(result.coherent_fraction > 0.9999);
    assert_eq!(receiver.counters().preroll_samples, 32);
    let response = preparation
        .source_mut()
        .schedule_acquisition(
            0,
            AccessObservation {
                code,
                signature,
                nominal_start: at(23040),
                received_start: result.correlation_start,
            },
            umts_dsp::indicators::Acquisition::Positive,
            at(27290),
            0,
        )
        .unwrap();
    assert_eq!(response.indicator_start, at(35840));
    pump.start().unwrap();
    let mut received = Vec::new();
    for index in 0..18 / batch.slots() {
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        let block = peer.output.recv().unwrap();
        assert_eq!(
            block.time_ticks,
            Some(1_000_000_000 + ((index * batch.slots()) as f64 / 1500.0 * 1e9).round() as u64)
        );
        received.extend(block.samples);
    }
    assert_eq!(received.len(), 18 * 5120);
    let mut background = crate::test_support::source(false);
    let cell = background.information();
    let amplitude = crate::test_support::amplitudes().primary.cpich
        * 10.0_f32.powf(f32::from(cell.sib5().settings().aich_power_offset_db) / 20.0);
    let generator =
        umts_dsp::indicators::AcquisitionDownlink::new(cell.primary_code(), 2, amplitude).unwrap();
    let mut expected_chips = vec![Complex32::default(); 18 * 2560];
    let mut indicators = umts_dsp::indicators::AcquisitionIndicators::new(1 << 13).unwrap();
    indicators
        .set(13, umts_dsp::indicators::Acquisition::Positive)
        .unwrap();
    generator
        .add_access_slot(at(35840), &indicators, &mut expected_chips[35840..40960])
        .unwrap();
    let mut expected = vec![Complex32::default(); received.len()];
    PulseShaper::new(SamplesPerChip::Two, 32)
        .unwrap()
        .process(&expected_chips, &mut expected)
        .unwrap();
    let mut background_slot = vec![Complex32::default(); 5120];
    let mut maximum_error = (0.0_f32, 0);
    for slot in 0..18 {
        background.generate_slot(&mut background_slot).unwrap();
        for (offset, &sample) in background_slot.iter().enumerate() {
            let index = slot * 5120 + offset;
            let error = (received[index] - sample - expected[index]).norm();
            if error > maximum_error.0 {
                maximum_error = (error, index);
            }
        }
    }
    // Filtering a sum and subtracting separately rounded Float32 outputs is not bit-exact.
    assert!(
        maximum_error.0 < 4.0 * f32::EPSILON,
        "maximum float error: {maximum_error:?}"
    );
    pump.stop().unwrap();
    receiver.stop().unwrap();
}
