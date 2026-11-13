// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn signatures_and_message_code_branches_match_the_spec() {
    let rows = [
        "++++++++++++++++",
        "+-+-+-+-+-+-+-+-",
        "++--++--++--++--",
        "+--++--++--++--+",
        "++++----++++----",
        "+-+--+-++-+--+-+",
        "++----++++----++",
        "+--+-++-+--+-++-",
        "++++++++--------",
        "+-+-+-+--+-+-+-+",
        "++--++----++--++",
        "+--++--+-++--++-",
        "++++--------++++",
        "+-+--+-+-+-++-+-",
        "++----++--++++--",
        "+--+-++--++-+--+",
    ];
    for (number, row) in rows.iter().enumerate() {
        let signature = PreambleSignature::new(number as u8).unwrap();
        let expected: Vec<_> = row
            .bytes()
            .map(|value| if value == b'+' { 1.0 } else { -1.0 })
            .collect();
        assert_eq!(signature.chips().as_slice(), expected);
        for sf in [32, 64, 128, 256] {
            let (data, control) = signature.message_codes(sf).unwrap();
            assert_eq!(control.spreading_factor(), 256);
            assert_eq!(control.index(), 16 * number as u16 + 15);
            assert_eq!(data.spreading_factor(), sf);
            assert_eq!(data.index(), sf * number as u16 / 16);
            assert!(!data.overlaps(control));
        }
    }
    assert_eq!(
        PreambleSignature::new(13)
            .unwrap()
            .message_codes(64)
            .unwrap()
            .0
            .index(),
        52
    );
    assert!(PreambleSignature::new(16).is_err());
    for sf in [0, 16, 63, 512] {
        assert!(
            PreambleSignature::new(0)
                .unwrap()
                .message_codes(sf)
                .is_err()
        );
    }
}

#[test]
fn complete_preambles_match_binary_recurrence_and_trigonometric_construction() {
    for number in [0, 7504, 8191] {
        let mut x = vec![0_u8; PREAMBLE_CHIPS + 25];
        let mut y = vec![1_u8; PREAMBLE_CHIPS + 25];
        for (index, value) in x[..24].iter_mut().enumerate() {
            *value = ((u32::from(number) >> index) & 1) as u8;
        }
        x[24] = 1;
        for index in 0..PREAMBLE_CHIPS {
            x[index + 25] = x[index + 3] ^ x[index];
            y[index + 25] = y[index + 3] ^ y[index + 2] ^ y[index + 1] ^ y[index];
        }
        for signature in [0, 3, 13, 15] {
            let mut output = vec![Complex32::default(); PREAMBLE_CHIPS];
            PrachCode::new(number)
                .unwrap()
                .write_preamble(PreambleSignature::new(signature).unwrap(), &mut output)
                .unwrap();
            for (index, value) in output.iter().enumerate() {
                let gold = if x[index] ^ y[index] == 0 { 1.0 } else { -1.0 };
                let signature = if ((index % 16) & signature as usize)
                    .count_ones()
                    .is_multiple_of(2)
                {
                    1.0
                } else {
                    -1.0
                };
                let phase =
                    std::f64::consts::FRAC_PI_4 + index as f64 * std::f64::consts::FRAC_PI_2;
                let expected = Complex64::new(phase.cos(), phase.sin()) * (gold * signature);
                assert!((f64::from(value.re) - expected.re).abs() < 1e-7);
                assert!((f64::from(value.im) - expected.im).abs() < 1e-7);
                assert!((value.norm_sqr() - 1.0).abs() < 1e-6);
            }
        }
    }
}

#[test]
fn cell_identity_selects_both_preamble_and_message_codes_without_aliasing() {
    assert_eq!(
        PrachCode::for_cell(PrimaryScramblingCode::new(469).unwrap(), 0)
            .unwrap()
            .number(),
        7504
    );
    assert_eq!(
        PrachCode::for_cell(PrimaryScramblingCode::new(511).unwrap(), 15)
            .unwrap()
            .number(),
        8191
    );
    assert!(PrachCode::for_cell(PrimaryScramblingCode::new(0).unwrap(), 16).is_err());
    assert!(PrachCode::new(8192).is_err());
    let code = PrachCode::new(7504).unwrap();
    let mut first = vec![Complex32::default(); 38400];
    let mut second = first.clone();
    code.write_message_scrambling(&mut first).unwrap();
    code.write_message_scrambling(&mut second).unwrap();
    assert_eq!(first, second);
    UplinkLongSequence::new(UplinkScramblingCode::new(7504).unwrap(), 4096)
        .unwrap()
        .write(&mut second);
    assert_eq!(first, second);
    assert!(first.iter().all(|chip| chip.norm_sqr() == 2.0));
    let mut invalid = [Complex32::new(0.2, 0.3); 3];
    let saved = invalid;
    assert!(
        code.write_preamble(PreambleSignature::new(0).unwrap(), &mut invalid)
            .is_err()
    );
    assert_eq!(invalid, saved);
    assert!(code.write_message_scrambling(&mut invalid).is_err());
    assert_eq!(invalid, saved);
}

