// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_am::{AmPayloadPublishError, CommonAmError},
    common_um::{CommonUmError, UmPayloadPublishError},
    signalling::ConnectionReleaseProgress,
    test_support::count,
    tx_evidence,
};
use std::num::NonZeroU64;
use umts_dsp::waveform::WaveformWindow;
use umts_rlc::{
    acknowledged::{AmEntity, AmLimits, ReceiveTime},
    unacknowledged::{EInterpretation, LiWidth, UmConfig, UmLimits, UmTransmitter},
};
use umts_rrc::establishment::SignallingProfile;
use umts_rrc::nas::DownlinkPriority;
use umts_types::CnDomain;
use umts_types::time::SampleInstant;

fn bits(bytes: &[u8]) -> Bits<'_> {
    Bits::new(bytes, bytes.len() * BITS_PER_BYTE).unwrap()
}
fn receipt() -> (tx_evidence::TransportSubmission, tx_evidence::Writer) {
    let sample = |index| SampleInstant {
        epoch: at(0).epoch,
        index,
    };
    let (reader, writer) = tx_evidence::channel(sample(0)).unwrap();
    (
        reader.receipt(sample(0), sample(20), at(10)).unwrap(),
        writer,
    )
}
fn payload(
    radio: &mut CommonSignalling,
    key: UeKey,
    bearer: SignallingBearer,
    maximum: usize,
    transport: &tx_evidence::TransportSubmission,
) -> Option<Vec<u8>> {
    let mut output = None;
    let published = radio
        .publish_srb_payload(key, bearer, at(0), count(maximum), |bytes| {
            output = Some(bytes.to_vec());
            Ok(transport.clone())
        })
        .unwrap();
    assert_eq!(published, output.is_some());
    output
}
fn am_bytes(peer: &mut AmEntity, message: &[u8]) -> Vec<u8> {
    peer.enqueue(message).unwrap();
    peer.publish_next(at(0), |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap()
        .1
}
fn received(radio: &mut CommonSignalling, key: UeKey) -> Vec<(Vec<u8>, u64)> {
    radio
        .registry
        .get_mut(key)
        .unwrap()
        .control
        .deliver_with(key, |delivery| {
            Ok::<_, ()>(
                delivery
                    .covered_sdus()
                    .map(|(bytes, coverage)| (bytes.to_vec(), coverage.first_received().index))
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap()
        .unwrap()
}
fn common(radio: &CommonSignalling, key: UeKey, bearer: SignallingBearer) -> CommonHeader {
    CommonHeader::Dedicated {
        identity: CommonIdentity::Cell(radio.assigned(key).unwrap().crnti()),
        channel: logical(bearer).unwrap(),
    }
}

#[test]
fn raw_and_common_am_reception_share_the_existing_window_and_first_observation_time() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    radio
        .receive_srb_payload(
            key,
            SignallingBearer::Unacknowledged,
            ReceiveTime::new(at(10), at(10)).unwrap(),
            bits(&[1, 0xff]),
        )
        .unwrap();
    let mut sender = peer(&source);
    let _complete = am_bytes(&mut sender, &COMPLETE);
    let first = am_bytes(&mut sender, b"first");
    radio
        .receive_srb_payload(
            key,
            SignallingBearer::ControlAm,
            ReceiveTime::new(at(20), at(30)).unwrap(),
            bits(&first),
        )
        .unwrap();
    assert_eq!(received(&mut radio, key), [(b"first".to_vec(), 20)]);
    assert_eq!(
        radio
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions()
            .next_receive,
        2
    );
    let second = am_bytes(&mut sender, b"second");
    radio
        .receive_timed(
            key,
            ReceiveTime::new(at(40), at(50)).unwrap(),
            CommonPdu {
                header: common(&radio, key, SignallingBearer::ControlAm),
                payload: bits(&second),
            },
        )
        .unwrap();
    assert_eq!(received(&mut radio, key), [(b"second".to_vec(), 40)]);
    assert_eq!(
        radio
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions()
            .next_receive,
        3
    );
}

#[test]
fn raw_am_rejects_fixed_size_changes_before_advancing_state_or_invoking_protection() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let before = radio.registry.get(key).unwrap().control.window_positions();
    for length in [0, 1, 8, 512] {
        let bytes = vec![0; length];
        assert_eq!(
            radio.receive_srb_payload(
                key,
                SignallingBearer::ControlAm,
                ReceiveTime::simultaneous(at(0)),
                bits(&bytes)
            ),
            Err(SignallingError::Am(CommonAmError::Payload))
        );
    }
    assert_eq!(
        radio.registry.get(key).unwrap().control.window_positions(),
        before
    );
}

#[test]
fn raw_am_admission_failure_retains_sdu_receipt_and_exact_retry_bytes() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let bank = &mut radio.registry.get_mut(key).unwrap().nas_bearer;
    let tracked = bank
        .enqueue_tracked(key, bits(b"kept across transport pressure"))
        .unwrap();
    let before = bank.window_positions();
    let mut rejected = Vec::new();
    for _ in 0..2 {
        assert_eq!(
            radio.publish_srb_payload(key, SignallingBearer::NasAm, at(0), count(512), |bytes| {
                rejected.push(bytes.to_vec());
                Err(SignallingError::Capacity)
            }),
            Err(SignallingError::Capacity)
        );
        assert_eq!(
            radio
                .registry
                .get(key)
                .unwrap()
                .nas_bearer
                .window_positions(),
            before
        );
    }
    assert_eq!(rejected[0], rejected[1]);
    let (transport, _writer) = receipt();
    let accepted = payload(&mut radio, key, SignallingBearer::NasAm, 512, &transport).unwrap();
    assert_eq!(accepted, rejected[0]);
    let bank = &radio.registry.get(key).unwrap().nas_bearer;
    assert_eq!(
        bank.window_positions().next_transmit,
        before.next_transmit + 1
    );
    assert!(matches!(
        bank.transmission(key, tracked).unwrap(),
        umts_rlc::acknowledged::SduTransmission::InFlight { .. }
    ));
    assert_eq!(accepted.len(), bank.transmit_pdu_bytes().get());
}

#[test]
fn signalling_release_waits_for_nas_ack_not_only_transport_publication() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = crate::core_link::tests::ready(&mut radio);
    let (key, _, _) =
        crate::core_link::tests::register(&mut link, &core, &mut radio, &source, 0, 3);
    let connection = radio.connection(key, CnDomain::Cs).unwrap();
    radio
        .downlink_nas(connection, DownlinkPriority::Signalling, &[5, 0x31])
        .unwrap();
    assert_eq!(
        radio.notify_core_connection_release(key, CnDomain::Cs),
        Ok(ConnectionReleaseProgress::PendingNasDelivery)
    );
    assert_eq!(radio.connection(key, CnDomain::Cs), Some(connection));
    assert!(
        !radio
            .registry
            .get(key)
            .unwrap()
            .release_after_core_disconnect
    );
    let (transport, mut writer) = receipt();
    for _ in 0..4 {
        if !radio
            .registry
            .get(key)
            .unwrap()
            .nas_bearer
            .pending_new_data()
        {
            break;
        }
        assert!(payload(&mut radio, key, SignallingBearer::NasAm, 512, &transport).is_some());
    }
    let bank = &radio.registry.get(key).unwrap().nas_bearer;
    assert!(!bank.pending_new_data());
    assert_eq!(bank.window_positions().next_transmit, 1);
    assert!(writer.accepted(WaveformWindow {
        first_sample: SampleInstant {
            epoch: key.epoch,
            index: 0
        },
        next_sample: SampleInstant {
            epoch: key.epoch,
            index: 20
        },
        chip_origin_sample: SampleInstant {
            epoch: key.epoch,
            index: 0
        },
    }));
    assert_eq!(
        radio.notify_core_connection_release(key, CnDomain::Cs),
        Ok(ConnectionReleaseProgress::PendingNasDelivery)
    );
    // TS 25.322 STATUS header followed by ACK SUFI with LSN 1.
    const ACK_ONE: [u8; 3] = [0x02, 0x00, 0x10];
    let mut ack = vec![0; peer(&source).configuration().transmit_pdu_bytes.get()];
    ack[..ACK_ONE.len()].copy_from_slice(&ACK_ONE);
    radio
        .receive_srb_payload(
            key,
            SignallingBearer::NasAm,
            ReceiveTime::simultaneous(at(20)),
            bits(&ack),
        )
        .unwrap();
    assert_eq!(
        radio.notify_core_connection_release(key, CnDomain::Cs),
        Ok(ConnectionReleaseProgress::Complete)
    );
    assert_eq!(radio.connection(key, CnDomain::Cs), None);
    assert!(
        radio
            .registry
            .get(key)
            .unwrap()
            .release_after_core_disconnect
    );
    let candidate = radio.idle_candidate(key.slot, at(20)).unwrap();
    assert!(candidate.release_after_core_disconnect);
    assert_eq!(
        candidate.blocker,
        Some(crate::signalling::IdleBlocker::ControlTransmit)
    );
}

