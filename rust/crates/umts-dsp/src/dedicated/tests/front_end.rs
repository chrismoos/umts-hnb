// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    carrier::CarrierOffsetHz,
    fractional_sampling::FractionalSampler,
    pulse_shaping::{PulseShaper, SamplesPerChip},
    receive_filter::ReceiveFilter,
};
use umts_types::time::SampleInstant;

#[test]
fn full_rate_filter_timing_sampling_and_dpcch_reception_share_one_waveform() {
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        let waveform = uplink_waveform(0, &[(64, 16, false)], 0.2);
        let mut padded = vec![Complex32::default(); 32];
        padded.extend(&waveform);
        padded.resize(padded.len() + 64, Complex32::default());
        let mut tx = PulseShaper::new(rate, 32).unwrap();
        let mut raw = vec![Complex32::default(); tx.output_length(padded.len()).unwrap()];
        tx.process(&padded, &mut raw).unwrap();
        for delay in 0..rate.get() {
            let mut delayed = vec![Complex32::default(); delay];
            delayed.extend(&raw);
            let origin = SampleInstant {
                epoch: at(0).epoch,
                index: 137,
            };
            let mut filter = ReceiveFilter::full_rate(rate, 32, origin).unwrap();
            let mut filtered =
                vec![Complex32::default(); filter.output_length(delayed.len()).unwrap()];
            let window = filter.process(origin, &delayed, &mut filtered).unwrap();
            let first = window.first_center.unwrap();
            let format = UplinkControlFormat::new(0).unwrap();
            let code = Arc::new(
                UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap(),
            );
            let plan = Arc::new(
                DedicatedTimingPlan::new(&code, format, 0, rate.get(), 64 * rate.get()).unwrap(),
            );
            let mut search = DedicatedTimingSearch::new(plan.clone());
            let peak = search
                .search(
                    first,
                    &filtered[..plan.input_length()],
                    CarrierOffsetHz::new(0.0).unwrap(),
                )
                .unwrap()
                .strongest();
            assert_eq!(
                peak.position.sample().index,
                origin.index
                    + (32 * rate.get()) as u64
                    + tx.group_delay_samples() as u64
                    + delay as u64
            );
            // Orthogonal data contributes energy in the pilot window, so coherent fraction is not unity.
            assert!(peak.coherent_fraction > 0.25);
            let mut chips = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
            FractionalSampler::new()
                .write_chips(first, &filtered, peak.position, rate, &mut chips)
                .unwrap();
            let mut receiver = UplinkDpchReceiver::new(code, format).unwrap();
            receiver.begin_frame(at(0)).unwrap();
            for (slot, input) in chips.chunks_exact(CHIPS_PER_SLOT as usize).enumerate() {
                let control = receiver
                    .receive(at(slot as u64 * CHIPS_PER_SLOT), input)
                    .unwrap();
                let expected = Complex64::new(
                    f64::from(channel(slot, 0.2).re),
                    f64::from(channel(slot, 0.2).im),
                ) * 0.6;
                assert!((control.pilot.channel - expected).norm() < 0.0006);
            }
            let decision = receiver
                .frame()
                .unwrap()
                .decode_tfci(
                    &TfciDecoder::new(&[Tfci::new(0).unwrap(), Tfci::new(511).unwrap()]).unwrap(),
                )
                .unwrap()
                .unwrap();
            assert_eq!(decision.tfci.index(), 511);
        }
    }
}
