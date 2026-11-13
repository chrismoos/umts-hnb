// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, source};
use umts_mac::{FddCommonChannel, decode_common, encode_common};
use umts_rlc::acknowledged::AmEntity;
use umts_rrc_codec::decode_uplink_common;
use umts_types::CnDomain;

use crate::test_support::capabilities as capability_fixture;

pub(crate) fn initial_dch_procedure(
    radio: &CommonSignalling,
    key: UeKey,
    now: ChipInstant,
) -> umts_rrc::dedicated::RadioBearerSetupProcedure {
    super::control::tests::procedure_at(radio, key, now)
}

pub(crate) const REQUEST: [u8; 10] = [0x20, 0x90, 0x01, 0x01, 0, 0, 0, 0, 0x01, 0x60];
// TS 25.331 setup completion, transaction 0, CS START 0x12345 and PS START 0x54321.
pub(crate) const COMPLETE: [u8; 7] = [0x48, 0x08, 0x48, 0xd1, 0x6a, 0x86, 0x42];
pub(crate) const INITIAL: [u8; 7] = [0x14, 0x10, 0, 0, 0x08, 0x29, 0x20];

pub(crate) fn limits() -> SignallingLimits {
    SignallingLimits {
        maximum_ues: NonZeroU32::new(8).unwrap(),
        maximum_sdu_bytes: count(1024),
        queued_sdus_per_bearer: count(4),
        ingress_entries: count(4),
        core_entries: count(4),
        response_guard_chips: NonZeroU64::new(38_400_000).unwrap(),
        drx_coefficient: 5,
        per_ue_cpu_ns_per_frame: 100_000,
        tmsi_registration: None,
        cell_resources: Resources {
            queue_bytes: 8 * 1024 * 1024,
            cpu_ns_per_frame: 8 * 100_000,
            ..Resources::default()
        },
    }
}
pub(crate) fn request(variant: u8) -> DecodedUplinkCommon {
    let mut bytes = REQUEST;
    bytes[7] = variant;
    decode_uplink_common(&bytes, 78).unwrap()
}
pub(crate) fn controller(source: &CommonTx) -> CommonSignalling {
    CommonSignalling::new(source, 1, 1, limits()).unwrap()
}
pub(crate) fn peer(source: &CommonTx) -> AmEntity {
    let (tx, rx) = CommonAmBearer::sizes(source, 1).unwrap();
    AmEntity::new(
        SignallingProfile.network_am_configuration(rx, tx, 0, 0),
        AmLimits {
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(4),
            maximum_buffer_bytes: 1024 * 1024,
        },
        at(0),
    )
    .unwrap()
}
pub(crate) fn send(
    controller: &mut CommonSignalling,
    key: UeKey,
    bearer: SignallingBearer,
    peer: &mut AmEntity,
    bytes: &[u8],
) {
    peer.enqueue(bytes).unwrap();
    let identity = controller.assigned(key).unwrap().crnti();
    peer.publish_next(at(0), |publication| {
        let mut mac = vec![0; publication.bytes.len() + 3];
        encode_common(
            FddCommonChannel::Rach,
            CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(identity),
                channel: logical(bearer).unwrap(),
            },
            Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE).unwrap(),
            &mut mac,
        )
        .unwrap();
        controller.receive(
            key,
            at(0),
            decode_common(
                FddCommonChannel::Rach,
                Bits::new(&mac, mac.len() * BITS_PER_BYTE).unwrap(),
            )
            .unwrap(),
        )
    })
    .unwrap()
    .unwrap();
    assert_eq!(controller.collect_one(key, bearer, at(0)), Ok(true));
}
pub(crate) fn establish(
    controller: &mut CommonSignalling,
    source: &CommonTx,
    variant: u8,
) -> UeKey {
    let key = controller
        .request(source, at(0), &request(variant))
        .unwrap();
    let mut peer = peer(source);
    send(
        controller,
        key,
        SignallingBearer::ControlAm,
        &mut peer,
        &COMPLETE,
    );
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert_eq!(controller.phase(key), Some(EstablishmentPhase::Established));
    key
}

