// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{hint::black_box, time::Instant};
use umts_dsp::tfci::{Tfci, TfciDecoder};

const ROUNDS: usize = 120;
const WARMUP: usize = 8;

fn main() {
    let input = std::array::from_fn(|i| ((i * 17 % 101) as f32 - 50.0) * 0.125);
    for count in [8, 32, 128, 256, 512, 1024] {
        let allowed: Vec<_> = (0..count).map(|i| Tfci::new(i as u16).unwrap()).collect();
        let decoder = TfciDecoder::new(&allowed).unwrap();
        let mut times = Vec::with_capacity(ROUNDS);
        for round in 0..ROUNDS + WARMUP {
            let began = Instant::now();
            black_box(decoder.decode(black_box(&input)).unwrap());
            if round >= WARMUP {
                times.push(began.elapsed().as_secs_f64() * 1e6);
            }
        }
        times.sort_by(f64::total_cmp);
        println!(
            "candidates={count} median_us={:.3} p95_us={:.3} max_us={:.3}",
            times[ROUNDS / 2],
            times[ROUNDS * 95 / 100],
            times[ROUNDS - 1]
        );
    }
}
