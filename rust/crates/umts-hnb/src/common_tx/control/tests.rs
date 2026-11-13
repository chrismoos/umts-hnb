// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_tx::tests::recover_current_fach,
    radio_io::{PumpProgress, TxPreparation, TxPump, TxSettings, prepare_tx},
    test_support::{at, count, source},
};
use sdr_hal::{
    Radio,
    memory::{MemoryRadio, MemoryRadioHandle},
};
use std::time::Duration;

fn pipeline(
    capacity: usize,
    budget: usize,
) -> (TxPreparation, TxPump, CommonTxPort, MemoryRadioHandle) {
    let (mut radio, peer) = MemoryRadio::new(2);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source(false),
        tx,
        TxSettings::offline(1_000_000_000, count(1), count(1), count(3)),
    )
    .unwrap();
    let port = preparation
        .control_port(count(capacity), count(budget))
        .unwrap();
    pump.start().unwrap();
    (preparation, pump, port, peer)
}

fn payload(port: &CommonTxPort, channel: usize) -> Vec<Bit> {
    let bits = port.information().sib5().secondary_channels()[channel].transports()[0]
        .formats
        .groups[0]
        .transport_block_bits as usize;
    (0..bits)
        .map(|index| if index % 3 == 0 { Bit::One } else { Bit::Zero })
        .collect()
}

fn slot(preparation: &mut TxPreparation, pump: &mut TxPump, peer: &MemoryRadioHandle) {
    preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    peer.output.recv_timeout(Duration::from_secs(1)).unwrap();
}

fn ue(generation: u64) -> UeKey {
    UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::new(generation).unwrap(),
    }
}

#[test]
fn control_port_requires_worker_commit_and_full_driver_frame_including_filter_tail() {
    let (mut preparation, mut pump, mut port, peer) = pipeline(2, 1);
    let data = payload(&port, 1);
    let proof = port
        .publish_transport(at(0), 1, &data, &[], Some(ue(1)))
        .unwrap();
    assert_eq!(proof.plan_stage(), Some(PlanStage::Queued));
    assert!(!proof.submitted());
    for index in 0..16 {
        slot(&mut preparation, &mut pump, &peer);
        assert_eq!(proof.plan_stage(), Some(PlanStage::Committed));
        assert_eq!(proof.submitted(), index == 15);
    }
    let event = port.next_event().unwrap();
    assert_eq!(event.id, proof.plan_id().unwrap());
    assert_eq!(event.owner, Some(ue(1)));
    assert_eq!(event.stage, PlanStage::Committed);
    assert!(port.next_event().is_none());
    pump.stop().unwrap();
    assert!(!proof.submitted());
    assert!(!port.available());
}

#[test]
fn control_port_late_worker_rejection_cannot_be_confirmed_by_unrelated_driver_progress() {
    let (mut preparation, mut pump, mut port, peer) = pipeline(2, 1);
    let unrelated = preparation.source().transport_submission(at(0)).unwrap();
    let paging = payload(&port, 0);
    let _future = port
        .publish_transport(at(2 * CHIPS_PER_FRAME), 0, &paging, &[false; 18], None)
        .unwrap();
    let data = payload(&port, 1);
    let rejected = port
        .publish_transport(at(0), 1, &data, &[], Some(ue(1)))
        .unwrap();
    slot(&mut preparation, &mut pump, &peer);
    assert_eq!(rejected.plan_stage(), Some(PlanStage::Queued));
    slot(&mut preparation, &mut pump, &peer);
    assert_eq!(
        rejected.plan_stage(),
        Some(PlanStage::Rejected(PlanRejection::Late))
    );
    for _ in 2..16 {
        slot(&mut preparation, &mut pump, &peer);
    }
    assert!(unrelated.submitted());
    assert!(!rejected.submitted());
    let mut rejected_event = false;
    for _ in 0..2 {
        if let Some(event) = port.next_event() {
            rejected_event |= event.id == rejected.plan_id().unwrap()
                && event.stage == PlanStage::Rejected(PlanRejection::Late);
        }
    }
    assert!(rejected_event);
}

#[test]
fn control_port_queue_budget_and_retained_evidence_bound_reuse_without_erasing_outcomes() {
    let (mut preparation, mut pump, mut port, peer) = pipeline(1, 1);
    let data = payload(&port, 1);
    let proof = port.publish_transport(at(0), 1, &data, &[], None).unwrap();
    assert!(matches!(
        port.publish_transport(at(CHIPS_PER_FRAME), 1, &data, &[], None),
        Err(CommonTxError::Capacity)
    ));
    slot(&mut preparation, &mut pump, &peer);
    assert!(matches!(
        port.publish_transport(at(CHIPS_PER_FRAME), 1, &data, &[], None),
        Err(CommonTxError::Capacity)
    ));
    let event = port.next_event().unwrap();
    assert_eq!(event.stage, PlanStage::Committed);
    assert!(matches!(
        port.publish_transport(at(CHIPS_PER_FRAME), 1, &data, &[], None),
        Err(CommonTxError::Capacity)
    ));
    drop(proof);
    let next = port
        .publish_transport(at(CHIPS_PER_FRAME), 1, &data, &[], None)
        .unwrap();
    assert!(next.plan_id().unwrap().get() > event.id.get());
    assert_eq!(next.plan_stage(), Some(PlanStage::Queued));
}