#[test]
fn nas_waits_for_setup_complete_without_blocking_another_ue() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = radio.request(&source, at(0), &request(0)).unwrap();
    let mut nas_peer = peer(&source);
    send(
        &mut radio,
        key,
        SignallingBearer::NasAm,
        &mut nas_peer,
        &INITIAL,
    );
    assert_eq!(radio.process_one(), None);
    assert!(radio.ingress.pending(key, SignallingBearer::NasAm));
    let other = establish(&mut radio, &source, 1);
    assert_eq!(radio.phase(other), Some(EstablishmentPhase::Established));
    let mut control_peer = peer(&source);
    send(
        &mut radio,
        key,
        SignallingBearer::ControlAm,
        &mut control_peer,
        &COMPLETE,
    );
    assert_eq!(radio.process_one(), Some(Ok(key)));
    assert_eq!(
        radio.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    assert!(!radio.ingress.pending(key, SignallingBearer::NasAm));
}

#[test]
fn nas_during_rrc_release_stays_owned_without_reopening_or_blocking_a_peer() {
    let source = source(false);
    let mut radio = controller(&source);
    let closing = establish(&mut radio, &source, 0);
    let other = establish(&mut radio, &source, 1);
    radio
        .release_idle_rrc(
            closing,
            umts_rrc::mobility::ReleaseCompleteRetries::new(1).unwrap(),
        )
        .unwrap();
    let mut closing_peer = peer(&source);
    send(
        &mut radio,
        closing,
        SignallingBearer::NasAm,
        &mut closing_peer,
        &INITIAL,
    );
    assert_eq!(radio.process_one(), None);
    assert!(radio.ingress.pending(closing, SignallingBearer::NasAm));
    assert!(radio.registry.get(closing).unwrap().router.is_none());
    assert!(radio.core().is_empty());
    let mut other_peer = peer(&source);
    send(
        &mut radio,
        other,
        SignallingBearer::NasAm,
        &mut other_peer,
        &INITIAL,
    );
    assert_eq!(
        radio.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    assert!(radio.ingress.pending(closing, SignallingBearer::NasAm));
    assert!(!radio.ingress.pending(other, SignallingBearer::NasAm));
    radio.retire(closing).unwrap();
    assert!(!radio.ingress.pending(closing, SignallingBearer::NasAm));
    assert_eq!(radio.phase(other), Some(EstablishmentPhase::Established));
}

pub(crate) fn finish_registration(controller: &mut CommonSignalling, key: UeKey) {
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    assert_eq!(controller.process_one(), None);
    assert_eq!(controller.core().len(), 1);
    controller
        .core()
        .publish_one(|event| {
            assert_eq!(event.key, key);
            assert!(matches!(event.kind, CoreEventKind::Register { .. }));
            Ok::<_, ()>(())
        })
        .unwrap();
    controller
        .registration_accepted(key, CoreContextId::new(key.slot + 100).unwrap())
        .unwrap();
}

pub(crate) fn send_capability_report(
    controller: &mut CommonSignalling,
    key: UeKey,
    peer: &mut AmEntity,
) {
    send_capability_report_with_transaction(controller, key, peer, TransactionId::new(2).unwrap());
}

pub(crate) fn send_capability_report_with_transaction(
    controller: &mut CommonSignalling,
    key: UeKey,
    peer: &mut AmEntity,
    transaction: TransactionId,
) {
    let mut information = capability_fixture::information();
    information.rrc_transaction_identifier =
        Some(umts_rrc_schema::information_elements::RRCTransactionIdentifier(transaction.get()));
    send_capability_report_information(controller, key, peer, information);
}

pub(crate) fn send_capability_report_information(
    controller: &mut CommonSignalling,
    key: UeKey,
    peer: &mut AmEntity,
    information: umts_rrc_schema::pdu_definitions::UECapabilityInformation,
) {
    use umts_rrc_schema::class_definitions::{ULDCCHMessage, ULDCCHMessageType};
    let message = ULDCCHMessage::new(
        None,
        ULDCCHMessageType::ueCapabilityInformation(information),
    );
    let bytes = rasn::uper::encode(&message).unwrap();
    peer.enqueue(&bytes).unwrap();
    for _ in 0..64 {
        let publication = peer
            .publish_next(at(0), |publication| {
                let mut mac = vec![0; publication.bytes.len() + 3];
                encode_common(
                    FddCommonChannel::Rach,
                    CommonHeader::Dedicated {
                        identity: CommonIdentity::Cell(controller.assigned(key).unwrap().crnti()),
                        channel: logical(SignallingBearer::ControlAm).unwrap(),
                    },
                    Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE).unwrap(),
                    &mut mac,
                )
                .unwrap();
                controller.receive(
                    key,
                    at(0),
                    decode_common(
                        FddCommonChannel::Rach,
                        Bits::new(&mac, mac.len() * BITS_PER_BYTE).unwrap(),
                    )
                    .unwrap(),
                )
            })
            .unwrap();
        assert!(publication.is_some());
        if controller
            .collect_one(key, SignallingBearer::ControlAm, at(0))
            .unwrap()
        {
            return;
        }
    }
    panic!("capability SDU did not complete within the bounded transport window");
}

