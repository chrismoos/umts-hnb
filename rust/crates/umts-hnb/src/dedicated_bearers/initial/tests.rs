// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, rel99 as fixture};
use std::num::{NonZeroU8, NonZeroU64};
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rlc::acknowledged::AmLimits;
use umts_rrc::dedicated::Rel99Rlc;
use umts_security::counters::Start;

const STORAGE_BUDGET: usize = 4_000_000;

fn key() -> UeKey {
    UeKey {
        epoch: at(0).epoch,
        slot: 1,
        generation: NonZeroU64::MIN,
    }
}

fn profiles() -> Arc<UserRlcProfiles> {
    let plan = Arc::new(
        CompiledRel99::new(
            fixture::configuration(true, true, 10),
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    );
    let am = umts_rrc::establishment::SignallingProfile.network_am_configuration(
        count(40),
        count(40),
        0,
        0,
    );
    let policies: Vec<_> = plan
        .configuration()
        .bearers()
        .iter()
        .filter(|rb| rb.domain.is_some())
        .map(|rb| {
            let policy = match rb.rlc {
                Rel99Rlc::Transparent { .. } => UserRlcPolicy::Transparent {
                    errors: ErrorDelivery::Indicate,
                    maximum_sdu_bits: count(208),
                },
                Rel99Rlc::Acknowledged { .. } => UserRlcPolicy::Acknowledged {
                    network: NetworkAmPolicy {
                        maximum_data_attempts: am.maximum_data_attempts,
                        maximum_reset_attempts: am.maximum_reset_attempts,
                        reset_timeout_chips: am.reset_timeout_chips,
                        polling: am.polling,
                        status_prohibit_chips: am.status_prohibit_chips,
                        missing_pdu_indicator: am.missing_pdu_indicator,
                    },
                    limits: AmLimits {
                        maximum_sdu_bytes: count(512),
                        queued_sdus: count(4),
                        maximum_buffer_bytes: 2_000_000,
                    },
                },
                Rel99Rlc::Unacknowledged => unreachable!("combined fixture uses AM packet service"),
            };
            (BearerId::new(rb.id).unwrap(), policy)
        })
        .collect();
    Arc::new(UserRlcProfiles::new(plan, &policies, count(STORAGE_BUDGET)).unwrap())
}

fn instantiate(profiles: &UserRlcProfiles) -> Vec<BearerEndpoint> {
    let start = Start::new(3).unwrap();
    profiles
        .instantiate(key(), [Some(start); DOMAINS], at(0))
        .unwrap()
}

#[test]
fn actual_am_and_tm_owners_keep_each_full_original_reservation() {
    let profiles = profiles();
    let originals = profiles.placeholders(key());
    let mut replacements = instantiate(&profiles);
    assert_eq!(originals.len(), 4);
    assert_eq!(replacements.len(), originals.len());
    let reserved: usize = originals
        .iter()
        .map(|endpoint| endpoint.storage_bytes)
        .sum();
    let allocated: usize = replacements
        .iter()
        .map(|endpoint| endpoint.storage_bytes)
        .sum();
    assert!(reserved > allocated);
    for (original, replacement) in originals.iter().zip(&mut replacements) {
        let actual = replacement.storage_bytes;
        preserve_reservation(original, replacement).unwrap();
        assert!(original.storage_bytes > actual);
        assert_eq!(replacement.storage_bytes, original.storage_bytes);
        assert!(!matches!(replacement.entity, Entity::AwaitingUser { .. }));
    }
    assert_eq!(
        replacements
            .iter()
            .map(|endpoint| endpoint.storage_bytes)
            .sum::<usize>(),
        reserved
    );
    for original in &originals {
        let released = replacements
            .iter()
            .find(|endpoint| endpoint.id == original.id)
            .unwrap();
        assert_eq!(
            reserved - released.storage_bytes,
            originals
                .iter()
                .filter(|endpoint| endpoint.id != original.id)
                .map(|endpoint| endpoint.storage_bytes)
                .sum::<usize>()
        );
    }
}

#[test]
fn repeated_instantiation_never_loses_the_original_reservation_margin() {
    let profiles = profiles();
    let originals = profiles.placeholders(key());
    let reserved: usize = originals
        .iter()
        .map(|endpoint| endpoint.storage_bytes)
        .sum();
    for _ in 0..5 {
        let mut replacements = instantiate(&profiles);
        for (original, replacement) in originals.iter().zip(&mut replacements) {
            preserve_reservation(original, replacement).unwrap();
        }
        let mut remaining = reserved;
        while let Some(released) = replacements.pop() {
            remaining = remaining.checked_sub(released.storage_bytes).unwrap();
            assert_eq!(
                remaining,
                replacements
                    .iter()
                    .map(|endpoint| endpoint.storage_bytes)
                    .sum::<usize>()
            );
        }
        assert_eq!(remaining, 0);
    }
}

#[test]
fn one_endpoint_cannot_borrow_a_sibling_reservation_before_sibling_retirement() {
    let profiles = profiles();
    let originals = profiles.placeholders(key());
    let mut replacements = instantiate(&profiles);
    let actual = replacements[0].storage_bytes;
    replacements[0].storage_bytes = originals[0].storage_bytes + 1;
    assert_eq!(
        preserve_reservation(&originals[0], &mut replacements[0]),
        Err(BearerError::Capacity)
    );
    assert_eq!(
        replacements[0].storage_bytes,
        originals[0].storage_bytes + 1
    );
    replacements[0].storage_bytes = actual;
    preserve_reservation(&originals[0], &mut replacements[0]).unwrap();
    assert_eq!(replacements[0].storage_bytes, originals[0].storage_bytes);
    assert!(matches!(originals[0].entity, Entity::AwaitingUser { .. }));
}

#[test]
fn mismatched_or_already_installed_owner_cannot_inherit_the_placeholder_charge() {
    let profiles = profiles();
    let originals = profiles.placeholders(key());
    let mut replacements = instantiate(&profiles);
    let actual = replacements[0].storage_bytes;
    assert_eq!(
        preserve_reservation(&originals[1], &mut replacements[0]),
        Err(BearerError::Mapping)
    );
    assert_eq!(replacements[0].storage_bytes, actual);
    replacements[0].ue.generation = NonZeroU64::new(2).unwrap();
    assert_eq!(
        preserve_reservation(&originals[0], &mut replacements[0]),
        Err(BearerError::Mapping)
    );
    assert_eq!(replacements[0].storage_bytes, actual);
    replacements[0].ue = key();
    let installed = instantiate(&profiles).remove(0);
    assert_eq!(
        preserve_reservation(&installed, &mut replacements[0]),
        Err(BearerError::Mapping)
    );
    assert_eq!(replacements[0].storage_bytes, actual);
}
