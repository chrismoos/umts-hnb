// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

fn instant(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
        index,
    }
}

fn despread(chips: &[Complex32], start: u64, code: u8, amplitude: f32) -> Vec<Complex32> {
    let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    downlink_sequence(
        PrimaryScramblingCode::new(469).unwrap().sequence_number(),
        &mut scrambling,
    )
    .unwrap();
    let mut ovsf = [0.0; 256];
    OvsfCode::new(256, u16::from(code))
        .unwrap()
        .write(&mut ovsf)
        .unwrap();
    chips
        .chunks_exact(256)
        .enumerate()
        .map(|(symbol, values)| {
            values
                .iter()
                .enumerate()
                .map(|(chip, value)| {
                    value
                        * scrambling[((start + symbol as u64 * 256 + chip as u64) % CHIPS_PER_FRAME)
                            as usize]
                            .conj()
                        * ovsf[chip]
                })
                .sum::<Complex32>()
                / (256.0 * amplitude)
        })
        .collect()
}

#[test]
fn acquisition_signatures_match_every_table_twenty_two_row() {
    let rows = [
        "++++++++++++++++",
        "+-+-+-+-+-+-+-+-",
        "++--++--++--++--",
        "+--++--++--++--+",
        "++++----++++----",
        "+-+--+-++-+--+-+",
        "++----++++----++",
        "+--+-++-+--+-++-",
        "++++++++--------",
        "+-+-+-+--+-+-+-+",
        "++--++----++--++",
        "+--++--+-++--++-",
        "++++--------++++",
        "+-+--+-+-+-++-+-",
        "++----++--++++--",
        "+--+-++--++-+--+",
    ];
    let mut signals = Vec::new();
    for (signature, row) in rows.iter().enumerate() {
        assert_eq!(row.len(), 16);
        let mut indicators = AcquisitionIndicators::new(u16::MAX).unwrap();
        indicators
            .set(signature as u8, Acquisition::Positive)
            .unwrap();
        let expected: Vec<_> = row
            .bytes()
            .flat_map(|byte| [if byte == b'+' { 1 } else { -1 }; 2])
            .collect();
        assert_eq!(indicators.signals().as_slice(), expected);
        signals.push(indicators.signals());
        indicators
            .set(signature as u8, Acquisition::Negative)
            .unwrap();
        assert_eq!(indicators.signals(), signals[signature].map(|value| -value));
    }
    for (a, left) in signals.iter().enumerate() {
        for (b, right) in signals.iter().enumerate() {
            assert_eq!(
                left.iter()
                    .zip(right)
                    .map(|(left, right)| left * right)
                    .sum::<i16>(),
                if a == b { 32 } else { 0 }
            );
        }
    }
}

#[test]
fn acquisition_spreading_is_coherent_across_every_access_slot_and_frame_wrap() {
    let amplitude = 0.3;
    let waveform =
        AcquisitionDownlink::new(PrimaryScramblingCode::new(469).unwrap(), 2, amplitude).unwrap();
    for signature in 0..16 {
        let mut indicators = AcquisitionIndicators::new(u16::MAX).unwrap();
        indicators.set(signature, Acquisition::Positive).unwrap();
        let signals = indicators.signals();
        for slot in 0..15 {
            let start =
                instant(SYSTEM_FRAME_MODULUS * CHIPS_PER_FRAME + slot * ACCESS_SLOT_CHIPS as u64);
            let mut output = vec![Complex32::default(); ACCESS_SLOT_CHIPS];
            waveform
                .add_access_slot(start, &indicators, &mut output)
                .unwrap();
            assert!(
                output[ACQUISITION_ACTIVE_CHIPS..]
                    .iter()
                    .all(|chip| *chip == Complex32::default())
            );
            for chip in &output[..ACQUISITION_ACTIVE_CHIPS] {
                assert!((chip.norm_sqr() - amplitude * amplitude).abs() < 1e-6);
            }
            let decoded = despread(
                &output[..ACQUISITION_ACTIVE_CHIPS],
                start.index,
                2,
                amplitude,
            );
            for (value, pair) in decoded.iter().zip(signals.chunks_exact(2)) {
                assert!((value.re - f32::from(pair[0])).abs() < 1e-5);
                assert!((value.im - f32::from(pair[1])).abs() < 1e-5);
            }
        }
    }
}

