// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ControlChannels, InitialActivation, InitialConfiguration};
use crate::{
    dedicated::{DedicatedError, ScheduledActivation},
    establishment::TransactionId,
};
use rasn::types::FixedBitString;
use umts_rrc_codec::{EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};

mod conversion;
mod messages;
#[cfg(test)]
mod tests;
use conversion::*;
pub(crate) use messages::{reconfigure, setup, setup_body};

const MAXIMUM_UARFCN: u16 = 16383;
const MAXIMUM_SCRAMBLING_CODE: u16 = 511;
const H_RNTI_BITS: usize = u16::BITS as usize;

#[derive(Clone)]
pub struct SecondaryCellConfiguration {
    h_rnti: u16,
    uarfcn_dl: u16,
    primary_scrambling_code: u16,
    control: ControlChannels,
    measurement_power_offset_half_db: i8,
    qam64: bool,
}

impl SecondaryCellConfiguration {
    pub fn new(
        h_rnti: u16,
        uarfcn_dl: u16,
        primary_scrambling_code: u16,
        control: ControlChannels,
        measurement_power_offset_half_db: i8,
        qam64: bool,
    ) -> Result<Self, DedicatedError> {
        if uarfcn_dl > MAXIMUM_UARFCN
            || primary_scrambling_code > MAXIMUM_SCRAMBLING_CODE
            || !(-12..=26).contains(&measurement_power_offset_half_db)
        {
            return Err(DedicatedError::Physical);
        }
        Ok(Self {
            h_rnti,
            uarfcn_dl,
            primary_scrambling_code,
            control,
            measurement_power_offset_half_db,
            qam64,
        })
    }

    pub fn h_rnti(&self) -> u16 {
        self.h_rnti
    }
    pub fn uarfcn_dl(&self) -> u16 {
        self.uarfcn_dl
    }
    pub fn primary_scrambling_code(&self) -> u16 {
        self.primary_scrambling_code
    }
    pub fn control(&self) -> &ControlChannels {
        &self.control
    }
    pub fn measurement_power_offset_half_db(&self) -> i8 {
        self.measurement_power_offset_half_db
    }
    pub fn qam64(&self) -> bool {
        self.qam64
    }

    fn wire(&self) -> ie::DLSecondaryCellInfoFDD {
        let mut identity = FixedBitString::<H_RNTI_BITS>::default();
        for index in 0..H_RNTI_BITS {
            identity.set(index, self.h_rnti & (1 << (H_RNTI_BITS - 1 - index)) != 0);
        }
        ie::DLSecondaryCellInfoFDD::newConfiguration(ie::DLSecondaryCellInfoFDDNewConfiguration {
            new_h_rnti: ie::HRNTI(identity),
            dl_64_qam_configured: self
                .qam64
                .then_some(ie::DLSecondaryCellInfoFDDNewConfigurationDl64QAMConfigured::R_true),
            // TS 25.331 10.3.6.31a: 64-QAM implies octet alignment, otherwise omission selects bit alignment.
            hs_dsch_tbsize_table: None,
            primary_cpich_info: ie::PrimaryCPICHInfo::new(ie::PrimaryScramblingCode(
                self.primary_scrambling_code,
            )),
            dl_scrambling_code: None,
            h_s_scchchannelisation_code_info: self
                .control
                .codes()
                .iter()
                .map(|code| ie::HSSCCHCodes(code.index() as u8))
                .collect(),
            measurement_power_offset: ie::MeasurementPowerOffset(
                self.measurement_power_offset_half_db,
            ),
            uarfcn_dl: ie::UARFCN(self.uarfcn_dl),
        })
    }
}

pub enum SecondaryCellUpdate<'a> {
    Configure(&'a SecondaryCellConfiguration),
    Continue,
    Remove,
}

impl SecondaryCellUpdate<'_> {
    fn wire(&self) -> Option<ie::DLSecondaryCellInfoFDD> {
        match self {
            Self::Configure(configuration) => Some(configuration.wire()),
            Self::Continue => Some(ie::DLSecondaryCellInfoFDD::R_continue(())),
            // TS 25.331 8.6.6.45 clears the stored secondary configuration on omission.
            Self::Remove => None,
        }
    }
}

pub(super) enum ConfiguredCells {
    Single,
    Dual(SecondaryCellConfiguration),
}

impl InitialConfiguration {
    pub fn with_secondary_cell(
        mut self,
        secondary: SecondaryCellConfiguration,
    ) -> Result<Self, DedicatedError> {
        self.validate_secondary_configuration(&secondary)?;
        self.cells = ConfiguredCells::Dual(secondary);
        Ok(self)
    }

    pub(crate) fn validate_secondary_configuration(
        &self,
        secondary: &SecondaryCellConfiguration,
    ) -> Result<(), DedicatedError> {
        if !self.uses_mac_ehs()
            || secondary.primary_scrambling_code == self.dedicated().physical().primary_scrambling
            || (secondary.qam64 && !self.parameters().harq.category().limits().qam64)
            // TS 25.214 6A.1.1 limits the UE to six monitored codes across both cells.
            || self.parameters().control.codes().len() + secondary.control.codes().len() > 6
        {
            return Err(DedicatedError::Physical);
        }
        Ok(())
    }

    pub fn secondary_cell(&self) -> Option<&SecondaryCellConfiguration> {
        match &self.cells {
            ConfiguredCells::Single => None,
            ConfiguredCells::Dual(secondary) => Some(secondary),
        }
    }

    pub fn message_with_secondary_cell(
        &self,
        transaction: TransactionId,
        activation: ScheduledActivation,
        secondary: SecondaryCellUpdate<'_>,
    ) -> Result<EncodedRrc, DedicatedError> {
        encode_downlink_dedicated(&self.value_with_secondary_cell(
            transaction,
            activation,
            secondary,
        )?)
        .map_err(DedicatedError::Codec)
    }

    pub(crate) fn value_with_secondary_cell(
        &self,
        transaction: TransactionId,
        activation: ScheduledActivation,
        secondary: SecondaryCellUpdate<'_>,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        if !self.uses_mac_ehs() {
            return Err(DedicatedError::Mapping);
        }
        if let SecondaryCellUpdate::Configure(configuration) = secondary {
            self.validate_secondary_configuration(configuration)?;
        }
        messages::reconfigure(transaction, self.body_r7(activation)?, secondary)
    }
}

impl InitialActivation {
    pub fn message_with_secondary_cell(
        &self,
        transaction: TransactionId,
        secondary: SecondaryCellUpdate<'_>,
    ) -> Result<EncodedRrc, DedicatedError> {
        self.configuration()
            .message_with_secondary_cell(transaction, self.activation(), secondary)
    }
}

pub fn secondary_cell_physical_reconfiguration(
    primary: &InitialConfiguration,
    transaction: TransactionId,
    activation: ScheduledActivation,
    secondary: SecondaryCellUpdate<'_>,
) -> Result<EncodedRrc, DedicatedError> {
    match secondary {
        SecondaryCellUpdate::Configure(configuration) => {
            primary.validate_secondary_configuration(configuration)?
        }
        SecondaryCellUpdate::Continue if primary.secondary_cell().is_none() => {
            return Err(DedicatedError::Physical);
        }
        SecondaryCellUpdate::Continue | SecondaryCellUpdate::Remove => (),
    }
    messages::physical(transaction, activation, secondary)
}
