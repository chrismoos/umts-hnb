// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    coding::{
        ChannelCoding, CodedBlockDecoder, CodedBlockEncoder, CodedBlockFormat, CodedBlockPlan,
    },
    tfci::Tfci,
};
use std::sync::Arc;

fn tfci(index: u16) -> Tfci {
    Tfci::new(index).unwrap()
}
fn format(
    bits: usize,
    blocks: usize,
    crc: TransportCrc,
    coding: ChannelCoding,
) -> CodedBlockFormat {
    CodedBlockFormat {
        block_bits: bits,
        blocks,
        crc,
        coding,
    }
}
fn definitions() -> Vec<TransportDefinition> {
    let voice = format(
        40,
        1,
        TransportCrc::Crc16,
        ChannelCoding::Convolutional(CodeRate::Third),
    );
    let packet = format(400, 1, TransportCrc::Crc24, ChannelCoding::Turbo);
    vec![
        TransportDefinition {
            id: id(1),
            interval: Rel99Tti::Ms20,
            attribute: NonZeroU16::new(16).unwrap(),
            formats: vec![CodedBlockFormat { blocks: 0, ..voice }, voice],
        },
        TransportDefinition {
            id: id(5),
            interval: Rel99Tti::Ms10,
            attribute: NonZeroU16::new(8).unwrap(),
            formats: vec![
                CodedBlockFormat {
                    blocks: 0,
                    ..packet
                },
                packet,
            ],
        },
    ]
}
fn combinations() -> Vec<CombinationDefinition> {
    [(0, [0, 0]), (7, [1, 0]), (15, [0, 1]), (33, [1, 1])]
        .into_iter()
        .map(|(index, formats)| CombinationDefinition {
            tfci: tfci(index),
            formats: formats.to_vec(),
        })
        .collect()
}
fn configured() -> Arc<ConfiguredTfcs> {
    Arc::new(ConfiguredTfcs::new(definitions(), combinations(), count(200000)).unwrap())
}

#[test]
fn shared_voice_and_signalling_reject_information_losing_downlink_allocations() {
    let transports = [
        (
            244,
            TransportCrc::Crc16,
            CodeRate::Third,
            Rel99Tti::Ms40,
            200,
        ),
        (
            81,
            TransportCrc::Crc12,
            CodeRate::Third,
            Rel99Tti::Ms20,
            200,
        ),
        (
            103,
            TransportCrc::None,
            CodeRate::Third,
            Rel99Tti::Ms20,
            190,
        ),
        (60, TransportCrc::None, CodeRate::Half, Rel99Tti::Ms20, 235),
    ];
    let definitions = transports
        .into_iter()
        .enumerate()
        .map(|(index, (bits, crc, rate, interval, attribute))| {
            let active = format(bits, 1, crc, ChannelCoding::Convolutional(rate));
            TransportDefinition {
                id: id(u8::try_from(index + 1).unwrap()),
                interval,
                attribute: NonZeroU16::new(attribute).unwrap(),
                formats: vec![
                    CodedBlockFormat {
                        blocks: 0,
                        ..active
                    },
                    active,
                ],
            }
        })
        .collect();
    let configured = Arc::new(
        ConfiguredTfcs::new(
            definitions,
            vec![
                CombinationDefinition {
                    tfci: tfci(0),
                    formats: vec![0; 4],
                },
                CombinationDefinition {
                    tfci: tfci(1),
                    formats: vec![1; 4],
                },
            ],
            count(200000),
        )
        .unwrap(),
    );
    for (number, capacity, accepted) in [(7, 120, false), (11, 420, true)] {
        let physical = DownlinkFormat::new(number, true).unwrap();
        assert_eq!(physical.data_bits() * 15, capacity);
        let fixed = FixedDownlinkTfcs::new(Arc::clone(&configured), physical, count(1), limits());
        let flexible =
            FlexibleDownlinkTfcs::new(Arc::clone(&configured), physical, count(1), limits());
        let validation =
            FlexibleDownlinkTfcs::validate_rate_matching(&configured, physical, count(1), limits());
        assert_eq!(fixed.is_ok(), accepted);
        assert_eq!(flexible.is_ok(), accepted);
        assert_eq!(validation.is_ok(), accepted);
        if let Ok(flexible) = flexible {
            assert!(flexible.matched_tti_bits(id(1), 1).unwrap() >= 244);
        }
    }
}
fn small() -> Arc<ConfiguredTfcs> {
    Arc::new(
        ConfiguredTfcs::new(
            vec![
                TransportDefinition {
                    id: id(1),
                    interval: Rel99Tti::Ms40,
                    attribute: NonZeroU16::new(3).unwrap(),
                    formats: vec![format(
                        1,
                        1,
                        TransportCrc::None,
                        ChannelCoding::Convolutional(CodeRate::Third),
                    )],
                },
                TransportDefinition {
                    id: id(5),
                    interval: Rel99Tti::Ms10,
                    attribute: NonZeroU16::MIN,
                    formats: vec![format(40, 1, TransportCrc::None, ChannelCoding::Turbo)],
                },
            ],
            vec![CombinationDefinition {
                tfci: tfci(4),
                formats: vec![0, 0],
            }],
            count(200000),
        )
        .unwrap(),
    )
}
fn frames(plan: CodedBlockPlan, interval: Rel99Tti, payload: &[Bit]) -> Vec<Bit> {
    let total = plan.coded_bits().div_ceil(interval.frames()) * interval.frames();
    let mut coded = vec![Bit::Zero; total];
    CodedBlockEncoder::new(plan)
        .unwrap()
        .encode(payload, &mut coded[..plan.coded_bits()])
        .unwrap();
    let mut output = coded.clone();
    Interleaver::first(interval, total)
        .unwrap()
        .interleave(&coded, &mut output)
        .unwrap();
    output
}

