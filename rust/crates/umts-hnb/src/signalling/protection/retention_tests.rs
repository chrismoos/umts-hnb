// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_tx::tests::recover_current_fach,
    signalling::tests::{controller, establish, peer},
    test_support::{at, source},
};
use std::num::NonZeroU64;
use umts_mac::{
    CommonHeader, CommonIdentity, CommonPdu, FddCommonChannel, LogicalChannelId, decode_common,
};
use umts_security::{
    Key,
    ciphering::{CipherActivation, CipherUse},
    contexts::{CipherContext, CipherMaterial, KeySet},
    counters::CipherMode,
};
use umts_types::{CnDomain, Direction, bits::BITS_PER_BYTE, time::CHIPS_PER_SLOT};

fn context(key: UeKey, generation: u64) -> CipherContext {
    KeySet::uia1(
        KeyOrigin {
            ue: key,
            domain: CnDomain::Cs,
            generation: NonZeroU64::new(generation).unwrap(),
        },
        CipherMaterial::Uea1(Key::new([generation as u8; 16])),
        Key::new([generation as u8; 16]),
    )
    .split()
    .0
}
fn install(context: &mut Context, key: CipherContext, at: u64) {
    let activations =
        [Direction::Downlink, Direction::Uplink].map(|direction| CipherActivation::Sequenced {
            bearer: 2,
            direction,
            anchor: CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(at), at as u32)
                .unwrap(),
            first_new: Position::Sequence(at),
        });
    context
        .protection
        .ciphers
        .update_with(key, &activations, || Ok::<_, ()>(()))
        .unwrap();
}

#[test]
fn host_collection_preserves_old_cipher_until_real_rlc_ack_advances_the_transmit_window() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    let old = context(key, 1);
    let new = context(key, 2);
    install(controller.registry.get_mut(key).unwrap(), old.clone(), 0);
    assert_eq!(
        controller.publish_bearer(key, SignallingBearer::ControlAm, &mut source, at(0), at(0)),
        Ok(true)
    );
    let mut samples = vec![num_complex::Complex32::default(); source.samples_per_slot()];
    for _ in 0..15 {
        source.generate_slot(&mut samples).unwrap();
    }
    let receipt = controller
        .registry
        .get_mut(key)
        .unwrap()
        .control
        .enqueue_tracked(key, Bits::new(&[0x42; 7], 7 * BITS_PER_BYTE).unwrap())
        .unwrap();
    let start = source.next_chip();
    assert_eq!(
        controller.publish_bearer(key, SignallingBearer::ControlAm, &mut source, start, start),
        Ok(true)
    );
    source.generate_slot(&mut samples).unwrap();
    let encoded = recover_current_fach(&source);
    let mac = decode_common(
        FddCommonChannel::Fach,
        Bits::new(&encoded, encoded.len() * BITS_PER_BYTE).unwrap(),
    )
    .unwrap();
    let mut bytes = vec![0; mac.payload.len() / BITS_PER_BYTE];
    mac.payload.copy_to(&mut bytes).unwrap();
    let body = &mut bytes[2..];
    old.apply(
        old.origin(),
        0,
        1,
        Direction::Downlink,
        body,
        body.len() * BITS_PER_BYTE,
    )
    .unwrap();
    let mut peer = peer(&source);
    peer.receive(source.next_chip(), &bytes).unwrap();
    install(controller.registry.get_mut(key).unwrap(), new.clone(), 1);
    let owned = controller.registry.get_mut(key).unwrap();
    assert_eq!(owned.control.window_positions().oldest_transmit, 0);
    assert_eq!(Protection::collect_keys(owned, key), Ok(0));
    assert_eq!(
        owned
            .protection
            .ciphers
            .selected(key, 2, Direction::Downlink, Position::Sequence(0)),
        Ok(CipherUse::Data {
            origin: Some(old.origin()),
            count: 0
        })
    );
    let now = source.next_chip();
    assert_eq!(now.index, start.index + CHIPS_PER_SLOT);
    peer.publish_next(now, |publication| {
        controller.receive(
            key,
            now,
            CommonPdu {
                header: CommonHeader::Dedicated {
                    identity: CommonIdentity::Cell(controller.assigned(key).unwrap().crnti()),
                    channel: LogicalChannelId::new(2).unwrap(),
                },
                payload: Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE)
                    .unwrap(),
            },
        )
    })
    .unwrap()
    .unwrap();
    let owned = controller.registry.get_mut(key).unwrap();
    assert_eq!(
        owned.control.transmission(key, receipt),
        Ok(SduTransmission::Acknowledged)
    );
    assert_eq!(owned.control.window_positions().oldest_transmit, 1);
    assert_eq!(Protection::collect_keys(owned, key), Ok(0));
    assert!(matches!(
        owned
            .protection
            .ciphers
            .selected(key, 2, Direction::Downlink, Position::Sequence(0)),
        Err(umts_security::ciphering::BearerCipherError::Cipher(
            umts_security::ciphering::Error::Position
        ))
    ));
    assert_eq!(
        owned
            .protection
            .ciphers
            .selected(key, 2, Direction::Downlink, Position::Sequence(1)),
        Ok(CipherUse::Data {
            origin: Some(new.origin()),
            count: 1
        })
    );
    let cached = owned.protection.collected_cipher_floors;
    assert_eq!(Protection::collect_keys(owned, key), Ok(0));
    assert_eq!(owned.protection.collected_cipher_floors, cached);
}

#[test]
fn delivery_or_recovery_holds_prevent_collection_from_advancing_any_retention_floor() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    let owned = controller.registry.get_mut(key).unwrap();
    let keyset = context(key, 1);
    install(owned, keyset.clone(), 0);
    let receipt = owned
        .control
        .enqueue_tracked(key, Bits::new(&[0x42], BITS_PER_BYTE).unwrap())
        .unwrap();
    owned.protection.delivery = Some(Delivery::Command {
        origin: keyset.origin(),
        receipt,
    });
    assert_eq!(
        owned.protection.bearer_reconfiguration_block(),
        Some(BearerProtectionBlock::SecurityCommand)
    );
    assert!(owned.protection.blocks_security_reconfiguration());
    assert_eq!(Protection::collect_keys(owned, key), Ok(0));
    assert_eq!(owned.protection.collected_cipher_floors, None);
    owned.protection.delivery = None;
    owned.protection.admission = ControlAdmission::RecoveryRequired { rejected: None };
    assert_eq!(
        owned.protection.bearer_reconfiguration_block(),
        Some(BearerProtectionBlock::RadioRecovery)
    );
    assert_eq!(Protection::collect_keys(owned, key), Ok(0));
    assert_eq!(owned.protection.collected_cipher_floors, None);
    assert_eq!(
        owned
            .protection
            .ciphers
            .selected(key, 2, Direction::Uplink, Position::Sequence(0)),
        Ok(CipherUse::Data {
            origin: Some(keyset.origin()),
            count: 0
        })
    );
}
