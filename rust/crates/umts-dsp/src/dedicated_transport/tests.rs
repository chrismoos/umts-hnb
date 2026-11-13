// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    bits::{Bit, TxBit},
    convolutional::CodeRate,
    crc::TransportCrc,
    dedicated::{DownlinkFormat, UplinkLayout},
    interleaving::{Interleaver, Rel99Tti},
    rate_matching::RateMatching,
    transport_blocks::{BlockEncoder, BlockFormat, BlockOutcome, BlockPlan},
};
use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

mod incremental;
mod radio_chain;
mod tfcs;

fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn id(value: u8) -> TransportId {
    TransportId::new(value).unwrap()
}
fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}
fn limits() -> TransportPlanLimits {
    TransportPlanLimits {
        maximum_coded_bits: count(200000),
        maximum_mapping_entries: count(200000),
        decode: crate::coding::DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
    }
}
fn rate(id_number: u8, bits: usize, weight: u16) -> UplinkRateInput {
    UplinkRateInput {
        id: id(id_number),
        frame_bits: bits,
        attribute: NonZeroU16::new(weight).unwrap(),
    }
}
fn all_layouts() -> Vec<UplinkLayout> {
    let mut result: Vec<_> = [256, 128, 64, 32, 16, 8, 4]
        .into_iter()
        .map(|sf| UplinkLayout::single(sf).unwrap())
        .collect();
    result.extend((2..=6).map(|count| UplinkLayout::multicode(count).unwrap()));
    result
}
fn allocate(channels: &[UplinkRateInput], percent: u8) -> UplinkAllocation {
    UplinkAllocation::new(
        channels,
        &all_layouts(),
        PuncturingLimit::new(percent).unwrap(),
    )
    .unwrap()
}
fn payload(bits: usize) -> Vec<Bit> {
    (0..bits).map(|index| Bit::from(index % 11 < 5)).collect()
}
fn coded_frames(format: BlockFormat, interval: Rel99Tti, input: &[Bit]) -> Vec<Bit> {
    let plan = BlockPlan::new(format, limits().maximum_coded_bits).unwrap();
    let equalized = plan.coded_bits().div_ceil(interval.frames()) * interval.frames();
    let mut bits = vec![Bit::Zero; equalized];
    BlockEncoder::new(plan)
        .encode(input, &mut bits[..plan.coded_bits()])
        .unwrap();
    let mut frames = bits.clone();
    Interleaver::first(interval, equalized)
        .unwrap()
        .interleave(&bits, &mut frames)
        .unwrap();
    frames
}
fn reference_second<T: Copy>(input: &[T]) -> Vec<T> {
    let mut output = Vec::new();
    for column in [
        0, 20, 10, 5, 15, 25, 3, 13, 23, 8, 18, 28, 1, 11, 21, 6, 16, 26, 4, 14, 24, 19, 9, 29, 12,
        2, 7, 22, 27, 17,
    ] {
        for index in (column..input.len()).step_by(30) {
            output.push(input[index]);
        }
    }
    output
}

#[test]
fn set_one_two_and_weighted_cumulative_floor_follow_literal_cases() {
    for (input, pl, capacity, codes) in [
        (100, 40, 150, 1),
        (400, 40, 600, 1),
        (12000, 40, 9600, 1),
        (12000, 100, 19200, 2),
        (90000, 40, 38400, 4),
    ] {
        let plan = allocate(&[rate(1, input, 1)], pl);
        assert_eq!(plan.total_bits(), capacity);
        assert_eq!(plan.layout().len(), codes);
    }
    let plan = allocate(&[rate(1, 17, 3), rate(2, 19, 2)], 100);
    assert_eq!(
        plan.channels()
            .iter()
            .map(|channel| channel.output_bits)
            .collect::<Vec<_>>(),
        [85, 65]
    );
    let plan = allocate(&[rate(1, 100, 1), rate(2, 100, 2), rate(3, 0, 1)], 100);
    assert_eq!(
        plan.channels()
            .iter()
            .map(|channel| channel.output_bits)
            .collect::<Vec<_>>(),
        [100, 200, 0]
    );
    let idle_attribute = allocate(&[rate(1, 0, 1), rate(2, 100, 8)], 100);
    assert_eq!(idle_attribute.total_bits(), 1200);
    assert_eq!(idle_attribute.channels()[0].output_bits, 0);
}

