// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use rasn::types::FixedBitString;
use umts_iuh::{
    ContextId,
    hnbap::{
        self, AccessMode, Capabilities as RegistrationCapabilities, CsgCapability, HnbParameters,
        Imsi, RegistrationCause, Release, UeIdentity,
    },
    rua,
    session::{Event, Session, SessionLimits, UplinkNas},
    wire::Limits,
};
use umts_rrc::{
    integrity::UplinkIntegrity,
    security::{IntegrityActivation, SignallingSet},
};
use umts_rrc_schema::{
    class_definitions::{ULDCCHMessage, ULDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};
use umts_security::counters::Start;
use umts_security::negotiation::IntegrityAlgorithm;
use umts_types::{Direction, RadioEpoch};

use crate::test_support::capabilities as fixtures;

const FRESH: u32 = 0x12345678;

#[test]
fn snow_negotiation_installs_matching_owned_contexts() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let mut command = command(2, ranap::KeyStatus::New);
    command.integrity.algorithms = vec![ranap::Algorithm(1)];
    let mut security = capabilities().security().clone();
    security.ciphering_algorithm_cap.set(13, true);
    security.integrity_protection_algorithm_cap.set(13, true);
    let capabilities = Capabilities::new(security, None).unwrap();
    let request = core.request(Domain::Cs, &command, 4);
    owner
        .receive(time(base, 4), ControlTime(4), request, &command)
        .unwrap();
    let Preparation::Prepared(origin) = owner
        .prepare(Domain::Cs, time(base, 5), &capabilities, plan(false))
        .unwrap()
    else {
        panic!("SNOW capability was not selected");
    };
    let keys = owner.keys(origin).unwrap();
    assert_eq!(keys.integrity.kind(), IntegrityAlgorithm::Uia2);
    let mut bytes = [0x5a; 4];
    keys.cipher
        .apply(origin, 123, 3, Direction::Uplink, &mut bytes, 32)
        .unwrap();
    let mut expected = [0x5a; 4];
    umts_security::algorithms::Uea2Key::new(&Key::new([0x22; 16]))
        .apply(123, 3, Direction::Uplink, &mut expected, 32)
        .unwrap();
    assert_eq!(bytes, expected);
}

#[test]
fn snow_only_policy_filters_core_order_without_changing_ue_capabilities() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::with_algorithms(
        key(),
        FRESH,
        Support {
            uia2: true,
            uea2: true,
            ..Support::default()
        },
    );
    let mut command = command(2, ranap::KeyStatus::New);
    command.integrity.algorithms = vec![ranap::Algorithm(0), ranap::Algorithm(1)];
    command.encryption.as_mut().unwrap().algorithms =
        vec![ranap::Algorithm(1), ranap::Algorithm(2)];
    let mut security = capabilities().security().clone();
    security.ciphering_algorithm_cap.set(13, true);
    security.integrity_protection_algorithm_cap.set(13, true);
    let capabilities = Capabilities::new(security, None).unwrap();
    let request = core.request(Domain::Cs, &command, 4);
    owner
        .receive(time(base, 4), ControlTime(4), request, &command)
        .unwrap();
    let result = owner
        .prepare_using(Domain::Cs, time(base, 5), &capabilities, |_, selection| {
            assert_eq!(
                selection.algorithms,
                Algorithms {
                    integrity: IntegrityAlgorithm::Uia2,
                    cipher: CipherAlternative::Uea2,
                }
            );
            Ok::<_, ()>(plan(false))
        })
        .unwrap();
    assert!(matches!(result, Preparation::Prepared(_)));
}

#[test]
fn failed_plan_factory_keeps_the_request_and_generation_retryable() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let command = command(1, ranap::KeyStatus::New);
    let request = core.request(Domain::Cs, &command, 4);
    owner
        .receive(time(base, 4), ControlTime(4), request, &command)
        .unwrap();
    let mut attempted = None;
    assert_eq!(
        owner.prepare_using(
            Domain::Cs,
            time(base, 5),
            &capabilities(),
            |origin, selection| {
                attempted = Some(origin);
                assert_eq!(selection.chosen_encryption(), Some(CipherAlternative::Uea1));
                Err::<RadioPlan, _>("transport busy")
            }
        ),
        Err(Error::Downstream("transport busy"))
    );
    assert_eq!(owner.phase(Domain::Cs), Phase::AwaitingPreparation);
    assert_eq!(owner.pending_origin(Domain::Cs), None);
    let result = owner
        .prepare_using(Domain::Cs, time(base, 6), &capabilities(), |origin, _| {
            assert_eq!(Some(origin), attempted);
            Ok::<_, ()>(plan(false))
        })
        .unwrap();
    assert_eq!(result, Preparation::Prepared(attempted.unwrap()));
}

