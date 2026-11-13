// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::{cell::RefCell, rc::Rc};
use umts_core::Gateway;
use umts_iuh::{session::RabOutcome, wire::Protocol};
use umts_sctp::{Metadata, Notification, Receive, SendResult};

enum Input {
    Established,
    Packet(Protocol, Vec<u8>),
    Closed,
}

struct Signalling {
    input: Rc<RefCell<VecDeque<Input>>>,
    held: Option<Input>,
}

impl umts_core::Transport for Signalling {
    type Error = ();

    fn complete_connect(&mut self) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn receive_once(&mut self) -> Result<Receive<'_>, Self::Error> {
        if self.held.is_none() {
            self.held = self.input.borrow_mut().pop_front();
        }
        Ok(match self.held.as_ref() {
            None => Receive::Idle,
            Some(Input::Established) => Receive::Notification(Notification::Established {
                inbound_streams: 2,
                outbound_streams: 2,
                association: 42,
            }),
            Some(Input::Packet(protocol, bytes)) => Receive::Record {
                metadata: Metadata {
                    stream: 0,
                    sequence: 0,
                    ppid: protocol.ppid().unwrap(),
                    association: 42,
                },
                bytes,
            },
            Some(Input::Closed) => Receive::Closed,
        })
    }

    fn acknowledge_receive(&mut self) -> Result<(), Self::Error> {
        assert!(self.held.take().is_some());
        Ok(())
    }

    fn send(&mut self, _: u16, _: u32, _: &[u8]) -> Result<SendResult, Self::Error> {
        Ok(SendResult::Sent)
    }
}

struct Connected {
    gateway: Gateway<Signalling>,
    input: Rc<RefCell<VecDeque<Input>>>,
    now: u64,
}

impl Connected {
    fn new(key: UeKey) -> Self {
        let input = Rc::new(RefCell::new(VecDeque::from([Input::Established])));
        let session = Session::new(
            key.epoch,
            AssociationId(NonZeroU64::MIN),
            HnbParameters {
                identity: b"packet reconciliation".to_vec(),
                plmn: [0, 0xf1, 0x10],
                lac: 23,
                rac: 7,
                sac: 42,
                cell: 1,
                access: AccessMode::Open,
                csg: None,
            },
            SessionLimits {
                maximum_ues: count(2),
                maximum_rab_requests_per_domain: count(8),
                maximum_rab_release_requests_per_domain: count(2),
                request_timeout_ms: NonZeroU64::new(1000).unwrap(),
                codec: WireLimits::default(),
            },
            ControlTime(0),
        )
        .unwrap();
        let mut core = Self {
            gateway: Gateway::new(
                session,
                Signalling {
                    input: input.clone(),
                    held: None,
                },
                umts_core::Limits {
                    outbound_entries: count(4),
                    maximum_packet_bytes: count(65536),
                    outbound_byte_budget: count(4 * 65536),
                    connect_timeout_ms: NonZeroU64::new(100).unwrap(),
                },
                ControlTime(0),
            )
            .unwrap(),
            input,
            now: 0,
        };
        core.drive();
        core.drive();
        core.input.borrow_mut().push_back(Input::Packet(
            Protocol::Hnbap,
            hnbap::Message::HnbAccepted {
                rnc: 1,
                mux_port: None,
            }
            .encode(WireLimits::default())
            .unwrap(),
        ));
        assert!(matches!(&core.drive()[..], [Event::HnbAccepted { .. }]));
        let identity = UeIdentity::Imsi(Imsi::from_digits("001010000000001").unwrap());
        core.gateway
            .register_ue(
                key,
                identity.clone(),
                Capabilities {
                    release: Release::R99,
                    csg: CsgCapability::NotCapable,
                },
                RegistrationCause::Normal,
                ControlTime(core.now),
            )
            .unwrap();
        core.drive();
        core.input.borrow_mut().push_back(Input::Packet(
            Protocol::Hnbap,
            hnbap::Message::UeAccepted {
                identity,
                context: ContextId::new(CONTEXT).unwrap(),
                membership: None,
            }
            .encode(WireLimits::default())
            .unwrap(),
        ));
        assert!(matches!(&core.drive()[..], [Event::UeAccepted { .. }]));
        for domain in [Domain::Cs, Domain::Ps] {
            core.gateway
                .publish_nas(
                    ControlTime(core.now),
                    UplinkNas {
                        key,
                        domain,
                        nas: &[8, 0x41],
                        selector: None,
                        cause: rua::EstablishmentCause::Normal,
                    },
                )
                .unwrap();
            core.drive();
        }
        core
    }

    fn drive(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        self.gateway
            .drive(ControlTime(self.now), |event| {
                events.push(event.clone());
                Ok::<_, ()>(())
            })
            .unwrap();
        self.now += 1;
        events
    }

    fn rua(&mut self, message: rua::Message) -> Vec<Event> {
        self.input.borrow_mut().push_back(Input::Packet(
            Protocol::Rua,
            message.encode(WireLimits::default()).unwrap(),
        ));
        self.drive()
    }

