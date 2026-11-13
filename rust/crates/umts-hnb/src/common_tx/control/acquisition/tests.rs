// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    radio_io::{PumpProgress, TxPreparation, TxPump, TxSettings, prepare_tx},
    test_support::{at, count, source},
};
use sdr_hal::{
    Radio,
    memory::{MemoryRadio, MemoryRadioHandle},
};
use std::time::Duration;
use umts_dsp::prach::PreambleSignature;

const NOMINAL: u64 = 23040;
const CYCLE: u64 = 61440;
const AI_START: u64 = 35840;

fn observation(cycle: u64) -> AccessObservation {
    AccessObservation {
        code: PrachCode::new(7504).unwrap(),
        signature: PreambleSignature::new(13).unwrap(),
        nominal_start: at(NOMINAL + cycle * CYCLE),
        received_start: at(NOMINAL + cycle * CYCLE),
    }
}

fn pipeline(
    capacity: usize,
    budget: usize,
) -> (TxPreparation, TxPump, ControlPorts, MemoryRadioHandle) {
    let (mut radio, peer) = MemoryRadio::new(2);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source(false),
        tx,
        TxSettings::offline(1_000_000_000, count(1), count(1), count(3)),
    )
    .unwrap();
    let ports = preparation
        .control_ports(count(4), count(1), count(capacity), count(budget))
        .unwrap();
    pump.start().unwrap();
    (preparation, pump, ports, peer)
}

fn slot(preparation: &mut TxPreparation, pump: &mut TxPump, peer: &MemoryRadioHandle) {
    preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    peer.output.recv_timeout(Duration::from_secs(1)).unwrap();
}

fn publish(port: &mut AcquisitionPort, cycle: u64) -> AcquisitionPublication {
    let observation = observation(cycle);
    port.publish_acquisition(
        0,
        observation,
        Acquisition::Positive,
        at(observation.received_start.index + 4096),
        0,
    )
    .unwrap()
}

#[test]
fn access_lane_bypasses_control_backlog_and_proves_only_its_complete_indicator_interval() {
    let (mut preparation, mut pump, mut ports, peer) = pipeline(2, 1);
    let bits = ports.transport.information().sib5().secondary_channels()[1].transports()[0]
        .formats
        .groups[0]
        .transport_block_bits as usize;
    let payload = vec![Bit::One; bits];
    let transport = ports
        .transport
        .publish_transport(at(CHIPS_PER_FRAME), 1, &payload, &[], None)
        .unwrap();
    let second = ports
        .transport
        .publish_transport(at(2 * CHIPS_PER_FRAME), 1, &payload, &[], None)
        .unwrap();
    let mut access = publish(&mut ports.acquisition, 0);
    assert_eq!(access.transport.plan_stage(), Some(PlanStage::Queued));
    assert_ne!(
        access.transport.plan_id().unwrap(),
        transport.plan_id().unwrap()
    );
    assert_eq!(
        access.transport.plan_id().unwrap().lane(),
        PlanLane::Acquisition
    );
    for index in 0..17 {
        slot(&mut preparation, &mut pump, &peer);
        if index == 0 {
            assert_eq!(access.transport.plan_stage(), Some(PlanStage::Scheduled));
            assert_eq!(second.plan_stage(), Some(PlanStage::Queued));
        }
        if index == 14 {
            assert_eq!(access.cancel_pending(), PlanStage::Committed);
        }
        assert_eq!(access.transport.submitted(), index == 16);
    }
    let event = ports.acquisition.next_event().unwrap();
    assert_eq!(event.window.indicator_start, at(AI_START));
    assert_eq!(event.observation.signature, observation(0).signature);
    assert_eq!(event.stage, PlanStage::Committed);
    assert!(ports.acquisition.next_event().is_none());
}

#[test]
fn dropping_access_authority_suppresses_queued_and_scheduled_waveforms() {
    for scheduled in [false, true] {
        let (mut preparation, mut pump, mut ports, peer) = pipeline(2, 1);
        let access = publish(&mut ports.acquisition, 0);
        let evidence = access.transport.clone();
        if scheduled {
            slot(&mut preparation, &mut pump, &peer);
        }
        drop(access);
        assert_eq!(evidence.plan_stage(), Some(PlanStage::Cancelled));
        let mut reference = source(false);
        let mut samples = vec![Complex32::default(); reference.samples_per_slot()];
        while reference.next_chip().index <= AI_START {
            reference.generate_slot(&mut samples).unwrap();
        }
        while preparation.source().next_chip().index <= AI_START {
            slot(&mut preparation, &mut pump, &peer);
        }
        assert_eq!(preparation.source().slot, reference.slot);
        assert!(!evidence.submitted());
        assert_eq!(
            ports.acquisition.next_event().unwrap().stage,
            PlanStage::Cancelled
        );
    }
}

