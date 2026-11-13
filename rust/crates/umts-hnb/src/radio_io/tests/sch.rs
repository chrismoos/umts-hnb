// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use sdr_hal::{Radio, memory::MemoryRadio};
use umts_dsp::{pulse_shaping::SamplesPerChip, synchronization};
use umts_types::time::CHIPS_PER_SLOT;

use crate::{
    common_tx::{CommonAmplitudes, CommonTx},
    radio_io::{PumpProgress, TxSettings, prepare_tx},
    test_support::{amplitudes, at, count, plan},
};

fn capture_first_slot(source: CommonTx) -> Vec<Complex32> {
    let (mut radio, peer) = MemoryRadio::new(1);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source,
        tx,
        TxSettings::offline(1_000, count(1), count(1), count(2)),
    )
    .unwrap();
    preparation.prepare_one().unwrap().unwrap();
    pump.start().unwrap();
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    let block = peer.output.recv().unwrap();
    assert_eq!(block.time_ticks, Some(1_000));
    pump.stop().unwrap();
    block.samples
}

fn source_with_sch(amplitudes: CommonAmplitudes) -> CommonTx {
    CommonTx::new(
        plan(false),
        at(0),
        SamplesPerChip::Two,
        32,
        amplitudes,
        count(8),
        100,
    )
    .unwrap()
}

fn independent_first_slot_sch() -> Vec<Complex32> {
    let cell_plan = plan(false);
    let cell = cell_plan.information();
    let primary = synchronization::primary();
    let secondary_number =
        synchronization::secondary_allocation(cell.primary_code().group()).unwrap()[0];
    let secondary = synchronization::secondary(secondary_number).unwrap();
    let mut chips = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    for chip in 0..synchronization::SYNCHRONIZATION_CHIPS {
        chips[chip] =
            (primary[chip] * 0.04 + secondary[chip] * 0.04) * -std::f32::consts::FRAC_1_SQRT_2;
    }
    // TS 25.104 6.8.1, direct convolution independent of the production polyphase shaper.
    const ROLL_OFF: f64 = 0.22;
    const RATE: usize = 2;
    const SPAN: usize = 32;
    let mut taps: Vec<f64> = (0..=SPAN * RATE)
        .map(|index| {
            let t = (index as f64 - (SPAN * RATE / 2) as f64) / RATE as f64;
            let x = (1.0 - ROLL_OFF) * t;
            let sinc = if x == 0.0 {
                1.0
            } else {
                (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
            };
            ((1.0 - ROLL_OFF) * sinc
                + 4.0 * ROLL_OFF / std::f64::consts::PI
                    * (std::f64::consts::PI * (1.0 + ROLL_OFF) * t).cos())
                / (1.0 - (4.0 * ROLL_OFF * t).powi(2))
        })
        .collect();
    let scale = (RATE as f64 / taps.iter().map(|tap| tap * tap).sum::<f64>()).sqrt();
    for tap in &mut taps {
        *tap *= scale
    }
    let mut samples = vec![Complex32::default(); chips.len() * RATE];
    for (chip, value) in chips.into_iter().enumerate() {
        for (offset, tap) in taps.iter().enumerate() {
            if let Some(sample) = samples.get_mut(chip * RATE + offset) {
                *sample += value * *tap as f32;
            }
        }
    }
    samples
}

#[test]
fn memory_radio_tx_preserves_non_sttd_negative_sch_symbol_through_pulse_shaping() {
    let with_sch = capture_first_slot(source_with_sch(amplitudes()));
    let mut without = amplitudes();
    without.primary.primary_sch = 0.0;
    without.primary.secondary_sch = 0.0;
    let without_sch = capture_first_slot(source_with_sch(without));
    let expected = independent_first_slot_sch();

    assert_eq!(with_sch.len(), 5_120);
    assert_eq!(without_sch.len(), with_sch.len());
    assert_eq!(expected.len(), with_sch.len());
    let mut maximum_error = 0.0_f32;
    for ((&with, &without), &expected) in with_sch.iter().zip(&without_sch).zip(&expected) {
        let error = (with - without - expected).norm();
        maximum_error = maximum_error.max(error);
    }
    assert!(
        maximum_error < 3e-6,
        "maximum SCH waveform error {maximum_error}"
    );
}

#[test]
fn final_iq_capture_matches_actual_hal_floating_point_samples_without_quantization() {
    let directory = std::env::temp_dir().join(format!(
        "umts-final-iq-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (mut radio, peer) = MemoryRadio::new(1);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source_with_sch(amplitudes()),
        tx,
        TxSettings::offline(1_000, count(1), count(1), count(2)),
    )
    .unwrap();
    use super::super::capture::live;
    let geometry = live::Geometry {
        direction: live::Direction::Transmit,
        epoch: at(0).epoch.get(),
        sample_rate: umts_types::time::SampleRate::new(7_680_000, 1).unwrap(),
        tick_rate: std::num::NonZeroU64::new(1_000_000).unwrap(),
    };
    let (mut port, recorder) = live::channel(geometry);
    let capture = live::Recording::new(
        directory.clone(),
        1,
        geometry,
        std::time::Duration::from_millis(1),
    )
    .unwrap();
    assert!(port.publish(capture).is_ok());
    pump.attach_live_capture(recorder);
    preparation.prepare_one().unwrap().unwrap();
    pump.start().unwrap();
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    let block = peer.output.recv().unwrap();
    pump.stop().unwrap();
    pump.finish_capture();
    let (_, result) = port.take_finished().unwrap().finish();
    let summary = result.unwrap();
    assert_eq!(summary.records, 1);
    assert_eq!(summary.incomplete_records, 0);
    let bytes = std::fs::read(directory.join("raw-tx-0.bin")).unwrap();
    const HEADER_BYTES: usize = 32;
    const BLOCK_HEADER_BYTES: usize = 24;
    assert_eq!(&bytes[..8], b"UMTSTX01");
    assert_eq!(
        u64::from_le_bytes(bytes[HEADER_BYTES..HEADER_BYTES + 8].try_into().unwrap()),
        0
    );
    let payload = &bytes[HEADER_BYTES + BLOCK_HEADER_BYTES..];
    assert_eq!(payload.len(), block.samples.len() * 8);
    for (pair, sample) in payload.chunks_exact(8).zip(&block.samples) {
        assert_eq!(&pair[..4], sample.re.to_le_bytes());
        assert_eq!(&pair[4..], sample.im.to_le_bytes());
    }
    std::fs::remove_dir_all(directory).unwrap();
}