    fn request(&mut self, assignment: Assignment) -> RabRequest {
        let events = self.rua(rua::Message::Direct {
            context: ContextId::new(CONTEXT).unwrap(),
            domain: Domain::Ps,
            ranap: assignment.encode(WireLimits::default()).unwrap(),
        });
        let [Event::RabAssignment { request, .. }] = events.as_slice() else {
            panic!("expected real RAB request")
        };
        *request
    }

    fn setup(&mut self, item: &SetupOrModify) -> RabRequest {
        self.request(Assignment {
            setup: vec![item.clone()],
            release: vec![],
        })
    }

    fn release(&mut self, id: u8) -> RabRequest {
        self.request(Assignment {
            setup: vec![],
            release: vec![umts_iuh::ranap::rab::Release::new(
                id,
                umts_iuh::ranap::rab::Cause::from_code(NORMAL_RELEASE).unwrap(),
            )],
        })
    }

    fn accepted(&mut self, ticket: Ticket, setup: Setup) {
        self.gateway
            .record_rab_outcome(
                ControlTime(self.now),
                ticket.request(),
                RabOutcome::Setup(setup),
            )
            .unwrap();
        assert!(
            self.gateway
                .publish_ready_rab(ControlTime(self.now))
                .unwrap()
        );
        self.drive();
    }

    fn disconnect(&mut self, domain: Domain) -> umts_iuh::session::IuReleaseRequest {
        let events = self.rua(rua::Message::Disconnect {
            context: ContextId::new(CONTEXT).unwrap(),
            domain,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        });
        let [Event::Disconnected { request, .. }] = events.as_slice() else {
            panic!("owned disconnect required")
        };
        *request
    }
}

#[test]
fn active_packet_transport_modification_is_atomic_and_keeps_the_radio_owner() {
    for change_radio in [false, true] {
        let (mut env, initial) = environment();
        let radio = mapping(&env, &initial.procedure, env.plan.clone());
        let id = initial
            .procedure
            .rabs()
            .iter()
            .find(|rab| rab.domain == CnDomain::Ps)
            .unwrap()
            .identity;
        let mut core = Connected::new(env.key);
        let mut packets = rabs();
        let original = item(id);
        let request = core.setup(&original);
        let authority = core
            .gateway
            .authorize_rab(request, id, RabOperation::SetupOrModify)
            .unwrap();
        let ticket = packets.prepare(authority, &original, radio).unwrap();
        start_radio(&mut env, initial);
        complete_radio(&mut env);
        let authority = core
            .gateway
            .authorize_rab(request, id, RabOperation::SetupOrModify)
            .unwrap();
        let setup = packets
            .complete(ticket, authority, &mut env.receive)
            .unwrap();
        core.accepted(ticket, setup);
        let mut update = SetupOrModify {
            first: First::modification(id),
            second: Second::default(),
        };
        update.first.transport = Some(Transport::new(
            Address::from_ip(Domain::Ps, replacement_peer().ip()).unwrap(),
            Association::gtp(7),
        ));
        if change_radio {
            let mut parameters = original.first.parameters.clone().unwrap();
            parameters.maximum = qos::MaximumRates::new(&[PACKET_RATE / 2]).unwrap();
            update.first.parameters = Some(parameters);
        }
        let changing = core.setup(&update);
        let now = ControlTime(core.now);
        let mut link = crate::core_link::CoreLink::new(core.gateway);
        let cursor = env.source.next_chip();
        let result = packets.modify_existing(
            &mut link,
            &mut env.receive,
            crate::packet_rabs::PacketModification {
                request: changing,
                item: &update,
                identity: env.identity,
                plan: &env.plan,
                now,
            },
        );
        assert_eq!(result.is_ok(), !change_radio);
        assert_eq!(env.source.next_chip(), cursor);
        assert_eq!(
            env.admission.procedure(env.identity).unwrap().phase(),
            SetupPhase::Complete
        );
        assert_eq!(
            packets.sequence_state(ticket.key()).unwrap().next_uplink,
            u16::MAX
        );
        if change_radio {
            assert_eq!(packets.requested(ticket.key()), Some(&original));
            assert!(
                link.gateway()
                    .authorize_rab(changing, id, RabOperation::SetupOrModify)
                    .is_ok()
            );
        } else {
            assert_eq!(
                packets.requested(ticket.key()).unwrap().first.transport,
                update.first.transport
            );
            assert!(
                link.gateway()
                    .authorize_rab(changing, id, RabOperation::SetupOrModify)
                    .is_err()
            );
        }
        uplink(&mut packets, &env, b"preserved established bearer");
        packets.step(&mut env.receive).unwrap();
        let (wire, peer) = &packets.io_mut().outgoing[0];
        assert_eq!(
            *peer,
            if change_radio {
                super::peer()
            } else {
                replacement_peer()
            }
        );
        assert_eq!(
            gtpu::decode(wire).unwrap().payload,
            b"preserved established bearer"
        );
    }
}

