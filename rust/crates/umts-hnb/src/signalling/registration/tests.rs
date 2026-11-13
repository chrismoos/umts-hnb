// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::signalling::tests::{COMPLETE, INITIAL, controller, establish, peer, request, send};
use crate::test_support::{at, source};
use umts_rrc_codec::decode_uplink_dedicated;
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{CnDomain, time::CHIPS_PER_SECOND};

const IDENTITY: [u8; 11] = [5, 0x59, 8, 0x09, 0x10, 0x10, 0, 0, 0, 0, 0x99];

#[test]
fn core_identity_is_owned_until_rrc_retirement_not_only_until_iu_release() {
    let source = source(false);
    let mut controller = controller(&source);
    let (mut link, core) = crate::core_link::tests::ready(&mut controller);
    let (key, _, _) =
        crate::core_link::tests::register(&mut link, &core, &mut controller, &source, 0, 3);
    let connection = controller.connection(key, CnDomain::Cs).unwrap();
    let imsi = Imsi::from_digits("991010000000001").unwrap();
    let initial_identity = controller.registry.get(key).unwrap().registration_imsi;
    controller
        .associate_core_identity(connection, imsi)
        .unwrap();
    assert_eq!(controller.permanent_identity(key), Some(imsi));
    assert_eq!(
        controller.registry.get(key).unwrap().registration_imsi,
        initial_identity
    );
    controller.core_registration_lost(key, RegistrationFailure::CoreUnavailable);
    assert!(controller.connection(key, CnDomain::Cs).is_none());
    assert_eq!(controller.permanent_identity(key), Some(imsi));
    controller.retire(key).unwrap();
    assert!(controller.registry.get(key).is_none());
    assert_eq!(
        controller.associate_core_identity(connection, imsi),
        Err(SignallingError::Origin)
    );
}

fn registration_request(imsi: Imsi) -> RegistrationRequest {
    RegistrationRequest {
        identity: RegistrationIdentity::Imsi(imsi),
        access_release: AccessRelease::R99,
        supports_csg: false,
        emergency: false,
    }
}

fn establish_unknown(controller: &mut CommonSignalling, source: &CommonTx) -> UeKey {
    let mut request = request(0);
    let ULCCCHMessageType::rrcConnectionRequest(body) = &mut request.value.message else {
        panic!("connection request");
    };
    body.initial_ue_identity =
        InitialUEIdentity::imei(ie::IMEI((0..15).map(|_| ie::IMEIDigit(0)).collect()));
    let key = controller.request(source, at(0), &request).unwrap();
    send(
        controller,
        key,
        SignallingBearer::ControlAm,
        &mut peer(source),
        &COMPLETE,
    );
    assert_eq!(controller.process_one(), Some(Ok(key)));
    key
}
fn direct(domain: CnDomain, nas: &[u8]) -> Vec<u8> {
    let message = class::ULDCCHMessage::new(
        None,
        class::ULDCCHMessageType::uplinkDirectTransfer(pdu::UplinkDirectTransfer::new(
            match domain {
                CnDomain::Cs => ie::CNDomainIdentity::cs_domain,
                CnDomain::Ps => ie::CNDomainIdentity::ps_domain,
            },
            ie::NASMessage(nas.to_vec().into()),
            None,
            None,
        )),
    );
    rasn::uper::encode(&message).unwrap()
}
fn deliver(controller: &mut CommonSignalling, key: UeKey, bytes: &[u8]) {
    controller
        .ingress
        .admit(key, SignallingBearer::NasAm, 1, bytes.len())
        .unwrap();
    controller
        .ingress
        .push_admitted(key, SignallingBearer::NasAm, at(0), bytes);
}

