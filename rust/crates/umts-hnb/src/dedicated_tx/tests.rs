// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, source};
use std::num::{NonZeroU8, NonZeroU64};
use umts_dsp::{
    coding::{CodedBlockDecoder, DecodeBudget},
    dedicated_transport::TransportPlanLimits,
    interleaving::{Interleaver, Rel99Tti},
    pulse_shaping::{PulseShaper, SamplesPerChip},
    scrambling::downlink_sequence,
    transport_blocks::BlockOutcome,
};
use umts_rrc::dedicated::Rel99Configuration;
use umts_types::UeKey;

use crate::test_support::rel99 as fixture;
mod transitions;

#[test]
fn downlink_gain_is_latched_per_physical_slot_and_preserves_every_dpch_field() {
    let make = || {
        let mut owner = session(
            compile(true, true, true, 3, 0),
            identity(0, 1),
            FrameAnchor {
                time: at(0),
                cfn: 0,
            },
            4,
        );
        owner
            .configure_downlink_power(DownlinkPowerSettings {
                initial_offset_db: -6.0,
                minimum_offset_db: -20.0,
                maximum_age_chips: CHIPS_PER_FRAME,
            })
            .unwrap();
        prepare(&mut owner, Tfci::new(23).unwrap());
        owner.arm(at(0)).unwrap();
        owner
    };
    let mut reference = make();
    let mut candidate = make();
    let mut first = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    let mut actual = first.clone();
    reference.mix(at(0), &mut first, &mut |_| Bit::One).unwrap();
    candidate
        .mix(at(0), &mut actual[..100], &mut |_| Bit::One)
        .unwrap();
    candidate.downlink_power.as_mut().unwrap().observe(
        DownlinkControl {
            identity: identity(0, 1),
            measurement_end: at(1),
            command: DownlinkCommand::Down,
        },
        at(100),
    );
    candidate
        .mix(at(100), &mut actual[100..], &mut |_| Bit::One)
        .unwrap();
    assert_eq!(actual, first);
    actual.fill(Complex32::default());
    first.fill(Complex32::default());
    reference
        .mix(at(CHIPS_PER_SLOT), &mut first, &mut |_| Bit::One)
        .unwrap();
    candidate
        .mix(at(CHIPS_PER_SLOT), &mut actual, &mut |_| Bit::One)
        .unwrap();
    let gain = 10.0_f32.powf(-1.0 / 20.0);
    for (actual, expected) in actual.iter().zip(first) {
        assert_eq!(*actual, expected * gain);
    }
}

