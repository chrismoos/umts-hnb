// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod hsupa;
use std::num::{NonZeroU16, NonZeroU64};
use umts_dsp::{
    bits::Bit,
    coding::{ChannelCoding, CodedBlockFormat},
    convolutional::CodeRate,
    crc::TransportCrc,
    dedicated_transport::{CombinationDefinition, TransportDefinition},
    interleaving::Rel99Tti,
    tfci::Tfci,
};
use umts_rlc::{
    acknowledged::{AmConfig, AmLimits, Polling},
    unacknowledged::{EInterpretation, LiWidth, UmConfig, UmLimits},
};
use umts_security::{
    Key,
    algorithms::Uea1Key,
    ciphering::{
        BearerCipherConfiguration, BearerCiphers, CipherActivation, Configuration, CountAnchor,
        Position,
    },
    contexts::{CipherContext, CipherMaterial, KeyOrigin, KeySet},
};
use umts_types::{CnDomain, Direction, RadioEpoch};

fn n(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

#[test]
fn sdu_size_rejection_is_distinct_from_temporary_am_and_um_queue_pressure() {
    const MAXIMUM_BYTES: usize = 128;
    const QUEUED_SDUS: usize = 4;
    let mut bank = bank(8, Segmentation::Disabled);
    for bearer in [rb(20), rb(1)] {
        for bytes in [Vec::new(), vec![0; MAXIMUM_BYTES + 1]] {
            assert_eq!(bank.enqueue_sdu(bearer, &bytes), Err(BearerError::SduSize));
        }
        for _ in 0..QUEUED_SDUS {
            bank.enqueue_sdu(bearer, &[0x42; MAXIMUM_BYTES]).unwrap();
        }
        assert!(matches!(
            bank.enqueue_sdu(bearer, b"full"),
            Err(BearerError::Am(AmError::Capacity) | BearerError::Um(UmError::Capacity))
        ));
        assert!(matches!(
            bank.enqueue_sdu(bearer, &[0; MAXIMUM_BYTES + 1]),
            Err(BearerError::SduSize)
        ));
    }
    assert!(matches!(
        bank.enqueue_sdu(rb(10), &[]),
        Err(BearerError::Mapping)
    ));
}

#[test]
fn downlink_sdu_size_uses_transmitter_not_asymmetric_receiver_capacity() {
    const TRANSMIT_BYTES: usize = 256;
    let mut transmit_limits = um_limits();
    transmit_limits.maximum_sdu_bytes = n(TRANSMIT_BYTES);
    let mut endpoint = BearerEndpoint::unacknowledged(
        key(),
        rb(20),
        Scope::User(CnDomain::Ps),
        UmTransmitter::new(um_configuration(), transmit_limits).unwrap(),
        UmReceiver::new(um_configuration(), um_limits()).unwrap(),
    )
    .unwrap();
    endpoint.enqueue(&[0x42; TRANSMIT_BYTES]).unwrap();
    assert!(matches!(
        endpoint.enqueue(&[0; TRANSMIT_BYTES + 1]),
        Err(BearerError::SduSize)
    ));
}

#[test]
fn selective_output_retirement_preserves_fifo_bytes_and_provenance_at_every_ring_wrap() {
    const SLOTS: usize = 8;
    for head in 0..SLOTS {
        for count in 0..=SLOTS {
            for mask in 0u16..1 << SLOTS {
                let mut output = Outputs {
                    slots: (0..SLOTS)
                        .map(|_| Output {
                            metadata: None,
                            bytes: vec![0; 2],
                        })
                        .collect(),
                    head,
                    queued: 0,
                };
                let mut keep = [false; MAX_BEARERS];
                let mut expected = Vec::new();
                for offset in 0..count {
                    let id = rb(FIRST_USER_BEARER + offset as u8);
                    keep[id.index()] = mask & (1 << offset) != 0;
                    let metadata = UpperSdu {
                        identity: identity(),
                        bearer: id,
                        scope: Scope::User(CnDomain::Ps),
                        received: at(offset as u64),
                        bits: 16,
                        quality: Quality::Correct,
                        coverage: None,
                        transparent_origin: None,
                    };
                    let bytes = [offset as u8, 0xa5];
                    output.push(metadata, Bits::new(&bytes, 16).unwrap());
                    if keep[id.index()] {
                        expected.push((metadata, bytes));
                    }
                }
                assert_eq!(output.discard_except(&keep), count - expected.len());
                assert_eq!(output.queued, expected.len());
                for (offset, (metadata, bytes)) in expected.iter().enumerate() {
                    let slot = &output.slots[(head + offset) % SLOTS];
                    assert_eq!(slot.metadata, Some(*metadata));
                    assert_eq!(slot.bytes.as_slice(), bytes.as_slice());
                }
                for offset in expected.len()..SLOTS {
                    assert!(output.slots[(head + offset) % SLOTS].metadata.is_none());
                }
            }
        }
    }
}

#[test]
fn removed_am_receive_state_is_pending_even_when_outer_output_contains_only_survivors() {
    let mut bank = bank(1, Segmentation::Disabled);
    let mut sender = am();
    sender.enqueue(b"undelivered").unwrap();
    let index = bank.lookup[rb(20).index()].unwrap();
    let Entity::Acknowledged(receiver) = &mut bank.endpoints[index].entity else {
        panic!("AM")
    };
    sender
        .publish_next(at(0), |publication| {
            receiver.receive(at(0), publication.bytes).unwrap();
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    let metadata = UpperSdu {
        identity: identity(),
        bearer: rb(10),
        scope: Scope::User(CnDomain::Cs),
        received: at(0),
        bits: 8,
        quality: Quality::Correct,
        coverage: None,
        transparent_origin: None,
    };
    bank.output.push(metadata, Bits::new(&[0x35], 8).unwrap());
    bank.delivery_cursor = index;
    assert_eq!(
        bank.service(at(0), &mut ciphers(), ResetPermission::Defer),
        Ok(BearerProgress::Idle)
    );
    assert!(bank.endpoints[index].pending_receive_data());
    assert_eq!(bank.output.queued, 1);
    assert_eq!(
        bank.output.slots[bank.output.head].metadata.unwrap().bearer,
        rb(10)
    );
}

#[test]
fn independently_owned_signalling_rlc_is_not_a_root_controller_route() {
    assert!(bank(4, Segmentation::Disabled).owns_signalling_rlc());
}

#[test]
fn compatible_am_transports_share_one_sequence_queue_and_receive_window() {
    let source = formats();
    let copy = |definition: &TransportDefinition| TransportDefinition {
        id: definition.id,
        interval: definition.interval,
        attribute: definition.attribute,
        formats: definition.formats.clone(),
    };
    let mut definitions: Vec<_> = source
        .transport_ids()
        .map(|id| copy(source.definition(id).unwrap()))
        .collect();
    let mut extra = copy(source.definition(tr(2)).unwrap());
    extra.id = tr(4);
    definitions.push(extra);
    let formats = Arc::new(
        ConfiguredTfcs::new(
            definitions,
            vec![CombinationDefinition {
                tfci: Tfci::new(0).unwrap(),
                formats: vec![0; 4],
            }],
            n(100_000),
        )
        .unwrap(),
    );
    let mut mappings = bindings();
    mappings.push(TransportBinding {
        transport: tr(4),
        channels: TransportChannels::Single(rb(20)),
    });
    let mut bank = DedicatedBearers::new(
        identity(),
        anchor(),
        256,
        formats.clone(),
        mappings.clone(),
        endpoints(Segmentation::Disabled),
        limits(8),
    )
    .unwrap();
    bank.configure_downlink(anchor(), 256, formats, mappings)
        .unwrap();
    bank.activate(identity()).unwrap();
    let mut ciphers = BearerCiphers::new(key(), n(1)).unwrap();
    let count = CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(0), 0).unwrap();
    ciphers
        .register_with(
            BearerCipherConfiguration {
                bearer: 20,
                scope: Scope::User(CnDomain::Ps),
                transmit: count,
                receive: count,
                initial: Configuration::Unprotected,
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    bank.enqueue_sdu(rb(20), &[0x62; 60]).unwrap();
    for (sequence, transport) in [(0, 2), (1, 4)] {
        let pdu = bank
            .publish_sequenced(
                DownlinkGrant {
                    identity: identity(),
                    bearer: rb(20),
                    transport: tr(transport),
                    format: 0,
                    start: at(sequence),
                },
                at(sequence),
                &ciphers,
                |pdu| Ok::<_, ()>(packed(pdu.data)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            u16::from_be_bytes([pdu[0] & 0x7f, pdu[1]]) >> 3,
            sequence as u16
        );
    }
    let mut sender = am();
    sender.enqueue(&[0x24; 60]).unwrap();
    for (frame, transport) in [(0, 2), (1, 4)] {
        let pdu = sender
            .publish_next(at(frame), |pdu| Ok::<_, ()>(pdu.bytes.to_vec()))
            .unwrap()
            .unwrap()
            .1;
        enqueue(
            &mut bank,
            transport,
            frame,
            bits(&pdu, pdu.len() * BITS_PER_BYTE),
        )
        .unwrap();
        run(&mut bank, &mut ciphers);
    }
    let Entity::Acknowledged(entity) = &bank.endpoints[bank.lookup[rb(20).index()].unwrap()].entity
    else {
        unreachable!()
    };
    assert_eq!(entity.send_sequence(), 2);
    assert_eq!(entity.outstanding_pdus(), 2);
    assert_eq!(entity.receive_sequence(), 2);
}
fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 0,
        generation: NonZeroU64::MIN,
    }
}
fn identity() -> ReceiveIdentity {
    ReceiveIdentity {
        ue: key(),
        configuration: NonZeroU64::MIN,
    }
}
fn at(frame: u64) -> ChipInstant {
    ChipInstant {
        epoch: key().epoch,
        index: 1024 + frame * CHIPS_PER_FRAME,
    }
}
fn anchor() -> FrameAnchor {
    FrameAnchor {
        time: at(0),
        cfn: 0,
    }
}
fn rb(id: u8) -> BearerId {
    BearerId::new(id).unwrap()
}
fn tr(id: u8) -> TransportId {
    TransportId::new(id).unwrap()
}
fn logical(id: u8) -> LogicalChannelId {
    LogicalChannelId::new(id).unwrap()
}
fn am_configuration() -> AmConfig {
    AmConfig {
        transmit_pdu_bytes: n(16),
        receive_pdu_bytes: n(16),
        transmit_window: NonZeroU16::new(32).unwrap(),
        receive_window: NonZeroU16::new(32).unwrap(),
        maximum_data_attempts: NonZeroU16::new(8).unwrap(),
        maximum_reset_attempts: NonZeroU16::new(4).unwrap(),
        reset_timeout_chips: NonZeroU64::new(100_000).unwrap(),
        polling: Polling {
            every_pdus: None,
            last_transmission: true,
            last_retransmission: true,
            timeout_chips: None,
            prohibit_chips: None,
        },
        status_prohibit_chips: None,
        missing_pdu_indicator: true,
        accept_special_he: false,
        initial_tx_hfn: 0,
        initial_rx_hfn: 0,
    }
}
fn am() -> AmEntity {
    AmEntity::new(
        am_configuration(),
        AmLimits {
            maximum_sdu_bytes: n(128),
            queued_sdus: n(4),
            maximum_buffer_bytes: 1_000_000,
        },
        at(0),
    )
    .unwrap()
}

#[test]
fn terminal_packet_reset_is_reported_without_receive_traffic() {
    let mut configuration = am_configuration();
    configuration.maximum_reset_attempts = NonZeroU16::MIN;
    let mut entity = AmEntity::new(
        configuration,
        AmLimits {
            maximum_sdu_bytes: n(128),
            queued_sdus: n(4),
            maximum_buffer_bytes: 1_000_000,
        },
        at(0),
    )
    .unwrap();
    entity.initiate_reset(at(0)).unwrap();
    assert!(matches!(
        entity.publish_next(at(0), |_| Ok::<_, ()>(())),
        Err(umts_rlc::acknowledged::PublishError::Rlc(
            AmError::ResetLimit
        ))
    ));
    let mut failed_bank = bank(8, Segmentation::Disabled);
    let index = failed_bank.lookup[rb(20).index()].unwrap();
    failed_bank.endpoints[index].entity = Entity::Acknowledged(Box::new(entity));
    assert!(failed_bank.pending.is_none());
    assert_eq!(
        failed_bank.service(at(0), &mut ciphers(), ResetPermission::Defer),
        Err(BearerError::Am(AmError::State))
    );
    let mut healthy = bank(8, Segmentation::Disabled);
    assert_eq!(
        healthy.service(at(0), &mut ciphers(), ResetPermission::Defer),
        Ok(BearerProgress::Idle)
    );
}

#[test]
fn acknowledged_reassembly_batches_are_bounded_and_skip_idle_bearers() {
    let mut configuration = am_configuration();
    configuration.transmit_window = NonZeroU16::new(128).unwrap();
    configuration.receive_window = NonZeroU16::new(128).unwrap();
    let make = || {
        AmEntity::new(
            configuration,
            AmLimits {
                maximum_sdu_bytes: n(128),
                queued_sdus: n(4),
                maximum_buffer_bytes: 1_000_000,
            },
            at(0),
        )
        .unwrap()
    };
    let mut peer = make();
    let mut bank = bank(128, Segmentation::Disabled);
    let index = bank.lookup[rb(20).index()].unwrap();
    bank.endpoints[index].entity = Entity::Acknowledged(Box::new(make()));
    let Entity::Acknowledged(receiver) = &mut bank.endpoints[index].entity else {
        unreachable!()
    };
    for value in 0..70u8 {
        peer.enqueue(&[value; 12]).unwrap();
        peer.publish_next(at(0), |publication| {
            receiver.receive(at(0), publication.bytes).unwrap();
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    }
    assert_eq!(
        bank.service(at(1), &mut ciphers(), ResetPermission::Defer),
        Ok(BearerProgress::AcknowledgedSdus)
    );
    assert_eq!(bank.counters().am_reassembled_pdus, 64);
    assert_eq!(bank.counters().am_reassembly_budget_exhausted, 1);
    let first = collect(&mut bank);
    assert_eq!(first.len(), 64);
    for (value, (_, bytes)) in first.iter().enumerate() {
        assert_eq!(bytes, &[value as u8; 12]);
    }
    assert_eq!(
        bank.service(at(2), &mut ciphers(), ResetPermission::Defer),
        Ok(BearerProgress::AcknowledgedSdus)
    );
    assert_eq!(bank.counters().am_reassembled_pdus, 70);
    assert_eq!(collect(&mut bank).len(), 6);
    assert_eq!(
        bank.counters().maximum_am_delivery_age_chips,
        at(2).index - at(0).index
    );
}
fn um_configuration() -> UmConfig {
    UmConfig {
        li_width: LiWidth::Seven,
        interpretation: EInterpretation::Normal,
    }
}
fn um_limits() -> UmLimits {
    UmLimits {
        maximum_pdu_bytes: n(12),
        maximum_sdu_bytes: n(128),
        queued_sdus: n(4),
        maximum_buffer_bytes: 4096,
    }
}
fn transparent(segmentation: Segmentation) -> TransparentSettings {
    TransparentSettings {
        uplink_segmentation: segmentation,
        downlink_segmentation: segmentation,
        errors: ErrorDelivery::Indicate,
        limits: TmLimits {
            maximum_pdus: n(4),
            maximum_pdu_bits: n(17),
            maximum_sdu_bits: n(if segmentation == Segmentation::Enabled {
                68
            } else {
                17
            }),
        },
    }
}
fn endpoints(segmentation: Segmentation) -> Vec<BearerEndpoint> {
    vec![
        BearerEndpoint::transparent(
            key(),
            rb(10),
            Scope::User(CnDomain::Cs),
            transparent(segmentation),
        )
        .unwrap(),
        BearerEndpoint::transparent(
            key(),
            rb(11),
            Scope::User(CnDomain::Cs),
            transparent(Segmentation::Disabled),
        )
        .unwrap(),
        BearerEndpoint::acknowledged(key(), rb(20), Scope::User(CnDomain::Ps), am()).unwrap(),
        BearerEndpoint::unacknowledged(
            key(),
            rb(1),
            Scope::Signalling,
            UmTransmitter::new(um_configuration(), um_limits()).unwrap(),
            UmReceiver::new(um_configuration(), um_limits()).unwrap(),
        )
        .unwrap(),
    ]
}
fn formats() -> Arc<ConfiguredTfcs> {
    let definitions = [
        (1, 21, 4, Rel99Tti::Ms20),
        (2, 128, 1, Rel99Tti::Ms10),
        (3, 96, 1, Rel99Tti::Ms10),
    ]
    .into_iter()
    .map(|(id, bits, blocks, interval)| TransportDefinition {
        id: tr(id),
        interval,
        attribute: NonZeroU16::new(8).unwrap(),
        formats: vec![CodedBlockFormat {
            block_bits: bits,
            blocks,
            crc: TransportCrc::Crc16,
            coding: ChannelCoding::Convolutional(CodeRate::Half),
        }],
    })
    .collect();
    Arc::new(
        ConfiguredTfcs::new(
            definitions,
            vec![CombinationDefinition {
                tfci: Tfci::new(0).unwrap(),
                formats: vec![0, 0, 0],
            }],
            n(100_000),
        )
        .unwrap(),
    )
}
fn bindings() -> Vec<TransportBinding> {
    vec![
        TransportBinding {
            transport: tr(1),
            channels: TransportChannels::Multiplexed(vec![
                (logical(1), rb(10)),
                (logical(2), rb(11)),
            ]),
        },
        TransportBinding {
            transport: tr(2),
            channels: TransportChannels::Single(rb(20)),
        },
        TransportBinding {
            transport: tr(3),
            channels: TransportChannels::Single(rb(1)),
        },
    ]
}
fn limits(slots: usize) -> BearerLimits {
    BearerLimits {
        maximum_transport_bits: n(4096),
        output_slots: n(slots),
        maximum_sdu_bytes: n(128),
        maximum_buffer_bytes: 2_000_000,
    }
}
fn bank(slots: usize, segmentation: Segmentation) -> DedicatedBearers {
    let mut bank = DedicatedBearers::new(
        identity(),
        anchor(),
        256,
        formats(),
        bindings(),
        endpoints(segmentation),
        limits(slots),
    )
    .unwrap();
    bank.configure_downlink(anchor(), 256, formats(), bindings())
        .unwrap();
    bank.activate(identity()).unwrap();
    bank
}

#[test]
fn hsdpa_route_boundary_excludes_packet_grants_without_changing_cs_or_uplink() {
    let mut bank = bank(8, Segmentation::Disabled);
    let previous = formats();
    let mut definitions: Vec<_> = previous
        .transport_ids()
        .map(|id| {
            let definition = previous.definition(id).unwrap();
            TransportDefinition {
                id,
                interval: definition.interval,
                attribute: definition.attribute,
                formats: definition.formats.clone(),
            }
        })
        .collect();
    let mut empty = definitions[1].formats[0];
    empty.blocks = 0;
    definitions[1].formats.push(empty);
    let formats = Arc::new(
        ConfiguredTfcs::new(
            definitions,
            vec![
                CombinationDefinition {
                    tfci: Tfci::new(0).unwrap(),
                    formats: vec![0, 0, 0],
                },
                CombinationDefinition {
                    tfci: Tfci::new(1).unwrap(),
                    formats: vec![0, 1, 0],
                },
            ],
            n(100_000),
        )
        .unwrap(),
    );
    bank.maps[bank.current]
        .as_mut()
        .unwrap()
        .downlink
        .as_mut()
        .unwrap()
        .formats = formats;
    let uplink = bank.maps[bank.current].as_ref().unwrap().uplink.clone();
    bank.schedule_hsdpa_downlink(identity(), at(2), &[rb(20)])
        .unwrap();
    let grant = DownlinkGrant {
        identity: identity(),
        bearer: rb(20),
        transport: tr(2),
        format: 0,
        start: at(1),
    };
    assert!(bank.grant(grant).is_ok());
    assert!(matches!(
        bank.grant(DownlinkGrant {
            start: at(2),
            ..grant
        }),
        Err(BearerError::Mapping)
    ));
    let plan = bank.maps[bank.current]
        .as_ref()
        .unwrap()
        .downlink
        .as_ref()
        .unwrap();
    assert!(!plan.dch_transport_available(tr(2), at(2)));
    assert!(plan.dch_transport_available(tr(1), at(2)));
    assert!(plan.dch_transport_available(tr(3), at(2)));
    assert!(Arc::ptr_eq(
        &uplink,
        &bank.maps[bank.current].as_ref().unwrap().uplink
    ));
}

#[test]
fn hsdpa_route_validation_is_transactional() {
    let mut bank = bank(8, Segmentation::Disabled);
    assert_eq!(
        bank.schedule_hsdpa_downlink(identity(), at(2), &[rb(20)]),
        Err(BearerError::Transport)
    );
    assert_eq!(
        bank.schedule_hsdpa_downlink(identity(), at(1), &[rb(20)]),
        Err(BearerError::Time)
    );
    assert_eq!(
        bank.schedule_hsdpa_downlink(identity(), at(2), &[rb(20), rb(10)]),
        Err(BearerError::Mapping)
    );
    let plan = bank.maps[bank.current]
        .as_ref()
        .unwrap()
        .downlink
        .as_ref()
        .unwrap();
    assert!(plan.hsdpa_activation.iter().all(Option::is_none));
}

#[test]
fn shared_packet_publication_rejection_preserves_rlc_sequence_and_cipher_bytes() {
    let mut bank = bank(8, Segmentation::Disabled);
    let ciphers = ciphers();
    bank.enqueue_sdu(rb(20), b"retained packet").unwrap();
    let request = || transmit::SequencedGrant {
        bearer: rb(20),
        payload_bits: 128,
        mapping: DedicatedMapping::Single,
        logical: None,
    };
    let mut rejected = None;
    let result = bank.publish_sequenced_payload(request(), at(0), &ciphers, |pdu| {
        rejected = Some(packed(pdu.data));
        Err::<(), _>("HARQ capacity")
    });
    assert!(matches!(
        result,
        Err(TransmitError::Admission("HARQ capacity"))
    ));
    let accepted = bank
        .publish_sequenced_payload(request(), at(0), &ciphers, |pdu| {
            Ok::<_, ()>(packed(pdu.data))
        })
        .unwrap()
        .unwrap();
    assert_eq!(Some(accepted), rejected);
    let Entity::Acknowledged(entity) = &bank.endpoints[bank.lookup[rb(20).index()].unwrap()].entity
    else {
        panic!("packet RLC");
    };
    assert_eq!(entity.outstanding_pdus(), 1);
}

#[test]
fn invalid_packet_size_does_not_consume_rlc_publication() {
    let mut bank = bank(8, Segmentation::Disabled);
    bank.enqueue_sdu(rb(20), b"retained packet").unwrap();
    let result: Result<Option<()>, TransmitError<()>> = bank.publish_sequenced_payload(
        transmit::SequencedGrant {
            bearer: rb(20),
            payload_bits: 120,
            mapping: DedicatedMapping::Single,
            logical: None,
        },
        at(0),
        &ciphers(),
        |_| panic!("invalid PDU must not reach HARQ"),
    );
    assert!(matches!(
        result,
        Err(TransmitError::Bearer(BearerError::Transport))
    ));
    let Entity::Acknowledged(entity) = &bank.endpoints[bank.lookup[rb(20).index()].unwrap()].entity
    else {
        panic!("packet RLC");
    };
    assert_eq!(entity.outstanding_pdus(), 0);
}

#[test]
fn asymmetric_tm_segmentation_controls_the_actual_receive_and_transmit_paths_independently() {
    let mut reference = None;
    for (uplink, downlink) in [
        (Segmentation::Enabled, Segmentation::Disabled),
        (Segmentation::Disabled, Segmentation::Enabled),
    ] {
        let mut endpoints = endpoints(Segmentation::Disabled);
        let mut settings = transparent(Segmentation::Enabled);
        settings.uplink_segmentation = uplink;
        settings.downlink_segmentation = downlink;
        endpoints[0] =
            BearerEndpoint::transparent(key(), rb(10), Scope::User(CnDomain::Cs), settings)
                .unwrap();
        let mut bank = DedicatedBearers::new(
            identity(),
            anchor(),
            256,
            formats(),
            bindings(),
            endpoints,
            limits(8),
        )
        .unwrap();
        bank.configure_downlink(anchor(), 256, formats(), bindings())
            .unwrap();
        bank.activate(identity()).unwrap();
        let mut ciphers = ciphers();
        let (input, _) = tm_input(0x1a00, 0x11);
        enqueue(&mut bank, 1, 0, bits(&input, 84)).unwrap();
        run(&mut bank, &mut ciphers);
        let output = collect(&mut bank);
        let count = output
            .iter()
            .filter(|(meta, _)| meta.bearer == rb(10))
            .count();
        assert_eq!(
            count,
            if uplink == Segmentation::Enabled {
                1
            } else {
                2
            }
        );
        let payload = [0x91, 0x34, 0x80];
        let mut joined = [0; 9];
        for block in 0..4 {
            bits(&payload, 17)
                .copy_into(&mut joined, block * 17)
                .unwrap();
        }
        let sdus = if downlink == Segmentation::Enabled {
            vec![bits(&joined, 68)]
        } else {
            vec![bits(&payload, 17); 4]
        };
        let transmitted = bank
            .publish_transparent(
                DownlinkGrant {
                    identity: identity(),
                    bearer: rb(10),
                    transport: tr(1),
                    format: 0,
                    start: at(0),
                },
                &sdus,
                &ciphers,
                |pdu| Ok::<_, ()>(packed(pdu.data)),
            )
            .unwrap();
        if let Some(reference) = &reference {
            assert_eq!(&transmitted, reference);
        }
        reference = Some(transmitted);
    }
}
fn context(domain: CnDomain, generation: u64, byte: u8) -> CipherContext {
    KeySet::uia1(
        KeyOrigin {
            ue: key(),
            domain,
            generation: NonZeroU64::new(generation).unwrap(),
        },
        CipherMaterial::Uea1(Key::new([byte; 16])),
        Key::new([0; 16]),
    )
    .split()
    .0
}
fn ciphers() -> BearerCiphers {
    let mut ciphers = BearerCiphers::new(key(), n(8)).unwrap();
    for (id, scope, mode, domain, secret) in [
        (
            10,
            Scope::User(CnDomain::Cs),
            CipherMode::Transparent,
            CnDomain::Cs,
            0x11,
        ),
        (
            11,
            Scope::User(CnDomain::Cs),
            CipherMode::Transparent,
            CnDomain::Cs,
            0x11,
        ),
        (
            20,
            Scope::User(CnDomain::Ps),
            CipherMode::Acknowledged,
            CnDomain::Ps,
            0x22,
        ),
        (
            1,
            Scope::Signalling,
            CipherMode::Unacknowledged,
            CnDomain::Cs,
            0x11,
        ),
    ] {
        let count = if mode == CipherMode::Transparent {
            CountAnchor::new(mode, Position::Frame(256), 0x1a00).unwrap()
        } else {
            CountAnchor::new(mode, Position::Sequence(0), 0).unwrap()
        };
        ciphers
            .register_with(
                BearerCipherConfiguration {
                    bearer: id,
                    scope,
                    transmit: count,
                    receive: count,
                    initial: Configuration::Configured(context(domain, 1, secret)),
                },
                || Ok::<_, ()>(()),
            )
            .unwrap();
    }
    ciphers
}
fn bits(bytes: &[u8], length: usize) -> Bits<'_> {
    Bits::new(bytes, length).unwrap()
}
fn packed(data: Bits<'_>) -> Vec<u8> {
    let mut bytes = vec![0; data.len().div_ceil(BITS_PER_BYTE)];
    data.copy_to(&mut bytes).unwrap();
    bytes
}
fn enqueue(
    bank: &mut DedicatedBearers,
    channel: u8,
    frame: u64,
    payload: Bits<'_>,
) -> Result<(), BearerError> {
    let config = bank.configured().definition(tr(channel)).unwrap();
    let format = config.formats[0];
    let input: Vec<_> = (0..payload.len())
        .map(|i| Bit::from(payload.bit(i).unwrap()))
        .collect();
    bank.enqueue_transport(TransportDelivery {
        decoded: DecodedTransport {
            identity: identity(),
            transport: tr(channel),
            format: 0,
            start: at(frame),
            block_bits: format.block_bits,
            blocks: format.blocks,
            crc_checked: true,
        },
        payload: &input,
    })
}
fn collect(bank: &mut DedicatedBearers) -> Vec<(UpperSdu, Vec<u8>)> {
    let mut output = Vec::new();
    while bank.process_one(|delivery| {
        output.push((delivery.metadata, packed(delivery.data)));
        true
    }) {}
    output
}
fn run(bank: &mut DedicatedBearers, ciphers: &mut BearerCiphers) {
    for _ in 0..24 {
        bank.service(at(20), ciphers, ResetPermission::Defer)
            .unwrap();
    }
}
fn tm_input(first_count: u32, secret: u8) -> (Vec<u8>, Vec<Vec<u8>>) {
    let payloads = [
        vec![0x81, 0x92, 0x80],
        vec![0x54, 0x2a, 0],
        vec![0x45, 0x67, 0],
        vec![0xfa, 0xde, 0x80],
    ];
    let mut encrypted = [vec![0; 5], vec![0; 5]];
    for bearer in 0..2 {
        bits(&payloads[bearer], 17)
            .copy_into(&mut encrypted[bearer], 0)
            .unwrap();
        bits(&payloads[bearer + 2], 17)
            .copy_into(&mut encrypted[bearer], 17)
            .unwrap();
        Uea1Key::new(&Key::new([secret; 16]))
            .apply(
                first_count,
                9 + bearer as u8,
                Direction::Uplink,
                &mut encrypted[bearer],
                34,
            )
            .unwrap();
    }
    let mut transport = vec![0; 11];
    for block in 0..4 {
        umts_types::bits::write_u32(&mut transport, block * 21, 4, (block % 2) as u32).unwrap();
        bits(&encrypted[block % 2], 34)
            .subspan((block / 2) * 17..(block / 2 + 1) * 17)
            .unwrap()
            .copy_into(&mut transport, block * 21 + 4)
            .unwrap();
    }
    (transport, payloads.to_vec())
}

#[test]
fn transparent_ct_demultiplexing_ciphers_each_rb_concatenation_once_at_first_tti_cfn() {
    let mut bank = bank(8, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let (input, expected) = tm_input(0x1a00, 0x11);
    enqueue(&mut bank, 1, 0, bits(&input, 84)).unwrap();
    assert_eq!(
        bank.service(at(1), &mut ciphers, ResetPermission::Defer),
        Ok(BearerProgress::Idle)
    );
    run(&mut bank, &mut ciphers);
    let output = collect(&mut bank);
    assert_eq!(output.len(), 4);
    for (position, (metadata, bytes)) in output.iter().enumerate() {
        let expected_index = [0, 2, 1, 3][position];
        assert_eq!(metadata.bearer, rb(if position < 2 { 10 } else { 11 }));
        assert_eq!(*bytes, expected[expected_index]);
        assert_eq!(metadata.bits, 17);
        assert_eq!(metadata.quality, Quality::Correct);
        assert_eq!(metadata.received, at(2));
    }
    assert_eq!(bank.counters().tm_units, 2);
    assert_eq!(bank.counters().pdus, 4);
    assert_eq!(
        enqueue(&mut bank, 1, 0, bits(&input, 84)),
        Err(BearerError::Time)
    );
}

#[test]
fn transparent_segmentation_and_domain_rekey_preserve_other_bearers_and_old_frame_selection() {
    let mut bank = bank(8, Segmentation::Enabled);
    let mut ciphers = ciphers();
    let next = context(CnDomain::Cs, 2, 0x33);
    ciphers
        .update_with(
            next,
            &[CipherActivation::Transparent {
                domain: CnDomain::Cs,
                anchor: CountAnchor::new(CipherMode::Transparent, Position::Frame(264), 0x2a08)
                    .unwrap(),
                first_frame: 264,
            }],
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let ps_before = ciphers
        .selected(key(), 20, Direction::Uplink, Position::Sequence(0))
        .unwrap();
    for (frame, count, secret) in [(0, 0x1a00, 0x11), (8, 0x2a08, 0x33)] {
        let (input, expected) = tm_input(count, secret);
        enqueue(&mut bank, 1, frame, bits(&input, 84)).unwrap();
        run(&mut bank, &mut ciphers);
        let output = collect(&mut bank);
        assert_eq!(output.len(), 3);
        let mut joined = vec![0; 5];
        bits(&expected[0], 17).copy_into(&mut joined, 0).unwrap();
        bits(&expected[2], 17).copy_into(&mut joined, 17).unwrap();
        assert_eq!(output[0].0.bits, 34);
        assert_eq!(output[0].1, joined);
        assert_eq!(output[1].1, expected[1]);
        assert_eq!(output[2].1, expected[3]);
    }
    assert_eq!(
        ciphers
            .selected(key(), 20, Direction::Uplink, Position::Sequence(0))
            .unwrap(),
        ps_before
    );
}

#[test]
fn um_batch_admission_and_upper_retry_preserve_input_and_sequence_until_capacity_exists() {
    let mut bank = bank(4, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let (input, _) = tm_input(0x1a00, 0x11);
    enqueue(&mut bank, 1, 0, bits(&input, 84)).unwrap();
    run(&mut bank, &mut ciphers);
    let mut pdu = [1, 0xf9, 5, 11, 0xfe, b'a', b'b', b'X', b'Y', b'Z', 0, 0];
    Uea1Key::new(&Key::new([0x11; 16]))
        .apply(0, 0, Direction::Uplink, &mut pdu[1..], 88)
        .unwrap();
    enqueue(&mut bank, 3, 0, bits(&pdu, 96)).unwrap();
    assert_eq!(
        bank.service(at(20), &mut ciphers, ResetPermission::Defer),
        Ok(BearerProgress::Backpressure)
    );
    assert_eq!(bank.endpoints[3].mode(), CipherMode::Unacknowledged);
    let Entity::Unacknowledged { receive, .. } = &bank.endpoints[3].entity else {
        unreachable!()
    };
    assert_eq!(receive.expected_sequence(), 0);
    assert!(!bank.process_one(|_| false));
    let mut removed = 0;
    while removed < 2 {
        assert!(bank.process_one(|_| true));
        removed += 1;
    }
    run(&mut bank, &mut ciphers);
    let output = collect(&mut bank);
    assert_eq!(output.len(), 4);
    assert_eq!(output[2].1, b"ab");
    assert_eq!(output[3].1, b"XYZ");
    assert_eq!(output[2].0.bearer, rb(1));
}

#[test]
fn am_user_data_uses_ps_cipher_with_original_receive_time_and_emits_clear_status() {
    let mut bank = bank(8, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let mut peer = am();
    peer.enqueue(b"packet").unwrap();
    let (_, mut pdu) = peer
        .publish_next(at(0), |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap();
    let pdu_len = pdu.len();
    Uea1Key::new(&Key::new([0x22; 16]))
        .apply(
            0,
            19,
            Direction::Uplink,
            &mut pdu[2..],
            (pdu_len - 2) * BITS_PER_BYTE,
        )
        .unwrap();
    enqueue(&mut bank, 2, 0, bits(&pdu, pdu_len * BITS_PER_BYTE)).unwrap();
    run(&mut bank, &mut ciphers);
    let output = collect(&mut bank);
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].1, b"packet");
    assert_eq!(output[0].0.scope, Scope::User(CnDomain::Ps));
    assert_eq!(output[0].0.received, at(1));
    assert!(output[0].0.coverage.is_some());
    let grant = DownlinkGrant {
        identity: identity(),
        bearer: rb(20),
        transport: tr(2),
        format: 0,
        start: at(21),
    };
    let (status, usage) = bank
        .publish_sequenced(grant, at(20), &ciphers, |pdu| {
            Ok::<_, ()>((packed(pdu.data), pdu.cipher))
        })
        .unwrap()
        .unwrap();
    assert_eq!(usage, umts_security::ciphering::CipherUse::Control);
    assert_eq!(
        peer.receive(at(21), &status).unwrap(),
        umts_rlc::acknowledged::AmReceive::StatusApplied
    );
    assert_eq!(peer.outstanding_pdus(), 0);
}

#[test]
fn sequenced_downlink_rejection_does_not_advance_um_and_tm_retries_do_not_reuse_committed_counts() {
    let mut bank = bank(8, Segmentation::Disabled);
    let ciphers = ciphers();
    bank.enqueue_sdu(rb(1), b"data").unwrap();
    let grant = DownlinkGrant {
        identity: identity(),
        bearer: rb(1),
        transport: tr(3),
        format: 0,
        start: at(0),
    };
    let mut attempted = None;
    assert!(matches!(
        bank.publish_sequenced(grant, at(0), &ciphers, |pdu| {
            attempted = Some(packed(pdu.data));
            Err::<(), _>("full")
        }),
        Err(TransmitError::Admission("full"))
    ));
    let actual = bank
        .publish_sequenced(grant, at(0), &ciphers, |pdu| Ok::<_, ()>(packed(pdu.data)))
        .unwrap()
        .unwrap();
    assert_eq!(actual, attempted.unwrap());
    let payloads: [Vec<u8>; 4] = std::array::from_fn(|_| vec![0x91, 0x34, 0x80]);
    let sdus: Vec<_> = payloads.iter().map(|value| bits(value, 17)).collect();
    let grant = DownlinkGrant {
        identity: identity(),
        bearer: rb(10),
        transport: tr(1),
        format: 0,
        start: at(0),
    };
    let mut attempted = None;
    assert!(matches!(
        bank.publish_transparent(grant, &sdus, &ciphers, |pdu| {
            attempted = Some(packed(pdu.data));
            Err::<(), _>(1)
        }),
        Err(TransmitError::Admission(1))
    ));
    let actual = bank
        .publish_transparent(grant, &sdus, &ciphers, |pdu| Ok::<_, ()>(packed(pdu.data)))
        .unwrap();
    assert_eq!(actual, attempted.unwrap());
    assert!(matches!(
        bank.publish_transparent(grant, &sdus, &ciphers, |_| Ok::<_, ()>(())),
        Err(TransmitError::Bearer(BearerError::Time))
    ));
}

#[test]
fn dedicated_um_grants_emit_padding_after_a_short_burst() {
    let mut bank = bank(8, Segmentation::Disabled);
    let ciphers = ciphers();
    bank.enqueue_sdu(rb(1), b"short").unwrap();
    let grant = DownlinkGrant {
        identity: identity(),
        bearer: rb(1),
        transport: tr(3),
        format: 0,
        start: at(0),
    };
    assert!(
        bank.publish_sequenced(grant, at(0), &ciphers, |_| Ok::<_, ()>(()))
            .unwrap()
            .is_some()
    );
    assert!(
        bank.publish_sequenced(grant, at(0), &ciphers, |_| Ok::<_, ()>(()))
            .unwrap()
            .is_none()
    );
    let resolved = bank.grant(grant).unwrap();
    let padding = transmit::SequencedGrant {
        bearer: grant.bearer,
        payload_bits: resolved.format.block_bits
            - if resolved.logical.is_some() {
                C_T_BITS
            } else {
                0
            },
        mapping: resolved.mapping,
        logical: resolved.logical,
    };
    for _ in 0..3 {
        assert!(
            bank.publish_um_padding(padding, at(0), &ciphers, |pdu| {
                assert_eq!(pdu.data.len(), resolved.format.block_bits);
                Ok::<_, ()>(())
            })
            .unwrap()
            .is_some()
        );
    }
    let Entity::Unacknowledged { transmit, .. } =
        &bank.endpoints[bank.lookup[rb(1).index()].unwrap()].entity
    else {
        panic!("UM endpoint required");
    };
    assert_eq!(transmit.next_position(), 4);
    assert!(!bank.downlink_recovery_required);
}

#[test]
fn stale_configuration_bad_geometry_and_scope_mismatch_cannot_mutate_bearer_delivery() {
    let mut bank = bank(8, Segmentation::Disabled);
    let (input, _) = tm_input(0x1a00, 0x11);
    enqueue(&mut bank, 1, 0, bits(&input, 84)).unwrap();
    let mut wrong = BearerCiphers::new(UeKey { slot: 1, ..key() }, n(8)).unwrap();
    assert!(matches!(
        bank.service(at(20), &mut wrong, ResetPermission::Defer),
        Err(BearerError::Cipher(_))
    ));
    assert_eq!(bank.counters().tm_units, 0);
    let mut ciphers = ciphers();
    run(&mut bank, &mut ciphers);
    assert_eq!(collect(&mut bank).len(), 4);
    assert_eq!(
        bank.activate(ReceiveIdentity {
            configuration: NonZeroU64::new(2).unwrap(),
            ..identity()
        }),
        Err(BearerError::Owner)
    );
    assert_eq!(
        enqueue(&mut bank, 1, 1, bits(&input, 84)),
        Err(BearerError::Time)
    );
    bank.retire();
    assert_eq!(
        enqueue(&mut bank, 1, 2, bits(&input, 84)),
        Err(BearerError::Phase)
    );
    let endpoints = match bank.into_endpoints() {
        Ok(endpoints) => endpoints,
        Err(_) => panic!("retired endpoints"),
    };
    assert_eq!(endpoints.len(), 4);
}

#[test]
fn impossible_complete_upper_batch_is_distinguished_from_temporary_backpressure() {
    let mut bank = bank(1, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let (input, _) = tm_input(0x1a00, 0x11);
    enqueue(&mut bank, 1, 0, bits(&input, 84)).unwrap();
    assert_eq!(
        bank.service(at(20), &mut ciphers, ResetPermission::Defer),
        Err(BearerError::BatchCapacity)
    );
    assert_eq!(bank.counters().sdus, 0);
    assert_eq!(bank.counters().backpressure, 0);
}

#[test]
fn acknowledged_reset_requires_explicit_settled_authorization_and_keeps_control_clear() {
    let mut bank = bank(8, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let mut peer = am();
    peer.initiate_reset(at(0)).unwrap();
    let (_, reset) = peer
        .publish_next(at(0), |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap();
    enqueue(&mut bank, 2, 0, bits(&reset, 128)).unwrap();
    assert_eq!(
        bank.service(at(20), &mut ciphers, ResetPermission::Defer),
        Err(BearerError::ResetRecoveryRequired(
            ResetRecoveryReason::AuthorizationDeferred,
        ))
    );
    let permission = ResetPermission::Settled {
        cs: Some(context(CnDomain::Cs, 1, 0x11).origin()),
        ps: Some(context(CnDomain::Ps, 1, 0x22).origin()),
    };
    bank.service(at(20), &mut ciphers, permission).unwrap();
    let grant = DownlinkGrant {
        identity: identity(),
        bearer: rb(20),
        transport: tr(2),
        format: 0,
        start: at(21),
    };
    let (ack, usage, kind) = bank
        .publish_sequenced(grant, at(20), &ciphers, |pdu| {
            Ok::<_, ()>((packed(pdu.data), pdu.cipher, pdu.acknowledged_kind))
        })
        .unwrap()
        .unwrap();
    assert_eq!(usage, umts_security::ciphering::CipherUse::Control);
    assert_eq!(
        kind,
        Some(umts_rlc::acknowledged::PublicationKind::ResetAck)
    );
    assert!(matches!(
        peer.receive(at(21), &ack).unwrap(),
        umts_rlc::acknowledged::AmReceive::ResetAcknowledged { .. }
    ));
    assert_eq!(peer.state(), umts_rlc::acknowledged::AmState::Ready);
}

#[test]
fn endpoint_ownership_transfer_preserves_um_sequence_and_committed_tm_frame_floor() {
    let mut bank = bank(8, Segmentation::Disabled);
    let ciphers = ciphers();
    bank.enqueue_sdu(rb(1), b"first").unwrap();
    let grant = DownlinkGrant {
        identity: identity(),
        bearer: rb(1),
        transport: tr(3),
        format: 0,
        start: at(0),
    };
    let first = bank
        .publish_sequenced(grant, at(0), &ciphers, |pdu| Ok::<_, ()>(packed(pdu.data)))
        .unwrap()
        .unwrap();
    assert_eq!(first[0] >> 1, 0);
    let bytes = [0x91, 0x34, 0x80];
    let sdus = [bits(&bytes, 17); 4];
    let grant = DownlinkGrant {
        identity: identity(),
        bearer: rb(10),
        transport: tr(1),
        format: 0,
        start: at(0),
    };
    let transmitted = bank
        .publish_transparent(grant, &sdus, &ciphers, |pdu| Ok::<_, ()>(packed(pdu.data)))
        .unwrap();
    let mut encrypted = vec![0; 9];
    for block in 0..4 {
        bits(&transmitted, 84)
            .subspan(block * 21 + 4..(block + 1) * 21)
            .unwrap()
            .copy_into(&mut encrypted, block * 17)
            .unwrap();
    }
    Uea1Key::new(&Key::new([0x11; 16]))
        .apply(0x1a00, 9, Direction::Downlink, &mut encrypted, 68)
        .unwrap();
    for block in 0..4 {
        assert_eq!(
            packed(
                bits(&encrypted, 68)
                    .subspan(block * 17..(block + 1) * 17)
                    .unwrap()
            ),
            bytes
        );
    }
    bank.retire();
    let endpoints = match bank.into_endpoints() {
        Ok(endpoints) => endpoints,
        Err(_) => panic!("retired owner"),
    };
    let identity = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..identity()
    };
    let mut insufficient = limits(8);
    insufficient.maximum_buffer_bytes = 1;
    let rejected = match DedicatedBearers::new(
        identity,
        anchor(),
        256,
        formats(),
        bindings(),
        endpoints,
        insufficient,
    ) {
        Ok(_) => panic!("resource rejection"),
        Err(rejected) => rejected,
    };
    assert_eq!(rejected.error, BearerError::Capacity);
    let mut next = DedicatedBearers::new(
        identity,
        anchor(),
        256,
        formats(),
        bindings(),
        rejected.endpoints,
        limits(8),
    )
    .unwrap();
    next.configure_downlink(anchor(), 256, formats(), bindings())
        .unwrap();
    next.activate(identity).unwrap();
    assert!(matches!(
        next.publish_transparent(
            DownlinkGrant { identity, ..grant },
            &sdus,
            &ciphers,
            |_| Ok::<_, ()>(())
        ),
        Err(TransmitError::Bearer(BearerError::Time))
    ));
    next.enqueue_sdu(rb(1), b"second").unwrap();
    let grant = DownlinkGrant {
        identity,
        bearer: rb(1),
        transport: tr(3),
        format: 0,
        start: at(1),
    };
    let second = next
        .publish_sequenced(grant, at(0), &ciphers, |pdu| Ok::<_, ()>(packed(pdu.data)))
        .unwrap()
        .unwrap();
    assert_eq!(second[0] >> 1, 1);
}

#[test]
fn invalid_endpoint_binding_returns_the_live_rlc_owner_instead_of_resetting_or_dropping_it() {
    let mut entity = am();
    entity.enqueue(b"retained").unwrap();
    let rejected =
        match BearerEndpoint::acknowledged(key(), rb(1), Scope::User(CnDomain::Ps), entity) {
            Ok(_) => panic!("invalid SRB scope"),
            Err(rejected) => rejected,
        };
    assert_eq!(rejected.error, BearerError::Mapping);
    assert!(rejected.entity.pending_new_data());
    let endpoint =
        BearerEndpoint::acknowledged(key(), rb(20), Scope::User(CnDomain::Ps), *rejected.entity)
            .unwrap();
    let Entity::Acknowledged(entity) = endpoint.entity else {
        unreachable!()
    };
    assert_eq!(entity.send_sequence(), 0);
    assert!(entity.pending_new_data());
    let mut transmit = UmTransmitter::new(um_configuration(), um_limits()).unwrap();
    transmit.enqueue(b"held").unwrap();
    let receive = UmReceiver::new(um_configuration(), um_limits()).unwrap();
    let rejected =
        match BearerEndpoint::unacknowledged(key(), rb(20), Scope::Signalling, transmit, receive) {
            Ok(_) => panic!("invalid user scope"),
            Err(rejected) => rejected,
        };
    assert_eq!(rejected.entity.0.pending_sdus(), 1);
    assert_eq!(rejected.entity.0.next_position(), 0);
    assert_eq!(rejected.entity.1.expected_sequence(), 0);
}

#[test]
fn received_tm_frame_floor_survives_rebind_rejection_and_later_activation() {
    let mut bank = bank(8, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let (input, _) = tm_input(0x1a00, 0x11);
    enqueue(&mut bank, 1, 0, bits(&input, 84)).unwrap();
    run(&mut bank, &mut ciphers);
    assert_eq!(collect(&mut bank).len(), 4);
    bank.retire();
    let endpoints = match bank.into_endpoints() {
        Ok(endpoints) => endpoints,
        Err(_) => panic!("retired endpoints"),
    };
    let identity = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..identity()
    };
    let rejected = match DedicatedBearers::new(
        identity,
        anchor(),
        256,
        formats(),
        bindings(),
        endpoints,
        limits(8),
    ) {
        Ok(_) => panic!("backdated activation"),
        Err(rejected) => rejected,
    };
    assert_eq!(rejected.error, BearerError::Time);
    let anchor = FrameAnchor {
        time: at(2),
        cfn: 2,
    };
    let mut next = DedicatedBearers::new(
        identity,
        anchor,
        258,
        formats(),
        bindings(),
        rejected.endpoints,
        limits(8),
    )
    .unwrap();
    next.activate(identity).unwrap();
    let (input, expected) = tm_input(0x1a02, 0x11);
    let payload: Vec<_> = (0..84)
        .map(|i| Bit::from(bits(&input, 84).bit(i).unwrap()))
        .collect();
    next.enqueue_transport(TransportDelivery {
        decoded: DecodedTransport {
            identity,
            transport: tr(1),
            format: 0,
            start: at(2),
            block_bits: 21,
            blocks: 4,
            crc_checked: true,
        },
        payload: &payload,
    })
    .unwrap();
    run(&mut next, &mut ciphers);
    let output = collect(&mut next);
    assert_eq!(output.len(), 4);
    for payload in expected {
        assert!(output.iter().any(|(_, bytes)| *bytes == payload));
    }
}

#[test]
fn configured_crc_transport_outputs_feed_the_same_bearer_dispatch_without_relabeling() {
    use crate::dedicated_rx::{DedicatedReceiveSession, ReceiveLimits};
    use umts_dsp::{
        coding::{CodedBlockEncoder, DecodeBudget},
        dedicated::UplinkLayout,
        dedicated_transport::{PuncturingLimit, TransportPlanLimits, UplinkTfcs},
        interleaving::Interleaver,
    };
    let configured = formats();
    let tfcs = Arc::new(
        UplinkTfcs::new(
            configured.clone(),
            &[UplinkLayout::single(32).unwrap()],
            PuncturingLimit::new(100).unwrap(),
            TransportPlanLimits {
                maximum_coded_bits: n(100_000),
                maximum_mapping_entries: n(1_000_000),
                decode: DecodeBudget::new(std::num::NonZeroU8::new(4).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    );
    let mut receiver = DedicatedReceiveSession::new(
        identity(),
        anchor(),
        tfcs.clone(),
        ReceiveLimits {
            output_slots: n(8),
            maximum_output_bits: n(4096),
            maximum_output_bytes: 32768,
            maximum_coded_bits: n(100_000),
            maximum_frame_bits: n(100_000),
            maximum_decodes_per_step: n(1),
        },
    )
    .unwrap();
    let mut bank = bank(8, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let (tm, expected) = tm_input(0x1a00, 0x11);
    let mut peer = am();
    peer.enqueue(b"packet").unwrap();
    let (_, mut am) = peer
        .publish_next(at(0), |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap();
    Uea1Key::new(&Key::new([0x22; 16]))
        .apply(0, 19, Direction::Uplink, &mut am[2..], 112)
        .unwrap();
    let mut um = [1, 0xf9, 5, 11, 0xfe, b'a', b'b', b'X', b'Y', b'Z', 0, 0];
    Uea1Key::new(&Key::new([0x11; 16]))
        .apply(0, 0, Direction::Uplink, &mut um[1..], 88)
        .unwrap();
    let source = [bits(&tm, 84), bits(&am, 128), bits(&um, 96)];
    for frame in 0..2 {
        let allocation = tfcs.allocation(Tfci::new(0).unwrap()).unwrap();
        let mut combined = Vec::new();
        for ((id, source), matched) in configured
            .transport_ids()
            .zip(source)
            .zip(allocation.channels())
        {
            let definition = configured.definition(id).unwrap();
            let plan = configured.block_plan(id, 0).unwrap();
            let phases = definition.interval.frames();
            let width = plan.coded_bits().div_ceil(phases);
            let payload: Vec<_> = (0..source.len())
                .map(|i| Bit::from(source.bit(i).unwrap()))
                .collect();
            let mut coded = vec![Bit::Zero; width * phases];
            CodedBlockEncoder::new(plan)
                .unwrap()
                .encode(&payload, &mut coded[..plan.coded_bits()])
                .unwrap();
            let mut first = coded.clone();
            Interleaver::first(definition.interval, coded.len())
                .unwrap()
                .interleave(&coded, &mut first)
                .unwrap();
            let phase = frame % phases;
            let mut output = vec![Bit::Zero; matched.output_bits];
            plan.matching_uplink(definition.interval, output.len(), phase, n(1_000_000))
                .unwrap()
                .apply(&first[phase * width..(phase + 1) * width], &mut output)
                .unwrap();
            combined.extend(
                output
                    .iter()
                    .map(|bit| if *bit == Bit::Zero { 1.0_f64 } else { -1.0 }),
            );
        }
        let width = combined.len() / allocation.layout().len();
        let physical: Vec<_> = combined
            .chunks_exact(width)
            .map(|input| {
                let mut output = vec![0.0; width];
                Interleaver::second(width)
                    .unwrap()
                    .interleave(input, &mut output)
                    .unwrap();
                output
            })
            .collect();
        receiver
            .receive(
                identity(),
                at(frame as u64),
                frame as u8,
                Tfci::new(0).unwrap(),
                &physical.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            )
            .unwrap();
        for _ in 0..3 {
            receiver.service().unwrap();
            while receiver.process_one(|delivery| {
                assert!(delivery.decoded.crc_checked);
                if delivery.decoded.start == at(0) {
                    bank.enqueue_transport(delivery).unwrap();
                    run(&mut bank, &mut ciphers);
                }
                true
            }) {}
        }
    }
    let received = collect(&mut bank);
    assert_eq!(received.len(), 7);
    for payload in &expected {
        assert!(
            received
                .iter()
                .any(|(metadata, bytes)| metadata.bits == 17 && bytes == payload)
        );
    }
    assert!(received.iter().any(|(_, bytes)| bytes == b"packet"));
    assert!(received.iter().any(|(_, bytes)| bytes == b"ab"));
    assert!(received.iter().any(|(_, bytes)| bytes == b"XYZ"));
}
