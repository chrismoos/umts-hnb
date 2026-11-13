// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, rel99 as fixture};
use std::num::NonZeroU8;
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rrc::dedicated::{DchTfcs, NormalDpch, Rel99Configuration};
use umts_types::CnDomain;

const STORAGE_BYTES: usize = 4_000_000;

fn plan(
    voice: bool,
    packet: bool,
    edit: impl FnOnce(&mut DchTfcs, &mut DchTfcs, &mut Vec<Rel99Bearer>, &mut NormalDpch),
) -> Arc<CompiledRel99> {
    let (mut ul, mut dl, mut rbs, mut physical) = fixture::components(voice, packet, 10);
    edit(&mut ul, &mut dl, &mut rbs, &mut physical);
    Arc::new(
        CompiledRel99::new(
            Arc::new(Rel99Configuration::new(ul, dl, rbs, physical).unwrap()),
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    )
}

fn rb(id: u8) -> BearerId {
    BearerId::new(id).unwrap()
}

fn circuit() -> Vec<(BearerId, UserRlcPolicy)> {
    (10..=12)
        .map(|id| {
            (
                rb(id),
                UserRlcPolicy::Transparent {
                    errors: ErrorDelivery::Indicate,
                    maximum_sdu_bits: count(208),
                },
            )
        })
        .collect()
}

fn packet() -> Vec<(BearerId, UserRlcPolicy)> {
    vec![(
        rb(20),
        UserRlcPolicy::Acknowledged {
            network: NetworkAmPolicy {
                maximum_data_attempts: NonZeroU16::new(10).unwrap(),
                maximum_reset_attempts: NonZeroU16::new(4).unwrap(),
                reset_timeout_chips: NonZeroU64::new(1_536_000).unwrap(),
                polling: Polling {
                    every_pdus: Some(NonZeroU16::MIN),
                    last_transmission: true,
                    last_retransmission: true,
                    timeout_chips: NonZeroU64::new(768_000),
                    prohibit_chips: None,
                },
                status_prohibit_chips: NonZeroU64::new(384_000),
                missing_pdu_indicator: true,
            },
            limits: AmLimits {
                maximum_sdu_bytes: count(512),
                queued_sdus: count(4),
                maximum_buffer_bytes: STORAGE_BYTES,
            },
        },
    )]
}

fn key() -> UeKey {
    UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    }
}

#[test]
fn addition_profiles_only_new_cs_endpoints_and_charges_only_their_reservations() {
    let previous = plan(false, true, |_, _, _, _| {});
    let successor = plan(true, true, |_, _, _, _| {});
    let previous_address = Arc::as_ptr(&previous);
    let added = UserRlcProfiles::additional(
        successor.clone(),
        &previous,
        &circuit(),
        count(STORAGE_BYTES),
    )
    .unwrap();
    assert!(Arc::ptr_eq(added.plan(), &successor));
    assert_eq!(Arc::as_ptr(&previous), previous_address);
    assert_eq!(previous.configuration().bearers().len(), 4);
    let placeholders = Arc::new(added).placeholders(key());
    assert_eq!(
        placeholders
            .iter()
            .map(|endpoint| endpoint.id.get())
            .collect::<Vec<_>>(),
        [10, 11, 12]
    );
    let added = UserRlcProfiles::additional(
        successor.clone(),
        &previous,
        &circuit(),
        count(STORAGE_BYTES),
    )
    .unwrap();
    let mut policies = circuit();
    policies.extend(packet());
    let full = UserRlcProfiles::new(successor, &policies, count(STORAGE_BYTES)).unwrap();
    let old = UserRlcProfiles::new(previous, &packet(), count(STORAGE_BYTES)).unwrap();
    assert_eq!(
        added.storage_bytes() + old.storage_bytes(),
        full.storage_bytes()
    );
    let start = Start::new(3).unwrap();
    let endpoints = added.instantiate(key(), [Some(start); 2], at(0)).unwrap();
    assert_eq!(
        endpoints
            .iter()
            .map(|endpoint| endpoint.id.get())
            .collect::<Vec<_>>(),
        [10, 11, 12]
    );
}

#[test]
fn existing_cs_can_add_only_the_new_packet_entity() {
    let previous = plan(true, false, |_, _, _, _| {});
    let successor = plan(true, true, |_, _, _, _| {});
    let added =
        UserRlcProfiles::additional(successor, &previous, &packet(), count(STORAGE_BYTES)).unwrap();
    let endpoints = added
        .instantiate(key(), [Some(Start::new(7).unwrap()); 2], at(0))
        .unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].id.get(), 20);
    assert_eq!(endpoints[0].scope, Scope::User(CnDomain::Ps));
}