#[test]
fn only_never_submitted_commands_can_be_discarded_without_restarting_the_request() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let first = prepare(&mut owner, &mut core, time(base, 4));
    assert_eq!(owner.transaction(Domain::Cs), Some(plan(false).transaction));
    assert!(!owner.can_prepare(Domain::Ps));
    assert_eq!(owner.discard_prepared(Domain::Ps, first), Err(Error::Phase));
    owner.discard_prepared(Domain::Cs, first).unwrap();
    assert_eq!(owner.phase(Domain::Cs), Phase::AwaitingPreparation);
    assert_eq!(owner.pending_origin(Domain::Cs), None);
    assert!(owner.keys(first).is_none());
    let Preparation::Prepared(second) = owner
        .prepare(Domain::Cs, time(base, 100), &capabilities(), plan(false))
        .unwrap()
    else {
        panic!("request must remain retryable");
    };
    assert_ne!(first, second);
    assert_eq!(owner.discard_prepared(Domain::Cs, first), Err(Error::Phase));
    let integrity = owner.keys(second).unwrap().integrity().clone();
    let mut downlink = DownlinkIntegrity::new(integrity, 2, FRESH, 0).unwrap();
    owner
        .publish_with(Domain::Cs, time(base, 101), &mut downlink, |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        owner.discard_prepared(Domain::Cs, second),
        Err(Error::Phase)
    );
    assert_eq!(
        owner.timeout_action(Domain::Cs, time(base, 5004)),
        Ok(Some(second))
    );
}

#[test]
fn discarded_unsent_command_keeps_the_original_core_token_and_deadline() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let origin = prepare(&mut owner, &mut core, time(base, 4));
    owner.discard_prepared(Domain::Cs, origin).unwrap();
    assert_eq!(owner.timeout_action(Domain::Cs, time(base, 5003)), Ok(None));
    assert_eq!(owner.phase(Domain::Cs), Phase::AwaitingPreparation);
    assert_eq!(owner.timeout_action(Domain::Cs, time(base, 5004)), Ok(None));
    assert_eq!(owner.phase(Domain::Cs), Phase::Reporting);
    core.report(&mut owner, Domain::Cs, time(base, 5004));
    assert_eq!(owner.phase(Domain::Cs), Phase::Idle);
}

#[test]
fn cipher_plan_uses_the_actual_command_and_never_fabricates_a_reused_start() {
    let base = Instant::now();
    let start = Start::new(0x23456).unwrap();
    for status in [ranap::KeyStatus::New, ranap::KeyStatus::Old] {
        let mut core = Core::new();
        let mut owner = ConnectionSecurity::new(key(), FRESH);
        let command = command(1, status);
        let request = core.request(Domain::Cs, &command, 4);
        owner
            .receive(time(base, 4), ControlTime(4), request, &command)
            .unwrap();
        owner
            .prepare(Domain::Cs, time(base, 5), &capabilities(), plan(false))
            .unwrap();
        if status == ranap::KeyStatus::Old {
            assert!(matches!(
                owner.cipher_plan(Domain::Cs, None),
                Err(Error::MissingStart)
            ));
        } else {
            assert_eq!(
                owner
                    .cipher_plan(Domain::Cs, None)
                    .unwrap()
                    .unwrap()
                    .counters,
                CounterInitialization::NewKeys
            );
        }
        let transport = owner.cipher_plan(Domain::Cs, Some(start)).unwrap().unwrap();
        assert_eq!(
            transport.context.origin(),
            owner.pending_origin(Domain::Cs).unwrap()
        );
        assert_eq!(transport.downlink, plan(false).cipher.unwrap());
        assert_eq!(transport.dpch_cfn, None);
        assert_eq!(
            transport.counters,
            if status == ranap::KeyStatus::New {
                CounterInitialization::NewKeys
            } else {
                CounterInitialization::ReusedKeys(start)
            }
        );
        assert_eq!(owner.phase(Domain::Cs), Phase::Prepared);
        owner.retire();
        assert!(matches!(
            owner.cipher_plan(Domain::Cs, Some(start)),
            Err(Error::Phase)
        ));
    }
}

fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 3,
        generation: NonZeroU64::MIN,
    }
}
fn time(base: Instant, millis: u64) -> SecurityTime {
    SecurityTime {
        control: ControlTime(millis),
        monotonic: base + Duration::from_millis(millis),
    }
}
fn capabilities() -> Capabilities {
    Capabilities::new(
        fixtures::information()
            .ue_radio_access_capability
            .unwrap()
            .security_capability,
        None,
    )
    .unwrap()
}
fn command(cipher: u8, status: ranap::KeyStatus) -> ranap::SecurityCommand {
    ranap::SecurityCommand {
        integrity: ranap::Protection {
            algorithms: vec![ranap::Algorithm(0)],
            key: ranap::Key::new([0x11; 16]),
        },
        encryption: Some(ranap::Protection {
            algorithms: vec![ranap::Algorithm(cipher)],
            key: ranap::Key::new([0x22; 16]),
        }),
        key_status: status,
    }
}
struct Core {
    session: Session,
    association: AssociationId,
    context: ContextId,
}
impl Core {
    fn new() -> Self {
        let association = AssociationId(NonZeroU64::MIN);
        let context = ContextId::new(42).unwrap();
        let identity = UeIdentity::Imsi(Imsi::from_digits("001010000000099").unwrap());
        let mut session = Session::new(
            key().epoch,
            association,
            HnbParameters {
                identity: b"Rust security owner test".to_vec(),
                plmn: [0, 0xf1, 0x10],
                lac: 23,
                rac: 7,
                sac: 42,
                cell: 1,
                access: AccessMode::Open,
                csg: None,
            },
            SessionLimits {
                maximum_rab_requests_per_domain: std::num::NonZeroUsize::new(4).unwrap(),
                maximum_rab_release_requests_per_domain: std::num::NonZeroUsize::new(2).unwrap(),
                maximum_ues: std::num::NonZeroUsize::new(8).unwrap(),
                request_timeout_ms: NonZeroU64::new(1000).unwrap(),
                codec: Limits::default(),
            },
            ControlTime(0),
        )
        .unwrap();
        session
            .register_hnb_with(ControlTime(0), |_| Ok::<_, ()>(()))
            .unwrap();
        let hnb = hnbap::Message::HnbAccepted {
            rnc: 1,
            mux_port: None,
        };
        session
            .receive_hnbap_with(
                association,
                ControlTime(1),
                &hnbap::Message::decode(&hnb.encode(Limits::default()).unwrap(), Limits::default())
                    .unwrap(),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        session
            .register_ue_with(
                key(),
                identity.clone(),
                RegistrationCapabilities {
                    release: Release::R99,
                    csg: CsgCapability::NotCapable,
                },
                RegistrationCause::Normal,
                ControlTime(2),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        let accepted = hnbap::Message::UeAccepted {
            identity,
            context,
            membership: None,
        };
        session
            .receive_hnbap_with(
                association,
                ControlTime(2),
                &hnbap::Message::decode(
                    &accepted.encode(Limits::default()).unwrap(),
                    Limits::default(),
                )
                .unwrap(),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        for domain in [Domain::Cs, Domain::Ps] {
            session
                .publish_nas_with(
                    UplinkNas {
                        key: key(),
                        domain,
                        nas: &[5, 0x24],
                        selector: None,
                        cause: rua::EstablishmentCause::Normal,
                    },
                    ControlTime(3),
                    |_| Ok::<_, ()>(()),
                )
                .unwrap();
        }
        Self {
            session,
            association,
            context,
        }
    }
    fn request(
        &mut self,
        domain: Domain,
        command: &ranap::SecurityCommand,
        at: u64,
    ) -> SecurityRequest {
        let message = rua::Message::Direct {
            domain,
            context: self.context,
            ranap: ranap::encode_security(command, Limits::default()).unwrap(),
        };
        self.session
            .receive_rua_with(
                self.association,
                ControlTime(at),
                &rua::Message::decode(
                    &message.encode(Limits::default()).unwrap(),
                    Limits::default(),
                )
                .unwrap(),
                |event| {
                    let Event::SecurityCommand { request, .. } = event else {
                        panic!("expected scoped security command");
                    };
                    Ok::<_, ()>(*request)
                },
            )
            .unwrap()
    }
    fn report(&mut self, owner: &mut ConnectionSecurity, domain: Domain, now: SecurityTime) {
        owner
            .publish_report_with(domain, now, |token, response| {
                self.session.publish_security_with(
                    token,
                    response,
                    now.control,
                    |_| Ok::<_, ()>(()),
                )
            })
            .unwrap()
            .unwrap();
    }
}
fn plan(modify: bool) -> RadioPlan {
    let layout = BearerLayout::new(SignallingSet::Base, &[]).unwrap();
    RadioPlan {
        transaction: TransactionId::new(2).unwrap(),
        layout,
        integrity: if modify {
            IntegrityChange::Modify(
                IntegrityActivation::new(SignallingSet::Base, &[0, 0, 0, 0]).unwrap(),
            )
        } else {
            IntegrityChange::Start { fresh: FRESH }
        },
        cipher: Some(ActivationSequences::new(layout, &[(1, 0), (2, 8), (3, 0)]).unwrap()),
        dpch_cfn: None,
    }
}
fn fixed(value: u32) -> FixedBitString<32> {
    let mut bits = FixedBitString::default();
    for index in 0..32 {
        bits.set(index, value & (1 << (31 - index)) != 0);
    }
    bits
}
fn completion(context: &IntegrityContext, count: u32) -> Vec<u8> {
    completion_with_cipher(context, count, true)
}
fn completion_with_cipher(context: &IntegrityContext, count: u32, cipher: bool) -> Vec<u8> {
    let body = pdu::SecurityModeComplete::new(
        ie::RRCTransactionIdentifier(2),
        Some(ie::IntegrityProtActivationInfo::new(
            ie::RRCMessageSequenceNumberList(vec![ie::RRCMessageSequenceNumber(0); 4]),
        )),
        cipher.then(|| {
            ie::RBActivationTimeInfoList(
                [(1, 0), (2, 8), (3, 0)]
                    .into_iter()
                    .map(|(rb, sn)| {
                        ie::RBActivationTimeInfo::new(ie::RBIdentity(rb), ie::RLCSequenceNumber(sn))
                    })
                    .collect(),
            )
        }),
        None,
    );
    let mut message = ULDCCHMessage::new(
        Some(ie::IntegrityCheckInfo::new(
            ie::MessageAuthenticationCode(fixed(2)),
            ie::RRCMessageSequenceNumber(0),
        )),
        ULDCCHMessageType::securityModeComplete(body),
    );
    let preimage = rasn::uper::encode(&message).unwrap();
    let mac = context
        .algorithm(context.origin())
        .unwrap()
        .authenticate(
            count,
            FRESH,
            Direction::Uplink,
            &preimage,
            preimage.len() * 8,
        )
        .unwrap();
    message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed(u32::from_be_bytes(mac))),
        ie::RRCMessageSequenceNumber((count & 15) as u8),
    ));
    rasn::uper::encode(&message).unwrap()
}

