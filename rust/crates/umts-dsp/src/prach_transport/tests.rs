// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(crate) fn reference(
    format: PrachFormat,
    payload: &[Bit],
    sf: u16,
    bad_crc: bool,
    bad_filler: bool,
) -> Vec<f32> {
    let crc_bits = format.crc.width();
    let polynomial = match format.crc {
        TransportCrc::None => 1,
        TransportCrc::Crc8 => 0x19b,
        TransportCrc::Crc12 => 0x180f,
        TransportCrc::Crc16 => 0x11021,
        TransportCrc::Crc24 => 0x1800063,
    };
    let mut joined = Vec::new();
    for (number, payload) in payload.chunks_exact(format.block_bits).enumerate() {
        let mut dividend: Vec<_> = payload.iter().map(|bit| *bit as u8).collect();
        dividend.resize(payload.len() + crc_bits, 0);
        for index in 0..payload.len() {
            if dividend[index] != 0 {
                for offset in 0..=crc_bits {
                    dividend[index + offset] ^= ((polynomial >> (crc_bits - offset)) & 1) as u8;
                }
            }
        }
        joined.extend_from_slice(payload);
        let mut parity: Vec<_> = dividend[payload.len()..]
            .iter()
            .rev()
            .map(|bit| Bit::from(*bit != 0))
            .collect();
        if bad_crc && number == 0 {
            parity[0] = Bit::from(parity[0] == Bit::Zero);
        }
        joined.extend(parity);
    }
    let count = joined.len().div_ceil(504);
    let width = joined.len().div_ceil(count);
    let filler = count * width - joined.len();
    let mut blocks = vec![Bit::Zero; filler];
    blocks.extend(joined);
    if bad_filler {
        assert!(filler > 0);
        blocks[0] = Bit::One;
    }
    let generators: &[u16] = match format.rate {
        CodeRate::Half => &[0o561, 0o753],
        CodeRate::Third => &[0o557, 0o663, 0o711],
    };
    let mut coded = Vec::new();
    for block in blocks.chunks_exact(width) {
        for time in 0..block.len() + 8 {
            for generator in generators {
                let parity = (0..=8)
                    .filter(|delay| {
                        time >= *delay
                            && time - *delay < block.len()
                            && generator & (1 << (8 - delay)) != 0
                    })
                    .fold(false, |parity, delay| {
                        parity ^ (block[time - delay] == Bit::One)
                    });
                coded.push(Bit::from(parity));
            }
        }
    }
    let frames = format.interval.frames();
    let input_bits = coded.len().div_ceil(frames);
    coded.resize(input_bits * frames, Bit::One);
    let mut first = Vec::new();
    for column in 0..frames {
        first.extend(coded.iter().skip(column).step_by(frames).copied());
    }
    let output_bits = 38400 / usize::from(sf);
    let mut output = Vec::new();
    for (frame, source) in first.chunks_exact(input_bits).enumerate() {
        let delta = output_bits as i64 - input_bits as i64;
        let remainder = delta.rem_euclid(input_bits as i64);
        let q = if remainder != 0 && 2 * remainder <= input_bits as i64 {
            (input_bits as f64 / remainder as f64).ceil() as i64
        } else {
            (input_bits as f64 / (remainder - input_bits as i64) as f64).ceil() as i64
        };
        let q = if q % 2 == 0 { q + 1 } else { q };
        let phase = if frames == 2 && frame == 1 {
            q.unsigned_abs() / 2
        } else {
            0
        };
        let mut error = ((2 * phase * delta.unsigned_abs() + 1) % (2 * input_bits as u64)) as i64;
        let mut matched = Vec::new();
        for bit in source {
            error -= 2 * delta.abs();
            if delta < 0 {
                if error <= 0 {
                    error += 2 * input_bits as i64;
                } else {
                    matched.push(*bit);
                }
            } else {
                matched.push(*bit);
                while error <= 0 {
                    matched.push(*bit);
                    error += 2 * input_bits as i64;
                }
            }
        }
        assert_eq!(matched.len(), output_bits);
        let rows = output_bits.div_ceil(30);
        for column in [
            0, 20, 10, 5, 15, 25, 3, 13, 23, 8, 18, 28, 1, 11, 21, 6, 16, 26, 4, 14, 24, 19, 9, 29,
            12, 2, 7, 22, 27, 17,
        ] {
            for row in 0..rows {
                let index = row * 30 + column;
                if index < matched.len() {
                    output.push(if matched[index] == Bit::Zero {
                        4.0
                    } else {
                        -4.0
                    });
                }
            }
        }
    }
    output
}