#[test]
fn configured_formats_reject_ambiguous_identity_coding_and_combinations() {
    let mut wrong = definitions();
    wrong.reverse();
    assert!(ConfiguredTfcs::new(wrong, combinations(), count(200000)).is_err());
    let mut wrong = definitions();
    wrong[0].formats[1].coding = ChannelCoding::Turbo;
    assert!(ConfiguredTfcs::new(wrong, combinations(), count(200000)).is_err());
    let mut wrong = definitions();
    wrong[0].formats[1].crc = TransportCrc::Crc24;
    assert!(ConfiguredTfcs::new(wrong, combinations(), count(200000)).is_err());
    let mut wrong = definitions();
    let duplicate = wrong[0].formats[1];
    wrong[0].formats.push(duplicate);
    assert!(ConfiguredTfcs::new(wrong, combinations(), count(200000)).is_err());
    let mut wrong = combinations();
    wrong.swap(0, 1);
    assert!(ConfiguredTfcs::new(definitions(), wrong, count(200000)).is_err());
    let mut wrong = combinations();
    wrong[1].formats = vec![0, 0];
    assert!(ConfiguredTfcs::new(definitions(), wrong, count(200000)).is_err());
    let mut wrong = combinations();
    wrong[1].formats = vec![2, 0];
    assert!(ConfiguredTfcs::new(definitions(), wrong, count(200000)).is_err());
    let config = configured();
    assert_eq!(config.total_coded_bits(), 192 + 1284);
    assert!(ConfiguredTfcs::new(definitions(), combinations(), count(1475)).is_err());
    assert!(config.combination(tfci(3)).is_none());
    assert!(config.block_plan(id(3), 0).is_none());
}

