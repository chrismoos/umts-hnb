// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use std::{hint::black_box, num::NonZeroU64, sync::Arc, time::Instant};
use umts_dsp::{
    carrier::{CarrierCorrector, CarrierOffsetHz},
    prach::{PREAMBLE_CHIPS, PrachCode, PreambleSignature},
    prach_search::{PreambleSearchPlan, PreambleTimingSearch},
};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_SECOND, CHIPS_PER_SLOT, ChipInstant, SampleInstant, SampleRate},
};

const WARMUP_WINDOWS: usize = 8;
const MEASURED_WINDOWS: usize = 120;

fn main() {
    println!("Isolated coherent PRACH search, synthetic input.");
    let start = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index: 0,
    };
    let code = PrachCode::new(7504).unwrap();
    let signature = PreambleSignature::new(13).unwrap();
    for (mask, delays) in [(1 << 13, 100), (u16::MAX, 100), (u16::MAX, 5120)] {
        let setup_start = Instant::now();
        let plan =
            Arc::new(PreambleSearchPlan::new(code, mask, 0..PREAMBLE_CHIPS, delays).unwrap());
        let mut receiver = PreambleTimingSearch::new(plan.clone());
        let setup_ms = setup_start.elapsed().as_secs_f64() * 1e3;
        let mut input = vec![Complex32::default(); plan.input_length()];
        code.write_preamble(
            signature,
            &mut input[delays - 1..delays - 1 + PREAMBLE_CHIPS],
        )
        .unwrap();
        let mut times = Vec::with_capacity(MEASURED_WINDOWS);
        for iteration in 0..WARMUP_WINDOWS + MEASURED_WINDOWS {
            let before = Instant::now();
            let window = receiver.search(start, black_box(&input)).unwrap();
            let best = black_box(window.strongest(signature).unwrap());
            let elapsed = before.elapsed().as_secs_f64();
            assert_eq!(best.correlation_start.index, (delays - 1) as u64);
            if iteration >= WARMUP_WINDOWS {
                times.push(elapsed);
            }
        }
        times.sort_by(f64::total_cmp);
        println!(
            "signatures={} delays={} setup_ms={:.2} input_rf_us={:.2} median_us={:.2} p95_us={:.2} max_us={:.2}",
            mask.count_ones(),
            delays,
            setup_ms,
            input.len() as f64 / CHIPS_PER_SECOND as f64 * 1e6,
            times[MEASURED_WINDOWS / 2] * 1e6,
            times[MEASURED_WINDOWS * 95 / 100] * 1e6,
            times[MEASURED_WINDOWS - 1] * 1e6
        );
    }
    for samples_per_chip in [2, 4, 8] {
        let rate = SampleRate::new(CHIPS_PER_SECOND * samples_per_chip, 1).unwrap();
        let mut corrector = CarrierCorrector::new(
            rate,
            CarrierOffsetHz::new(1377.0).unwrap(),
            0.0,
            SampleInstant {
                epoch: start.epoch,
                index: 0,
            },
        )
        .unwrap();
        let mut samples =
            vec![Complex32::new(0.7, -0.3); (CHIPS_PER_SLOT * samples_per_chip) as usize];
        let mut times = Vec::with_capacity(MEASURED_WINDOWS);
        for iteration in 0..WARMUP_WINDOWS + MEASURED_WINDOWS {
            samples.fill(Complex32::new(0.7, -0.3));
            let before = Instant::now();
            corrector
                .process(corrector.next_input(), black_box(&mut samples))
                .unwrap();
            let elapsed = before.elapsed().as_secs_f64();
            black_box(&samples);
            if iteration >= WARMUP_WINDOWS {
                times.push(elapsed);
            }
        }
        times.sort_by(f64::total_cmp);
        println!(
            "carrier_sps={} median_us={:.2} p95_us={:.2} max_us={:.2}",
            samples_per_chip,
            times[MEASURED_WINDOWS / 2] * 1e6,
            times[MEASURED_WINDOWS * 95 / 100] * 1e6,
            times[MEASURED_WINDOWS - 1] * 1e6
        );
    }
}