#[test]
fn capability_information_crosses_common_am_before_core_registration_and_is_per_ue() {
    for compact in [false, true] {
        let source = source(compact);
        let mut controller = controller(&source);
        let first = establish(&mut controller, &source, 0);
        let second = establish(&mut controller, &source, 1);
        assert!(controller.security_capabilities(first).unwrap().is_none());
        for key in [first, second] {
            controller
                .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
                .unwrap();
        }
        let mut peer = peer(&source);
        peer.enqueue(&COMPLETE).unwrap();
        peer.publish_next(at(0), |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        send_capability_report(&mut controller, first, &mut peer);
        assert_eq!(controller.process_one(), Some(Ok(first)));
        assert_eq!(controller.capabilities(first).unwrap().key(), first);
        assert!(
            controller
                .security_capabilities(first)
                .unwrap()
                .unwrap()
                .support()
                .uia1
        );
        assert!(controller.capabilities(second).is_none());
        assert_eq!(
            controller
                .registry
                .get(second)
                .unwrap()
                .capabilities
                .phase(),
            capabilities::ExchangePhase::AwaitingInformation
        );
        assert_eq!(
            controller.registry.get(first).unwrap().registration,
            Registration::Unregistered
        );
        assert!(controller.core().is_empty());
    }
}

#[test]
fn full_control_queue_does_not_start_capability_request_and_expiry_preserves_the_ue() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    for _ in 0..limits().queued_sdus_per_bearer.get() {
        controller
            .registry
            .get_mut(key)
            .unwrap()
            .control
            .enqueue(key, Bits::new(&[0], 8).unwrap())
            .unwrap();
    }
    assert_eq!(
        controller.request_capabilities(key, at(0), TransactionId::new(2).unwrap()),
        Err(SignallingError::Capacity)
    );
    assert_eq!(
        controller.registry.get(key).unwrap().capabilities.phase(),
        capabilities::ExchangePhase::Idle
    );
    controller
        .publish_bearer(key, SignallingBearer::ControlAm, &mut source, at(0), at(0))
        .unwrap();
    controller
        .publish_bearer(
            key,
            SignallingBearer::ControlAm,
            &mut source,
            at(0),
            at(umts_types::time::CHIPS_PER_FRAME),
        )
        .unwrap();
    controller
        .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
        .unwrap();
    assert_eq!(
        controller.expire_some(
            at(limits().response_guard_chips.get()),
            NonZeroU32::new(8).unwrap()
        ),
        Ok(0)
    );
    assert_eq!(controller.counters().capability_timeouts, 1);
    assert_eq!(controller.phase(key), Some(EstablishmentPhase::Established));
}

#[test]
fn allocated_setup_am_completion_nas_core_queue_and_fach_downlink_are_connected() {
    for compact in [false, true] {
        let mut source = source(compact);
        let mut controller = controller(&source);
        let key = establish(&mut controller, &source, 0);
        let context = controller.registry.get(key).unwrap();
        assert_eq!(
            context.setup.starts().unwrap(),
            umts_rrc::establishment::DomainStarts {
                cs: Some(0x12345),
                ps: Some(0x54321)
            }
        );
        let mut peer = peer(&source);
        send(
            &mut controller,
            key,
            SignallingBearer::NasAm,
            &mut peer,
            &INITIAL,
        );
        finish_registration(&mut controller, key);
        assert_eq!(controller.process_one(), Some(Ok(key)));
        let connection = controller.connection(key, CnDomain::Cs).unwrap();
        assert_eq!(connection.key.ue, key);
        assert_eq!(
            controller
                .core()
                .publish_one(|_| Err::<(), _>("transport full")),
            Err("transport full")
        );
        assert_eq!(controller.core().len(), 1);
        assert_eq!(
            controller.core().publish_one(|event| {
                assert_eq!(event.key, key);
                let CoreEventKind::Nas {
                    kind,
                    connection: routed,
                    nas,
                    initial,
                    emergency,
                } = event.kind
                else {
                    panic!("NAS event");
                };
                assert_eq!(kind, umts_rrc::nas::CoreNasKind::Initial);
                assert!(!emergency);
                assert_eq!(routed, connection);
                assert_eq!(nas, &[0x05, 0x24]);
                assert!(initial.is_some());
                Ok::<_, ()>(())
            }),
            Ok(Some(()))
        );
        assert!(controller.core().is_empty());
        controller
            .downlink_nas(connection, DownlinkPriority::Sms, &[0x09, 0x01])
            .unwrap();
        assert_eq!(
            controller.publish_bearer(key, SignallingBearer::NasAm, &mut source, at(0), at(0)),
            Ok(true)
        );
        assert_eq!(controller.phase(key), Some(EstablishmentPhase::Established));
    }
}

