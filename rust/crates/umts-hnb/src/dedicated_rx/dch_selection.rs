// SPDX-License-Identifier: AGPL-3.0-or-later

use super::ReceiveError;
use umts_dsp::{
    dedicated_transport::UplinkTfcs,
    tfci::{Tfci, TfciDecoder},
};
use umts_rrc::hsupa::DchCoexistenceActivation;

pub struct PreparedDchSelection {
    pub(super) decoder: TfciDecoder,
    source: Box<[Tfci]>,
    original_rows: Box<[Vec<usize>]>,
}

impl PreparedDchSelection {
    pub fn new(configuration: &DchCoexistenceActivation) -> Result<Self, ReceiveError> {
        // TS 25.212 4.2.7.1.1 uses the minimum attribute even for empty TrCHs.
        // Reusing the predecessor allocation requires that minimum to be unchanged.
        let minimum = |tfcs: &umts_rrc::dedicated::DchTfcs| {
            tfcs.transports()
                .iter()
                .map(|channel| channel.rate_matching_attribute)
                .min()
        };
        if minimum(configuration.dedicated().uplink()) != minimum(configuration.uplink()) {
            return Err(ReceiveError::Configuration);
        }
        let source = configuration
            .source_combinations()
            .iter()
            .map(|index| {
                Tfci::new(u16::try_from(*index).map_err(|_| ReceiveError::Capacity)?)
                    .map_err(ReceiveError::Dsp)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_boxed_slice();
        let allowed = (0..source.len())
            .map(|index| {
                Tfci::new(u16::try_from(index).map_err(|_| ReceiveError::Capacity)?)
                    .map_err(ReceiveError::Dsp)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            decoder: TfciDecoder::new(&allowed)?,
            source,
            original_rows: configuration
                .dedicated()
                .uplink()
                .combinations()
                .iter()
                .map(|row| row.formats.clone())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        })
    }

    pub(super) fn matches(&self, tfcs: &UplinkTfcs) -> bool {
        self.original_rows.len() == tfcs.configured().combinations().len()
            && self
                .original_rows
                .iter()
                .zip(tfcs.configured().combinations())
                .all(|(expected, actual)| expected == &actual.formats)
            && self
                .source
                .iter()
                .all(|tfci| tfcs.allocation(*tfci).is_some())
    }

    // The new wire TFCI selects an unchanged compiled transport combination.
    pub(super) fn source(&self, wire: Tfci) -> Result<Tfci, ReceiveError> {
        self.source
            .get(usize::from(wire.index()))
            .copied()
            .ok_or(ReceiveError::Configuration)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self.decoder.storage_bytes()
            + size_of_val(&*self.source)
            + size_of_val(&*self.original_rows)
            + self
                .original_rows
                .iter()
                .map(|row| row.capacity() * size_of::<usize>())
                .sum::<usize>()
    }
}
