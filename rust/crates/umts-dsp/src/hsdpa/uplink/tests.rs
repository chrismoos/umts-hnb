// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::uplink_scrambling::UplinkScramblingCode;
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
        index,
    }
}

#[test]
fn feedback_uses_corresponding_frames_not_global_slot_rounding() {
    assert_eq!(
        feedback_start(at(7680), at(0), at(1024)).unwrap(),
        at(34560)
    );
    assert_eq!(
        feedback_start(at(38400), at(256), at(1280)).unwrap(),
        at(65280)
    );
    assert!(feedback_start(at(257), at(0), at(1024)).is_err());
    assert!(feedback_start(at(38400), at(0), at(1024)).is_err());
}

#[test]
fn despreading_preserves_configured_branches_and_scrambling_across_frame_wrap() {
    let scrambling = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0).unwrap()).unwrap());
    let channel = Complex64::new(0.3, -0.7);
    for (maximum, code_index, quadrature) in [
        (0, 33, true),
        (1, 64, true),
        (2, 1, false),
        (3, 32, true),
        (4, 1, false),
        (5, 32, true),
        (6, 1, false),
    ] {
        let receiver = FeedbackChannel::new(Arc::clone(&scrambling), maximum, at(1024)).unwrap();
        let mut code = [0.0; SPREADING];
        OvsfCode::new(SPREADING as u16, code_index)
            .unwrap()
            .write(&mut code)
            .unwrap();
        let offset = CHIPS_PER_FRAME as usize - SPREADING;
        let input: Vec<_> = (0..CHIPS_PER_SLOT as usize)
            .map(|index| {
                let sign = if (index / SPREADING).is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
                let branch = if quadrature {
                    Complex64::new(0.0, sign)
                } else {
                    Complex64::new(sign, 0.0)
                };
                let scramble = scrambling.scrambling()[(offset + index) % CHIPS_PER_FRAME as usize];
                let value = channel
                    * branch
                    * f64::from(code[index % SPREADING])
                    * Complex64::new(scramble.re.into(), scramble.im.into());
                Complex32::new(value.re as f32, value.im as f32)
            })
            .collect();
        let output = receiver
            .despread_slot(at(1024 + offset as u64), &input, &[channel; SYMBOLS])
            .unwrap();
        for (index, value) in output.into_iter().enumerate() {
            let expected = channel.norm_sqr() * if index % 2 == 0 { 1.0 } else { -1.0 };
            assert!((f64::from(value) - expected).abs() < 1e-6);
        }
    }
}