#[test]
fn simultaneous_acquisition_indicators_keep_per_signature_power_and_float_headroom() {
    let waveform =
        AcquisitionDownlink::new(PrimaryScramblingCode::new(469).unwrap(), 2, 0.2).unwrap();
    let mut indicators = AcquisitionIndicators::new(u16::MAX).unwrap();
    for signature in 0..16 {
        indicators.set(signature, Acquisition::Positive).unwrap();
    }
    let mut output = vec![Complex32::default(); ACCESS_SLOT_CHIPS];
    waveform
        .add_access_slot(instant(0), &indicators, &mut output)
        .unwrap();
    let power = output[..ACQUISITION_ACTIVE_CHIPS]
        .iter()
        .map(|chip| chip.norm_sqr())
        .sum::<f32>()
        / ACQUISITION_ACTIVE_CHIPS as f32;
    assert!((power - 16.0 * 0.2 * 0.2).abs() < 1e-5);
    assert!(output.iter().any(|chip| chip.norm() > 3.19));
    let saved = output.clone();
    waveform
        .add_access_slot(
            instant(0),
            &AcquisitionIndicators::new(u16::MAX).unwrap(),
            &mut output,
        )
        .unwrap();
    assert_eq!(output, saved);
}

#[test]
fn paging_rotations_match_literal_sfn_boundary_cases() {
    for (sfn, rotations) in [
        (0, [0, 0, 0, 0]),
        (7, [15, 31, 63, 126]),
        (8, [2, 4, 9, 18]),
        (63, [13, 27, 54, 108]),
        (64, [2, 4, 9, 18]),
        (511, [11, 22, 45, 90]),
        (512, [2, 4, 9, 18]),
        (4095, [9, 18, 36, 72]),
    ] {
        for (mode, rotation) in [
            PagingCount::PerFrame18,
            PagingCount::PerFrame36,
            PagingCount::PerFrame72,
            PagingCount::PerFrame144,
        ]
        .into_iter()
        .zip(rotations)
        {
            assert_eq!(mode.position(0, sfn).unwrap(), rotation);
            assert_eq!(
                mode.position(mode.get() - 1, sfn).unwrap(),
                (rotation + mode.get() - 1) % mode.get()
            );
        }
    }
}

#[test]
fn paging_bit_repetition_and_reserved_tail_match_table_twenty_four() {
    for mode in [
        PagingCount::PerFrame18,
        PagingCount::PerFrame36,
        PagingCount::PerFrame72,
        PagingCount::PerFrame144,
    ] {
        let count = mode.get();
        let mut bits = [TxBit::Data(Bit::One); PAGING_FRAME_BITS];
        for indicator in 0..count {
            let mut input = vec![false; count];
            input[indicator] = true;
            mode.encode(0, &input, &mut bits).unwrap();
            for (index, bit) in bits[..PAGING_ACTIVE_BITS].iter().enumerate() {
                assert_eq!(
                    *bit,
                    TxBit::Data(Bit::from(index / (PAGING_ACTIVE_BITS / count) == indicator))
                );
            }
            assert!(
                bits[PAGING_ACTIVE_BITS..]
                    .iter()
                    .all(|bit| *bit == TxBit::Dtx)
            );
        }
    }
}

#[test]
fn paging_uses_the_shifted_frame_sfn_and_unshifted_scrambling_clock() {
    let pch = instant(64 * CHIPS_PER_FRAME + 19 * 256);
    let start = paging_start(pch).unwrap();
    assert_eq!(pch.index - start.index, CHIPS_PER_FRAME + 7680);
    assert_eq!(start.system_frame(), 62);
    assert_eq!(start.epoch, pch.epoch);
    for mode in [
        PagingCount::PerFrame18,
        PagingCount::PerFrame36,
        PagingCount::PerFrame72,
        PagingCount::PerFrame144,
    ] {
        let waveform =
            PagingDownlink::new(PrimaryScramblingCode::new(469).unwrap(), 7, mode, 0.25).unwrap();
        let mut input = vec![false; mode.get()];
        input[0] = true;
        let mut output = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        waveform.add_frame(start, &input, &mut output).unwrap();
        assert!(
            output[PAGING_ACTIVE_BITS / 2 * 256..]
                .iter()
                .all(|chip| *chip == Complex32::default())
        );
        let mut bits = [TxBit::Dtx; PAGING_FRAME_BITS];
        mode.encode(62, &input, &mut bits).unwrap();
        let decoded = despread(
            &output[..PAGING_ACTIVE_BITS / 2 * 256],
            start.index,
            7,
            0.25,
        );
        for (value, pair) in decoded.iter().zip(bits.chunks_exact(2)) {
            assert!((value.re - pair[0].amplitude()).abs() < 1e-5);
            assert!((value.im - pair[1].amplitude()).abs() < 1e-5);
        }
    }
}

