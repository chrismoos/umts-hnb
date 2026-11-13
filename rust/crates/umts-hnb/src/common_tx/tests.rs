// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    access::AccessObservation,
    test_support::{amplitudes, at, count, plan, source},
};
use umts_dsp::{
    convolutional::TerminatedDecoder, interleaving::Interleaver, rate_matching::RateMatching,
    scrambling::downlink_sequence, waveform::BroadcastWaveform,
};
use umts_types::time::SYSTEM_FRAME_MODULUS;

mod cipher_um;

#[test]
fn zero_remaining_dynamic_power_preserves_fixed_samples() {
    let fixed = vec![Complex32::new(0.125, -0.0625); CHIPS_PER_SLOT as usize];
    let dynamic = vec![Complex32::new(0.25, 0.5); fixed.len()];
    let mut output = fixed.clone();
    let (gain, _) = mix_dynamic(&mut output, &dynamic, 0.0).unwrap();
    assert_eq!(gain, 0.0);
    assert_eq!(output, fixed);
}

#[test]
fn dynamic_budget_preserves_fixed_components_and_bounds_only_dynamic_power() {
    let fixed = vec![Complex32::new(0.125, -0.0625); CHIPS_PER_SLOT as usize];
    for amplitude in [0.0_f32, 0.125, 0.5, 1.0] {
        let dynamic = vec![Complex32::new(amplitude, amplitude); fixed.len()];
        let mut output = fixed.clone();
        let (gain, _) = mix_dynamic(&mut output, &dynamic, 0.125).unwrap();
        let power = dynamic
            .iter()
            .map(|chip| f64::from((*chip * gain).norm_sqr()))
            .sum::<f64>()
            / dynamic.len() as f64;
        assert!(power <= 0.125);
        for ((actual, fixed), dynamic) in output.iter().zip(&fixed).zip(&dynamic) {
            assert_eq!(*actual, *fixed + *dynamic * gain);
        }
        if amplitude <= 0.125 {
            assert_eq!(gain, 1.0);
        }
    }
    let mut output = fixed.clone();
    let mut invalid = fixed.clone();
    invalid[1].re = f32::NAN;
    assert!(mix_dynamic(&mut output, &invalid, 0.125).is_err());
    assert_eq!(output, fixed);
}

#[test]
fn crest_reduction_adds_delay_to_waveform_receipts_without_changing_safe_samples() {
    let mut reference = source(false);
    let mut reduced = source(false);
    reduced.set_component_peak_limit(0.9).unwrap();
    reduced.enable_crest_reduction(SamplesPerChip::Two).unwrap();
    let delay = reduced.group_delay_samples() - reference.group_delay_samples();
    assert_eq!(delay, 256);
    let mut clean = Vec::new();
    let mut candidate = Vec::new();
    for _ in 0..3 {
        let mut a = vec![Complex32::default(); reference.samples_per_slot()];
        let mut b = a.clone();
        let before = reference.generate_slot(&mut a).unwrap();
        let after = reduced.generate_slot(&mut b).unwrap();
        assert_eq!(before.first_sample, after.first_sample);
        assert_eq!(before.next_sample, after.next_sample);
        assert_eq!(
            after.chip_origin_sample.index,
            before.chip_origin_sample.index + delay as u64
        );
        assert!(!reduced.take_crest_rejected());
        clean.extend(a);
        candidate.extend(b);
    }
    assert_eq!(&candidate[delay..], &clean[..clean.len() - delay]);
    assert_eq!(
        reduced.enable_crest_reduction(SamplesPerChip::Two),
        Err(CommonTxError::Conflict)
    );
}

#[test]
fn final_component_guard_preserves_safe_samples_and_rejects_before_publication() {
    let mut reference = source(true);
    let mut guarded = source(true);
    guarded.set_component_peak_limit(0.9).unwrap();
    let mut expected = vec![Complex32::default(); reference.samples_per_slot()];
    let mut actual = expected.clone();
    assert_eq!(
        reference.generate_slot(&mut expected),
        guarded.generate_slot(&mut actual)
    );
    assert_eq!(actual, expected);
    let mut unsafe_source = source(true);
    unsafe_source.set_component_peak_limit(0.0001).unwrap();
    let start = unsafe_source.next_chip();
    assert_eq!(
        unsafe_source.generate_slot(&mut actual),
        Err(CommonTxError::ComponentPeak)
    );
    assert_eq!(unsafe_source.next_chip(), start);
    assert_eq!(
        unsafe_source.generate_slot(&mut actual),
        Err(CommonTxError::Poisoned)
    );
}

fn prepared_frame(source: &CommonTx) -> Vec<Complex32> {
    let last_slot = source.next.index.checked_sub(CHIPS_PER_SLOT).unwrap();
    let frame = ChipInstant {
        epoch: source.next.epoch,
        index: last_slot / CHIPS_PER_FRAME * CHIPS_PER_FRAME,
    };
    let mut chips = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    source
        .common
        .compose_frame(source.broadcast.frame_at(frame).unwrap(), &mut chips)
        .unwrap();
    for stream in &source.secondary {
        stream
            .generator
            .add_frame(&stream.data, Some(stream.selected_tfci), &mut chips)
            .unwrap();
    }
    chips
}

