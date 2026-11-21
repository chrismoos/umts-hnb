// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    signalling::tests::{INITIAL, controller, establish, peer as radio_peer, send},
    test_support::{at, count, source},
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    num::{NonZeroU32, NonZeroU64},
    rc::Rc,
};
use umts_iuh::{
    ContextId, Domain,
    hnbap::{self, AccessMode, HnbParameters},
    ranap,
    session::{AssociationId, Session, SessionLimits},
    wire::{Limits, Protocol},
};
use umts_sctp::{Metadata, Notification, Receive, SendResult};
use umts_types::{CnDomain, UeKey};

mod common_identity;
mod paging;
mod rab_outcomes;
pub(crate) mod security;

#[test]
fn stale_uplink_nas_is_consumed_without_blocking_the_next_ue() {
    use umts_rrc::nas::{CoreConnection, CoreNas, CoreNasKind};
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (first, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let (second, peer_context, _) = register(&mut link, &core, &mut radio, &source, 1, 6);
    for (key, context) in [(first, context.get() + 1), (second, peer_context.get())] {
        radio
            .core()
            .push_nas(
                CoreNas {
                    connection: CoreConnection {
                        key: umts_types::ConnectionKey {
                            ue: key,
                            domain: CnDomain::Cs,
                        },
                        context: CoreContextId::new(context).unwrap(),
                    },
                    kind: CoreNasKind::Initial,
                    initial: None,
                    nas: &[1],
                },
                false,
            )
            .unwrap();
    }
    let stale = drive(&mut link, &mut radio, 9);
    assert!(stale.uplink_rejected);
    assert!(drive(&mut link, &mut radio, 10).uplink_published);
    assert!(
        matches!(core.last_rua(), rua::Message::Direct { context, .. } if context == peer_context)
    );
}

#[test]
fn newer_access_releases_use_the_core_release_eight_and_later_category() {
    for value in [
        AccessRelease::R8,
        AccessRelease::R9,
        AccessRelease::R10,
        AccessRelease::R11,
        AccessRelease::R12,
        AccessRelease::R13,
        AccessRelease::R14,
        AccessRelease::R15,
        AccessRelease::R16,
    ] {
        assert_eq!(release(value), Release::R8AndLater);
    }
}

enum Input {
    Packet(Protocol, Vec<u8>),
    Notification(Notification),
}
#[derive(Default)]
struct Shared {
    input: VecDeque<Input>,
    sent: Vec<(u32, Vec<u8>)>,
    blocked: bool,
}
#[derive(Clone)]
pub(crate) struct Peer(Rc<RefCell<Shared>>);
pub(crate) struct Mock {
    shared: Peer,
    held: Option<Input>,
}
impl Transport for Mock {
    type Error = ();
    fn complete_connect(&mut self) -> Result<bool, ()> {
        Ok(true)
    }
    fn receive_once(&mut self) -> Result<Receive<'_>, ()> {
        if self.held.is_none() {
            self.held = self.shared.0.borrow_mut().input.pop_front();
        }
        Ok(match &self.held {
            None => Receive::Idle,
            Some(Input::Notification(value)) => Receive::Notification(*value),
            Some(Input::Packet(protocol, bytes)) => Receive::Record {
                metadata: Metadata {
                    stream: 0,
                    sequence: 0,
                    ppid: protocol.ppid().unwrap(),
                    association: 42,
                },
                bytes,
            },
        })
    }
    fn acknowledge_receive(&mut self) -> Result<(), ()> {
        assert!(self.held.take().is_some());
        Ok(())
    }
    fn send(&mut self, _: u16, ppid: u32, bytes: &[u8]) -> Result<SendResult, ()> {
        let mut state = self.shared.0.borrow_mut();
        if state.blocked {
            return Ok(SendResult::Retry);
        }
        state.sent.push((ppid, bytes.to_vec()));
        Ok(SendResult::Sent)
    }
}
impl Peer {
    pub(crate) fn set_blocked(&self, blocked: bool) {
        self.0.borrow_mut().blocked = blocked;
    }
    pub(crate) fn rua_packet(&self, bytes: Vec<u8>) {
        self.0
            .borrow_mut()
            .input
            .push_back(Input::Packet(Protocol::Rua, bytes));
    }
    pub(crate) fn hnbap(&self, message: hnbap::Message) {
        self.0.borrow_mut().input.push_back(Input::Packet(
            Protocol::Hnbap,
            message.encode(Limits::default()).unwrap(),
        ));
    }
    pub(crate) fn rua(&self, message: rua::Message) {
        self.0.borrow_mut().input.push_back(Input::Packet(
            Protocol::Rua,
            message.encode(Limits::default()).unwrap(),
        ));
    }
    pub(crate) fn last_hnbap(&self) -> hnbap::Message {
        let state = self.0.borrow();
        let (protocol, bytes) = state.sent.last().unwrap();
        assert_eq!(*protocol, Protocol::Hnbap.ppid().unwrap());
        hnbap::Message::decode(bytes, Limits::default())
            .unwrap()
            .message
    }
    pub(crate) fn last_rua(&self) -> rua::Message {
        let state = self.0.borrow();
        let (protocol, bytes) = state.sent.last().unwrap();
        assert_eq!(*protocol, Protocol::Rua.ppid().unwrap());
        rua::Message::decode(bytes, Limits::default())
            .unwrap()
            .message
    }
}
fn drive(link: &mut CoreLink<Mock>, radio: &mut CommonSignalling, now: u64) -> Step {
    link.drive(ControlTime(now), radio, |_, _| {
        Err::<(), _>("unhandled event")
    })
    .unwrap()
}

