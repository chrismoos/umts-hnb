// SPDX-License-Identifier: AGPL-3.0-or-later

use super::super::super as control;
use super::*;
mod attempts;
mod initial;
mod reconciliation;
use crate::{
    dedicated_bearers::{SduDelivery, UpperSdu},
    packet_bearers::PacketBearers,
    packet_rabs::{self, PacketRabs, RadioBinding, Ticket},
};
use std::{collections::VecDeque, io, net::SocketAddr, num::NonZeroU16};
use umts_iuh::{
    ContextId, Domain,
    hnbap::{
        self, AccessMode, Capabilities, CsgCapability, HnbParameters, Imsi, RegistrationCause,
        Release, UeIdentity,
    },
    ranap::rab::{
        Address, Assignment, AssignmentResponse, Association, First, Mode, Peer, Second, Setup,
        SetupOrModify, Transport, UserPlane, qos,
    },
    rua,
    session::{
        AssociationId, ControlTime, Event, RabOperation, RabRequest, Session, SessionLimits,
        UplinkNas,
    },
    wire::Limits as WireLimits,
};
use umts_user_plane::{
    gtpu::{self, MessageType, PeerTeid},
    packet::{BoundDatagrams, Datagrams, Limits as PacketLimits, PacketPlane, RouteKey},
};

const GTP_PORT: u16 = 2152;
const CONTEXT: u32 = 42;
const PACKET_RATE: u32 = 384_000;
const NORMAL_RELEASE: u16 = 83;

#[derive(Default)]
struct Memory {
    incoming: VecDeque<(Vec<u8>, SocketAddr)>,
    outgoing: Vec<(Vec<u8>, SocketAddr)>,
    blocked: bool,
}
impl Datagrams for Memory {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let Some((bytes, peer)) = self.incoming.pop_front() else {
            return Ok(None);
        };
        output[..bytes.len()].copy_from_slice(&bytes);
        Ok(Some((bytes.len(), peer)))
    }
    fn send(&mut self, bytes: &[u8], peer: SocketAddr) -> io::Result<bool> {
        if self.blocked {
            return Ok(false);
        }
        self.outgoing.push((bytes.to_vec(), peer));
        Ok(true)
    }
}
impl BoundDatagrams for Memory {
    fn local_address(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::from(([127, 0, 0, 1], GTP_PORT)))
    }
}

fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 2], GTP_PORT))
}
fn replacement_peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 3], GTP_PORT))
}

fn rabs() -> PacketRabs<Memory> {
    let plane = PacketPlane::new(
        Memory::default(),
        PacketLimits {
            routes: count(2),
            packets_per_route: count(2),
            maximum_payload_bytes: count(1500),
            maximum_storage_bytes: 1_000_000,
            receive_budget: count(2),
            route_visits: count(2),
        },
    )
    .unwrap();
    PacketRabs::new(PacketBearers::new(plane), None, NonZeroU16::new(3).unwrap()).unwrap()
}

fn item(id: u8) -> SetupOrModify {
    let parameters = qos::Parameters::new(
        qos::TrafficClass::Background,
        qos::Asymmetry::SymmetricBidirectional,
        qos::MaximumRates::new(&[PACKET_RATE]).unwrap(),
        qos::DeliveryOrder::Requested,
        qos::MaxSduBits::new(12_000).unwrap(),
        qos::Subflows::new(vec![
            qos::Subflow::new(
                qos::ResidualError::new(1, 6).unwrap(),
                qos::Delivery::No,
                Some(qos::SduError::new(1, 3).unwrap()),
                None,
            )
            .unwrap(),
        ])
        .unwrap(),
    );
    let mut first = First::modification(id);
    first.parameters = Some(parameters);
    first.user_plane = Some(UserPlane::new(Mode::Transparent, 3).unwrap());
    first.transport = Some(Transport::new(
        Address::from_ip(Domain::Ps, peer().ip()).unwrap(),
        Association::gtp(0),
    ));
    let mut second = Second::default();
    second.uplink_gtp_sequence = Some(u16::MAX);
    second.downlink_gtp_sequence = Some(42);
    SetupOrModify { first, second }
}