pub(super) fn identity(slot: u32, configuration: u64) -> ReceiveIdentity {
    ReceiveIdentity {
        ue: UeKey {
            epoch: at(0).epoch,
            slot,
            generation: NonZeroU64::MIN,
        },
        configuration: NonZeroU64::new(configuration).unwrap(),
    }
}
fn decode_budget() -> DecodeBudget {
    DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap()
}
pub(super) fn compile(
    flexible: bool,
    voice: bool,
    packet: bool,
    code: u16,
    offset: u8,
) -> Arc<CompiledRel99> {
    compile_with_algorithm(
        flexible,
        voice,
        packet,
        code,
        offset,
        umts_rrc::dedicated::UplinkPowerControl::Algorithm1 { step_db: 1 },
    )
}
pub(super) fn compile_with_algorithm(
    flexible: bool,
    voice: bool,
    packet: bool,
    code: u16,
    offset: u8,
    algorithm: umts_rrc::dedicated::UplinkPowerControl,
) -> Arc<CompiledRel99> {
    let (ul, dl, bearers, mut physical) = fixture::components(voice, packet, 10);
    physical.power_control = algorithm;
    physical.flexible_position = flexible;
    physical.downlink_codes = vec![code];
    physical.frame_offset_256_chips = offset;
    physical.default_offset_512_chips = u16::from(offset) / 2;
    let configuration = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
    Arc::new(
        CompiledRel99::new(
            configuration,
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: decode_budget(),
            },
        )
        .unwrap(),
    )
}
fn amplitudes() -> DpchAmplitudes {
    DpchAmplitudes {
        data: 0.2,
        tpc: 0.2,
        tfci: 0.2,
        pilot: 0.2,
    }
}
pub(super) fn session(
    plan: Arc<CompiledRel99>,
    key: ReceiveIdentity,
    anchor: FrameAnchor,
    slots: usize,
) -> DedicatedTransmitSession {
    DedicatedTransmitSession::new(
        key,
        anchor,
        plan,
        amplitudes(),
        TransmitLimits {
            frame_slots: count(slots),
            maximum_frame_bits: count(100_000),
            maximum_retained_bits: count(1_000_000),
        },
    )
    .unwrap()
}
pub(super) fn payloads(
    session: &DedicatedTransmitSession,
    tfci: Tfci,
) -> Vec<(TransportId, Vec<Bit>)> {
    let configured = session.plan.downlink_configured();
    let row = configured.combination(tfci).unwrap();
    session
        .transports
        .iter()
        .zip(&row.formats)
        .filter(|(transport, _)| usize::from(session.next_cfn).is_multiple_of(transport.frames))
        .map(|(transport, &format)| {
            let length = configured
                .block_plan(transport.id, format)
                .unwrap()
                .payload_bits();
            (
                transport.id,
                (0..length).map(|index| Bit::from(index % 3 == 1)).collect(),
            )
        })
        .collect()
}
fn prepare(session: &mut DedicatedTransmitSession, tfci: Tfci) {
    let data = payloads(session, tfci);
    let inputs: Vec<_> = data
        .iter()
        .map(|(transport, bits)| TransportPayload {
            transport: *transport,
            bits,
        })
        .collect();
    session
        .prepare_frame(session.identity, session.next_prepare, tfci, &inputs)
        .unwrap();
}
fn bank(source: &crate::common_tx::CommonTx, owners: usize) -> DedicatedTransmitBank {
    DedicatedTransmitBank::new(
        source.information(),
        source.next_chip(),
        count(owners),
        count(8),
        count(2_000_000),
    )
    .unwrap()
}

#[test]
fn dedicated_admission_compares_codes_only_within_the_same_scrambling_tree() {
    let source = source(false);
    let mut bank = bank(&source, 3);
    let make = |slot, secondary| {
        let source = compile(true, false, true, 3, 0);
        let mut physical = source.configuration().physical().clone();
        physical.secondary_scrambling = secondary;
        let plan = Arc::new(
            CompiledRel99::new(
                Arc::new(source.configuration().with_physical(physical).unwrap()),
                count(200_000),
                TransportPlanLimits {
                    maximum_coded_bits: count(200_000),
                    maximum_mapping_entries: count(300_000),
                    decode: decode_budget(),
                },
            )
            .unwrap(),
        );
        session(
            plan,
            identity(slot, 1),
            FrameAnchor {
                time: at(0),
                cfn: 0,
            },
            2,
        )
    };
    insert(&mut bank, make(0, None));
    insert(&mut bank, make(1, Some(1)));
    assert!(matches!(
        bank.insert(make(2, Some(1))),
        Err((TransmitError::Conflict, _))
    ));
    insert(&mut bank, make(2, Some(2)));
}
fn insert(bank: &mut DedicatedTransmitBank, session: DedicatedTransmitSession) {
    if let Err((error, _)) = bank.insert(session) {
        panic!("admission: {error:?}");
    }
}