pub(crate) fn finish_peer_disconnect(
    link: &mut CoreLink<Mock>,
    radio: &mut CommonSignalling,
    now: u64,
) {
    let mut request = None;
    link.drive(ControlTime(now), radio, |_, event| {
        let Event::Disconnected { request: owner, .. } = event else {
            panic!("disconnect owner required")
        };
        request = Some(*owner);
        Ok::<_, ()>(())
    })
    .unwrap();
    link.gateway_mut()
        .complete_iu_release(ControlTime(now), request.unwrap(), &[])
        .unwrap();
    radio.finish_core_connection_release(request.unwrap());
}
fn established_peer() -> Peer {
    let peer = Peer(Rc::new(RefCell::new(Shared::default())));
    peer.0
        .borrow_mut()
        .input
        .push_back(Input::Notification(Notification::Established {
            inbound_streams: 2,
            outbound_streams: 2,
            association: 42,
        }));
    peer
}
fn mock_gateway(peer: &Peer, association: NonZeroU64, now: ControlTime) -> Gateway<Mock> {
    let source = source(false);
    let session = Session::new(
        source.next_chip().epoch,
        AssociationId(association),
        HnbParameters {
            identity: b"Rust controller gateway test".to_vec(),
            plmn: [0, 0xf1, 0x10],
            lac: 23,
            rac: 7,
            sac: 42,
            cell: 0x1234567,
            access: AccessMode::Open,
            csg: None,
        },
        SessionLimits {
            maximum_rab_requests_per_domain: std::num::NonZeroUsize::new(4).unwrap(),
            maximum_rab_release_requests_per_domain: std::num::NonZeroUsize::new(2).unwrap(),
            maximum_ues: count(8),
            request_timeout_ms: NonZeroU64::new(1000).unwrap(),
            codec: Limits::default(),
        },
        now,
    )
    .unwrap();
    Gateway::new(
        session,
        Mock {
            shared: peer.clone(),
            held: None,
        },
        umts_core::Limits {
            outbound_entries: count(8),
            maximum_packet_bytes: count(65536),
            outbound_byte_budget: count(8 * 65536),
            connect_timeout_ms: NonZeroU64::new(100).unwrap(),
        },
        now,
    )
    .unwrap()
}
fn accept_registration(
    link: &mut CoreLink<Mock>,
    peer: &Peer,
    radio: &mut CommonSignalling,
    now: u64,
    rnc: u16,
) {
    drive(link, radio, now);
    drive(link, radio, now + 1);
    assert!(matches!(peer.last_hnbap(), hnbap::Message::HnbRegister(_)));
    peer.hnbap(hnbap::Message::HnbAccepted {
        rnc,
        mux_port: None,
    });
    drive(link, radio, now + 2);
    assert_eq!(link.gateway().phase(), umts_core::Phase::Serving);
}
pub(crate) fn ready(radio: &mut CommonSignalling) -> (CoreLink<Mock>, Peer) {
    let peer = established_peer();
    let mut link = CoreLink::new(mock_gateway(&peer, NonZeroU64::MIN, ControlTime(0)));
    accept_registration(&mut link, &peer, radio, 0, 1);
    (link, peer)
}
fn ready_with_reconnect(
    radio: &mut CommonSignalling,
    reconnect: Reconnect<Mock>,
) -> (CoreLink<Mock>, Peer) {
    let peer = established_peer();
    let mut link = CoreLink::with_reconnect(
        mock_gateway(&peer, NonZeroU64::MIN, ControlTime(0)),
        reconnect,
    );
    accept_registration(&mut link, &peer, radio, 0, 1);
    (link, peer)
}
fn shut_down_peer(link: &mut CoreLink<Mock>, peer: &Peer, radio: &mut CommonSignalling) -> u64 {
    peer.0
        .borrow_mut()
        .input
        .push_back(Input::Notification(Notification::Shutdown {
            association: 42,
        }));
    for now in 3..10 {
        drive(link, radio, now);
        if link.gateway().phase() == umts_core::Phase::Closed {
            return now;
        }
    }
    panic!("peer shutdown did not close the association");
}

