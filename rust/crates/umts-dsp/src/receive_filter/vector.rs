// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::DspError;
use num_complex::Complex32;
use wide::f32x8;

pub(super) const COMPLEX_LANES: usize = 4;
pub(super) const SPECIALIZED_TAPS: usize = 65;
const SPECIALIZED_HALF: usize = SPECIALIZED_TAPS / 2;
const SPECIALIZED_GROUPS: usize = SPECIALIZED_HALF / COMPLEX_LANES;
#[cfg(all(test, target_feature = "avx2"))]
const FOUR_OUTPUT_SAMPLES: usize = SPECIALIZED_TAPS + COMPLEX_LANES - 1;

pub(super) struct Coefficients {
    packed: Vec<f32x8>,
}

impl Coefficients {
    #[cfg(all(test, target_feature = "avx2"))]
    pub(super) fn consecutive_four(
        values: &[Complex32],
        taps: &[f32],
    ) -> Option<[Complex32; COMPLEX_LANES]> {
        let values: &[Complex32; FOUR_OUTPUT_SAMPLES] = values.try_into().ok()?;
        let taps: &[f32; SPECIALIZED_TAPS] = taps.try_into().ok()?;
        let mut sums = [f32x8::ZERO; COMPLEX_LANES];
        // Each chain preserves one scalar tap-index residue and its f32 rounding order.
        for group in 0..SPECIALIZED_GROUPS {
            for (residue, sum) in sums.iter_mut().enumerate() {
                let tap = group * COMPLEX_LANES + residue;
                let front = pack_four(&values[tap..]);
                let back = pack_four(&values[SPECIALIZED_TAPS - 1 - tap..]);
                *sum += (front + back) * f32x8::splat(taps[tap]);
            }
        }
        let result = (sums[0] + sums[1]) + (sums[2] + sums[3]);
        let result =
            result + pack_four(&values[SPECIALIZED_HALF..]) * f32x8::splat(taps[SPECIALIZED_HALF]);
        let lanes = result.to_array();
        Some(std::array::from_fn(|index| {
            let lane = (COMPLEX_LANES - 1 - index) * 2;
            Complex32::new(lanes[lane], lanes[lane + 1])
        }))
    }

    pub(super) fn new(taps: &[f32]) -> Result<Self, DspError> {
        let mut packed = Vec::new();
        packed
            .try_reserve_exact(taps.len() / 2 / COMPLEX_LANES)
            .map_err(|_| DspError::Capacity)?;
        packed.extend(
            taps[..taps.len() / 2]
                .chunks_exact(COMPLEX_LANES)
                .map(|taps| {
                    f32x8::new([
                        taps[0], taps[0], taps[1], taps[1], taps[2], taps[2], taps[3], taps[3],
                    ])
                }),
        );
        Ok(Self { packed })
    }

    pub(super) fn dot(&self, values: &[Complex32], taps: &[f32]) -> Complex32 {
        if let (Ok(values), Ok(taps), Ok(packed)) = (
            <&[Complex32; SPECIALIZED_TAPS]>::try_from(values),
            <&[f32; SPECIALIZED_TAPS]>::try_from(taps),
            <&[f32x8; SPECIALIZED_GROUPS]>::try_from(self.packed.as_slice()),
        ) {
            return Self::dot_fixed(values, taps, packed);
        }
        let half = taps.len() / 2;
        let mut sums = f32x8::ZERO;
        let mut front = values[..half].chunks_exact(COMPLEX_LANES);
        let mut back = values[half + 1..].rchunks_exact(COMPLEX_LANES);
        for ((front, back), &taps) in front.by_ref().zip(back.by_ref()).zip(&self.packed) {
            let front = f32x8::new([
                front[0].re,
                front[0].im,
                front[1].re,
                front[1].im,
                front[2].re,
                front[2].im,
                front[3].re,
                front[3].im,
            ]);
            let back = f32x8::new([
                back[3].re, back[3].im, back[2].re, back[2].im, back[1].re, back[1].im, back[0].re,
                back[0].im,
            ]);
            sums += (front + back) * taps;
        }
        // Preserve scalar accumulation order, including separate multiply and add rounding.
        let lanes = sums.to_array();
        let mut sum = Complex32::new(
            (lanes[0] + lanes[2]) + (lanes[4] + lanes[6]),
            (lanes[1] + lanes[3]) + (lanes[5] + lanes[7]),
        );
        for ((&front, &back), &tap) in front
            .remainder()
            .iter()
            .zip(back.remainder().iter().rev())
            .zip(&taps[self.packed.len() * COMPLEX_LANES..half])
        {
            sum += (front + back) * tap;
        }
        sum + values[half] * taps[half]
    }

