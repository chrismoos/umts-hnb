// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{hint::black_box, time::Instant};
use umts_security::{Key, algorithms::Uea1Key};
use umts_types::Direction;

const REPETITIONS: u32 = 1000;
const ROUNDS: usize = 9;

fn main() {
    let key = Uea1Key::new(&Key::new([0x42; 16]));
    for bits in [120_usize, 320, 1272, 8192, 20_000] {
        let mut bytes = vec![0x5a; bits.div_ceil(8)];
        let mut times = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let started = Instant::now();
            for count in 0..REPETITIONS {
                key.apply(
                    black_box(count),
                    3,
                    Direction::Downlink,
                    black_box(&mut bytes),
                    black_box(bits),
                )
                .unwrap();
            }
            black_box(&bytes);
            times.push(started.elapsed().as_secs_f64() / f64::from(REPETITIONS));
        }
        times.sort_by(f64::total_cmp);
        println!("bits={bits} median_us={:.3}", times[ROUNDS / 2] * 1e6);
    }
}
