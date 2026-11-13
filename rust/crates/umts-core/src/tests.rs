// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod iu_release;
mod rab_outcomes;
use std::{
    collections::VecDeque,
    num::NonZeroU64,
    sync::{Arc, Mutex},
};
use umts_iuh::{
    ContextId, Domain,
    hnbap::{
        self, AccessMode, Capabilities, CsgCapability, HnbParameters, Imsi, RegistrationCause,
        Release, UeIdentity,
    },
    ranap, rua,
    session::{
        AssociationId, ControlTime, Event, Frame, FrameScope, HnbState, Session, SessionLimits,
        UplinkNas,
    },
    wire::{Limits as CodecLimits, Protocol},
};
use umts_sctp::{Metadata, Notification};
use umts_types::{RadioEpoch, UeKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MockError {
    Broken,
}
enum Input {
    Notification(Notification),
    Packet(u32, Vec<u8>),
    Closed,
    Failure,
}
#[derive(Default)]
struct Shared {
    input: VecDeque<Input>,
    sent: Vec<(u16, u32, Vec<u8>)>,
    connected: bool,
    blocked: bool,
    fail_send: bool,
    sends: usize,
    reads: usize,
    acknowledgments: usize,
    dropped: usize,
}
struct Mock {
    shared: Arc<Mutex<Shared>>,
    held: Option<Input>,
}
#[derive(Clone)]
struct Peer(Arc<Mutex<Shared>>);
impl Peer {
    fn put(&self, input: Input) {
        self.0.lock().unwrap().input.push_back(input);
    }
    fn hnbap(&self, message: hnbap::Message) {
        self.put(Input::Packet(
            Protocol::Hnbap.ppid().unwrap(),
            message.encode(CodecLimits::default()).unwrap(),
        ));
    }
    fn rua(&self, message: rua::Message) {
        self.put(Input::Packet(
            Protocol::Rua.ppid().unwrap(),
            message.encode(CodecLimits::default()).unwrap(),
        ));
    }
    fn last(&self) -> (u16, u32, Vec<u8>) {
        self.0.lock().unwrap().sent.last().unwrap().clone()
    }
    fn block(&self, blocked: bool) {
        self.0.lock().unwrap().blocked = blocked;
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.shared.lock().unwrap().dropped += 1;
    }
}
impl Transport for Mock {
    type Error = MockError;
    fn complete_connect(&mut self) -> Result<bool, Self::Error> {
        Ok(self.shared.lock().unwrap().connected)
    }
    fn receive_once(&mut self) -> Result<Receive<'_>, Self::Error> {
        let mut shared = self.shared.lock().unwrap();
        shared.reads += 1;
        if self.held.is_none() {
            self.held = shared.input.pop_front();
        }
        drop(shared);
        Ok(match self.held.as_ref() {
            None => Receive::Idle,
            Some(Input::Notification(notification)) => Receive::Notification(*notification),
            Some(Input::Closed) => Receive::Closed,
            Some(Input::Failure) => return Err(MockError::Broken),
            Some(Input::Packet(ppid, bytes)) => Receive::Record {
                metadata: Metadata {
                    stream: 0,
                    sequence: 0,
                    ppid: *ppid,
                    association: 42,
                },
                bytes,
            },
        })
    }
    fn acknowledge_receive(&mut self) -> Result<(), Self::Error> {
        assert!(self.held.take().is_some());
        self.shared.lock().unwrap().acknowledgments += 1;
        Ok(())
    }
    fn send(&mut self, stream: u16, ppid: u32, bytes: &[u8]) -> Result<SendResult, Self::Error> {
        let mut shared = self.shared.lock().unwrap();
        shared.sends += 1;
        if shared.fail_send {
            return Err(MockError::Broken);
        }
        if shared.blocked {
            return Ok(SendResult::Retry);
        }
        shared.sent.push((stream, ppid, bytes.to_vec()));
        Ok(SendResult::Sent)
    }
}
fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn time(value: u64) -> ControlTime {
    ControlTime(value)
}
fn key(slot: u32) -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot,
        generation: NonZeroU64::MIN,
    }
}
fn identity(slot: u32) -> UeIdentity {
    UeIdentity::Imsi(Imsi::from_digits(&format!("001010{:09}", slot)).unwrap())
}
fn capabilities() -> Capabilities {
    Capabilities {
        release: Release::R8AndLater,
        csg: CsgCapability::NotCapable,
    }
}
fn limits(entries: usize) -> Limits {
    Limits {
        outbound_entries: count(entries),
        maximum_packet_bytes: count(65536),
        outbound_byte_budget: count(entries * 65536),
        connect_timeout_ms: NonZeroU64::new(100).unwrap(),
    }
}
fn session() -> Session {
    Session::new(
        key(0).epoch,
        AssociationId(NonZeroU64::MIN),
        HnbParameters {
            identity: b"Rust gateway test".to_vec(),
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
            maximum_ues: count(4),
            request_timeout_ms: NonZeroU64::new(1000).unwrap(),
            codec: CodecLimits::default(),
        },
        time(0),
    )
    .unwrap()
}
fn make_gateway(entries: usize) -> (Gateway<Mock>, Peer) {
    let shared = Arc::new(Mutex::new(Shared {
        connected: true,
        ..Shared::default()
    }));
    let peer = Peer(shared.clone());
    peer.put(Input::Notification(Notification::Established {
        inbound_streams: 2,
        outbound_streams: 2,
        association: 42,
    }));
    (
        Gateway::new(
            session(),
            Mock { shared, held: None },
            limits(entries),
            time(0),
        )
        .unwrap(),
        peer,
    )
}
fn drive(gateway: &mut Gateway<Mock>, at: u64) -> Vec<Event> {
    let mut events = Vec::new();
    gateway
        .drive(time(at), |event| {
            events.push(event.clone());
            Ok::<_, ()>(())
        })
        .unwrap();
    events
}
fn ready(entries: usize) -> (Gateway<Mock>, Peer) {
    let (mut gateway, peer) = make_gateway(entries);
    assert!(drive(&mut gateway, 0).is_empty());
    assert_eq!(gateway.phase(), Phase::AwaitingAssociation);
    drive(&mut gateway, 1);
    let (_, ppid, bytes) = peer.last();
    assert_eq!(ppid, Protocol::Hnbap.ppid().unwrap());
    assert!(matches!(
        hnbap::Message::decode(&bytes, CodecLimits::default())
            .unwrap()
            .message,
        hnbap::Message::HnbRegister(_)
    ));
    peer.hnbap(hnbap::Message::HnbAccepted {
        rnc: 1,
        mux_port: None,
    });
    assert!(matches!(
        &drive(&mut gateway, 2)[..],
        [Event::HnbAccepted { rnc: 1 }]
    ));
    assert_eq!(gateway.phase(), Phase::Serving);
    (gateway, peer)
}
fn register(gateway: &mut Gateway<Mock>, peer: &Peer, slot: u32, at: u64) -> ContextId {
    gateway
        .register_ue(
            key(slot),
            identity(slot),
            capabilities(),
            RegistrationCause::Normal,
            time(at),
        )
        .unwrap();
    drive(gateway, at + 1);
    let context = ContextId::new(100 + slot).unwrap();
    peer.hnbap(hnbap::Message::UeAccepted {
        identity: identity(slot),
        context,
        membership: None,
    });
    assert!(
        matches!(&drive(gateway, at + 2)[..], [Event::UeAccepted { key: accepted, .. }] if *accepted == key(slot))
    );
    context
}
fn nas(gateway: &mut Gateway<Mock>, slot: u32, domain: Domain, at: u64) -> Result<(), Error> {
    gateway.publish_nas(
        time(at),
        UplinkNas {
            key: key(slot),
            domain,
            nas: &[0x05, 0x31, 0x11],
            selector: None,
            cause: rua::EstablishmentCause::Normal,
        },
    )
}