#[test]
fn user_cipher_tracks_explicit_cipher_commands_not_the_latest_integrity_generation() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    assert!(owner.user_cipher(Domain::Cs).unwrap().is_none());
    let mut first = None;
    for cycle in 0..3 {
        let tick = 10 + cycle * 10;
        let cipher_present = cycle != 1;
        let mut command = command(0, ranap::KeyStatus::New);
        if !cipher_present {
            command.encryption = None;
        }
        let request = core.request(Domain::Cs, &command, tick);
        owner
            .receive(time(base, tick), ControlTime(tick), request, &command)
            .unwrap();
        let mut radio_plan = plan(cycle != 0);
        if !cipher_present {
            radio_plan.cipher = None;
        }
        let Preparation::Prepared(origin) = owner
            .prepare(Domain::Cs, time(base, tick), &capabilities(), radio_plan)
            .unwrap()
        else {
            panic!("valid security command rejected")
        };
        assert!(matches!(owner.user_cipher(Domain::Cs), Err(Error::Phase)));
        let context = owner.keys(origin).unwrap().integrity().clone();
        let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
        owner
            .publish_with(Domain::Cs, time(base, tick), &mut downlink, |_| {
                Ok::<_, ()>(())
            })
            .unwrap();
        let bytes = completion_with_cipher(&context, 1, cipher_present);
        UplinkIntegrity::new(context, 2, FRESH, Start::new(0).unwrap(), Some(0))
            .unwrap()
            .receive_with(origin, &bytes, &mut vec![0; bytes.len()], |accepted| {
                owner.complete_with(Domain::Cs, time(base, tick + 1), accepted, |activation| {
                    assert!(activation.new_keys);
                    Ok::<_, ()>(())
                })
            })
            .unwrap();
        assert_eq!(owner.unconfirmed_origin(), Some(origin));
        let held_cipher = owner.user_cipher(Domain::Cs).unwrap().unwrap();
        assert_eq!(
            held_cipher.context.origin(),
            if cipher_present {
                origin
            } else {
                first.unwrap()
            }
        );
        assert!(owner.user_cipher(Domain::Ps).unwrap().is_none());
        core.report(&mut owner, Domain::Cs, time(base, tick + 2));
        owner
            .confirm_completion_with(origin, || Ok::<_, ()>(()))
            .unwrap();
        let cipher = owner.user_cipher(Domain::Cs).unwrap().unwrap();
        assert_eq!(cipher.context.origin(), held_cipher.context.origin());
        if cycle == 0 {
            first = Some(origin);
        }
        assert_eq!(
            cipher.context.origin(),
            if cipher_present {
                origin
            } else {
                first.unwrap()
            }
        );
        assert!(cipher.context.is_clear());
        assert_eq!(
            cipher.command_counters,
            Some(CounterInitialization::NewKeys)
        );
        assert_eq!(owner.active_origin(Domain::Cs), Some(origin));
        assert!(owner.user_cipher(Domain::Ps).unwrap().is_none());
    }
    owner.retire();
    assert!(matches!(owner.user_cipher(Domain::Cs), Err(Error::Phase)));
}

