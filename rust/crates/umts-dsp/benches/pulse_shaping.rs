// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use std::{hint::black_box, time::Instant};
use umts_dsp::pulse_shaping::{PulseShaper, SamplesPerChip};

const WARMUP: usize = 8;
const ROUNDS: usize = 120;

fn main() {
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for count in [2560, 2561] {
            let mut shaper = PulseShaper::new(rate, 32).unwrap();
            let input: Vec<_> = (0..count)
                .map(|index| {
                    Complex32::new(
                        (index as f32 * 0.067).sin() * 0.2,
                        (index as f32 * 0.113).cos() * 0.1,
                    )
                })
                .collect();
            let mut output = vec![Complex32::default(); shaper.output_length(count).unwrap()];
            let mut times = Vec::with_capacity(ROUNDS);
            for iteration in 0..WARMUP + ROUNDS {
                let started = Instant::now();
                shaper
                    .process(black_box(&input), black_box(&mut output))
                    .unwrap();
                let elapsed = started.elapsed().as_secs_f64() * 1e6;
                black_box(&output);
                if iteration >= WARMUP {
                    times.push(elapsed);
                }
            }
            times.sort_by(f64::total_cmp);
            let fingerprint = output.iter().fold(0u64, |value, sample| {
                value.rotate_left(1)
                    ^ (u64::from(sample.re.to_bits()) << u32::BITS)
                    ^ u64::from(sample.im.to_bits())
            });
            println!(
                "sps={} chips={count} median_us={:.2} p95_us={:.2} max_us={:.2} fingerprint={fingerprint:016x}",
                rate.get(),
                times[ROUNDS / 2],
                times[ROUNDS * 95 / 100],
                times[ROUNDS - 1]
            );
        }
    }
}