const RECONNECT_BACKOFF_MS: u64 = 10;

#[test]
fn closed_association_reconnects_after_the_backoff_and_registers_again() {
    let source = source(false);
    let mut radio = controller(&source);
    let peers = Rc::new(RefCell::new(Vec::<Peer>::new()));
    let created = peers.clone();
    let reconnect = Reconnect::new(
        None,
        RECONNECT_BACKOFF_MS,
        Box::new(
            move |now: ControlTime, generation: u64| -> Result<Gateway<Mock>, String> {
                let peer = established_peer();
                created.borrow_mut().push(peer.clone());
                Ok(mock_gateway(
                    &peer,
                    NonZeroU64::new(generation + 1).unwrap(),
                    now,
                ))
            },
        ),
    );
    let (mut link, core) = ready_with_reconnect(&mut radio, reconnect);
    let closed = shut_down_peer(&mut link, &core, &mut radio);
    drive(&mut link, &mut radio, closed + RECONNECT_BACKOFF_MS - 1);
    assert!(peers.borrow().is_empty());
    assert_eq!(link.gateway().phase(), umts_core::Phase::Closed);
    drive(&mut link, &mut radio, closed + RECONNECT_BACKOFF_MS);
    assert_eq!(peers.borrow().len(), 1);
    assert_eq!(link.gateway().phase(), umts_core::Phase::Connecting);
    let fresh = peers.borrow()[0].clone();
    accept_registration(
        &mut link,
        &fresh,
        &mut radio,
        closed + RECONNECT_BACKOFF_MS + 1,
        2,
    );
    assert_eq!(radio.rnc(), 2);
    assert_eq!(link.reconnect.as_ref().unwrap().failures(), 0);
    assert!(!link.reconnect_exhausted());
}

#[test]
fn reconnect_stops_after_the_configured_attempts() {
    let source = source(false);
    let mut radio = controller(&source);
    let attempts = Rc::new(std::cell::Cell::new(0_u32));
    let counted = attempts.clone();
    let reconnect = Reconnect::new(
        NonZeroU32::new(2),
        RECONNECT_BACKOFF_MS,
        Box::new(
            move |_: ControlTime, _: u64| -> Result<Gateway<Mock>, String> {
                counted.set(counted.get() + 1);
                Err("HNB-GW unreachable".to_owned())
            },
        ),
    );
    let (mut link, core) = ready_with_reconnect(&mut radio, reconnect);
    let closed = shut_down_peer(&mut link, &core, &mut radio);
    let first = closed + RECONNECT_BACKOFF_MS;
    drive(&mut link, &mut radio, first);
    assert_eq!(attempts.get(), 1);
    drive(&mut link, &mut radio, first + 1);
    drive(&mut link, &mut radio, first + 1 + RECONNECT_BACKOFF_MS);
    assert_eq!(attempts.get(), 2);
    assert!(!link.reconnect_exhausted());
    drive(&mut link, &mut radio, first + 2 + RECONNECT_BACKOFF_MS);
    assert!(link.reconnect_exhausted());
    drive(&mut link, &mut radio, first + 3 + 10 * RECONNECT_BACKOFF_MS);
    assert_eq!(attempts.get(), 2);
    assert_eq!(link.gateway().phase(), umts_core::Phase::Closed);
}

