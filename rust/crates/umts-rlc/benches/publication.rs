// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    hint::black_box,
    num::{NonZeroU16, NonZeroU64, NonZeroUsize},
    time::Instant,
};
use umts_rlc::acknowledged::{AmConfig, AmEntity, AmLimits, Polling};
use umts_types::{RadioEpoch, time::ChipInstant};

const PDU_BYTES: usize = 64;
const PAYLOAD_BYTES: usize = 48;
const QUEUED_SDUS: usize = 8;
const MAXIMUM_SDU_BYTES: usize = 4096;
const BUFFER_LIMIT: usize = 32 * 1024 * 1024;
const RECEIVE_WINDOW: u16 = 32;
const WARMUP: usize = 4;
const ROUNDS: usize = 40;
const CALLS: usize = 1000;

fn main() {
    println!("Isolated AM publication readiness, synthetic input.");
    let now = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index: 0,
    };
    for window in [32, 256, 2047] {
        for pending in [false, true] {
            let mut entity = AmEntity::new(
                AmConfig {
                    transmit_pdu_bytes: NonZeroUsize::new(PDU_BYTES).unwrap(),
                    receive_pdu_bytes: NonZeroUsize::new(PDU_BYTES).unwrap(),
                    transmit_window: NonZeroU16::new(window).unwrap(),
                    receive_window: NonZeroU16::new(RECEIVE_WINDOW).unwrap(),
                    maximum_data_attempts: NonZeroU16::MAX,
                    maximum_reset_attempts: NonZeroU16::MAX,
                    reset_timeout_chips: NonZeroU64::MAX,
                    polling: Polling {
                        every_pdus: None,
                        last_transmission: false,
                        last_retransmission: false,
                        timeout_chips: None,
                        prohibit_chips: None,
                    },
                    status_prohibit_chips: None,
                    missing_pdu_indicator: true,
                    accept_special_he: false,
                    initial_tx_hfn: 0,
                    initial_rx_hfn: 0,
                },
                AmLimits {
                    maximum_sdu_bytes: NonZeroUsize::new(MAXIMUM_SDU_BYTES).unwrap(),
                    queued_sdus: NonZeroUsize::new(QUEUED_SDUS).unwrap(),
                    maximum_buffer_bytes: BUFFER_LIMIT,
                },
                now,
            )
            .unwrap();
            for _ in 0..window - 1 {
                entity.enqueue(&[0x55; PAYLOAD_BYTES]).unwrap();
                assert!(
                    entity
                        .publish_next(now, |_| Ok::<_, ()>(()))
                        .unwrap()
                        .is_some()
                );
            }
            assert_eq!(entity.outstanding_pdus(), usize::from(window - 1));
            if pending {
                entity.enqueue(&[0x55; PAYLOAD_BYTES]).unwrap();
            }
            let mut times = Vec::with_capacity(ROUNDS);
            let mut ready = 0;
            for round in 0..WARMUP + ROUNDS {
                let start = Instant::now();
                for _ in 0..CALLS {
                    ready += usize::from(
                        black_box(&mut entity)
                            .publication_ready(black_box(now))
                            .unwrap(),
                    );
                }
                if round >= WARMUP {
                    times.push(start.elapsed().as_nanos() as f64 / CALLS as f64);
                }
            }
            assert_eq!(ready, usize::from(pending) * (WARMUP + ROUNDS) * CALLS);
            times.sort_by(f64::total_cmp);
            println!(
                "window={window} pending={pending} median_ns={:.2} p95_ns={:.2} max_ns={:.2}",
                times[ROUNDS / 2],
                times[ROUNDS * 95 / 100],
                times[ROUNDS - 1]
            );
        }
    }
}
