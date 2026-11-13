// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    carrier::CarrierOffsetHz,
    dedicated::{CarrierTrackingSettings, LikelihoodSettings, UplinkGains},
};
use umts_types::time::CHIPS_PER_SECOND;

#[test]
fn tracked_cfo_phase_and_gain_recover_tfci_and_late_data_with_finite_likelihoods() {
    let settings = CarrierTrackingSettings {
        initial: CarrierOffsetHz::new(0.0).unwrap(),
        maximum_absolute_hz: 2000.0,
        update_fraction: 0.5,
        minimum_coherence: 0.8,
    };
    for gain in [1e-12, 1.0, 1e12] {
        let mut input = uplink_waveform(0, &[(64, 16, false)], gain);
        for (index, value) in input.iter_mut().enumerate() {
            *value *= Complex32::from_polar(
                1.0,
                (std::f64::consts::TAU * 1200.0 * index as f64 / CHIPS_PER_SECOND as f64) as f32,
            );
        }
        let mut receiver = UplinkDpchReceiver::with_tracking(
            Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap()),
            UplinkControlFormat::new(0).unwrap(),
            settings,
        )
        .unwrap();
        receive_frame(&mut receiver, 1024, &input);
        let frame = receiver.frame().unwrap();
        let last = frame.controls()[14].carrier.unwrap();
        assert!((last.next_hz - 1200.0).abs() < 1.0, "{last:?}");
        let likelihood = LikelihoodSettings::new(128.0).unwrap();
        let decoder = TfciDecoder::new(&[Tfci::new(511).unwrap(), Tfci::new(17).unwrap()]).unwrap();
        let exported = frame.tfci_likelihoods(likelihood).unwrap().unwrap();
        let mut normalized = [0.0; crate::tfci::UPLINK_CODE_BITS];
        crate::soft_bits::normalize_soft_bits(&exported, &mut normalized).unwrap();
        let exported_decision = decoder.decode(&normalized).unwrap().unwrap();
        let direct_decision = frame
            .decode_tfci_reliable(&decoder, likelihood)
            .unwrap()
            .unwrap();
        assert_eq!(exported_decision.tfci, direct_decision.tfci);
        assert_eq!(
            exported_decision.normalized_score,
            direct_decision.normalized_score
        );
        assert_eq!(
            exported_decision.normalized_margin,
            direct_decision.normalized_margin
        );
        assert_eq!(
            frame
                .decode_tfci_reliable(&decoder, likelihood)
                .unwrap()
                .unwrap()
                .tfci,
            Tfci::new(511).unwrap()
        );
        let mut soft = vec![0.0; 600];
        frame
            .despread_llrs(
                UplinkLayout::single(64).unwrap(),
                0,
                UplinkGains::new(10, 15).unwrap(),
                likelihood,
                &mut soft,
            )
            .unwrap();
        assert!(soft.iter().all(|value| value.is_finite()));
        for (index, value) in soft.iter().enumerate().skip(400) {
            assert_eq!(*value < 0.0, data_bit(0, index) == Bit::One);
        }
    }
}
