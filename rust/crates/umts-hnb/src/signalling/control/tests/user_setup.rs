// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{BearerId, NetworkAmPolicy, UserRlcPolicy, UserRlcProfiles},
    dedicated_rx::ReceiveIdentity,
    rel99_plan::CompiledRel99,
    test_support::count,
};
use rasn::types::FixedBitString;
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rlc::{acknowledged::AmLimits, acknowledged::ReceiveProvenance};

pub(in crate::signalling::control) fn response(
    transaction: TransactionId,
    start: u32,
) -> ULDCCHMessageType {
    let mut message = complete(transaction);
    let ULDCCHMessageType::radioBearerSetupComplete(body) = &mut message else {
        unreachable!()
    };
    let mut bits = FixedBitString::default();
    for index in 0..20 {
        bits.set(index, start & (1 << (19 - index)) != 0);
    }
    body.start_value = Some(ie::STARTValue(bits));
    message
}
fn profiles(configuration: Arc<Rel99Configuration>) -> UserRlcProfiles {
    let plan = Arc::new(
        CompiledRel99::new(
            configuration,
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    );
    let mut policies = vec![];
    for id in 10..=12 {
        policies.push((
            BearerId::new(id).unwrap(),
            UserRlcPolicy::Transparent {
                errors: umts_rlc::transparent::ErrorDelivery::Indicate,
                maximum_sdu_bits: count(208),
            },
        ));
    }
    if plan.configuration().bearer(20).is_some() {
        let config = SignallingProfile.network_am_configuration(count(40), count(40), 0, 0);
        policies.push((
            BearerId::new(20).unwrap(),
            UserRlcPolicy::Acknowledged {
                network: NetworkAmPolicy {
                    maximum_data_attempts: config.maximum_data_attempts,
                    maximum_reset_attempts: config.maximum_reset_attempts,
                    reset_timeout_chips: config.reset_timeout_chips,
                    polling: config.polling,
                    status_prohibit_chips: config.status_prohibit_chips,
                    missing_pdu_indicator: config.missing_pdu_indicator,
                },
                limits: AmLimits {
                    maximum_sdu_bytes: count(512),
                    queued_sdus: count(4),
                    maximum_buffer_bytes: 2_000_000,
                },
            },
        ));
    }
    UserRlcProfiles::new(plan, &policies, count(4_000_000)).unwrap()
}

#[test]
fn protected_new_path_response_creates_only_exact_canonical_user_rlc_with_its_start() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let (integrity, fresh) = install_integrity(&mut radio, key);
    let retained = radio.retained_signalling(key).unwrap();
    let configuration = configuration_for(&retained, false);
    let mut procedure = RadioBearerSetupProcedure::initial(
        key,
        radio.next_bearer_setup_transaction(key).unwrap(),
        InitialDchSetup::new(configuration.clone(), retained, fixture::rabs(true, false)).unwrap(),
        at(0),
        at(1000),
        NonZeroU64::new(1_000_000).unwrap(),
    )
    .unwrap();
    let profile = profiles(configuration.clone());
    let equivalent_but_foreign = profiles(configuration_for(
        &radio.retained_signalling(key).unwrap(),
        false,
    ));
    let identity = ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::new(2).unwrap(),
    };
    let receipt = radio
        .publish_bearer_setup_on(&mut procedure, identity, at(0))
        .unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    let bytes = integrity::protected(
        &integrity,
        fresh,
        2,
        2,
        response(procedure.transaction(), 0x12345),
    );
    uplink_on(
        &mut radio,
        key,
        &mut sender,
        &bytes,
        100,
        ReceiveProvenance::Path(identity.configuration),
    );
    let mut accepted = None;
    assert_eq!(
        radio.process_one_at_with_control(at(130), security_time(), |received, mut security| {
            assert!(received.accepted_user_setup(&procedure, &security).is_err());
            received.bearer_setup_response(&mut procedure, &mut security)?;
            let value = received.accepted_user_setup(&procedure, &security)?;
            assert_eq!(value.key(), key);
            assert_eq!(value.time().received(), at(100));
            assert_eq!(value.counters().new_bearer_start().unwrap().get(), 0x12345);
            accepted = Some(value);
            Ok(())
        }),
        Some(Ok(key))
    );
    let accepted = accepted.unwrap();
    assert!(matches!(
        profile.instantiate_accepted(&accepted, at(129)),
        Err(crate::dedicated_bearers::BearerError::Time)
    ));
    assert!(matches!(
        equivalent_but_foreign.instantiate_accepted(&accepted, at(130)),
        Err(crate::dedicated_bearers::BearerError::Mapping)
    ));
    let endpoints = profile.instantiate_accepted(&accepted, at(130)).unwrap();
    assert_eq!(endpoints.len(), 3);
    assert_eq!(endpoints[2].id().get(), 12);
}

#[test]
fn unbound_response_cannot_mint_user_setup_even_after_legacy_completion_is_accepted() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let mut procedure = procedure(&radio, key);
    let receipt = radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    uplink(
        &mut radio,
        key,
        &mut sender,
        &clear(response(procedure.transaction(), 3)),
        100,
    );
    assert_eq!(
        radio.process_one_at_with_control(at(130), security_time(), |received, mut security| {
            received.bearer_setup_response(&mut procedure, &mut security)?;
            assert!(matches!(
                received.accepted_user_setup(&procedure, &security),
                Err(SignallingError::Mapping)
            ));
            Ok(())
        }),
        Some(Ok(key))
    );
}