fn decode_common(
    chips: &[Complex32],
    channel: &SecondaryChannel,
    cell: &CellInformation,
    payload: &[Bit],
) {
    assert_eq!(recover_common(chips, channel, cell, payload.len()), payload);
}

fn recover_common(
    chips: &[Complex32],
    channel: &SecondaryChannel,
    cell: &CellInformation,
    payload_bits: usize,
) -> Vec<Bit> {
    let sf = usize::from(channel.settings().code.spreading_factor());
    let mut code = vec![0.0; sf];
    channel.settings().code.write(&mut code).unwrap();
    let mut scrambling = vec![Complex32::default(); chips.len()];
    downlink_sequence(cell.primary_code().sequence_number(), &mut scrambling).unwrap();
    let mut data = Vec::new();
    let mut control = Vec::new();
    let control_bits = if sf >= 128 { 2 } else { 8 };
    for slot in 0..15 {
        for symbol in 0..2560 / sf {
            let start = slot * 2560 + symbol * sf;
            let value = (0..sf)
                .map(|chip| chips[start + chip] * scrambling[start + chip].conj() * code[chip])
                .sum::<Complex32>();
            for (branch, value) in [value.re, value.im].into_iter().enumerate() {
                if symbol * 2 + branch < control_bits {
                    control.push(value);
                } else {
                    data.push(value);
                }
            }
        }
    }
    for (index, value) in control.iter().enumerate() {
        assert_eq!(
            *value < 0.0,
            Tfci::new(1).unwrap().encode()[index % 32] == Bit::One
        );
    }
    let mut matched = vec![0.0; data.len()];
    Interleaver::second(data.len())
        .unwrap()
        .deinterleave(&data, &mut matched)
        .unwrap();
    let mut coded = vec![0.0; (payload_bits + 12 + 8) * 2];
    RateMatching::downlink_convolutional_flexible(coded.len(), matched.len())
        .unwrap()
        .combine_llrs(&matched, &mut coded)
        .unwrap();
    let mut decoded = vec![Bit::Zero; payload_bits + 12];
    TerminatedDecoder::new(CodeRate::Half)
        .decode(&coded, &mut decoded)
        .unwrap();
    assert!(
        TransportCrc::Crc12
            .verify(&decoded[..payload_bits], &decoded[payload_bits..])
            .unwrap()
    );
    decoded.truncate(payload_bits);
    decoded
}

pub(crate) fn recover_current_fach(source: &CommonTx) -> Vec<u8> {
    let width = usize::from(source.information().common_layout().transport_block_bits());
    let bits = recover_common(
        &prepared_frame(source),
        &source.information().sib5().secondary_channels()[1],
        source.information(),
        width,
    );
    let mut bytes = vec![0; width / umts_types::bits::BITS_PER_BYTE];
    umts_dsp::bits::pack_msb(&bits, &mut bytes).unwrap();
    bytes
}

#[test]
fn queued_um_common_control_recovers_through_composite_fach_crc_mac_and_reassembly() {
    use crate::common_control_tx::CommonControlDownlink;
    use umts_mac::{CommonHeader, FddCommonChannel, decode_common as decode_mac};
    use umts_rlc::unacknowledged::{EInterpretation, LiWidth, UmConfig, UmLimits, UmReceiver};
    use umts_types::bits::Bits;
    for compact in [false, true] {
        let mut source = source(compact);
        let cfg = UmConfig {
            li_width: LiWidth::Seven,
            interpretation: EInterpretation::Normal,
        };
        let limits = UmLimits {
            maximum_pdu_bytes: CommonControlDownlink::maximum_pdu_bytes(&source, 1).unwrap(),
            maximum_sdu_bytes: count(512),
            queued_sdus: count(8),
            maximum_buffer_bytes: 16 * 1024,
        };
        let mut downlink = CommonControlDownlink::new(&source, 1, limits).unwrap();
        let mut receiver = UmReceiver::new(cfg, limits).unwrap();
        let mut expected = Vec::new();
        for length in [1, 100, 2, 256] {
            let payload: Vec<_> = (0..length).map(|byte| (byte * 29 + length) as u8).collect();
            downlink
                .enqueue(Bits::new(&payload, payload.len() * 8).unwrap())
                .unwrap();
            expected.push(payload);
        }
        downlink
            .enqueue(Bits::new(&[0xab, 0xcf], 13).unwrap())
            .unwrap();
        expected.push(vec![0xab, 0xc8]);
        let mut actual = Vec::new();
        let mut frame = 0;
        let mut samples = vec![Complex32::default(); source.samples_per_slot()];
        while downlink.has_pending() {
            assert!(frame < 100);
            assert!(
                downlink
                    .publish_frame(&mut source, at(frame * CHIPS_PER_FRAME))
                    .unwrap()
            );
            source.generate_slot(&mut samples).unwrap();
            let payload_bits =
                usize::from(source.information().common_layout().transport_block_bits());
            let payload = recover_common(
                &prepared_frame(&source),
                &source.information().sib5().secondary_channels()[1],
                source.information(),
                payload_bits,
            );
            let mut bytes = vec![0; payload_bits / 8];
            umts_dsp::bits::pack_msb(&payload, &mut bytes).unwrap();
            let mac = decode_mac(
                FddCommonChannel::Fach,
                Bits::new(&bytes, payload_bits).unwrap(),
            )
            .unwrap();
            assert_eq!(mac.header, CommonHeader::CommonControl);
            let mut rlc = vec![0; mac.payload.len() / 8];
            mac.payload.copy_to(&mut rlc).unwrap();
            actual.extend(receiver.receive(&rlc).unwrap().sdus().map(<[u8]>::to_vec));
            for _ in 1..15 {
                source.generate_slot(&mut samples).unwrap();
            }
            frame += 1;
        }
        assert_eq!(actual, expected);
        assert_eq!(receiver.counters().gaps, 0);
    }
}