#[test]
fn added_um_uses_the_same_directional_validation_without_recreating_cs() {
    let previous = plan(true, false, |_, _, _, _| {});
    let successor = plan(true, true, |_, _, rbs, _| {
        rbs.iter_mut().find(|rb| rb.id == 20).unwrap().rlc = Rel99Rlc::Unacknowledged;
    });
    let limits = UmLimits {
        maximum_pdu_bytes: count(512),
        maximum_sdu_bytes: count(512),
        queued_sdus: count(4),
        maximum_buffer_bytes: STORAGE_BYTES,
    };
    let added = UserRlcProfiles::additional(
        successor,
        &previous,
        &[(
            rb(20),
            UserRlcPolicy::Unacknowledged {
                uplink: limits,
                downlink: limits,
            },
        )],
        count(STORAGE_BYTES),
    )
    .unwrap();
    let endpoints = added
        .instantiate(key(), [Some(Start::new(2).unwrap()); 2], at(0))
        .unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].id.get(), 20);
    assert_eq!(
        endpoints[0].mode(),
        umts_security::counters::CipherMode::Unacknowledged
    );
}

#[test]
fn old_duplicate_missing_and_unchanged_policy_sets_reject_without_mutating_plans() {
    let previous = plan(false, true, |_, _, _, _| {});
    let successor = plan(true, true, |_, _, _, _| {});
    for policies in [
        vec![],
        circuit()[..2].to_vec(),
        vec![circuit()[0], circuit()[0], circuit()[2]],
        vec![packet()[0], circuit()[1], circuit()[2]],
    ] {
        assert!(matches!(
            UserRlcProfiles::additional(
                successor.clone(),
                &previous,
                &policies,
                count(STORAGE_BYTES)
            ),
            Err(BearerError::Mapping)
        ));
    }
    assert!(matches!(
        UserRlcProfiles::additional(previous.clone(), &previous, &[], count(STORAGE_BYTES)),
        Err(BearerError::Mapping)
    ));
    assert!(matches!(
        UserRlcProfiles::new(successor.clone(), &circuit(), count(STORAGE_BYTES)),
        Err(BearerError::Mapping)
    ));
    assert_eq!(successor.configuration().bearers().len(), 7);
}

#[test]
fn retained_mapping_and_directional_transport_changes_are_not_treated_as_additions() {
    let previous = plan(false, true, |_, _, _, _| {});
    let removed = plan(true, false, |_, _, _, _| {});
    assert!(matches!(
        UserRlcProfiles::additional(removed, &previous, &circuit(), count(STORAGE_BYTES)),
        Err(BearerError::UnsupportedBinding)
    ));
    let remapped = plan(true, true, |_, _, bearers, _| {
        bearers
            .iter_mut()
            .find(|rb| rb.id == 2)
            .unwrap()
            .mapping
            .uplink[0]
            .priority = 2;
    });
    assert!(matches!(
        UserRlcProfiles::additional(remapped, &previous, &circuit(), count(STORAGE_BYTES)),
        Err(BearerError::UnsupportedBinding)
    ));
    for direction in [DchDirection::Uplink, DchDirection::Downlink] {
        let changed = plan(true, true, |ul, dl, _, _| {
            let selected = match direction {
                DchDirection::Uplink => ul,
                DchDirection::Downlink => dl,
            };
            let mut transports = selected.transports().to_vec();
            let id = if direction == DchDirection::Uplink {
                4
            } else {
                9
            };
            transports
                .iter_mut()
                .find(|transport| transport.id == id)
                .unwrap()
                .groups[0]
                .rlc_bits = 328;
            *selected =
                DchTfcs::new(direction, transports, selected.combinations().to_vec()).unwrap();
        });
        assert!(matches!(
            UserRlcProfiles::additional(changed, &previous, &circuit(), count(STORAGE_BYTES)),
            Err(BearerError::UnsupportedBinding)
        ));
    }
}

#[test]
fn new_profiles_keep_mode_and_memory_validation() {
    let previous = plan(false, true, |_, _, _, _| {});
    let successor = plan(true, true, |_, _, _, _| {});
    let added = UserRlcProfiles::additional(
        successor.clone(),
        &previous,
        &circuit(),
        count(STORAGE_BYTES),
    )
    .unwrap();
    assert!(matches!(
        UserRlcProfiles::additional(
            successor.clone(),
            &previous,
            &circuit(),
            count(added.storage_bytes() - 1)
        ),
        Err(BearerError::Capacity)
    ));
    let mut mismatched = circuit();
    mismatched[0].1 = packet()[0].1;
    assert!(matches!(
        UserRlcProfiles::additional(successor, &previous, &mismatched, count(STORAGE_BYTES)),
        Err(BearerError::UnsupportedBinding)
    ));
}
