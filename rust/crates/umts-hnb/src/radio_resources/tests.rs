// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{self, count, rel99 as fixture};
use std::num::{NonZeroU8, NonZeroU64};
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_types::{RadioEpoch, UeKey, channel_codes::OvsfCode};

const MAXIMUM_CODED_BITS: usize = 200_000;
const MAXIMUM_MAPPING_ENTRIES: usize = 300_000;
const FIRST_UPLINK_CODE: u32 = 17;

fn key(slot: u32) -> UeKey {
    UeKey {
        epoch: test_support::at(0).epoch,
        slot,
        generation: NonZeroU64::MIN,
    }
}

fn limits() -> TransportPlanLimits {
    TransportPlanLimits {
        maximum_coded_bits: count(MAXIMUM_CODED_BITS),
        maximum_mapping_entries: count(MAXIMUM_MAPPING_ENTRIES),
        decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
    }
}

fn prepare(
    resources: &mut RadioResources,
    key: UeKey,
    template: &Rel99Configuration,
) -> Result<AllocatedRadio, Error> {
    resources.prepare(key, template, count(MAXIMUM_CODED_BITS), limits())
}

fn codes(allocation: &AllocatedRadio) -> Vec<OvsfCode> {
    let physical = allocation.plan().configuration().physical();
    physical
        .downlink_codes
        .iter()
        .map(|index| OvsfCode::new(physical.downlink_sf, *index).unwrap())
        .collect()
}

fn sf128_template() -> Rel99Configuration {
    let template = fixture::configuration(false, false, 10);
    let mut physical = template.physical().clone();
    physical.downlink_sf = 128;
    physical.downlink_pilot_bits = 8;
    physical.downlink_codes = vec![0];
    template.with_physical(physical).unwrap()
}

#[test]
fn sf128_users_borrow_the_lowest_hsdpa_code_until_the_last_one_leaves() {
    let cell = test_support::plan(true);
    let mut resources =
        RadioResources::new(key(0).epoch, cell.information(), count(3), 17..=19).unwrap();
    let common = resources.occupied;
    let hsdpa = resources
        .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let hsupa = resources.reserve_hsupa().unwrap();
    let loan = OvsfCode::new(16, u16::from(hsdpa.codes().first_data())).unwrap();
    let template = sf128_template();
    let first = prepare(&mut resources, key(0), &template).unwrap();
    assert!(codes(&first).iter().all(|code| !code.overlaps(loan)));
    assert_eq!(lent_data_codes(), 0);
    let primary = resources.occupied;
    let second = prepare(&mut resources, key(1), &template).unwrap();
    let third = prepare(&mut resources, key(2), &template).unwrap();
    for borrower in [&second, &third] {
        assert_eq!(
            borrower
                .plan()
                .configuration()
                .physical()
                .secondary_scrambling,
            None
        );
        assert!(codes(borrower).iter().all(|code| code.overlaps(loan)));
    }
    assert_ne!(codes(&second), codes(&third));
    assert_eq!(lent_data_codes(), 1 << loan.index());
    assert!(
        resources
            .secondary_occupied
            .iter()
            .flatten()
            .all(|used| !*used)
    );
    resources.cancel(second).unwrap();
    assert_eq!(lent_data_codes(), 1 << loan.index());
    resources.cancel(third).unwrap();
    assert_eq!(lent_data_codes(), 0);
    assert_eq!(resources.occupied, primary);
    resources.cancel(first).unwrap();
    resources.release_hsupa(hsupa).unwrap();
    resources.release_hsdpa(hsdpa).unwrap();
    assert_eq!(resources.occupied, common);
}