#[test]
fn raw_am_preparation_and_admission_errors_are_distinct_and_never_consume_data() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let bank = &mut radio.registry.get_mut(key).unwrap().nas_bearer;
    bank.enqueue(key, bits(b"retry")).unwrap();
    let before = bank.window_positions();
    assert_eq!(
        bank.publish_payload_mapped(
            key,
            at(0),
            |_| Err("mapping"),
            |_, _| Ok(()),
            |_| -> Result<tx_evidence::TransportSubmission, &str> { panic!("not admitted") }
        ),
        Err(AmPayloadPublishError::Preparation("mapping"))
    );
    assert_eq!(
        bank.publish_payload_mapped(
            key,
            at(0),
            |position| Ok(position as u32),
            |_, _| Err("cipher"),
            |_| -> Result<tx_evidence::TransportSubmission, &str> { panic!("not admitted") }
        ),
        Err(AmPayloadPublishError::Preparation("cipher"))
    );
    assert_eq!(
        bank.publish_payload_mapped(
            key,
            at(0),
            |position| Ok::<_, &str>(position as u32),
            |_, _| Ok(()),
            |_| Err::<tx_evidence::TransportSubmission, _>("queue")
        ),
        Err(AmPayloadPublishError::Admission("queue"))
    );
    assert_eq!(bank.window_positions(), before);
}

