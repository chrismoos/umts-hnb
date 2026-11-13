// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn candidate(
    flexible: bool,
    generation: u64,
    frame: u64,
    cfn: u8,
    code: u16,
    offset: u8,
) -> DedicatedTransmitSession {
    session(
        compile(flexible, true, true, code, offset),
        identity(0, generation),
        FrameAnchor {
            time: at(frame * CHIPS_PER_FRAME + u64::from(offset) * 256),
            cfn,
        },
        8,
    )
}
fn old_bank(flexible: bool, offset: u8) -> DedicatedTransmitBank {
    let mut bank = bank(&source(false), 2);
    let mut old = candidate(flexible, 1, 0, 252, 3, offset);
    for _ in 0..4 {
        prepare(&mut old, Tfci::new(23).unwrap());
    }
    insert(&mut bank, old);
    bank.arm(identity(0, 1)).unwrap();
    bank
}

#[test]
fn temporal_code_reuse_matches_independent_old_and_new_slot_composition() {
    for flexible in [false, true] {
        let offset = 4;
        let mut bank = old_bank(flexible, offset);
        let mut next = candidate(!flexible, 2, 4, 0, 3, offset);
        for _ in 0..2 {
            prepare(&mut next, Tfci::new(23).unwrap());
        }
        let mut old_reference = candidate(flexible, 1, 0, 252, 3, offset);
        for _ in 0..4 {
            prepare(&mut old_reference, Tfci::new(23).unwrap());
        }
        old_reference.arm(at(0)).unwrap();
        let mut new_reference = candidate(!flexible, 2, 4, 0, 3, offset);
        for _ in 0..2 {
            prepare(&mut new_reference, Tfci::new(23).unwrap());
        }
        new_reference.arm(at(0)).unwrap();
        assert!(
            bank.stage_replacement(identity(0, 1), Box::new(next))
                .is_ok()
        );
        assert_eq!(bank.arm(identity(0, 2)), Err(TransmitError::Phase));
        let boundary = bank
            .commit_replacement(identity(0, 1), identity(0, 2))
            .unwrap()
            .time;
        let mut requests = Vec::new();
        for slot in 0..76 {
            let start = at(slot * CHIPS_PER_SLOT);
            let end = start.index + CHIPS_PER_SLOT;
            let mut actual = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
            assert_eq!(
                bank.mix_slot(start, &mut actual, |request| {
                    requests.push(request);
                    Bit::from(request.identity.configuration.get() == 2)
                })
                .unwrap(),
                0
            );
            let mut expected = vec![Complex32::default(); actual.len()];
            if start.index < boundary.index {
                let length = (end.min(boundary.index) - start.index) as usize;
                old_reference
                    .mix(start, &mut expected[..length], &mut |_| Bit::Zero)
                    .unwrap();
            }
            if end > boundary.index {
                let first = start.index.max(boundary.index);
                new_reference
                    .mix(
                        at(first),
                        &mut expected[(first - start.index) as usize..],
                        &mut |_| Bit::One,
                    )
                    .unwrap();
            }
            assert_eq!(actual, expected);
        }
        let old = bank.session(identity(0, 1)).unwrap();
        assert!(
            matches!(old.phase(), TransmitPhase::Stopped { boundary: stopped, .. } if stopped == boundary)
        );
        assert_eq!(old.counters().mixed_chips, 4 * CHIPS_PER_FRAME);
        assert_eq!(old.counters().consumed_frames, 4);
        assert_eq!(old.queued_frames(), 0);
        let old_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.identity == identity(0, 1))
            .collect();
        assert_eq!(old_requests.len(), 60);
        assert!(
            old_requests
                .iter()
                .all(|request| request.physical_slot_start.index < boundary.index)
        );
        let new_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.identity == identity(0, 2))
            .collect();
        assert_eq!(new_requests.len(), 16);
        assert_eq!(new_requests[0].physical_slot_start, boundary);
        assert_eq!(
            bank.synthesized_retirement(identity(0, 1)),
            Ok((at(1024), boundary))
        );
    }
}

#[test]
fn staged_owner_is_silent_and_empty_successor_cannot_commit() {
    let mut bank = old_bank(true, 0);
    assert!(
        bank.stage_replacement(identity(0, 1), Box::new(candidate(true, 2, 4, 0, 3, 0)))
            .is_ok()
    );
    assert_eq!(
        bank.commit_replacement(identity(0, 1), identity(0, 2))
            .err(),
        Some(TransmitError::Underrun)
    );
    assert_eq!(
        bank.session(identity(0, 1)).unwrap().phase(),
        TransmitPhase::Armed
    );
    assert!(matches!(
        bank.session(identity(0, 2)).unwrap().phase(),
        TransmitPhase::Staged { .. }
    ));
    let mut observed = Vec::new();
    let mut output = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    bank.mix_slot(at(0), &mut output, |request| {
        observed.push(request.identity);
        Bit::One
    })
    .unwrap();
    assert_eq!(observed, [identity(0, 1)]);
    assert_eq!(
        bank.synthesized_retirement(identity(0, 1)),
        Err(TransmitError::Phase)
    );
}