#[test]
fn repeated_requests_share_one_reserved_context_and_timeout_does_not_reuse_air_identity() {
    let source = source(false);
    let mut controller = controller(&source);
    let shared = controller.resources();
    let first = controller.request(&source, at(0), &request(0)).unwrap();
    let reserved = controller.resources();
    let assigned = controller.assigned(first).unwrap();
    assert_eq!(controller.request(&source, at(1), &request(0)), Ok(first));
    assert_eq!(controller.resources(), reserved);
    assert_eq!(
        controller.expire_some(
            at(limits().response_guard_chips.get()),
            NonZeroU32::new(8).unwrap()
        ),
        Ok(1)
    );
    assert_eq!(controller.resources(), shared);
    assert_eq!(controller.phase(first), None);
    assert_eq!(controller.key_for_cell_id(assigned.crnti()), None);
    let next = controller
        .request(
            &source,
            at(limits().response_guard_chips.get()),
            &request(0),
        )
        .unwrap();
    assert_eq!(next.slot, first.slot);
    assert_ne!(next.generation, first.generation);
    assert_ne!(controller.assigned(next).unwrap().crnti(), assigned.crnti());
    assert_eq!(controller.counters().retransmissions, 1);
    assert_eq!(controller.counters().created, 2);
}

#[test]
fn failed_setup_publication_releases_resources_and_does_not_consume_radio_ids() {
    let source = source(true);
    let mut controller = controller(&source);
    for variant in 0..4 {
        controller
            .request(&source, at(0), &request(variant))
            .unwrap();
    }
    let before = controller.resources();
    let next = controller.next_radio_id;
    let result = controller.request(&source, at(0), &request(4));
    assert_eq!(
        result,
        Err(SignallingError::Common(CommonControlTxError::Rlc(
            umts_rlc::unacknowledged::UmError::Capacity
        )))
    );
    assert_eq!(controller.resources(), before);
    assert_eq!(controller.next_radio_id, next);
    assert_eq!(controller.identities.len(), 4);
    assert!(controller.registry.at_slot(4).is_none());
}

#[test]
fn core_backpressure_preserves_decoded_nas_and_does_not_create_domain_link() {
    let source = source(false);
    let mut options = limits();
    options.core_entries = count(1);
    let mut controller = CommonSignalling::new(&source, 1, 1, options).unwrap();
    let first = establish(&mut controller, &source, 0);
    let second = establish(&mut controller, &source, 1);
    let mut a = peer(&source);
    let mut b = peer(&source);
    send(
        &mut controller,
        first,
        SignallingBearer::NasAm,
        &mut a,
        &INITIAL,
    );
    finish_registration(&mut controller, first);
    assert_eq!(controller.process_one(), Some(Ok(first)));
    send(
        &mut controller,
        second,
        SignallingBearer::NasAm,
        &mut b,
        &INITIAL,
    );
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::Capacity))
    );
    assert_eq!(controller.connection(second, CnDomain::Cs), None);
    let allocated_contexts = controller.gateway_contexts.len();
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::Capacity))
    );
    assert_eq!(controller.gateway_contexts.len(), allocated_contexts);
    controller.core().publish_one(|_| Ok::<_, ()>(())).unwrap();
    finish_registration(&mut controller, second);
    assert_eq!(controller.process_one(), Some(Ok(second)));
    assert_ne!(
        controller.connection(first, CnDomain::Cs),
        controller.connection(second, CnDomain::Cs)
    );
    assert_eq!(controller.process_one(), None);
}