#[test]
fn link_without_reconnect_stays_closed() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let closed = shut_down_peer(&mut link, &core, &mut radio);
    drive(&mut link, &mut radio, closed + 1000);
    assert_eq!(link.gateway().phase(), umts_core::Phase::Closed);
    assert!(!link.reconnect_exhausted());
}
pub(crate) fn register(
    link: &mut CoreLink<Mock>,
    core: &Peer,
    radio: &mut CommonSignalling,
    source: &crate::common_tx::CommonTx,
    variant: u8,
    now: u64,
) -> (UeKey, ContextId, umts_rlc::acknowledged::AmEntity) {
    let key = establish(radio, source, variant);
    let mut ue = radio_peer(source);
    send(
        radio,
        key,
        umts_rrc::nas::SignallingBearer::NasAm,
        &mut ue,
        &INITIAL,
    );
    assert_eq!(
        radio.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    assert!(drive(link, radio, now).uplink_published);
    let hnbap::Message::UeRegister {
        identity,
        capabilities,
        cause,
    } = core.last_hnbap()
    else {
        panic!("UE registration")
    };
    assert_eq!(capabilities.release, Release::R99);
    assert_eq!(capabilities.csg, CsgCapability::NotCapable);
    assert_eq!(cause, RegistrationCause::Normal);
    let context = ContextId::new(100 + key.slot).unwrap();
    core.hnbap(hnbap::Message::UeAccepted {
        identity,
        context,
        membership: None,
    });
    drive(link, radio, now + 1);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    drive(link, radio, now + 2);
    (key, context, ue)
}

#[test]
fn core_security_waits_for_actual_capabilities_and_times_out_through_the_reserved_reply() {
    use crate::connection_security::Phase as SecurityPhase;
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let command = ranap::SecurityCommand {
        integrity: ranap::Protection {
            algorithms: vec![ranap::Algorithm(0)],
            key: ranap::Key::new([0x11; 16]),
        },
        encryption: None,
        key_status: ranap::KeyStatus::Old,
    };
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::encode_security(&command, Limits::default()).unwrap(),
    });
    drive(&mut link, &mut radio, 6);
    assert_eq!(
        radio.security(key).unwrap().phase(Domain::Cs),
        SecurityPhase::AwaitingPreparation
    );
    assert_eq!(
        radio.security(key).unwrap().phase(Domain::Ps),
        SecurityPhase::Idle
    );
    let mut need_capabilities = false;
    for _ in 0..16 {
        need_capabilities |= radio
            .service_security_one(SecurityTime {
                control: ControlTime(6),
                monotonic: std::time::Instant::now(),
            })
            .unwrap()
            == SecurityWork::NeedCapabilities {
                key,
                domain: Domain::Cs,
            };
    }
    assert!(need_capabilities);
    assert!(radio.security_capabilities(key).unwrap().is_none());
    let mut queued = false;
    for _ in 0..16 {
        queued |= radio
            .service_security_one(SecurityTime {
                control: ControlTime(5006),
                monotonic: std::time::Instant::now(),
            })
            .unwrap()
            == SecurityWork::ReportQueued {
                key,
                domain: Domain::Cs,
            };
    }
    assert!(queued);
    drive(&mut link, &mut radio, 5006);
    let rua::Message::Direct {
        domain,
        context: returned,
        ranap,
    } = core.last_rua()
    else {
        panic!("expected scoped security reject");
    };
    assert_eq!((domain, returned), (Domain::Cs, context));
    assert_eq!(
        ranap::SecurityResponse::decode(&ranap, Limits::default())
            .unwrap()
            .response,
        ranap::SecurityResponse::Reject(ranap::SecurityReject::RadioProcedureFailure)
    );
    assert_eq!(
        radio.security(key).unwrap().phase(Domain::Cs),
        SecurityPhase::Idle
    );
}