#[test]
fn queued_old_frames_beyond_activation_reject_commit_without_losing_data() {
    let mut bank = old_bank(true, 0);
    let mut next = candidate(true, 2, 4, 0, 3, 0);
    prepare(&mut next, Tfci::new(23).unwrap());
    assert!(
        bank.stage_replacement(identity(0, 1), Box::new(next))
            .is_ok()
    );
    let old = bank.session(identity(0, 1)).unwrap();
    let data = payloads(old, Tfci::new(23).unwrap());
    let inputs: Vec<_> = data
        .iter()
        .map(|(transport, bits)| TransportPayload {
            transport: *transport,
            bits,
        })
        .collect();
    bank.prepare_frame(
        identity(0, 1),
        at(4 * CHIPS_PER_FRAME),
        Tfci::new(23).unwrap(),
        &inputs,
    )
    .unwrap();
    assert_eq!(
        bank.commit_replacement(identity(0, 1), identity(0, 2))
            .err(),
        Some(TransmitError::Time)
    );
    assert_eq!(bank.session(identity(0, 1)).unwrap().queued_frames(), 5);
    assert_eq!(
        bank.session(identity(0, 1)).unwrap().phase(),
        TransmitPhase::Armed
    );
}

#[test]
fn committed_predecessor_cannot_consume_another_frame_at_the_boundary() {
    let mut bank = old_bank(true, 0);
    let mut next = candidate(true, 2, 4, 0, 3, 0);
    prepare(&mut next, Tfci::new(23).unwrap());
    assert!(
        bank.stage_replacement(identity(0, 1), Box::new(next))
            .is_ok()
    );
    bank.commit_replacement(identity(0, 1), identity(0, 2))
        .unwrap();
    assert_eq!(
        bank.prepare_frame(
            identity(0, 1),
            at(4 * CHIPS_PER_FRAME),
            Tfci::new(23).unwrap(),
            &[]
        ),
        Err(TransmitError::Time)
    );
    assert_eq!(bank.session(identity(0, 1)).unwrap().queued_frames(), 4);
}

#[test]
fn peer_and_common_codes_remain_reserved_while_predecessor_overlap_is_allowed() {
    let mut bank = old_bank(true, 0);
    let peer = session(
        compile(true, false, true, 4, 0),
        identity(1, 1),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        4,
    );
    insert(&mut bank, peer);
    for code in [0, 4] {
        let result =
            bank.stage_replacement(identity(0, 1), Box::new(candidate(true, 2, 4, 0, code, 0)));
        assert!(matches!(result, Err((TransmitError::Conflict, _))));
    }
    assert!(
        bank.stage_replacement(identity(0, 1), Box::new(candidate(true, 2, 4, 0, 3, 0)))
            .is_ok()
    );
    assert!(bank.session(identity(1, 1)).is_some());
    assert!(matches!(
        bank.stage_replacement(identity(0, 2), Box::new(candidate(true, 3, 8, 4, 3, 0))),
        Err((TransmitError::Phase, _))
    ));
}

#[test]
fn invalid_cfn_time_and_timing_constructions_return_the_original_owner() {
    let mut bank = old_bank(true, 0);
    for (frame, cfn, offset, error) in [
        (2, 0, 0, TransmitError::Time),
        (4, 4, 0, TransmitError::Time),
        (4, 0, 2, TransmitError::TimingChange),
    ] {
        let next = Box::new(candidate(true, 2, frame, cfn, 3, offset));
        let address = std::ptr::from_ref(next.as_ref());
        let Err((observed, returned)) = bank.stage_replacement(identity(0, 1), next) else {
            panic!("invalid successor admitted");
        };
        assert_eq!(observed, error);
        assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
        assert_eq!(
            bank.session(identity(0, 1)).unwrap().phase(),
            TransmitPhase::Armed
        );
    }
}

#[test]
fn cancelled_successor_does_not_allow_generation_reuse_or_change_old_emission() {
    let mut bank = old_bank(true, 0);
    assert!(
        bank.stage_replacement(identity(0, 1), Box::new(candidate(true, 2, 4, 0, 3, 0)))
            .is_ok()
    );
    let returned = bank.remove_boxed(identity(0, 2)).unwrap();
    assert_eq!(returned.identity(), identity(0, 2));
    assert!(matches!(
        bank.stage_replacement(identity(0, 1), Box::new(candidate(true, 2, 4, 0, 3, 0))),
        Err((TransmitError::Identity, _))
    ));
    assert_eq!(
        bank.session(identity(0, 1)).unwrap().phase(),
        TransmitPhase::Armed
    );
}