#[test]
fn security_command_arrival_time_survives_upper_backpressure_without_redecoding() {
    let (mut gateway, peer) = ready(3);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    drive(&mut gateway, 7);
    let command = ranap::SecurityCommand {
        integrity: ranap::Protection {
            algorithms: vec![ranap::Algorithm(0)],
            key: ranap::Key::new([0x11; 16]),
        },
        encryption: None,
        key_status: ranap::KeyStatus::Old,
    };
    peer.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::encode_security(&command, CodecLimits::default()).unwrap(),
    });
    let decoded_before = gateway.counters().decoded;
    let mut original = None;
    assert!(matches!(
        gateway.drive(time(8), |event| {
            let Event::SecurityCommand {
                request,
                received_at,
                ..
            } = event
            else {
                panic!("expected security request");
            };
            assert_eq!(*received_at, time(8));
            original = Some(*request);
            Err::<(), _>("upper full")
        }),
        Err(DriveError::Upper("upper full"))
    ));
    gateway
        .drive(time(4008), |event| {
            let Event::SecurityCommand {
                request,
                received_at,
                ..
            } = event
            else {
                panic!("expected retained security request");
            };
            assert_eq!(Some(*request), original);
            assert_eq!(*received_at, time(8));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(gateway.counters().decoded, decoded_before + 1);
}

#[test]
fn security_response_retains_request_under_transport_backpressure() {
    let (mut gateway, peer) = ready(3);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    drive(&mut gateway, 7);
    let command = ranap::SecurityCommand {
        integrity: ranap::Protection {
            algorithms: vec![ranap::Algorithm(0)],
            key: ranap::Key::new([0x11; 16]),
        },
        encryption: Some(ranap::Protection {
            algorithms: vec![ranap::Algorithm(1)],
            key: ranap::Key::new([0x22; 16]),
        }),
        key_status: ranap::KeyStatus::New,
    };
    peer.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::encode_security(&command, CodecLimits::default()).unwrap(),
    });
    let events = drive(&mut gateway, 8);
    let [Event::SecurityCommand { request, .. }] = events.as_slice() else {
        panic!("security command must retain a scoped request");
    };
    let request = *request;
    let response = ranap::SecurityResponse::Complete {
        integrity: ranap::Algorithm(0),
        encryption: Some(ranap::Algorithm(1)),
    };
    peer.block(true);
    for _ in 0..3 {
        nas(&mut gateway, 0, Domain::Cs, 9).unwrap();
    }
    assert!(matches!(
        gateway.publish_security_response(time(9), request, response),
        Err(Error::Capacity)
    ));
    assert_eq!(gateway.queued_frames(), 3);
    peer.block(false);
    drive(&mut gateway, 10);
    gateway
        .publish_security_response(time(10), request, response)
        .unwrap();
    for at in 11..=13 {
        drive(&mut gateway, at);
    }
    let (_, ppid, bytes) = peer.last();
    assert_eq!(ppid, Protocol::Rua.ppid().unwrap());
    let rua::Message::Direct {
        domain,
        context: returned,
        ranap,
    } = rua::Message::decode(&bytes, CodecLimits::default())
        .unwrap()
        .message
    else {
        panic!("security response must use connection-oriented RUA");
    };
    assert_eq!((domain, returned), (Domain::Cs, context));
    assert_eq!(
        ranap::SecurityResponse::decode(&ranap, CodecLimits::default())
            .unwrap()
            .response,
        response
    );
    assert!(matches!(
        gateway.publish_security_response(time(14), request, response),
        Err(Error::Session(umts_iuh::Error::Origin))
    ));
}

