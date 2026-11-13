// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    radio_resources::RadioResources,
    test_support::{at, count, source},
};
use std::num::{NonZeroU8, NonZeroU64};
use umts_dsp::{
    hsdpa::{
        CrcMethod, Modulation, ScchAssignment, ScchEncoder, TransmissionVersion, TransportEncoder,
    },
    waveform::WaveformWindow,
};
use umts_mac::hsdpa::{HarqProcess, ProcessId};
use umts_types::{UeKey, hsdpa::Category, time::SampleInstant};

fn bank(source: &CommonTx) -> TransmitBank {
    let mut resources =
        RadioResources::new(at(0).epoch, source.information(), count(4), 0..=3).unwrap();
    let lease = resources
        .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::new(2).unwrap())
        .unwrap();
    TransmitBank::new(
        source,
        lease,
        count(4),
        count(TransmitBank::required_storage_bytes(count(4)).unwrap()),
    )
    .unwrap_or_else(|_| panic!("valid bank"))
}

#[test]
fn retirement_preflight_is_nonmutating_and_existing_retirement_is_idempotent() {
    let source = source(true);
    let mut bank = bank(&source);
    let ue = |slot| UeKey {
        epoch: at(0).epoch,
        slot,
        generation: NonZeroU64::MIN,
    };
    assert_eq!(bank.can_begin_retirement(ue(0)), Ok(()));
    assert!(bank.retiring.iter().all(Option::is_none));
    for index in 0..bank.retiring.len() {
        bank.begin_retirement(ue(index as u32)).unwrap();
    }
    assert_eq!(
        bank.can_begin_retirement(ue(bank.retiring.len() as u32)),
        Err(Error::Capacity)
    );
    assert_eq!(bank.can_begin_retirement(ue(0)), Ok(()));
    bank.begin_retirement(ue(0)).unwrap();
}

fn burst(
    bank: &TransmitBank,
    ue: u32,
    start: u64,
    code_offset: u8,
    control: usize,
) -> PreparedTransmission {
    burst_with_modulation(bank, ue, start, code_offset, control, Modulation::Qpsk)
}

fn burst_with_modulation(
    bank: &TransmitBank,
    ue: u32,
    start: u64,
    code_offset: u8,
    control: usize,
    modulation: Modulation,
) -> PreparedTransmission {
    let mut harq = HarqProcess::new(
        UeKey {
            epoch: at(0).epoch,
            slot: ue,
            generation: NonZeroU64::MIN,
        },
        NonZeroU64::MIN,
        ProcessId::new(0).unwrap(),
    );
    harq.replace(());
    let transmission = harq
        .schedule(
            at(start + 2 * CHIPS_PER_SLOT),
            at(start + 20 * CHIPS_PER_SLOT),
        )
        .unwrap();
    let assignment = ScchAssignment::new(
        bank.codes().first_data() + code_offset,
        1,
        modulation,
        0,
        0,
        TransmissionVersion::new(0).unwrap(),
        transmission.new_data,
    )
    .unwrap();
    let bits = assignment.table().transport_bits(modulation, 1, 0).unwrap();
    let mut encoder = TransportEncoder::new(bits, count(100_000)).unwrap();
    let mut coded = vec![Bit::Zero; encoder.coded_bits()];
    encoder
        .encode(
            &vec![Bit::One; bits.get()],
            CrcMethod::Method1,
            false,
            &mut coded,
        )
        .unwrap();
    let mut plan = TransmissionPlan::new(
        assignment,
        Category::new(if modulation == Modulation::Qam64 {
            14
        } else {
            10
        })
        .unwrap(),
        &ScchEncoder::new(77).unwrap(),
        count(28_800),
        count(100_000),
    )
    .unwrap();
    WaveformPreparer::new(bank.codes(), bank.primary)
        .unwrap()
        .prepare(
            Preparation {
                transmission,
                plan: &mut plan,
                coded: &coded,
                control_code: bank.codes().control_codes().nth(control).unwrap().index(),
                amplitudes: Amplitudes {
                    control: 0.1,
                    data: 0.1,
                },
            },
            WaveformBuffer::default(),
        )
        .unwrap_or_else(|rejected| panic!("valid preparation: {:?}", rejected.error))
}

