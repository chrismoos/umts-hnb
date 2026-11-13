// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{
        BearerId, BearerLimits, BearerProgress, DedicatedBearers, ServiceError, TransportBinding,
        TransportChannels,
    },
    dedicated_rx::{DecodedTransport, FrameAnchor, ReceiveIdentity, TransportDelivery},
};
use umts_dsp::{
    bits::Bit,
    coding::{ChannelCoding, CodedBlockFormat},
    convolutional::CodeRate,
    crc::TransportCrc,
    dedicated_transport::{
        CombinationDefinition, ConfiguredTfcs, TransportDefinition, TransportId,
    },
    interleaving::Rel99Tti,
    tfci::Tfci,
};
use umts_types::time::CHIPS_PER_FRAME;

fn bank(radio: &CommonSignalling, key: UeKey, generation: u64) -> DedicatedBearers {
    let mut endpoints = radio.signalling_endpoints(key).unwrap();
    let endpoint = endpoints.swap_remove(1);
    let size = radio
        .registry
        .get(key)
        .unwrap()
        .control
        .configuration()
        .receive_pdu_bytes
        .get()
        * BITS_PER_BYTE;
    let transport = TransportId::new(1).unwrap();
    let formats = Arc::new(
        ConfiguredTfcs::new(
            vec![TransportDefinition {
                id: transport,
                interval: Rel99Tti::Ms10,
                attribute: std::num::NonZeroU16::MIN,
                formats: vec![CodedBlockFormat {
                    block_bits: size,
                    blocks: 1,
                    crc: TransportCrc::Crc16,
                    coding: ChannelCoding::Convolutional(CodeRate::Half),
                }],
            }],
            vec![CombinationDefinition {
                tfci: Tfci::new(0).unwrap(),
                formats: vec![0],
            }],
            NonZeroUsize::new(100_000).unwrap(),
        )
        .unwrap(),
    );
    let identity = ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::new(generation).unwrap(),
    };
    let mut bank = DedicatedBearers::new(
        identity,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        0,
        formats,
        vec![TransportBinding {
            transport,
            channels: TransportChannels::Single(BearerId::new(2).unwrap()),
        }],
        vec![endpoint],
        BearerLimits {
            maximum_transport_bits: NonZeroUsize::new(4096).unwrap(),
            output_slots: NonZeroUsize::MIN,
            maximum_sdu_bytes: radio.limits.maximum_sdu_bytes,
            maximum_buffer_bytes: 1_000_000,
        },
    )
    .unwrap();
    bank.activate(identity).unwrap();
    bank
}

fn service(
    radio: &mut CommonSignalling,
    bank: &mut DedicatedBearers,
    tick: u64,
    admit: bool,
) -> Result<BearerProgress, ServiceError<SignallingError>> {
    let key = bank.identity().ue;
    radio
        .with_bearer_access(key, |ciphers, reset, receiver| {
            bank.service_routed(at(tick), ciphers, reset, |pdu, ciphers| {
                assert_eq!(pdu.identity.ue, key);
                assert_eq!(pdu.bearer, SignallingBearer::ControlAm);
                if !admit {
                    return Ok(false);
                }
                receiver
                    .receive(ciphers, pdu.bearer, pdu.time, pdu.payload)
                    .map(|_| true)
            })
        })
        .unwrap()
}

fn enqueue(bank: &mut DedicatedBearers, frame: u64, bytes: &[u8]) {
    let payload: Vec<_> = (0..bytes.len() * BITS_PER_BYTE)
        .map(|index| {
            Bit::from(bytes[index / BITS_PER_BYTE] & (1 << (7 - index % BITS_PER_BYTE)) != 0)
        })
        .collect();
    bank.enqueue_transport(TransportDelivery {
        decoded: DecodedTransport {
            identity: bank.identity(),
            transport: TransportId::new(1).unwrap(),
            format: 0,
            start: at(frame * CHIPS_PER_FRAME),
            block_bits: payload.len(),
            blocks: 1,
            crc_checked: true,
        },
        payload: &payload,
    })
    .unwrap();
}

