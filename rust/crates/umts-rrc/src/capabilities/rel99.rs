// SPDX-License-Identifier: AGPL-3.0-or-later

use super::CapabilityReport;
use crate::{
    dedicated::{DchDirection, DchTfcs, DedicatedError, Rel99Configuration, Rel99Rlc},
    transport::ChannelCoding,
};
use umts_rrc_schema::{information_elements as ie, pdu_definitions as pdu};
use umts_types::time::CHIPS_PER_FRAME;

mod values;
const RLC_KBYTE_OCTETS: usize = 1024;
const DERATE_MATCHING_FACTOR_NUMERATOR: usize = 33;
const DERATE_MATCHING_FACTOR_DENOMINATOR: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    TransportBits,
    ConvolutionalBits,
    TurboBits,
    TransportBlocks,
    TransportChannels,
    Combinations,
    Formats,
    DownlinkCodes,
    DownlinkPhysicalBits,
    UplinkPhysicalBits,
    AmEntities,
    AmWindow,
    Sf512,
    DownlinkBeforeDeratematchingBits,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    MissingRadio,
    MissingFdd,
    Encoding,
    InvalidValue(&'static str),
    Configuration(DedicatedError),
    Exceeded {
        direction: Option<DchDirection>,
        limit: Limit,
        required: usize,
        supported: usize,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectionLimits {
    pub transport_bits: usize,
    pub convolutional_bits: usize,
    pub turbo_bits: Option<usize>,
    pub transport_blocks: usize,
    pub transport_channels: usize,
    pub combinations: usize,
    pub formats: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportUsage {
    pub bits: usize,
    pub convolutional_bits: usize,
    pub turbo_bits: usize,
    pub blocks: usize,
}

impl TransportUsage {
    pub fn checked_add(self, other: Self) -> Result<Self, Error> {
        let add = |a: usize, b| {
            a.checked_add(b)
                .ok_or(Error::InvalidValue("transport usage overflow"))
        };
        Ok(Self {
            bits: add(self.bits, other.bits)?,
            convolutional_bits: add(self.convolutional_bits, other.convolutional_bits)?,
            turbo_bits: add(self.turbo_bits, other.turbo_bits)?,
            blocks: add(self.blocks, other.blocks)?,
        })
    }

    /// Bits total the uncoded transport blocks, including MAC headers and excluding CRC.
    pub fn transport(coding: ChannelCoding, bits: usize, blocks: usize) -> Self {
        Self {
            bits,
            blocks,
            convolutional_bits: if matches!(coding, ChannelCoding::Convolutional(_)) {
                bits
            } else {
                0
            },
            turbo_bits: if matches!(coding, ChannelCoding::Turbo) {
                bits
            } else {
                0
            },
        }
    }
}

impl DirectionLimits {
    pub fn validate_usage(
        self,
        direction: DchDirection,
        usage: TransportUsage,
    ) -> Result<(), Error> {
        for (limit, required, supported) in [
            (Limit::TransportBits, usage.bits, self.transport_bits),
            (
                Limit::ConvolutionalBits,
                usage.convolutional_bits,
                self.convolutional_bits,
            ),
            (
                Limit::TurboBits,
                usage.turbo_bits,
                self.turbo_bits.unwrap_or(0),
            ),
            (Limit::TransportBlocks, usage.blocks, self.transport_blocks),
        ] {
            check(Some(direction), limit, required, supported)?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rel99Limits {
    pub uplink: DirectionLimits,
    pub downlink: DirectionLimits,
    pub downlink_codes: usize,
    pub downlink_physical_bits: usize,
    pub uplink_physical_bits: usize,
    pub sf512: bool,
    pub am_entities: usize,
    pub am_window: usize,
    pub am_buffer_bytes: usize,
}
impl Rel99Limits {
    pub fn from_report(report: &CapabilityReport) -> Result<Self, Error> {
        let information = report.information().map_err(|_| Error::Encoding)?;
        Self::from_information(&information)
    }
    pub fn from_information(information: &pdu::UECapabilityInformation) -> Result<Self, Error> {
        let radio = information
            .ue_radio_access_capability
            .as_ref()
            .ok_or(Error::MissingRadio)?;
        let physical = radio
            .physical_channel_capability
            .fdd_phys_ch_capability
            .as_ref()
            .ok_or(Error::MissingFdd)?;
        let dl = &radio.transport_channel_capability.dl_trans_ch_capability;
        let ul = &radio.transport_channel_capability.ul_trans_ch_capability;
        if !matches!(
            ul.mode_specific_info,
            ie::ULTransChCapabilityModeSpecificInfo::fdd(())
        ) {
            return Err(Error::MissingFdd);
        }
        if !(1..=8).contains(&physical.downlink_phys_ch_capability.max_no_dpch_pdsch_codes)
            || !(1..=8).contains(&dl.max_simultaneous_cctr_ch_count.0)
        {
            return Err(Error::InvalidValue("physical/CCTrCH count"));
        }
        let r5 = information
            .v370_non_critical_extensions
            .as_ref()
            .and_then(|v| v.v380_non_critical_extensions.as_ref())
            .and_then(|v| v.v3a0_non_critical_extensions.as_ref())
            .and_then(|v| v.later_non_critical_extensions.as_ref())
            .and_then(|v| v.v4b0_non_critical_extensions.as_ref())
            .and_then(|v| v.v590_non_critical_extensions.as_ref())
            .and_then(|v| {
                v.ue_capability_information_v590ext
                    .ue_radio_access_capability_v590ext
                    .as_ref()
            });
        let am_buffer = match r5.and_then(|v| v.rlc_capability_r5_ext.total_rlc_am_buffer_size) {
            Some(buffer) => values::buffer_ext(buffer)?,
            None => values::buffer(radio.rlc_capability.total_rlc_am_buffer_size)?,
        };
        Ok(Self {
            uplink: DirectionLimits {
                transport_bits: values::bits(ul.max_no_bits_transmitted)?,
                convolutional_bits: values::bits(ul.max_conv_code_bits_transmitted)?,
                turbo_bits: values::turbo(&ul.turbo_encoding_support)?,
                transport_blocks: values::blocks_ul(ul.max_transmitted_blocks)?,
                transport_channels: values::channels_ul(ul.max_simultaneous_trans_chs)?,
                combinations: values::combinations_ul(ul.max_number_of_tfc)?,
                formats: values::formats(ul.max_number_of_tf)?,
            },
            downlink: DirectionLimits {
                transport_bits: values::bits(dl.max_no_bits_received)?,
                convolutional_bits: values::bits(dl.max_conv_code_bits_received)?,
                turbo_bits: values::turbo(&dl.turbo_decoding_support)?,
                transport_blocks: values::blocks_dl(dl.max_received_transport_blocks)?,
                transport_channels: values::channels_dl(dl.max_simultaneous_trans_chs)?,
                combinations: values::combinations_dl(dl.max_number_of_tfc)?,
                formats: values::formats(dl.max_number_of_tf)?,
            },
            downlink_codes: usize::from(
                physical.downlink_phys_ch_capability.max_no_dpch_pdsch_codes,
            ),
            downlink_physical_bits: values::physical_dl(
                physical
                    .downlink_phys_ch_capability
                    .max_no_phys_ch_bits_received,
            )?,
            uplink_physical_bits: values::physical_ul(
                physical
                    .uplink_phys_ch_capability
                    .max_no_dpdch_bits_transmitted,
            )?,
            sf512: physical.downlink_phys_ch_capability.support_for_sf_512,
            am_entities: values::entities(radio.rlc_capability.maximum_am_entity_number)?,
            am_window: values::window(radio.rlc_capability.maximum_rlc_window_size)?,
            am_buffer_bytes: am_buffer
                .checked_mul(RLC_KBYTE_OCTETS)
                .ok_or(Error::InvalidValue("RLC buffer"))?,
        })
    }
    /// TS 25.306 4.5.1 limits matched bits before DTX to 6.6 times the reported TB-bit capability.
    pub fn validate_downlink_matched_bits(&self, bits: usize) -> Result<(), Error> {
        let supported = self
            .downlink
            .transport_bits
            .checked_mul(DERATE_MATCHING_FACTOR_NUMERATOR)
            .ok_or(Error::InvalidValue("de-rate-matching bound"))?
            / DERATE_MATCHING_FACTOR_DENOMINATOR;
        check(
            Some(DchDirection::Downlink),
            Limit::DownlinkBeforeDeratematchingBits,
            bits,
            supported,
        )
    }
    /// This checks the dedicated configuration alone, not additional simultaneous common/HSPA load.
    pub fn validate_dch(&self, configuration: &Rel99Configuration) -> Result<(), Error> {
        self.direction(configuration, configuration.uplink(), self.uplink)?;
        self.direction(configuration, configuration.downlink(), self.downlink)?;
        let physical = configuration.physical();
        check(
            None,
            Limit::DownlinkCodes,
            physical.downlink_codes.len(),
            self.downlink_codes,
        )?;
        check(
            None,
            Limit::DownlinkPhysicalBits,
            physical.downlink_codes.len() * 2 * CHIPS_PER_FRAME as usize
                / usize::from(physical.downlink_sf),
            self.downlink_physical_bits,
        )?;
        check(
            None,
            Limit::UplinkPhysicalBits,
            usize::from(physical.uplink_dpdch_count) * CHIPS_PER_FRAME as usize
                / usize::from(physical.uplink_minimum_sf),
            self.uplink_physical_bits,
        )?;
        if physical.downlink_sf == 512 && !self.sf512 {
            return Err(Error::Exceeded {
                direction: Some(DchDirection::Downlink),
                limit: Limit::Sf512,
                required: 1,
                supported: 0,
            });
        }
        let mut entities = 0;
        for bearer in configuration.bearers() {
            if let Rel99Rlc::Acknowledged { uplink, downlink } = &bearer.rlc {
                entities += 1;
                check(
                    None,
                    Limit::AmWindow,
                    values::tx_window(uplink.transmission_window_size)?,
                    self.am_window,
                )?;
                check(
                    None,
                    Limit::AmWindow,
                    values::rx_window(downlink.receiving_window_size)?,
                    self.am_window,
                )?;
            }
        }
        // TS 25.306 4.3 shares the AM buffer dynamically, it is not a sum of every maximum window.
        check(None, Limit::AmEntities, entities, self.am_entities)
    }
    fn direction(
        &self,
        configuration: &Rel99Configuration,
        tfcs: &DchTfcs,
        limits: DirectionLimits,
    ) -> Result<(), Error> {
        let direction = Some(tfcs.direction());
        check(
            direction,
            Limit::TransportChannels,
            tfcs.transports().len(),
            limits.transport_channels,
        )?;
        check(
            direction,
            Limit::Combinations,
            tfcs.combinations().len(),
            limits.combinations,
        )?;
        let formats = tfcs
            .transports()
            .iter()
            .map(|transport| transport.formats().count())
            .sum();
        check(direction, Limit::Formats, formats, limits.formats)?;
        for row in tfcs.combinations() {
            let mut usage = TransportUsage::default();
            for (transport, format) in tfcs.transports().iter().zip(&row.formats) {
                let format = transport
                    .formats()
                    .nth(*format)
                    .ok_or(Error::Configuration(DedicatedError::Combination))?;
                let bits = format.transport_bits(
                    configuration
                        .multiplexed(tfcs.direction(), transport.id)
                        .map_err(Error::Configuration)?,
                ) * usize::from(format.blocks);
                usage = usage.checked_add(TransportUsage::transport(
                    transport.coding,
                    bits,
                    usize::from(format.blocks),
                ))?;
            }
            // TS 25.306 4.5.1/4.5.2 counts TB bits, not encoded FEC bits or bitrate divided by TTI.
            limits.validate_usage(tfcs.direction(), usage)?;
        }
        Ok(())
    }
}
fn check(
    direction: Option<DchDirection>,
    limit: Limit,
    required: usize,
    supported: usize,
) -> Result<(), Error> {
    if required > supported {
        Err(Error::Exceeded {
            direction,
            limit,
            required,
            supported,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
