// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{hint::black_box, num::NonZeroU8, sync::Arc, time::Instant};
use umts_dsp::{
    bits::Bit,
    turbo::{TurboDecoder, TurboInterleaver, coded_bits},
};

const WARMUP: usize = 4;
const ROUNDS: usize = 40;

fn main() {
    for length in [530, 5114] {
        for iterations in [1, 4, 8] {
            let plan = Arc::new(TurboInterleaver::new(length).unwrap());
            let mut decoder = TurboDecoder::new(plan, NonZeroU8::new(iterations).unwrap()).unwrap();
            let input: Vec<_> = (0..coded_bits(length).unwrap())
                .map(|i| ((i * 17 % 101) as f32 - 50.0) * 0.125)
                .collect();
            let mut output = vec![Bit::Zero; length];
            let mut times = Vec::with_capacity(ROUNDS);
            for round in 0..WARMUP + ROUNDS {
                let began = Instant::now();
                decoder
                    .decode(black_box(&input), black_box(&mut output))
                    .unwrap();
                if round >= WARMUP {
                    times.push(began.elapsed().as_secs_f64() * 1e6);
                }
            }
            times.sort_by(f64::total_cmp);
            println!(
                "bits={length} iterations={iterations} median_us={:.2} p95_us={:.2} max_us={:.2}",
                times[ROUNDS / 2],
                times[ROUNDS * 95 / 100],
                times[ROUNDS - 1]
            );
        }
    }
}