#[test]
fn rab_response_keeps_its_original_request_through_upper_and_transport_backpressure() {
    use umts_iuh::ranap::rab::{
        Assignment, AssignmentResponse, First, Second, Setup, SetupOrModify,
    };
    let (mut gateway, peer) = ready(3);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Ps, 6).unwrap();
    drive(&mut gateway, 7);
    let assignment = Assignment {
        setup: vec![SetupOrModify {
            first: First::modification(5),
            second: Second::default(),
        }],
        release: vec![],
    };
    peer.rua(rua::Message::Direct {
        domain: Domain::Ps,
        context,
        ranap: assignment.encode(CodecLimits::default()).unwrap(),
    });
    let mut original = None;
    assert!(matches!(
        gateway.drive(time(8), |event| {
            let Event::RabAssignment {
                request,
                received_at,
                ..
            } = event
            else {
                panic!("expected RAB assignment")
            };
            original = Some(*request);
            assert_eq!(*received_at, time(8));
            Err::<(), _>("upper full")
        }),
        Err(DriveError::Upper("upper full"))
    ));
    let events = drive(&mut gateway, 9);
    let [
        Event::RabAssignment {
            request,
            received_at,
            ..
        },
    ] = events.as_slice()
    else {
        panic!("expected retained RAB assignment")
    };
    assert_eq!(Some(*request), original);
    assert_eq!(*received_at, time(8));
    assert!(
        gateway
            .authorize_rab(*request, 5, umts_iuh::session::RabOperation::SetupOrModify)
            .is_ok()
    );
    assert!(
        gateway
            .authorize_rab(*request, 5, umts_iuh::session::RabOperation::Release)
            .is_err()
    );
    let response = AssignmentResponse {
        setup: vec![Setup::new(5)],
        ..Default::default()
    };
    peer.block(true);
    for _ in 0..3 {
        nas(&mut gateway, 0, Domain::Ps, 10).unwrap();
    }
    assert!(matches!(
        gateway.publish_rab_response(time(10), *request, &response),
        Err(Error::Capacity)
    ));
    assert!(
        gateway
            .authorize_rab(*request, 5, umts_iuh::session::RabOperation::SetupOrModify)
            .is_ok()
    );
    peer.block(false);
    drive(&mut gateway, 11);
    gateway
        .publish_rab_response(time(11), *request, &response)
        .unwrap();
    assert!(
        gateway
            .authorize_rab(*request, 5, umts_iuh::session::RabOperation::SetupOrModify)
            .is_err()
    );
    for at in 12..=14 {
        drive(&mut gateway, at);
    }
    let (_, ppid, bytes) = peer.last();
    assert_eq!(ppid, Protocol::Rua.ppid().unwrap());
    let rua::Message::Direct {
        context: returned,
        domain,
        ranap,
    } = rua::Message::decode(&bytes, CodecLimits::default())
        .unwrap()
        .message
    else {
        panic!("expected RUA direct")
    };
    assert_eq!((returned, domain), (context, Domain::Ps));
    assert_eq!(
        AssignmentResponse::decode(&ranap, CodecLimits::default())
            .unwrap()
            .0,
        response
    );
    assert!(matches!(
        gateway.publish_rab_response(time(15), *request, &response),
        Err(Error::Session(umts_iuh::Error::Origin))
    ));
}

#[test]
fn registration_and_two_domains_traverse_real_codecs_and_owned_transport() {
    let (mut gateway, peer) = ready(8);
    let context = register(&mut gateway, &peer, 0, 3);
    for (domain, at) in [(Domain::Cs, 6), (Domain::Ps, 8)] {
        nas(&mut gateway, 0, domain, at).unwrap();
        drive(&mut gateway, at + 1);
        let (stream, ppid, bytes) = peer.last();
        assert_eq!(stream, 0);
        assert_eq!(ppid, Protocol::Rua.ppid().unwrap());
        assert!(
            matches!(rua::Message::decode(&bytes, CodecLimits::default()).unwrap().message,
            rua::Message::Connect { context: actual, domain: actual_domain, .. } if actual == context && actual_domain == domain)
        );
    }
    for (domain, at) in [(Domain::Cs, 10), (Domain::Ps, 11)] {
        peer.rua(rua::Message::Direct {
            domain,
            context,
            ranap: ranap::downlink_direct(&[0x05, 0x31, 0x11], None, CodecLimits::default())
                .unwrap(),
        });
        assert!(
            matches!(&drive(&mut gateway, at)[..], [Event::Ranap { key: Some(actual), domain: Some(actual_domain), message }]
            if *actual == key(0) && *actual_domain == domain && matches!(&message.message, ranap::Inbound::Direct { nas, .. } if nas == &[0x05, 0x31, 0x11]))
        );
    }
    assert_eq!(gateway.context(key(0)), Some(context));
}