#[test]
fn a_reconfiguration_that_needs_room_borrows_and_retirement_returns_the_code() {
    let cell = test_support::plan(true);
    let mut resources =
        RadioResources::new(key(0).epoch, cell.information(), count(2), 17..=18).unwrap();
    let hsdpa = resources
        .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let _hsupa = resources.reserve_hsupa().unwrap();
    let loan = OvsfCode::new(16, u16::from(hsdpa.codes().first_data())).unwrap();
    let _voice = prepare(&mut resources, key(0), &sf128_template()).unwrap();
    let template = fixture::configuration(false, false, 10);
    let mut physical = template.physical().clone();
    physical.downlink_sf = 256;
    physical.downlink_pilot_bits = 4;
    physical.downlink_codes = vec![0];
    let signalling = prepare(
        &mut resources,
        key(1),
        &template.with_physical(physical).unwrap(),
    )
    .unwrap();
    assert!(codes(&signalling).iter().all(|code| !code.overlaps(loan)));
    assert_eq!(lent_data_codes(), 0);
    let change = resources
        .prepare_reconfiguration(
            signalling.lease(),
            signalling.plan(),
            &sf128_template(),
            count(MAXIMUM_CODED_BITS),
            limits(),
        )
        .unwrap();
    let staged = change.plan().configuration().physical();
    assert_eq!(staged.secondary_scrambling, None);
    assert!(
        staged
            .downlink_codes
            .iter()
            .all(|index| OvsfCode::new(128, *index).unwrap().overlaps(loan))
    );
    assert_eq!(lent_data_codes(), 1 << loan.index());
    resources.cancel_reconfiguration(change).unwrap();
    assert_eq!(lent_data_codes(), 0);
}

#[test]
fn primary_space_fits_sf256_beside_sf128_without_borrowing() {
    let cell = test_support::plan(true);
    let mut resources =
        RadioResources::new(key(0).epoch, cell.information(), count(2), 17..=18).unwrap();
    let common = resources.occupied;
    let hsdpa = resources
        .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let hsupa = resources.reserve_hsupa().unwrap();
    let template = fixture::configuration(false, false, 10);
    let mut physical = template.physical().clone();
    physical.downlink_sf = 128;
    physical.downlink_pilot_bits = 8;
    physical.downlink_codes = vec![0];
    let first = prepare(
        &mut resources,
        key(0),
        &template.with_physical(physical.clone()).unwrap(),
    )
    .unwrap();
    resources.check_primary_downlink_space(&physical).unwrap();
    assert_eq!(lent_data_codes(), 0);
    resources.check_downlink_space(&physical).unwrap();
    physical.downlink_sf = 256;
    physical.downlink_pilot_bits = 4;
    resources.check_primary_downlink_space(&physical).unwrap();
    let second = prepare(
        &mut resources,
        key(1),
        &template.with_physical(physical).unwrap(),
    )
    .unwrap();
    assert_eq!(
        second
            .plan()
            .configuration()
            .physical()
            .secondary_scrambling,
        None
    );
    assert_eq!(lent_data_codes(), 0);
    resources.cancel(second).unwrap();
    resources.cancel(first).unwrap();
    resources.release_hsupa(hsupa).unwrap();
    resources.release_hsdpa(hsdpa).unwrap();
    assert_eq!(resources.occupied, common);
}

