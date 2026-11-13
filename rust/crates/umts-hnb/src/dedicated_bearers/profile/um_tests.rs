// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, rel99 as fixture};
use std::num::{NonZeroU8, NonZeroU64};
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rrc::dedicated::{Combination, DchFormatGroup, DchTfcs, Rel99Configuration};
use umts_security::ciphering::{
    BearerCipherConfiguration, BearerCiphers, Configuration, CountAnchor, Position,
};
use umts_types::CnDomain;

const USER_BEARER: u8 = 20;
const STORAGE_BUDGET: usize = 4_000_000;

fn key() -> UeKey {
    UeKey {
        epoch: at(0).epoch,
        slot: 1,
        generation: NonZeroU64::MIN,
    }
}

fn direction(original: &DchTfcs, sizes: &[u16]) -> DchTfcs {
    let mut transports = original.transports().to_vec();
    transports[0].groups = sizes
        .iter()
        .enumerate()
        .map(|(index, octets)| DchFormatGroup {
            rlc_bits: octets * u16::try_from(BITS_PER_BYTE).unwrap(),
            blocks: if index == 0 { vec![0, 1] } else { vec![1] },
        })
        .collect();
    let mut combinations = Vec::new();
    for signal in 0..2 {
        for packet in 0..=sizes.len() {
            combinations.push(Combination {
                formats: vec![packet, signal],
                gain: original.combinations()[0].gain,
            });
        }
    }
    DchTfcs::new(original.direction(), transports, combinations).unwrap()
}

