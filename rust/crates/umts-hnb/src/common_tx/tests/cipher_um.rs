// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_am::CommonAmBearer,
    common_um::{CommonUmBearer, CommonUmPreparedError, CommonUmPreparedReceiveError},
};
use std::num::NonZeroU64;
use umts_mac::{
    CommonHeader, CommonIdentity, FddCommonChannel, LogicalChannelId, decode_common as decode_mac,
    encode_common,
};
use umts_rlc::unacknowledged::{
    EInterpretation, LiWidth, UmConfig, UmLimits, UmPublishError, UmReceiver, UmTransmitter,
};
use umts_security::{
    Key,
    ciphering::{
        BearerCipherConfiguration, BearerCiphers, CipherActivation, Configuration, CountAnchor,
        Position, Scope,
    },
    contexts::{CipherMaterial, KeyOrigin, KeySet},
    counters::{CipherMode, Start},
};
use umts_types::{
    CnDomain, Direction, UeKey,
    bits::{BITS_PER_BYTE, Bits},
};

#[test]
fn prepared_um_ciphering_crosses_fach_waveform_and_rach_mac_without_losing_admission_state() {
    for compact in [false, true] {
        let mut source = source(compact);
        let key = UeKey {
            epoch: at(0).epoch,
            slot: 3,
            generation: NonZeroU64::MIN,
        };
        let identity = 0x2345;
        let (tx_bytes, rx_bytes) = CommonAmBearer::sizes(&source, 1).unwrap();
        let config = UmConfig {
            li_width: LiWidth::Seven,
            interpretation: EInterpretation::Normal,
        };
        let tx_limits = UmLimits {
            maximum_pdu_bytes: tx_bytes,
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(8),
            maximum_buffer_bytes: 64 * 1024,
        };
        let rx_limits = UmLimits {
            maximum_pdu_bytes: rx_bytes,
            ..tx_limits
        };
        let mut network =
            CommonUmBearer::new(&source, 1, key, identity, config, tx_limits, rx_limits).unwrap();
        let mut ciphers = BearerCiphers::new(key, count(1)).unwrap();
        let anchor = CountAnchor::from_start(
            CipherMode::Unacknowledged,
            Position::Sequence(0),
            Start::new(0).unwrap(),
        )
        .unwrap();
        ciphers
            .register_with(
                BearerCipherConfiguration {
                    bearer: 1,
                    scope: Scope::Signalling,
                    transmit: anchor,
                    receive: anchor,
                    initial: Configuration::Unprotected,
                },
                || Ok::<_, ()>(()),
            )
            .unwrap();
        let context = KeySet::uia1(
            KeyOrigin {
                ue: key,
                domain: CnDomain::Cs,
                generation: NonZeroU64::MIN,
            },
            CipherMaterial::Uea1(Key::new([0x42; 16])),
            Key::new([0x11; 16]),
        )
        .split()
        .0;
        ciphers
            .update_with(
                context,
                &[
                    CipherActivation::Sequenced {
                        bearer: 1,
                        direction: Direction::Downlink,
                        anchor,
                        first_new: Position::Sequence(0),
                    },
                    CipherActivation::Sequenced {
                        bearer: 1,
                        direction: Direction::Uplink,
                        anchor,
                        first_new: Position::Sequence(0),
                    },
                ],
                || Ok::<_, ()>(()),
            )
            .unwrap();
        let downlink = (0..2 * tx_bytes.get())
            .map(|index| (index * 19) as u8)
            .collect::<Vec<_>>();
        network
            .enqueue(
                key,
                Bits::new(&downlink, downlink.len() * BITS_PER_BYTE).unwrap(),
            )
            .unwrap();
        assert_eq!(
            network.publish_frame_prepared(&mut source, at(0), |_, _| Err::<(), _>(7)),
            Err(CommonUmPreparedError::Preparation(7))
        );
        assert_eq!(network.transmit_position(), 0);
        let mut receiver = UmReceiver::new(config, tx_limits).unwrap();
        let mut recovered = Vec::new();
        let mut samples = vec![Complex32::default(); source.samples_per_slot()];
        for _ in 0..32 {
            let now = source.next_chip();
            let mut plaintext = Vec::new();
            let published = network
                .publish_frame_prepared(&mut source, now, |publication, bytes| {
                    plaintext.extend_from_slice(publication.bytes);
                    ciphers
                        .apply(
                            key,
                            1,
                            Direction::Downlink,
                            Position::Sequence(publication.position),
                            bytes,
                            bytes.len() * BITS_PER_BYTE,
                        )
                        .map(|_| ())
                })
                .unwrap();
            source.generate_slot(&mut samples).unwrap();
            if published {
                let bytes = recover_current_fach(&source);
                let mac = decode_mac(
                    FddCommonChannel::Fach,
                    Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
                )
                .unwrap();
                let mut encrypted = vec![0; mac.payload.len() / BITS_PER_BYTE];
                mac.payload.copy_to(&mut encrypted).unwrap();
                assert_eq!(encrypted[0], plaintext[0]);
                assert_ne!(encrypted[1..], plaintext[1..]);
                let mut plain = encrypted.clone();
                let received = receiver
                    .receive_prepared_admitted(
                        &encrypted,
                        |position| {
                            ciphers
                                .apply(
                                    key,
                                    1,
                                    Direction::Downlink,
                                    Position::Sequence(position),
                                    &mut plain,
                                    encrypted.len() * BITS_PER_BYTE,
                                )
                                .unwrap();
                            Ok::<_, ()>(plain.as_slice())
                        },
                        |_, _| Ok::<_, ()>(()),
                    )
                    .unwrap();
                recovered.extend(received.sdus().map(<[u8]>::to_vec));
            }
            for _ in 1..15 {
                source.generate_slot(&mut samples).unwrap();
            }
            if !recovered.is_empty() {
                break;
            }
        }
        assert_eq!(recovered, [downlink]);
        let uplink = (0..2 * rx_bytes.get())
            .map(|index| (index * 23) as u8)
            .collect::<Vec<_>>();
        let mut sender = UmTransmitter::new(config, rx_limits).unwrap();
        sender.enqueue(&uplink).unwrap();
        let mut recovered = Vec::new();
        let mut reject_once = true;
        let mut rejected = false;
        for _ in 0..32 {
            if !sender.has_pending() {
                break;
            }
            let before = sender.next_position();
            let received_before = network.receive_position().unwrap();
            let result = sender.transmit_positioned_with(rx_bytes.get(), |publication| {
                let mut encrypted = publication.bytes.to_vec();
                ciphers
                    .apply(
                        key,
                        1,
                        Direction::Uplink,
                        Position::Sequence(publication.position),
                        &mut encrypted,
                        publication.bytes.len() * BITS_PER_BYTE,
                    )
                    .unwrap();
                let mut bytes = vec![0; encrypted.len() + 3];
                encode_common(
                    FddCommonChannel::Rach,
                    CommonHeader::Dedicated {
                        identity: CommonIdentity::Cell(identity),
                        channel: LogicalChannelId::new(1).unwrap(),
                    },
                    Bits::new(&encrypted, encrypted.len() * BITS_PER_BYTE).unwrap(),
                    &mut bytes,
                )
                .unwrap();
                let mac = decode_mac(
                    FddCommonChannel::Rach,
                    Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
                )
                .unwrap();
                match network.receive_prepared_admitted(
                    key,
                    mac,
                    |position, bytes| {
                        ciphers
                            .apply(
                                key,
                                1,
                                Direction::Uplink,
                                Position::Sequence(position),
                                bytes,
                                bytes.len() * BITS_PER_BYTE,
                            )
                            .map(|_| ())
                    },
                    |count, _| {
                        if count != 0 && reject_once {
                            reject_once = false;
                            Err(7)
                        } else {
                            Ok(())
                        }
                    },
                ) {
                    Ok(received) => {
                        recovered.extend(received.sdus().map(<[u8]>::to_vec));
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            });
            match result {
                Ok(Some(())) => (),
                Err(UmPublishError::Downstream(CommonUmPreparedReceiveError::Admission(7))) => {
                    rejected = true;
                    assert_eq!(sender.next_position(), before);
                    assert_eq!(network.receive_position().unwrap(), received_before);
                }
                other => panic!("unexpected prepared UM result: {other:?}"),
            }
        }
        assert!(rejected);
        assert!(!sender.has_pending());
        assert_eq!(recovered, [uplink]);
        assert_eq!(network.receive_position().unwrap(), sender.next_position());
    }
}
