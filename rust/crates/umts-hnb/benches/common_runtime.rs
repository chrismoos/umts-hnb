// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use std::{
    hint::black_box,
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    time::Instant,
};
use umts_config::Config;
use umts_dsp::{
    bits::Bit, common_downlink::CommonChannelAmplitudes, indicators::PAGING_LEAD_CHIPS,
    pulse_shaping::SamplesPerChip,
};
use umts_hnb::{
    cell::{CellPlan, ReferencePowers},
    common_tx::{CommonAmplitudes, CommonTx},
};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant},
};

const WARMUP_SLOTS: usize = 30;
const MEASURED_SLOTS: usize = 150;
const PAGING_TRANSPORT_LEAD_FRAMES: u64 = PAGING_LEAD_CHIPS.div_ceil(CHIPS_PER_FRAME);

fn main() {
    println!(
        "Connected common TX synthesis, synthetic transport load, no radio or live RT qualification."
    );
    let mut config = Config::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../rust/test-support/fixtures/config/serving.yaml"),
    )
    .unwrap();
    for compact in [false, true] {
        config.hspa.downlink.compact_common_channels = compact;
        let epoch = RadioEpoch::new(NonZeroU64::MIN);
        let before = Instant::now();
        let plan = CellPlan::compile(
            &config,
            epoch,
            1,
            ReferencePowers {
                cpich_tx_dbm: 10,
                uplink_interference_dbm: -100,
            },
        )
        .unwrap();
        let width = usize::from(plan.information().common_layout().transport_block_bits());
        let gains = CommonAmplitudes {
            primary: CommonChannelAmplitudes {
                cpich: 0.12,
                pccpch: 0.12,
                primary_sch: 0.04,
                secondary_sch: 0.04,
            },
            fach: 0.16,
            pch: 0.16,
        };
        let mut source = CommonTx::new(
            plan,
            ChipInstant { epoch, index: 0 },
            SamplesPerChip::Two,
            32,
            gains,
            NonZeroUsize::new(8).unwrap(),
            100,
        )
        .unwrap();
        let setup_ms = before.elapsed().as_secs_f64() * 1e3;
        let payload: Vec<_> = (0..width).map(|i| Bit::from(i % 7 < 3)).collect();
        let mut output = vec![Complex32::default(); source.samples_per_slot()];
        let mut timings: [Vec<f64>; 3] =
            std::array::from_fn(|_| Vec::with_capacity(MEASURED_SLOTS));
        for iteration in 0..WARMUP_SLOTS + MEASURED_SLOTS {
            let start = source.next_chip();
            if start.index.is_multiple_of(CHIPS_PER_FRAME) {
                source.queue_transport(start, 1, &payload, &[]).unwrap();
                source
                    .queue_transport(
                        ChipInstant {
                            index: start.index + PAGING_TRANSPORT_LEAD_FRAMES * CHIPS_PER_FRAME,
                            ..start
                        },
                        0,
                        &payload,
                        &[true; 18],
                    )
                    .unwrap();
            }
            let class = if start.index.is_multiple_of(CHIPS_PER_FRAME) {
                0
            } else if (start.index + 3 * CHIPS_PER_SLOT).is_multiple_of(CHIPS_PER_FRAME) {
                1
            } else {
                2
            };
            let before = Instant::now();
            black_box(source.generate_slot(black_box(&mut output)).unwrap());
            let elapsed = before.elapsed().as_secs_f64();
            black_box(&output);
            if iteration >= WARMUP_SLOTS {
                timings[class].push(elapsed);
            }
        }
        println!("compact={} setup_ms={:.2}", compact, setup_ms);
        for (name, mut times) in ["frame-start", "pich-start", "ordinary"]
            .into_iter()
            .zip(timings)
        {
            times.sort_by(f64::total_cmp);
            let count = times.len();
            println!(
                "class={} slots={} median_us={:.2} p95_us={:.2} max_us={:.2}",
                name,
                count,
                times[count / 2] * 1e6,
                times[count * 95 / 100] * 1e6,
                times[count - 1] * 1e6
            );
        }
    }
}
