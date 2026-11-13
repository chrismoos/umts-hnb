// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{hint::black_box, num::NonZeroUsize, time::Instant};
use umts_dsp::{
    bits::Bit,
    crc::TransportCrc,
    hsdpa::{CrcMethod, TransportEncoder},
};

const WARMUP: usize = 8;
const ROUNDS: usize = 120;

fn main() {
    for length in [137, 20251, 27952] {
        let input: Vec<_> = (0..length)
            .map(|i| Bit::from((i * 17 + i / 3) % 5 < 2))
            .collect();
        let mut encoder = TransportEncoder::new(
            NonZeroUsize::new(length).unwrap(),
            NonZeroUsize::new(100_000).unwrap(),
        )
        .unwrap();
        let mut output = vec![Bit::Zero; encoder.coded_bits()];
        for transport in [false, true] {
            let mut times = Vec::with_capacity(ROUNDS);
            for round in 0..WARMUP + ROUNDS {
                let began = Instant::now();
                if transport {
                    encoder
                        .encode(
                            black_box(&input),
                            CrcMethod::Method1,
                            true,
                            black_box(&mut output),
                        )
                        .unwrap();
                } else {
                    black_box(TransportCrc::Crc24.remainder(black_box(&input)));
                }
                if round >= WARMUP {
                    times.push(began.elapsed().as_secs_f64() * 1e6);
                }
            }
            times.sort_by(f64::total_cmp);
            println!(
                "bits={length} transport={transport} median_us={:.3} p95_us={:.3} max_us={:.3}",
                times[ROUNDS / 2],
                times[ROUNDS * 95 / 100],
                times[ROUNDS - 1]
            );
        }
    }
}
