// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn ps_transport_release_retries_backpressure_once_and_keeps_cs_connected() {
    let (mut gateway, peer) = ready(3);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    drive(&mut gateway, 7);
    nas(&mut gateway, 0, Domain::Ps, 8).unwrap();
    drive(&mut gateway, 9);
    peer.block(true);
    for _ in 0..3 {
        nas(&mut gateway, 0, Domain::Ps, 10).unwrap();
    }
    for at in 11..=12 {
        assert_eq!(
            gateway.request_ps_transport_release(time(at), key(0), context),
            Err(Error::Capacity)
        );
        assert_eq!(gateway.queued_frames(), 3);
    }
    peer.block(false);
    for at in 13..=15 {
        drive(&mut gateway, at);
    }
    assert_eq!(gateway.queued_frames(), 0);
    let sent_before = peer.0.lock().unwrap().sent.len();
    peer.block(true);
    assert_eq!(
        gateway.request_ps_transport_release(time(16), key(0), context),
        Ok(true)
    );
    for _ in 0..2 {
        nas(&mut gateway, 0, Domain::Cs, 16).unwrap();
    }
    for _ in 0..2 {
        assert_eq!(
            gateway.request_ps_transport_release(time(17), key(0), context),
            Ok(false)
        );
        assert_eq!(gateway.queued_frames(), 3);
    }
    drive(&mut gateway, 17);
    assert_eq!(peer.0.lock().unwrap().sent.len(), sent_before);
    peer.block(false);
    for at in 18..=20 {
        drive(&mut gateway, at);
    }
    assert_eq!(gateway.queued_frames(), 0);
    let sent = peer.0.lock().unwrap().sent[sent_before..].to_vec();
    assert_eq!(sent.len(), 3);
    for (index, (stream, ppid, bytes)) in sent.into_iter().enumerate() {
        assert_eq!(stream, 0);
        assert_eq!(ppid, Protocol::Rua.ppid().unwrap());
        let rua::Message::Direct {
            context: actual,
            domain,
            ranap: bytes,
        } = rua::Message::decode(&bytes, CodecLimits::default())
            .unwrap()
            .message
        else {
            panic!("both Iu connections must remain open")
        };
        assert_eq!(actual, context);
        if index == 0 {
            assert_eq!(domain, Domain::Ps);
            let packet =
                umts_iuh::wire::Packet::decode(Protocol::Ranap, &bytes, CodecLimits::default())
                    .unwrap();
            assert_eq!(packet.outcome, umts_iuh::wire::Outcome::Initiating);
            assert_eq!(packet.procedure, 11);
            let cause: ranap::rab::Cause = packet.required(4).unwrap().decode().unwrap();
            assert!(matches!(cause, ranap::rab::Cause::Radio(_)));
            assert_eq!(cause.code(), 15);
        } else {
            assert_eq!(domain, Domain::Cs);
        }
    }
    assert_eq!(gateway.context(key(0)), Some(context));
    assert_eq!(
        gateway.request_ps_transport_release(time(21), key(0), context),
        Ok(false)
    );
    assert_eq!(gateway.queued_frames(), 0);
}

#[test]
fn peer_disconnect_keeps_cleanup_authority_without_blocking_other_domain_or_sending_reply() {
    let (mut gateway, peer) = ready(3);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    drive(&mut gateway, 7);
    nas(&mut gateway, 0, Domain::Ps, 8).unwrap();
    drive(&mut gateway, 9);
    peer.rua(rua::Message::Disconnect {
        context,
        domain: Domain::Cs,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    let events = drive(&mut gateway, 10);
    let [Event::Disconnected { request, .. }] = events.as_slice() else {
        panic!("owned peer release required")
    };
    let request = *request;
    assert!(gateway.owns_iu_release(request));
    peer.rua(rua::Message::Direct {
        context,
        domain: Domain::Ps,
        ranap: ranap::downlink_direct(&[1], None, CodecLimits::default()).unwrap(),
    });
    assert!(matches!(
        drive(&mut gateway, 11).as_slice(),
        [Event::Ranap {
            domain: Some(Domain::Ps),
            ..
        }]
    ));
    let queued = gateway.queued_frames();
    gateway
        .complete_iu_release_without_reports(time(12), request)
        .unwrap();
    assert_eq!(gateway.queued_frames(), queued);
    assert!(!gateway.owns_iu_release(request));
    nas(&mut gateway, 0, Domain::Cs, 13).unwrap();
    drive(&mut gateway, 14);
    let (_, _, bytes) = peer.last();
    assert!(matches!(
        rua::Message::decode(&bytes, CodecLimits::default())
            .unwrap()
            .message,
        rua::Message::Connect {
            domain: Domain::Cs,
            ..
        }
    ));
}

#[test]
fn real_gateway_retains_release_on_backpressure_and_publishes_one_domain_disconnect() {
    let (mut gateway, peer) = ready(3);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    drive(&mut gateway, 7);
    nas(&mut gateway, 0, Domain::Ps, 8).unwrap();
    drive(&mut gateway, 9);
    peer.block(true);
    for _ in 0..3 {
        nas(&mut gateway, 0, Domain::Ps, 10).unwrap();
    }
    peer.rua(rua::Message::Direct {
        context,
        domain: Domain::Cs,
        ranap: ranap::release::Command {
            cause: ranap::rab::Cause::from_code(83).unwrap(),
        }
        .encode(CodecLimits::default())
        .unwrap(),
    });
    let events = drive(&mut gateway, 11);
    let [Event::IuRelease { request, .. }] = events.as_slice() else {
        panic!("expected owned release")
    };
    let request = *request;
    assert!(gateway.owns_iu_release(request));
    for _ in 0..2 {
        assert_eq!(
            gateway.complete_iu_release_without_reports(time(12), request),
            Err(Error::Capacity)
        );
        assert!(gateway.owns_iu_release(request));
        assert_eq!(gateway.queued_frames(), 3);
    }
    peer.block(false);
    drive(&mut gateway, 13);
    gateway
        .complete_iu_release_without_reports(time(14), request)
        .unwrap();
    assert!(!gateway.owns_iu_release(request));
    assert_eq!(gateway.context(key(0)), Some(context));
    for at in 15..=17 {
        drive(&mut gateway, at);
    }
    let (_, ppid, bytes) = peer.last();
    assert_eq!(ppid, Protocol::Rua.ppid().unwrap());
    assert!(
        matches!(rua::Message::decode(&bytes, CodecLimits::default()).unwrap().message,
        rua::Message::Disconnect { context: actual, domain: Domain::Cs,
            cause: rua::Cause::Radio(rua::RadioCause::Normal), ranap: Some(ref bytes) }
            if actual == context && bytes == &[0x20, 1, 0, 3, 0, 0, 0])
    );
    nas(&mut gateway, 0, Domain::Ps, 18).unwrap();
    drive(&mut gateway, 19);
    let (_, _, bytes) = peer.last();
    assert!(matches!(
        rua::Message::decode(&bytes, CodecLimits::default())
            .unwrap()
            .message,
        rua::Message::Direct {
            domain: Domain::Ps,
            ..
        }
    ));
}