#[test]
fn radio_retirement_waits_for_core_admission_and_peer_survives_unchanged() {
    let source = source(false);
    let mut options = limits();
    options.core_entries = count(1);
    let mut controller = CommonSignalling::new(&source, 1, 1, options).unwrap();
    let first = establish(&mut controller, &source, 0);
    let second = establish(&mut controller, &source, 1);
    let mut peer = peer(&source);
    send(
        &mut controller,
        first,
        SignallingBearer::NasAm,
        &mut peer,
        &INITIAL,
    );
    finish_registration(&mut controller, first);
    controller.process_one().unwrap().unwrap();
    let before = controller.resources();
    assert_eq!(controller.retire(first), Err(SignallingError::Capacity));
    assert_eq!(controller.resources(), before);
    assert_eq!(
        controller.phase(first),
        Some(EstablishmentPhase::Established)
    );
    controller.core().publish_one(|_| Ok::<_, ()>(())).unwrap();
    controller.retire(first).unwrap();
    assert_eq!(controller.phase(first), None);
    assert_eq!(
        controller.phase(second),
        Some(EstablishmentPhase::Established)
    );
    controller
        .core()
        .publish_one(|event| {
            let CoreEventKind::RadioFailure {
                connections,
                context,
                registration_pending,
            } = event.kind
            else {
                panic!("radio failure");
            };
            assert_eq!(
                context.unwrap(),
                CoreContextId::new(first.slot + 100).unwrap()
            );
            assert!(!registration_pending);
            assert_eq!(event.key, first);
            assert_eq!(connections[0].unwrap().key.ue, first);
            assert!(connections[1].is_none());
            Ok::<_, ()>(())
        })
        .unwrap();
}

#[test]
fn admission_limits_wrong_epoch_and_bounded_expiry_leave_peer_resources_intact() {
    let source = source(true);
    let mut options = limits();
    options.cell_resources.queue_bytes = 1;
    assert!(matches!(
        CommonSignalling::new(&source, 1, 1, options),
        Err(SignallingError::Capacity)
    ));
    let mut controller = controller(&source);
    let incompatible = crate::test_support::source(false);
    let before = controller.resources();
    assert_eq!(
        controller.request(&incompatible, at(0), &request(0)),
        Err(SignallingError::Common(CommonControlTxError::Profile))
    );
    assert_eq!(controller.resources(), before);
    let first = controller.request(&source, at(0), &request(0)).unwrap();
    let second = controller.request(&source, at(0), &request(1)).unwrap();
    let timeout = at(limits().response_guard_chips.get());
    assert_eq!(controller.expire_some(timeout, NonZeroU32::MIN), Ok(1));
    assert_eq!(controller.phase(first), None);
    assert_eq!(
        controller.phase(second),
        Some(EstablishmentPhase::AwaitingComplete)
    );
    assert_eq!(controller.expire_some(timeout, NonZeroU32::MIN), Ok(1));
    assert_eq!(controller.phase(second), None);
}

#[test]
fn registration_pending_does_not_block_peer_setup_and_cs_ps_share_gateway_identity() {
    let source = source(false);
    let mut controller = controller(&source);
    let first = establish(&mut controller, &source, 0);
    let mut first_peer = peer(&source);
    send(
        &mut controller,
        first,
        SignallingBearer::NasAm,
        &mut first_peer,
        &INITIAL,
    );
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    let second = establish(&mut controller, &source, 1);
    assert_eq!(
        controller.phase(second),
        Some(EstablishmentPhase::Established)
    );
    controller.core().publish_one(|_| Ok::<_, ()>(())).unwrap();
    let gateway_id = CoreContextId::new(0xabcdef).unwrap();
    controller.registration_accepted(first, gateway_id).unwrap();
    assert_eq!(controller.process_one(), Some(Ok(first)));
    let cs = controller.connection(first, CnDomain::Cs).unwrap();
    controller.core().publish_one(|_| Ok::<_, ()>(())).unwrap();
    let mut ps_initial = INITIAL;
    ps_initial[1] |= 0x80;
    send(
        &mut controller,
        first,
        SignallingBearer::NasAm,
        &mut first_peer,
        &ps_initial,
    );
    assert_eq!(controller.process_one(), Some(Ok(first)));
    let ps = controller.connection(first, CnDomain::Ps).unwrap();
    assert_ne!(cs.key.domain, ps.key.domain);
    assert_eq!(cs.context, gateway_id);
    assert_eq!(ps.context, gateway_id);
    assert_eq!(controller.gateway_contexts.len(), 1);
    controller.registration_accepted(first, gateway_id).unwrap();
    controller.core().publish_one(|_| Ok::<_, ()>(())).unwrap();
    let mut second_peer = peer(&source);
    send(
        &mut controller,
        second,
        SignallingBearer::NasAm,
        &mut second_peer,
        &INITIAL,
    );
    assert_eq!(
        controller.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    assert_eq!(
        controller.registration_accepted(second, gateway_id),
        Err(SignallingError::Nas(NasError::Context))
    );
    assert_eq!(controller.connection(second, CnDomain::Cs), None);
}