#[test]
fn raw_am_cannot_mutate_clear_headers_or_control_pdus_during_preparation() {
    for control in [false, true] {
        let source = source(false);
        let mut radio = controller(&source);
        let key = establish(&mut radio, &source, 0);
        let context = radio.registry.get_mut(key).unwrap();
        let bank = if control {
            &mut context.control
        } else {
            &mut context.nas_bearer
        };
        if !control {
            bank.enqueue(key, bits(b"data")).unwrap();
        }
        let before = bank.window_positions();
        let result = bank.publish_payload_mapped(
            key,
            at(0),
            |position| Ok::<_, ()>(position as u32),
            |_, bytes| {
                let index = if control { bytes.len() - 1 } else { 0 };
                bytes[index] ^= 1;
                Ok(())
            },
            |_| -> Result<tx_evidence::TransportSubmission, ()> {
                panic!("invalid plaintext admitted")
            },
        );
        assert_eq!(
            result,
            Err(AmPayloadPublishError::Bearer(CommonAmError::Payload))
        );
        assert_eq!(bank.window_positions(), before);
    }
}

#[test]
fn raw_status_evidence_requires_actual_submission_and_dies_with_its_writer() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let (transport, mut writer) = receipt();
    assert!(
        payload(
            &mut radio,
            key,
            SignallingBearer::ControlAm,
            512,
            &transport
        )
        .is_some()
    );
    let submission = radio
        .registry
        .get(key)
        .unwrap()
        .control
        .status_submission(key)
        .unwrap()
        .unwrap()
        .clone();
    assert!(!submission.transport.submitted());
    assert!(writer.accepted(WaveformWindow {
        first_sample: SampleInstant {
            epoch: key.epoch,
            index: 0
        },
        next_sample: SampleInstant {
            epoch: key.epoch,
            index: 20
        },
        chip_origin_sample: SampleInstant {
            epoch: key.epoch,
            index: 0
        },
    }));
    assert!(submission.transport.submitted());
    drop(writer);
    assert!(!submission.transport.viable());
    assert!(!submission.transport.submitted());
}