#[test]
fn capability_restrictions_identity_order_and_all_idle_are_not_silently_changed() {
    assert!(
        UplinkAllocation::new(
            &[rate(1, 1000, 1)],
            &[UplinkLayout::single(128).unwrap()],
            PuncturingLimit::new(40).unwrap()
        )
        .is_err()
    );
    for bad in [
        vec![rate(2, 100, 1), rate(1, 100, 1)],
        vec![rate(1, 100, 1), rate(1, 100, 1)],
        vec![rate(1, 100, 257)],
    ] {
        assert!(
            UplinkAllocation::new(&bad, &all_layouts(), PuncturingLimit::new(100).unwrap())
                .is_err()
        );
    }
    assert!(
        UplinkAllocation::new(
            &[rate(1, 100, 1)],
            &[UplinkLayout::single(128).unwrap(); 2],
            PuncturingLimit::new(100).unwrap()
        )
        .is_err()
    );
    for percent in [0, 39, 41, 101] {
        assert!(PuncturingLimit::new(percent).is_err());
    }
    assert!(TransportId::new(0).is_err() && TransportId::new(33).is_err());
    let idle = UplinkAllocation::new(
        &[rate(1, 0, 1), rate(7, 0, 8)],
        &[],
        PuncturingLimit::new(100).unwrap(),
    )
    .unwrap();
    assert!(idle.layout().is_empty());
    let mut composite = UplinkComposite::new(idle).unwrap();
    let frame = composite.receive(&[]).unwrap();
    assert_eq!(frame.get(id(1)), Some([].as_slice()));
    assert_eq!(frame.get(id(7)), Some([].as_slice()));
    assert!(frame.get(id(8)).is_none());
}

#[test]
fn each_physical_code_is_deinterleaved_before_transport_demultiplexing() {
    for (input, expected_codes) in [(19000, 2), (28000, 3), (38000, 4), (47000, 5), (57000, 6)] {
        let allocation = allocate(&[rate(1, input / 3, 1), rate(7, input - input / 3, 1)], 100);
        assert_eq!(allocation.layout().len(), expected_codes);
        let split = allocation.channels()[0].output_bits;
        let joined: Vec<f64> = (0..allocation.total_bits())
            .map(|index| index as f64 + 0.25)
            .collect();
        let physical: Vec<Vec<f64>> = joined.chunks_exact(9600).map(reference_second).collect();
        let references: Vec<_> = physical.iter().map(Vec::as_slice).collect();
        let mut receiver = UplinkComposite::new(allocation).unwrap();
        let output = receiver.receive(&references).unwrap();
        assert_eq!(output.get(id(1)).unwrap(), &joined[..split]);
        assert_eq!(output.get(id(7)).unwrap(), &joined[split..]);
    }
}

#[test]
fn downlink_fixed_and_flexible_frames_use_contiguous_segmentation_and_second_dtx() {
    let format = DownlinkFormat::new(3, true).unwrap();
    let mut composite =
        DownlinkComposite::new(format, count(2), &[id(1), id(7)], count(10000)).unwrap();
    let first: Vec<_> = payload(137)
        .into_iter()
        .map(TxBit::Data)
        .chain([TxBit::Dtx; 11])
        .collect();
    let second: Vec<_> = payload(173).into_iter().map(TxBit::Data).collect();
    let mut actual = vec![TxBit::Data(Bit::One); composite.frame_bits()];
    composite
        .compose(
            &[
                DownlinkTransportFrame {
                    id: id(1),
                    bits: &first,
                },
                DownlinkTransportFrame {
                    id: id(7),
                    bits: &second,
                },
            ],
            &mut actual,
        )
        .unwrap();
    let mut joined = first.clone();
    joined.extend_from_slice(&second);
    joined.resize(actual.len(), TxBit::Dtx);
    let expected: Vec<_> = joined
        .chunks_exact(format.data_bits() * 15)
        .flat_map(reference_second)
        .collect();
    assert_eq!(actual, expected);
    let flexible = [Bit::One; 12];
    composite
        .compose_flexible(
            &[
                DownlinkTransportFrame {
                    id: id(1),
                    bits: &flexible,
                },
                DownlinkTransportFrame {
                    id: id(7),
                    bits: &[],
                },
            ],
            &mut actual,
        )
        .unwrap();
    assert_eq!(
        actual
            .iter()
            .filter(|bit| **bit == TxBit::Data(Bit::One))
            .count(),
        12
    );
    let previous = actual.clone();
    assert!(
        composite
            .compose_flexible(
                &[
                    DownlinkTransportFrame {
                        id: id(7),
                        bits: &flexible
                    },
                    DownlinkTransportFrame {
                        id: id(1),
                        bits: &[]
                    }
                ],
                &mut actual
            )
            .is_err()
    );
    assert_eq!(actual, previous);
    assert!(DownlinkComposite::new(format, count(2), &[id(1)], count(1)).is_err());
}