#[test]
fn fixed_and_flexible_composition_keep_ttis_and_wrap_cfn_without_allocating_new_storage() {
    for flexible in [false, true] {
        let plan = compile(flexible, true, true, 3, 0);
        let tfci = Tfci::new(23).unwrap();
        let mut session = session(
            plan,
            identity(0, 1),
            FrameAnchor {
                time: at(0),
                cfn: 252,
            },
            4,
        );
        let frame_pointers: Vec<_> = session
            .frames
            .iter()
            .map(|frame| frame.data.as_ptr())
            .collect();
        let transport_pointers: Vec<_> = session
            .transports
            .iter()
            .map(|transport| transport.encoded.as_ptr())
            .collect();
        session.arm(at(0)).unwrap_err();
        for frame in 0..8 {
            prepare(&mut session, tfci);
            if frame == 0 {
                session.arm(at(0)).unwrap();
            }
            assert_eq!(
                session.frames[session.head].metadata.unwrap().cfn,
                252_u8.wrapping_add(frame as u8)
            );
            for slot in 0..SLOTS_PER_FRAME {
                let mut chips = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
                session
                    .mix(
                        at(frame * CHIPS_PER_FRAME + slot * CHIPS_PER_SLOT),
                        &mut chips,
                        &mut |_| Bit::Zero,
                    )
                    .unwrap();
                assert!(
                    chips
                        .iter()
                        .all(|chip| chip.re.is_finite() && chip.im.is_finite())
                );
            }
            assert_eq!(session.queued_frames(), 0);
        }
        assert_eq!(
            frame_pointers,
            session
                .frames
                .iter()
                .map(|frame| frame.data.as_ptr())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            transport_pointers,
            session
                .transports
                .iter()
                .map(|transport| transport.encoded.as_ptr())
                .collect::<Vec<_>>()
        );
        assert_eq!(session.counters.consumed_frames, 8);
        assert_eq!(session.counters.physical_slots, 120);
    }
}