#[test]
fn control_port_backpressure_preserves_um_sdu_and_sequence_for_retry() {
    use crate::common_control_tx::{CommonControlDownlink, CommonControlTxError};
    use umts_rlc::unacknowledged::UmLimits;
    use umts_types::bits::Bits;
    let (mut preparation, mut pump, mut port, peer) = pipeline(1, 1);
    let data = payload(&port, 1);
    let proof = port.publish_transport(at(0), 1, &data, &[], None).unwrap();
    let mut downlink = CommonControlDownlink::new(
        &port,
        1,
        UmLimits {
            maximum_pdu_bytes: CommonControlDownlink::maximum_pdu_bytes(&port, 1).unwrap(),
            maximum_sdu_bytes: count(512),
            queued_sdus: count(4),
            maximum_buffer_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    downlink.enqueue(Bits::new(&[0xa5], 8).unwrap()).unwrap();
    assert_eq!(
        downlink.publish_frame(&mut port, at(CHIPS_PER_FRAME)),
        Err(CommonControlTxError::Transmit(CommonTxError::Capacity))
    );
    assert_eq!(downlink.sequence(), 0);
    assert_eq!(downlink.pending_sdus(), 1);
    slot(&mut preparation, &mut pump, &peer);
    port.next_event().unwrap();
    drop(proof);
    assert_eq!(
        downlink.publish_frame(&mut port, at(CHIPS_PER_FRAME)),
        Ok(true)
    );
    assert_eq!(downlink.sequence(), 1);
    assert_eq!(downlink.pending_sdus(), 0);
}

#[test]
fn control_port_cancellation_is_generation_scoped_and_keeps_peer_plans() {
    let (mut preparation, mut pump, mut port, peer) = pipeline(3, 2);
    let data = payload(&port, 1);
    let old = port
        .publish_transport(at(CHIPS_PER_FRAME), 1, &data, &[], Some(ue(1)))
        .unwrap();
    let current = port
        .publish_transport(at(2 * CHIPS_PER_FRAME), 1, &data, &[], Some(ue(2)))
        .unwrap();
    assert_eq!(
        port.cancel_ue(ue(1)),
        Cancellation {
            cancelled: 1,
            committed: 0
        }
    );
    assert_eq!(old.plan_stage(), Some(PlanStage::Cancelled));
    assert_eq!(current.plan_stage(), Some(PlanStage::Queued));
    for index in 0..46 {
        slot(&mut preparation, &mut pump, &peer);
        if index == 15 {
            assert!(
                preparation.source().secondary[1]
                    .data
                    .iter()
                    .all(|bit| *bit == TxBit::Dtx)
            );
        }
        if index == 30 {
            let bytes = recover_current_fach(preparation.source());
            let mut expected = vec![0; data.len().div_ceil(umts_types::bits::BITS_PER_BYTE)];
            umts_dsp::bits::pack_msb(&data, &mut expected).unwrap();
            assert_eq!(bytes, expected);
        }
    }
    assert!(!old.submitted());
    assert!(current.submitted());
    assert_eq!(
        port.cancel_ue(ue(2)),
        Cancellation {
            cancelled: 0,
            committed: 1
        }
    );
}

#[test]
fn control_port_plan_identity_includes_the_radio_epoch() {
    let first = PlanId {
        epoch: at(0).epoch,
        lane: PlanLane::Transport,
        serial: NonZeroU64::MIN,
    };
    let second = PlanId {
        epoch: umts_types::RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        lane: PlanLane::Transport,
        serial: NonZeroU64::MIN,
    };
    assert_eq!(first.get(), second.get());
    assert_ne!(first, second);
    assert_eq!(first.epoch(), at(0).epoch);
}

#[test]
fn control_port_cancellation_and_commit_have_one_winner_under_concurrent_execution() {
    for _ in 0..64 {
        let state = Arc::new(PlanState {
            id: PlanId {
                epoch: at(0).epoch,
                lane: PlanLane::Transport,
                serial: NonZeroU64::MIN,
            },
            owner: Some(ue(1)),
            stage: AtomicU8::new(SCHEDULED),
        });
        let writer = PlanWriter(state.clone());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            writer.commit()
        });
        barrier.wait();
        let cancellation = state.terminate(CANCELLED);
        let committed = worker.join().unwrap();
        if committed {
            assert_eq!(cancellation, PlanStage::Committed);
            assert_eq!(state.stage(), PlanStage::Committed);
        } else {
            assert_eq!(cancellation, PlanStage::Cancelled);
            assert_eq!(state.stage(), PlanStage::Cancelled);
        }
    }
}