#[test]
fn scheduled_publication_retains_the_exact_reused_start_for_user_cipher_provenance() {
    let base = Instant::now();
    let start = Start::new(0x12345).unwrap();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let origin = prepare(&mut owner, &mut core, time(base, 4));
    let context = owner.keys(origin).unwrap().integrity().clone();
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    assert!(
        owner
            .publish_scheduled_with(
                Domain::Cs,
                time(base, 5),
                Some(start),
                &mut schedule,
                |_| Err::<(), _>("full")
            )
            .is_err()
    );
    owner
        .publish_scheduled_with(
            Domain::Cs,
            time(base, 5),
            Some(start),
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let count = start.initial_count() | 1;
    let bytes = completion(&context, count);
    UplinkIntegrity::new(context, 2, FRESH, start, Some(0))
        .unwrap()
        .receive_with(origin, &bytes, &mut vec![0; bytes.len()], |accepted| {
            owner.complete_with(Domain::Cs, time(base, 6), accepted, |activation| {
                assert!(!activation.new_keys);
                Ok::<_, ()>(())
            })
        })
        .unwrap();
    core.report(&mut owner, Domain::Cs, time(base, 7));
    owner
        .confirm_completion_with(origin, || Ok::<_, ()>(()))
        .unwrap();
    let cipher = owner.user_cipher(Domain::Cs).unwrap().unwrap();
    assert_eq!(cipher.context.origin(), origin);
    assert!(!cipher.context.is_clear());
    assert_eq!(
        cipher.command_counters,
        Some(CounterInitialization::ReusedKeys(start))
    );
}
fn prepare(owner: &mut ConnectionSecurity, core: &mut Core, now: SecurityTime) -> KeyOrigin {
    let command = command(1, ranap::KeyStatus::Old);
    let token = core.request(Domain::Cs, &command, now.control.0);
    owner.receive(now, now.control, token, &command).unwrap();
    let Preparation::Prepared(origin) = owner
        .prepare(Domain::Cs, now, &capabilities(), plan(false))
        .unwrap()
    else {
        panic!("expected prepared radio exchange");
    };
    origin
}

#[test]
fn core_queue_backpressure_does_not_undo_authenticated_radio_completion() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let origin = prepare(&mut owner, &mut core, time(base, 4));
    let context = owner.keys(origin).unwrap().integrity().clone();
    let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
    assert!(
        owner
            .publish_with(Domain::Cs, time(base, 5), &mut downlink, |_| Err::<(), _>(
                "full"
            ))
            .is_err()
    );
    assert_eq!(downlink.next_count(), Ok(0));
    assert_eq!(owner.phase(Domain::Cs), Phase::Prepared);
    owner
        .publish_with(
            Domain::Cs,
            time(base, 5),
            &mut downlink,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let bytes = completion(&context, 1);
    let mut scratch = vec![0; bytes.len()];
    let mut uplink =
        UplinkIntegrity::new(context, 2, FRESH, Start::new(0).unwrap(), Some(0)).unwrap();
    assert!(
        uplink
            .receive_with(origin, &bytes, &mut scratch, |accepted| {
                owner.complete_with(Domain::Cs, time(base, 6), accepted, |_| {
                    Err::<(), _>("activation unavailable")
                })
            })
            .is_err()
    );
    assert_eq!(uplink.last_sequence(), Some(0));
    assert_eq!(owner.active_origin(Domain::Cs), None);
    uplink
        .receive_with(origin, &bytes, &mut scratch, |accepted| {
            owner.complete_with(Domain::Cs, time(base, 6), accepted, |activation| {
                assert_eq!(activation.keys.origin(), origin);
                assert_eq!(activation.completion.origin(), origin);
                Ok::<_, ()>(())
            })
        })
        .unwrap();
    assert_eq!(uplink.last_sequence(), Some(1));
    assert_eq!(owner.phase(Domain::Cs), Phase::Reporting);
    assert_eq!(owner.active_origin(Domain::Cs), Some(origin));
    assert_eq!(
        owner.publish_report_with(Domain::Cs, time(base, 7), |_, _| Err::<(), _>("full")),
        Err(Error::Downstream("full"))
    );
    assert_eq!(owner.active_origin(Domain::Cs), Some(origin));
    core.report(&mut owner, Domain::Cs, time(base, 8));
    assert_eq!(owner.phase(Domain::Cs), Phase::Idle);
    assert!(owner.keys(origin).is_some());
    assert_eq!(owner.unconfirmed_origin(), Some(origin));
    assert_eq!(
        owner.confirm_completion_with(origin, || Err::<(), _>("delivery unproven")),
        Err(Error::Downstream("delivery unproven"))
    );
    assert_eq!(owner.unconfirmed_origin(), Some(origin));
    owner
        .confirm_completion_with(origin, || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(owner.unconfirmed_origin(), None);
    assert_eq!(
        owner.confirm_completion_with(origin, || Ok::<_, ()>(())),
        Err(Error::Phase)
    );
}

#[test]
fn rekey_keeps_previous_domain_keys_until_completion_delivery_and_blocks_another_prepare() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let first = prepare(&mut owner, &mut core, time(base, 4));
    for (cycle, origin) in [
        (0, first),
        (
            1,
            KeyOrigin {
                generation: NonZeroU64::new(2).unwrap(),
                ..first
            },
        ),
    ] {
        let tick = 5 + cycle * 10;
        if cycle != 0 {
            let mut command = command(1, ranap::KeyStatus::New);
            command.integrity.key = ranap::Key::new([0x55; 16]);
            let request = core.request(Domain::Cs, &command, tick);
            owner
                .receive(time(base, tick), ControlTime(tick), request, &command)
                .unwrap();
            assert_eq!(
                owner.prepare(Domain::Cs, time(base, tick), &capabilities(), plan(true)),
                Ok(Preparation::Prepared(origin))
            );
        }
        let context = owner.keys(origin).unwrap().integrity().clone();
        let count = cycle as u32 + 1;
        let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, count - 1).unwrap();
        owner
            .publish_with(Domain::Cs, time(base, tick), &mut downlink, |_| {
                Ok::<_, ()>(())
            })
            .unwrap();
        let bytes = completion(&context, count);
        UplinkIntegrity::new(
            context,
            2,
            FRESH,
            Start::new(0).unwrap(),
            Some((count - 1) as u8),
        )
        .unwrap()
        .receive_with(origin, &bytes, &mut vec![0; bytes.len()], |accepted| {
            owner.complete_with(Domain::Cs, time(base, tick + 1), accepted, |_| {
                Ok::<_, ()>(())
            })
        })
        .unwrap();
        core.report(&mut owner, Domain::Cs, time(base, tick + 2));
        assert_eq!(owner.unconfirmed_origin(), Some(origin));
        if cycle != 0 {
            assert_eq!(owner.previous_completion_origin(), Some(first));
            assert!(owner.keys(first).is_some());
            assert_eq!(owner.active_origin(Domain::Cs), Some(origin));
            let command = command(1, ranap::KeyStatus::New);
            let request = core.request(Domain::Ps, &command, tick + 3);
            owner
                .receive(
                    time(base, tick + 3),
                    ControlTime(tick + 3),
                    request,
                    &command,
                )
                .unwrap();
            assert_eq!(
                owner.prepare(
                    Domain::Ps,
                    time(base, tick + 3),
                    &capabilities(),
                    plan(true)
                ),
                Err(Error::Phase)
            );
            assert_eq!(owner.phase(Domain::Ps), Phase::AwaitingPreparation);
        }
        owner
            .confirm_completion_with(origin, || Ok::<_, ()>(()))
            .unwrap();
    }
    assert!(owner.keys(first).is_none());
    assert_eq!(owner.previous_completion_origin(), None);
}