#[test]
fn upper_backpressure_reuses_decoded_record_and_allows_peer_commands() {
    let (mut gateway, peer) = ready(8);
    gateway
        .register_ue(
            key(0),
            identity(0),
            capabilities(),
            RegistrationCause::Normal,
            time(3),
        )
        .unwrap();
    drive(&mut gateway, 4);
    peer.hnbap(hnbap::Message::UeAccepted {
        identity: identity(0),
        context: ContextId::new(100).unwrap(),
        membership: None,
    });
    assert!(matches!(
        gateway.drive(time(5), |_| Err(9u8)),
        Err(DriveError::Upper(9))
    ));
    let before = peer.0.lock().unwrap().reads;
    let decoded = gateway.counters().decoded;
    assert_eq!(gateway.context(key(0)), None);
    gateway
        .register_ue(
            key(1),
            identity(1),
            capabilities(),
            RegistrationCause::Normal,
            time(6),
        )
        .unwrap();
    assert!(matches!(
        gateway.drive(time(7), |_| Err(9u8)),
        Err(DriveError::Upper(9))
    ));
    assert_eq!(peer.0.lock().unwrap().reads, before);
    assert_eq!(gateway.counters().decoded, decoded);
    assert!(
        matches!(hnbap::Message::decode(&peer.last().2, CodecLimits::default()).unwrap().message,
        hnbap::Message::UeRegister { identity: actual, .. } if actual == identity(1))
    );
    drive(&mut gateway, 8);
    assert_eq!(gateway.context(key(0)), Some(ContextId::new(100).unwrap()));
}

#[test]
fn kernel_send_backpressure_does_not_consume_or_reorder_frames() {
    let (mut gateway, peer) = ready(4);
    register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    nas(&mut gateway, 0, Domain::Ps, 7).unwrap();
    peer.block(true);
    let sent = peer.0.lock().unwrap().sent.len();
    assert!(
        gateway
            .drive(time(8), |_| Ok::<_, ()>(()))
            .unwrap()
            .backpressure
    );
    assert_eq!(gateway.queued_frames(), 2);
    assert_eq!(peer.0.lock().unwrap().sent.len(), sent);
    peer.block(false);
    drive(&mut gateway, 9);
    assert_eq!(gateway.queued_frames(), 1);
    assert!(matches!(
        rua::Message::decode(&peer.last().2, CodecLimits::default())
            .unwrap()
            .message,
        rua::Message::Connect {
            domain: Domain::Cs,
            ..
        }
    ));
    drive(&mut gateway, 10);
    assert_eq!(gateway.queued_frames(), 0);
    assert!(matches!(
        rua::Message::decode(&peer.last().2, CodecLimits::default())
            .unwrap()
            .message,
        rua::Message::Connect {
            domain: Domain::Ps,
            ..
        }
    ));
}

#[test]
fn cached_rua_is_cancelled_only_after_its_ue_retirement_is_admitted() {
    let (mut gateway, peer) = ready(3);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    drive(&mut gateway, 7);
    peer.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::downlink_direct(&[5, 0x31], None, CodecLimits::default()).unwrap(),
    });
    assert!(matches!(
        gateway.drive(time(8), |_| Err(7u8)),
        Err(DriveError::Upper(7))
    ));
    for now in 9..12 {
        nas(&mut gateway, 0, Domain::Cs, now).unwrap();
    }
    assert_eq!(
        gateway.retire_radio_failure(key(0), time(12)),
        Err(Error::Capacity)
    );
    assert_eq!(gateway.counters().cancelled_inbound, 0);
    for now in 13..15 {
        let progress = gateway
            .drive(time(now), |_| -> Result<(), ()> {
                panic!("closing UE cannot receive cached traffic")
            })
            .unwrap();
        assert!(progress.transmitted && progress.backpressure);
        assert!(!progress.delivered);
    }
    gateway.retire_radio_failure(key(0), time(15)).unwrap();
    assert_eq!(gateway.counters().cancelled_inbound, 1);
    assert_eq!(gateway.registered_ues(), 0);
    for now in 16..20 {
        drive(&mut gateway, now);
    }
    assert_eq!(gateway.phase(), Phase::Serving);
    assert_eq!(gateway.loss_reason(), None);
}

#[test]
fn retiring_a_peer_preserves_the_cached_rua_owner() {
    let (mut gateway, peer) = ready(3);
    register(&mut gateway, &peer, 0, 3);
    let context = register(&mut gateway, &peer, 1, 6);
    nas(&mut gateway, 1, Domain::Cs, 9).unwrap();
    drive(&mut gateway, 10);
    peer.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::downlink_direct(&[5, 0x31], None, CodecLimits::default()).unwrap(),
    });
    assert!(matches!(
        gateway.drive(time(11), |_| Err(7u8)),
        Err(DriveError::Upper(7))
    ));
    gateway.retire_radio_failure(key(0), time(12)).unwrap();
    assert_eq!(gateway.counters().cancelled_inbound, 0);
    assert!(
        matches!(&drive(&mut gateway, 13)[..], [Event::Ranap { key: Some(owner), .. }] if *owner == key(1))
    );
    assert_eq!(gateway.registered_ues(), 1);
    assert_eq!(gateway.phase(), Phase::Serving);
}