#[test]
fn rejected_frame_admission_preserves_encoder_history_and_tti_formats() {
    let mut session = session(
        compile(true, true, true, 3, 0),
        identity(0, 1),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    );
    let tfci = Tfci::new(23).unwrap();
    let data = payloads(&session, tfci);
    let mut inputs: Vec<_> = data
        .iter()
        .map(|(transport, bits)| TransportPayload {
            transport: *transport,
            bits,
        })
        .collect();
    inputs[0].bits = &[];
    assert_eq!(
        session.prepare_frame(session.identity, at(0), tfci, &inputs),
        Err(TransmitError::Payload)
    );
    assert_eq!(session.next_prepare(), at(0));
    assert!(
        session
            .transports
            .iter()
            .all(|transport| transport.active_format.is_none())
    );
    prepare(&mut session, tfci);
    let saved: Vec<_> = session
        .transports
        .iter()
        .map(|transport| transport.encoded.clone())
        .collect();
    assert_eq!(
        session.prepare_frame(
            session.identity,
            at(CHIPS_PER_FRAME),
            Tfci::new(0).unwrap(),
            &[]
        ),
        Err(TransmitError::FormatChangedWithinTti)
    );
    assert_eq!(session.next_prepare(), at(CHIPS_PER_FRAME));
    assert_eq!(
        saved,
        session
            .transports
            .iter()
            .map(|transport| transport.encoded.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        session.prepare_frame(identity(0, 2), at(CHIPS_PER_FRAME), tfci, &[]),
        Err(TransmitError::Identity)
    );
    prepare(&mut session, tfci);
    assert_eq!(
        session.prepare_frame(session.identity, at(2 * CHIPS_PER_FRAME), tfci, &[]),
        Err(TransmitError::Capacity)
    );
}

#[test]
fn offset_dpch_synthesis_binds_tpc_once_per_physical_slot_and_keeps_cell_scrambling_phase() {
    let source = source(false);
    let key = identity(0, 1);
    let offset = 512;
    let mut owner = session(
        compile(true, false, true, 3, 2),
        key,
        FrameAnchor {
            time: at(offset),
            cfn: 0,
        },
        2,
    );
    let tfci = Tfci::new(3).unwrap();
    prepare(&mut owner, tfci);
    let expected = owner.frames[owner.head].data.clone();
    prepare(&mut owner, tfci);
    let mut bank = bank(&source, 2);
    insert(&mut bank, owner);
    bank.arm(key).unwrap();
    let mut chips = vec![Complex32::default(); 16 * CHIPS_PER_SLOT as usize];
    let mut decisions = Vec::new();
    for (index, output) in chips.chunks_exact_mut(CHIPS_PER_SLOT as usize).enumerate() {
        assert_eq!(
            bank.mix_slot(at(index as u64 * CHIPS_PER_SLOT), output, |request| {
                decisions.push(request);
                Bit::from((request.physical_slot_start.index - offset) / CHIPS_PER_SLOT % 2 == 1)
            }),
            Ok(0)
        );
    }
    assert!(
        chips[..offset as usize]
            .iter()
            .all(|chip| *chip == Complex32::default())
    );
    assert_eq!(decisions.len(), 16);
    for (index, request) in decisions.iter().enumerate() {
        assert_eq!(request.identity, key);
        assert_eq!(
            request.physical_slot_start,
            at(offset + index as u64 * CHIPS_PER_SLOT)
        );
        assert!(request.synthesis_at.index <= request.physical_slot_start.index);
        assert!(request.physical_slot_start.index - request.synthesis_at.index < CHIPS_PER_SLOT);
    }
    let format = bank.session(key).unwrap().composite.format();
    let mut code = vec![0.0; 16];
    OvsfCode::new(16, 3).unwrap().write(&mut code).unwrap();
    let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    downlink_sequence(
        source.information().primary_code().sequence_number(),
        &mut scrambling,
    )
    .unwrap();
    let mut recovered = Vec::new();
    for slot in 0..15 {
        let mut field = Vec::new();
        for symbol in 0..160 {
            let first = offset as usize + slot * 2560 + symbol * 16;
            let value = (0..16)
                .map(|index| {
                    chips[first + index]
                        * scrambling[(first + index) % scrambling.len()].conj()
                        * code[index]
                })
                .sum::<Complex32>();
            field.extend([value.re, value.im]);
        }
        let first_end = format.data1_bits();
        let tpc_end = first_end + format.tpc_bits();
        let tfci_end = tpc_end + format.tfci_bits();
        assert!(
            field[first_end..tpc_end]
                .iter()
                .all(|value| (*value < 0.0) == (slot % 2 == 1))
        );
        let word = tfci.encode();
        for (index, value) in field[tpc_end..tfci_end].iter().enumerate() {
            assert_eq!(
                *value < 0.0,
                word[(slot * format.tfci_bits() + index) % 32] == Bit::One
            );
        }
        recovered.extend_from_slice(&field[..first_end]);
        recovered.extend_from_slice(&field[tfci_end..tfci_end + format.data2_bits()]);
    }
    for (&value, &bit) in recovered.iter().zip(&expected) {
        match bit {
            TxBit::Dtx => assert!(value.abs() < 1e-5),
            TxBit::Data(bit) => assert_eq!(value < 0.0, bit == Bit::One),
        }
    }
}

#[test]
fn flexible_packet_tti_recovers_payload_and_crc_after_second_and_first_deinterleaving() {
    let plan = compile(true, false, true, 3, 0);
    let mut session = session(
        plan.clone(),
        identity(0, 1),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        4,
    );
    let tfci = Tfci::new(1).unwrap();
    let expected = payloads(&session, tfci);
    let mut frames = Vec::new();
    for _ in 0..2 {
        prepare(&mut session, tfci);
        let index = (session.head + session.queued - 1) % session.frames.len();
        let physical: Vec<_> = session.frames[index]
            .data
            .iter()
            .map(|bit| match bit {
                TxBit::Data(Bit::Zero) => 8.0,
                TxBit::Data(Bit::One) => -8.0,
                TxBit::Dtx => 0.0,
            })
            .collect();
        let mut multiplexed = vec![0.0; physical.len()];
        Interleaver::second(physical.len())
            .unwrap()
            .deinterleave(&physical, &mut multiplexed)
            .unwrap();
        frames.push(multiplexed);
    }
    let id = TransportId::new(9).unwrap();
    let configured = plan.downlink_configured();
    let row = configured.combination(tfci).unwrap();
    let format = row.formats[0];
    let block = configured.block_plan(id, format).unwrap();
    let matched = match plan.downlink() {
        DownlinkPlans::Flexible(plans) => plans.matched_tti_bits(id, format).unwrap(),
        _ => unreachable!(),
    };
    let mut first = Vec::new();
    for frame in frames {
        first.extend_from_slice(&frame[..matched / 2]);
    }
    let mut ordered = vec![0.0; matched];
    Interleaver::first(Rel99Tti::Ms20, matched)
        .unwrap()
        .deinterleave(&first, &mut ordered)
        .unwrap();
    let matching = block.matching_flexible(matched, count(300_000)).unwrap();
    let mut coded = vec![0.0; block.coded_bits()];
    matching.combine_llrs(&ordered, &mut coded).unwrap();
    let mut decoded = vec![Bit::Zero; block.payload_bits()];
    assert_eq!(
        CodedBlockDecoder::new(block, decode_budget())
            .unwrap()
            .decode(&coded, &mut decoded),
        Ok(BlockOutcome::Decoded)
    );
    assert_eq!(
        decoded,
        expected
            .iter()
            .find(|(transport, _)| *transport == id)
            .unwrap()
            .1
    );
}

#[test]
fn one_ue_underrun_is_explicit_and_does_not_mutate_or_stop_healthy_peer() {
    let source = source(false);
    let a = identity(0, 1);
    let b = identity(1, 1);
    let mut first = session(
        compile(true, false, true, 3, 0),
        a,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    );
    let mut second = session(
        compile(true, false, true, 4, 0),
        b,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    );
    for owner in [&mut first, &mut second] {
        prepare(owner, Tfci::new(3).unwrap());
    }
    prepare(&mut second, Tfci::new(3).unwrap());
    let mut bank = bank(&source, 2);
    insert(&mut bank, first);
    insert(&mut bank, second);
    bank.arm(a).unwrap();
    bank.arm(b).unwrap();
    for slot in 0..30 {
        let mut chips = vec![Complex32::default(); 2560];
        let failed = bank
            .mix_slot(at(slot * CHIPS_PER_SLOT), &mut chips, |_| Bit::Zero)
            .unwrap();
        assert_eq!(failed, usize::from(slot == 15));
        assert!(chips.iter().any(|chip| chip.norm_sqr() > 0.0));
    }
    assert_eq!(
        bank.session(a).unwrap().failure(),
        Some(&TransmitError::Underrun)
    );
    assert_eq!(bank.session(a).unwrap().phase(), TransmitPhase::Failed);
    assert_eq!(bank.session(a).unwrap().counters().underruns, 1);
    assert!(matches!(
        bank.session_mut(a)
            .unwrap()
            .prepare_frame(a, at(0), Tfci::new(3).unwrap(), &[]),
        Err(TransmitError::Underrun)
    ));
    assert_eq!(bank.session(b).unwrap().phase(), TransmitPhase::Armed);
    assert_eq!(bank.session(b).unwrap().counters().consumed_frames, 2);
}

#[test]
fn code_tree_and_generation_admission_are_transactional_and_removal_is_generation_scoped() {
    let source = source(false);
    let mut bank = bank(&source, 2);
    let make = |key, code| {
        session(
            compile(true, false, true, code, 0),
            key,
            FrameAnchor {
                time: at(0),
                cfn: 0,
            },
            2,
        )
    };
    insert(&mut bank, make(identity(0, 1), 3));
    assert!(matches!(
        bank.insert(make(identity(1, 1), 3)),
        Err((TransmitError::Conflict, _))
    ));
    assert!(matches!(
        bank.insert(make(identity(1, 1), 0)),
        Err((TransmitError::Conflict, _))
    ));
    insert(&mut bank, make(identity(1, 1), 4));
    let saved = bank.retained_bits;
    assert!(matches!(
        bank.remove(identity(0, 2)),
        Err(TransmitError::Identity)
    ));
    assert_eq!(bank.retained_bits, saved);
    assert_eq!(
        bank.remove(identity(0, 1)).unwrap().phase(),
        TransmitPhase::Retired
    );
    assert!(matches!(
        bank.insert(make(identity(0, 1), 3)),
        Err((TransmitError::Identity, _))
    ));
    insert(&mut bank, make(identity(0, 2), 3));
    assert!(bank.session(identity(1, 1)).is_some());
    assert_eq!(bank.retained_bits, saved);
}

#[test]
fn prepared_is_not_armed_and_late_arm_cannot_rewrite_already_synthesized_time() {
    let source = source(false);
    let key = identity(0, 1);
    let mut session = session(
        compile(true, false, true, 3, 0),
        key,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    );
    prepare(&mut session, Tfci::new(3).unwrap());
    let mut bank = bank(&source, 1);
    insert(&mut bank, session);
    let mut chips = vec![Complex32::default(); 2560];
    assert_eq!(
        bank.mix_slot(at(0), &mut chips, |_| panic!("unarmed TPC")),
        Ok(0)
    );
    assert!(chips.iter().all(|chip| *chip == Complex32::default()));
    assert_eq!(
        bank.arm(key),
        Err(TransmitError::Late(TransmitTimingStage::Arm))
    );
    assert_eq!(bank.session(key).unwrap().phase(), TransmitPhase::Prepared);
}

#[test]
fn scheduling_cursor_excludes_the_slot_already_being_synthesized() {
    use crate::common_tx::CommonTxTarget;

    let mut source = source(false);
    let _submission = source.submission_writer().unwrap();
    let port = source.control_port(count(2), count(2)).unwrap();
    let key = identity(0, 1);
    let mut owner = session_for_common(key);
    prepare(&mut owner, Tfci::new(3).unwrap());
    let mut bank = bank(&source, 1);
    insert(&mut bank, owner);
    bank.arm(key).unwrap();
    let mut output = vec![Complex32::default(); source.samples_per_slot()];
    for slot in 0..3 {
        assert_eq!(port.next_chip(), at(slot * CHIPS_PER_SLOT));
        let mut observed = false;
        source
            .generate_slot_with_dedicated(&mut output, &mut bank, |_| {
                observed = true;
                assert_eq!(port.next_chip(), at((slot + 1) * CHIPS_PER_SLOT));
                Bit::One
            })
            .unwrap();
        assert!(observed);
        assert_eq!(port.next_chip(), source.next_chip());
    }
}

#[test]
fn common_and_dedicated_are_added_before_one_continuous_pulse_shaper() {
    let mut common = source(false);
    let mut combined = source(false);
    let key = identity(0, 1);
    let mut session = session(
        compile(true, false, true, 3, 0),
        key,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    );
    prepare(&mut session, Tfci::new(3).unwrap());
    let mut reference_session = session_for_common(key);
    prepare(&mut reference_session, Tfci::new(3).unwrap());
    reference_session.arm(at(0)).unwrap();
    let mut bank = bank(&combined, 1);
    insert(&mut bank, session);
    bank.arm(key).unwrap();
    let mut reference = PulseShaper::new(SamplesPerChip::Two, 32).unwrap();
    let mut a = vec![Complex32::default(); common.samples_per_slot()];
    let mut b = a.clone();
    let mut expected = a.clone();
    for slot in 0..15 {
        let start = at(slot * CHIPS_PER_SLOT);
        let mut dedicated = vec![Complex32::default(); 2560];
        reference_session
            .mix(start, &mut dedicated, &mut |_| Bit::One)
            .unwrap();
        reference.process(&dedicated, &mut expected).unwrap();
        common.generate_slot(&mut a).unwrap();
        combined
            .generate_slot_with_dedicated(&mut b, &mut bank, |_| Bit::One)
            .unwrap();
        for ((&a, &b), &expected) in a.iter().zip(&b).zip(&expected) {
            assert!((b - a - expected).norm() < 2e-6);
        }
    }
}

fn session_for_common(key: ReceiveIdentity) -> DedicatedTransmitSession {
    session(
        compile(true, false, true, 3, 0),
        key,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    )
}

#[test]
fn multicode_segments_are_independently_interleaved_and_additional_dpch_has_no_control() {
    let (ul, dl, bearers, mut physical) = fixture::components(false, true, 10);
    physical.downlink_codes = vec![3, 4];
    let configuration = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
    let plan = Arc::new(
        CompiledRel99::new(
            configuration,
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: decode_budget(),
            },
        )
        .unwrap(),
    );
    let mut owner = session(
        plan,
        identity(0, 1),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    );
    prepare(&mut owner, Tfci::new(3).unwrap());
    let expected = owner.frames[owner.head].data.clone();
    owner.arm(at(0)).unwrap();
    let mut chips = vec![Complex32::default(); 2560];
    let mut decisions = 0;
    owner
        .mix(at(0), &mut chips, &mut |_| {
            decisions += 1;
            Bit::One
        })
        .unwrap();
    assert_eq!(decisions, 1);
    let format = owner.composite.format();
    let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    downlink_sequence(
        PrimaryScramblingCode::new(469).unwrap().sequence_number(),
        &mut scrambling,
    )
    .unwrap();
    for (channel, number) in [3, 4].into_iter().enumerate() {
        let mut code = vec![0.0; 16];
        OvsfCode::new(16, number).unwrap().write(&mut code).unwrap();
        let mut fields = Vec::new();
        for (symbol, input) in chips.chunks_exact(16).enumerate() {
            let value = input
                .iter()
                .enumerate()
                .map(|(chip, &value)| value * scrambling[symbol * 16 + chip].conj() * code[chip])
                .sum::<Complex32>();
            fields.extend([value.re, value.im]);
        }
        let tpc_end = format.data1_bits() + format.tpc_bits();
        let data2_start = tpc_end + format.tfci_bits();
        let data2_end = data2_start + format.data2_bits();
        if channel == 1 {
            assert!(
                fields[format.data1_bits()..data2_start]
                    .iter()
                    .chain(&fields[data2_end..])
                    .all(|value| value.abs() < 1e-5)
            );
        } else {
            assert!(
                fields[format.data1_bits()..tpc_end]
                    .iter()
                    .all(|value| *value < 0.0)
            );
        }
        let data: Vec<_> = fields[..format.data1_bits()]
            .iter()
            .chain(&fields[data2_start..data2_end])
            .copied()
            .collect();
        let first = channel * format.data_bits() * SLOTS_PER_FRAME as usize;
        for (&value, &bit) in data
            .iter()
            .zip(&expected[first..first + format.data_bits()])
        {
            match bit {
                TxBit::Dtx => assert!(value.abs() < 1e-5),
                TxBit::Data(bit) => assert_eq!(value < 0.0, bit == Bit::One),
            }
        }
    }
}