#[test]
fn r99_setup_publication_reaches_common_waveform_but_requires_dedicated_completion_to_establish() {
    use crate::common_control_tx::CommonControlDownlink;
    use std::num::NonZeroU64;
    use umts_mac::{FddCommonChannel, decode_common as decode_mac};
    use umts_rlc::unacknowledged::{EInterpretation, LiWidth, UmConfig, UmLimits, UmReceiver};
    use umts_rrc::establishment::{
        AssignedRntis, CellFachSetup, Completion, Establishment, EstablishmentPhase,
        SignallingProfile, TransactionId,
    };
    use umts_rrc_codec::{decode_downlink_common, decode_uplink_common, decode_uplink_dedicated};
    use umts_types::{UeKey, bits::Bits};

    let request =
        decode_uplink_common(&[0x20, 0x90, 0x01, 0x01, 0, 0, 0, 0, 0x01, 0x60], 78).unwrap();
    let key = UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    };
    for compact in [false, true] {
        let mut source = source(compact);
        let parameters = CellFachSetup {
            assigned: AssignedRntis::new(1, 0x12345, 0x2345).unwrap(),
            transaction: TransactionId::new(2).unwrap(),
            drx_coefficient: 5,
            signalling: SignallingProfile,
        };
        let mut establishment = Establishment::prepare(key, at(0), &request, &parameters).unwrap();
        let limits = UmLimits {
            maximum_pdu_bytes: CommonControlDownlink::maximum_pdu_bytes(&source, 1).unwrap(),
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(4),
            maximum_buffer_bytes: 8192,
        };
        let mut downlink = CommonControlDownlink::new(&source, 1, limits).unwrap();
        let mut receiver = UmReceiver::new(
            UmConfig {
                li_width: LiWidth::Seven,
                interpretation: EInterpretation::Normal,
            },
            limits,
        )
        .unwrap();
        establishment
            .publish_with(at(0), NonZeroU64::new(3_840_000).unwrap(), |encoded| {
                downlink.enqueue(Bits::new(&encoded.bytes, encoded.bit_length).unwrap())
            })
            .unwrap();
        assert_eq!(establishment.phase(), EstablishmentPhase::AwaitingComplete);
        let mut sdus = Vec::new();
        let mut samples = vec![Complex32::default(); source.samples_per_slot()];
        let mut frame = 0;
        while downlink.has_pending() {
            assert!(frame < 100);
            downlink
                .publish_frame(&mut source, at(frame * CHIPS_PER_FRAME))
                .unwrap();
            source.generate_slot(&mut samples).unwrap();
            let width = usize::from(source.information().common_layout().transport_block_bits());
            let payload = recover_common(
                &prepared_frame(&source),
                &source.information().sib5().secondary_channels()[1],
                source.information(),
                width,
            );
            let mut bytes = vec![0; width / 8];
            umts_dsp::bits::pack_msb(&payload, &mut bytes).unwrap();
            let mac =
                decode_mac(FddCommonChannel::Fach, Bits::new(&bytes, width).unwrap()).unwrap();
            let mut pdu = vec![0; mac.payload.len() / 8];
            mac.payload.copy_to(&mut pdu).unwrap();
            sdus.extend(receiver.receive(&pdu).unwrap().sdus().map(<[u8]>::to_vec));
            for _ in 1..15 {
                source.generate_slot(&mut samples).unwrap();
            }
            frame += 1;
        }
        assert_eq!(sdus, vec![establishment.response().bytes.clone()]);
        assert_eq!(
            decode_downlink_common(&sdus[0], sdus[0].len() * 8)
                .unwrap()
                .consumed_bits,
            establishment.response().bit_length
        );
        assert_eq!(establishment.phase(), EstablishmentPhase::AwaitingComplete);
        use crate::common_am::CommonAmBearer;
        use umts_mac::{CommonHeader, CommonIdentity, LogicalChannelId, encode_common};
        use umts_rlc::acknowledged::{AmEntity, AmLimits, AmReceive, PublicationKind};
        let (downlink_size, uplink_size) = CommonAmBearer::sizes(&source, 1).unwrap();
        let network_config =
            SignallingProfile.network_am_configuration(downlink_size, uplink_size, 0, 0);
        let peer_config = umts_rlc::acknowledged::AmConfig {
            transmit_pdu_bytes: uplink_size,
            receive_pdu_bytes: downlink_size,
            ..network_config
        };
        let limits = AmLimits {
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(4),
            maximum_buffer_bytes: 1024 * 1024,
        };
        let logical = LogicalChannelId::new(2).unwrap();
        let mut network = CommonAmBearer::new(
            &source,
            1,
            key,
            parameters.assigned.crnti(),
            logical,
            crate::common_am::CommonAmSettings {
                config: network_config,
                limits,
                started_at: source.next_chip(),
            },
        )
        .unwrap();
        let now = at(frame * CHIPS_PER_FRAME);
        let mut peer = AmEntity::new(peer_config, limits, now).unwrap();
        peer.enqueue(&[0x48, 0x48, 0x48, 0xd1, 0x6a, 0x86, 0x42])
            .unwrap();
        let mut rach = vec![0; uplink_size.get() + 3];
        peer.publish_next(now, |publication| {
            encode_common(
                FddCommonChannel::Rach,
                CommonHeader::Dedicated {
                    identity: CommonIdentity::Cell(parameters.assigned.crnti()),
                    channel: logical,
                },
                Bits::new(publication.bytes, publication.bytes.len() * 8).unwrap(),
                &mut rach,
            )
        })
        .unwrap()
        .unwrap();
        let mac = decode_mac(
            FddCommonChannel::Rach,
            Bits::new(&rach, rach.len() * 8).unwrap(),
        )
        .unwrap();
        assert_eq!(network.receive(key, now, mac), Ok(AmReceive::Stored));
        network
            .deliver_with(key, |delivery| {
                assert_eq!(delivery.count(), 1);
                for sdu in delivery.sdus() {
                    let completion = decode_uplink_dedicated(sdu, sdu.len() * 8).unwrap();
                    assert_eq!(
                        establishment.complete(key, now, &completion),
                        Ok(Completion::Established)
                    );
                }
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(establishment.starts().unwrap().cs, Some(0x12345));
        assert_eq!(establishment.starts().unwrap().ps, Some(0x54321));
        assert_eq!(
            network.publish_frame(&mut source, now, now),
            Ok(Some(PublicationKind::Status))
        );
        source.generate_slot(&mut samples).unwrap();
        let width = usize::from(source.information().common_layout().transport_block_bits());
        let payload = recover_common(
            &prepared_frame(&source),
            &source.information().sib5().secondary_channels()[1],
            source.information(),
            width,
        );
        let mut bytes = vec![0; width / 8];
        umts_dsp::bits::pack_msb(&payload, &mut bytes).unwrap();
        let mac = decode_mac(FddCommonChannel::Fach, Bits::new(&bytes, width).unwrap()).unwrap();
        assert_eq!(
            mac.header,
            CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(parameters.assigned.crnti()),
                channel: logical
            }
        );
        let mut acknowledgement = vec![0; downlink_size.get()];
        mac.payload.copy_to(&mut acknowledgement).unwrap();
        assert_eq!(
            peer.receive(now, &acknowledgement),
            Ok(AmReceive::StatusApplied)
        );
        assert_eq!(peer.outstanding_pdus(), 0);
    }
}