#[test]
fn hsupa_pool_coexists_with_hsdpa_and_user_release_is_owner_checked() {
    let cell = test_support::plan(false);
    let make = || RadioResources::new(key(0).epoch, cell.information(), count(8), 17..=30).unwrap();
    let mut resources = make();
    let mut other = make();
    let common = resources.occupied;
    let hsdpa = resources
        .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let pool = resources.reserve_hsupa().unwrap();
    for code in hsdpa
        .codes()
        .data_codes()
        .chain(hsdpa.codes().control_codes())
    {
        assert!(!code.overlaps(pool.codes().absolute()));
        assert!(!code.overlaps(pool.codes().indicator()));
    }
    let radio = prepare(
        &mut resources,
        key(0),
        &fixture::configuration(false, true, 10),
    )
    .unwrap()
    .into_lease();
    let before = resources.next;
    assert!(
        resources
            .reserve_hsupa_user(&pool, &radio, Some(6))
            .is_err()
    );
    assert_eq!(resources.next, before);
    let user = resources
        .reserve_hsupa_user(&pool, &radio, Some(0))
        .unwrap();
    assert_ne!(
        user.controls().acknowledgement_signature(),
        user.controls().relative().unwrap().signature()
    );
    assert!(resources.reserve_hsupa_user(&pool, &radio, None).is_err());
    let pool = resources.release_hsupa(pool).unwrap_err().lease;
    let user = other.release_hsupa_user(user).unwrap_err().lease;
    resources.release_hsupa_user(user).unwrap();
    resources.release(radio).unwrap();
    resources.release_hsupa(pool).unwrap();
    resources.release_hsdpa(hsdpa).unwrap();
    assert_eq!(resources.occupied, common);
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn hsupa_pool_identity_cannot_outlive_or_cross_its_reservation() {
    let cell = test_support::plan(false);
    let make = || RadioResources::new(key(0).epoch, cell.information(), count(8), 17..=30).unwrap();
    let mut resources = make();
    let mut other = make();
    let pool = resources.reserve_hsupa().unwrap();
    let identity = pool.identity();
    let radio = prepare(
        &mut resources,
        key(0),
        &fixture::configuration(false, true, 10),
    )
    .unwrap()
    .into_lease();
    let other_pool = other.reserve_hsupa().unwrap();
    assert!(
        resources
            .reserve_hsupa_user_in_pool(other_pool.identity(), &radio, None)
            .is_err()
    );
    let user = resources
        .reserve_hsupa_user_in_pool(identity, &radio, None)
        .unwrap();
    resources.release_hsupa_user(user).unwrap();
    resources.release_hsupa(pool).unwrap();
    let replacement = resources.reserve_hsupa().unwrap();
    assert!(
        resources
            .reserve_hsupa_user_in_pool(identity, &radio, None)
            .is_err()
    );
    let user = resources
        .reserve_hsupa_user_in_pool(replacement.identity(), &radio, None)
        .unwrap();
    resources.release_hsupa_user(user).unwrap();
    resources.release_hsupa(replacement).unwrap();
    resources.release(radio).unwrap();
    other.release_hsupa(other_pool).unwrap();
    assert_eq!(resources.outstanding(), 0);
    assert_eq!(other.outstanding(), 0);
}

#[test]
fn hsdpa_pool_and_two_dedicated_users_share_one_nonoverlapping_tree() {
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key(0).epoch, cell.information(), count(8), 17..=30).unwrap();
    let common = resources.occupied;
    let pool = resources
        .reserve_hsdpa(NonZeroU8::new(10).unwrap(), NonZeroU8::new(2).unwrap())
        .unwrap();
    assert_eq!(pool.codes().data_codes().count(), 10);
    assert_eq!(pool.codes().control_codes().count(), 2);
    let template = fixture::configuration(false, true, 10);
    let first = prepare(&mut resources, key(0), &template).unwrap();
    let second = prepare(&mut resources, key(1), &template).unwrap();
    assert_eq!(resources.outstanding(), 3);
    for dedicated in codes(&first).into_iter().chain(codes(&second)) {
        for shared in pool
            .codes()
            .data_codes()
            .chain(pool.codes().control_codes())
        {
            assert!(!dedicated.overlaps(shared));
        }
    }
    let occupied = resources.occupied;
    assert!(
        resources
            .reserve_hsdpa(NonZeroU8::MIN, NonZeroU8::MIN)
            .is_err()
    );
    assert_eq!(resources.occupied, occupied);
    resources.cancel(first).unwrap();
    resources.cancel(second).unwrap();
    resources.release_hsdpa(pool).unwrap();
    assert_eq!(resources.occupied, common);
}