#[test]
fn common_code_reservations_must_match_even_with_the_same_primary_scrambling_code() {
    let original = source(false);
    let mut changed = source(true);
    let mut bank = bank(&original, 1);
    assert_eq!(
        original.information().primary_code(),
        changed.information().primary_code()
    );
    let mut output = vec![Complex32::new(0.5, -0.25); changed.samples_per_slot()];
    let before = output.clone();
    assert_eq!(
        changed.generate_slot_with_dedicated(&mut output, &mut bank, |_| Bit::Zero),
        Err(crate::common_tx::CommonTxError::Conflict)
    );
    assert_eq!(changed.next_chip(), at(0));
    assert_eq!(bank.next_chip(), at(0));
    assert_eq!(before, output);
}

#[test]
fn every_voice_mode_sid_and_empty_combination_is_schedulable_without_a_mixed_tti() {
    for flexible in [false, true] {
        for (voice, packet) in [(true, false), (false, true), (true, true)] {
            let plan = compile(flexible, voice, packet, 3, 0);
            let mut owner = session(
                plan.clone(),
                identity(0, 1),
                FrameAnchor {
                    time: at(0),
                    cfn: 0,
                },
                4,
            );
            for row in plan.downlink_configured().combinations() {
                let first = owner.next_prepare.index;
                for _ in 0..4 {
                    prepare(&mut owner, row.tfci);
                }
                if owner.phase == TransmitPhase::Prepared {
                    owner.arm(at(0)).unwrap();
                }
                for slot in 0..60 {
                    let mut chips = vec![Complex32::default(); 2560];
                    owner
                        .mix(at(first + slot * CHIPS_PER_SLOT), &mut chips, &mut |_| {
                            Bit::Zero
                        })
                        .unwrap();
                }
                assert_eq!(owner.queued_frames(), 0);
            }
            assert_eq!(
                owner.counters.prepared_frames as usize,
                plan.downlink_configured().combinations().len() * 4
            );
        }
    }
}