#[test]
fn variable_frame_allocations_are_unmatched_before_one_whole_tti_normalization() {
    for interval in [Rel99Tti::Ms10, Rel99Tti::Ms20, Rel99Tti::Ms40] {
        for gain in [1e-24, 1.0, 1e24] {
            let format = BlockFormat {
                block_bits: 101,
                blocks: 2,
                crc: TransportCrc::Crc16,
                rate: CodeRate::Third,
            };
            let expected = payload(202);
            let frames = coded_frames(format, interval, &expected);
            let width = frames.len() / interval.frames();
            let variants = [width + 17, width + 41];
            let mut decoder =
                DedicatedUplink::new(id(3), format, interval, &variants, limits()).unwrap();
            decoder.begin(at(1024), 252).unwrap();
            let mut output = vec![Bit::One; 202];
            for (frame, input) in frames.chunks_exact(width).enumerate() {
                let mut matched = vec![Bit::Zero; variants[frame % 2]];
                RateMatching::uplink_convolutional(width, matched.len(), interval, frame)
                    .unwrap()
                    .apply(input, &mut matched)
                    .unwrap();
                let soft: Vec<_> = matched
                    .iter()
                    .map(|bit| if *bit == Bit::Zero { gain } else { -gain })
                    .collect();
                let allocation = MatchedTransport {
                    id: id(3),
                    input_bits: width,
                    output_bits: soft.len(),
                };
                decoder
                    .receive(at(1024 + frame as u64 * CHIPS_PER_FRAME), allocation, &soft)
                    .unwrap();
                if frame + 1 < interval.frames() {
                    assert_eq!(decoder.decode(&mut output), Ok(None));
                }
            }
            assert_eq!(decoder.decode(&mut output), Ok(Some(BlockOutcome::Decoded)));
            assert_eq!(output, expected);
            assert!(decoder.decode(&mut output).is_err());
        }
    }
}

#[test]
fn tti_identity_timing_and_invalid_receive_do_not_advance_the_collector() {
    let format = BlockFormat {
        block_bits: 40,
        blocks: 1,
        crc: TransportCrc::Crc16,
        rate: CodeRate::Half,
    };
    let mut decoder =
        DedicatedUplink::new(id(1), format, Rel99Tti::Ms20, &[150], limits()).unwrap();
    assert!(decoder.begin(at(1024), 255).is_err());
    decoder.begin(at(1024), 254).unwrap();
    let allocation = MatchedTransport {
        id: id(1),
        input_bits: decoder.frame_input_bits(),
        output_bits: 150,
    };
    assert!(
        decoder
            .receive(at(1024 + CHIPS_PER_FRAME), allocation, &[1.0; 150])
            .is_err()
    );
    assert!(
        decoder
            .receive(
                at(1024),
                MatchedTransport {
                    id: id(2),
                    ..allocation
                },
                &[1.0; 150]
            )
            .is_err()
    );
    assert!(
        decoder
            .receive(at(1024), allocation, &[f64::NAN; 150])
            .is_err()
    );
    assert!(
        decoder
            .receive(at(1024), allocation, &[f64::MAX; 150])
            .is_err()
    );
    decoder.receive(at(1024), allocation, &[1.0; 150]).unwrap();
    assert!(decoder.receive(at(1024), allocation, &[1.0; 150]).is_err());
    decoder.begin(at(1024 + 2 * CHIPS_PER_FRAME), 0).unwrap();
    assert!(DedicatedUplink::new(id(1), format, Rel99Tti::Ms80, &[150], limits()).is_err());
    assert!(
        DedicatedUplink::new(
            id(1),
            format,
            Rel99Tti::Ms20,
            &[150],
            TransportPlanLimits {
                maximum_mapping_entries: count(1),
                ..limits()
            }
        )
        .is_err()
    );
    let absent = BlockFormat {
        blocks: 0,
        ..format
    };
    let mut decoder = DedicatedUplink::new(id(1), absent, Rel99Tti::Ms10, &[0], limits()).unwrap();
    decoder.begin(at(1024), 0).unwrap();
    decoder
        .receive(
            at(1024),
            MatchedTransport {
                id: id(1),
                input_bits: 0,
                output_bits: 0,
            },
            &[],
        )
        .unwrap();
    assert_eq!(decoder.decode(&mut []), Ok(Some(BlockOutcome::Absent)));
}