#[test]
fn common_am_queue_failure_and_wrong_ue_mapping_do_not_advance_peer_bearers() {
    use crate::common_am::{CommonAmBearer, CommonAmError};
    use std::num::NonZeroU64;
    use umts_mac::{CommonHeader, CommonIdentity, CommonPdu, LogicalChannelId};
    use umts_rlc::acknowledged::{AmLimits, PublicationKind};
    use umts_rrc::establishment::SignallingProfile;
    use umts_types::{UeKey, bits::Bits};
    let mut source = source(false);
    let key = UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    };
    let peer_key = UeKey { slot: 1, ..key };
    let stale = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key
    };
    let logical = LogicalChannelId::new(2).unwrap();
    let (tx, rx) = CommonAmBearer::sizes(&source, 1).unwrap();
    let config = SignallingProfile.network_am_configuration(tx, rx, 0, 0);
    let limits = AmLimits {
        maximum_sdu_bytes: count(256),
        queued_sdus: count(4),
        maximum_buffer_bytes: 1024 * 1024,
    };
    let mut one = CommonAmBearer::new(
        &source,
        1,
        key,
        0x2345,
        logical,
        crate::common_am::CommonAmSettings {
            config,
            limits,
            started_at: at(0),
        },
    )
    .unwrap();
    let mut two = CommonAmBearer::new(
        &source,
        1,
        peer_key,
        0x2346,
        logical,
        crate::common_am::CommonAmSettings {
            config,
            limits,
            started_at: at(0),
        },
    )
    .unwrap();
    let payload = Bits::new(&[0x31, 0x62, 0x93], 24).unwrap();
    assert_eq!(one.enqueue(stale, payload), Err(CommonAmError::Origin));
    one.enqueue(key, payload).unwrap();
    two.enqueue(peer_key, payload).unwrap();
    let input = vec![0; rx.get()];
    let wrong = CommonPdu {
        header: CommonHeader::Dedicated {
            identity: CommonIdentity::Cell(0x2346),
            channel: logical,
        },
        payload: Bits::new(&input, input.len() * 8).unwrap(),
    };
    assert_eq!(one.receive(key, at(0), wrong), Err(CommonAmError::Mapping));
    assert_eq!(one.receive(stale, at(0), wrong), Err(CommonAmError::Origin));
    let width = usize::from(source.information().common_layout().transport_block_bits());
    source
        .queue_transport(at(0), 1, &vec![Bit::Zero; width], &[])
        .unwrap();
    assert_eq!(
        one.publish_frame(&mut source, at(0), at(0)),
        Err(CommonAmError::Transmit(CommonTxError::Conflict))
    );
    assert_eq!(one.counters().transmitted, 0);
    assert_eq!(two.counters().transmitted, 0);
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    for _ in 0..15 {
        source.generate_slot(&mut samples).unwrap();
    }
    assert_eq!(
        one.publish_frame(&mut source, at(CHIPS_PER_FRAME), at(CHIPS_PER_FRAME)),
        Ok(Some(PublicationKind::Data {
            sequence: 0,
            count: 0,
            retransmission: false
        }))
    );
    assert_eq!(two.counters().transmitted, 0);
    assert_eq!(one.counters().publication_failures, 1);
    assert_eq!(
        two.publish_frame(&mut source, at(CHIPS_PER_FRAME), at(CHIPS_PER_FRAME)),
        Err(CommonAmError::Transmit(CommonTxError::Conflict))
    );
    for _ in 0..15 {
        source.generate_slot(&mut samples).unwrap();
    }
    assert_eq!(
        two.publish_frame(
            &mut source,
            at(2 * CHIPS_PER_FRAME),
            at(2 * CHIPS_PER_FRAME)
        ),
        Ok(Some(PublicationKind::Data {
            sequence: 0,
            count: 0,
            retransmission: false
        }))
    );
}