#[test]
fn raw_control_status_cannot_bypass_pending_upper_signalling_admission() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    radio
        .ingress
        .admit(key, SignallingBearer::ControlAm, 1, COMPLETE.len())
        .unwrap();
    radio
        .ingress
        .push_admitted(key, SignallingBearer::ControlAm, at(0), &COMPLETE);
    assert_eq!(
        radio.publish_srb_payload(
            key,
            SignallingBearer::ControlAm,
            at(0),
            count(512),
            |_| panic!("upper admission bypassed")
        ),
        Ok(false)
    );
    assert!(
        radio
            .registry
            .get(key)
            .unwrap()
            .control
            .status_submission(key)
            .unwrap()
            .is_none()
    );
    radio.ingress.cancel_for(key);
    let (transport, _writer) = receipt();
    assert!(
        payload(
            &mut radio,
            key,
            SignallingBearer::ControlAm,
            512,
            &transport
        )
        .is_some()
    );
}

#[test]
fn raw_reset_reuses_the_same_directional_cipher_policy_and_does_not_touch_peers() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let other = establish(&mut radio, &source, 1);
    let origin = activate(
        &mut radio,
        key,
        0,
        &[Direction::Downlink, Direction::Uplink],
    );
    let before_other = radio
        .registry
        .get(other)
        .unwrap()
        .control
        .window_positions();
    let reset = reset_pdu(&source);
    for _ in 0..2 {
        radio
            .receive_srb_payload(
                key,
                SignallingBearer::ControlAm,
                ReceiveTime::simultaneous(at(0)),
                bits(&reset),
            )
            .unwrap();
    }
    let context = radio.registry.get(key).unwrap();
    let positions = context.control.window_positions();
    for (direction, position) in [
        (Direction::Downlink, positions.next_transmit),
        (Direction::Uplink, positions.next_receive),
    ] {
        assert_eq!(
            context
                .protection
                .ciphers
                .selected(key, 2, direction, Position::Sequence(position)),
            Ok(CipherUse::Data {
                count: 1 << CipherMode::Acknowledged.sequence_bits(),
                origin: Some(origin)
            })
        );
    }
    assert_eq!(
        radio
            .registry
            .get(other)
            .unwrap()
            .control
            .window_positions(),
        before_other
    );
}

fn um_config() -> UmConfig {
    UmConfig {
        li_width: LiWidth::Seven,
        interpretation: EInterpretation::Normal,
    }
}
fn um_peer(source: &CommonTx) -> UmTransmitter {
    let (_, maximum_pdu_bytes) = crate::common_am::CommonAmBearer::sizes(source, 1).unwrap();
    UmTransmitter::new(
        um_config(),
        UmLimits {
            maximum_pdu_bytes,
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(4),
            maximum_buffer_bytes: usize::MAX,
        },
    )
    .unwrap()
}