#[test]
fn normalized_probe_recovers_every_signature_independently_of_gain_and_phase() {
    let code = PrachCode::new(7504).unwrap();
    let probe = PreambleProbe::new(code, u16::MAX).unwrap();
    for number in 0..16 {
        let signature = PreambleSignature::new(number).unwrap();
        let mut chips = vec![Complex32::default(); PREAMBLE_CHIPS];
        code.write_preamble(signature, &mut chips).unwrap();
        for gain in [1e-16_f32, 1e-6, 0.25, 20.0] {
            let channel = Complex32::new(0.6 * gain, -0.8 * gain);
            let input: Vec<_> = chips.iter().map(|chip| chip * channel).collect();
            let output = probe.correlate(&input).unwrap();
            for (index, result) in output.iter().enumerate() {
                let result = result.unwrap();
                assert_eq!(usize::from(result.signature.number()), index);
                if index == usize::from(number) {
                    assert!((result.coherent_fraction - 1.0).abs() < 1e-6);
                    assert!((result.blockwise_fraction - 1.0).abs() < 1e-6);
                    assert!(
                        (result.amplitude
                            / Complex64::new(f64::from(channel.re), f64::from(channel.im))
                            - Complex64::new(1.0, 0.0))
                        .norm()
                            < 1e-6
                    );
                } else {
                    assert!(result.coherent_fraction < 1e-12);
                    assert!(result.blockwise_fraction < 1e-12);
                }
            }
        }
    }
}

#[test]
fn blockwise_probe_quantifies_frequency_rotation_without_hiding_coherent_loss() {
    let code = PrachCode::new(7504).unwrap();
    let probe = PreambleProbe::new(code, 1 << 13).unwrap();
    let mut chips = vec![Complex32::default(); PREAMBLE_CHIPS];
    code.write_preamble(PreambleSignature::new(13).unwrap(), &mut chips)
        .unwrap();
    for frequency in [-3000.0, 3000.0] {
        let input: Vec<_> = chips
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    * Complex32::from_polar(
                        0.01,
                        (std::f64::consts::TAU * frequency * index as f64 / CHIPS_PER_SECOND as f64)
                            as f32,
                    )
            })
            .collect();
        let results = probe.correlate(&input).unwrap();
        let result = results[13].unwrap();
        assert!(
            results
                .iter()
                .enumerate()
                .all(|(index, result)| index == 13 || result.is_none())
        );
        assert!(result.coherent_fraction < 0.01);
        assert!(result.blockwise_fraction > 0.85);
        assert!((result.phase_slope_hz.unwrap() - frequency).abs() < 0.01);
    }
}

#[test]
fn probe_rejects_nonfinite_input_and_distinguishes_silence_from_correlation() {
    let code = PrachCode::new(0).unwrap();
    assert!(PreambleProbe::new(code, 0).is_err());
    let probe = PreambleProbe::new(code, u16::MAX).unwrap();
    let mut input = vec![Complex32::default(); PREAMBLE_CHIPS];
    for value in probe.correlate(&input).unwrap().into_iter().flatten() {
        assert_eq!(value.coherent_fraction, 0.0);
        assert_eq!(value.blockwise_fraction, 0.0);
        assert!(value.phase_slope_hz.is_none());
    }
    assert!(probe.correlate(&input[..1]).is_err());
    input[0].re = f32::NAN;
    assert!(matches!(
        probe.correlate(&input),
        Err(DspError::NonFiniteEvidence)
    ));
    input[0].re = f32::INFINITY;
    assert!(probe.correlate(&input).is_err());
}

#[test]
fn probe_separates_simultaneous_signatures_and_accounts_for_unselected_interference() {
    let code = PrachCode::new(7504).unwrap();
    let probe = PreambleProbe::new(code, (1 << 3) | (1 << 13)).unwrap();
    let mut input = vec![Complex32::default(); PREAMBLE_CHIPS];
    let mut reference = input.clone();
    for (signature, channel) in [
        (3, Complex32::new(0.6, 0.8)),
        (13, Complex32::new(-0.3, 0.4)),
        (7, Complex32::new(0.25, 0.0)),
    ] {
        code.write_preamble(PreambleSignature::new(signature).unwrap(), &mut reference)
            .unwrap();
        for (sum, value) in input.iter_mut().zip(&reference) {
            *sum += value * channel;
        }
    }
    let results = probe.correlate(&input).unwrap();
    assert!(results[7].is_none());
    for (signature, power, channel) in [
        (3, 1.0, Complex64::new(0.6, 0.8)),
        (13, 0.25, Complex64::new(-0.3, 0.4)),
    ] {
        let result = results[signature].unwrap();
        assert!((result.coherent_fraction - power / 1.3125).abs() < 1e-6);
        assert!((result.amplitude - channel).norm() < 1e-6);
    }
}

#[test]
fn partial_windows_keep_absolute_signature_phase_and_reject_invalid_spans() {
    let code = PrachCode::new(8191).unwrap();
    let probe = PreambleProbe::new(code, 1 << 13).unwrap();
    let mut input = vec![Complex32::default(); PREAMBLE_CHIPS];
    code.write_preamble(PreambleSignature::new(13).unwrap(), &mut input)
        .unwrap();
    for offset in [0, 1, 15, 256, 1024, 2048] {
        let result = probe
            .correlate_span(&input[offset..offset + 1024], offset)
            .unwrap()[13]
            .unwrap();
        assert!((result.coherent_fraction - 1.0).abs() < 1e-6);
    }
    for (size, offset) in [(0, 0), (33, 0), (1024, usize::MAX), (4096, 1)] {
        assert!(probe.correlate_span(&input[..size], offset).is_err());
    }
}
