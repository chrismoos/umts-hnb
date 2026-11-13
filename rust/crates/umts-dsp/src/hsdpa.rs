// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, bits::Bit, interleaving::Interleaver};

mod cqi;
mod dual_feedback;
mod feedback;
mod harq;
mod physical;
mod scch;
mod size;
mod transmission;
mod transport;
mod uplink;
pub use cqi::CqiReference;
pub use dual_feedback::{
    CellAck, CompositeCqi, DualHarqAck, decode_cqi_composite, decode_harq_ack_dual,
};
pub use feedback::{Cqi, FeedbackEvidence, HarqAck, decode_cqi, decode_harq_ack};
pub use harq::{HarqRateMatching, Redundancy};
pub use physical::{PdschSpreader, ScchSpreader};
pub use scch::{ScchAssignment, ScchEncoder, SecondaryCellOrder, TransmissionVersion};
pub use size::{TransportTable, transport_bits};
pub use transmission::TransmissionPlan;
pub use transport::{CrcMethod, TransportEncoder};
pub use uplink::{FeedbackChannel, feedback_start};

#[cfg(test)]
mod tests;

const INTERLEAVER_BITS: usize = 960;
const SYMBOLS_PER_SUBFRAME: usize = 480;
pub const MAX_CODES: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modulation {
    Qpsk,
    Qam16,
    Qam64,
}

impl Modulation {
    pub const fn bits_per_symbol(self) -> usize {
        match self {
            Self::Qpsk => 2,
            Self::Qam16 => 4,
            Self::Qam64 => 6,
        }
    }

    pub const fn bits_per_code(self) -> usize {
        SYMBOLS_PER_SUBFRAME * self.bits_per_symbol()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstellationVersion {
    Original,
    Swap,
    Invert,
    SwapInvert,
}

// TS 25.212 4.5.4.4 fills systematic rows first and alternates parity 2 then parity 1.
pub fn collect_harq_bits<T: Copy>(
    modulation: Modulation,
    systematic: &[T],
    parity1: &[T],
    parity2: &[T],
    output: &mut [T],
) -> Result<(), DspError> {
    let rows = modulation.bits_per_symbol();
    let total = systematic
        .len()
        .checked_add(parity1.len())
        .and_then(|value| value.checked_add(parity2.len()))
        .ok_or(DspError::Capacity)?;
    if total != output.len()
        || total == 0
        || !total.is_multiple_of(rows)
        || parity2.len() != (parity1.len() + parity2.len()).div_ceil(2)
    {
        return Err(DspError::InvalidLength);
    }
    let columns = total / rows;
    let full_rows = systematic.len() / columns;
    let extra_columns = systematic.len() % columns;
    let mut systematic_index = 0;
    let mut parity_index = 0;
    for (column, values) in output.chunks_exact_mut(rows).enumerate() {
        let systematic_rows = full_rows + usize::from(column < extra_columns);
        for (row, value) in values.iter_mut().enumerate() {
            *value = if row < systematic_rows {
                let bit = systematic[systematic_index];
                systematic_index += 1;
                bit
            } else {
                let bit = if parity_index % 2 == 0 {
                    parity2[parity_index / 2]
                } else {
                    parity1[parity_index / 2]
                };
                parity_index += 1;
                bit
            };
        }
    }
    Ok(())
}

pub struct PhysicalChannelInterleaver {
    source: [usize; INTERLEAVER_BITS],
}

impl PhysicalChannelInterleaver {
    pub fn new() -> Result<Self, DspError> {
        let input = std::array::from_fn::<_, INTERLEAVER_BITS, _>(|index| index);
        let mut source = [0; INTERLEAVER_BITS];
        Interleaver::second(INTERLEAVER_BITS)?.interleave(&input, &mut source)?;
        Ok(Self { source })
    }

    // TS 25.212 4.5.5–4.5.7: contiguous code segments, parallel pair interleavers.
    pub fn prepare(
        &self,
        modulation: Modulation,
        version: ConstellationVersion,
        input: &[Bit],
        output: &mut [Bit],
    ) -> Result<(), DspError> {
        self.prepare_with(modulation, version, input, output, |bit| {
            Bit::from(bit == Bit::Zero)
        })
    }

    pub(super) fn prepare_with<T: Copy>(
        &self,
        modulation: Modulation,
        version: ConstellationVersion,
        input: &[T],
        output: &mut [T],
        invert: impl Fn(T) -> T,
    ) -> Result<(), DspError> {
        let code_bits = modulation.bits_per_code();
        let codes = input.len() / code_bits;
        if !(1..=MAX_CODES).contains(&codes)
            || !input.len().is_multiple_of(code_bits)
            || output.len() != input.len()
            || (modulation == Modulation::Qpsk && version != ConstellationVersion::Original)
        {
            return Err(DspError::InvalidLength);
        }
        let lanes = modulation.bits_per_symbol() / 2;
        for (input, output) in input
            .chunks_exact(code_bits)
            .zip(output.chunks_exact_mut(code_bits))
        {
            for (index, &source) in self.source.iter().enumerate() {
                for lane in 0..lanes {
                    let from = (source / 2) * lanes * 2 + lane * 2 + source % 2;
                    let to = (index / 2) * lanes * 2 + lane * 2 + index % 2;
                    output[to] = input[from];
                }
            }
            if modulation == Modulation::Qam16 {
                for symbol in output.chunks_exact_mut(4) {
                    if matches!(
                        version,
                        ConstellationVersion::Swap | ConstellationVersion::SwapInvert
                    ) {
                        symbol.swap(0, 2);
                        symbol.swap(1, 3);
                    }
                    if matches!(
                        version,
                        ConstellationVersion::Invert | ConstellationVersion::SwapInvert
                    ) {
                        symbol[2] = invert(symbol[2]);
                        symbol[3] = invert(symbol[3]);
                    }
                }
            } else if modulation == Modulation::Qam64 {
                // TS 25.212 Table 11A permutes bit pairs and inverts the middle pair.
                for symbol in output.chunks_exact_mut(6) {
                    let original = [
                        symbol[0], symbol[1], symbol[2], symbol[3], symbol[4], symbol[5],
                    ];
                    let order = match version {
                        ConstellationVersion::Original => [0, 1, 2],
                        ConstellationVersion::Swap => [2, 1, 0],
                        ConstellationVersion::Invert => [1, 2, 0],
                        ConstellationVersion::SwapInvert => [0, 1, 2],
                    };
                    for (pair, source) in order.into_iter().enumerate() {
                        for component in 0..2 {
                            let value = original[2 * source + component];
                            symbol[2 * pair + component] =
                                if pair == 1 && version != ConstellationVersion::Original {
                                    invert(value)
                                } else {
                                    value
                                };
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