#[test]
fn dedicated_srb_uses_original_am_owner_and_proves_new_configuration() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let (integrity, fresh) = install_integrity(&mut radio, key);
    let mut bank = bank(&radio, key, 2);
    assert!(!bank.owns_signalling_rlc());
    let mut procedure = procedure(&radio, key);
    let receipt = radio
        .publish_bearer_setup_on(&mut procedure, bank.identity(), at(0))
        .unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    sender
        .enqueue(&integrity::protected(
            &integrity,
            fresh,
            2,
            2,
            complete(procedure.transaction()),
        ))
        .unwrap();
    let mut frame = 0;
    while sender.pending_new_data() {
        let bytes = sender
            .publish_next(at(0), |publication| Ok::<_, ()>(publication.bytes.to_vec()))
            .unwrap()
            .unwrap()
            .1;
        enqueue(&mut bank, frame, &bytes);
        let tick = (frame + 1) * CHIPS_PER_FRAME;
        let before = radio.registry.get(key).unwrap().control.window_positions();
        assert_eq!(
            service(&mut radio, &mut bank, tick, false),
            Ok(BearerProgress::Backpressure)
        );
        assert_eq!(
            radio.registry.get(key).unwrap().control.window_positions(),
            before
        );
        assert_eq!(
            service(&mut radio, &mut bank, tick, true),
            Ok(BearerProgress::Pdu)
        );
        frame += 1;
    }
    let now = frame * CHIPS_PER_FRAME;
    radio
        .collect_one(key, SignallingBearer::ControlAm, at(now))
        .unwrap();
    assert_eq!(
        radio.process_one_at_with_control(
            at(now + 10),
            security_time(),
            |message, mut security| {
                assert_eq!(
                    message.protection(),
                    ControlProtection::Integrity {
                        origin: integrity.origin(),
                        count: 2
                    }
                );
                assert_eq!(
                    message.time().provenance(),
                    umts_rlc::acknowledged::ReceiveProvenance::Path(bank.identity().configuration)
                );
                message
                    .bearer_setup_response(&mut procedure, &mut security)
                    .map(|_| ())
            }
        ),
        Some(Ok(key))
    );
    assert_eq!(procedure.phase(), SetupPhase::Complete);
    assert!(!bank.process_one(|_| panic!("raw SRB must not be delivered as a user SDU")));
}

#[test]
fn old_and_new_routes_share_reassembly_but_cannot_forge_new_link_completion() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let mut old = bank(&radio, key, 1);
    let mut new = bank(&radio, key, 2);
    let mut procedure = procedure(&radio, key);
    let receipt = radio
        .publish_bearer_setup_on(&mut procedure, new.identity(), at(0))
        .unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    let ULDCCHMessageType::radioBearerSetupComplete(mut body) = complete(procedure.transaction())
    else {
        unreachable!();
    };
    body.later_non_critical_extensions = Some(
        pdu::RadioBearerSetupCompleteLaterNonCriticalExtensions::new(
            Some(rasn::types::BitString::repeat(false, 640)),
            None,
        ),
    );
    let bytes = clear(ULDCCHMessageType::radioBearerSetupComplete(body));
    sender.enqueue(&bytes).unwrap();
    let mut frame = 0;
    let mut data_pdus = 0;
    while sender.pending_new_data() {
        let (kind, bytes) = sender
            .publish_next(at(0), |publication| Ok::<_, ()>(publication.bytes.to_vec()))
            .unwrap()
            .unwrap();
        let first_data =
            matches!(kind, umts_rlc::acknowledged::PublicationKind::Data { .. }) && data_pdus == 0;
        if matches!(kind, umts_rlc::acknowledged::PublicationKind::Data { .. }) {
            data_pdus += 1;
        }
        let bank = if first_data { &mut old } else { &mut new };
        enqueue(bank, frame, &bytes);
        assert_eq!(
            service(&mut radio, bank, (frame + 1) * CHIPS_PER_FRAME, true),
            Ok(BearerProgress::Pdu)
        );
        radio
            .collect_one(
                key,
                SignallingBearer::ControlAm,
                at((frame + 1) * CHIPS_PER_FRAME),
            )
            .unwrap();
        frame += 1;
    }
    assert!(data_pdus > 1);
    assert_eq!(
        radio.process_one_at_with_control(
            at((frame + 1) * CHIPS_PER_FRAME),
            security_time(),
            |message, mut security| {
                assert_eq!(
                    message.time().provenance(),
                    umts_rlc::acknowledged::ReceiveProvenance::Mixed
                );
                message
                    .bearer_setup_response(&mut procedure, &mut security)
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Mapping))
    );
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
}
