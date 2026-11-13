// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::{NonZeroU8, NonZeroU64, NonZeroUsize};
use umts_dsp::transport_blocks::CrcCounts;
use umts_types::RadioEpoch;

#[test]
fn edch_requires_first_attempt_and_signal_support_for_both_crc_outcomes() {
    use super::super::UplinkDecodeObservation;
    use umts_dsp::{
        hsupa::{ControlDecision, ControlInformation},
        transport_blocks::BlockOutcome,
    };
    use umts_mac::hsupa::Reception;
    use umts_types::hsupa::{Etfci, Rsn};
    for outcome in [BlockOutcome::Decoded, BlockOutcome::InvalidCrc { block: 0 }] {
        for (presence, expected) in [
            (Some(100.0), CrcEligibility::AirLink),
            (Some(-100.0), CrcEligibility::UncertainControl),
            (None, CrcEligibility::UncertainControl),
            (Some(f64::NAN), CrcEligibility::UncertainControl),
        ] {
            let mut observation = UplinkDecodeObservation::Edch {
                identity: identity(1),
                start: at(10),
                control: ControlDecision {
                    information: ControlInformation {
                        format: Etfci::new(127).unwrap(),
                        sequence: Rsn::new(0).unwrap(),
                        happy: true,
                    },
                    normalized_score: 1.0,
                    normalized_margin: Some(0.5),
                },
                feedback: if outcome == BlockOutcome::Decoded {
                    Reception::Ack { deliver: true }
                } else {
                    Reception::Nack
                },
                outcome,
                presence_log_likelihood: presence,
            };
            assert_eq!(
                LinkCrcObservation::from_decode(observation).eligibility,
                expected
            );
            if let UplinkDecodeObservation::Edch { control, .. } = &mut observation {
                control.information.sequence = Rsn::new(1).unwrap();
            }
            assert_eq!(
                LinkCrcObservation::from_decode(observation).eligibility,
                CrcEligibility::Retransmission
            );
        }
    }
}

fn identity(configuration: u64) -> ReceiveIdentity {
    ReceiveIdentity {
        ue: UeKey {
            epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
            slot: 0,
            generation: NonZeroU64::new(1).unwrap(),
        },
        configuration: NonZeroU64::new(configuration).unwrap(),
    }
}

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: identity(1).ue.epoch,
        index,
    }
}

fn observation(configuration: u64, index: u64, passed: usize, failed: usize) -> LinkCrcObservation {
    LinkCrcObservation {
        identity: identity(configuration),
        start: at(index),
        stream: CrcStream::Dch(NonZeroU8::new(1).unwrap()),
        crc_counts: CrcCounts { passed, failed },
        eligibility: CrcEligibility::AirLink,
    }
}

#[test]
fn counts_balance_and_exclusions_do_not_change_target() {
    let settings = OuterLoopSettings::new(10.0, 17.0, 20.0, 0.1).unwrap();
    let mut controller = LinkOuterLoop::new(identity(1).ue, settings).unwrap();
    controller.observe(observation(1, 1, 9, 1)).unwrap();
    assert!((controller.status().target.db() - 17.0).abs() < 1e-12);
    for (offset, eligibility) in [
        CrcEligibility::LocalLoss,
        CrcEligibility::Retransmission,
        CrcEligibility::NoTransmission,
        CrcEligibility::Unchecked,
        CrcEligibility::UncertainControl,
    ]
    .into_iter()
    .enumerate()
    {
        let mut excluded = observation(1, offset as u64 + 2, 0, 100);
        excluded.eligibility = eligibility;
        controller.observe(excluded).unwrap();
    }
    assert!((controller.status().target.db() - 17.0).abs() < 1e-12);
    assert_eq!(controller.status().counters.passed, 9);
    assert_eq!(controller.status().counters.failed, 1);
    controller.observe(observation(1, 7, 0, 100)).unwrap();
    assert_eq!(controller.status().target.db(), 20.0);
    controller.observe(observation(1, 7, 1000, 0)).unwrap();
    assert_eq!(controller.status().target.db(), 20.0);
    assert_eq!(controller.status().counters.stale, 1);
    controller.observe(observation(1, 8, 1000, 0)).unwrap();
    assert_eq!(controller.status().target.db(), 10.0);
}

#[test]
fn replacement_preserves_target_and_rejects_out_of_window_results() {
    let settings = OuterLoopSettings::new(10.0, 17.0, 20.0, 0.1).unwrap();
    let mut registry = LinkPowerRegistry::new(NonZeroUsize::new(1).unwrap()).unwrap();
    registry.attach(identity(1), settings).unwrap();
    registry.activate(identity(1), at(10)).unwrap();
    registry.observe(observation(1, 11, 0, 1)).unwrap();
    registry.attach(identity(2), settings).unwrap();
    registry.replace(identity(1), identity(2), at(20)).unwrap();
    assert_eq!(registry.status(identity(2)).unwrap().target.db(), 17.25);
    registry.observe(observation(1, 20, 0, 1)).unwrap();
    registry.observe(observation(2, 19, 0, 1)).unwrap();
    assert_eq!(registry.status(identity(2)).unwrap().target.db(), 17.25);
    assert_eq!(registry.status(identity(2)).unwrap().counters.stale, 2);
    registry.retire(identity(1)).unwrap();
    registry.observe(observation(2, 20, 0, 1)).unwrap();
    assert_eq!(registry.status(identity(2)).unwrap().target.db(), 17.5);
    registry.retire(identity(2)).unwrap();
    assert!(registry.status(identity(2)).is_err());
}
