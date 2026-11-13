// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use std::{hint::black_box, num::NonZeroU64, time::Instant};
use umts_dsp::{pulse_shaping::SamplesPerChip, receive_filter::ReceiveFilter};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_SECOND, CHIPS_PER_SLOT, SampleInstant},
};

const WARMUP_SLOTS: usize = 8;
const MEASURED_SLOTS: usize = 120;

fn main() {
    println!("Isolated receive filter, synthetic input.");
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for span in [16, 32] {
            for full_rate in [false, true] {
                let origin = SampleInstant {
                    epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
                    index: 0,
                };
                let mut filter = if full_rate {
                    ReceiveFilter::full_rate(rate, span, origin).unwrap()
                } else {
                    ReceiveFilter::new(rate, span, 0, origin).unwrap()
                };
                let input: Vec<_> = (0..CHIPS_PER_SLOT as usize * rate.get())
                    .map(|i| Complex32::new((i % 17) as f32 / 17.0, (i % 13) as f32 / 13.0))
                    .collect();
                let mut output = vec![Complex32::default(); input.len()];
                let mut times = Vec::with_capacity(MEASURED_SLOTS);
                for iteration in 0..WARMUP_SLOTS + MEASURED_SLOTS {
                    let count = filter.output_length(input.len()).unwrap();
                    let start = Instant::now();
                    black_box(
                        filter
                            .process(
                                filter.next_input(),
                                black_box(&input),
                                black_box(&mut output[..count]),
                            )
                            .unwrap(),
                    );
                    let elapsed = start.elapsed().as_secs_f64();
                    black_box(&output[..count]);
                    if iteration >= WARMUP_SLOTS {
                        times.push(elapsed);
                    }
                }
                times.sort_by(f64::total_cmp);
                let rf_seconds = CHIPS_PER_SLOT as f64 / CHIPS_PER_SECOND as f64;
                println!(
                    "sps={} span={} full_rate={} slots={} median_us={:.2} p95_us={:.2} max_us={:.2} worst_headroom={:.2}x",
                    rate.get(),
                    span,
                    full_rate,
                    MEASURED_SLOTS,
                    times[MEASURED_SLOTS / 2] * 1e6,
                    times[MEASURED_SLOTS * 95 / 100] * 1e6,
                    times[MEASURED_SLOTS - 1] * 1e6,
                    rf_seconds / times[MEASURED_SLOTS - 1]
                );
            }
        }
    }
}
