// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{BearerId, DedicatedBearers, DownlinkFrame, FrameBuildError, UpperSdu},
    dedicated_rx::ReceiveIdentity,
    rel99_admission::{FrameOpportunity, FramePolicy},
    signalling::CommonSignalling,
};
use std::{collections::VecDeque, num::NonZeroU64};
use umts_dsp::tfci::Tfci;
use umts_rlc::transparent::Quality;
use umts_security::ciphering::Scope;
use umts_types::{CnDomain, RadioEpoch, UeKey, bits::Bits, time::ChipInstant};

const PAYLOAD: [u8; 2] = [0xab, 0xcd];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Route,
    Closed,
}

struct Policy {
    expected: VecDeque<(u32, Result<bool, Failure>)>,
    seen: Vec<UpperSdu>,
}

impl Policy {
    fn new(expected: impl IntoIterator<Item = (u32, Result<bool, Failure>)>) -> Self {
        Self {
            expected: expected.into_iter().collect(),
            seen: Vec::new(),
        }
    }
}

impl FramePolicy for Policy {
    fn select(
        &mut self,
        _: FrameOpportunity,
        _: ChipInstant,
        _: &mut DedicatedBearers,
        _: &mut CommonSignalling,
    ) -> Result<Tfci, FrameBuildError> {
        panic!("uplink outcome test cannot schedule a frame")
    }

    fn fill(
        &mut self,
        _: &mut DownlinkFrame<'_>,
        _: ChipInstant,
        _: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError> {
        panic!("uplink outcome test cannot build a frame")
    }
}

impl DuplexFramePolicy for Policy {
    type UplinkError = Failure;

    fn uplink(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Self::UplinkError> {
        let (slot, result) = self
            .expected
            .pop_front()
            .expect("unexpected delivery callback");
        assert_eq!(delivery.metadata.identity.ue.slot, slot);
        assert_eq!(delivery.metadata.bits, delivery.data.len());
        assert_eq!(delivery.data.read_u32(0, 16), Ok(0xabcd));
        self.seen.push(delivery.metadata);
        result
    }
}

fn metadata(slot: u32, received: u64) -> UpperSdu {
    let epoch = RadioEpoch::new(NonZeroU64::MIN);
    UpperSdu {
        identity: ReceiveIdentity {
            ue: UeKey {
                epoch,
                slot,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::MIN,
        },
        bearer: BearerId::new(20).unwrap(),
        scope: Scope::User(CnDomain::Ps),
        received: ChipInstant {
            epoch,
            index: received,
        },
        bits: PAYLOAD.len() * 8,
        quality: Quality::Correct,
        coverage: None,
        transparent_origin: None,
    }
}

fn delivery(metadata: UpperSdu) -> SduDelivery<'static> {
    SduDelivery {
        mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
        metadata,
        data: Bits::new(&PAYLOAD, metadata.bits).unwrap(),
    }
}

#[test]
fn one_ue_error_retains_its_delivery_without_preventing_a_peer_or_retry_from_accepting() {
    let first = metadata(0, 10);
    let peer = metadata(1, 20);
    let mut policy = Policy::new([(0, Err(Failure::Route)), (1, Ok(true)), (0, Ok(true))]);
    let mut outcomes = UplinkOutcomes::default();
    assert!(!outcomes.deliver(&mut policy, delivery(first)));
    assert!(outcomes.deliver(&mut policy, delivery(peer)));
    assert!(outcomes.deliver(&mut policy, delivery(first)));
    assert_eq!(policy.seen, [first, peer, first]);
    assert!(policy.expected.is_empty());
    assert_eq!(outcomes.failures, 1);
    let failure = outcomes.first.unwrap();
    assert_eq!(failure.metadata, first);
    assert_eq!(failure.error, Failure::Route);
}

#[test]
fn every_error_is_counted_but_only_the_original_typed_failure_and_metadata_are_retained() {
    let first = metadata(0, 10);
    let second = metadata(1, 20);
    let later = metadata(0, 30);
    let mut policy = Policy::new([
        (0, Err(Failure::Route)),
        (1, Err(Failure::Closed)),
        (0, Err(Failure::Closed)),
        (1, Ok(true)),
    ]);
    let mut outcomes = UplinkOutcomes::default();
    for item in [first, second, later] {
        assert!(!outcomes.deliver(&mut policy, delivery(item)));
    }
    assert!(outcomes.deliver(&mut policy, delivery(second)));
    assert_eq!(outcomes.failures, 3);
    let failure = outcomes.first.unwrap();
    assert_eq!(failure.metadata, first);
    assert_eq!(failure.error, Failure::Route);
    assert_eq!(policy.seen, [first, second, later, second]);
    assert!(policy.expected.is_empty());
}

#[test]
fn ordinary_backpressure_is_not_a_failure_and_does_not_suppress_other_deliveries() {
    let first = metadata(0, 10);
    let peer = metadata(1, 20);
    let mut policy = Policy::new([(0, Ok(false)), (1, Ok(true)), (0, Ok(true))]);
    let mut outcomes = UplinkOutcomes::default();
    assert!(!outcomes.deliver(&mut policy, delivery(first)));
    assert!(outcomes.deliver(&mut policy, delivery(peer)));
    assert!(outcomes.deliver(&mut policy, delivery(first)));
    assert_eq!(outcomes.failures, 0);
    assert!(outcomes.first.is_none());
    assert_eq!(policy.seen, [first, peer, first]);
    assert!(policy.expected.is_empty());
}

#[test]
fn saturated_error_accounting_never_wraps_or_replaces_the_first_failure() {
    let first = metadata(0, 10);
    let peer = metadata(1, 20);
    let mut policy = Policy::new([
        (0, Err(Failure::Route)),
        (1, Err(Failure::Closed)),
        (1, Ok(true)),
    ]);
    let mut outcomes = UplinkOutcomes {
        first: None,
        failures: usize::MAX - 1,
    };
    assert!(!outcomes.deliver(&mut policy, delivery(first)));
    assert_eq!(outcomes.failures, usize::MAX);
    assert!(!outcomes.deliver(&mut policy, delivery(peer)));
    assert_eq!(outcomes.failures, usize::MAX);
    assert!(outcomes.deliver(&mut policy, delivery(peer)));
    assert_eq!(outcomes.failures, usize::MAX);
    let failure = outcomes.first.unwrap();
    assert_eq!(failure.metadata, first);
    assert_eq!(failure.error, Failure::Route);
    assert_eq!(policy.seen, [first, peer, peer]);
    assert!(policy.expected.is_empty());
}
