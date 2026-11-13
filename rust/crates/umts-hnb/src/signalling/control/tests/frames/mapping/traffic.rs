// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{EmptyTransparentTraffic, FrameBuildError, TrafficFrame},
    rel99_admission::{FrameOpportunity, FramePolicy, TrafficFramePolicy},
};

#[test]
fn scheduled_traffic_selects_exact_successor_mapping_before_predecessor_retirement() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let plan = remapped(&f);
    let next = stage(&mut f, 4, plan);
    f.bank.commit_mapping(next).unwrap();
    let boundary = at(CHIPS_PER_FRAME * 4);
    let selected = f
        .bank
        .select_traffic_on(
            TrafficFrame {
                identity: next,
                start: boundary,
            },
            at(0),
            &mut f.radio,
            &EmptyTransparentTraffic,
            count(1024),
        )
        .unwrap();
    assert_eq!(f.bank.identity(), old);
    let mut policy = TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024));
    assert_eq!(
        policy.select(
            FrameOpportunity {
                identity: next,
                start: boundary,
                dedicated_signalling: true,
                user_bearers_ready: true,
                idle: selected,
            },
            at(0),
            &mut f.bank,
            &mut f.radio
        ),
        Ok(selected)
    );
    assert_eq!(f.bank.identity(), old);
    assert_eq!(
        f.bank.select_traffic(
            at(0),
            at(0),
            &mut f.radio,
            &EmptyTransparentTraffic,
            count(1024)
        ),
        f.bank.select_traffic_on(
            TrafficFrame {
                identity: old,
                start: at(0)
            },
            at(0),
            &mut f.radio,
            &EmptyTransparentTraffic,
            count(1024)
        )
    );
}

#[test]
fn mapped_traffic_rejects_unknown_generation_and_wrong_frame_without_advancing_owner() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let plan = remapped(&f);
    let next = stage(&mut f, 4, plan);
    let foreign = ReceiveIdentity {
        configuration: NonZeroU64::new(3).unwrap(),
        ..next
    };
    assert_eq!(
        f.bank.select_traffic_on(
            TrafficFrame {
                identity: foreign,
                start: at(CHIPS_PER_FRAME * 4)
            },
            at(0),
            &mut f.radio,
            &EmptyTransparentTraffic,
            count(1024)
        ),
        Err(FrameBuildError::Bearer(BearerError::Owner))
    );
    for start in [at(0), at(CHIPS_PER_FRAME * 4 + 1)] {
        assert_eq!(
            f.bank.select_traffic_on(
                TrafficFrame {
                    identity: next,
                    start
                },
                at(0),
                &mut f.radio,
                &EmptyTransparentTraffic,
                count(1024)
            ),
            Err(FrameBuildError::Bearer(BearerError::Time))
        );
    }
    assert_eq!(f.bank.identity(), old);
    assert_eq!(f.bank.mapping_phase(old), Ok(MappingPhase::Active));
    assert_eq!(
        f.bank.mapping_phase(next),
        Ok(MappingPhase::Staged { previous: old })
    );
}
