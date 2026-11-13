// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
#[cfg(not(target_feature = "avx2"))]
use wide::f32x4;
use wide::f32x8;

pub(super) enum PhaseTaps {
    #[cfg(not(target_feature = "avx2"))]
    Two(Vec<f32x4>),
    #[cfg(target_feature = "avx2")]
    TwoWide(Vec<f32x8>),
    Four(Vec<f32x8>),
    Eight(Vec<[f32x8; 2]>),
}

fn row<const N: usize>(phases: &[Vec<f32>], tap: usize, first: usize) -> [f32; N] {
    std::array::from_fn(|lane| phases[first + lane / 2][tap])
}

fn write<const N: usize>(lanes: [f32; N], samples: &mut [Complex32]) {
    for (sample, pair) in samples.iter_mut().zip(lanes.chunks_exact(2)) {
        *sample = Complex32::new(pair[0], pair[1]);
    }
}

fn synthesize_block(
    history: &[Complex32],
    output: &mut [Complex32],
    samples_per_chip: usize,
    tap_count: usize,
    mut synthesize: impl FnMut(&[Complex32], &mut [Complex32]),
) {
    let count = output.len() / samples_per_chip;
    for (index, samples) in output.chunks_exact_mut(samples_per_chip).enumerate() {
        let start = count - 1 - index;
        synthesize(&history[start..start + tap_count], samples);
    }
}

#[cfg(target_feature = "avx2")]
fn synthesize_two(history: &[Complex32], samples: &mut [Complex32], taps: &[f32x8]) {
    let chips = samples.len() / 2;
    let quad_samples = chips / 4 * 8;
    let (quads, samples) = samples.split_at_mut(quad_samples);
    for (index, output) in quads.chunks_exact_mut(8).enumerate() {
        let first = chips - 1 - 4 * index;
        let a = &history[first..first + taps.len()];
        let b = &history[first - 1..first - 1 + taps.len()];
        let c = &history[first - 2..first - 2 + taps.len()];
        let d = &history[first - 3..first - 3 + taps.len()];
        let mut sums = [f32x8::ZERO; 2];
        for ((((&a, &b), &c), &d), &tap) in a.iter().zip(b).zip(c).zip(d).zip(taps) {
            sums[0] += f32x8::new([a.re, a.im, a.re, a.im, b.re, b.im, b.re, b.im]) * tap;
            sums[1] += f32x8::new([c.re, c.im, c.re, c.im, d.re, d.im, d.re, d.im]) * tap;
        }
        let (first, second) = output.split_at_mut(4);
        write(sums[0].to_array(), first);
        write(sums[1].to_array(), second);
    }
    let chips = samples.len() / 2;
    let paired_samples = chips / 2 * 4;
    let (pairs, remainder) = samples.split_at_mut(paired_samples);
    for (index, output) in pairs.chunks_exact_mut(4).enumerate() {
        let first = chips - 1 - 2 * index;
        let second = first - 1;
        let first = &history[first..first + taps.len()];
        let second = &history[second..second + taps.len()];
        let mut sum = f32x8::ZERO;
        for ((&first, &second), &tap) in first.iter().zip(second).zip(taps) {
            sum += f32x8::new([
                first.re, first.im, first.re, first.im, second.re, second.im, second.re, second.im,
            ]) * tap;
        }
        write(sum.to_array(), output);
    }
    if !remainder.is_empty() {
        let mut sum = f32x8::ZERO;
        for (&value, &tap) in history[..taps.len()].iter().zip(taps) {
            sum += f32x8::new([
                value.re, value.im, value.re, value.im, value.re, value.im, value.re, value.im,
            ]) * tap;
        }
        write(sum.to_array(), remainder);
    }
}

impl PhaseTaps {
    pub fn new(phases: &[Vec<f32>]) -> Self {
        let length = phases[0].len();
        match phases.len() {
            2 => {
                #[cfg(target_feature = "avx2")]
                {
                    Self::TwoWide(
                        (0..length)
                            .map(|tap| {
                                f32x8::new(std::array::from_fn(|lane| phases[(lane / 2) % 2][tap]))
                            })
                            .collect(),
                    )
                }
                #[cfg(not(target_feature = "avx2"))]
                {
                    Self::Two(
                        (0..length)
                            .map(|tap| f32x4::new(row(phases, tap, 0)))
                            .collect(),
                    )
                }
            }
            4 => Self::Four(
                (0..length)
                    .map(|tap| f32x8::new(row(phases, tap, 0)))
                    .collect(),
            ),
            8 => Self::Eight(
                (0..length)
                    .map(|tap| {
                        [
                            f32x8::new(row(phases, tap, 0)),
                            f32x8::new(row(phases, tap, 4)),
                        ]
                    })
                    .collect(),
            ),
            _ => unreachable!("validated samples per chip"),
        }
    }

    pub fn synthesize_block(&self, history: &[Complex32], samples: &mut [Complex32]) {
        // Every lane retains its sample's sequential tap accumulation order.
        match self {
            #[cfg(target_feature = "avx2")]
            Self::TwoWide(taps) => synthesize_two(history, samples, taps),
            #[cfg(not(target_feature = "avx2"))]
            Self::Two(taps) => {
                synthesize_block(history, samples, 2, taps.len(), |history, samples| {
                    let mut sum = f32x4::ZERO;
                    for (&value, &tap) in history.iter().zip(taps) {
                        sum += f32x4::new([value.re, value.im, value.re, value.im]) * tap;
                    }
                    write(sum.to_array(), samples);
                })
            }
            Self::Four(taps) => {
                synthesize_block(history, samples, 4, taps.len(), |history, samples| {
                    let mut sum = f32x8::ZERO;
                    for (&value, &tap) in history.iter().zip(taps) {
                        sum += f32x8::new([
                            value.re, value.im, value.re, value.im, value.re, value.im, value.re,
                            value.im,
                        ]) * tap;
                    }
                    write(sum.to_array(), samples);
                })
            }
            Self::Eight(taps) => {
                synthesize_block(history, samples, 8, taps.len(), |history, samples| {
                    let mut sums = [f32x8::ZERO; 2];
                    for (&value, &taps) in history.iter().zip(taps) {
                        let value = f32x8::new([
                            value.re, value.im, value.re, value.im, value.re, value.im, value.re,
                            value.im,
                        ]);
                        sums[0] += value * taps[0];
                        sums[1] += value * taps[1];
                    }
                    let (first, second) = samples.split_at_mut(4);
                    write(sums[0].to_array(), first);
                    write(sums[1].to_array(), second);
                })
            }
        }
    }
}