#[test]
fn invalid_indicator_inputs_and_timing_leave_output_unchanged() {
    assert!(AcquisitionIndicators::new(0).is_err());
    let mut indicators = AcquisitionIndicators::new(1 << 13).unwrap();
    assert!(indicators.set(12, Acquisition::Positive).is_err());
    assert!(indicators.set(16, Acquisition::Positive).is_err());
    assert_eq!(indicators.signals(), [0; 32]);
    let acquisition =
        AcquisitionDownlink::new(PrimaryScramblingCode::new(469).unwrap(), 2, 0.25).unwrap();
    let mut output = vec![Complex32::new(0.1, -0.2); ACCESS_SLOT_CHIPS];
    let saved = output.clone();
    for index in [1, u64::MAX / 5120 * 5120] {
        assert!(
            acquisition
                .add_access_slot(instant(index), &indicators, &mut output)
                .is_err()
        );
        assert_eq!(output, saved);
    }
    let paging = PagingDownlink::new(
        PrimaryScramblingCode::new(469).unwrap(),
        7,
        PagingCount::PerFrame18,
        0.25,
    )
    .unwrap();
    let mut output = vec![Complex32::new(0.1, -0.2); CHIPS_PER_FRAME as usize];
    let saved = output.clone();
    for index in [1, u64::MAX / 256 * 256] {
        assert!(
            paging
                .add_frame(instant(index), &[false; 18], &mut output)
                .is_err()
        );
        assert_eq!(output, saved);
    }
    assert!(
        paging
            .add_frame(instant(0), &[false; 17], &mut output)
            .is_err()
    );
    assert_eq!(output, saved);
    assert!(paging_start(instant(0)).is_err());
    assert!(paging_start(instant(7681)).is_err());
    let mut bits = [TxBit::Dtx; PAGING_FRAME_BITS];
    assert!(
        PagingCount::PerFrame18
            .encode(4096, &[false; 18], &mut bits)
            .is_err()
    );
    assert_eq!(bits, [TxBit::Dtx; PAGING_FRAME_BITS]);
}

#[test]
fn shifted_indicators_coexist_with_common_channels_without_overwriting_them() {
    use crate::{
        broadcast::FRAME_BITS,
        common_downlink::{CommonChannelAmplitudes, CommonDownlink},
    };
    let primary = PrimaryScramblingCode::new(469).unwrap();
    let common = CommonDownlink::new(
        primary,
        CommonChannelAmplitudes {
            cpich: 0.1,
            pccpch: 0.1,
            primary_sch: 0.0,
            secondary_sch: 0.0,
        },
    )
    .unwrap();
    let mut output = vec![Complex32::default(); 2 * CHIPS_PER_FRAME as usize];
    for frame in output.chunks_exact_mut(CHIPS_PER_FRAME as usize) {
        common
            .compose_frame(&[Bit::Zero; FRAME_BITS], frame)
            .unwrap();
    }
    let paging = PagingDownlink::new(primary, 7, PagingCount::PerFrame18, 0.12).unwrap();
    let pich_start = paging_start(instant(2 * CHIPS_PER_FRAME)).unwrap();
    let first = pich_start.index as usize;
    let last = first + CHIPS_PER_FRAME as usize;
    let mut pages = [false; 18];
    pages[7] = true;
    paging
        .add_frame(pich_start, &pages, &mut output[first..last])
        .unwrap();
    let mut indicators = AcquisitionIndicators::new((1 << 13) | (1 << 3)).unwrap();
    indicators.set(13, Acquisition::Positive).unwrap();
    indicators.set(3, Acquisition::Negative).unwrap();
    let acquisition = AcquisitionDownlink::new(primary, 2, 0.2).unwrap();
    let access_start = 7 * ACCESS_SLOT_CHIPS;
    let access_end = access_start + ACCESS_SLOT_CHIPS;
    let saved = output.clone();
    acquisition
        .add_access_slot(
            instant(access_start as u64),
            &indicators,
            &mut output[access_start..access_end],
        )
        .unwrap();
    assert_eq!(
        &output[access_start + ACQUISITION_ACTIVE_CHIPS..access_end],
        &saved[access_start + ACQUISITION_ACTIVE_CHIPS..access_end]
    );
    for (value, pair) in despread(
        &output[access_start..access_start + ACQUISITION_ACTIVE_CHIPS],
        access_start as u64,
        2,
        0.2,
    )
    .iter()
    .zip(indicators.signals().chunks_exact(2))
    {
        assert!((value.re - f32::from(pair[0])).abs() < 1e-4);
        assert!((value.im - f32::from(pair[1])).abs() < 1e-4);
    }
    let mut bits = [TxBit::Dtx; PAGING_FRAME_BITS];
    PagingCount::PerFrame18
        .encode(pich_start.system_frame(), &pages, &mut bits)
        .unwrap();
    for (value, pair) in despread(
        &output[first..first + PAGING_ACTIVE_BITS / 2 * 256],
        pich_start.index,
        7,
        0.12,
    )
    .iter()
    .zip(bits.chunks_exact(2))
    {
        assert!((value.re - pair[0].amplitude()).abs() < 1e-4);
        assert!((value.im - pair[1].amplitude()).abs() < 1e-4);
    }
}