struct Core {
    session: Session,
    key: UeKey,
    time: u64,
}
impl Core {
    fn new(key: UeKey) -> Self {
        let mut session = Session::new(
            key.epoch,
            AssociationId(NonZeroU64::MIN),
            HnbParameters {
                identity: b"packet bearer integration".to_vec(),
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
        session
            .register_hnb_with(ControlTime(0), |_| Ok::<_, ()>(()))
            .unwrap();
        let accepted = hnbap::Message::HnbAccepted {
            rnc: 1,
            mux_port: None,
        };
        session
            .receive_hnbap_with(
                AssociationId(NonZeroU64::MIN),
                ControlTime(1),
                &hnbap::Message::decode(
                    &accepted.encode(WireLimits::default()).unwrap(),
                    WireLimits::default(),
                )
                .unwrap(),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        let identity = UeIdentity::Imsi(Imsi::from_digits("001010000000001").unwrap());
        session
            .register_ue_with(
                key,
                identity.clone(),
                Capabilities {
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
            context: ContextId::new(CONTEXT).unwrap(),
            membership: None,
        };
        session
            .receive_hnbap_with(
                AssociationId(NonZeroU64::MIN),
                ControlTime(2),
                &hnbap::Message::decode(
                    &accepted.encode(WireLimits::default()).unwrap(),
                    WireLimits::default(),
                )
                .unwrap(),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        for domain in [Domain::Cs, Domain::Ps] {
            session
                .publish_nas_with(
                    UplinkNas {
                        key,
                        domain,
                        nas: &[8, 0x41],
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
            key,
            time: 3,
        }
    }

    fn request(&mut self, item: &SetupOrModify, domain: Domain) -> RabRequest {
        let assignment = Assignment {
            setup: vec![item.clone()],
            release: vec![],
        };
        self.assignment(assignment, domain)
    }

    fn assignment(&mut self, assignment: Assignment, domain: Domain) -> RabRequest {
        self.time += 1;
        let message = rua::Message::Direct {
            domain,
            context: ContextId::new(CONTEXT).unwrap(),
            ranap: assignment.encode(WireLimits::default()).unwrap(),
        };
        let decoded = rua::Message::decode(
            &message.encode(WireLimits::default()).unwrap(),
            WireLimits::default(),
        )
        .unwrap();
        self.session
            .receive_rua_with(
                AssociationId(NonZeroU64::MIN),
                ControlTime(self.time),
                &decoded,
                |event| {
                    let Event::RabAssignment {
                        request,
                        assignment: decoded,
                        ..
                    } = event
                    else {
                        panic!("expected RAB assignment")
                    };
                    assert_eq!(request.key(), self.key);
                    assert_eq!(decoded, &assignment);
                    Ok::<_, ()>(*request)
                },
            )
            .unwrap()
    }

    fn success(&mut self, ticket: Ticket, setup: Setup) {
        self.time += 1;
        self.session
            .publish_rab_with(
                ticket.request(),
                &AssignmentResponse {
                    setup: vec![setup],
                    ..AssignmentResponse::default()
                },
                ControlTime(self.time),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
    }

    fn prepare(
        &mut self,
        rabs: &mut PacketRabs<Memory>,
        item: &SetupOrModify,
        radio: RadioBinding,
    ) -> Result<Ticket, packet_rabs::Error> {
        let request = self.request(item, Domain::Ps);
        let authorization = self
            .session
            .authorize_rab(request, item.first.id.get(), RabOperation::SetupOrModify)
            .map_err(packet_rabs::Error::Protocol)?;
        rabs.prepare(authorization, item, radio)
    }

    fn complete(
        &self,
        rabs: &mut PacketRabs<Memory>,
        ticket: Ticket,
        receive: &mut DedicatedController,
    ) -> Result<Setup, packet_rabs::Error> {
        let authorization = self
            .session
            .authorize_rab(
                ticket.request(),
                ticket.key().rab,
                RabOperation::SetupOrModify,
            )
            .map_err(packet_rabs::Error::Protocol)?;
        rabs.complete(ticket, authorization, receive)
    }
}

fn mapping(
    env: &Environment,
    procedure: &RadioBearerSetupProcedure,
    plan: Arc<CompiledRel99>,
) -> RadioBinding {
    let rab = procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    RadioBinding::from_procedure(env.identity, plan, procedure, rab).unwrap()
}

fn start_radio(env: &mut Environment, request: Box<InitialAdmission>) {
    env.admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap();
    until(env, AdmissionPhase::Prefilling, at(0));
    env.feedback
        .publish(TpcPlan {
            identity: env.identity,
            first_slot: at(0),
            commands: vec![Bit::Zero; 60],
        })
        .ok()
        .unwrap();
    env.transmit.service().unwrap();
    assert!(matches!(
        env.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback { result: Ok(()), .. }
    ));
    until(env, AdmissionPhase::Starting, at(0));
}

fn complete_radio(env: &mut Environment) {
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    for _ in 0..31 {
        assert!(
            env.writer.accepted(
                env.transmit
                    .generate_slot(&mut env.source, &mut samples)
                    .unwrap()
            )
        );
        let now = env.source.next_chip();
        step(env, now);
    }
    let now = env.source.next_chip();
    until(env, AdmissionPhase::AwaitingComplete, now);
    let procedure = env.admission.procedure(env.identity).unwrap();
    let receipt = procedure.receipt().unwrap();
    let transaction = procedure.transaction();
    let mut sender = crate::signalling::tests::peer(&env.source);
    sender.enqueue(&control::COMPLETE).unwrap();
    sender
        .publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    control::publish(&mut env.radio, env.key, receipt, &mut sender);
    let (kind, status) = sender
        .publish_next(now, |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap();
    assert_eq!(kind, umts_rlc::acknowledged::PublicationKind::Status);
    let received = now.index + 100;
    let mut complete = control::user_setup::response(transaction, 3);
    let umts_rrc_schema::class_definitions::ULDCCHMessageType::radioBearerSetupComplete(body) =
        &mut complete
    else {
        unreachable!()
    };
    body.count_c_activation_time = Some(umts_rrc_schema::information_elements::ActivationTime(208));
    control::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut sender,
        &control::clear(complete),
        received,
        ReceiveProvenance::Path(env.identity.configuration),
        Some(at(received)),
    );
    let serviced = at(received + 20);
    assert_eq!(
        env.radio.process_one_at_with_control(
            serviced,
            control::security_time(),
            |received, mut security| env
                .admission
                .response(&received, &mut security, &mut env.receive)
                .unwrap()
        ),
        Some(Ok(env.key))
    );
    step(env, serviced);
    env.radio
        .receive_srb_payload(
            env.key,
            SignallingBearer::ControlAm,
            ReceiveTime::simultaneous(serviced),
            Bits::new(&status, status.len() * BITS_PER_BYTE).unwrap(),
        )
        .unwrap();
    step(env, serviced);
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Active)
    );
    assert_eq!(
        env.admission.procedure(env.identity).unwrap().phase(),
        SetupPhase::Complete
    );
}

fn uplink(rabs: &mut PacketRabs<Memory>, env: &Environment, bytes: &[u8]) {
    let mut bearers = env
        .plan
        .configuration()
        .bearers()
        .iter()
        .filter(|bearer| bearer.domain == Some(CnDomain::Ps));
    let bearer = BearerId::new(bearers.next().unwrap().id).unwrap();
    assert!(bearers.next().is_none());
    let metadata = UpperSdu {
        identity: env.identity,
        bearer,
        scope: Scope::User(CnDomain::Ps),
        received: at(0),
        bits: bytes.len() * BITS_PER_BYTE,
        quality: umts_rlc::transparent::Quality::NotReported,
        coverage: None,
        transparent_origin: None,
    };
    assert!(
        rabs.uplink(SduDelivery {
            mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
            metadata,
            data: Bits::new(bytes, metadata.bits).unwrap()
        })
        .unwrap()
    );
}

fn downlink(teid: u32, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; gtpu::HEADER_BYTES + gtpu::OPTIONAL_BYTES + payload.len()];
    let length = gtpu::encode(
        gtpu::Packet {
            kind: MessageType::Data,
            teid: Some(PeerTeid::new(teid)),
            sequence: Some(sequence),
            n_pdu: None,
            payload,
        },
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}

fn local_teid(setup: &Setup) -> u32 {
    let transport = Transport::new(
        setup.address.clone().unwrap(),
        setup.association.clone().unwrap(),
    );
    let Peer::Ps { address, teid } = transport.ip_peer(Domain::Ps).unwrap() else {
        unreachable!()
    };
    assert_eq!(address, SocketAddr::from(([127, 0, 0, 1], GTP_PORT)).ip());
    assert_ne!(teid, 0);
    teid
}

fn active() -> (Environment, PacketRabs<Memory>, Core, RouteKey, u32) {
    let (mut env, initial) = environment();
    let radio = mapping(&env, &initial.procedure, env.plan.clone());
    let id = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    let mut core = Core::new(env.key);
    let mut rabs = rabs();
    let item = item(id);
    let ticket = core.prepare(&mut rabs, &item, radio).unwrap();
    start_radio(&mut env, initial);
    complete_radio(&mut env);
    let setup = core.complete(&mut rabs, ticket, &mut env.receive).unwrap();
    let teid = local_teid(&setup);
    core.success(ticket, setup);
    (env, rabs, core, ticket.key(), teid)
}

#[test]
fn peer_error_indication_reports_the_active_rab_once_only_when_enabled() {
    // `item` gives the core-side tunnel TEID zero.
    const PEER_TEID: u32 = 0;
    for enabled in [false, true] {
        let (mut env, rabs, _core, key, _) = active();
        let mut rabs = rabs.with_error_indication_release(enabled);
        let mut indication = [0_u8; 64];
        let size = gtpu::error_indication(PEER_TEID, peer().ip(), 1, &mut indication).unwrap();
        rabs.io_mut()
            .incoming
            .push_back((indication[..size].to_vec(), peer()));
        rabs.step(&mut env.receive).unwrap();
        assert_eq!(rabs.counters().error_indications_received, 1);
        let mut failed = Vec::new();
        rabs.take_peer_failures(|route| {
            failed.push(route);
            true
        });
        rabs.take_peer_failures(|route| {
            failed.push(route);
            true
        });
        assert_eq!(failed, if enabled { vec![key] } else { Vec::new() });
    }
}

#[test]
fn setup_stays_pending_until_original_radio_and_cipher_owners_complete() {
    let (mut env, initial) = environment();
    let id = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    let radio = mapping(&env, &initial.procedure, env.plan.clone());
    let mut core = Core::new(env.key);
    let mut rabs = rabs();
    let item = item(id);
    let request = core.request(&item, Domain::Ps);
    let authorization = core
        .session
        .authorize_rab(request, id, RabOperation::SetupOrModify)
        .unwrap();
    let ticket = rabs.prepare(authorization, &item, radio).unwrap();
    let packet_mapping = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap();
    let early = SduDelivery {
        mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
        metadata: UpperSdu {
            identity: env.identity,
            bearer: BearerId::new(packet_mapping.bearers[0]).unwrap(),
            scope: Scope::User(CnDomain::Ps),
            received: at(0),
            bits: 8,
            quality: umts_rlc::transparent::Quality::NotReported,
            coverage: None,
            transparent_origin: None,
        },
        data: Bits::new(&[0xa5], 8).unwrap(),
    };
    assert!(!rabs.uplink(early).unwrap());
    rabs.step(&mut env.receive).unwrap();
    assert!(rabs.io_mut().outgoing.is_empty());
    assert!(core.complete(&mut rabs, ticket, &mut env.receive).is_err());
    assert!(rabs.requested(ticket.key()).is_none());
    assert!(matches!(
        rabs.prepare(
            core.session
                .authorize_rab(request, id, RabOperation::SetupOrModify)
                .unwrap(),
            &item,
            mapping(&env, &initial.procedure, env.plan.clone())
        ),
        Err(packet_rabs::Error::Conflict)
    ));
    start_radio(&mut env, initial);
    assert!(core.complete(&mut rabs, ticket, &mut env.receive).is_err());
    assert!(
        !env.receive
            .with_active_bank(env.identity, |bank| bank.user_bearers_ready())
            .unwrap()
    );
    complete_radio(&mut env);
    let setup = core.complete(&mut rabs, ticket, &mut env.receive).unwrap();
    let teid = local_teid(&setup);
    core.success(ticket, setup);
    let state = rabs.sequence_state(ticket.key()).unwrap();
    assert_eq!(
        (state.next_uplink, state.next_downlink),
        (u16::MAX, Some(42))
    );
    uplink(&mut rabs, &env, b"uplink");
    rabs.io_mut()
        .incoming
        .push_back((downlink(teid, 42, b"downlink"), peer()));
    let result = rabs.step(&mut env.receive).unwrap();
    assert_eq!(result.transport.delivered, 1);
    assert_eq!(result.radio_deferred, 0);
    let sent = gtpu::decode(&rabs.io_mut().outgoing[0].0).unwrap();
    assert_eq!(
        (sent.teid, sent.sequence, sent.payload),
        (Some(PeerTeid::new(0)), Some(u16::MAX), b"uplink".as_slice())
    );
    let state = rabs.sequence_state(ticket.key()).unwrap();
    assert_eq!((state.next_uplink, state.next_downlink), (0, Some(43)));
    rabs.io_mut()
        .incoming
        .push_back((downlink(teid, 99, b"wrong peer"), replacement_peer()));
    assert_eq!(rabs.step(&mut env.receive).unwrap().transport.delivered, 0);
    assert_eq!(
        rabs.sequence_state(ticket.key()).unwrap().next_downlink,
        Some(43)
    );
}

#[test]
fn cancel_new_reclaims_the_mapping_and_invalidates_its_ticket() {
    let (mut env, initial) = environment();
    let id = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    let item = item(id);
    let mut core = Core::new(env.key);
    let mut rabs = rabs();
    let first = core
        .prepare(
            &mut rabs,
            &item,
            mapping(&env, &initial.procedure, env.plan.clone()),
        )
        .unwrap();
    rabs.cancel(first).unwrap();
    assert!(rabs.cancel(first).is_err());
    assert!(core.complete(&mut rabs, first, &mut env.receive).is_err());
    let second = core
        .prepare(
            &mut rabs,
            &item,
            mapping(&env, &initial.procedure, env.plan.clone()),
        )
        .unwrap();
    assert_ne!(first, second);
    rabs.cancel(second).unwrap();
    env.radio.retire_frame_pool(initial.pool).unwrap();
}

#[test]
fn cancelling_modification_preserves_the_evolved_sequence_and_queued_uplink() {
    let (mut env, mut rabs, mut core, key, _) = active();
    uplink(&mut rabs, &env, b"first");
    rabs.step(&mut env.receive).unwrap();
    assert_eq!(rabs.sequence_state(key).unwrap().next_uplink, 0);
    let original = rabs.requested(key).unwrap().clone();
    uplink(&mut rabs, &env, b"retained");
    let mut modification = SetupOrModify {
        first: First::modification(key.rab),
        second: Second::default(),
    };
    modification.first.transport = Some(Transport::new(
        Address::from_ip(Domain::Ps, replacement_peer().ip()).unwrap(),
        Association::gtp(44),
    ));
    modification.second.uplink_gtp_sequence = Some(111);
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let ticket = core.prepare(&mut rabs, &modification, radio).unwrap();
    rabs.cancel(ticket).unwrap();
    assert_eq!(rabs.requested(key), Some(&original));
    assert_eq!(rabs.sequence_state(key).unwrap().next_uplink, 0);
    assert!(core.complete(&mut rabs, ticket, &mut env.receive).is_err());
    rabs.step(&mut env.receive).unwrap();
    let (bytes, destination) = &rabs.io_mut().outgoing[1];
    assert_eq!(*destination, peer());
    let packet = gtpu::decode(bytes).unwrap();
    assert_eq!(
        (packet.teid, packet.sequence, packet.payload),
        (Some(PeerTeid::new(0)), Some(0), b"retained".as_slice())
    );
    let mut invalid = modification;
    invalid.first.user_plane = Some(UserPlane::new(Mode::PredefinedSdu, 3).unwrap());
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    assert!(core.prepare(&mut rabs, &invalid, radio).is_err());
    assert_eq!(rabs.requested(key), Some(&original));
    assert_eq!(rabs.sequence_state(key).unwrap().next_uplink, 1);
}

#[test]
fn absent_network_only_modification_fields_do_not_rewind_live_sequence_state() {
    let (mut env, mut rabs, mut core, key, teid) = active();
    uplink(&mut rabs, &env, b"first");
    rabs.io_mut()
        .incoming
        .push_back((downlink(teid, 42, b"first downlink"), peer()));
    rabs.step(&mut env.receive).unwrap();
    let original = rabs.requested(key).unwrap().clone();
    uplink(&mut rabs, &env, b"queued across modification");
    let modification = SetupOrModify {
        first: First::modification(key.rab),
        second: Second::default(),
    };
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let ticket = core.prepare(&mut rabs, &modification, radio).unwrap();
    let setup = core.complete(&mut rabs, ticket, &mut env.receive).unwrap();
    assert_eq!(local_teid(&setup), teid);
    core.success(ticket, setup);
    assert!(
        core.session
            .authorize_rab(ticket.request(), key.rab, RabOperation::SetupOrModify)
            .is_err()
    );
    assert_eq!(rabs.requested(key), Some(&original));
    let state = rabs.sequence_state(key).unwrap();
    assert_eq!((state.next_uplink, state.next_downlink), (0, Some(43)));
    rabs.step(&mut env.receive).unwrap();
    let packet = gtpu::decode(&rabs.io_mut().outgoing[1].0).unwrap();
    assert_eq!(packet.sequence, Some(0));
    assert_eq!(packet.payload, b"queued across modification");
    assert_eq!(rabs.sequence_state(key).unwrap().next_uplink, 1);
}

#[test]
fn wrong_request_scope_and_noncanonical_plan_cannot_replace_working_mapping() {
    let (mut env, mut rabs, mut core, key, _) = active();
    let original = rabs.requested(key).unwrap().clone();
    let modification = SetupOrModify {
        first: First::modification(key.rab),
        second: Second::default(),
    };
    let procedure = env.admission.procedure(env.identity).unwrap();
    let wrong_domain = core.request(&modification, Domain::Cs);
    assert!(matches!(
        rabs.prepare(
            core.session
                .authorize_rab(wrong_domain, key.rab, RabOperation::SetupOrModify)
                .unwrap(),
            &modification,
            mapping(&env, procedure, env.plan.clone())
        ),
        Err(packet_rabs::Error::Identity)
    ));
    let mut foreign_key = env.key;
    foreign_key.generation = NonZeroU64::new(env.key.generation.get() + 1).unwrap();
    let mut foreign_core = Core::new(foreign_key);
    let wrong_owner = foreign_core.request(&modification, Domain::Ps);
    assert!(matches!(
        rabs.prepare(
            foreign_core
                .session
                .authorize_rab(wrong_owner, key.rab, RabOperation::SetupOrModify)
                .unwrap(),
            &modification,
            mapping(&env, procedure, env.plan.clone())
        ),
        Err(packet_rabs::Error::Identity)
    ));
    let noncanonical = Arc::new(
        CompiledRel99::new(
            env.plan.configuration().clone(),
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    );
    let wrong_plan = mapping(&env, procedure, noncanonical);
    assert!(core.prepare(&mut rabs, &modification, wrong_plan).is_err());
    assert_eq!(rabs.requested(key), Some(&original));
    uplink(&mut rabs, &env, b"still working");
    rabs.step(&mut env.receive).unwrap();
    assert_eq!(
        gtpu::decode(&rabs.io_mut().outgoing[0].0).unwrap().payload,
        b"still working"
    );
}

#[test]
fn release_between_prepare_and_complete_revokes_the_old_commit_authority() {
    let (mut env, mut rabs, mut core, key, _) = active();
    let original = rabs.requested(key).unwrap().clone();
    let mut modification = SetupOrModify {
        first: First::modification(key.rab),
        second: Second::default(),
    };
    modification.first.transport = Some(Transport::new(
        Address::from_ip(Domain::Ps, replacement_peer().ip()).unwrap(),
        Association::gtp(44),
    ));
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let ticket = core.prepare(&mut rabs, &modification, radio).unwrap();
    let release = core.assignment(
        Assignment {
            setup: vec![],
            release: vec![umts_iuh::ranap::rab::Release::new(
                key.rab,
                umts_iuh::ranap::rab::Cause::from_code(NORMAL_RELEASE).unwrap(),
            )],
        },
        Domain::Ps,
    );
    assert!(
        core.session
            .authorize_rab(ticket.request(), key.rab, RabOperation::SetupOrModify)
            .is_err()
    );
    let release_authority = core
        .session
        .authorize_rab(release, key.rab, RabOperation::Release)
        .unwrap();
    assert!(matches!(
        rabs.complete(ticket, release_authority, &mut env.receive),
        Err(packet_rabs::Error::Identity)
    ));
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let release_authority = core
        .session
        .authorize_rab(release, key.rab, RabOperation::Release)
        .unwrap();
    assert!(matches!(
        rabs.prepare(release_authority, &modification, radio),
        Err(packet_rabs::Error::Identity)
    ));
    assert_eq!(rabs.requested(key), Some(&original));
    assert_eq!(rabs.sequence_state(key).unwrap().next_uplink, u16::MAX);
    rabs.cancel(ticket).unwrap();
    assert!(rabs.release_transport(key).unwrap().is_some());
    assert!(rabs.requested(key).is_none());
}

#[test]
fn current_authority_cannot_cross_sessions_rabs_or_ticket_serials() {
    let (mut env, mut rabs, mut core, key, _) = active();
    let original = rabs.requested(key).unwrap().clone();
    let modification = SetupOrModify {
        first: First::modification(key.rab),
        second: Second::default(),
    };
    let mut foreign = Core::new(env.key);
    assert_eq!(foreign.session.association(), core.session.association());
    let foreign_request = foreign.request(&modification, Domain::Ps);
    let foreign_authority = foreign
        .session
        .authorize_rab(foreign_request, key.rab, RabOperation::SetupOrModify)
        .unwrap();
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    assert!(matches!(
        rabs.prepare(foreign_authority, &modification, radio),
        Err(packet_rabs::Error::Identity)
    ));
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let ticket = core.prepare(&mut rabs, &modification, radio).unwrap();
    let foreign_authority = foreign
        .session
        .authorize_rab(foreign_request, key.rab, RabOperation::SetupOrModify)
        .unwrap();
    assert!(matches!(
        rabs.complete(ticket, foreign_authority, &mut env.receive),
        Err(packet_rabs::Error::Identity)
    ));
    assert!(
        core.session
            .authorize_rab(ticket.request(), key.rab + 1, RabOperation::SetupOrModify)
            .is_err()
    );
    assert!(
        core.session
            .authorize_rab(ticket.request(), key.rab, RabOperation::Release)
            .is_err()
    );
    let other = SetupOrModify {
        first: First::modification(key.rab + 1),
        second: Second::default(),
    };
    let other_request = core.request(&other, Domain::Ps);
    let other_authority = core
        .session
        .authorize_rab(other_request, key.rab + 1, RabOperation::SetupOrModify)
        .unwrap();
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    assert!(matches!(
        rabs.prepare(other_authority, &modification, radio),
        Err(packet_rabs::Error::Identity)
    ));
    let other_authority = core
        .session
        .authorize_rab(other_request, key.rab + 1, RabOperation::SetupOrModify)
        .unwrap();
    assert!(matches!(
        rabs.complete(ticket, other_authority, &mut env.receive),
        Err(packet_rabs::Error::Identity)
    ));
    assert_eq!(rabs.requested(key), Some(&original));
    let setup = core.complete(&mut rabs, ticket, &mut env.receive).unwrap();
    core.success(ticket, setup);
    assert!(
        core.session
            .authorize_rab(ticket.request(), key.rab, RabOperation::SetupOrModify)
            .is_err()
    );
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let replacement = core.prepare(&mut rabs, &modification, radio).unwrap();
    let replacement_authority = core
        .session
        .authorize_rab(replacement.request(), key.rab, RabOperation::SetupOrModify)
        .unwrap();
    assert!(matches!(
        rabs.complete(ticket, replacement_authority, &mut env.receive),
        Err(packet_rabs::Error::Identity)
    ));
    rabs.cancel(replacement).unwrap();
    uplink(&mut rabs, &env, b"authority preserved");
    rabs.step(&mut env.receive).unwrap();
    let packet = gtpu::decode(&rabs.io_mut().outgoing[0].0).unwrap();
    assert_eq!(packet.sequence, Some(u16::MAX));
    assert_eq!(packet.payload, b"authority preserved");
}

#[test]
fn cancelled_attempt_cannot_cancel_or_complete_a_retry_of_the_same_live_request() {
    let (mut env, mut rabs, mut core, key, _) = active();
    let original = rabs.requested(key).unwrap().clone();
    let modification = SetupOrModify {
        first: First::modification(key.rab),
        second: Second::default(),
    };
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let first = core.prepare(&mut rabs, &modification, radio).unwrap();
    rabs.cancel(first).unwrap();
    let radio = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let authority = core
        .session
        .authorize_rab(first.request(), key.rab, RabOperation::SetupOrModify)
        .unwrap();
    let second = rabs.prepare(authority, &modification, radio).unwrap();
    assert_eq!(first.request(), second.request());
    assert_eq!(first.key(), second.key());
    assert_ne!(first, second);
    assert!(rabs.cancel(first).is_err());
    assert!(core.complete(&mut rabs, first, &mut env.receive).is_err());
    assert_eq!(rabs.requested(key), Some(&original));
    let setup = core.complete(&mut rabs, second, &mut env.receive).unwrap();
    core.success(second, setup);
    uplink(&mut rabs, &env, b"retry preserved");
    rabs.step(&mut env.receive).unwrap();
    let packet = gtpu::decode(&rabs.io_mut().outgoing[0].0).unwrap();
    assert_eq!(packet.sequence, Some(u16::MAX));
    assert_eq!(packet.payload, b"retry preserved");
}

#[test]
fn peer_error_indication_waits_for_prepared_rab_activation_and_acceptance() {
    let (mut env, initial) = environment();
    let radio = mapping(&env, &initial.procedure, env.plan.clone());
    let id = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    let mut core = Core::new(env.key);
    let mut rabs = rabs().with_error_indication_release(true);
    let ticket = core.prepare(&mut rabs, &item(id), radio).unwrap();
    let mut indication = [0_u8; 64];
    let size = gtpu::error_indication(0, peer().ip(), 1, &mut indication).unwrap();
    rabs.io_mut()
        .incoming
        .push_back((indication[..size].to_vec(), peer()));
    rabs.step(&mut env.receive).unwrap();
    rabs.take_peer_failures(|_| panic!("prepared route cannot request release yet"));
    start_radio(&mut env, initial);
    complete_radio(&mut env);
    let setup = core.complete(&mut rabs, ticket, &mut env.receive).unwrap();
    core.success(ticket, setup);
    let mut deferred = Vec::new();
    rabs.take_peer_failures(|key| {
        deferred.push(key);
        false
    });
    assert_eq!(deferred, [ticket.key()]);
    let mut accepted = Vec::new();
    rabs.take_peer_failures(|key| {
        accepted.push(key);
        true
    });
    rabs.take_peer_failures(|_| panic!("accepted notification must not repeat"));
    assert_eq!(accepted, [ticket.key()]);
}