#[test]
fn uplink_tfcs_precompiles_restricted_choices_and_mixed_tti_output_variants() {
    let config = configured();
    let plan = UplinkTfcs::new(
        Arc::clone(&config),
        &[UplinkLayout::single(32).unwrap()],
        PuncturingLimit::new(40).unwrap(),
        limits(),
    )
    .unwrap();
    assert!(plan.allocation(tfci(0)).unwrap().layout().is_empty());
    assert_eq!(
        plan.allocation(tfci(7)).unwrap().channels()[0].output_bits,
        1200
    );
    let combined = plan.allocation(tfci(33)).unwrap();
    assert_eq!(combined.channels()[0].output_bits, 156);
    assert_eq!(combined.channels()[1].output_bits, 1044);
    assert!(plan.receiver_plan(id(1), 1).is_some());
    assert!(plan.receiver_plan(id(1), 2).is_none());
    assert!(plan.allocation(tfci(8)).is_none());
    let word = tfci(33).encode();
    let decision = plan
        .decoder()
        .decode(&std::array::from_fn(|bit| {
            if word[bit] == Bit::Zero { 1.0 } else { -1.0 }
        }))
        .unwrap()
        .unwrap();
    assert_eq!(decision.tfci, tfci(33));
    let mut voice =
        DedicatedUplink::from_plan(Arc::clone(plan.receiver_plan(id(1), 1).unwrap())).unwrap();
    let mut packet =
        DedicatedUplink::from_plan(Arc::clone(plan.receiver_plan(id(5), 1).unwrap())).unwrap();
    let voice_payload = payload(40);
    let packet_payload = payload(400);
    let voice_plan = config.block_plan(id(1), 1).unwrap();
    let packet_plan = config.block_plan(id(5), 1).unwrap();
    let voice_frames = frames(voice_plan, Rel99Tti::Ms20, &voice_payload);
    let packet_frames = frames(packet_plan, Rel99Tti::Ms10, &packet_payload);
    voice.begin(at(1024), 0).unwrap();
    packet.begin(at(1024 + CHIPS_PER_FRAME), 1).unwrap();
    for (frame, choice) in [tfci(7), tfci(33)].into_iter().enumerate() {
        let allocation = plan.allocation(choice).unwrap();
        let a = allocation.channels()[0];
        let b = allocation.channels()[1];
        let mut matched_voice = vec![Bit::Zero; a.output_bits];
        voice_plan
            .matching_uplink(Rel99Tti::Ms20, a.output_bits, frame, count(200000))
            .unwrap()
            .apply(
                &voice_frames[frame * 96..(frame + 1) * 96],
                &mut matched_voice,
            )
            .unwrap();
        let mut composite: Vec<_> = matched_voice
            .iter()
            .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 })
            .collect();
        if frame == 1 {
            let mut matched_packet = vec![Bit::Zero; b.output_bits];
            packet_plan
                .matching_uplink(Rel99Tti::Ms10, b.output_bits, 0, count(200000))
                .unwrap()
                .apply(&packet_frames, &mut matched_packet)
                .unwrap();
            composite.extend(
                matched_packet
                    .iter()
                    .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 }),
            );
        }
        let mut physical = vec![0.0; 1200];
        Interleaver::second(1200)
            .unwrap()
            .interleave(&composite, &mut physical)
            .unwrap();
        let inputs = [
            UplinkRateInput {
                id: id(1),
                frame_bits: a.input_bits,
                attribute: NonZeroU16::new(16).unwrap(),
            },
            UplinkRateInput {
                id: id(5),
                frame_bits: b.input_bits,
                attribute: NonZeroU16::new(8).unwrap(),
            },
        ];
        let allocation = UplinkAllocation::new(
            &inputs,
            &[UplinkLayout::single(32).unwrap()],
            PuncturingLimit::new(40).unwrap(),
        )
        .unwrap();
        let mut demultiplexer = UplinkComposite::new(allocation).unwrap();
        let received = demultiplexer.receive(&[&physical]).unwrap();
        let time = at(1024 + frame as u64 * CHIPS_PER_FRAME);
        voice
            .receive(time, a, received.get(id(1)).unwrap())
            .unwrap();
        if frame == 1 {
            packet
                .receive(time, b, received.get(id(5)).unwrap())
                .unwrap();
        }
    }
    let mut actual_voice = [Bit::Zero; 40];
    let mut actual_packet = vec![Bit::Zero; 400];
    assert_eq!(
        voice.decode(&mut actual_voice),
        Ok(Some(BlockOutcome::Decoded))
    );
    assert_eq!(
        packet.decode(&mut actual_packet),
        Ok(Some(BlockOutcome::Decoded))
    );
    assert_eq!(actual_voice.as_slice(), voice_payload);
    assert_eq!(actual_packet, packet_payload);
}