#[test]
fn identity_request_is_real_srb3_downlink_and_response_crosses_am_before_registration() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = establish_unknown(&mut controller, &source);
    let mut peer = peer(&source);
    send(
        &mut controller,
        key,
        SignallingBearer::NasAm,
        &mut peer,
        &INITIAL,
    );
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert!(controller.core.is_empty());
    let context = controller.registry.get_mut(key).unwrap();
    assert_eq!(context.registration, Registration::Identifying);
    assert_eq!(context.deferred.len(), 1);
    assert!(matches!(
        context.identification.as_ref().unwrap().phase(),
        Phase::Awaiting {
            transmissions: 1,
            ..
        }
    ));
    assert_eq!(
        controller.publish_bearer(key, SignallingBearer::NasAm, &mut source, at(0), at(0)),
        Ok(true)
    );
    send(
        &mut controller,
        key,
        SignallingBearer::NasAm,
        &mut peer,
        &direct(CnDomain::Cs, &IDENTITY),
    );
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert!(controller.core.is_empty());
    assert!(matches!(
        controller
            .registry
            .get(key)
            .unwrap()
            .identification
            .as_ref()
            .unwrap()
            .phase(),
        Phase::Ready(_)
    ));
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert_eq!(
        controller.registry.get(key).unwrap().registration,
        Registration::Pending
    );
    controller
        .core
        .publish_one(|event| {
            let CoreEventKind::Register { request } = event.kind else {
                panic!("registration")
            };
            assert_eq!(
                request.identity,
                RegistrationIdentity::Imsi(Imsi::from_digits("001010000000099").unwrap())
            );
            Ok::<_, ()>(())
        })
        .unwrap();
    let context_id = CoreContextId::new(77).unwrap();
    controller.registration_accepted(key, context_id).unwrap();
    assert_eq!(controller.process_one(), Some(Ok(key)));
    controller
        .core
        .publish_one(|event| {
            let CoreEventKind::Nas { nas, kind, .. } = event.kind else {
                panic!("original NAS")
            };
            assert_eq!(nas, &[5, 0x24]);
            assert_eq!(kind, umts_rrc::nas::CoreNasKind::Initial);
            Ok::<_, ()>(())
        })
        .unwrap();
    // The locally terminated IDENTITY RESPONSE is not replaced by an MM NULL towards the core.
    assert_eq!(controller.process_one(), None);
}

#[test]
fn registration_holds_both_domains_and_direct_nas_without_blocking_identity_response() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish_unknown(&mut controller, &source);
    deliver(&mut controller, key, &INITIAL);
    assert_eq!(controller.process_one(), Some(Ok(key)));
    let mut ps = INITIAL;
    ps[1] |= 0x80;
    deliver(&mut controller, key, &ps);
    assert_eq!(controller.process_one(), Some(Ok(key)));
    deliver(&mut controller, key, &direct(CnDomain::Cs, &[5, 0x31]));
    assert_eq!(controller.process_one(), Some(Ok(key)));
    deliver(&mut controller, key, &direct(CnDomain::Cs, &IDENTITY));
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert_eq!(controller.process_one(), Some(Ok(key)));
    controller.core.publish_one(|_| Ok::<_, ()>(())).unwrap();
    controller
        .registration_accepted(key, CoreContextId::new(7).unwrap())
        .unwrap();
    for (domain, kind, bytes) in [
        (
            CnDomain::Cs,
            umts_rrc::nas::CoreNasKind::Initial,
            &[5, 0x24][..],
        ),
        (
            CnDomain::Ps,
            umts_rrc::nas::CoreNasKind::Initial,
            &[5, 0x24][..],
        ),
        (
            CnDomain::Cs,
            umts_rrc::nas::CoreNasKind::Direct,
            &[5, 0x31][..],
        ),
    ] {
        assert_eq!(controller.process_one(), Some(Ok(key)));
        controller
            .core
            .publish_one(|event| {
                let CoreEventKind::Nas {
                    connection,
                    nas,
                    kind: actual,
                    ..
                } = event.kind
                else {
                    panic!("NAS")
                };
                assert_eq!(connection.key.domain, domain);
                assert_eq!(actual, kind);
                assert_eq!(nas, bytes);
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    assert_eq!(controller.process_one(), None);
}

#[test]
fn full_core_queue_cannot_restart_completed_identity_timer_or_lose_original() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish_unknown(&mut controller, &source);
    deliver(&mut controller, key, &INITIAL);
    assert_eq!(controller.process_one(), Some(Ok(key)));
    let imsi = Imsi::from_digits("001010000000099").unwrap();
    for _ in 0..controller.limits.core_entries.get() {
        controller
            .core
            .push_registration(key, registration_request(imsi))
            .unwrap();
    }
    deliver(&mut controller, key, &direct(CnDomain::Cs, &IDENTITY));
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::Capacity))
    );
    let context = controller.registry.get_mut(key).unwrap();
    poll_identification(context, at(20 * CHIPS_PER_SECOND)).unwrap();
    assert!(matches!(
        context.identification.as_ref().unwrap().phase(),
        Phase::Ready(_)
    ));
    assert_eq!(context.deferred.len(), 1);
    controller.core.publish_one(|_| Ok::<_, ()>(())).unwrap();
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert_eq!(
        controller.registry.get(key).unwrap().registration,
        Registration::Pending
    );
}

