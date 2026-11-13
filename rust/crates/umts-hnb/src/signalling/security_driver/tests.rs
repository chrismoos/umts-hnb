// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    connection_security::Phase,
    core_link::{
        CoreLink,
        tests::{Mock, Peer, ready, register, security::fach},
    },
    signalling::tests::{COMPLETE, controller, peer, send_capability_report_with_transaction},
    test_support::{at, source},
};
use std::time::{Duration, Instant};
use umts_iuh::{ContextId, Domain, ranap, rua, session::ControlTime, wire::Limits};
use umts_rlc::acknowledged::AmEntity;
use umts_rrc_codec::decode_downlink_dedicated;
use umts_rrc_schema::class_definitions::DLDCCHMessageType;

struct Fixture {
    source: CommonTx,
    radio: CommonSignalling,
    link: CoreLink<Mock>,
    core: Peer,
    key: UeKey,
    context: ContextId,
    peer: AmEntity,
    base: Instant,
}

impl Fixture {
    fn new() -> Self {
        let source = source(false);
        let mut radio = controller(&source);
        let (mut link, core) = ready(&mut radio);
        let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
        let mut peer = peer(&source);
        peer.enqueue(&COMPLETE).unwrap();
        peer.publish_next(at(0), |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        Self {
            source,
            radio,
            link,
            core,
            key,
            context,
            peer,
            base: Instant::now(),
        }
    }
    fn time(&self, millis: u64) -> SecurityTime {
        SecurityTime {
            control: ControlTime(millis),
            monotonic: self.base + Duration::from_millis(millis),
        }
    }
    fn request(&mut self, cipher: u8) {
        self.core.rua(rua::Message::Direct {
            domain: Domain::Cs,
            context: self.context,
            ranap: ranap::encode_security(
                &ranap::SecurityCommand {
                    integrity: ranap::Protection {
                        algorithms: vec![ranap::Algorithm(0)],
                        key: ranap::Key::new([0x11; 16]),
                    },
                    encryption: Some(ranap::Protection {
                        algorithms: vec![ranap::Algorithm(cipher)],
                        key: ranap::Key::new([0x22; 16]),
                    }),
                    key_status: ranap::KeyStatus::New,
                },
                Limits::default(),
            )
            .unwrap(),
        });
        self.link
            .drive_at(self.time(6), &mut self.radio, |_, _| Err::<(), _>(()))
            .unwrap();
        assert_eq!(
            self.radio.security(self.key).unwrap().phase(Domain::Cs),
            Phase::AwaitingPreparation
        );
    }
    fn actuate(&mut self, work: SecurityWork, millis: u64) -> SecurityAction {
        self.radio
            .actuate_security(work, at(0), self.time(millis))
            .unwrap()
    }
    fn need(&self) -> SecurityWork {
        SecurityWork::NeedCapabilities {
            key: self.key,
            domain: Domain::Cs,
        }
    }
    fn prepare(&self) -> SecurityWork {
        SecurityWork::ReadyToPrepare {
            key: self.key,
            domain: Domain::Cs,
        }
    }
    fn confirm_capabilities(&mut self) {
        assert!(
            matches!(self.actuate(self.need(), 7), SecurityAction::CapabilitiesRequested { transaction, .. } if transaction.get() == 0)
        );
        send_capability_report_with_transaction(
            &mut self.radio,
            self.key,
            &mut self.peer,
            TransactionId::new(0).unwrap(),
        );
        assert_eq!(self.radio.process_one_at(self.time(8)), Some(Ok(self.key)));
        self.drain();
    }
    fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut messages = Vec::new();
        for _ in 0..8 {
            messages.extend(fach(
                &mut self.radio,
                &mut self.source,
                self.key,
                SignallingBearer::ControlAm,
                &mut self.peer,
                None,
            ));
        }
        messages
    }
    fn fill(&mut self) {
        for _ in 0..self.radio.limits.queued_sdus_per_bearer.get() {
            self.radio
                .registry
                .get_mut(self.key)
                .unwrap()
                .control
                .enqueue(self.key, Bits::new(&[0], 8).unwrap())
                .unwrap();
        }
    }
}

#[test]
fn automatic_capability_enquiry_is_idempotent_and_capacity_does_not_consume_an_identifier() {
    let mut f = Fixture::new();
    f.request(1);
    f.fill();
    assert_eq!(
        f.actuate(f.need(), 7),
        SecurityAction::Deferred {
            key: f.key,
            domain: Domain::Cs
        }
    );
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .capabilities
            .pending_transaction(),
        None
    );
    f.drain();
    assert!(
        matches!(f.actuate(f.need(), 8), SecurityAction::CapabilitiesRequested { transaction, .. } if transaction.get() == 0)
    );
    let next = f
        .radio
        .registry
        .get(f.key)
        .unwrap()
        .control
        .window_positions()
        .next_transmit;
    for _ in 0..8 {
        assert!(
            matches!(f.actuate(f.need(), 9), SecurityAction::CapabilitiesPending { transaction, .. } if transaction.get() == 0)
        );
    }
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .control
            .window_positions()
            .next_transmit,
        next
    );
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .transactions
            .next(MessageType::CapabilityEnquiry)
            .get(),
        1
    );
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .transactions
            .next(MessageType::SecurityModeCommand)
            .get(),
        0
    );
}