#[test]
fn frame_and_cell_reservations_reject_excess_without_losing_existing_owners() {
    let plan = compile(true, false, true, 3, 0);
    let limits = TransmitLimits {
        frame_slots: count(2),
        maximum_frame_bits: count(1),
        maximum_retained_bits: count(1),
    };
    assert!(matches!(
        DedicatedTransmitSession::new(
            identity(0, 1),
            FrameAnchor {
                time: at(0),
                cfn: 0
            },
            plan.clone(),
            amplitudes(),
            limits
        ),
        Err(TransmitError::Capacity)
    ));
    assert!(matches!(
        DedicatedTransmitSession::new(
            identity(0, 1),
            FrameAnchor {
                time: at(256),
                cfn: 0
            },
            plan.clone(),
            amplitudes(),
            TransmitLimits {
                maximum_frame_bits: count(100_000),
                maximum_retained_bits: count(1_000_000),
                ..limits
            }
        ),
        Err(TransmitError::Time)
    ));
    let source = source(false);
    let first = session(
        plan,
        identity(0, 1),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    );
    let reservation = first.resources.retained_bits;
    let mut bank = DedicatedTransmitBank::new(
        source.information(),
        at(0),
        count(2),
        count(2),
        count(reservation),
    )
    .unwrap();
    insert(&mut bank, first);
    let other = session(
        compile(true, false, true, 4, 0),
        identity(1, 1),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        2,
    );
    assert!(matches!(
        bank.insert(other),
        Err((TransmitError::Capacity, _))
    ));
    assert_eq!(bank.retained_bits, reservation);
    assert!(bank.session(identity(0, 1)).is_some());
    bank.remove(identity(0, 1)).unwrap();
    assert_eq!(bank.retained_bits, 0);
    assert_eq!(bank.channels, 0);
}

#[test]
fn wrong_epoch_and_common_cursor_mismatch_leave_waveform_and_all_owners_unchanged() {
    let mut source = source(false);
    let mut bank = bank(&source, 2);
    let mut output = vec![Complex32::new(1.0, -1.0); source.samples_per_slot()];
    let before = output.clone();
    let wrong = ChipInstant {
        epoch: umts_types::RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 0,
    };
    assert_eq!(
        bank.mix_slot(wrong, &mut output[..2560], |_| Bit::Zero),
        Err(TransmitError::Time)
    );
    let mut chips = vec![Complex32::default(); 2560];
    bank.mix_slot(at(0), &mut chips, |_| Bit::Zero).unwrap();
    assert_eq!(
        source.generate_slot_with_dedicated(&mut output, &mut bank, |_| Bit::Zero),
        Err(crate::common_tx::CommonTxError::Conflict)
    );
    assert_eq!(source.next_chip(), at(0));
    assert_eq!(output, before);
}