#[test]
fn whole_radio_failure_requires_atomic_three_frame_admission() {
    let (mut gateway, peer) = ready(3);
    register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    nas(&mut gateway, 0, Domain::Ps, 7).unwrap();
    nas(&mut gateway, 0, Domain::Cs, 8).unwrap();
    assert_eq!(
        gateway.retire_radio_failure(key(0), time(9)),
        Err(Error::Capacity)
    );
    assert_eq!(gateway.registered_ues(), 1);
    assert_eq!(gateway.queued_frames(), 3);
    assert!(nas(&mut gateway, 0, Domain::Ps, 10).is_err());
    for at in 11..14 {
        drive(&mut gateway, at);
    }
    gateway.retire_radio_failure(key(0), time(14)).unwrap();
    assert_eq!(gateway.registered_ues(), 0);
    assert_eq!(gateway.queued_frames(), 3);
    for at in 15..18 {
        drive(&mut gateway, at);
    }
    let shared = peer.0.lock().unwrap();
    let tail = &shared.sent[shared.sent.len() - 3..];
    for (entry, expected) in tail[..2].iter().zip([Domain::Cs, Domain::Ps]) {
        assert!(
            matches!(rua::Message::decode(&entry.2, CodecLimits::default()).unwrap().message, rua::Message::Disconnect { domain, .. } if domain == expected)
        );
    }
    assert!(matches!(
        hnbap::Message::decode(&tail[2].2, CodecLimits::default())
            .unwrap()
            .message,
        hnbap::Message::UeDeregister { .. }
    ));
}