#[test]
fn core_request_capability_confirmation_and_automatic_command_publication_are_connected() {
    let mut f = Fixture::new();
    f.request(1);
    f.confirm_capabilities();
    let action = f.actuate(f.prepare(), 9);
    let SecurityAction::CommandPublished {
        transaction,
        origin,
        ..
    } = action
    else {
        panic!("{action:?}")
    };
    assert_eq!(transaction.get(), 0);
    assert_eq!(
        f.radio.security(f.key).unwrap().phase(Domain::Cs),
        Phase::AwaitingRadio
    );
    assert_eq!(
        f.radio.security(f.key).unwrap().pending_origin(Domain::Cs),
        Some(origin)
    );
    let messages = f.drain();
    assert!(messages.iter().any(|bytes| {
        let decoded = decode_downlink_dedicated(bytes, bytes.len() * BITS_PER_BYTE).unwrap();
        matches!(
            decoded.value.message,
            DLDCCHMessageType::securityModeCommand(_)
        )
    }));
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .transactions
            .next(MessageType::SecurityModeCommand)
            .get(),
        1
    );
    assert_eq!(
        f.actuate(f.prepare(), 10),
        SecurityAction::Deferred {
            key: f.key,
            domain: Domain::Cs
        }
    );
}

#[test]
fn blocked_unsent_command_replans_without_reusing_key_generation_or_extending_core_guard() {
    let mut f = Fixture::new();
    f.request(1);
    f.confirm_capabilities();
    let now = f.time(9);
    f.radio
        .prepare_common_security(f.key, Domain::Cs, now, TransactionId::new(0).unwrap())
        .unwrap();
    let first = f
        .radio
        .security(f.key)
        .unwrap()
        .pending_origin(Domain::Cs)
        .unwrap();
    f.fill();
    assert_eq!(
        f.actuate(
            SecurityWork::CommandReady {
                key: f.key,
                domain: Domain::Cs
            },
            10
        ),
        SecurityAction::Deferred {
            key: f.key,
            domain: Domain::Cs
        }
    );
    assert_eq!(
        f.radio.security(f.key).unwrap().phase(Domain::Cs),
        Phase::AwaitingPreparation
    );
    assert!(f.radio.security(f.key).unwrap().keys(first).is_none());
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .transactions
            .next(MessageType::SecurityModeCommand)
            .get(),
        0
    );
    f.drain();
    let action = f.actuate(f.prepare(), 11);
    assert!(
        matches!(action, SecurityAction::CommandPublished { transaction, origin, .. } if transaction.get() == 0 && origin != first)
    );
    let origin = f
        .radio
        .security(f.key)
        .unwrap()
        .pending_origin(Domain::Cs)
        .unwrap();
    assert_eq!(
        f.actuate(
            SecurityWork::AbortRequired {
                key: f.key,
                domain: Domain::Cs,
                origin
            },
            5006
        ),
        SecurityAction::RecoveryRequired {
            key: f.key,
            origin: Some(origin)
        }
    );
    assert!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .protection
            .blocks_status()
    );
    let now = f.time(5007);
    let work: Vec<_> = (0..2 * f.radio.limits.maximum_ues.get())
        .map(|_| f.radio.service_security_one(now).unwrap())
        .collect();
    assert!(work.contains(&SecurityWork::RecoveryRequired { key: f.key }));
    assert!(
        !work
            .iter()
            .any(|work| matches!(work, SecurityWork::AbortRequired { .. }))
    );
    assert_eq!(
        f.radio.security(f.key).unwrap().phase(Domain::Cs),
        Phase::AwaitingRadio
    );
}

#[test]
fn unsupported_algorithms_and_expired_unpublished_commands_produce_core_rejections() {
    for expired in [false, true] {
        let mut f = Fixture::new();
        f.request(if expired { 1 } else { 2 });
        f.confirm_capabilities();
        let action = if expired {
            let now = f.time(9);
            f.radio
                .prepare_common_security(f.key, Domain::Cs, now, TransactionId::new(0).unwrap())
                .unwrap();
            let origin = f
                .radio
                .security(f.key)
                .unwrap()
                .pending_origin(Domain::Cs)
                .unwrap();
            let work = SecurityWork::AbortRequired {
                key: f.key,
                domain: Domain::Cs,
                origin,
            };
            assert_eq!(
                f.radio.actuate_security(work, at(0), f.time(10)),
                Err(SignallingError::Phase)
            );
            f.actuate(work, 5006)
        } else {
            f.actuate(f.prepare(), 9)
        };
        assert!(matches!(action, SecurityAction::Rejected { .. }));
        assert_eq!(
            f.radio.security(f.key).unwrap().phase(Domain::Cs),
            Phase::Reporting
        );
        assert!(
            f.radio
                .publish_security_report(f.key, Domain::Cs, f.time(5007))
                .unwrap()
        );
        assert_eq!(
            f.radio.security(f.key).unwrap().phase(Domain::Cs),
            Phase::Idle
        );
    }
}

#[test]
fn stale_owner_and_wrong_radio_epoch_cannot_actuate_a_procedure() {
    let mut f = Fixture::new();
    f.request(1);
    let mut key = f.key;
    key.generation = NonZeroU64::new(key.generation.get() + 1).unwrap();
    assert_eq!(
        f.radio.actuate_security(
            SecurityWork::NeedCapabilities {
                key,
                domain: Domain::Cs
            },
            at(0),
            f.time(7)
        ),
        Err(SignallingError::Origin)
    );
    let mut now = at(0);
    now.epoch = umts_types::RadioEpoch::new(NonZeroU64::new(now.epoch.get() + 1).unwrap());
    assert_eq!(
        f.radio.actuate_security(f.need(), now, f.time(7)),
        Err(SignallingError::Origin)
    );
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .capabilities
            .pending_transaction(),
        None
    );
}
