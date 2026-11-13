// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::DspError;

// A single scale preserves relative reliability across slots/frames while avoiding gain-dependent float underflow.
pub fn normalize_soft_bits(input: &[f64], output: &mut [f32]) -> Result<(), DspError> {
    if input.len() != output.len() {
        return Err(DspError::InvalidLength);
    }
    if input.iter().any(|value| !value.is_finite()) {
        return Err(DspError::NonFiniteEvidence);
    }
    let scale = input
        .iter()
        .fold(0.0_f64, |scale, value| scale.max(value.abs()));
    for (out, value) in output.iter_mut().zip(input) {
        *out = if scale > 0.0 {
            (*value / scale) as f32
        } else {
            0.0
        };
    }
    Ok(())
}