#[test]
fn no_identity_and_timer_expiry_require_explicit_radio_release_not_fake_registration() {
    for unavailable in [false, true] {
        let source = source(false);
        let mut controller = controller(&source);
        let key = establish_unknown(&mut controller, &source);
        deliver(&mut controller, key, &INITIAL);
        assert_eq!(controller.process_one(), Some(Ok(key)));
        if unavailable {
            deliver(
                &mut controller,
                key,
                &direct(CnDomain::Cs, &[5, 0x19, 1, 0]),
            );
            assert_eq!(controller.process_one(), Some(Ok(key)));
            assert_eq!(controller.process_one(), Some(Ok(key)));
        } else {
            poll_identification(
                controller.registry.get_mut(key).unwrap(),
                at(12 * CHIPS_PER_SECOND),
            )
            .unwrap();
        }
        assert_eq!(
            controller.registration_failure(key),
            Some(if unavailable {
                RegistrationFailure::IdentityUnavailable
            } else {
                RegistrationFailure::IdentityTimeout
            })
        );
        assert!(controller.core.is_empty());
        assert_eq!(controller.phase(key), Some(EstablishmentPhase::Established));
        assert!(controller.registry.get(key).unwrap().router.is_none());
    }
}

#[test]
fn deferred_capacity_and_publication_failure_preserve_fifo_and_router_state() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    deliver(&mut controller, key, &INITIAL);
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    for _ in 1..controller.limits.ingress_entries.get() {
        deliver(&mut controller, key, &direct(CnDomain::Cs, &[5, 0x31]));
        assert_eq!(controller.process_one(), Some(Ok(key)));
    }
    deliver(&mut controller, key, &direct(CnDomain::Cs, &[5, 0x32]));
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::Capacity))
    );
    controller.core.publish_one(|_| Ok::<_, ()>(())).unwrap();
    controller
        .registration_accepted(key, CoreContextId::new(77).unwrap())
        .unwrap();
    let imsi = controller
        .registry
        .get(key)
        .unwrap()
        .registration_imsi
        .unwrap();
    for _ in 0..controller.limits.core_entries.get() {
        controller
            .core
            .push_registration(key, registration_request(imsi))
            .unwrap();
    }
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::Capacity))
    );
    assert_eq!(controller.connection(key, CnDomain::Cs), None);
    assert_eq!(
        controller.registry.get(key).unwrap().deferred.len(),
        controller.limits.ingress_entries.get()
    );
    controller.core.publish_one(|_| Ok::<_, ()>(())).unwrap();
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert!(controller.connection(key, CnDomain::Cs).is_some());
}

#[test]
fn literal_identity_response_is_not_delivered_to_core_nas() {
    let bytes = direct(CnDomain::Cs, &IDENTITY);
    let decoded = decode_uplink_dedicated(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
    let inspected = inspect_uplink_nas(SignallingBearer::NasAm, &decoded, false).unwrap();
    assert_eq!(inspected.nas(), &IDENTITY);
    assert!(inspected.initial().is_none());
}

#[test]
fn ready_identity_under_core_backpressure_does_not_block_peer_rrc_completion() {
    let source = source(false);
    let mut controller = controller(&source);
    let first = establish_unknown(&mut controller, &source);
    deliver(&mut controller, first, &INITIAL);
    assert_eq!(controller.process_one(), Some(Ok(first)));
    for _ in 0..controller.limits.core_entries.get() {
        controller
            .core
            .push_registration(
                first,
                registration_request(Imsi::from_digits("001010000000099").unwrap()),
            )
            .unwrap();
    }
    deliver(&mut controller, first, &direct(CnDomain::Cs, &IDENTITY));
    assert_eq!(controller.process_one(), Some(Ok(first)));
    let second = establish(&mut controller, &source, 1);
    assert_eq!(
        controller.phase(second),
        Some(EstablishmentPhase::Established)
    );
    assert_eq!(controller.registry.get(first).unwrap().deferred.len(), 1);
}

#[test]
fn duplicate_identity_while_gateway_registration_pending_is_consumed_locally() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish_unknown(&mut controller, &source);
    deliver(&mut controller, key, &INITIAL);
    assert_eq!(controller.process_one(), Some(Ok(key)));
    deliver(&mut controller, key, &direct(CnDomain::Cs, &IDENTITY));
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert_eq!(controller.process_one(), Some(Ok(key)));
    deliver(&mut controller, key, &direct(CnDomain::Cs, &IDENTITY));
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert_eq!(controller.registry.get(key).unwrap().deferred.len(), 1);
    assert_eq!(controller.core.len(), 1);
}