#[test]
fn rrc_initial_crosses_actual_aper_registration_and_rua_with_unchanged_nas_and_selector() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let rua::Message::Connect {
        domain,
        context: received,
        ranap,
        selector,
        cause,
        ..
    } = core.last_rua()
    else {
        panic!("RUA connect")
    };
    assert_eq!(domain, Domain::Cs);
    assert_eq!(received, context);
    assert_eq!(cause, rua::EstablishmentCause::Normal);
    assert_eq!(rasn::uper::encode(&selector.unwrap()).unwrap(), [0x20, 0]);
    assert_eq!(
        ranap,
        ranap::initial_ue(
            context,
            Domain::Cs,
            1,
            ranap::Location {
                plmn: [0, 0xf1, 0x10],
                lac: 23,
                rac: 7,
                sac: 42,
            },
            &[5, 0x24],
            Limits::default()
        )
        .unwrap()
    );
    assert_eq!(
        radio.connection(key, CnDomain::Cs).unwrap().context.get(),
        context.get()
    );
    assert!(radio.core().is_empty());
}

#[test]
fn downlink_actual_rua_ranap_reaches_the_same_ue_srb3_bearer() {
    let mut source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::downlink_direct(&[5, 0x31], Some(Priority::Sms), Limits::default()).unwrap(),
    });
    assert!(drive(&mut link, &mut radio, 6).gateway.delivered);
    assert_eq!(
        radio.publish_bearer(
            key,
            umts_rrc::nas::SignallingBearer::NasAm,
            &mut source,
            at(0),
            at(0)
        ),
        Ok(true)
    );
    assert_eq!(radio.registration_failure(key), None);
}

#[test]
fn both_domains_share_context_and_radio_failure_queues_all_three_core_teardowns() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, mut ue) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let mut ps = INITIAL;
    ps[1] |= 0x80;
    send(
        &mut radio,
        key,
        umts_rrc::nas::SignallingBearer::NasAm,
        &mut ue,
        &ps,
    );
    assert_eq!(radio.process_one(), Some(Ok(key)));
    drive(&mut link, &mut radio, 6);
    assert!(
        matches!(core.last_rua(), rua::Message::Connect { domain: Domain::Ps, context: id, .. } if id == context)
    );
    assert_eq!(
        radio.connection(key, CnDomain::Ps).unwrap().context.get(),
        context.get()
    );
    radio.retire(key).unwrap();
    for (now, domain) in [(7, Domain::Cs), (8, Domain::Ps)] {
        drive(&mut link, &mut radio, now);
        assert!(
            matches!(core.last_rua(), rua::Message::Disconnect { domain: received, context: id, ranap: None, .. } if received == domain && id == context)
        );
    }
    drive(&mut link, &mut radio, 9);
    assert!(
        matches!(core.last_hnbap(), hnbap::Message::UeDeregister { context: id, .. } if id == context)
    );
}

#[test]
fn final_domain_release_intent_preserves_other_domain_and_other_ue() {
    use crate::signalling::ConnectionReleaseProgress;
    use umts_rrc::nas::SignallingBearer;

    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, _, mut ue) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let other = crate::signalling::tests::establish(&mut radio, &source, 1);
    let mut ps = INITIAL;
    ps[1] |= 0x80;
    send(&mut radio, key, SignallingBearer::NasAm, &mut ue, &ps);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    drive(&mut link, &mut radio, 6);
    assert_eq!(
        radio.notify_core_connection_release(key, CnDomain::Cs),
        Ok(ConnectionReleaseProgress::Complete)
    );
    assert!(radio.connection(key, CnDomain::Ps).is_some());
    assert!(
        !radio
            .idle_candidate(key.slot, at(0))
            .unwrap()
            .release_after_core_disconnect
    );
    assert_eq!(
        radio.notify_core_connection_release(key, CnDomain::Ps),
        Ok(ConnectionReleaseProgress::Complete)
    );
    assert!(
        radio
            .idle_candidate(key.slot, at(0))
            .unwrap()
            .release_after_core_disconnect
    );
    assert!(
        !radio
            .idle_candidate(other.slot, at(0))
            .unwrap()
            .release_after_core_disconnect
    );
    send(&mut radio, key, SignallingBearer::NasAm, &mut ue, &INITIAL);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    assert!(radio.connection(key, CnDomain::Cs).is_some());
    assert_eq!(
        radio.release_idle_rrc(
            key,
            umts_rrc::mobility::ReleaseCompleteRetries::new(1).unwrap()
        ),
        Err(SignallingError::Phase)
    );
}