#[test]
fn observed_domain_release_cancels_unsent_frames_before_upper_admission() {
    let (mut gateway, peer) = ready(8);
    let context = register(&mut gateway, &peer, 0, 3);
    register(&mut gateway, &peer, 1, 6);
    nas(&mut gateway, 0, Domain::Cs, 9).unwrap();
    drive(&mut gateway, 10);
    nas(&mut gateway, 0, Domain::Ps, 11).unwrap();
    drive(&mut gateway, 12);
    nas(&mut gateway, 1, Domain::Cs, 13).unwrap();
    drive(&mut gateway, 14);
    nas(&mut gateway, 0, Domain::Cs, 15).unwrap();
    nas(&mut gateway, 0, Domain::Ps, 16).unwrap();
    nas(&mut gateway, 1, Domain::Cs, 17).unwrap();
    peer.block(true);
    peer.rua(rua::Message::Disconnect {
        domain: Domain::Cs,
        context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    assert!(matches!(
        gateway.drive(time(18), |_| Err(7u8)),
        Err(DriveError::Upper(7))
    ));
    assert!(nas(&mut gateway, 0, Domain::Cs, 19).is_err());
    peer.block(false);
    let sent = peer.0.lock().unwrap().sent.len();
    assert!(matches!(
        gateway.drive(time(20), |_| Err(7u8)),
        Err(DriveError::Upper(7))
    ));
    assert_eq!(peer.0.lock().unwrap().sent.len(), sent);
    assert!(matches!(
        gateway.drive(time(21), |_| Err(7u8)),
        Err(DriveError::Upper(7))
    ));
    assert!(
        matches!(rua::Message::decode(&peer.last().2, CodecLimits::default()).unwrap().message, rua::Message::Direct { domain: Domain::Ps, context: actual, .. } if actual == context)
    );
    drive(&mut gateway, 22);
    assert!(
        matches!(rua::Message::decode(&peer.last().2, CodecLimits::default()).unwrap().message, rua::Message::Direct { domain: Domain::Cs, context: actual, .. } if actual != context)
    );
}

#[test]
fn transport_loss_closes_immediately_and_defers_resource_retirement() {
    let (mut gateway, peer) = ready(8);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    peer.0.lock().unwrap().fail_send = true;
    assert!(matches!(
        gateway.drive(time(7), |_| Ok::<_, ()>(())),
        Err(DriveError::Transport(MockError::Broken))
    ));
    assert_eq!(gateway.phase(), Phase::Draining);
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    assert_eq!(gateway.queued_frames(), 0);
    assert_eq!(gateway.context(key(0)), Some(context));
    assert!(matches!(
        gateway.drive(time(8), |_| Err(9u8)),
        Err(DriveError::Upper(9))
    ));
    assert!(nas(&mut gateway, 0, Domain::Ps, 9).is_err());
    assert_eq!(gateway.registered_ues(), 1);
    let events = drive(&mut gateway, 10);
    assert!(matches!(&events[..], [Event::AssociationLost { owners, .. }] if owners == &[key(0)]));
    assert_eq!(gateway.registered_ues(), 0);
    assert_eq!(gateway.phase(), Phase::Closed);
    assert_eq!(gateway.loss_reason(), Some(LossReason::TransportFailure));
}

#[test]
fn quiet_connect_and_registration_deadlines_invalidate_without_replaying() {
    let (mut gateway, peer) = make_gateway(4);
    peer.0.lock().unwrap().connected = false;
    assert!(drive(&mut gateway, 99).is_empty());
    assert!(matches!(
        &drive(&mut gateway, 100)[..],
        [Event::AssociationLost { .. }]
    ));
    assert_eq!(gateway.loss_reason(), Some(LossReason::ConnectTimeout));
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    let (mut gateway, peer) = make_gateway(4);
    drive(&mut gateway, 0);
    drive(&mut gateway, 1);
    let sent = peer.0.lock().unwrap().sent.len();
    assert!(matches!(
        gateway.drive(time(1001), |_| Err(2u8)),
        Err(DriveError::Upper(2))
    ));
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    drive(&mut gateway, 1002);
    assert_eq!(gateway.loss_reason(), Some(LossReason::RequestTimeout));
    assert_eq!(peer.0.lock().unwrap().sent.len(), sent);
}

#[test]
fn cancelled_registration_accept_is_automatically_deregistered() {
    let (mut gateway, peer) = ready(4);
    gateway
        .register_ue(
            key(0),
            identity(0),
            capabilities(),
            RegistrationCause::Normal,
            time(3),
        )
        .unwrap();
    drive(&mut gateway, 4);
    gateway.retire_radio_failure(key(0), time(5)).unwrap();
    let context = ContextId::new(100).unwrap();
    peer.hnbap(hnbap::Message::UeAccepted {
        identity: identity(0),
        context,
        membership: None,
    });
    assert!(matches!(
        &drive(&mut gateway, 6)[..],
        [Event::CancelledRegistrationAccepted { .. }]
    ));
    assert_eq!(gateway.registered_ues(), 1);
    drive(&mut gateway, 7);
    assert_eq!(gateway.registered_ues(), 0);
    drive(&mut gateway, 8);
    assert!(
        matches!(hnbap::Message::decode(&peer.last().2, CodecLimits::default()).unwrap().message, hnbap::Message::UeDeregister { context: actual, .. } if actual == context)
    );
    assert_eq!(gateway.counters().cleaned_cancelled, 1);
}

#[test]
fn malformed_and_unknown_records_do_not_cross_deliver_or_close_peers() {
    let (mut gateway, peer) = ready(4);
    peer.put(Input::Packet(999, vec![1]));
    assert!(matches!(
        gateway.drive(time(3), |_| Ok::<_, ()>(())),
        Err(DriveError::Owner(Error::Protocol))
    ));
    peer.put(Input::Packet(Protocol::Rua.ppid().unwrap(), vec![1]));
    assert!(matches!(
        gateway.drive(time(4), |_| Ok::<_, ()>(())),
        Err(DriveError::Owner(Error::Session(_)))
    ));
    assert_eq!(gateway.phase(), Phase::Serving);
    assert_eq!(peer.0.lock().unwrap().dropped, 0);
    assert_eq!(gateway.counters().rejected, 2);
    // TS 25.468 10: the undecodable RUA record is answered with RUA ERROR INDICATION,
    // cause transfer syntax error. The unknown PPID is not answered.
    assert_eq!(gateway.counters().error_indications, 1);
    gateway.drive(time(5), |_| Ok::<_, ()>(())).unwrap();
    let (_, ppid, bytes) = peer.last();
    assert_eq!(Some(ppid), Protocol::Rua.ppid());
    assert!(matches!(
        rua::Message::decode(&bytes, CodecLimits::default())
            .unwrap()
            .message,
        rua::Message::ErrorIndication {
            cause: Some(rua::Cause::Protocol(
                umts_iuh::ProtocolCause::TransferSyntax
            )),
            ..
        }
    ));
    assert!(matches!(
        gateway.drive(time(2), |_| Ok::<_, ()>(())),
        Err(DriveError::Owner(Error::Time))
    ));
}

#[test]
fn remote_deregistration_keeps_backoff_when_upper_queue_is_full() {
    let (mut gateway, peer) = ready(4);
    register(&mut gateway, &peer, 0, 3);
    peer.hnbap(hnbap::Message::HnbDeregistered {
        cause: hnbap::Cause::Radio(hnbap::RadioCause::Overload),
        backoff_seconds: Some(2),
    });
    assert!(matches!(
        gateway.drive(time(6), |_| Err(8u8)),
        Err(DriveError::Upper(8))
    ));
    assert_eq!(gateway.phase(), Phase::Draining);
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    assert_eq!(gateway.registered_ues(), 1);
    let events = drive(&mut gateway, 7);
    assert!(matches!(
        &events[..],
        [Event::AssociationLost {
            backoff_until: Some(ControlTime(2006)),
            ..
        }]
    ));
    assert_eq!(gateway.hnb_state(), HnbState::Backoff { until: time(2006) });
}

#[test]
fn receive_loss_and_shutdown_are_bounded_and_terminal() {
    for input in [
        Input::Failure,
        Input::Closed,
        Input::Notification(Notification::Shutdown { association: 42 }),
    ] {
        let (mut gateway, peer) = ready(4);
        peer.put(input);
        let _ = gateway.drive(time(3), |_| Ok::<_, ()>(()));
        drive(&mut gateway, 4);
        assert_eq!(gateway.phase(), Phase::Closed);
        assert_eq!(peer.0.lock().unwrap().dropped, 1);
        let reads = peer.0.lock().unwrap().reads;
        drive(&mut gateway, 5);
        assert_eq!(peer.0.lock().unwrap().reads, reads);
    }
}

#[test]
fn local_shutdown_is_idempotent_and_retains_session_custody() {
    let (mut gateway, peer) = ready(4);
    register(&mut gateway, &peer, 0, 3);
    gateway.shutdown();
    assert_eq!(gateway.phase(), Phase::Draining);
    assert_eq!(gateway.loss_reason(), Some(LossReason::LocalShutdown));
    assert_eq!(gateway.counters().losses, 1);
    assert_eq!(gateway.registered_ues(), 1);
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    assert!(matches!(
        gateway.register_ue(
            key(1),
            identity(1),
            capabilities(),
            RegistrationCause::Normal,
            time(6),
        ),
        Err(Error::State)
    ));
    gateway.shutdown();
    assert_eq!(gateway.phase(), Phase::Draining);
    assert_eq!(gateway.counters().losses, 1);
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    assert_eq!(gateway.registered_ues(), 1);
}

#[test]
fn outbox_checks_whole_batch_association_protocol_and_capacity_before_mutation() {
    let association = AssociationId(NonZeroU64::MIN);
    let mut queue = queue::Outbox::new(association, limits(3)).unwrap();
    let frame = Frame {
        association,
        scope: FrameScope::Hnb,
        protocol: Protocol::Hnbap,
        data: vec![1],
    };
    let stale = Frame {
        association: AssociationId(NonZeroU64::new(2).unwrap()),
        ..frame.clone()
    };
    assert_eq!(
        queue.admit(&[frame.clone(), stale]),
        Err(Error::Association)
    );
    assert_eq!(queue.len(), 0);
    assert_eq!(
        queue.admit(&[Frame {
            protocol: Protocol::Ranap,
            ..frame.clone()
        }]),
        Err(Error::Protocol)
    );
    queue.admit(&[frame.clone(), frame.clone()]).unwrap();
    assert_eq!(queue.admit(&[frame.clone(), frame]), Err(Error::Capacity));
    assert_eq!(queue.len(), 2);
    assert!(matches!(
        queue::Outbox::new(
            association,
            Limits {
                outbound_byte_budget: count(1),
                ..limits(3)
            }
        ),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        queue::Outbox::new(association, limits(2)),
        Err(Error::Capacity)
    ));
}

#[test]
fn one_step_never_drains_queued_sends_or_inbound_records() {
    let (mut gateway, peer) = ready(8);
    register(&mut gateway, &peer, 0, 3);
    for at in 6..10 {
        nas(&mut gateway, 0, Domain::Cs, at).unwrap();
    }
    for _ in 0..4 {
        peer.rua(rua::Message::Direct {
            context: ContextId::new(100).unwrap(),
            domain: Domain::Cs,
            ranap: ranap::downlink_direct(&[0x05, 0x31, 0x11], None, CodecLimits::default())
                .unwrap(),
        });
    }
    let (sends, reads) = {
        let shared = peer.0.lock().unwrap();
        (shared.sends, shared.reads)
    };
    let events = drive(&mut gateway, 10);
    let shared = peer.0.lock().unwrap();
    assert_eq!(shared.sends, sends + 1);
    assert_eq!(shared.reads, reads + 1);
    assert_eq!(events.len(), 1);
    assert_eq!(shared.input.len(), 3);
    assert_eq!(gateway.queued_frames(), 3);
}

#[test]
fn constructor_guards_overflow_budget_and_clock_without_protocol_side_effects() {
    let (gateway, peer) = make_gateway(4);
    drop(gateway);
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let transport = Mock {
        shared: shared.clone(),
        held: None,
    };
    let invalid = Limits {
        outbound_entries: count(usize::MAX),
        maximum_packet_bytes: count(2),
        ..limits(4)
    };
    assert!(matches!(
        Gateway::new(session(), transport, invalid, time(0)),
        Err(Error::Capacity)
    ));
    assert_eq!(shared.lock().unwrap().sends, 0);
    let transport = Mock {
        shared: shared.clone(),
        held: None,
    };
    assert!(matches!(
        Gateway::new(session(), transport, limits(4), time(u64::MAX)),
        Err(Error::Time)
    ));
    assert_eq!(shared.lock().unwrap().sends, 0);
}

#[test]
fn registration_rejection_backoff_closes_transport_without_automatic_reconnect() {
    let (mut gateway, peer) = make_gateway(4);
    drive(&mut gateway, 0);
    drive(&mut gateway, 1);
    peer.hnbap(hnbap::Message::HnbRejected {
        cause: hnbap::Cause::Radio(hnbap::RadioCause::Overload),
        backoff_seconds: Some(2),
    });
    assert!(matches!(
        gateway.drive(time(2), |_| Err(3u8)),
        Err(DriveError::Upper(3))
    ));
    assert_eq!(gateway.phase(), Phase::Registering);
    assert!(matches!(
        &drive(&mut gateway, 3)[..],
        [Event::HnbRejected {
            backoff_until: Some(ControlTime(2003)),
            ..
        }]
    ));
    assert_eq!(gateway.phase(), Phase::Closed);
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    let sent = peer.0.lock().unwrap().sent.len();
    drive(&mut gateway, 3000);
    assert_eq!(peer.0.lock().unwrap().sent.len(), sent);
    assert_eq!(
        gateway.loss_reason(),
        Some(LossReason::RegistrationRejected)
    );
}

#[test]
fn cached_registration_response_does_not_disable_the_owner_resource_guard() {
    let (mut gateway, peer) = make_gateway(4);
    drive(&mut gateway, 0);
    drive(&mut gateway, 1);
    peer.hnbap(hnbap::Message::HnbAccepted {
        rnc: 1,
        mux_port: None,
    });
    assert!(matches!(
        gateway.drive(time(2), |_| Err(3u8)),
        Err(DriveError::Upper(3))
    ));
    assert!(matches!(
        &drive(&mut gateway, 1001)[..],
        [Event::AssociationLost { .. }]
    ));
    assert_eq!(gateway.phase(), Phase::Closed);
    assert_eq!(gateway.counters().decoded, 1);
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
}

#[test]
fn remote_ue_retirement_suppresses_all_its_unsent_scopes_but_not_a_peer() {
    let (mut gateway, peer) = ready(8);
    let context = register(&mut gateway, &peer, 0, 3);
    let peer_context = register(&mut gateway, &peer, 1, 6);
    nas(&mut gateway, 0, Domain::Cs, 9).unwrap();
    nas(&mut gateway, 0, Domain::Ps, 10).unwrap();
    nas(&mut gateway, 1, Domain::Cs, 11).unwrap();
    peer.block(true);
    peer.hnbap(hnbap::Message::UeDeregister {
        context,
        cause: hnbap::Cause::Radio(hnbap::RadioCause::ConnectionLost),
    });
    assert!(matches!(
        gateway.drive(time(12), |_| Err(4u8)),
        Err(DriveError::Upper(4))
    ));
    let sent = peer.0.lock().unwrap().sent.len();
    peer.block(false);
    for at in 13..15 {
        assert!(matches!(
            gateway.drive(time(at), |_| Err(4u8)),
            Err(DriveError::Upper(4))
        ));
        assert_eq!(peer.0.lock().unwrap().sent.len(), sent);
    }
    drive(&mut gateway, 15);
    assert_eq!(gateway.registered_ues(), 1);
    assert_eq!(gateway.context(key(1)), Some(peer_context));
    assert!(
        matches!(rua::Message::decode(&peer.last().2, CodecLimits::default()).unwrap().message, rua::Message::Connect { context: actual, .. } if actual == peer_context)
    );
}

#[test]
fn cancelled_cleanup_cannot_block_a_backpressured_remote_retirement() {
    let (mut gateway, peer) = ready(3);
    register(&mut gateway, &peer, 1, 3);
    gateway
        .register_ue(
            key(0),
            identity(0),
            capabilities(),
            RegistrationCause::Normal,
            time(6),
        )
        .unwrap();
    drive(&mut gateway, 7);
    gateway.retire_radio_failure(key(0), time(8)).unwrap();
    for at in 9..12 {
        nas(&mut gateway, 1, Domain::Cs, at).unwrap();
    }
    peer.block(true);
    let context = ContextId::new(100).unwrap();
    peer.hnbap(hnbap::Message::UeAccepted {
        identity: identity(0),
        context,
        membership: None,
    });
    drive(&mut gateway, 12);
    peer.hnbap(hnbap::Message::UeDeregister {
        context,
        cause: hnbap::Cause::Radio(hnbap::RadioCause::ConnectionLost),
    });
    assert!(matches!(
        gateway.drive(time(13), |_| Err(5u8)),
        Err(DriveError::Upper(5))
    ));
    let events = drive(&mut gateway, 14);
    assert!(
        matches!(&events[..], [Event::UeDeregistered { key: actual, .. }] if *actual == key(0))
    );
    drive(&mut gateway, 15);
    assert_eq!(gateway.registered_ues(), 1);
    assert_eq!(gateway.counters().cleaned_cancelled, 1);
    assert_eq!(gateway.queued_frames(), 3);
    assert_eq!(gateway.phase(), Phase::Serving);
}

#[test]
fn cancellation_uses_full_ue_generation_and_keeps_a_reused_slot_independent() {
    let association = AssociationId(NonZeroU64::MIN);
    let mut queue = queue::Outbox::new(association, limits(3)).unwrap();
    let old = Frame {
        association,
        scope: FrameScope::Ue {
            key: key(0),
            domain: Some(Domain::Cs),
        },
        protocol: Protocol::Rua,
        data: vec![1],
    };
    let current = Frame {
        scope: FrameScope::Ue {
            key: UeKey {
                generation: NonZeroU64::new(2).unwrap(),
                ..key(0)
            },
            domain: Some(Domain::Cs),
        },
        data: vec![2],
        ..old.clone()
    };
    queue.admit(&[old, current]).unwrap();
    queue.cancel(key(0), None);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let mut transport = Mock {
        shared: shared.clone(),
        held: None,
    };
    assert_eq!(queue.transmit_one(&mut transport).unwrap(), None);
    assert_eq!(shared.lock().unwrap().sends, 0);
    assert_eq!(
        queue.transmit_one(&mut transport).unwrap(),
        Some(SendResult::Sent)
    );
    assert_eq!(shared.lock().unwrap().sent[0].2, [2]);
    assert_eq!(queue.len(), 0);
}

#[test]
fn terminal_notify_metadata_survives_upper_backpressure_and_transport_close() {
    use umts_iuh::wire::{Criticality, InformationElement, Packet};
    let (mut gateway, peer) = ready(4);
    register(&mut gateway, &peer, 0, 3);
    let bytes = hnbap::Message::HnbDeregistered {
        cause: hnbap::Cause::Radio(hnbap::RadioCause::Overload),
        backoff_seconds: Some(2),
    }
    .encode(CodecLimits::default())
    .unwrap();
    let mut packet = Packet::decode(Protocol::Hnbap, &bytes, CodecLimits::default()).unwrap();
    packet
        .ies
        .push(InformationElement::new(65000, Criticality::Notify, &0u8).unwrap());
    peer.put(Input::Packet(
        Protocol::Hnbap.ppid().unwrap(),
        packet.encode(CodecLimits::default()).unwrap(),
    ));
    let mut initial = None;
    assert!(matches!(
        gateway.drive(time(6), |event| {
            initial = Some(event.clone());
            Err(7u8)
        }),
        Err(DriveError::Upper(7))
    ));
    assert_eq!(peer.0.lock().unwrap().dropped, 1);
    let events = drive(&mut gateway, 7);
    assert_eq!(events, [initial.unwrap()]);
    assert!(
        matches!(&events[..], [Event::Notified { notify_ies, event, .. }]
        if notify_ies == &[65000] && matches!(event.as_ref(), Event::AssociationLost { backoff_until: Some(ControlTime(2006)), .. }))
    );
}