#[test]
fn overlapping_power_is_rejected_before_waveform_generation() {
    let mut source = source(true);
    source.set_dynamic_power_limit(0.015).unwrap();
    let mut bank = bank(&source);
    let first = burst(&bank, 0, 0, 0, 0);
    let second = burst(&bank, 1, 0, 1, 1);
    assert!(bank.enqueue(first).is_ok());
    let rejected = bank
        .enqueue(second)
        .expect_err("combined power exceeds budget");
    assert_eq!(rejected.error, Error::Dsp(DspError::InvalidAmplitude));
    assert_eq!(bank.next_chip(), at(0));
    let later = burst(&bank, 1, 3 * CHIPS_PER_SLOT, 1, 1);
    assert!(bank.enqueue(later).is_ok());
    let mut output = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    assert!(bank.mix_slot(at(0), &mut output).is_ok());
}

#[test]
fn overlapping_users_require_disjoint_data_and_control_codes() {
    let source = source(true);
    let mut bank = bank(&source);
    let first = burst(&bank, 0, 0, 0, 0);
    let conflict = burst(&bank, 1, 0, 0, 1);
    let independent = burst(&bank, 1, 0, 1, 1);
    assert!(bank.enqueue(first).is_ok());
    assert!(matches!(
        bank.enqueue(conflict),
        Err(RejectedTransmission {
            error: Error::Conflict,
            ..
        })
    ));
    assert!(bank.enqueue(independent).is_ok());
    let mut output = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    bank.mix_slot(at(0), &mut output).unwrap();
    assert!(output.iter().any(|value| value.norm_sqr() > 0.0));
    let preserved = output.clone();
    assert_eq!(bank.mix_slot(at(0), &mut output), Err(Error::Time));
    assert_eq!(output, preserved);
}

#[test]
fn waveform_reports_constant_data_power_only_during_the_pdsch_subframe() {
    let source = source(true);
    let mut combined = bank(&source);
    let transmission = burst(&combined, 0, 0, 0, 0);
    let power = transmission.metadata().nominal_data_power;
    let expected: Vec<_> = transmission
        .storage
        .control
        .iter()
        .zip(&transmission.storage.data)
        .map(|(control, data)| *control + *data)
        .collect();
    assert!(combined.enqueue(transmission).is_ok());
    for slot in 0..BURST_SLOTS {
        let mut reference = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
        let start = at(slot as u64 * CHIPS_PER_SLOT);
        let reserved = combined.mix_slot(start, &mut reference).unwrap();
        assert_eq!(
            reserved,
            if slot < CONTROL_LEAD_SLOTS {
                0.0
            } else {
                power
            }
        );
        assert_eq!(
            reference,
            expected[slot * CHIPS_PER_SLOT as usize..(slot + 1) * CHIPS_PER_SLOT as usize]
        );
    }
}