#[test]
fn prach_convolutional_profiles_decode_independent_transport_vectors() {
    for interval in [Rel99Tti::Ms10, Rel99Tti::Ms20] {
        for rate in [CodeRate::Half, CodeRate::Third] {
            for crc in [
                TransportCrc::None,
                TransportCrc::Crc8,
                TransportCrc::Crc12,
                TransportCrc::Crc16,
                TransportCrc::Crc24,
            ] {
                for (block_bits, blocks) in [(48, 1), (256, 1), (256, 2), (256, 4)] {
                    for puncturing in [40, 100] {
                        let format = PrachFormat {
                            block_bits,
                            blocks,
                            crc,
                            rate,
                            interval,
                        };
                        let joined = (block_bits + crc.width()) * blocks;
                        let count = joined.div_ceil(504);
                        let coded = count * (joined.div_ceil(count) + 8) * rate.denominator();
                        let raw_frame = coded.div_ceil(interval.frames());
                        let result = PrachTransportPlan::new(format, 32, puncturing);
                        if raw_frame * usize::from(puncturing) > 1200 * 100 {
                            assert!(matches!(result, Err(DspError::Capacity)));
                            continue;
                        }
                        let plan = result.unwrap();
                        let sf = [256, 128, 64, 32]
                            .into_iter()
                            .find(|sf| 38400 / usize::from(*sf) >= raw_frame)
                            .unwrap_or(32);
                        assert_eq!(plan.spreading_factor(), sf);
                        assert_eq!(plan.coded_bits(), coded);
                        assert_eq!(plan.frames(), interval.frames());
                        let payload: Vec<_> = (0..block_bits * blocks)
                            .map(|i| Bit::from((i * 7 + i / 5) % 13 < 6))
                            .collect();
                        let input = reference(format, &payload, sf, false, false);
                        let mut decoder = PrachTransportDecoder::new(plan);
                        let mut actual = vec![Bit::Zero; payload.len()];
                        assert!(
                            decoder.decode(&input, &mut actual).unwrap(),
                            "bits {block_bits}, blocks {blocks}, rate {rate:?}, crc {crc:?}, interval {interval:?}, PL {puncturing}"
                        );
                        assert_eq!(actual, payload);
                        assert!(decoder.decode(&input, &mut actual).unwrap());
                        assert_eq!(actual, payload);
                    }
                }
            }
        }
    }
}

#[test]
fn invalid_crc_or_filler_never_publishes_partial_transport_blocks() {
    let format = PrachFormat {
        block_bits: 256,
        blocks: 4,
        crc: TransportCrc::Crc16,
        rate: CodeRate::Third,
        interval: Rel99Tti::Ms20,
    };
    let payload = vec![Bit::Zero; 1024];
    let plan = PrachTransportPlan::new(format, 32, 40).unwrap();
    let sf = plan.spreading_factor();
    let mut decoder = PrachTransportDecoder::new(plan);
    for (bad_crc, bad_filler) in [(true, false), (false, true)] {
        let input = reference(format, &payload, sf, bad_crc, bad_filler);
        let mut output = vec![Bit::One; 1024];
        assert!(!decoder.decode(&input, &mut output).unwrap());
        assert!(output.iter().all(|bit| *bit == Bit::One));
    }
    let mut input = reference(format, &payload, sf, false, false);
    input[1] = f32::NAN;
    let mut output = vec![Bit::One; 1024];
    assert!(decoder.decode(&input, &mut output).is_err());
    assert!(output.iter().all(|bit| *bit == Bit::One));
    let valid = reference(format, &payload, sf, false, false);
    assert!(decoder.decode(&valid, &mut output).unwrap());
    assert_eq!(output, payload);
    output.fill(Bit::One);
    assert!(
        !decoder
            .decode(&vec![0.0; valid.len()], &mut output)
            .unwrap()
    );
    assert!(output.iter().all(|bit| *bit == Bit::One));
}

#[test]
fn spreading_selection_respects_minimum_sf_and_puncturing_limits() {
    let format = PrachFormat {
        block_bits: 256,
        blocks: 1,
        crc: TransportCrc::Crc16,
        rate: CodeRate::Half,
        interval: Rel99Tti::Ms10,
    };
    let plan = PrachTransportPlan::new(format, 32, 100).unwrap();
    assert_eq!(
        (
            plan.spreading_factor(),
            plan.frame_bits(),
            plan.coded_bits()
        ),
        (64, 600, 560)
    );
    let plan = PrachTransportPlan::new(
        PrachFormat {
            blocks: 2,
            ..format
        },
        32,
        100,
    )
    .unwrap();
    assert_eq!(
        (
            plan.spreading_factor(),
            plan.frame_bits(),
            plan.coded_bits()
        ),
        (32, 1200, 1120)
    );
    assert!(matches!(
        PrachTransportPlan::new(format, 128, 100),
        Err(DspError::Capacity)
    ));
    assert_eq!(
        PrachTransportPlan::new(format, 128, 40)
            .unwrap()
            .spreading_factor(),
        128
    );
    for minimum in [0, 16, 63, 512] {
        assert!(PrachTransportPlan::new(format, minimum, 100).is_err());
    }
    for percent in [0, 39, 41, 101] {
        assert!(PrachTransportPlan::new(format, 32, percent).is_err());
    }
    for invalid in [
        PrachFormat {
            blocks: 0,
            ..format
        },
        PrachFormat {
            blocks: usize::MAX,
            ..format
        },
        PrachFormat {
            block_bits: usize::MAX,
            ..format
        },
        PrachFormat {
            interval: Rel99Tti::Ms40,
            ..format
        },
    ] {
        assert!(PrachTransportPlan::new(invalid, 32, 100).is_err());
    }
}