#[test]
fn raw_um_handles_bounded_variable_pdus_without_weakening_common_transport_validation() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let mut sender = um_peer(&source);
    for (index, length) in [8, 12].into_iter().enumerate() {
        let message = [0x24, 0x30 + index as u8, 0x42];
        sender.enqueue(&message).unwrap();
        let raw = sender
            .transmit_with(length, |bytes| Ok::<_, ()>(bytes.to_vec()))
            .unwrap()
            .unwrap();
        assert_eq!(
            radio.receive(
                key,
                at(0),
                CommonPdu {
                    header: common(&radio, key, SignallingBearer::Unacknowledged),
                    payload: bits(&raw),
                }
            ),
            Err(SignallingError::Um(CommonUmError::Payload))
        );
        assert_eq!(
            radio
                .registry
                .get(key)
                .unwrap()
                .um
                .receive_position()
                .unwrap(),
            index as u64
        );
        radio
            .receive_srb_payload(
                key,
                SignallingBearer::Unacknowledged,
                ReceiveTime::simultaneous(at(0)),
                bits(&raw),
            )
            .unwrap();
        let view = radio.ingress.front_eligible(|_, _| true).unwrap();
        assert_eq!(view.bytes, message);
        assert_eq!(view.bearer, SignallingBearer::Unacknowledged);
        radio.ingress.pop();
    }
    assert_eq!(
        radio
            .registry
            .get(key)
            .unwrap()
            .um
            .receive_position()
            .unwrap(),
        2
    );
}

#[test]
fn raw_um_publication_retries_without_advancing_sn_and_fach_resumes_the_same_owner() {
    let mut source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    radio
        .registry
        .get_mut(key)
        .unwrap()
        .um
        .enqueue(key, bits(b"one"))
        .unwrap();
    let mut rejected = Vec::new();
    assert_eq!(
        radio.publish_srb_payload(
            key,
            SignallingBearer::Unacknowledged,
            at(0),
            count(8),
            |bytes| {
                rejected = bytes.to_vec();
                Err(SignallingError::Capacity)
            }
        ),
        Err(SignallingError::Capacity)
    );
    assert_eq!(radio.registry.get(key).unwrap().um.transmit_position(), 0);
    let (transport, _writer) = receipt();
    assert_eq!(
        payload(
            &mut radio,
            key,
            SignallingBearer::Unacknowledged,
            8,
            &transport
        )
        .unwrap(),
        rejected
    );
    assert_eq!(radio.registry.get(key).unwrap().um.transmit_position(), 1);
    radio
        .registry
        .get_mut(key)
        .unwrap()
        .um
        .enqueue(key, bits(b"two"))
        .unwrap();
    assert_eq!(
        radio.publish_bearer(
            key,
            SignallingBearer::Unacknowledged,
            &mut source,
            at(0),
            at(0)
        ),
        Ok(true)
    );
    assert_eq!(radio.registry.get(key).unwrap().um.transmit_position(), 2);
}

#[test]
fn raw_um_rejects_preparation_header_mutation_and_preserves_distinct_failures() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let bank = &mut radio.registry.get_mut(key).unwrap().um;
    bank.enqueue(key, bits(b"held")).unwrap();
    assert_eq!(
        bank.publish_payload_prepared(
            key,
            count(8),
            |_, bytes| {
                bytes[0] ^= 1;
                Ok::<_, ()>(())
            },
            |_| -> Result<(), ()> { panic!("changed header admitted") }
        ),
        Err(UmPayloadPublishError::Bearer(CommonUmError::Payload))
    );
    assert_eq!(
        bank.publish_payload_prepared(
            key,
            count(8),
            |_, _| Err::<(), _>("cipher"),
            |_| Ok::<_, &str>(())
        ),
        Err(UmPayloadPublishError::Preparation("cipher"))
    );
    assert_eq!(
        bank.publish_payload_prepared(key, count(8), |_, _| Ok::<_, &str>(()), |_| Err("queue")),
        Err(UmPayloadPublishError::Admission("queue"))
    );
    assert_eq!(bank.transmit_position(), 0);
}