#[test]
fn access_status_pool_retains_unread_outcomes_and_external_receipts() {
    let (mut preparation, mut pump, mut ports, peer) = pipeline(1, 1);
    let mut access = publish(&mut ports.acquisition, 0);
    let proof = access.transport.clone();
    let id = proof.plan_id().unwrap();
    assert_eq!(access.cancel_pending(), PlanStage::Cancelled);
    drop(access);
    slot(&mut preparation, &mut pump, &peer);
    let attempt = |port: &mut AcquisitionPort| {
        port.publish_acquisition(
            0,
            observation(1),
            Acquisition::Positive,
            at(observation(1).received_start.index + 4096),
            0,
        )
    };
    assert!(matches!(
        attempt(&mut ports.acquisition),
        Err(CommonTxError::Capacity)
    ));
    assert_eq!(
        ports.acquisition.next_event().unwrap().stage,
        PlanStage::Cancelled
    );
    assert!(matches!(
        attempt(&mut ports.acquisition),
        Err(CommonTxError::Capacity)
    ));
    drop(proof);
    let next = attempt(&mut ports.acquisition).unwrap();
    assert!(next.transport.plan_id().unwrap().get() > id.get());
}

#[test]
fn access_worker_rechecks_deadline_and_reports_late_without_claiming_transmission() {
    let (mut preparation, mut pump, mut ports, peer) = pipeline(3, 1);
    for _ in 0..13 {
        slot(&mut preparation, &mut pump, &peer);
    }
    let _first = publish(&mut ports.acquisition, 1);
    let _second = publish(&mut ports.acquisition, 2);
    let late = publish(&mut ports.acquisition, 0);
    for _ in 0..3 {
        slot(&mut preparation, &mut pump, &peer);
    }
    assert_eq!(
        late.transport.plan_stage(),
        Some(PlanStage::Rejected(PlanRejection::Late))
    );
    assert!(!late.viable());
    assert!(!late.transport.submitted());
    let mut found = false;
    for _ in 0..3 {
        if let Some(event) = ports.acquisition.next_event() {
            found |= event.id == late.transport.plan_id().unwrap()
                && event.stage == PlanStage::Rejected(PlanRejection::Late);
        }
    }
    assert!(found);
}

#[test]
fn access_source_close_publication_race_cannot_strand_queued_work() {
    for _ in 0..24 {
        let (preparation, _pump, mut ports, _peer) = pipeline(2, 1);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            drop(preparation);
        });
        barrier.wait();
        let access = ports.acquisition.publish_acquisition(
            0,
            observation(0),
            Acquisition::Positive,
            at(NOMINAL + 4096),
            0,
        );
        worker.join().unwrap();
        match access {
            Ok(access) => {
                assert_eq!(
                    access.transport.plan_stage(),
                    Some(PlanStage::Rejected(PlanRejection::Closed))
                );
                assert!(!access.viable());
            }
            Err(error) => assert_eq!(error, CommonTxError::Poisoned),
        }
    }
}

#[test]
fn access_port_factory_validates_every_limit_before_transferring_ownership() {
    let (mut radio, _peer) = MemoryRadio::new(2);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, _pump) = prepare_tx(
        source(false),
        tx,
        TxSettings::offline(0, count(1), count(1), count(2)),
    )
    .unwrap();
    assert!(matches!(
        preparation.control_ports(count(2), count(1), count(65), count(1)),
        Err(CommonTxError::Capacity)
    ));
    assert!(matches!(
        preparation.control_ports(count(2), count(1), count(2), count(3)),
        Err(CommonTxError::Capacity)
    ));
    let ports = preparation
        .control_ports(count(2), count(1), count(2), count(1))
        .unwrap();
    assert!(ports.transport.available());
    assert!(matches!(
        preparation.source_mut().schedule_acquisition(
            0,
            observation(0),
            Acquisition::Positive,
            at(NOMINAL + 4096),
            0
        ),
        Err(CommonTxError::Conflict)
    ));
    assert!(matches!(
        preparation.control_ports(count(2), count(1), count(2), count(1)),
        Err(CommonTxError::Conflict)
    ));
}

#[test]
fn access_cancellation_races_waveform_commit_without_false_suppression() {
    let mut reference = source(false);
    let mut samples = vec![Complex32::default(); reference.samples_per_slot()];
    while reference.next_chip().index <= AI_START {
        reference.generate_slot(&mut samples).unwrap();
    }
    let empty = reference.slot.clone();
    for _ in 0..32 {
        let (mut preparation, mut pump, mut ports, peer) = pipeline(1, 1);
        let mut access = publish(&mut ports.acquisition, 0);
        for _ in 0..14 {
            slot(&mut preparation, &mut pump, &peer);
        }
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            slot(&mut preparation, &mut pump, &peer);
            preparation.source().slot.clone()
        });
        barrier.wait();
        let stage = access.cancel_pending();
        let actual = worker.join().unwrap();
        match stage {
            PlanStage::Cancelled => assert_eq!(actual, empty),
            PlanStage::Committed => assert_ne!(actual, empty),
            stage => panic!("unexpected access cancellation result {stage:?}"),
        }
        assert_eq!(access.transport.plan_stage(), Some(stage));
    }
}