#[test]
fn association_loss_invalidates_active_and_pending_core_registration() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (first, _, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let second = establish(&mut radio, &source, 1);
    send(
        &mut radio,
        second,
        umts_rrc::nas::SignallingBearer::NasAm,
        &mut radio_peer(&source),
        &INITIAL,
    );
    assert_eq!(
        radio.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    core.0
        .borrow_mut()
        .input
        .push_back(Input::Notification(Notification::Shutdown {
            association: 42,
        }));
    drive(&mut link, &mut radio, 6);
    drive(&mut link, &mut radio, 7);
    assert_eq!(
        radio.registration_failure(first),
        Some(RegistrationFailure::CoreUnavailable)
    );
    assert_eq!(
        radio.registration_failure(second),
        Some(RegistrationFailure::CoreUnavailable)
    );
    assert!(radio.core().is_empty());
    assert!(radio.connection(first, CnDomain::Cs).is_none());
    assert_eq!(link.gateway().phase(), umts_core::Phase::Closed);
}

#[test]
fn unsupported_events_require_explicit_consumer_admission() {
    let source = source(false);
    let mut radio = controller(&source);
    let event = Event::AssociationLost {
        owners: Vec::new(),
        backoff_until: None,
    };
    dispatch(&mut radio, &event, ControlTime(0), &mut |_, _| {
        Err::<(), _>(7)
    })
    .unwrap();
    let event = Event::HnbRejected {
        cause: hnbap::Cause::Radio(hnbap::RadioCause::Unspecified),
        backoff_until: None,
    };
    assert!(matches!(
        dispatch(
            &mut radio,
            &event,
            ControlTime(0),
            &mut |_, _| Err::<(), _>(7)
        ),
        Err(DispatchError::Other(7))
    ));
}

#[test]
fn every_fixed_width_idnns_value_survives_rrc_to_rua_conversion() {
    for word in 0..=u16::MAX {
        let bytes = word.to_be_bytes();
        let rrc = rasn::uper::decode(&bytes).unwrap();
        let converted = selector::convert(&rrc).unwrap();
        assert_eq!(rasn::uper::encode(&converted).unwrap(), bytes);
    }
}

#[test]
fn loss_cleans_queued_registration_even_when_gateway_owner_list_is_empty() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    send(
        &mut radio,
        key,
        umts_rrc::nas::SignallingBearer::NasAm,
        &mut radio_peer(&source),
        &INITIAL,
    );
    assert_eq!(
        radio.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    assert_eq!(radio.core().len(), 1);
    dispatch(
        &mut radio,
        &Event::AssociationLost {
            owners: Vec::new(),
            backoff_until: None,
        },
        ControlTime(0),
        &mut |_, _| Err::<(), _>(()),
    )
    .unwrap();
    assert!(radio.core().is_empty());
    assert_eq!(
        radio.registration_failure(key),
        Some(RegistrationFailure::CoreUnavailable)
    );
    assert!(
        radio
            .idle_candidate(key.slot, at(0))
            .unwrap()
            .release_after_core_disconnect
    );
}

#[test]
fn downlink_backpressure_preserves_gateway_envelope_until_srb3_accepts() {
    let mut source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let connection = radio.connection(key, CnDomain::Cs).unwrap();
    for _ in 0..4 {
        radio
            .downlink_nas(connection, DownlinkPriority::Signalling, &[5, 0x31])
            .unwrap();
    }
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::downlink_direct(&[5, 0x31], None, Limits::default()).unwrap(),
    });
    assert!(matches!(
        link.drive(ControlTime(6), &mut radio, |_, _| Err::<(), _>(())),
        Err(Error::Drive(umts_core::DriveError::Upper(
            DispatchError::Radio(SignallingError::Am(crate::common_am::CommonAmError::Rlc(
                umts_rlc::acknowledged::AmError::Capacity
            )))
        )))
    ));
    let decoded = link.gateway().counters().decoded;
    assert_eq!(
        radio.publish_bearer(
            key,
            umts_rrc::nas::SignallingBearer::NasAm,
            &mut source,
            at(0),
            at(0)
        ),
        Ok(true)
    );
    // STATUS publication cannot retire a tracked NAS delivery.
    assert_eq!(
        radio.publish_bearer(
            key,
            umts_rrc::nas::SignallingBearer::NasAm,
            &mut source,
            at(0),
            at(umts_types::time::CHIPS_PER_FRAME),
        ),
        Ok(true)
    );
    assert!(
        link.drive(ControlTime(7), &mut radio, |_, _| Err::<(), _>(()))
            .is_err()
    );
    let mut ack = vec![0; radio_peer(&source).configuration().transmit_pdu_bytes.get()];
    // TS 25.322 STATUS header followed by ACK SUFI with LSN 1.
    const ACK_ONE: [u8; 3] = [0x02, 0x00, 0x10];
    ack[..ACK_ONE.len()].copy_from_slice(&ACK_ONE);
    radio
        .receive_srb_payload(
            key,
            umts_rrc::nas::SignallingBearer::NasAm,
            umts_rlc::acknowledged::ReceiveTime::simultaneous(at(
                umts_types::time::CHIPS_PER_FRAME,
            )),
            umts_types::bits::Bits::new(&ack, ack.len() * umts_types::bits::BITS_PER_BYTE).unwrap(),
        )
        .unwrap();
    assert!(drive(&mut link, &mut radio, 8).gateway.delivered);
    assert_eq!(link.gateway().counters().decoded, decoded);
}