#[test]
fn downlink_fractional_rate_allocation_and_flexible_correction_match_literal_values() {
    let physical = DownlinkFormat::new(3, true).unwrap();
    FlexibleDownlinkTfcs::validate_rate_matching(&small(), physical, count(1), limits()).unwrap();
    let fixed = FixedDownlinkTfcs::new(small(), physical, count(1), limits()).unwrap();
    let flexible = FlexibleDownlinkTfcs::new(small(), physical, count(1), limits()).unwrap();
    assert_eq!(fixed.frame_capacity(), 210);
    // 27/4 × 3 and 132 produce 152.25 weighted bits/frame. Premature integer division gives a different answer.
    assert_eq!(fixed.reserved_frame_bits(id(1)), Some(27));
    assert_eq!(fixed.reserved_frame_bits(id(5)), Some(183));
    assert_eq!(fixed.matched_tti_bits(id(1), 0), Some(108));
    assert_eq!(fixed.matched_tti_bits(id(5), 0), Some(183));
    assert_eq!(fixed.matched_tti_bits(id(5), usize::MAX), None);
    // Tentative widths are 28 and 183. The ascending-TFCI correction reduces only the first to 27.
    assert_eq!(flexible.matched_tti_bits(id(1), 0), Some(108));
    assert_eq!(flexible.matched_tti_bits(id(5), 0), Some(183));
    assert_eq!(flexible.frame_capacity(), 210);
}

#[test]
fn fixed_allocation_uses_entire_tfs_maximum_even_for_unselected_formats() {
    let config = small();
    let definitions = vec![
        TransportDefinition {
            id: id(1),
            interval: Rel99Tti::Ms40,
            attribute: NonZeroU16::new(3).unwrap(),
            formats: vec![
                config.definition(id(1)).unwrap().formats[0],
                format(
                    100,
                    1,
                    TransportCrc::None,
                    ChannelCoding::Convolutional(CodeRate::Third),
                ),
            ],
        },
        TransportDefinition {
            id: id(5),
            interval: Rel99Tti::Ms10,
            attribute: NonZeroU16::MIN,
            formats: config.definition(id(5)).unwrap().formats.clone(),
        },
    ];
    let config = Arc::new(
        ConfiguredTfcs::new(
            definitions,
            vec![CombinationDefinition {
                tfci: tfci(4),
                formats: vec![0, 0],
            }],
            count(200000),
        )
        .unwrap(),
    );
    let fixed = FixedDownlinkTfcs::new(
        config,
        DownlinkFormat::new(3, true).unwrap(),
        count(1),
        limits(),
    )
    .unwrap();
    assert_eq!(fixed.reserved_frame_bits(id(1)), Some(136));
    assert_eq!(fixed.reserved_frame_bits(id(5)), Some(74));
    assert_eq!(fixed.matched_tti_bits(id(1), 0), Some(46));
    assert_eq!(fixed.matched_tti_bits(id(1), 1), None);
    assert!(fixed.encoder(id(1), 1).is_err());
    let mut encoder = fixed.encoder(id(1), 0).unwrap();
    let mut frames = vec![TxBit::Dtx; encoder.tti_bits()];
    encoder.encode(&[Bit::One], &mut frames).unwrap();
    assert_eq!(frames.iter().filter(|bit| **bit != TxBit::Dtx).count(), 46);
}

