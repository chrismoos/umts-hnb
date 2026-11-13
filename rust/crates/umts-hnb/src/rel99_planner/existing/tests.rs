// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    rel99_planner::tests::{circuit, cs_request, limits, packet, policy, ps_request},
    test_support::{at, count, rel99 as fixture},
};
use std::num::{NonZeroU8, NonZeroU64};
use umts_rrc::establishment::TransactionId;
use umts_rrc_codec::decode_downlink_dedicated;
use umts_rrc_schema::class_definitions::DLDCCHMessageType;
use umts_security::counters::Start;

const PACKET_RATE: u32 = 12_800;

fn check_transition(cs_first: bool, unacknowledged: bool) {
    let cs = cs_request();
    let ps = ps_request(PACKET_RATE);
    let circuit = circuit(&cs);
    let packet = packet(&ps, unacknowledged);
    let physical = [fixture::physical()];
    let policy = policy(&physical);
    let cs = Rab {
        domain: CnDomain::Cs,
        request: &cs,
        service: Service::Circuit(&circuit),
    };
    let ps = Rab {
        domain: CnDomain::Ps,
        request: &ps,
        service: Service::Packet(&packet),
    };
    let (first, second) = if cs_first { (cs, ps) } else { (ps, cs) };
    let initial = plan_initial(&fixture::retained(), &[first], &limits(), &policy).unwrap();
    let old = initial.compiled().clone();
    let next = plan_existing(old.clone(), initial.rabs(), &[second], &limits(), &policy).unwrap();
    assert!(Arc::ptr_eq(next.previous(), &old));
    assert!(Arc::ptr_eq(initial.profiles().plan(), &old));
    assert_eq!(next.retained_rabs().len(), 1);
    assert_eq!(next.rabs().len(), 1);
    for bearer in old.configuration().bearers() {
        assert_eq!(
            next.compiled().configuration().bearer(bearer.id),
            Some(bearer)
        );
    }
    assert_eq!(
        next.compiled().configuration().physical(),
        old.configuration().physical()
    );
    for (before, after) in [
        (
            old.configuration().uplink(),
            next.compiled().configuration().uplink(),
        ),
        (
            old.configuration().downlink(),
            next.compiled().configuration().downlink(),
        ),
    ] {
        for transport in before.transports() {
            let target = after.transport(transport.id).unwrap();
            assert_eq!(target.interval, transport.interval);
            assert_eq!(target.coding, transport.coding);
            assert_eq!(target.crc, transport.crc);
            assert_eq!(
                target.rate_matching_attribute,
                transport.rate_matching_attribute
            );
            assert_eq!(
                target.formats().collect::<Vec<_>>(),
                transport.formats().collect::<Vec<_>>()
            );
        }
    }
    let key = UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    };
    let new_endpoints = next
        .profiles()
        .instantiate(key, [Some(Start::new(3).unwrap()); 2], at(0))
        .unwrap();
    let new_ids = next
        .rabs()
        .iter()
        .flat_map(|rab| rab.bearers.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(new_endpoints.len(), new_ids.len());
    assert!(
        new_endpoints
            .iter()
            .all(|endpoint| new_ids.contains(&endpoint.id().get()))
    );
    assert!(
        new_ids
            .iter()
            .all(|id| old.configuration().bearer(*id).is_none())
    );
    let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(8).unwrap()).unwrap();
    let setup = ExistingDchSetup::new(
        old.configuration().clone(),
        next.compiled().configuration().clone(),
        next.new_rabs.clone(),
        activation,
    )
    .unwrap();
    let message = setup.message(TransactionId::new(1).unwrap()).unwrap();
    assert!(matches!(
        decode_downlink_dedicated(&message.bytes, message.bit_length)
            .unwrap()
            .value
            .message,
        DLDCCHMessageType::radioBearerSetup(_)
    ));
}

#[test]
fn adding_circuit_to_am_packet_preserves_old_bearers_and_profiles() {
    check_transition(false, false);
}

#[test]
fn adding_circuit_to_um_packet_preserves_old_bearers_and_profiles() {
    check_transition(false, true);
}

#[test]
fn adding_am_packet_to_circuit_preserves_old_bearers_and_profiles() {
    check_transition(true, false);
}

#[test]
fn adding_um_packet_to_circuit_preserves_old_bearers_and_profiles() {
    check_transition(true, true);
}

#[test]
fn active_inventory_must_account_for_every_prior_user_bearer_once() {
    let request = ps_request(PACKET_RATE);
    let packet = packet(&request, false);
    let rab = || Rab {
        domain: CnDomain::Ps,
        request: &request,
        service: Service::Packet(&packet),
    };
    let physical = [fixture::physical()];
    let policy = policy(&physical);
    let initial = plan_initial(&fixture::retained(), &[rab()], &limits(), &policy).unwrap();
    assert!(matches!(
        plan_existing(
            initial.compiled().clone(),
            &[],
            &[rab()],
            &limits(),
            &policy
        ),
        Err(Error::Identity)
    ));
    let mut duplicate = initial.rabs().to_vec();
    duplicate.push(duplicate[0].clone());
    assert!(matches!(
        plan_existing(
            initial.compiled().clone(),
            &duplicate,
            &[rab()],
            &limits(),
            &policy
        ),
        Err(Error::Identity)
    ));
    let mut wrong = initial.rabs().to_vec();
    wrong[0].domain = CnDomain::Cs;
    assert!(matches!(
        plan_existing(
            initial.compiled().clone(),
            &wrong,
            &[rab()],
            &limits(),
            &policy
        ),
        Err(Error::Identity)
    ));
    assert!(matches!(
        plan_existing(
            initial.compiled().clone(),
            initial.rabs(),
            &[rab()],
            &limits(),
            &policy
        ),
        Err(Error::Unsupported)
    ));
}

#[test]
fn empty_requests_and_exhausted_combination_budgets_leave_the_active_plan_unchanged() {
    let cs = cs_request();
    let ps = ps_request(PACKET_RATE);
    let circuit = circuit(&cs);
    let packet = packet(&ps, false);
    let physical = [fixture::physical()];
    let mut policy = policy(&physical);
    let initial = plan_initial(
        &fixture::retained(),
        &[Rab {
            domain: CnDomain::Ps,
            request: &ps,
            service: Service::Packet(&packet),
        }],
        &limits(),
        &policy,
    )
    .unwrap();
    assert!(matches!(
        plan_existing(
            initial.compiled().clone(),
            initial.rabs(),
            &[],
            &limits(),
            &policy
        ),
        Err(Error::Capacity)
    ));
    policy.maximum_combinations = count(1);
    assert!(
        plan_existing(
            initial.compiled().clone(),
            initial.rabs(),
            &[Rab {
                domain: CnDomain::Cs,
                request: &cs,
                service: Service::Circuit(&circuit)
            }],
            &limits(),
            &policy
        )
        .is_err()
    );
    assert_eq!(
        initial
            .compiled()
            .configuration()
            .bearers()
            .iter()
            .filter(|rb| rb.domain.is_some())
            .count(),
        1
    );
    assert_eq!(
        initial
            .profiles()
            .instantiate(
                UeKey {
                    epoch: at(0).epoch,
                    slot: 0,
                    generation: NonZeroU64::MIN
                },
                [Some(Start::new(3).unwrap()); 2],
                at(0)
            )
            .unwrap()
            .len(),
        1
    );
}