#[test]
fn delayed_admission_and_capability_wait_never_restart_the_original_guard() {
    let base = Instant::now();
    let mut core = Core::new();
    let command = command(1, ranap::KeyStatus::Old);
    let token = core.request(Domain::Cs, &command, 4);
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    assert_eq!(
        owner.receive(time(base, 4000), ControlTime(4), token, &command),
        Ok(Admission::Accepted)
    );
    assert_eq!(
        owner.receive(time(base, 4500), ControlTime(4), token, &command),
        Ok(Admission::Duplicate)
    );
    assert_eq!(
        owner.prepare(Domain::Cs, time(base, 5004), &capabilities(), plan(false)),
        Ok(Preparation::Rejected(
            ranap::SecurityReject::RadioProcedureFailure
        ))
    );
    assert_eq!(owner.phase(Domain::Cs), Phase::Reporting);
    core.report(&mut owner, Domain::Cs, time(base, 5004));
    assert_eq!(
        owner.receive(time(base, 5005), ControlTime(4), token, &command),
        Err(Error::Owner)
    );
}

#[test]
fn radio_timeout_requires_explicit_successful_recovery_before_reporting_failure() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let origin = prepare(&mut owner, &mut core, time(base, 4));
    assert_eq!(
        owner.timeout_action(Domain::Cs, time(base, 5004)),
        Ok(Some(origin))
    );
    assert_eq!(owner.phase(Domain::Cs), Phase::Prepared);
    assert!(
        owner
            .abort_with(
                Domain::Cs,
                time(base, 5004),
                AbortReason::Timeout,
                |_, _| Err::<(), _>("recovery blocked")
            )
            .is_err()
    );
    assert!(owner.keys(origin).is_some());
    assert_eq!(owner.phase(Domain::Cs), Phase::Prepared);
    owner
        .abort_with(
            Domain::Cs,
            time(base, 5004),
            AbortReason::Timeout,
            |actual, reason| {
                assert_eq!(actual, origin);
                assert_eq!(reason, AbortReason::Timeout);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    assert_eq!(owner.phase(Domain::Cs), Phase::Reporting);
    assert!(owner.keys(origin).is_none());
    core.report(&mut owner, Domain::Cs, time(base, 5005));
}

#[test]
fn both_domains_retain_independent_requests_but_serialize_shared_rrc_security() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let command = command(1, ranap::KeyStatus::Old);
    let cs = core.request(Domain::Cs, &command, 4);
    let ps = core.request(Domain::Ps, &command, 4);
    owner
        .receive(time(base, 4), ControlTime(4), cs, &command)
        .unwrap();
    owner
        .receive(time(base, 4), ControlTime(4), ps, &command)
        .unwrap();
    let Preparation::Prepared(first) = owner
        .prepare(Domain::Cs, time(base, 4), &capabilities(), plan(false))
        .unwrap()
    else {
        panic!("first command was not prepared");
    };
    assert_eq!(
        owner.prepare(Domain::Ps, time(base, 4), &capabilities(), plan(false)),
        Err(Error::Phase)
    );
    assert_eq!(owner.phase(Domain::Ps), Phase::AwaitingPreparation);
    owner
        .abort_with(Domain::Cs, time(base, 5), AbortReason::Cancelled, |_, _| {
            Ok::<_, ()>(())
        })
        .unwrap();
    let Preparation::Prepared(second) = owner
        .prepare(Domain::Ps, time(base, 5), &capabilities(), plan(false))
        .unwrap()
    else {
        panic!("peer command was not prepared");
    };
    assert_ne!(first.generation, second.generation);
    assert_ne!(first.domain, second.domain);
    assert!(owner.keys(first).is_none());
    assert!(owner.keys(second).is_some());
    owner.retire();
    assert_eq!(owner.phase(Domain::Ps), Phase::Retired);
    assert!(owner.keys(second).is_some());
    assert!(
        owner
            .publish_report_with(Domain::Cs, time(base, 6), |_, _| Ok::<_, ()>(()))
            .is_err()
    );
}