#[test]
fn fixed_and_flexible_encoders_preserve_each_coding_and_tti_without_shared_history() {
    let config = small();
    let physical = DownlinkFormat::new(3, true).unwrap();
    let fixed = FixedDownlinkTfcs::new(Arc::clone(&config), physical, count(1), limits()).unwrap();
    let flexible =
        FlexibleDownlinkTfcs::new(Arc::clone(&config), physical, count(1), limits()).unwrap();
    for id in [id(1), id(5)] {
        let definition = config.definition(id).unwrap();
        let plan = config.block_plan(id, 0).unwrap();
        let payload = payload(plan.payload_bits());
        let mut a = fixed.encoder(id, 0).unwrap();
        let mut b = flexible.encoder(id, 0).unwrap();
        let mut c = fixed.encoder(id, 0).unwrap();
        let mut fixed_frames = vec![TxBit::Dtx; a.tti_bits()];
        a.encode(&payload, &mut fixed_frames).unwrap();
        let mut repeated = vec![TxBit::Dtx; c.tti_bits()];
        c.encode(&vec![Bit::Zero; payload.len()], &mut repeated)
            .unwrap();
        c.encode(&payload, &mut repeated).unwrap();
        assert_eq!(fixed_frames, repeated);
        let mut flexible_frames = vec![Bit::Zero; b.tti_bits()];
        b.encode(&payload, &mut flexible_frames).unwrap();
        let mut first = fixed_frames.clone();
        Interleaver::first(definition.interval, a.tti_bits())
            .unwrap()
            .deinterleave(&fixed_frames, &mut first)
            .unwrap();
        let fixed_bits: Vec<_> = first
            .into_iter()
            .filter_map(|bit| match bit {
                TxBit::Data(bit) => Some(bit),
                TxBit::Dtx => None,
            })
            .collect();
        let mut flexible_bits = flexible_frames.clone();
        Interleaver::first(definition.interval, b.tti_bits())
            .unwrap()
            .deinterleave(&flexible_frames, &mut flexible_bits)
            .unwrap();
        for (bits, matching) in [
            (
                fixed_bits,
                plan.matching_fixed(
                    plan.coded_bits(),
                    a.tti_bits() as i64 - plan.coded_bits() as i64,
                    count(200000),
                )
                .unwrap(),
            ),
            (
                flexible_bits,
                plan.matching_flexible(b.tti_bits(), count(200000)).unwrap(),
            ),
        ] {
            let soft: Vec<_> = bits
                .iter()
                .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 })
                .collect();
            let mut combined = vec![0.0; plan.coded_bits()];
            matching.combine_llrs(&soft, &mut combined).unwrap();
            let mut actual = vec![Bit::Zero; payload.len()];
            assert_eq!(
                CodedBlockDecoder::new(plan, limits().decode)
                    .unwrap()
                    .decode(&combined, &mut actual),
                Ok(BlockOutcome::Decoded)
            );
            assert_eq!(actual, payload);
        }
    }
}

#[test]
fn idle_tfcs_emits_no_transport_bits_and_nonselected_formats_have_no_decoder() {
    let mut definitions = definitions();
    for definition in &mut definitions {
        definition.formats[1].blocks = 2;
    }
    let config = Arc::new(
        ConfiguredTfcs::new(
            definitions,
            vec![CombinationDefinition {
                tfci: tfci(0),
                formats: vec![0, 0],
            }],
            count(200000),
        )
        .unwrap(),
    );
    let uplink = UplinkTfcs::new(
        Arc::clone(&config),
        &all_layouts(),
        PuncturingLimit::new(40).unwrap(),
        limits(),
    )
    .unwrap();
    assert!(uplink.receiver_plan(id(1), 1).is_none());
    let flexible = FlexibleDownlinkTfcs::new(
        Arc::clone(&config),
        DownlinkFormat::new(3, true).unwrap(),
        count(1),
        limits(),
    )
    .unwrap();
    assert!(flexible.encoder(id(5), 1).is_err());
    let mut encoder = flexible.encoder(id(5), 0).unwrap();
    assert_eq!(encoder.tti_bits(), 0);
    encoder.encode(&[], &mut []).unwrap();
    let all_idle: Vec<_> = [id(1), id(5)]
        .into_iter()
        .map(|id| {
            let definition = config.definition(id).unwrap();
            TransportDefinition {
                id,
                interval: definition.interval,
                attribute: definition.attribute,
                formats: vec![definition.formats[0]],
            }
        })
        .collect();
    let all_idle = Arc::new(
        ConfiguredTfcs::new(
            all_idle,
            vec![CombinationDefinition {
                tfci: tfci(0),
                formats: vec![0, 0],
            }],
            count(200000),
        )
        .unwrap(),
    );
    let fixed = FixedDownlinkTfcs::new(
        all_idle,
        DownlinkFormat::new(3, true).unwrap(),
        count(1),
        limits(),
    )
    .unwrap();
    assert_eq!(fixed.reserved_frame_bits(id(1)), Some(0));
    fixed
        .encoder(id(1), 0)
        .unwrap()
        .encode(&[], &mut [])
        .unwrap();
}