#[test]
fn ciphered_common_am_recovers_fach_waveform_and_rach_payload_without_parsing_encrypted_lis() {
    use crate::common_am::CommonAmBearer;
    use std::num::NonZeroU64;
    use umts_mac::{
        CommonHeader, CommonIdentity, FddCommonChannel, LogicalChannelId,
        decode_common as decode_mac, encode_common,
    };
    use umts_rlc::acknowledged::{
        AmConfig, AmEntity, AmLimits, AmReceive, PublicationKind, ReceivePreparation,
    };
    use umts_rrc::establishment::SignallingProfile;
    use umts_security::{
        Key,
        ciphering::{Binding, CipherTimeline, Configuration, CountAnchor, Position, Scope},
        contexts::{CipherMaterial, KeyOrigin, KeySet},
        counters::CipherMode,
    };
    use umts_types::{CnDomain, Direction, UeKey, bits::Bits};

    for compact in [false, true] {
        let mut source = source(compact);
        let key = UeKey {
            epoch: at(0).epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        };
        let origin = KeyOrigin {
            ue: key,
            domain: CnDomain::Cs,
            generation: NonZeroU64::MIN,
        };
        let (cipher, _) = KeySet::uia1(
            origin,
            CipherMaterial::Uea1(Key::new([0x42; 16])),
            Key::new([0x24; 16]),
        )
        .split();
        let make_cipher = |direction| {
            CipherTimeline::new(
                Binding::new(
                    key,
                    Scope::Signalling,
                    2,
                    CipherMode::Acknowledged,
                    direction,
                )
                .unwrap(),
                CountAnchor::new(
                    CipherMode::Acknowledged,
                    Position::Sequence(
                        match direction {
                            Direction::Downlink => 3,
                            Direction::Uplink => 5,
                        } << 12,
                    ),
                    0,
                )
                .unwrap(),
                Configuration::Configured(cipher.clone()),
            )
            .unwrap()
        };
        let downlink = make_cipher(Direction::Downlink);
        let uplink = make_cipher(Direction::Uplink);
        let logical = LogicalChannelId::new(2).unwrap();
        let (tx, rx) = CommonAmBearer::sizes(&source, 1).unwrap();
        let config = SignallingProfile.network_am_configuration(tx, rx, 3, 5);
        let limits = AmLimits {
            maximum_sdu_bytes: count(256),
            queued_sdus: count(4),
            maximum_buffer_bytes: 1024 * 1024,
        };
        let mut network = CommonAmBearer::new(
            &source,
            1,
            key,
            0x2345,
            logical,
            crate::common_am::CommonAmSettings {
                config,
                limits,
                started_at: at(0),
            },
        )
        .unwrap();
        let mut peer = AmEntity::new(
            AmConfig {
                transmit_pdu_bytes: rx,
                receive_pdu_bytes: tx,
                initial_tx_hfn: 5,
                initial_rx_hfn: 3,
                ..config
            },
            limits,
            at(0),
        )
        .unwrap();
        let downlink_payload = b"encrypted downlink";
        network
            .enqueue(
                key,
                Bits::new(downlink_payload, downlink_payload.len() * 8).unwrap(),
            )
            .unwrap();
        assert!(
            network
                .publish_frame_prepared(&mut source, at(0), at(0), |_, _| Err::<(), _>(7))
                .is_err()
        );
        assert_eq!(network.counters().transmitted, 0);
        let mut published = None;
        let kind = network
            .publish_frame_mapped(
                &mut source,
                at(0),
                at(0),
                |position| match downlink.selected(key, Position::Sequence(position))? {
                    umts_security::ciphering::CipherUse::Data { count, .. } => Ok(count),
                    _ => unreachable!("data COUNT selection returned control"),
                },
                |publication, bytes| {
                    let PublicationKind::Data { count, .. } = publication.kind else {
                        panic!("not data")
                    };
                    let bits = bytes.len() * 8;
                    let position = publication.position.unwrap();
                    assert!(position > u64::from(count));
                    downlink.apply(key, Position::Sequence(position), bytes, bits)?;
                    assert_eq!(&bytes[..2], &publication.bytes[..2]);
                    assert_ne!(&bytes[2..], &publication.bytes[2..]);
                    published = Some(bytes.to_vec());
                    Ok::<_, umts_security::ciphering::Error>(())
                },
            )
            .unwrap()
            .unwrap();
        assert!(matches!(
            kind,
            PublicationKind::Data {
                sequence: 0,
                count: 0,
                retransmission: false
            }
        ));
        let mut samples = vec![Complex32::default(); source.samples_per_slot()];
        source.generate_slot(&mut samples).unwrap();
        let width = usize::from(source.information().common_layout().transport_block_bits());
        let recovered = recover_common(
            &prepared_frame(&source),
            &source.information().sib5().secondary_channels()[1],
            source.information(),
            width,
        );
        let mut bytes = vec![0; width / 8];
        umts_dsp::bits::pack_msb(&recovered, &mut bytes).unwrap();
        let mac = decode_mac(FddCommonChannel::Fach, Bits::new(&bytes, width).unwrap()).unwrap();
        assert_eq!(
            mac.header,
            CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(0x2345),
                channel: logical
            }
        );
        let mut ciphered = vec![0; tx.get()];
        mac.payload.copy_to(&mut ciphered).unwrap();
        assert_eq!(Some(&ciphered), published.as_ref());
        let mut plain = ciphered.clone();
        assert_eq!(
            peer.receive_prepared(at(0), &ciphered, |request| {
                let ReceivePreparation::Data { position, .. } = request else {
                    panic!("not data")
                };
                let bits = plain.len() * 8;
                downlink.apply(key, Position::Sequence(position), &mut plain, bits)?;
                Ok::<_, umts_security::ciphering::Error>(plain.as_slice())
            })
            .unwrap(),
            AmReceive::Stored
        );
        peer.deliver_with(|delivery| {
            assert_eq!(
                delivery.sdus().collect::<Vec<_>>(),
                [downlink_payload.as_slice()]
            );
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
        let uplink_payload = b"encrypted uplink";
        peer.enqueue(uplink_payload).unwrap();
        let mut received_data = false;
        for _ in 0..4 {
            let Some((kind, ())) = peer
                .publish_next_mapped(
                    at(0),
                    |position| {
                        let umts_security::ciphering::CipherUse::Data { count, .. } =
                            uplink.selected(key, Position::Sequence(position)).unwrap()
                        else {
                            panic!("not data")
                        };
                        Ok::<_, ()>(count)
                    },
                    |publication| {
                        let mut bytes = publication.bytes.to_vec();
                        if let PublicationKind::Data { .. } = publication.kind {
                            let bits = bytes.len() * 8;
                            uplink
                                .apply(
                                    key,
                                    Position::Sequence(publication.position.unwrap()),
                                    &mut bytes,
                                    bits,
                                )
                                .unwrap();
                        }
                        let mut rach = vec![0; rx.get() + 3];
                        let bits = encode_common(
                            FddCommonChannel::Rach,
                            CommonHeader::Dedicated {
                                identity: CommonIdentity::Cell(0x2345),
                                channel: logical,
                            },
                            Bits::new(&bytes, bytes.len() * 8).unwrap(),
                            &mut rach,
                        )
                        .unwrap();
                        let pdu =
                            decode_mac(FddCommonChannel::Rach, Bits::new(&rach, bits).unwrap())
                                .unwrap();
                        network
                            .receive_prepared(key, at(0), pdu, |request, bytes| {
                                if let ReceivePreparation::Data { position, .. } = request {
                                    let bits = bytes.len() * 8;
                                    uplink.apply(key, Position::Sequence(position), bytes, bits)?;
                                }
                                Ok::<_, umts_security::ciphering::Error>(())
                            })
                            .unwrap();
                        Ok::<_, ()>(())
                    },
                )
                .unwrap()
            else {
                break;
            };
            if matches!(kind, PublicationKind::Data { .. }) {
                received_data = true;
                break;
            }
        }
        assert!(received_data);
        network
            .deliver_with(key, |delivery| {
                assert_eq!(
                    delivery.sdus().collect::<Vec<_>>(),
                    [uplink_payload.as_slice()]
                );
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(network.counters().transmitted, 1);
        assert_eq!(network.counters().publication_failures, 1);
        assert_eq!(peer.outstanding_pdus(), 1);
    }
}

#[test]
fn common_transport_queues_recover_crc_valid_payloads_for_both_signalled_layouts() {
    for compact in [false, true] {
        let mut tx = source(compact);
        let width = usize::from(tx.information().common_layout().transport_block_bits());
        let fach: Vec<_> = (0..width).map(|i| Bit::from(i % 7 < 3)).collect();
        let pch: Vec<_> = (0..width).map(|i| Bit::from(i % 11 < 5)).collect();
        tx.queue_transport(at(0), 1, &fach, &[]).unwrap();
        let mut indicators = [false; 18];
        indicators[3] = true;
        tx.queue_transport(at(2 * CHIPS_PER_FRAME), 0, &pch, &indicators)
            .unwrap();
        let mut samples = vec![Complex32::default(); tx.samples_per_slot()];
        tx.generate_slot(&mut samples).unwrap();
        decode_common(
            &prepared_frame(&tx),
            &tx.information().sib5().secondary_channels()[1],
            tx.information(),
            &fach,
        );
        assert!(tx.secondary[0].data.iter().all(|bit| *bit == TxBit::Dtx));
        for _ in 1..30 {
            tx.generate_slot(&mut samples).unwrap();
        }
        tx.generate_slot(&mut samples).unwrap();
        decode_common(
            &prepared_frame(&tx),
            &tx.information().sib5().secondary_channels()[0],
            tx.information(),
            &pch,
        );
        assert!(tx.secondary[1].data.iter().all(|bit| *bit == TxBit::Dtx));
        assert!(tx.pending.iter().all(|entry| !entry.occupied));
    }
}

#[test]
fn shifted_pich_and_frame_crossing_aich_are_added_before_one_continuous_filter() {
    let mut tx = source(false);
    let width = usize::from(tx.information().common_layout().transport_block_bits());
    let payload: Vec<_> = (0..width).map(|i| Bit::from(i % 5 < 2)).collect();
    let mut indicators = [false; 18];
    indicators[4] = true;
    tx.queue_transport(at(2 * CHIPS_PER_FRAME), 0, &payload, &indicators)
        .unwrap();
    let observed = AccessObservation {
        code: PrachCode::new(7504).unwrap(),
        signature: umts_dsp::prach::PreambleSignature::new(13).unwrap(),
        nominal_start: at(23040),
        received_start: at(23094),
    };
    let result = tx
        .schedule_acquisition(0, observed, Acquisition::Positive, at(27190), 0)
        .unwrap();
    assert_eq!(result.indicator_start, at(35840));
    let mut actual = Vec::new();
    let mut expected_chips = Vec::new();
    let mut slot_output = vec![Complex32::default(); tx.samples_per_slot()];
    for slot in 0..30 {
        let window = tx.generate_slot(&mut slot_output).unwrap();
        assert_eq!(window.first_sample.index, slot * CHIPS_PER_SLOT * 2);
        assert_eq!(
            window.chip_origin_sample.index,
            window.first_sample.index + 32
        );
        actual.extend_from_slice(&slot_output);
        if slot % 15 == 0 {
            expected_chips.extend_from_slice(&prepared_frame(&tx));
        }
    }
    let information = tx.information();
    let cpich = amplitudes().primary.cpich;
    let aich = AcquisitionDownlink::new(
        information.primary_code(),
        2,
        cpich * 10.0_f32.powf(f32::from(information.sib5().settings().aich_power_offset_db) / 20.0),
    )
    .unwrap();
    let mut ai = umts_dsp::indicators::AcquisitionIndicators::new(1 << 13).unwrap();
    ai.set(13, Acquisition::Positive).unwrap();
    aich.add_access_slot(at(35840), &ai, &mut expected_chips[35840..40960])
        .unwrap();
    let pich = PagingDownlink::new(
        information.primary_code(),
        7,
        PagingCount::PerFrame18,
        cpich * 10.0_f32.powf(f32::from(information.sib5().settings().pich_power_offset_db) / 20.0),
    )
    .unwrap();
    pich.add_frame(at(30720), &indicators, &mut expected_chips[30720..69120])
        .unwrap();
    let mut next_pich = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    pich.add_frame(at(69120), &[false; 18], &mut next_pich)
        .unwrap();
    for (sample, &indicator) in expected_chips[69120..].iter_mut().zip(&next_pich) {
        *sample += indicator;
    }
    let mut expected = vec![Complex32::default(); actual.len()];
    PulseShaper::new(SamplesPerChip::Two, 32)
        .unwrap()
        .process(&expected_chips, &mut expected)
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn common_slots_preserve_secondary_frame_selection_and_addition_order_across_sfn_wrap() {
    let origin = at((SYSTEM_FRAME_MODULUS - 2) * CHIPS_PER_FRAME);
    for compact in [false, true] {
        let mut tx = CommonTx::new(
            plan(compact),
            origin,
            SamplesPerChip::Two,
            32,
            CommonAmplitudes {
                primary: CommonChannelAmplitudes {
                    cpich: 0.0,
                    pccpch: 0.125,
                    primary_sch: 0.25,
                    secondary_sch: 0.125,
                },
                fach: 0.25,
                pch: 0.125,
            },
            count(8),
            100,
        )
        .unwrap();
        let width = usize::from(tx.information().common_layout().transport_block_bits());
        let payload: Vec<_> = (0..width).map(|bit| Bit::from(bit % 7 < 3)).collect();
        tx.queue_transport(origin, 1, &payload, &[]).unwrap();
        tx.queue_transport(
            at(origin.index + 2 * CHIPS_PER_FRAME),
            0,
            &payload,
            &[false; 18],
        )
        .unwrap();
        tx.queue_transport(at(origin.index + 3 * CHIPS_PER_FRAME), 1, &payload, &[])
            .unwrap();
        let mut output = vec![Complex32::default(); tx.samples_per_slot()];
        let mut expected = Vec::new();
        for slot in 0..60 {
            tx.generate_slot(&mut output).unwrap();
            let offset = slot % 15;
            if offset == 0 {
                expected = prepared_frame(&tx);
                let paging = slot / 15 == 2;
                assert_eq!(
                    tx.secondary[0].selected_tfci,
                    if paging {
                        tx.secondary[0].full_tfci
                    } else {
                        tx.secondary[0].empty_tfci
                    }
                );
                assert_eq!(
                    tx.secondary[1].selected_tfci,
                    if matches!(slot / 15, 0 | 3) {
                        tx.secondary[1].full_tfci
                    } else {
                        tx.secondary[1].empty_tfci
                    }
                );
                assert_eq!(
                    tx.pending.iter().filter(|entry| entry.occupied).count(),
                    [2, 2, 1, 0][slot / 15]
                );
            }
            let start = offset * CHIPS_PER_SLOT as usize;
            assert_eq!(&tx.slot, &expected[start..start + CHIPS_PER_SLOT as usize]);
        }
        assert_eq!(tx.next_chip(), at(origin.index + 4 * CHIPS_PER_FRAME));
    }
}

#[test]
fn slot_broadcast_is_identical_to_the_existing_continuous_bch_path_across_sfn_wrap() {
    let origin = at((SYSTEM_FRAME_MODULUS - 2) * CHIPS_PER_FRAME);
    let primary = plan(false).information().primary_code();
    let gains = CommonAmplitudes {
        primary: CommonChannelAmplitudes {
            cpich: 0.0,
            pccpch: 0.2,
            primary_sch: 0.0,
            secondary_sch: 0.0,
        },
        fach: 0.0,
        pch: 0.0,
    };
    let mut expected_source = crate::broadcast::BroadcastSource::new(
        plan(false).into_parts().1,
        BroadcastWaveform::new(primary, gains.primary, SamplesPerChip::Two, 32, origin).unwrap(),
    )
    .unwrap();
    let mut actual_source = CommonTx::new(
        plan(false),
        origin,
        SamplesPerChip::Two,
        32,
        gains,
        count(8),
        100,
    )
    .unwrap();
    for _ in 0..2 {
        let mut expected = vec![Complex32::default(); expected_source.samples_per_tti()];
        expected_source.generate(&mut expected).unwrap();
        let mut actual = Vec::new();
        let mut output = vec![Complex32::default(); actual_source.samples_per_slot()];
        for _ in 0..30 {
            actual_source.generate_slot(&mut output).unwrap();
            actual.extend_from_slice(&output);
        }
        assert_eq!(actual, expected);
    }
}

#[test]
fn queue_deadlines_duplicates_capacity_and_invalid_slots_are_atomic() {
    let mut tx = CommonTx::new(
        plan(true),
        at(0),
        SamplesPerChip::Two,
        32,
        amplitudes(),
        count(1),
        100,
    )
    .unwrap();
    let width = usize::from(tx.information().common_layout().transport_block_bits());
    let payload = vec![Bit::Zero; width];
    assert_eq!(
        tx.queue_transport(at(1), 1, &payload, &[]),
        Err(CommonTxError::Time)
    );
    assert_eq!(
        tx.queue_transport(at(0), 99, &payload, &[]),
        Err(CommonTxError::Channel)
    );
    assert_eq!(
        tx.queue_transport(at(0), 1, &[], &[]),
        Err(CommonTxError::Payload)
    );
    tx.queue_transport(at(0), 1, &payload, &[]).unwrap();
    assert_eq!(
        tx.queue_transport(at(0), 1, &payload, &[]),
        Err(CommonTxError::Conflict)
    );
    assert_eq!(
        tx.queue_transport(at(CHIPS_PER_FRAME), 1, &payload, &[]),
        Err(CommonTxError::Capacity)
    );
    let before = tx.next_chip();
    assert_eq!(
        tx.generate_slot(&mut []),
        Err(CommonTxError::Dsp(DspError::InvalidLength))
    );
    assert_eq!(tx.next_chip(), before);
    let mut output = vec![Complex32::default(); tx.samples_per_slot()];
    tx.generate_slot(&mut output).unwrap();
    assert_eq!(
        tx.queue_transport(at(0), 1, &payload, &[]),
        Err(CommonTxError::Late)
    );
    for _ in 1..13 {
        tx.generate_slot(&mut output).unwrap();
    }
    assert_eq!(
        tx.queue_transport(at(2 * CHIPS_PER_FRAME), 0, &payload, &[false; 18]),
        Err(CommonTxError::Late)
    );
}
