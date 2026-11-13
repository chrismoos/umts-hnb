// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex64;
use std::{hint::black_box, num::NonZeroU64, time::Instant};
use umts_dsp::carrier::{CarrierCorrector, CarrierOffsetHz};
use umts_types::{
    RadioEpoch,
    time::{SampleInstant, SampleRate},
};

fn main() {
    for count in [2560, 5120, 10240] {
        for hz in [0.0, 1734.5, -3210.75] {
            let mut corrector = CarrierCorrector::new(
                SampleRate::new(7_680_000, 1).unwrap(),
                CarrierOffsetHz::new(hz).unwrap(),
                0.7,
                SampleInstant {
                    epoch: RadioEpoch::new(NonZeroU64::MIN),
                    index: 0,
                },
            )
            .unwrap();
            let mut input: Vec<_> = (0..count)
                .map(|i| Complex64::new((i % 17) as f64 / 17.0, (i % 13) as f64 / 13.0))
                .collect();
            let mut times = Vec::with_capacity(120);
            for iteration in 0..128 {
                let start = Instant::now();
                corrector
                    .process64(corrector.next_input(), black_box(&mut input))
                    .unwrap();
                if iteration >= 8 {
                    times.push(start.elapsed().as_secs_f64() * 1e6);
                }
            }
            times.sort_by(f64::total_cmp);
            println!(
                "samples={count} hz={hz} median_us={:.3} p95_us={:.3} max_us={:.3}",
                times[60], times[114], times[119]
            );
        }
    }
}