#[test]
fn dynamic_power_budget_does_not_normalize_qam64_slots_individually() {
    let mut reference = source(true);
    let mut limited = source(true);
    let mut reference_bank = bank(&reference);
    let mut limited_bank = bank(&limited);
    let transmission = burst_with_modulation(&limited_bank, 0, 0, 1, 0, Modulation::Qam64);
    let nominal = transmission.metadata().nominal_data_power;
    assert!(
        transmission
            .storage
            .data
            .chunks_exact(CHIPS_PER_SLOT as usize)
            .any(|slot| {
                slot.iter()
                    .map(|chip| f64::from(chip.re).powi(2) + f64::from(chip.im).powi(2))
                    .sum::<f64>()
                    / slot.len() as f64
                    > nominal
            })
    );
    limited.set_dynamic_power_limit(nominal as f32).unwrap();
    assert!(limited_bank.enqueue(transmission).is_ok());
    assert!(
        reference_bank
            .enqueue(burst_with_modulation(
                &reference_bank,
                0,
                0,
                1,
                0,
                Modulation::Qam64
            ))
            .is_ok()
    );
    let dedicated = |source: &CommonTx| {
        crate::dedicated_tx::DedicatedTransmitBank::new(
            source.information(),
            source.next_chip(),
            count(1),
            count(1),
            count(100_000),
        )
        .unwrap()
    };
    let mut reference_dedicated = dedicated(&reference);
    let mut limited_dedicated = dedicated(&limited);
    for _ in 0..BURST_SLOTS + 1 {
        let mut expected = vec![Complex32::default(); reference.samples_per_slot()];
        let mut actual = expected.clone();
        reference
            .generate_slot_with_hspa(
                &mut expected,
                &mut reference_dedicated,
                Some(&mut reference_bank),
                None,
                |_| Ok(Bit::Zero),
            )
            .unwrap();
        limited
            .generate_slot_with_hspa(
                &mut actual,
                &mut limited_dedicated,
                Some(&mut limited_bank),
                None,
                |_| Ok(Bit::Zero),
            )
            .unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
fn retirement_returns_unstarted_work_without_muting_another_ue() {
    let source = source(true);
    let mut bank = bank(&source);
    let retiring = burst(&bank, 0, 0, 0, 0);
    let ue = retiring.transmission().key.ue();
    let late = burst(&bank, 0, 12 * CHIPS_PER_SLOT, 0, 0);
    let peer = burst(&bank, 1, 0, 1, 1);
    let expected = peer.storage.control[..CHIPS_PER_SLOT as usize].to_vec();
    assert!(bank.enqueue(retiring).is_ok());
    assert!(bank.enqueue(peer).is_ok());
    bank.begin_retirement(ue).unwrap();
    assert!(matches!(
        bank.enqueue(late),
        Err(RejectedTransmission {
            error: Error::Unavailable,
            ..
        })
    ));
    assert!(!bank.finish_retirement(ue).unwrap());
    let returned = bank.take_completed().unwrap();
    assert_eq!(returned.transmission.transmission().key.ue(), ue);
    assert!(!returned.submitted);
    let mut output = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    bank.mix_slot(at(0), &mut output).unwrap();
    assert_eq!(output, expected);
    assert!(bank.finish_retirement(ue).unwrap());
    assert!(bank.take_completed().is_none());
}

#[test]
fn retirement_drains_started_waveform_and_waits_for_driver_resolution() {
    let mut source = source(true);
    let mut writer = source.submission_writer().unwrap();
    let mut bank = bank(&source);
    let transmission = burst(&bank, 0, 0, 0, 0);
    let ue = transmission.transmission().key.ue();
    assert!(bank.enqueue(transmission).is_ok());
    let mut output = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    bank.mix_slot(at(0), &mut output).unwrap();
    bank.begin_retirement(ue).unwrap();
    assert!(bank.take_completed().is_none());
    for slot in 1..BURST_SLOTS {
        output.fill(Complex32::default());
        bank.mix_slot(at(slot as u64 * CHIPS_PER_SLOT), &mut output)
            .unwrap();
        assert!(output.iter().any(|chip| chip.norm_sqr() > 0.0));
    }
    assert!(bank.take_completed().is_none());
    assert!(!bank.finish_retirement(ue).unwrap());
    let instant = |index| SampleInstant {
        epoch: ue.epoch,
        index,
    };
    assert!(writer.accepted(WaveformWindow {
        first_sample: instant(0),
        next_sample: instant((BURST_CHIPS * bank.rate + 2 * bank.group_delay) as u64),
        chip_origin_sample: instant(bank.group_delay as u64),
    }));
    assert!(bank.take_completed().unwrap().submitted);
    assert!(bank.finish_retirement(ue).unwrap());
}

#[test]
fn generated_chips_do_not_prove_driver_acceptance_and_loss_is_retained() {
    for lost in [false, true] {
        let mut source = source(true);
        let mut writer = source.submission_writer().unwrap();
        let mut bank = bank(&source);
        assert!(bank.enqueue(burst(&bank, 0, 0, 0, 0)).is_ok());
        for slot in 0..BURST_SLOTS {
            bank.mix_slot(
                at(slot as u64 * CHIPS_PER_SLOT),
                &mut vec![Complex32::default(); CHIPS_PER_SLOT as usize],
            )
            .unwrap();
        }
        assert!(bank.take_completed().is_none());
        let end = BURST_CHIPS * bank.rate + 2 * bank.group_delay;
        let instant = |index| SampleInstant {
            epoch: at(0).epoch,
            index,
        };
        let window = WaveformWindow {
            first_sample: instant(0),
            next_sample: instant(end as u64),
            chip_origin_sample: instant(bank.group_delay as u64),
        };
        assert!(if lost {
            writer.lost(window)
        } else {
            writer.accepted(window)
        });
        let completed = bank.take_completed().expect("resolved exact interval");
        assert_eq!(completed.submitted, !lost);
        assert_eq!(completed.transmission.transmission().key.ue().slot, 0);
        assert!(bank.take_completed().is_none());
    }
}

#[test]
fn production_slot_path_returns_full_tail_completion_through_bounded_commands() {
    use crate::dedicated_tx::{
        DedicatedTransmitRuntime, TransmitCommandOutcome, TransmitRuntimeLimits,
    };

    let mut source = source(true);
    let mut writer = source.submission_writer().unwrap();
    let bank = bank(&source);
    let prepared = burst(&bank, 0, 0, 0, 0);
    let key = prepared.transmission().key;
    let (mut runtime, mut ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(8),
        count(1_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(4),
            commands_per_step: count(2),
            feedback_capacity: count(4),
            feedback_per_step: count(2),
            maximum_inflight_retained_bits: count(8_000_000),
            maximum_frame_payload_bits: count(100_000),
        },
    )
    .unwrap();
    assert!(runtime.attach_hsdpa(bank).is_ok());
    assert!(ports.control.queue_hsdpa(prepared).is_ok());
    runtime.service().unwrap();
    assert!(matches!(ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::HsdpaQueued { key: actual, result: Ok(()) } if actual == key));
    assert_eq!(ports.control.retained_bits(), 0);
    assert!(runtime.take_closed_hsdpa().is_none());
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    for _ in 0..BURST_SLOTS + 1 {
        let window = runtime.generate_slot(&mut source, &mut samples).unwrap();
        assert!(writer.accepted(window));
    }
    ports.control.take_hsdpa_completion().unwrap();
    runtime.service().unwrap();
    match ports.control.try_event().unwrap().outcome {
        TransmitCommandOutcome::HsdpaCompletion(Some(completed)) => {
            assert!(completed.submitted);
            assert_eq!(completed.transmission.transmission().key, key);
            let buffer = completed.transmission.recycle();
            assert_eq!(buffer.storage.control.len(), BURST_CHIPS);
            assert_eq!(buffer.storage.data.len(), BURST_CHIPS);
        }
        _ => panic!("expected completed HSDPA transmission"),
    }
    assert_eq!(ports.control.outstanding(), 0);
    ports.control.begin_hsdpa_retirement(key.ue()).unwrap();
    runtime.service().unwrap();
    assert!(matches!(ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::HsdpaRetirementStarted { ue, result: Ok(()) } if ue == key.ue()));
    ports.control.finish_hsdpa_retirement(key.ue()).unwrap();
    runtime.service().unwrap();
    assert!(matches!(ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::HsdpaRetirementFinished { ue, result: Ok(true) } if ue == key.ue()));
    assert_eq!(ports.control.outstanding(), 0);
    assert_eq!(ports.control.retained_bits(), 0);
    runtime.close();
    let (_, pending) = runtime.take_closed_hsdpa().unwrap().into_retained();
    assert_eq!(pending.count(), 0);
}

#[test]
fn shared_hsdpa_bank_reaches_the_actual_memory_hal_through_tx_preparation() {
    use crate::{
        dedicated_tx::{TransmitCommandOutcome, TransmitRuntimeLimits},
        radio_io::{PumpProgress, TxSettings, prepare_tx},
    };
    use sdr_hal::{Radio, memory::MemoryRadio};
    let source = source(true);
    let bank = bank(&source);
    let prepared = burst(&bank, 0, 0, 0, 0);
    let key = prepared.transmission().key;
    let (mut radio, peer) = MemoryRadio::new(1);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source,
        tx,
        TxSettings::offline(1000, count(1), count(1), count(2)),
    )
    .unwrap();
    let bank = preparation
        .attach_hsdpa(bank)
        .expect_err("no dedicated owner");
    let mut ports = preparation
        .dedicated_ports(
            count(2),
            count(8),
            count(1_000_000),
            TransmitRuntimeLimits {
                command_capacity: count(4),
                commands_per_step: count(2),
                feedback_capacity: count(4),
                feedback_per_step: count(2),
                maximum_inflight_retained_bits: count(8_000_000),
                maximum_frame_payload_bits: count(100_000),
            },
        )
        .unwrap();
    assert!(preparation.attach_hsdpa(*bank).is_ok());
    assert!(ports.control.queue_hsdpa(prepared).is_ok());
    pump.start().unwrap();
    for _ in 0..BURST_SLOTS + 1 {
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        let block = peer.output.recv().unwrap();
        assert!(!block.samples.is_empty());
        assert!(
            block
                .samples
                .iter()
                .all(|sample| sample.re.is_finite() && sample.im.is_finite())
        );
    }
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::HsdpaQueued { key: actual, result: Ok(()) } if actual == key
    ));
    ports.control.take_hsdpa_completion().unwrap();
    preparation.prepare_one().unwrap().unwrap();
    match ports.control.try_event().unwrap().outcome {
        TransmitCommandOutcome::HsdpaCompletion(Some(completed)) => {
            assert!(completed.submitted);
            assert_eq!(completed.transmission.transmission().key, key);
        }
        _ => panic!("full HAL acceptance before completion"),
    }
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    peer.output.recv().unwrap();
    let mut commands = crate::hsdpa_packet::control::Commands::new(count(2)).unwrap();
    let mut retirement = crate::hsdpa_session::retirement::Retirement::new(key.ue());
    retirement.begin();
    for expected in [
        crate::hsdpa_session::RetirementProgress::Draining,
        crate::hsdpa_session::RetirementProgress::TransmitComplete,
    ] {
        retirement
            .service(&mut commands.port(&mut ports.control))
            .unwrap();
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        peer.output.recv().unwrap();
        assert!(commands.route(ports.control.try_event().unwrap()).is_none());
        let result = commands.port(&mut ports.control).take_result().unwrap();
        assert_eq!(retirement.result(&result), Ok(expected));
    }
    assert!(retirement.complete());
}

