// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, rel99 as fixture};
use std::num::{NonZeroU8, NonZeroU64};
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rlc::acknowledged::PublicationKind;
use umts_rrc::dedicated::{DchTfcs, NormalDpch, Rel99Configuration};
use umts_security::{
    ciphering::{BearerCipherConfiguration, BearerCiphers, Configuration, CountAnchor, Position},
    counters::CipherMode,
};
use umts_types::CnDomain;

fn key() -> UeKey {
    UeKey {
        epoch: at(0).epoch,
        slot: 1,
        generation: NonZeroU64::MIN,
    }
}
fn rb(id: u8) -> BearerId {
    BearerId::new(id).unwrap()
}
fn compile(
    voice: bool,
    packet: bool,
    edit: impl FnOnce(&mut DchTfcs, &mut DchTfcs, &mut Vec<Rel99Bearer>, &mut NormalDpch),
) -> Arc<CompiledRel99> {
    let (mut ul, mut dl, mut bearers, mut physical) = fixture::components(voice, packet, 10);
    edit(&mut ul, &mut dl, &mut bearers, &mut physical);
    Arc::new(
        CompiledRel99::new(
            Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap()),
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
fn am_policy() -> UserRlcPolicy {
    UserRlcPolicy::Acknowledged {
        network: NetworkAmPolicy {
            maximum_data_attempts: NonZeroU16::new(6).unwrap(),
            maximum_reset_attempts: NonZeroU16::new(8).unwrap(),
            reset_timeout_chips: NonZeroU64::new(123_456).unwrap(),
            polling: Polling {
                every_pdus: NonZeroU16::new(7),
                last_transmission: true,
                last_retransmission: true,
                timeout_chips: NonZeroU64::new(765_432),
                prohibit_chips: None,
            },
            status_prohibit_chips: NonZeroU64::new(50_000),
            missing_pdu_indicator: false,
        },
        limits: AmLimits {
            maximum_sdu_bytes: count(512),
            queued_sdus: count(4),
            maximum_buffer_bytes: 2_000_000,
        },
    }
}
fn policies(voice: bool, packet: bool) -> Vec<(BearerId, UserRlcPolicy)> {
    let mut result = Vec::new();
    if voice {
        for id in 10..=12 {
            result.push((
                rb(id),
                UserRlcPolicy::Transparent {
                    errors: ErrorDelivery::Indicate,
                    maximum_sdu_bits: count(208),
                },
            ));
        }
    }
    if packet {
        result.push((rb(20), am_policy()));
    }
    result
}
fn signalling() -> Vec<BearerEndpoint> {
    (1..=3)
        .map(|id| {
            BearerEndpoint::signalling(
                key(),
                rb(id),
                SignallingRoute {
                    mode: if id == 1 {
                        CipherMode::Unacknowledged
                    } else {
                        CipherMode::Acknowledged
                    },
                    receive_bytes: 18,
                    transmit_minimum: 42,
                    transmit_maximum: 42,
                },
                512,
            )
            .unwrap()
        })
        .collect()
}
fn bank(
    plan: Arc<CompiledRel99>,
    mut users: Vec<BearerEndpoint>,
) -> Result<DedicatedBearers, RejectedBearers> {
    let mut endpoints = signalling();
    endpoints.append(&mut users);
    let anchor = FrameAnchor {
        time: at(0),
        cfn: 0,
    };
    DedicatedBearers::from_compiled(
        ReceiveIdentity {
            ue: key(),
            configuration: NonZeroU64::MIN,
        },
        MappingActivation {
            uplink: anchor,
            uplink_frame: 0,
            downlink: anchor,
            downlink_frame: 0,
        },
        plan,
        endpoints,
        BearerLimits {
            maximum_transport_bits: count(10_000),
            output_slots: count(8),
            maximum_sdu_bytes: count(512),
            maximum_buffer_bytes: 4_000_000,
        },
    )
}

#[test]
fn committed_hsdpa_snapshot_uses_current_plan_and_rejects_foreign_route_or_identity() {
    use crate::{hsdpa_session::RetainedDpchPolicy, radio_resources::RadioResources};
    use umts_rrc::dedicated::ScheduledActivation;
    use umts_types::hsdpa::Category;

    let plan = compile(false, true, |_, _, _, _| ());
    let profiles =
        UserRlcProfiles::new(plan.clone(), &policies(false, true), count(4_000_000)).unwrap();
    let users = profiles
        .instantiate(key(), [None, Some(Start::new(0).unwrap())], at(0))
        .unwrap();
    let mut bank = bank(plan.clone(), users).unwrap();
    let old = bank.identity();
    bank.activate(old).unwrap();
    let config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap();
    let policy = RetainedDpchPolicy::new(
        &config.hspa.downlink,
        config
            .hspa
            .downlink
            .measurement_power_offset_db
            .unwrap_or(6.0),
    )
    .unwrap();
    let source = crate::test_support::source(true);
    let mut resources =
        RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
    let lease = resources
        .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let build = || {
        policy
            .construct(
                plan.configuration().clone(),
                Category::new(6).unwrap(),
                *lease.codes(),
                1,
                ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
            )
            .unwrap()
    };
    let activation = build();
    bank.reserve_hsdpa_route(old, activation.clone()).unwrap();
    assert!(bank.committed_hsdpa(old, &activation).is_ok());
    assert!(matches!(
        bank.committed_hsdpa(old, &build()),
        Err(BearerError::Mapping)
    ));

    let next = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..old
    };
    let current = compile(false, true, |_, _, _, _| ());
    let map = bank.maps[bank.current].as_mut().unwrap();
    map.identity = next;
    map.plan = Some(current.clone());
    let committed = bank.committed_hsdpa(next, &activation).unwrap();
    assert_eq!(committed.identity(), next);
    assert!(Arc::ptr_eq(committed.dedicated(), current.configuration()));
    assert!(Arc::ptr_eq(committed.activation(), &activation));
    assert!(!Arc::ptr_eq(committed.dedicated(), activation.dedicated()));
    assert!(matches!(
        bank.committed_hsdpa(old, &activation),
        Err(BearerError::Phase)
    ));
    bank.maps[bank.current].as_mut().unwrap().phase = MappingPhase::Scheduled { previous: old };
    assert!(matches!(
        bank.committed_hsdpa(next, &activation),
        Err(BearerError::Phase)
    ));
}

#[test]
fn pending_endpoints_reserve_complete_storage_without_allocating_user_rlc() {
    for (voice, packet) in [(true, false), (false, true), (true, true)] {
        let plan = compile(voice, packet, |_, _, _, _| ());
        let profiles = Arc::new(
            UserRlcProfiles::new(plan.clone(), &policies(voice, packet), count(4_000_000)).unwrap(),
        );
        let mut pending = profiles.placeholders(key());
        let initialized = profiles
            .instantiate(key(), [Some(Start::new(3).unwrap()); 2], at(0))
            .unwrap();
        assert_eq!(pending.len(), initialized.len());
        for (endpoint, ready) in pending.iter_mut().zip(&initialized) {
            assert!(matches!(endpoint.entity, Entity::AwaitingUser { .. }));
            assert_eq!(endpoint.mode(), ready.mode());
            assert!(endpoint.storage_bytes >= ready.storage_bytes);
            assert!(
                endpoint.raw.is_empty()
                    && endpoint.plain.is_empty()
                    && endpoint.transmit.is_empty()
                    && endpoint.mac.is_empty()
            );
            assert_eq!(endpoint.enqueue(&[0]), Err(BearerError::Phase));
        }
        let bank = bank(plan, pending).unwrap();
        assert!(!bank.user_bearers_ready());
        assert!(Arc::ptr_eq(
            &bank.awaiting_users().unwrap().unwrap(),
            &profiles
        ));
    }
}

#[test]
fn actual_directional_windows_and_local_network_policy_are_not_copied_from_ue_polling() {
    let plan = compile(false, true, |_, _, bearers, _| {
        let Rel99Rlc::Acknowledged { uplink, downlink } =
            &mut bearers.iter_mut().find(|rb| rb.id == 20).unwrap().rlc
        else {
            unreachable!()
        };
        uplink.transmission_window_size = ie::TransmissionWindowSize::tw64;
        downlink.receiving_window_size = ie::ReceivingWindowSize::rw16;
    });
    let profiles =
        UserRlcProfiles::new(plan.clone(), &policies(false, true), count(4_000_000)).unwrap();
    assert!(Arc::ptr_eq(profiles.plan(), &plan));
    let endpoints = profiles
        .instantiate(key(), [None, Some(Start::new(0x12345).unwrap())], at(0))
        .unwrap();
    let Entity::Acknowledged(entity) = &endpoints[0].entity else {
        unreachable!()
    };
    let config = entity.configuration();
    assert_eq!(config.transmit_window.get(), 16);
    assert_eq!(config.receive_window.get(), 64);
    assert_eq!(config.maximum_data_attempts.get(), 6);
    assert_eq!(config.maximum_reset_attempts.get(), 8);
    assert_eq!(config.reset_timeout_chips.get(), 123_456);
    assert_eq!(config.polling.every_pdus.unwrap().get(), 7);
    assert_eq!(config.status_prohibit_chips.unwrap().get(), 50_000);
    assert!(!config.missing_pdu_indicator);
    assert_eq!(
        (config.initial_tx_hfn, config.initial_rx_hfn),
        (0x12345, 0x12345)
    );
    assert_eq!(
        (
            config.transmit_pdu_bytes.get(),
            config.receive_pdu_bytes.get()
        ),
        (40, 40)
    );
    assert!(bank(plan, endpoints).is_ok());
}

#[test]
fn canonical_profiles_build_cs_ps_and_combined_without_replacing_the_signal_owners() {
    for (voice, packet) in [(true, false), (false, true), (true, true)] {
        let plan = compile(voice, packet, |_, _, _, _| {});
        let profiles =
            UserRlcProfiles::new(plan.clone(), &policies(voice, packet), count(4_000_000)).unwrap();
        let users = profiles
            .instantiate(
                key(),
                [Some(Start::new(3).unwrap()), Some(Start::new(5).unwrap())],
                at(0),
            )
            .unwrap();
        assert_eq!(users.len(), 3 * usize::from(voice) + usize::from(packet));
        let actual_bytes: usize = users
            .iter()
            .map(|endpoint| endpoint.storage_bytes + std::mem::size_of::<BearerEndpoint>())
            .sum();
        assert!(actual_bytes <= profiles.storage_bytes());
        let bank = bank(plan, users).unwrap();
        assert!(!bank.owns_signalling_rlc());
    }
}

#[test]
fn independent_tm_segmentation_directions_match_the_signalled_configuration() {
    for (uplink, downlink) in [(true, false), (false, true), (true, true), (false, false)] {
        let plan = compile(true, false, |_, _, bearers, _| {
            for rb in bearers.iter_mut().filter(|rb| rb.domain.is_some()) {
                rb.rlc = Rel99Rlc::Transparent {
                    uplink_segmentation: uplink,
                    downlink_segmentation: downlink,
                };
            }
        });
        let profiles =
            UserRlcProfiles::new(plan.clone(), &policies(true, false), count(4_000_000)).unwrap();
        let endpoints = profiles.instantiate(key(), [None, None], at(0)).unwrap();
        for endpoint in &endpoints {
            let Entity::Transparent { settings, .. } = endpoint.entity else {
                unreachable!()
            };
            assert_eq!(
                settings.uplink_segmentation == Segmentation::Enabled,
                uplink
            );
            assert_eq!(
                settings.downlink_segmentation == Segmentation::Enabled,
                downlink
            );
        }
        assert!(bank(plan, endpoints).is_ok());
    }
}

#[test]
fn profile_admission_rejects_missing_duplicate_wrong_mode_and_insufficient_storage() {
    let plan = compile(true, true, |_, _, _, _| {});
    let valid = policies(true, true);
    let mut duplicate = valid.clone();
    duplicate[3] = duplicate[0];
    let mut wrong_mode = valid.clone();
    wrong_mode[0].1 = am_policy();
    for invalid in [&valid[..3], duplicate.as_slice(), wrong_mode.as_slice()] {
        assert!(UserRlcProfiles::new(plan.clone(), invalid, count(4_000_000)).is_err());
    }
    let profiles = UserRlcProfiles::new(plan.clone(), &valid, count(4_000_000)).unwrap();
    assert!(matches!(
        UserRlcProfiles::new(plan, &valid, count(profiles.storage_bytes() - 1)),
        Err(BearerError::Capacity)
    ));
    assert!(matches!(
        profiles.instantiate(key(), [Some(Start::new(0).unwrap()), None], at(0)),
        Err(BearerError::Phase)
    ));
}

#[test]
fn canonical_bank_rejects_manual_am_entities_with_windows_different_from_the_wire() {
    let plan = compile(false, true, |_, _, _, _| {});
    let profiles =
        UserRlcProfiles::new(plan.clone(), &policies(false, true), count(4_000_000)).unwrap();
    let mut users = profiles
        .instantiate(key(), [None, Some(Start::new(0).unwrap())], at(0))
        .unwrap();
    let Entity::Acknowledged(entity) = &users[0].entity else {
        unreachable!()
    };
    let mut config = entity.configuration();
    config.receive_window = NonZeroU16::new(64).unwrap();
    let limits = entity.limits();
    users[0] = BearerEndpoint::acknowledged(
        key(),
        rb(20),
        Scope::User(CnDomain::Ps),
        AmEntity::new(config, limits, at(0)).unwrap(),
    )
    .unwrap();
    assert!(
        matches!(bank(plan, users), Err(rejected) if rejected.error == BearerError::UnsupportedBinding)
    );
}

#[test]
fn initialized_packet_endpoint_publishes_the_domain_start_count_through_the_real_bank() {
    let plan = compile(false, true, |_, _, _, _| {});
    let start = Start::new(0x12345).unwrap();
    let profiles =
        UserRlcProfiles::new(plan.clone(), &policies(false, true), count(4_000_000)).unwrap();
    let users = profiles
        .instantiate(key(), [None, Some(start)], at(0))
        .unwrap();
    let mut bank = bank(plan, users).unwrap();
    let identity = bank.identity();
    bank.activate(identity).unwrap();
    let mut ciphers = BearerCiphers::new(key(), count(1)).unwrap();
    let anchor = CountAnchor::from_start(
        CipherMode::Acknowledged,
        Position::Sequence(u64::from(start.initial_count())),
        start,
    )
    .unwrap();
    ciphers
        .register_with(
            BearerCipherConfiguration {
                bearer: 20,
                scope: Scope::User(CnDomain::Ps),
                transmit: anchor,
                receive: anchor,
                initial: Configuration::Unprotected,
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    bank.enqueue_sdu(rb(20), b"canonical packet bearer")
        .unwrap();
    bank.publish_sequenced(DownlinkGrant { identity, bearer: rb(20), transport: TransportId::new(9).unwrap(), format: 1, start: at(0) }, at(0), &ciphers, |pdu| {
        assert!(matches!(pdu.acknowledged_kind, Some(PublicationKind::Data { sequence: 0, count, retransmission: false }) if count == start.initial_count()));
        assert_eq!(pdu.cipher, umts_security::ciphering::CipherUse::Data { origin: None, count: start.initial_count() });
        Ok::<_, ()>(())
    }).unwrap().unwrap();
}
