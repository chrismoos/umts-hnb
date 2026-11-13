// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{
        BearerId, BearerLimits, DedicatedBearers, MappingActivation, NetworkAmPolicy,
        UserRlcPolicy, UserRlcProfiles,
    },
    dedicated_rx::FrameAnchor,
    rel99_plan::CompiledRel99,
    test_support::count,
};
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rlc::acknowledged::AmLimits;

fn compiled(configuration: Arc<Rel99Configuration>) -> Arc<CompiledRel99> {
    Arc::new(
        CompiledRel99::new(
            configuration,
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

fn timing(frame: u64) -> MappingActivation {
    let anchor = FrameAnchor {
        time: at(frame * CHIPS_PER_FRAME),
        cfn: frame as u8,
    };
    MappingActivation {
        uplink: anchor,
        downlink: anchor,
        uplink_frame: frame,
        downlink_frame: frame,
    }
}

fn staged(c: &mut Connected) -> (DedicatedBearers, usize) {
    let previous = compiled(c.previous.clone());
    let next = compiled(c.configuration.clone());
    let profiles = super::super::frames::user_profiles(previous.clone());
    let mut endpoints = c.radio.signalling_endpoints(c.key).unwrap();
    endpoints.extend(profiles.placeholders(c.key));
    let mut bank = DedicatedBearers::from_compiled(
        c.old,
        timing(0),
        previous.clone(),
        endpoints,
        BearerLimits {
            maximum_transport_bits: count(10_000),
            output_slots: count(8),
            maximum_sdu_bytes: c.radio.limits.maximum_sdu_bytes,
            maximum_buffer_bytes: 8_000_000,
        },
    )
    .unwrap();
    bank.activate(c.old).unwrap();
    c.radio
        .with_bearer_access(c.key, |ciphers, _, _| {
            bank.install_initial_users(
                &c.initial,
                [None, None],
                ciphers,
                c.initial.time().serviced(),
            )
            .unwrap();
        })
        .unwrap();
    let config = SignallingProfile.network_am_configuration(count(40), count(40), 0, 0);
    let policy = UserRlcPolicy::Acknowledged {
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
            maximum_buffer_bytes: 1_000_000,
        },
    };
    let added = Arc::new(
        UserRlcProfiles::additional(
            next.clone(),
            &previous,
            &[(BearerId::new(20).unwrap(), policy)],
            count(2_000_000),
        )
        .unwrap(),
    );
    let original_bytes = bank.storage_bytes();
    let prepared = bank
        .prepare_addition(c.new, timing(4), next, added.placeholders(c.key))
        .unwrap();
    bank.stage_addition(prepared).unwrap();
    (bank, original_bytes)
}

#[test]
fn authenticated_packet_addition_installs_only_new_users_without_resetting_voice() {
    let mut c = connected();
    let (mut bank, _) = staged(&mut c);
    bank.commit_mapping(c.new).unwrap();
    let voice_cipher = c
        .radio
        .with_bearer_access(c.key, |ciphers, _, _| {
            ciphers
                .selected(
                    c.key,
                    10,
                    umts_types::Direction::Uplink,
                    umts_security::ciphering::Position::Frame(8),
                )
                .unwrap()
        })
        .unwrap();
    assert!(!bank.user_bearers_ready());
    let bytes = bank.storage_bytes();
    publish(&mut c);
    let boundary = c.procedure.activation().unwrap().time().index;
    let path = ReceiveProvenance::Path(c.new.configuration);
    receive(&mut c, boundary, path);
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 10),
            security_time(),
            |received, mut security| {
                received
                    .additional_setup_response_with(&mut c.procedure, &mut security, |_, _| {
                        Err::<(), _>(SignallingError::Capacity)
                    })
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Capacity))
    );
    assert!(!bank.user_bearers_ready());
    assert_eq!(bank.storage_bytes(), bytes);
    c.radio
        .with_bearer_access(c.key, |ciphers, _, _| {
            assert_eq!(
                ciphers
                    .selected(
                        c.key,
                        10,
                        umts_types::Direction::Uplink,
                        umts_security::ciphering::Position::Frame(8)
                    )
                    .unwrap(),
                voice_cipher
            );
        })
        .unwrap();
    assert_eq!(c.procedure.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 20),
            security_time(),
            |received, mut security| {
                received
                    .additional_setup_response_with(
                        &mut c.procedure,
                        &mut security,
                        |accepted, ciphers| {
                            assert_eq!(
                                bank.install_additional_users(
                                    accepted,
                                    [None, None],
                                    ciphers,
                                    at(0)
                                ),
                                Err(crate::dedicated_bearers::BearerError::Phase)
                            );
                            assert!(!bank.user_bearers_ready());
                            bank.install_additional_users(
                                accepted,
                                [None, None],
                                ciphers,
                                received.time().serviced(),
                            )
                            .map_err(SignallingError::UserBearer)?;
                            assert_eq!(
                                ciphers
                                    .selected(
                                        c.key,
                                        10,
                                        umts_types::Direction::Uplink,
                                        umts_security::ciphering::Position::Frame(8)
                                    )
                                    .unwrap(),
                                voice_cipher
                            );
                            Ok(())
                        },
                    )
                    .map(|_| ())
            }
        ),
        Some(Ok(c.key))
    );
    assert!(bank.user_bearers_ready());
    assert_eq!(bank.storage_bytes(), bytes);
    bank.enqueue_sdu(BearerId::new(20).unwrap(), &[0x12; 100])
        .unwrap();
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
}

#[test]
fn cancellation_restores_original_reservation_and_rejects_preinitialized_additions() {
    let mut c = connected();
    let (mut bank, original_bytes) = staged(&mut c);
    assert!(bank.storage_bytes() > original_bytes);
    assert!(!bank.user_bearers_ready());
    let prepared = bank.cancel_addition(c.old, c.new).unwrap();
    assert_eq!(bank.storage_bytes(), original_bytes);
    assert!(bank.user_bearers_ready());
    assert!(!bank.contains_identity(c.new));
    assert_eq!(bank.identity(), c.old);
    bank.stage_addition(prepared).unwrap();
    drop(bank.cancel_addition(c.old, c.new).unwrap());
    assert_eq!(bank.storage_bytes(), original_bytes);
    let next = compiled(c.configuration.clone());
    let start = umts_security::counters::Start::new(3).unwrap();
    let endpoints = super::super::frames::user_profiles(next.clone())
        .instantiate(c.key, [Some(start); 2], at(0))
        .unwrap()
        .into_iter()
        .filter(|endpoint| endpoint.id().get() == 20)
        .collect();
    let rejected = bank
        .prepare_addition(c.new, timing(4), next, endpoints)
        .unwrap_err();
    assert_eq!(rejected.error, crate::dedicated_bearers::BearerError::Phase);
    assert_eq!(rejected.endpoints.len(), 1);
    assert_eq!(bank.storage_bytes(), original_bytes);
    assert!(bank.user_bearers_ready());
}
