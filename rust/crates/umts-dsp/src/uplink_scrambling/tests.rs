// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn long_sequence_matches_independent_binary_recurrence_and_exact_quadrature_offset() {
    let length = SECOND_OFFSET as usize + 4097 + 38400 + 25;
    for number in [0, 0x00ab_cdef] {
        let mut x = vec![0_u8; length];
        let mut y = vec![1_u8; length];
        for (index, value) in x[..24].iter_mut().enumerate() {
            *value = ((number >> index) & 1) as u8;
        }
        x[24] = 1;
        for index in 0..length - 25 {
            x[index + 25] = x[index + 3] ^ x[index];
            y[index + 25] = y[index + 3] ^ y[index + 2] ^ y[index + 1] ^ y[index];
        }
        for offset in [0, 1, 4096, 4097] {
            let mut actual = vec![Complex32::default(); 38400];
            UplinkLongSequence::new(UplinkScramblingCode::new(number).unwrap(), offset)
                .unwrap()
                .write(&mut actual);
            for (index, chip) in actual.iter().enumerate() {
                let time = index + offset as usize;
                let real = if x[time] ^ y[time] == 0 { 1.0 } else { -1.0 };
                let paired = (time / 2) * 2 + 16_777_232;
                let second = if x[paired] ^ y[paired] == 0 {
                    1.0
                } else {
                    -1.0
                };
                let imaginary = real * second * if time.is_multiple_of(2) { 1.0 } else { -1.0 };
                assert_eq!(
                    *chip,
                    Complex32::new(real, imaginary),
                    "code {number}, offset {offset}, chip {index}"
                );
            }
        }
    }
}

#[test]
fn jump_ahead_preserves_the_full_period_and_composes_exactly() {
    for taps in [X_TAPS, Y_TAPS] {
        for bit in 0..REGISTER_BITS {
            let state = 1 << bit;
            assert_eq!(jump(state, taps, PERIOD), state);
            assert_eq!(
                jump(jump(state, taps, 4096), taps, SECOND_OFFSET),
                jump(state, taps, SECOND_OFFSET + 4096)
            );
        }
    }
}

#[test]
fn split_writes_and_sequence_wrap_preserve_the_same_chips() {
    let code = UplinkScramblingCode::new(8191).unwrap();
    let mut whole = UplinkLongSequence::new(code, PERIOD - 3).unwrap();
    let mut split = whole.clone();
    let mut expected = [Complex32::default(); 19];
    whole.write(&mut expected);
    let mut actual = [Complex32::default(); 19];
    split.write(&mut actual[..1]);
    split.write(&mut actual[1..7]);
    split.write(&mut actual[7..]);
    assert_eq!(actual, expected);
    let mut start = [Complex32::default(); 16];
    UplinkLongSequence::new(code, 0).unwrap().write(&mut start);
    assert_eq!(expected[3..], start);
    assert!(UplinkScramblingCode::new(1 << 24).is_err());
    assert!(UplinkLongSequence::new(code, PERIOD).is_err());
}