#[test]
fn iu_release_retains_reopening_nas_until_core_completion() {
    use crate::signalling::ConnectionReleaseProgress;
    use umts_rrc::nas::{CoreNas, CoreNasKind, SignallingBearer};
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, mut ue) = register(&mut link, &core, &mut radio, &source, 0, 3);
    core.rua(rua::Message::Disconnect {
        context,
        domain: Domain::Cs,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    let mut request = None;
    link.drive(ControlTime(6), &mut radio, |_, event| {
        let Event::Disconnected { request: owner, .. } = event else {
            panic!("release expected")
        };
        request = Some(*owner);
        Ok::<_, ()>(())
    })
    .unwrap();
    let request = request.unwrap();
    assert_eq!(
        radio.notify_owned_connection_release(request),
        Ok(ConnectionReleaseProgress::Complete)
    );
    let released = umts_rrc::nas::CoreConnection {
        key: umts_types::ConnectionKey {
            ue: key,
            domain: CnDomain::Cs,
        },
        context: CoreContextId::new(context.get()).unwrap(),
    };
    let queued = radio.core().len();
    assert_eq!(
        radio.core().push_nas(
            CoreNas {
                connection: released,
                kind: CoreNasKind::Direct,
                initial: None,
                nas: &[1],
            },
            false,
        ),
        Ok(())
    );
    assert_eq!(radio.core().len(), queued);
    send(&mut radio, key, SignallingBearer::NasAm, &mut ue, &INITIAL);
    assert_eq!(radio.process_one(), Some(Err(SignallingError::Capacity)));
    assert_eq!(radio.process_one(), None);
    assert_eq!(radio.connection(key, CnDomain::Cs), None);
    assert_eq!(
        radio.notify_owned_connection_release(request),
        Ok(ConnectionReleaseProgress::Complete)
    );
    let peer = umts_rrc::nas::CoreConnection {
        key: umts_types::ConnectionKey {
            ue: key,
            domain: CnDomain::Ps,
        },
        context: CoreContextId::new(context.get()).unwrap(),
    };
    radio
        .core()
        .push_nas(
            CoreNas {
                connection: peer,
                kind: CoreNasKind::Initial,
                initial: None,
                nas: &[1],
            },
            false,
        )
        .unwrap();
    radio.core().publish_one(|_| Ok::<_, ()>(())).unwrap();
    link.gateway_mut()
        .complete_iu_release(ControlTime(7), request, &[])
        .unwrap();
    radio.finish_core_connection_release(request);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    assert!(radio.connection(key, CnDomain::Cs).is_some());
    assert!(drive(&mut link, &mut radio, 8).uplink_published);
    assert!(matches!(
        core.last_rua(),
        rua::Message::Connect {
            domain: Domain::Cs,
            ..
        }
    ));
    assert!(matches!(
        radio.notify_owned_connection_release(request),
        Err(SignallingError::Phase)
    ));
    assert!(radio.connection(key, CnDomain::Cs).is_some());
}

#[test]
fn radio_retirement_releases_a_cached_backpressured_downlink_without_closing_association() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let connection = radio.connection(key, CnDomain::Cs).unwrap();
    for _ in 0..4 {
        radio
            .downlink_nas(connection, DownlinkPriority::Signalling, &[5, 0x31])
            .unwrap();
    }
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::downlink_direct(&[5, 0x31], None, Limits::default()).unwrap(),
    });
    assert!(
        link.drive(ControlTime(6), &mut radio, |_, _| Err::<(), _>(()))
            .is_err()
    );
    let decoded = link.gateway().counters().decoded;
    radio.retire(key).unwrap();
    for now in 7..12 {
        drive(&mut link, &mut radio, now);
    }
    assert_eq!(radio.phase(key), None);
    assert_eq!(link.gateway().phase(), umts_core::Phase::Serving);
    assert_eq!(link.gateway().loss_reason(), None);
    assert_eq!(link.gateway().registered_ues(), 0);
    assert_eq!(link.gateway().counters().decoded, decoded);
    assert_eq!(link.gateway().counters().cancelled_inbound, 1);
    assert!(
        matches!(core.last_hnbap(), hnbap::Message::UeDeregister { context: id, .. } if id == context)
    );
}

