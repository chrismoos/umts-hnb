// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use std::{hint::black_box, num::NonZeroU64, time::Instant};
use umts_dsp::{
    fractional_sampling::{FractionalSampler, SamplePosition},
    pulse_shaping::SamplesPerChip,
};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_SLOT, SampleInstant},
};

fn main() {
    let sampler = FractionalSampler::new();
    let origin = SampleInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index: 0,
    };
    let count = CHIPS_PER_SLOT as usize;
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        let input: Vec<_> = (0..count * rate.get() + sampler.guard_samples() * 2)
            .map(|i| Complex32::new((i % 17) as f32 / 17.0, (i % 13) as f32 / 13.0))
            .collect();
        for fraction in [0.0, 0.123, 0.5, 0.999] {
            let position = SamplePosition::new(
                SampleInstant {
                    index: sampler.guard_samples() as u64,
                    ..origin
                },
                fraction,
            )
            .unwrap();
            let mut output = vec![Complex32::default(); count];
            let mut times = Vec::with_capacity(120);
            for iteration in 0..128 {
                let start = Instant::now();
                black_box(
                    sampler
                        .write_chips(
                            origin,
                            black_box(&input),
                            position,
                            rate,
                            black_box(&mut output),
                        )
                        .unwrap(),
                );
                if iteration >= 8 {
                    times.push(start.elapsed().as_secs_f64() * 1e6);
                }
            }
            times.sort_by(f64::total_cmp);
            println!(
                "sps={} fraction={} median_us={:.2} p95_us={:.2} max_us={:.2}",
                rate.get(),
                fraction,
                times[60],
                times[114],
                times[119]
            );
        }
    }
}