#[test]
fn raw_downlink_uses_existing_cipher_positions_without_reinitializing_the_owner() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let (transport, _writer) = receipt();
    payload(
        &mut radio,
        key,
        SignallingBearer::ControlAm,
        512,
        &transport,
    )
    .unwrap();
    let origin = activate(&mut radio, key, 0, &[Direction::Downlink]);
    let (tx, rx) = crate::common_am::CommonAmBearer::sizes(&source, 1).unwrap();
    let mut plain_sender = AmEntity::new(
        SignallingProfile.network_am_configuration(tx, rx, 0, 0),
        AmLimits {
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(4),
            maximum_buffer_bytes: usize::MAX,
        },
        at(0),
    )
    .unwrap();
    for index in 0..2 {
        let message = [0x32, 0x40 + index, 0x5a];
        let plain = am_bytes(&mut plain_sender, &message);
        radio
            .registry
            .get_mut(key)
            .unwrap()
            .control
            .enqueue(key, bits(&message))
            .unwrap();
        let mut encrypted = payload(
            &mut radio,
            key,
            SignallingBearer::ControlAm,
            512,
            &transport,
        )
        .unwrap();
        assert_eq!(&encrypted[..2], &plain[..2]);
        assert_ne!(encrypted, plain);
        let context = radio.registry.get(key).unwrap();
        assert_eq!(
            context
                .protection
                .ciphers
                .apply(
                    key,
                    2,
                    Direction::Downlink,
                    Position::Sequence(u64::from(index)),
                    &mut encrypted,
                    plain.len() * BITS_PER_BYTE
                )
                .unwrap(),
            CipherUse::Data {
                count: u32::from(index),
                origin: Some(origin)
            }
        );
        assert_eq!(encrypted, plain);
        assert_eq!(
            context.control.window_positions().next_transmit,
            u64::from(index) + 1
        );
    }
}

#[test]
fn raw_srb_interfaces_reject_stale_owners_wrong_epochs_unconfigured_srb_and_small_grants() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    let stale = UeKey {
        generation: NonZeroU64::new(key.generation.get() + 1).unwrap(),
        ..key
    };
    assert_eq!(
        radio.receive_srb_payload(
            stale,
            SignallingBearer::ControlAm,
            ReceiveTime::simultaneous(at(0)),
            bits(&[0; 8])
        ),
        Err(SignallingError::Origin)
    );
    assert_eq!(
        radio.publish_srb_payload(
            stale,
            SignallingBearer::ControlAm,
            at(0),
            count(512),
            |_| panic!("stale owner admitted")
        ),
        Err(SignallingError::Origin)
    );
    let foreign = umts_types::time::ChipInstant {
        epoch: umts_types::RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 0,
    };
    assert_eq!(
        radio.receive_srb_payload(
            key,
            SignallingBearer::ControlAm,
            ReceiveTime::simultaneous(foreign),
            bits(&[0; 8])
        ),
        Err(SignallingError::Origin)
    );
    assert_eq!(
        radio.publish_srb_payload(
            key,
            SignallingBearer::ControlAm,
            foreign,
            count(512),
            |_| panic!("foreign time admitted")
        ),
        Err(SignallingError::Origin)
    );
    assert_eq!(
        radio.publish_srb_payload(
            key,
            SignallingBearer::LowPriorityNasAm,
            at(0),
            count(512),
            |_| panic!("unconfigured bearer admitted")
        ),
        Err(SignallingError::Mapping)
    );
    assert_eq!(
        radio.publish_srb_payload(
            key,
            SignallingBearer::ControlAm,
            at(0),
            count(1),
            |_| panic!("undersized grant admitted")
        ),
        Err(SignallingError::Capacity)
    );
}