#[test]
fn compact_common_channels_leave_control_space_with_fifteen_data_codes() {
    let cell = test_support::plan(true);
    let mut resources =
        RadioResources::new(key(0).epoch, cell.information(), count(8), 17..=30).unwrap();
    let common = resources.occupied;
    let downlink = resources
        .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let uplink = resources.reserve_hsupa().unwrap();
    assert_eq!(downlink.codes().data_codes().count(), 15);
    let controls = uplink.codes();
    for code in downlink
        .codes()
        .data_codes()
        .chain(downlink.codes().control_codes())
    {
        assert!(!code.overlaps(controls.absolute()));
        assert!(!code.overlaps(controls.indicator()));
    }
    assert_eq!(resources.occupied.iter().filter(|used| !**used).count(), 6);
    resources.release_hsupa(uplink).unwrap();
    resources.release_hsdpa(downlink).unwrap();
    assert_eq!(resources.occupied, common);
}

#[test]
fn hsdpa_pool_failure_is_transactional_and_wrong_owner_returns_lease() {
    let cell = test_support::plan(false);
    let make = || RadioResources::new(key(0).epoch, cell.information(), count(8), 17..=30).unwrap();
    let mut first = make();
    let mut other = make();
    let original = first.occupied;
    assert!(
        first
            .reserve_hsdpa(NonZeroU8::new(16).unwrap(), NonZeroU8::MIN)
            .is_err()
    );
    assert_eq!(first.occupied, original);
    assert_eq!(first.next, Some(NonZeroU64::MIN));
    let pool = first
        .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let occupied = first.occupied;
    let rejected = other.release_hsdpa(pool).unwrap_err();
    assert_eq!(rejected.error, Error::Identity);
    assert_eq!(first.occupied, occupied);
    first.release_hsdpa(rejected.lease).unwrap();
    assert_eq!(first.occupied, original);
}