#[test]
fn control_port_paging_commit_starts_at_the_pich_not_the_later_transport_frame() {
    let (mut preparation, mut pump, mut port, peer) = pipeline(1, 1);
    let data = payload(&port, 0);
    let proof = port
        .publish_transport(at(2 * CHIPS_PER_FRAME), 0, &data, &[true; 18], Some(ue(1)))
        .unwrap();
    for _ in 0..12 {
        slot(&mut preparation, &mut pump, &peer);
    }
    assert_eq!(proof.plan_stage(), Some(PlanStage::Scheduled));
    slot(&mut preparation, &mut pump, &peer);
    assert_eq!(proof.plan_stage(), Some(PlanStage::Committed));
    assert_eq!(
        port.cancel_ue(ue(1)),
        Cancellation {
            cancelled: 0,
            committed: 1
        }
    );
    for _ in 13..46 {
        slot(&mut preparation, &mut pump, &peer);
    }
    assert!(proof.submitted());
}

#[test]
fn control_port_drop_or_worker_drop_closes_queued_work_without_false_receipts() {
    let (preparation, pump, mut port, _) = pipeline(1, 1);
    let data = payload(&port, 1);
    let proof = port.publish_transport(at(0), 1, &data, &[], None).unwrap();
    drop(preparation);
    assert!(!port.available());
    assert_eq!(
        proof.plan_stage(),
        Some(PlanStage::Rejected(PlanRejection::Closed))
    );
    assert!(!proof.submitted());
    drop(pump);
    let (mut preparation, _pump, port, _) = pipeline(1, 1);
    drop(port);
    assert!(preparation.prepare_one().is_err());
    assert!(preparation.stop_token().is_stopping());
}

#[test]
fn control_port_source_close_racing_publication_never_leaves_queued_evidence() {
    for _ in 0..24 {
        let (preparation, pump, mut port, _peer) = pipeline(1, 1);
        let data = payload(&port, 1);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            drop(preparation);
        });
        barrier.wait();
        let result = port.publish_transport(at(0), 1, &data, &[], None);
        worker.join().unwrap();
        match result {
            Ok(proof) => {
                assert_eq!(
                    proof.plan_stage(),
                    Some(PlanStage::Rejected(PlanRejection::Closed))
                );
                assert!(!proof.submitted());
            }
            Err(error) => assert_eq!(error, CommonTxError::Poisoned),
        }
        assert!(!port.available());
        drop(pump);
    }
}

#[test]
fn control_port_preserves_controller_setup_through_um_mac_and_composite_fach_decode() {
    use crate::{
        common_control_tx::CommonControlDownlink,
        signalling::{
            CommonSignalling,
            tests::{limits, request},
        },
    };
    use umts_rlc::unacknowledged::{EInterpretation, LiWidth, UmConfig, UmLimits, UmReceiver};
    use umts_types::bits::{BITS_PER_BYTE, Bits};
    let (mut preparation, mut pump, mut port, peer) = pipeline(4, 2);
    let mut controller = CommonSignalling::new(&port, 1, 1, limits()).unwrap();
    let key = controller.request(&port, at(0), &request(0)).unwrap();
    let mut receiver = UmReceiver::new(
        UmConfig {
            li_width: LiWidth::Seven,
            interpretation: EInterpretation::Normal,
        },
        UmLimits {
            maximum_pdu_bytes: CommonControlDownlink::maximum_pdu_bytes(&port, 1).unwrap(),
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(4),
            maximum_buffer_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    let mut received = Vec::new();
    for frame in 0..32 {
        assert!(
            controller
                .publish_rb0(&mut port, at(frame * CHIPS_PER_FRAME))
                .unwrap()
        );
        slot(&mut preparation, &mut pump, &peer);
        let bytes = recover_current_fach(preparation.source());
        let mac = umts_mac::decode_common(
            umts_mac::FddCommonChannel::Fach,
            Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
        )
        .unwrap();
        assert_eq!(mac.header, umts_mac::CommonHeader::CommonControl);
        let mut rlc = vec![0; mac.payload.len() / BITS_PER_BYTE];
        mac.payload.copy_to(&mut rlc).unwrap();
        received.extend(receiver.receive(&rlc).unwrap().sdus().map(<[u8]>::to_vec));
        for _ in 1..15 {
            slot(&mut preparation, &mut pump, &peer);
        }
        for _ in 0..4 {
            if port.next_event().is_none() {
                break;
            }
        }
        if !received.is_empty() {
            break;
        }
    }
    assert_eq!(received.len(), 1);
    let setup =
        umts_rrc_codec::decode_downlink_common(&received[0], received[0].len() * BITS_PER_BYTE)
            .unwrap();
    let request = request(0);
    let umts_rrc_schema::class_definitions::ULCCCHMessageType::rrcConnectionRequest(body) =
        &request.value.message
    else {
        unreachable!()
    };
    let expected = umts_rrc::establishment::build_cell_fach_setup(
        &body.initial_ue_identity,
        &umts_rrc::establishment::CellFachSetup {
            assigned: controller.assigned(key).unwrap(),
            transaction: umts_rrc::establishment::TransactionId::new(0).unwrap(),
            drx_coefficient: 5,
            signalling: umts_rrc::establishment::SignallingProfile,
        },
    )
    .unwrap();
    assert_eq!(
        setup.value,
        umts_rrc_codec::decode_downlink_common(&expected.bytes, expected.bit_length)
            .unwrap()
            .value
    );
}