#[test]
fn unsupported_algorithms_report_reject_without_publishing_a_radio_command() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let command = command(2, ranap::KeyStatus::Old);
    let token = core.request(Domain::Cs, &command, 4);
    owner
        .receive(time(base, 4), ControlTime(4), token, &command)
        .unwrap();
    assert_eq!(
        owner.prepare(Domain::Cs, time(base, 4), &capabilities(), plan(false)),
        Ok(Preparation::Rejected(
            ranap::SecurityReject::AlgorithmsUnsupported
        ))
    );
    assert_eq!(owner.pending_origin(Domain::Cs), None);
    core.report(&mut owner, Domain::Cs, time(base, 5));
}

#[test]
fn active_domain_algorithms_constrain_peer_requests_and_old_key_reuse() {
    let base = Instant::now();
    let mut core = Core::new();
    let mut owner = ConnectionSecurity::new(key(), FRESH);
    let first = prepare(&mut owner, &mut core, time(base, 4));
    let context = owner.keys(first).unwrap().integrity().clone();
    let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
    owner
        .publish_with(
            Domain::Cs,
            time(base, 5),
            &mut downlink,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let bytes = completion(&context, 1);
    UplinkIntegrity::new(context, 2, FRESH, Start::new(0).unwrap(), Some(0))
        .unwrap()
        .receive_with(first, &bytes, &mut vec![0; bytes.len()], |accepted| {
            owner.complete_with(Domain::Cs, time(base, 6), accepted, |_| Ok::<_, ()>(()))
        })
        .unwrap();
    core.report(&mut owner, Domain::Cs, time(base, 8));
    owner
        .confirm_completion_with(first, || Ok::<_, ()>(()))
        .unwrap();
    let old = command(1, ranap::KeyStatus::Old);
    let token = core.request(Domain::Cs, &old, 9);
    owner
        .receive(time(base, 9), ControlTime(9), token, &old)
        .unwrap();
    assert_eq!(
        owner.prepare(Domain::Cs, time(base, 9), &capabilities(), plan(true)),
        Ok(Preparation::Rejected(ranap::SecurityReject::Conflict))
    );
    assert_eq!(owner.active_origin(Domain::Cs), Some(first));
    core.report(&mut owner, Domain::Cs, time(base, 10));
    let incompatible = command(0, ranap::KeyStatus::Old);
    let token = core.request(Domain::Ps, &incompatible, 11);
    owner
        .receive(time(base, 11), ControlTime(11), token, &incompatible)
        .unwrap();
    assert_eq!(
        owner.prepare(Domain::Ps, time(base, 11), &capabilities(), plan(true)),
        Ok(Preparation::Rejected(ranap::SecurityReject::Conflict))
    );
    assert_eq!(owner.active_origin(Domain::Cs), Some(first));
    assert_eq!(owner.active_origin(Domain::Ps), None);
    core.report(&mut owner, Domain::Ps, time(base, 12));
    let mut allowed = command(1, ranap::KeyStatus::Old);
    allowed.integrity.key = ranap::Key::new([0x33; 16]);
    allowed.encryption.as_mut().unwrap().key = ranap::Key::new([0x44; 16]);
    let token = core.request(Domain::Ps, &allowed, 13);
    owner
        .receive(time(base, 13), ControlTime(13), token, &allowed)
        .unwrap();
    assert_eq!(
        owner.prepare(Domain::Ps, time(base, 13), &capabilities(), plan(false)),
        Err(Error::RadioPlan)
    );
    let Preparation::Prepared(second) = owner
        .prepare(Domain::Ps, time(base, 13), &capabilities(), plan(true))
        .unwrap()
    else {
        panic!("matching peer alternatives were not prepared");
    };
    assert_eq!(second.generation.get(), first.generation.get() + 1);
    assert_ne!(first.domain, second.domain);
    assert_eq!(owner.active_origin(Domain::Cs), Some(first));
    assert_eq!(owner.pending_origin(Domain::Ps), Some(second));
    assert!(owner.keys(first).is_some() && owner.keys(second).is_some());
}

#[test]
fn released_iu_connection_retains_radio_security_but_accepts_old_keys_on_reconnect() {
    for domain in [Domain::Cs, Domain::Ps] {
        let base = Instant::now();
        let mut core = Core::new();
        let mut owner = ConnectionSecurity::new(key(), FRESH);
        let first_command = command(1, ranap::KeyStatus::New);
        let request = core.request(domain, &first_command, 4);
        owner
            .receive(time(base, 4), ControlTime(4), request, &first_command)
            .unwrap();
        let Preparation::Prepared(first) = owner
            .prepare(domain, time(base, 4), &capabilities(), plan(false))
            .unwrap()
        else {
            panic!("initial security rejected");
        };
        assert_eq!(owner.release_signalling(domain), Err(Error::Phase));
        let context = owner.keys(first).unwrap().integrity().clone();
        let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
        owner
            .publish_with(domain, time(base, 5), &mut downlink, |_| Ok::<_, ()>(()))
            .unwrap();
        let bytes = completion(&context, 1);
        UplinkIntegrity::new(context, 2, FRESH, Start::new(0).unwrap(), Some(0))
            .unwrap()
            .receive_with(first, &bytes, &mut vec![0; bytes.len()], |accepted| {
                owner.complete_with(domain, time(base, 6), accepted, |_| Ok::<_, ()>(()))
            })
            .unwrap();
        core.report(&mut owner, domain, time(base, 7));
        owner
            .confirm_completion_with(first, || Ok::<_, ()>(()))
            .unwrap();
        let cipher_before = owner.user_cipher(domain).unwrap().unwrap();
        let release = rua::Message::Direct {
            domain,
            context: core.context,
            ranap: ranap::release::Command {
                cause: ranap::rab::Cause::from_code(83).unwrap(),
            }
            .encode(Limits::default())
            .unwrap(),
        };
        let token = core
            .session
            .receive_rua_with(
                core.association,
                ControlTime(8),
                &rua::Message::decode(
                    &release.encode(Limits::default()).unwrap(),
                    Limits::default(),
                )
                .unwrap(),
                |event| {
                    let Event::IuRelease { request, .. } = event else {
                        panic!("expected release");
                    };
                    Ok::<_, ()>(*request)
                },
            )
            .unwrap();
        owner.release_signalling(domain).unwrap();
        owner.release_signalling(domain).unwrap();
        core.session
            .complete_iu_release_without_reports_with(token, ControlTime(8), |_| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(owner.active_origin(domain), Some(first));
        assert!(owner.keys(first).is_some());
        let retained = owner.user_cipher(domain).unwrap().unwrap();
        assert_eq!(retained.context.origin(), cipher_before.context.origin());
        assert_eq!(retained.command_counters, cipher_before.command_counters);
        core.session
            .publish_nas_with(
                UplinkNas {
                    key: key(),
                    domain,
                    nas: &[5, 0x24],
                    selector: None,
                    cause: rua::EstablishmentCause::Normal,
                },
                ControlTime(9),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        let reused = command(1, ranap::KeyStatus::Old);
        let request = core.request(domain, &reused, 10);
        owner
            .receive(time(base, 10), ControlTime(10), request, &reused)
            .unwrap();
        assert!(matches!(
            owner.prepare(domain, time(base, 10), &capabilities(), plan(true)),
            Ok(Preparation::Prepared(_))
        ));
        let transport = owner
            .cipher_plan(domain, Some(Start::new(17).unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            transport.counters,
            CounterInitialization::ReusedKeys(Start::new(17).unwrap())
        );
        assert_eq!(owner.active_origin(domain), Some(first));
    }
}