#[test]
fn fixed_rate_matching_uses_the_tfs_maximum_not_the_current_block() {
    for (delta, expected) in [(3, vec![0, 0, 1, 2, 2]), (-3, vec![1])] {
        let matching = RateMatching::downlink_convolutional_fixed(3, 8, delta, count(100)).unwrap();
        let mut output = vec![0; matching.output_bits()];
        matching.apply(&[0, 1, 2], &mut output).unwrap();
        assert_eq!(output, expected);
    }
    let mut flexible = [0; 5];
    RateMatching::downlink_convolutional_flexible(3, 5)
        .unwrap()
        .apply(&[0, 1, 2], &mut flexible)
        .unwrap();
    assert_eq!(flexible, [0, 0, 1, 1, 2]);
    assert!(RateMatching::downlink_convolutional_fixed(3, 2, 1, count(10)).is_err());
}

#[test]
fn first_dtx_precedes_interleaving_and_80ms_dch_is_limited_to_sf512() {
    let format = BlockFormat {
        block_bits: 13,
        blocks: 1,
        crc: TransportCrc::Crc8,
        rate: CodeRate::Half,
    };
    let settings = FixedDownlinkSettings {
        blocks: format,
        interval: Rel99Tti::Ms20,
        maximum_coded_tti_bits: 78,
        reserved_frame_bits: 24,
        physical: DownlinkFormat::new(3, true).unwrap(),
    };
    let mut encoder = FixedDownlink::new(settings, limits()).unwrap();
    let mut frames = vec![TxBit::Dtx; encoder.tti_bits()];
    encoder.encode(&payload(13), &mut frames).unwrap();
    assert_eq!(
        frames
            .chunks_exact(24)
            .map(|frame| frame.iter().filter(|bit| **bit != TxBit::Dtx).count())
            .collect::<Vec<_>>(),
        [18, 17]
    );
    let settings = FixedDownlinkSettings {
        blocks: BlockFormat {
            blocks: 0,
            ..format
        },
        interval: Rel99Tti::Ms80,
        reserved_frame_bits: 30,
        ..settings
    };
    assert!(FixedDownlink::new(settings, limits()).is_err());
    let mut encoder = FixedDownlink::new(
        FixedDownlinkSettings {
            physical: DownlinkFormat::new(1, true).unwrap(),
            ..settings
        },
        limits(),
    )
    .unwrap();
    let mut frames = vec![TxBit::Data(Bit::One); encoder.tti_bits()];
    encoder.encode(&[], &mut frames).unwrap();
    assert!(frames.iter().all(|bit| *bit == TxBit::Dtx));
}

#[test]
fn shared_immutable_plans_keep_two_concurrent_receiver_histories_independent() {
    use std::{sync::Arc, thread};
    fn send_sync<T: Send + Sync>() {}
    send_sync::<UplinkTransportPlan>();
    let format = BlockFormat {
        block_bits: 40,
        blocks: 1,
        crc: TransportCrc::Crc16,
        rate: CodeRate::Third,
    };
    let plan = Arc::new(
        UplinkTransportPlan::new(id(1), format, Rel99Tti::Ms20, &[150], limits()).unwrap(),
    );
    let workers: Vec<_> = (0..2)
        .map(|worker| {
            let plan = plan.clone();
            thread::spawn(move || {
                let mut decoder = DedicatedUplink::from_plan(plan).unwrap();
                let expected: Vec<_> = (0..40)
                    .map(|index| Bit::from((index + worker) % 3 == 0))
                    .collect();
                let frames = coded_frames(format, Rel99Tti::Ms20, &expected);
                let width = frames.len() / 2;
                for trial in 0..20 {
                    let start = 1024 + worker as u64 * 128 + trial * 2 * CHIPS_PER_FRAME;
                    decoder.begin(at(start), (trial * 2) as u8).unwrap();
                    for (frame, input) in frames.chunks_exact(width).enumerate() {
                        let mut matched = vec![Bit::Zero; 150];
                        RateMatching::uplink_convolutional(width, 150, Rel99Tti::Ms20, frame)
                            .unwrap()
                            .apply(input, &mut matched)
                            .unwrap();
                        let soft: Vec<_> = matched
                            .iter()
                            .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 })
                            .collect();
                        decoder
                            .receive(
                                at(start + frame as u64 * CHIPS_PER_FRAME),
                                MatchedTransport {
                                    id: id(1),
                                    input_bits: width,
                                    output_bits: 150,
                                },
                                &soft,
                            )
                            .unwrap();
                    }
                    let mut output = vec![Bit::One; 40];
                    assert_eq!(decoder.decode(&mut output), Ok(Some(BlockOutcome::Decoded)));
                    assert_eq!(output, expected);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}