#[test]
fn reported_ue_limits_gate_allocation_without_claiming_resources_on_failure() {
    use crate::signalling::tests::{COMPLETE, controller, establish, peer, send_capability_report};
    use umts_rrc::{capabilities::rel99::Limit, establishment::TransactionId};
    let source = test_support::source(false);
    let mut signalling = controller(&source);
    let ue = establish(&mut signalling, &source, 0);
    let cell = test_support::plan(false);
    let mut resources = RadioResources::new(
        ue.epoch,
        cell.information(),
        count(8),
        FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE,
    )
    .unwrap();
    let template = fixture::configuration(false, true, 10);
    assert!(matches!(
        resources.prepare_for_ue(
            ue,
            &template,
            &signalling,
            count(MAXIMUM_CODED_BITS),
            limits()
        ),
        Err(Error::MissingCapabilities)
    ));
    assert_eq!(resources.outstanding(), 0);
    signalling
        .request_capabilities(ue, test_support::at(0), TransactionId::new(2).unwrap())
        .unwrap();
    let mut peer = peer(&source);
    peer.enqueue(&COMPLETE).unwrap();
    peer.publish_next(test_support::at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    send_capability_report(&mut signalling, ue, &mut peer);
    assert_eq!(signalling.process_one(), Some(Ok(ue)));
    assert_eq!(
        resources
            .prepare_for_ue(
                ue,
                &template,
                &signalling,
                count(MAXIMUM_CODED_BITS),
                limits()
            )
            .unwrap_err(),
        Error::Capability(umts_rrc::capabilities::rel99::Error::Exceeded {
            direction: Some(umts_rrc::dedicated::DchDirection::Uplink),
            limit: Limit::TransportBlocks,
            required: 5,
            supported: 4,
        })
    );
    assert_eq!(resources.outstanding(), 0);
    let template = fixture::configuration(true, false, 10);
    let mut physical = template.physical().clone();
    physical.downlink_codes.push(4);
    let unsupported = template.with_physical(physical).unwrap();
    assert!(matches!(
        resources.prepare_for_ue(
            ue,
            &unsupported,
            &signalling,
            count(MAXIMUM_CODED_BITS),
            limits()
        ),
        Err(Error::Capability(
            umts_rrc::capabilities::rel99::Error::Exceeded {
                limit: Limit::DownlinkCodes,
                ..
            }
        ))
    ));
    assert_eq!(resources.outstanding(), 0);
    let allocation = resources
        .prepare_for_ue(
            ue,
            &template,
            &signalling,
            count(MAXIMUM_CODED_BITS),
            limits(),
        )
        .unwrap();
    assert_eq!(
        allocation
            .plan()
            .configuration()
            .physical()
            .uplink_scrambling,
        FIRST_UPLINK_CODE
    );
    assert_eq!(resources.outstanding(), 1);
    resources.cancel(allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn mixed_bearer_templates_get_independent_codes_and_actual_common_reservations() {
    for compact in [false, true] {
        let cell = test_support::plan(compact);
        let mut resources = RadioResources::new(
            key(0).epoch,
            cell.information(),
            count(3),
            FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE + 2,
        )
        .unwrap();
        let mut allocations = Vec::new();
        for (slot, (voice, packet)) in [(true, false), (false, true), (true, true)]
            .into_iter()
            .enumerate()
        {
            let template = fixture::configuration(voice, packet, 10);
            let template = if voice && packet {
                let mut physical = template.physical().clone();
                physical.downlink_codes.push(4);
                std::sync::Arc::new(template.with_physical(physical).unwrap())
            } else {
                template
            };
            let allocation = prepare(&mut resources, key(slot as u32), &template).unwrap();
            let configuration = allocation.plan().configuration();
            assert_eq!(allocation.key(), key(slot as u32));
            assert_eq!(configuration.bearers(), template.bearers());
            assert_eq!(
                configuration.physical().primary_scrambling,
                cell.information().primary_code().index()
            );
            assert_eq!(
                configuration.physical().downlink_sf,
                template.physical().downlink_sf
            );
            assert_eq!(
                configuration.physical().downlink_codes.len(),
                template.physical().downlink_codes.len()
            );
            let assigned = codes(&allocation);
            for (index, code) in assigned.iter().copied().enumerate() {
                assert!(assigned[..index].iter().all(|other| !code.overlaps(*other)));
                for reserved in cell.information().downlink_code_reservations() {
                    assert!(!code.overlaps(reserved));
                }
                for previous in &allocations {
                    for claimed in codes(previous) {
                        assert!(!code.overlaps(claimed));
                    }
                    assert_ne!(
                        configuration.physical().uplink_scrambling,
                        previous.plan().configuration().physical().uplink_scrambling
                    );
                }
            }
            allocations.push(allocation);
        }
        assert_eq!(resources.outstanding(), 3);
        for allocation in allocations {
            resources.cancel(allocation).unwrap();
        }
        assert_eq!(resources.outstanding(), 0);
    }
}

#[test]
fn ue_capacity_and_foreign_cancel_preserve_the_original_allocation_authority() {
    let cell = test_support::plan(false);
    let mut resources = RadioResources::new(
        key(0).epoch,
        cell.information(),
        count(1),
        FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE + 1,
    )
    .unwrap();
    let mut foreign = RadioResources::new(
        key(0).epoch,
        cell.information(),
        count(1),
        FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE + 1,
    )
    .unwrap();
    let template = fixture::configuration(true, true, 10);
    let allocation = prepare(&mut resources, key(0), &template).unwrap();
    let original_codes = codes(&allocation);
    assert!(prepare(&mut resources, key(1), &template).is_err());
    let rejected = foreign.cancel(allocation).unwrap_err();
    assert_eq!(resources.outstanding(), 1);
    assert_eq!(foreign.outstanding(), 0);
    assert_eq!(rejected.allocation.key(), key(0));
    assert_eq!(codes(&rejected.allocation), original_codes);
    resources.cancel(rejected.allocation).unwrap();
    let replacement = prepare(&mut resources, key(0), &template).unwrap();
    assert_eq!(codes(&replacement), original_codes);
    let rejected = foreign.release(replacement.into_lease()).unwrap_err();
    assert_eq!(resources.outstanding(), 1);
    assert_eq!(foreign.outstanding(), 0);
    assert_eq!(rejected.lease.key(), key(0));
    resources.release(rejected.lease).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn full_ue_identity_does_not_allow_overlapping_generations_or_foreign_epochs() {
    let cell = test_support::plan(false);
    let mut resources = RadioResources::new(
        key(0).epoch,
        cell.information(),
        count(3),
        FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE + 2,
    )
    .unwrap();
    let template = fixture::configuration(false, true, 10);
    let allocation = prepare(&mut resources, key(0), &template).unwrap();
    assert!(prepare(&mut resources, key(0), &template).is_err());
    let next_generation = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key(0)
    };
    assert!(prepare(&mut resources, next_generation, &template).is_err());
    let foreign_epoch = UeKey {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        slot: 1,
        ..key(0)
    };
    assert!(prepare(&mut resources, foreign_epoch, &template).is_err());
    assert_eq!(resources.outstanding(), 1);
    resources.cancel(allocation).unwrap();
    let replacement = prepare(&mut resources, next_generation, &template).unwrap();
    assert_eq!(replacement.key(), next_generation);
    resources.cancel(replacement).unwrap();
}

#[test]
fn slot_reservation_tracks_exact_lease_until_cancel_or_release() {
    let cell = test_support::plan(false);
    let mut resources = RadioResources::new(
        key(0).epoch,
        cell.information(),
        count(2),
        FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE + 1,
    )
    .unwrap();
    let mut foreign = RadioResources::new(
        key(0).epoch,
        cell.information(),
        count(2),
        FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE + 1,
    )
    .unwrap();
    let template = fixture::configuration(false, true, 10);

    assert!(!resources.slot_reserved(0));
    assert!(!resources.slot_reserved(1));
    assert!(!resources.slot_reserved(2));
    assert!(!resources.slot_reserved(usize::MAX));

    let first = prepare(&mut resources, key(0), &template).unwrap();
    let peer = prepare(&mut resources, key(1), &template).unwrap();
    assert!(resources.slot_reserved(0));
    assert!(resources.slot_reserved(1));
    assert!(
        prepare(
            &mut resources,
            UeKey {
                generation: NonZeroU64::new(2).unwrap(),
                ..key(0)
            },
            &template
        )
        .is_err()
    );
    assert!(resources.slot_reserved(0));
    assert!(resources.slot_reserved(1));

    let rejected = foreign.cancel(first).unwrap_err();
    assert_eq!(rejected.error, Error::Identity);
    assert!(resources.slot_reserved(0));
    assert!(resources.slot_reserved(1));
    resources.cancel(rejected.allocation).unwrap();
    assert!(!resources.slot_reserved(0));
    assert!(resources.slot_reserved(1));

    let replacement = prepare(
        &mut resources,
        UeKey {
            generation: NonZeroU64::new(2).unwrap(),
            ..key(0)
        },
        &template,
    )
    .unwrap();
    assert!(resources.slot_reserved(0));
    resources.cancel(replacement).unwrap();
    assert!(!resources.slot_reserved(0));

    let peer_lease = peer.into_lease();
    let rejected = foreign.release(peer_lease).unwrap_err();
    assert_eq!(rejected.error, Error::Identity);
    assert!(resources.slot_reserved(1));
    resources.release(rejected.lease).unwrap();
    assert!(!resources.slot_reserved(1));
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn uplink_scrambling_exhaustion_is_independent_and_release_makes_it_reusable() {
    let cell = test_support::plan(false);
    let mut resources = RadioResources::new(
        key(0).epoch,
        cell.information(),
        count(2),
        FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE,
    )
    .unwrap();
    let template = fixture::configuration(true, false, 10);
    let allocation = prepare(&mut resources, key(0), &template).unwrap();
    assert_eq!(
        allocation
            .plan()
            .configuration()
            .physical()
            .uplink_scrambling,
        FIRST_UPLINK_CODE
    );
    assert!(prepare(&mut resources, key(1), &template).is_err());
    assert_eq!(resources.outstanding(), 1);
    resources.cancel(allocation).unwrap();
    let replacement = prepare(&mut resources, key(1), &template).unwrap();
    assert_eq!(
        replacement
            .plan()
            .configuration()
            .physical()
            .uplink_scrambling,
        FIRST_UPLINK_CODE
    );
    resources.cancel(replacement).unwrap();
}

#[test]
fn code_tree_exhaustion_does_not_overbook_common_codes_or_leak_released_claims() {
    let cell = test_support::plan(false);
    let template = fixture::configuration(false, true, 10);
    let sf = template.physical().downlink_sf;
    let total_codes = u32::from(sf) * (u32::from(SECONDARY_TREES) + 1);
    let mut resources = RadioResources::new(
        key(0).epoch,
        cell.information(),
        count(total_codes as usize + 1),
        FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE + total_codes,
    )
    .unwrap();
    let mut allocations = Vec::new();
    let mut exhausted = false;
    for slot in 0..=total_codes {
        match prepare(&mut resources, key(slot), &template) {
            Ok(allocation) => allocations.push(allocation),
            Err(_) => {
                exhausted = true;
                break;
            }
        }
    }
    assert!(exhausted);
    assert!(!allocations.is_empty());
    assert!(allocations.len() > usize::from(sf));
    assert!(allocations.len() < total_codes as usize);
    assert_eq!(resources.outstanding(), allocations.len());
    let released = allocations.pop().unwrap();
    let released_key = released.key();
    let original_codes = codes(&released);
    let original_tree = released
        .plan()
        .configuration()
        .physical()
        .secondary_scrambling;
    resources.cancel(released).unwrap();
    let replacement = prepare(&mut resources, released_key, &template).unwrap();
    assert_eq!(codes(&replacement), original_codes);
    assert_eq!(
        replacement
            .plan()
            .configuration()
            .physical()
            .secondary_scrambling,
        original_tree
    );
    allocations.push(replacement);
    for allocation in allocations {
        resources.cancel(allocation).unwrap();
    }
    assert_eq!(resources.outstanding(), 0);
    assert!(
        resources
            .secondary_occupied
            .iter()
            .flatten()
            .all(|used| !*used)
    );
}

#[test]
fn failed_plan_compilation_leaves_ue_code_and_scrambling_reservations_unchanged() {
    let cell = test_support::plan(false);
    let make = || {
        RadioResources::new(
            key(0).epoch,
            cell.information(),
            count(2),
            FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE + 1,
        )
        .unwrap()
    };
    let mut resources = make();
    let mut reference = make();
    let template = fixture::configuration(true, true, 10);
    assert!(
        resources
            .prepare(key(0), &template, count(1), limits())
            .is_err()
    );
    assert_eq!(resources.outstanding(), 0);
    let after_failure = prepare(&mut resources, key(0), &template).unwrap();
    let baseline = prepare(&mut reference, key(0), &template).unwrap();
    assert_eq!(codes(&after_failure), codes(&baseline));
    assert_eq!(
        after_failure
            .plan()
            .configuration()
            .physical()
            .uplink_scrambling,
        baseline.plan().configuration().physical().uplink_scrambling
    );
    resources.cancel(after_failure).unwrap();
    reference.cancel(baseline).unwrap();
    assert_eq!(resources.outstanding(), 0);
}