#[test]
fn consumed_response_and_superseding_release_preserve_active_packet_connection() {
    let (mut env, initial) = environment();
    let radio = mapping(&env, &initial.procedure, env.plan.clone());
    let id = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    let mut core = Connected::new(env.key);
    let mut rabs = rabs();
    let item = item(id);
    let request = core.setup(&item);
    let authority = core
        .gateway
        .authorize_rab(request, id, RabOperation::SetupOrModify)
        .unwrap();
    let ticket = rabs.prepare(authority, &item, radio).unwrap();
    start_radio(&mut env, initial);
    complete_radio(&mut env);
    let authority = core
        .gateway
        .authorize_rab(request, id, RabOperation::SetupOrModify)
        .unwrap();
    let setup = rabs.complete(ticket, authority, &mut env.receive).unwrap();
    local_teid(&setup);
    core.accepted(ticket, setup);
    assert!(
        core.gateway
            .authorize_rab(request, id, RabOperation::SetupOrModify)
            .is_err()
    );
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (0, 0));
    let modification = SetupOrModify {
        first: First::modification(id),
        second: Second::default(),
    };
    let changing = core.setup(&modification);
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let authority = core
        .gateway
        .authorize_rab(changing, id, RabOperation::SetupOrModify)
        .unwrap();
    let pending = rabs.prepare(authority, &modification, radio).unwrap();
    let foreign = Connected::new(env.key);
    let result = rabs.reconcile_core(&foreign.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (0, 0));
    assert_eq!(rabs.requested(ticket.key()), Some(&item));
    core.release(id);
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (1, 0));
    assert!(rabs.cancel(pending).is_err());
    assert_eq!(rabs.requested(ticket.key()), Some(&item));
    uplink(&mut rabs, &env, b"release still needs radio teardown");
    rabs.step(&mut env.receive).unwrap();
    let packet = gtpu::decode(&rabs.io_mut().outgoing[0].0).unwrap();
    assert_eq!(packet.sequence, Some(u16::MAX));
    assert_eq!(packet.payload, b"release still needs radio teardown");
    let cs_release = core.disconnect(Domain::Cs);
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (0, 0));
    core.gateway
        .complete_iu_release_without_reports(ControlTime(core.now), cs_release)
        .unwrap();
    let ps_release = core.disconnect(Domain::Ps);
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (0, 1));
    assert!(rabs.requested(ticket.key()).is_none());
    core.gateway
        .complete_iu_release_without_reports(ControlTime(core.now), ps_release)
        .unwrap();
    core.gateway
        .publish_nas(
            ControlTime(core.now),
            UplinkNas {
                key: env.key,
                domain: Domain::Ps,
                nas: &[8, 0x41],
                selector: None,
                cause: rua::EstablishmentCause::Normal,
            },
        )
        .unwrap();
    core.drive();
    let replacement = core.setup(&item);
    assert!(
        core.gateway
            .authorize_rab(replacement, id, RabOperation::SetupOrModify)
            .is_ok()
    );
    assert!(core.gateway.rab_connection_current(replacement));
    assert!(!core.gateway.rab_connection_current(ticket.request()));
    assert!(!ticket.request().same_connection(replacement));
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (0, 0));
    assert!(rabs.requested(ticket.key()).is_none());
    assert_eq!(
        env.admission.procedure(env.identity).unwrap().phase(),
        SetupPhase::Complete
    );
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (0, 0));
}

#[test]
fn release_and_association_loss_reclaim_uncommitted_initial_packet_resources() {
    let (mut env, initial) = environment();
    let id = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    let mut core = Connected::new(env.key);
    let mut rabs = rabs();
    let item = item(id);
    let request = core.setup(&item);
    let radio = mapping(&env, &initial.procedure, env.plan.clone());
    let authority = core
        .gateway
        .authorize_rab(request, id, RabOperation::SetupOrModify)
        .unwrap();
    let first = rabs.prepare(authority, &item, radio).unwrap();
    core.release(id);
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (1, 0));
    assert!(
        core.gateway
            .authorize_rab(request, id, RabOperation::SetupOrModify)
            .is_err()
    );
    assert!(rabs.cancel(first).is_err());
    assert!(rabs.requested(first.key()).is_none());
    let replacement = core.setup(&item);
    let radio = mapping(&env, &initial.procedure, env.plan.clone());
    let authority = core
        .gateway
        .authorize_rab(replacement, id, RabOperation::SetupOrModify)
        .unwrap();
    let second = rabs.prepare(authority, &item, radio).unwrap();
    assert_ne!(first, second);
    let authority = core
        .gateway
        .authorize_rab(replacement, id, RabOperation::SetupOrModify)
        .unwrap();
    assert!(rabs.complete(first, authority, &mut env.receive).is_err());
    core.input.borrow_mut().push_back(Input::Closed);
    core.drive();
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (1, 0));
    assert!(rabs.cancel(second).is_err());
    assert!(
        core.gateway
            .authorize_rab(replacement, id, RabOperation::SetupOrModify)
            .is_err()
    );
    let result = rabs.reconcile_core(&core.gateway).unwrap();
    assert_eq!((result.pending_cancelled, result.active_retired), (0, 0));
}