#[test]
fn empty_transport_blocks_crc_absence_and_no_crc_are_not_confused_with_zero_count() {
    for coding in [
        ChannelCoding::Convolutional(CodeRate::Half),
        ChannelCoding::Convolutional(CodeRate::Third),
        ChannelCoding::Turbo,
    ] {
        for crc in [
            TransportCrc::None,
            TransportCrc::Crc8,
            TransportCrc::Crc16,
            TransportCrc::Crc24,
        ] {
            for blocks in [0, 1, 2] {
                let plan =
                    CodedBlockPlan::new(format(0, blocks, crc, coding), count(200000)).unwrap();
                let mut coded = vec![Bit::One; plan.coded_bits()];
                CodedBlockEncoder::new(plan)
                    .unwrap()
                    .encode(&[], &mut coded)
                    .unwrap();
                assert!(coded.iter().all(|bit| *bit == Bit::Zero));
                let soft = vec![1.0; coded.len()];
                assert_eq!(
                    CodedBlockDecoder::new(plan, limits().decode)
                        .unwrap()
                        .decode(&soft, &mut []),
                    Ok(if blocks == 0 {
                        BlockOutcome::Absent
                    } else {
                        BlockOutcome::Decoded
                    })
                );
            }
        }
    }
}

#[test]
fn compiled_tfcs_rejects_illegal_tti_puncturing_and_aggregate_resource_exhaustion() {
    assert!(
        FlexibleDownlinkTfcs::validate_rate_matching(
            &configured(),
            DownlinkFormat::new(3, true).unwrap(),
            count(1),
            limits()
        )
        .is_err()
    );
    let mut definitions = definitions();
    definitions[0].interval = Rel99Tti::Ms80;
    let config = Arc::new(ConfiguredTfcs::new(definitions, combinations(), count(200000)).unwrap());
    assert!(
        UplinkTfcs::new(
            Arc::clone(&config),
            &all_layouts(),
            PuncturingLimit::new(40).unwrap(),
            limits()
        )
        .is_err()
    );
    assert!(
        FixedDownlinkTfcs::new(
            Arc::clone(&config),
            DownlinkFormat::new(3, true).unwrap(),
            count(1),
            limits()
        )
        .is_err()
    );
    assert!(
        FlexibleDownlinkTfcs::new(
            config,
            DownlinkFormat::new(3, true).unwrap(),
            count(1),
            limits()
        )
        .is_err()
    );
    let low = TransportPlanLimits {
        maximum_mapping_entries: count(4000),
        ..limits()
    };
    assert!(
        UplinkTfcs::new(
            configured(),
            &all_layouts(),
            PuncturingLimit::new(40).unwrap(),
            low
        )
        .is_err()
    );
    assert!(
        FixedDownlinkTfcs::new(
            configured(),
            DownlinkFormat::new(3, true).unwrap(),
            count(1),
            limits()
        )
        .is_err()
    );
    assert!(
        FlexibleDownlinkTfcs::new(
            configured(),
            DownlinkFormat::new(3, true).unwrap(),
            count(1),
            limits()
        )
        .is_err()
    );
}