#[test]
fn removed_longer_transport_still_controls_the_earliest_dl_switch_boundary() {
    use umts_rrc::{
        dedicated::{DchDirection, DchTfcs},
        transport::TransportInterval,
    };
    let (ul, dl, bearers, physical) = fixture::components(true, true, 10);
    let mut transports = dl.transports().to_vec();
    transports
        .iter_mut()
        .find(|transport| transport.id == 10)
        .unwrap()
        .interval = TransportInterval::Ms20;
    let dl = DchTfcs::new(
        DchDirection::Downlink,
        transports,
        dl.combinations().to_vec(),
    )
    .unwrap();
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
    let mut bank = bank(&source(false), 1);
    let mut old = candidate(true, 1, 0, 252, 3, 0);
    prepare(&mut old, Tfci::new(23).unwrap());
    insert(&mut bank, old);
    bank.arm(identity(0, 1)).unwrap();
    let next = Box::new(session(
        plan.clone(),
        identity(0, 2),
        FrameAnchor {
            time: at(2 * CHIPS_PER_FRAME),
            cfn: 254,
        },
        4,
    ));
    assert!(matches!(
        bank.stage_replacement(identity(0, 1), next),
        Err((TransmitError::Time, _))
    ));
    let aligned = Box::new(session(
        plan,
        identity(0, 2),
        FrameAnchor {
            time: at(4 * CHIPS_PER_FRAME),
            cfn: 0,
        },
        4,
    ));
    assert!(bank.stage_replacement(identity(0, 1), aligned).is_ok());
}

#[test]
fn overlapping_codes_do_not_waive_the_two_owner_memory_and_channel_budget() {
    let source = source(false);
    let mut bank = DedicatedTransmitBank::new(
        source.information(),
        at(0),
        count(1),
        count(1),
        count(2_000_000),
    )
    .unwrap();
    let mut old = candidate(true, 1, 0, 252, 3, 0);
    prepare(&mut old, Tfci::new(23).unwrap());
    insert(&mut bank, old);
    bank.arm(identity(0, 1)).unwrap();
    let before = bank.retained_bits;
    let next = Box::new(candidate(true, 2, 4, 0, 3, 0));
    let address = std::ptr::from_ref(next.as_ref());
    let Err((error, returned)) = bank.stage_replacement(identity(0, 1), next) else {
        panic!("uncharged overlap admitted");
    };
    assert_eq!(error, TransmitError::Capacity);
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    assert_eq!(bank.retained_bits, before);
    assert_eq!(bank.channels, 1);
}

#[test]
fn silent_and_future_owners_do_not_clear_or_mix_a_zero_contribution() {
    let marker = Complex32::new(3.0, -7.0);
    let mut staged = old_bank(true, 0);
    assert!(
        staged
            .stage_replacement(identity(0, 1), Box::new(candidate(true, 2, 4, 0, 3, 0)))
            .is_ok()
    );
    staged.remove_boxed(identity(0, 1)).unwrap();
    staged.contribution.fill(marker);
    let mut output = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    staged
        .mix_slot(at(0), &mut output, |_| panic!("silent owner requested TPC"))
        .unwrap();
    assert!(staged.contribution.iter().all(|value| *value == marker));
    assert!(output.iter().all(|value| *value == Complex32::default()));
    let mut future = bank(&source(false), 1);
    let mut owner = candidate(true, 1, 4, 0, 3, 0);
    prepare(&mut owner, Tfci::new(23).unwrap());
    insert(&mut future, owner);
    future.arm(identity(0, 1)).unwrap();
    future.contribution.fill(marker);
    future
        .mix_slot(at(0), &mut output, |_| {
            panic!("future owner requested early TPC")
        })
        .unwrap();
    assert!(future.contribution.iter().all(|value| *value == marker));
    assert!(output.iter().all(|value| *value == Complex32::default()));
}

#[test]
fn commit_at_an_already_synthesized_exact_boundary_stops_old_without_an_extra_pass() {
    let mut bank = old_bank(true, 0);
    let mut next = candidate(true, 2, 4, 0, 3, 0);
    prepare(&mut next, Tfci::new(23).unwrap());
    assert!(
        bank.stage_replacement(identity(0, 1), Box::new(next))
            .is_ok()
    );
    let mut output = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    for slot in 0..60 {
        output.fill(Complex32::default());
        bank.mix_slot(at(slot * CHIPS_PER_SLOT), &mut output, |_| Bit::Zero)
            .unwrap();
    }
    assert_eq!(bank.next_chip(), at(4 * CHIPS_PER_FRAME));
    bank.commit_replacement(identity(0, 1), identity(0, 2))
        .unwrap();
    assert!(matches!(
        bank.session(identity(0, 1)).unwrap().phase(),
        TransmitPhase::Stopped { .. }
    ));
    let mut seen = Vec::new();
    bank.mix_slot(at(4 * CHIPS_PER_FRAME), &mut output, |request| {
        seen.push(request.identity);
        Bit::One
    })
    .unwrap();
    assert_eq!(seen, [identity(0, 2)]);
    assert_eq!(
        bank.session(identity(0, 1))
            .unwrap()
            .counters()
            .physical_slots,
        60
    );
}