#[test]
fn retired_pending_registration_acceptance_is_cleaned_without_recreating_radio_owner() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let key = establish(&mut radio, &source, 0);
    send(
        &mut radio,
        key,
        umts_rrc::nas::SignallingBearer::NasAm,
        &mut radio_peer(&source),
        &INITIAL,
    );
    assert_eq!(
        radio.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    drive(&mut link, &mut radio, 3);
    let hnbap::Message::UeRegister { identity, .. } = core.last_hnbap() else {
        panic!("UE register")
    };
    radio.retire(key).unwrap();
    drive(&mut link, &mut radio, 4);
    let context = ContextId::new(77).unwrap();
    core.hnbap(hnbap::Message::UeAccepted {
        identity,
        context,
        membership: None,
    });
    drive(&mut link, &mut radio, 5);
    assert_eq!(link.gateway().registered_ues(), 1);
    drive(&mut link, &mut radio, 6);
    assert_eq!(link.gateway().registered_ues(), 0);
    assert_eq!(link.gateway().counters().cleaned_cancelled, 1);
    drive(&mut link, &mut radio, 7);
    assert!(
        matches!(core.last_hnbap(), hnbap::Message::UeDeregister { context: id, .. } if id == context)
    );
    assert!(radio.phase(key).is_none());
    assert!(radio.core().is_empty());
}

#[test]
fn release_eight_csg_and_emergency_properties_are_preserved_at_wire_admission() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let mut request =
        umts_rrc_codec::decode_uplink_common(&[0x28, 0x90, 1, 1, 0, 0, 0, 0, 1, 0x62, 0x20], 85)
            .unwrap();
    let umts_rrc_schema::class_definitions::ULCCCHMessageType::rrcConnectionRequest(body) =
        &mut request.value.message
    else {
        panic!("request")
    };
    body.establishment_cause =
        umts_rrc_schema::information_elements::EstablishmentCause::emergencyCall;
    let key = radio.request(&source, at(0), &request).unwrap();
    send(
        &mut radio,
        key,
        umts_rrc::nas::SignallingBearer::ControlAm,
        &mut radio_peer(&source),
        &crate::signalling::tests::COMPLETE,
    );
    assert_eq!(radio.process_one(), Some(Ok(key)));
    let mut ue = radio_peer(&source);
    send(
        &mut radio,
        key,
        umts_rrc::nas::SignallingBearer::NasAm,
        &mut ue,
        &[0x15, 0x10, 0, 0, 8, 0x29, 0x24, 0xd1, 0x20],
    );
    assert_eq!(
        radio.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    drive(&mut link, &mut radio, 3);
    let hnbap::Message::UeRegister {
        identity,
        capabilities,
        cause,
    } = core.last_hnbap()
    else {
        panic!("registration")
    };
    assert_eq!(
        capabilities,
        Capabilities {
            release: Release::R8AndLater,
            csg: CsgCapability::Capable
        }
    );
    assert_eq!(cause, RegistrationCause::Emergency);
    let context = ContextId::new(77).unwrap();
    core.hnbap(hnbap::Message::UeAccepted {
        identity,
        context,
        membership: None,
    });
    drive(&mut link, &mut radio, 4);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    drive(&mut link, &mut radio, 5);
    assert!(matches!(
        core.last_rua(),
        rua::Message::Connect {
            cause: rua::EstablishmentCause::Emergency,
            ..
        }
    ));
}