fn plan(uplink: &[u16], downlink: &[u16], um: bool) -> Arc<CompiledRel99> {
    let (ul, dl, mut bearers, physical) = fixture::components(false, true, 10);
    if um {
        bearers
            .iter_mut()
            .find(|rb| rb.id == USER_BEARER)
            .unwrap()
            .rlc = Rel99Rlc::Unacknowledged;
    }
    Arc::new(
        CompiledRel99::new(
            Arc::new(
                Rel99Configuration::new(
                    direction(&ul, uplink),
                    direction(&dl, downlink),
                    bearers,
                    physical,
                )
                .unwrap(),
            ),
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

fn limits() -> UmLimits {
    UmLimits {
        maximum_pdu_bytes: count(512),
        maximum_sdu_bytes: count(512),
        queued_sdus: count(4),
        maximum_buffer_bytes: STORAGE_BUDGET,
    }
}

fn policy() -> UserRlcPolicy {
    UserRlcPolicy::Unacknowledged {
        uplink: limits(),
        downlink: limits(),
    }
}

fn profiles(plan: Arc<CompiledRel99>) -> UserRlcProfiles {
    UserRlcProfiles::new(
        plan,
        &[(BearerId::new(USER_BEARER).unwrap(), policy())],
        count(STORAGE_BUDGET),
    )
    .unwrap()
}

fn bank(plan: Arc<CompiledRel99>, mut users: Vec<BearerEndpoint>) -> DedicatedBearers {
    let mut endpoints: Vec<_> = (1..=3)
        .map(|id| {
            BearerEndpoint::signalling(
                key(),
                BearerId::new(id).unwrap(),
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
        .collect();
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
            maximum_buffer_bytes: STORAGE_BUDGET,
        },
    )
    .unwrap()
}

#[test]
fn asymmetric_variable_sizes_use_one_ul_li_width_and_base_wire_dl_defaults() {
    let plan = plan(&[40, 160], &[40, 120], true);
    let profiles = Arc::new(profiles(plan.clone()));
    let pending = profiles.placeholders(key());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].mode(), CipherMode::Unacknowledged);
    assert!(matches!(pending[0].entity, Entity::AwaitingUser { .. }));
    let start = Start::new(0x12345).unwrap();
    let users = profiles
        .instantiate(key(), [None, Some(start)], at(0))
        .unwrap();
    assert_eq!(users[0].mode(), CipherMode::Unacknowledged);
    assert!(pending[0].storage_bytes >= users[0].storage_bytes);
    let Entity::Unacknowledged { transmit, receive } = &users[0].entity else {
        unreachable!()
    };
    assert_eq!(receive.configuration().li_width, LiWidth::Fifteen);
    assert_eq!(
        receive.configuration().interpretation,
        EInterpretation::Normal
    );
    assert_eq!(receive.limits().maximum_pdu_bytes.get(), 160);
    assert_eq!(transmit.limits().maximum_pdu_bytes.get(), 120);
    assert_eq!(transmit.minimum_pdu_bytes(), UM_MINIMUM_TRANSMIT_OCTETS);
    assert_eq!(
        receive.next_position(),
        Ok(u64::from(start.initial_count()))
    );
    assert_eq!(transmit.next_position(), u64::from(start.initial_count()));
    assert_eq!(transmit.sequence(), 0);
    assert!(
        profiles.storage_bytes() >= users[0].storage_bytes + std::mem::size_of::<BearerEndpoint>()
    );
    let bank = bank(plan, users);
    assert!(!bank.owns_signalling_rlc());
}

#[test]
fn deferred_um_rejects_missing_ps_start_and_keeps_its_prepared_ownership() {
    let profiles = Arc::new(profiles(plan(&[40], &[40], true)));
    let mut pending = profiles.placeholders(key());
    assert_eq!(pending[0].enqueue(b"not admitted"), Err(BearerError::Phase));
    assert!(matches!(
        profiles.instantiate(key(), [Some(Start::new(7).unwrap()), None], at(0)),
        Err(BearerError::Phase)
    ));
    assert!(matches!(pending[0].entity, Entity::AwaitingUser { .. }));
    assert!(pending[0].raw.is_empty() && pending[0].plain.is_empty());
    let start = Start::new(5).unwrap();
    let users = profiles
        .instantiate(key(), [Some(Start::new(7).unwrap()), Some(start)], at(0))
        .unwrap();
    let Entity::Unacknowledged { transmit, receive } = &users[0].entity else {
        unreachable!()
    };
    assert_eq!(transmit.next_position(), u64::from(start.initial_count()));
    assert_eq!(
        receive.next_position(),
        Ok(u64::from(start.initial_count()))
    );
    assert_eq!(receive.configuration().li_width, LiWidth::Seven);
}

#[test]
fn um_publication_uses_original_mode_and_ps_start_count_through_real_bank() {
    let plan = plan(&[40, 160], &[40, 120], true);
    let profiles = profiles(plan.clone());
    let start = Start::new(0x12345).unwrap();
    let users = profiles
        .instantiate(key(), [None, Some(start)], at(0))
        .unwrap();
    let mut bank = bank(plan, users);
    let identity = bank.identity();
    bank.activate(identity).unwrap();
    let mut ciphers = BearerCiphers::new(key(), count(1)).unwrap();
    let anchor = CountAnchor::from_start(
        CipherMode::Unacknowledged,
        Position::Sequence(u64::from(start.initial_count())),
        start,
    )
    .unwrap();
    ciphers
        .register_with(
            BearerCipherConfiguration {
                bearer: USER_BEARER,
                scope: Scope::User(CnDomain::Ps),
                transmit: anchor,
                receive: anchor,
                initial: Configuration::Unprotected,
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let bearer = BearerId::new(USER_BEARER).unwrap();
    let mut peer = UmReceiver::with_initial_position(
        UmConfig {
            li_width: LiWidth::Seven,
            interpretation: EInterpretation::Normal,
        },
        UmLimits {
            maximum_pdu_bytes: count(120),
            ..limits()
        },
        u64::from(start.initial_count()),
    )
    .unwrap();
    for (offset, format) in [1, 2].into_iter().enumerate() {
        let now = at(offset as u64 * 2 * CHIPS_PER_FRAME);
        bank.enqueue_sdu(bearer, b"directional UM payload").unwrap();
        bank.publish_sequenced(
            DownlinkGrant {
                identity,
                bearer,
                transport: TransportId::new(9).unwrap(),
                format,
                start: now,
            },
            now,
            &ciphers,
            |pdu| {
                assert!(pdu.acknowledged_kind.is_none());
                assert_eq!(
                    pdu.cipher,
                    umts_security::ciphering::CipherUse::Data {
                        origin: None,
                        count: start.initial_count() + offset as u32
                    }
                );
                let mac = umts_mac::decode_dedicated(umts_mac::DedicatedMapping::Single, pdu.data)
                    .unwrap();
                let mut bytes = vec![0; mac.payload.len() / BITS_PER_BYTE];
                mac.payload.copy_to(&mut bytes).unwrap();
                let received = peer.receive(&bytes).unwrap();
                assert_eq!(
                    received.position,
                    u64::from(start.initial_count()) + offset as u64
                );
                assert_eq!(
                    received.sdus().collect::<Vec<_>>(),
                    [b"directional UM payload".as_slice()]
                );
                Ok::<_, ()>(())
            },
        )
        .unwrap()
        .unwrap();
    }
}

#[test]
fn small_and_large_ul_pdus_keep_the_configured_largest_pdu_li_width() {
    let profiles = profiles(plan(&[40, 160], &[40], true));
    let start = Start::new(0x12345).unwrap();
    let mut users = profiles
        .instantiate(key(), [None, Some(start)], at(0))
        .unwrap();
    let Entity::Unacknowledged { receive, .. } = &mut users[0].entity else {
        unreachable!()
    };
    let mut ue = UmTransmitter::with_initial_position(
        receive.configuration(),
        receive.limits(),
        u64::from(start.initial_count()),
    )
    .unwrap();
    for (offset, size) in [40, 160].into_iter().enumerate() {
        ue.enqueue(b"same configured UL LI").unwrap();
        let bytes = ue
            .transmit_with(size, |bytes| Ok::<_, ()>(bytes.to_vec()))
            .unwrap()
            .unwrap();
        let received = receive.receive(&bytes).unwrap();
        assert_eq!(
            received.position,
            u64::from(start.initial_count()) + offset as u64
        );
        assert_eq!(
            received.sdus().collect::<Vec<_>>(),
            [b"same configured UL LI".as_slice()]
        );
    }
    assert_eq!(receive.configuration().li_width, LiWidth::Fifteen);
}

#[test]
fn dl_sizes_requiring_unsupported_base_li_geometry_are_rejected_not_reinterpreted() {
    for sizes in [&[126][..], &[40, 126][..]] {
        assert!(matches!(
            UserRlcProfiles::new(
                plan(&[40], sizes, true),
                &[(BearerId::new(USER_BEARER).unwrap(), policy())],
                count(STORAGE_BUDGET),
            ),
            Err(BearerError::UnsupportedBinding)
        ));
    }
}

#[test]
fn deferred_um_validates_mode_directional_limits_and_exact_memory_budget() {
    let id = BearerId::new(USER_BEARER).unwrap();
    assert!(matches!(
        UserRlcProfiles::new(
            plan(&[40], &[40], false),
            &[(id, policy())],
            count(STORAGE_BUDGET)
        ),
        Err(BearerError::UnsupportedBinding)
    ));
    let plan = plan(&[40, 160], &[40, 120], true);
    let valid = profiles(plan.clone());
    assert!(
        UserRlcProfiles::new(
            plan.clone(),
            &[(id, policy())],
            count(valid.storage_bytes())
        )
        .is_ok()
    );
    assert!(matches!(
        UserRlcProfiles::new(
            plan.clone(),
            &[(id, policy())],
            count(valid.storage_bytes() - 1)
        ),
        Err(BearerError::Capacity)
    ));
    for (uplink, downlink) in [
        (
            UmLimits {
                maximum_pdu_bytes: count(159),
                ..limits()
            },
            limits(),
        ),
        (
            limits(),
            UmLimits {
                maximum_pdu_bytes: count(119),
                ..limits()
            },
        ),
        (
            UmLimits {
                maximum_buffer_bytes: 1,
                ..limits()
            },
            limits(),
        ),
        (
            limits(),
            UmLimits {
                maximum_buffer_bytes: 1,
                ..limits()
            },
        ),
        (
            UmLimits {
                queued_sdus: count(257),
                ..limits()
            },
            limits(),
        ),
    ] {
        assert!(
            UserRlcProfiles::new(
                plan.clone(),
                &[(id, UserRlcPolicy::Unacknowledged { uplink, downlink })],
                count(STORAGE_BUDGET),
            )
            .is_err()
        );
    }
}