    fn dot_fixed(
        values: &[Complex32; SPECIALIZED_TAPS],
        taps: &[f32; SPECIALIZED_TAPS],
        packed: &[f32x8; SPECIALIZED_GROUPS],
    ) -> Complex32 {
        let mut sums = f32x8::ZERO;
        for (index, &tap) in packed.iter().enumerate() {
            let front = index * COMPLEX_LANES;
            let back = SPECIALIZED_TAPS - 1 - front;
            let left = f32x8::new([
                values[front].re,
                values[front].im,
                values[front + 1].re,
                values[front + 1].im,
                values[front + 2].re,
                values[front + 2].im,
                values[front + 3].re,
                values[front + 3].im,
            ]);
            let right = f32x8::new([
                values[back].re,
                values[back].im,
                values[back - 1].re,
                values[back - 1].im,
                values[back - 2].re,
                values[back - 2].im,
                values[back - 3].re,
                values[back - 3].im,
            ]);
            sums += (left + right) * tap;
        }
        let lanes = sums.to_array();
        Complex32::new(
            (lanes[0] + lanes[2]) + (lanes[4] + lanes[6]),
            (lanes[1] + lanes[3]) + (lanes[5] + lanes[7]),
        ) + values[SPECIALIZED_HALF] * taps[SPECIALIZED_HALF]
    }
}

#[cfg(all(test, target_feature = "avx2"))]
fn pack_four(values: &[Complex32]) -> f32x8 {
    f32x8::new([
        values[0].re,
        values[0].im,
        values[1].re,
        values[1].im,
        values[2].re,
        values[2].im,
        values[3].re,
        values[3].im,
    ])
}

// A block-local deinterleaved view avoids rebuilding interleaved SIMD vectors
// for every tap and output. The persistent history and its reset rules stay shared.
#[cfg(target_feature = "avx2")]
pub(super) type PlanarBlock = PlanarBlockShape<
    SPECIALIZED_TAPS,
    { SPECIALIZED_TAPS + super::INPUT_BLOCK_SAMPLES - 1 },
    { SPECIALIZED_TAPS + 7 },
>;
#[cfg(target_feature = "avx2")]
pub(super) type PlanarBlock33 =
    PlanarBlockShape<33, { 33 + super::INPUT_BLOCK_SAMPLES - 1 }, { 33 + 7 }>;

#[cfg(target_feature = "avx2")]
pub(super) struct PlanarBlockShape<const TAPS: usize, const HISTORY: usize, const WINDOW: usize> {
    real: [f32; HISTORY],
    imag: [f32; HISTORY],
}

#[cfg(target_feature = "avx2")]
impl<const TAPS: usize, const HISTORY: usize, const WINDOW: usize>
    PlanarBlockShape<TAPS, HISTORY, WINDOW>
{
    pub(super) const OUTPUTS: usize = 8;

    pub(super) fn new(history: &[Complex32]) -> Self {
        Self {
            real: std::array::from_fn(|index| history[index].re),
            imag: std::array::from_fn(|index| history[index].im),
        }
    }

    pub(super) fn consecutive_eight(&self, start: usize, taps: &[f32]) -> [Complex32; 8] {
        let real: &[f32; WINDOW] = self.real[start..start + WINDOW].try_into().unwrap();
        let imag: &[f32; WINDOW] = self.imag[start..start + WINDOW].try_into().unwrap();
        let taps: &[f32; TAPS] = taps.try_into().unwrap();
        let half = TAPS / 2;
        let mut real_sums = [f32x8::ZERO; COMPLEX_LANES];
        let mut imag_sums = [f32x8::ZERO; COMPLEX_LANES];
        for group in 0..half / COMPLEX_LANES {
            for residue in 0..COMPLEX_LANES {
                let tap = group * COMPLEX_LANES + residue;
                let back = TAPS - 1 - tap;
                let weight = f32x8::splat(taps[tap]);
                real_sums[residue] +=
                    (load_eight(&real[tap..]) + load_eight(&real[back..])) * weight;
                imag_sums[residue] +=
                    (load_eight(&imag[tap..]) + load_eight(&imag[back..])) * weight;
            }
        }
        let weight = f32x8::splat(taps[half]);
        let real = ((real_sums[0] + real_sums[1]) + (real_sums[2] + real_sums[3]))
            + load_eight(&real[half..]) * weight;
        let imag = ((imag_sums[0] + imag_sums[1]) + (imag_sums[2] + imag_sums[3]))
            + load_eight(&imag[half..]) * weight;
        let real = real.to_array();
        let imag = imag.to_array();
        std::array::from_fn(|index| {
            let lane = Self::OUTPUTS - 1 - index;
            Complex32::new(real[lane], imag[lane])
        })
    }
}

#[cfg(target_feature = "avx2")]
fn load_eight(values: &[f32]) -> f32x8 {
    f32x8::new(std::array::from_fn(|index| values[index]))
}
