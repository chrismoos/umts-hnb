// SPDX-License-Identifier: AGPL-3.0-or-later
use super::super::adaptation::{FeedbackTraining, LinkAdaptation};
use super::*;
use crate::{
    hsdpa_packet::PlanFormat,
    hsdpa_rx::{CarrierAcknowledgements, EvidenceLimits, Observation},
    test_support::{at, count},
};
use std::num::NonZeroU8;
use umts_dsp::hsdpa::{
    CellAck, DualHarqAck, FeedbackEvidence, Modulation, TransmissionVersion, TransportTable,
};
use umts_mac::hsdpa::{HarqEntity, ProcessId, ScheduledTransmission};
use umts_types::{UeKey, hsdpa::DualCellCategory};

fn fixture(incarnation: u64) -> (ScheduledTransmission, PlanFormat) {
    let mut harq = HarqEntity::new(
        UeKey {
            epoch: at(0).epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        },
        NonZeroU64::new(incarnation).unwrap(),
        DualCellCategory::new(24).unwrap().per_cell_category(),
        at(0),
        &[count(43_200); 6],
        NonZeroU8::MIN,
    )
    .unwrap();
    let process = ProcessId::new(0).unwrap();
    harq.insert(process, ()).unwrap();
    let transmission = harq.schedule(process, at(0), at(30_000)).unwrap();
    (
        transmission,
        PlanFormat {
            transport_bits: count(38_576),
            soft_bits: count(43_200),
            first_code: 1,
            codes: 15,
            modulation: Modulation::Qam64,
            size_index: 57,
            version: TransmissionVersion::new(0).unwrap(),
            table: TransportTable::OctetAligned,
            control_list_number: 1,
        },
    )
}

fn classify(word: DualHarqAck) -> [Feedback; 2] {
    let limits = EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
    FeedbackPolicy {
        acknowledgement: limits,
        cqi: limits,
    }
    .classify_ack_cells(&Observation {
        evidence: FeedbackEvidence {
            candidate: Some(CarrierAcknowledgements::Dual(word)),
            normalized_correlation: 1.0,
            normalized_margin: 1.0,
            mean_soft_power: 1.0,
        },
        minimum_pilot_fraction: 1.0,
        mean_relative_pilot_disturbance: 0.0,
    })
}

#[test]
fn dual_feedback_captures_each_original_cqi_before_consuming_association() {
    let mut snapshots = [FeedbackSnapshot::default(); 2];
    let mut adaptation = [LinkAdaptation::per_cqi(3, 10); 2];
    let reported = [Cqi::new(30).unwrap(), Cqi::new(29).unwrap()];
    for (word, expected) in [
        (
            DualHarqAck::Both {
                primary: CellAck::Ack,
                secondary: CellAck::Ack,
            },
            [10, 10],
        ),
        (
            DualHarqAck::Both {
                primary: CellAck::Nack,
                secondary: CellAck::Ack,
            },
            [-80, 20],
        ),
        (DualHarqAck::Primary(CellAck::Ack), [-70, 20]),
    ] {
        let feedback = classify(word);
        for cell in 0..2 {
            let (tx, format) = fixture(cell as u64 + 1);
            snapshots[cell].prepared_format(tx, format, false);
            snapshots[cell]
                .formats
                .selection_cqi(tx, Some(reported[cell]));
            adaptation[cell].bind_packet(tx.key.process(), FeedbackTraining::Bidirectional);
            snapshots[cell].admit_with_adaptation(
                &mut adaptation[cell],
                tx,
                Some(feedback[cell]),
                Some(at(40_000)),
            );
            assert_eq!(
                adaptation[cell].reported_offsets().unwrap()[usize::from(reported[cell].value())],
                expected[cell]
            );
            assert_eq!(snapshots[cell].cqi_offset_milli, expected[cell]);
            assert_eq!(snapshots[cell].formats.first_reported_cqi(tx), None);
            // A duplicate cannot apply the same first-attempt outcome twice.
            snapshots[cell].admit_with_adaptation(
                &mut adaptation[cell],
                tx,
                Some(Feedback::Ack),
                Some(at(40_000)),
            );
            assert_eq!(adaptation[cell].offset_milli(), expected[cell]);
        }
    }
}

#[test]
fn stale_mismatched_missing_and_retry_associations_do_not_calibrate() {
    for case in 0..4 {
        let (tx, format) = fixture(1);
        let mut snapshot = FeedbackSnapshot::default();
        let mut adaptation = LinkAdaptation::per_cqi(3, 10);
        if case != 2 {
            snapshot.prepared_format(tx, format, case == 3);
            snapshot
                .formats
                .selection_cqi(tx, Some(Cqi::new(30).unwrap()));
        }
        adaptation.bind_packet(tx.key.process(), FeedbackTraining::Bidirectional);
        let observed = match case {
            0 => ScheduledTransmission {
                expected_ack: at(tx.expected_ack.index + 1),
                ..tx
            },
            1 => fixture(2).0,
            _ => tx,
        };
        snapshot.admit_with_adaptation(
            &mut adaptation,
            observed,
            Some(Feedback::Ack),
            Some(at(40_000)),
        );
        assert_eq!(adaptation.offset_milli(), 0);
        assert_eq!(snapshot.cqi_offset_milli, 0);
        assert_eq!(adaptation.reported_offsets().unwrap()[30], 0);
    }
}

#[test]
fn dual_feedback_keeps_constrained_training_independent_of_the_other_carrier() {
    let mut snapshots = [FeedbackSnapshot::default(); 2];
    let mut adaptation = [LinkAdaptation::per_cqi(3, 10); 2];
    let reported = [Cqi::new(30).unwrap(), Cqi::new(29).unwrap()];
    let training = [
        FeedbackTraining::DecreaseOnly,
        FeedbackTraining::Bidirectional,
    ];
    for (word, expected) in [
        (
            DualHarqAck::Both {
                primary: CellAck::Ack,
                secondary: CellAck::Nack,
            },
            [0, -90],
        ),
        (
            DualHarqAck::Both {
                primary: CellAck::Nack,
                secondary: CellAck::Ack,
            },
            [-90, -80],
        ),
    ] {
        let feedback = classify(word);
        for cell in 0..2 {
            let (tx, format) = fixture(cell as u64 + 1);
            snapshots[cell].prepared_format(tx, format, false);
            snapshots[cell]
                .formats
                .selection_cqi(tx, Some(reported[cell]));
            adaptation[cell].bind_packet(tx.key.process(), training[cell]);
            snapshots[cell].admit_with_adaptation(
                &mut adaptation[cell],
                tx,
                Some(feedback[cell]),
                Some(at(40_000)),
            );
            assert_eq!(
                adaptation[cell].reported_offsets().unwrap()[usize::from(reported[cell].value())],
                expected[cell]
            );
            snapshots[cell].admit_with_adaptation(
                &mut adaptation[cell],
                tx,
                Some(Feedback::Nack),
                Some(at(40_000)),
            );
            assert_eq!(
                adaptation[cell].reported_offsets().unwrap()[usize::from(reported[cell].value())],
                expected[cell]
            );
        }
    }
}