fn order(bank: &TransmitBank, ue: u32, start: u64, control: usize) -> PreparedOrder {
    let identity = crate::dedicated_rx::ReceiveIdentity {
        ue: UeKey {
            epoch: at(0).epoch,
            slot: ue,
            generation: NonZeroU64::MIN,
        },
        configuration: NonZeroU64::MIN,
    };
    WaveformPreparer::new(bank.codes(), bank.primary)
        .unwrap()
        .prepare_order(
            OrderPreparation {
                key: OrderIdentity {
                    identity,
                    generation: 1,
                    ordinal: 1,
                    scch_start: at(start),
                    expected_ack: at(start + 12 * CHIPS_PER_SLOT),
                },
                h_rnti: 77,
                order: umts_dsp::hsdpa::SecondaryCellOrder::Deactivate,
                control_code: bank.codes().control_codes().nth(control).unwrap().index(),
                amplitude: 0.1,
            },
            OrderBuffer::default(),
        )
        .unwrap_or_else(|_| panic!("valid order"))
}

#[test]
fn orders_conflict_with_same_control_or_same_ue_data_and_retire() {
    let source = source(false);
    let mut bank = bank(&source);
    let first = order(&bank, 0, 0, 0);
    let ue = first.identity().identity.ue;
    bank.enqueue_order(first)
        .unwrap_or_else(|_| panic!("admit order"));
    let same_code = burst(&bank, 1, 0, 0, 0);
    assert!(matches!(
        bank.enqueue(same_code),
        Err(RejectedTransmission {
            error: Error::Conflict,
            ..
        })
    ));
    let same_ue = burst(&bank, 0, 0, 0, 1);
    assert!(matches!(
        bank.enqueue(same_ue),
        Err(RejectedTransmission {
            error: Error::Conflict,
            ..
        })
    ));
    let conflicting_order = order(&bank, 1, 0, 0);
    assert!(matches!(
        bank.enqueue_order(conflicting_order),
        Err(RejectedOrder {
            error: Error::Conflict,
            ..
        })
    ));
    bank.begin_retirement(ue).unwrap();
    assert!(!bank.retirement_ready(ue).unwrap());
    let completed = bank.take_completed_order().unwrap();
    assert!(!completed.submitted);
    assert!(bank.finish_retirement(ue).unwrap());
}

#[test]
fn accepted_order_synthesizes_only_three_control_slots() {
    let source = source(false);
    let mut bank = bank(&source);
    bank.enqueue_order(order(&bank, 0, 0, 0))
        .unwrap_or_else(|_| panic!("admit order"));
    for slot in 0..4 {
        let mut samples = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
        assert_eq!(
            bank.mix_slot(at(slot * CHIPS_PER_SLOT), &mut samples)
                .unwrap(),
            0.0
        );
        assert_eq!(
            samples.iter().any(|sample| sample.norm_sqr() > 0.0),
            slot < 3
        );
    }
}

#[test]
fn carrier_power_fault_returns_custody_and_preserves_common_samples() {
    let source = source(true);
    let mut bank = bank(&source);
    let prepared = burst(&bank, 0, 0, 0, 0);
    let identity = prepared.transmission();
    bank.enqueue(prepared)
        .unwrap_or_else(|_| panic!("valid burst"));
    // Corrupt accounting after admission to exercise the synthesis guard.
    bank.pending
        .iter_mut()
        .flatten()
        .next()
        .unwrap()
        .transmission
        .storage
        .metadata
        .as_mut()
        .unwrap()
        .nominal_data_power = f64::NAN;
    bank.next = at(2 * CHIPS_PER_SLOT);
    let mut output = vec![Complex32::new(0.2, -0.1); CHIPS_PER_SLOT as usize];
    let expected = output.clone();
    assert_eq!(bank.mix_slot(at(2 * CHIPS_PER_SLOT), &mut output), Ok(0.0));
    assert_eq!(output, expected);
    assert_eq!(bank.next_chip(), at(3 * CHIPS_PER_SLOT));
    let completed = bank.take_completed().expect("explicit failed completion");
    assert!(!completed.submitted);
    assert_eq!(completed.transmission.transmission(), identity);
    let next = burst(&bank, 1, 6 * CHIPS_PER_SLOT, 0, 0);
    assert_eq!(bank.enqueue(next).unwrap_err().error, Error::Unavailable);
}
