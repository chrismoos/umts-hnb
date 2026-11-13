// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    num::{NonZeroU16, NonZeroUsize},
    sync::Arc,
};
use umts_dsp::{
    DspError,
    coding::{ChannelCoding, CodedBlockFormat},
    convolutional::CodeRate,
    crc::TransportCrc,
    dedicated::{DownlinkFormat, UplinkControlFormat, UplinkGains, UplinkLayout},
    dedicated_transport::{
        CombinationDefinition, ConfiguredTfcs, FixedDownlinkTfcs, FlexibleDownlinkTfcs,
        PuncturingLimit, TransportDefinition, TransportId, TransportPlanLimits, UplinkTfcs,
    },
    interleaving::Rel99Tti,
    tfci::Tfci,
};
use umts_rrc::{
    dedicated::{DchFormat, DchTfcs, DchTransport, DedicatedError, NormalDpch, Rel99Configuration},
    transport,
};
mod requirements;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub use requirements::PlanRequirements;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rel99PlanError {
    Configuration(DedicatedError),
    Dsp(DspError),
    Uplink(DspError),
    Downlink(DspError),
    Capacity,
}
impl From<DspError> for Rel99PlanError {
    fn from(error: DspError) -> Self {
        Self::Dsp(error)
    }
}
impl From<DedicatedError> for Rel99PlanError {
    fn from(error: DedicatedError) -> Self {
        Self::Configuration(error)
    }
}
pub enum DownlinkPlans {
    Fixed(FixedDownlinkTfcs),
    Flexible(FlexibleDownlinkTfcs),
}
pub struct CompiledRel99 {
    configuration: Arc<Rel99Configuration>,
    uplink: Arc<UplinkTfcs>,
    uplink_configured: Arc<ConfiguredTfcs>,
    downlink_configured: Arc<ConfiguredTfcs>,
    downlink: DownlinkPlans,
    uplink_control: UplinkControlFormat,
    gains: Vec<UplinkGains>,
}
impl CompiledRel99 {
    /// TS 25.306 4.5.1 counts selected matched TTI bits before UE de-rate-matching, excluding DTX.
    pub fn maximum_downlink_matched_tti_bits(&self) -> Result<usize, Rel99PlanError> {
        let mut maximum = 0;
        for combination in self.downlink_configured.combinations() {
            let mut total = 0usize;
            for (id, format) in self
                .downlink_configured
                .transport_ids()
                .zip(&combination.formats)
            {
                let bits = match &self.downlink {
                    DownlinkPlans::Fixed(plan) => plan.matched_tti_bits(id, *format),
                    DownlinkPlans::Flexible(plan) => plan.matched_tti_bits(id, *format),
                }
                .ok_or(Rel99PlanError::Capacity)?;
                total = total.checked_add(bits).ok_or(Rel99PlanError::Capacity)?;
            }
            maximum = maximum.max(total);
        }
        Ok(maximum)
    }
    pub fn configuration(&self) -> &Arc<Rel99Configuration> {
        &self.configuration
    }
    pub fn uplink(&self) -> &Arc<UplinkTfcs> {
        &self.uplink
    }
    pub fn uplink_configured(&self) -> &Arc<ConfiguredTfcs> {
        &self.uplink_configured
    }
    pub fn downlink_configured(&self) -> &Arc<ConfiguredTfcs> {
        &self.downlink_configured
    }
    pub fn downlink(&self) -> &DownlinkPlans {
        &self.downlink
    }
    pub fn uplink_control(&self) -> UplinkControlFormat {
        self.uplink_control
    }
    pub fn new(
        configuration: Arc<Rel99Configuration>,
        maximum_total_coded_bits: NonZeroUsize,
        limits: TransportPlanLimits,
    ) -> Result<Self, Rel99PlanError> {
        let uplink_configured = Arc::new(compile(
            &configuration,
            configuration.uplink(),
            maximum_total_coded_bits,
        )?);
        let downlink_configured = Arc::new(compile(
            &configuration,
            configuration.downlink(),
            maximum_total_coded_bits,
        )?);
        if uplink_configured
            .total_coded_bits()
            .checked_add(downlink_configured.total_coded_bits())
            .is_none_or(|total| total > maximum_total_coded_bits.get())
        {
            return Err(Rel99PlanError::Capacity);
        }
        let physical = configuration.physical();
        let layouts = uplink_layouts(physical)?;
        let uplink = Arc::new(
            UplinkTfcs::new(
                uplink_configured.clone(),
                &layouts,
                PuncturingLimit::new(physical.puncturing_percent)?,
                limits,
            )
            .map_err(Rel99PlanError::Uplink)?,
        );
        let downlink_format = (0..=16)
            .find_map(|number| {
                DownlinkFormat::new(number, physical.downlink_tfci)
                    .ok()
                    .filter(|format| {
                        format.spreading_factor() == physical.downlink_sf
                            && format.pilots().bits() == usize::from(physical.downlink_pilot_bits)
                    })
            })
            .ok_or(DspError::InvalidCode)?;
        let channels =
            NonZeroUsize::new(physical.downlink_codes.len()).ok_or(Rel99PlanError::Capacity)?;
        let downlink = if physical.flexible_position {
            DownlinkPlans::Flexible(
                FlexibleDownlinkTfcs::new(
                    downlink_configured.clone(),
                    downlink_format,
                    channels,
                    limits,
                )
                .map_err(Rel99PlanError::Downlink)?,
            )
        } else {
            DownlinkPlans::Fixed(
                FixedDownlinkTfcs::new(
                    downlink_configured.clone(),
                    downlink_format,
                    channels,
                    limits,
                )
                .map_err(Rel99PlanError::Downlink)?,
            )
        };
        let uplink_control = UplinkControlFormat::new(if physical.uplink_tfci { 0 } else { 1 })?;
        let gains = configuration
            .uplink()
            .combinations()
            .iter()
            .map(|combination| {
                let gain = combination
                    .gain
                    .ok_or(Rel99PlanError::Configuration(DedicatedError::Gains))?;
                UplinkGains::new(gain.control, gain.data).map_err(Rel99PlanError::Dsp)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            configuration,
            uplink,
            uplink_configured,
            downlink_configured,
            downlink,
            uplink_control,
            gains,
        })
    }
    pub fn uplink_gains(&self, tfci: Tfci) -> Option<UplinkGains> {
        self.gains.get(usize::from(tfci.index())).copied()
    }
}
fn compile(
    configuration: &Rel99Configuration,
    tfcs: &DchTfcs,
    maximum: NonZeroUsize,
) -> Result<ConfiguredTfcs, Rel99PlanError> {
    compile_tfcs(tfcs, maximum, |id| {
        configuration
            .multiplexed(tfcs.direction(), id)
            .map_err(Into::into)
    })
}

pub(crate) fn compile_tfcs(
    tfcs: &DchTfcs,
    maximum: NonZeroUsize,
    multiplexed: impl Fn(u8) -> Result<bool, Rel99PlanError>,
) -> Result<ConfiguredTfcs, Rel99PlanError> {
    let mut definitions = Vec::with_capacity(tfcs.transports().len());
    for channel in tfcs.transports() {
        let multiplexed = multiplexed(channel.id)?;
        definitions.push(TransportDefinition {
            id: TransportId::new(channel.id)?,
            interval: transport_tti(channel.interval),
            attribute: NonZeroU16::new(channel.rate_matching_attribute)
                .ok_or(Rel99PlanError::Capacity)?,
            formats: channel
                .formats()
                .map(|format| coded_format(channel, format, multiplexed))
                .collect(),
        });
    }
    let combinations = tfcs
        .combinations()
        .iter()
        .enumerate()
        .map(|(index, row)| {
            Ok(CombinationDefinition {
                tfci: Tfci::new(index as u16)?,
                formats: row.formats.clone(),
            })
        })
        .collect::<Result<_, DspError>>()?;
    ConfiguredTfcs::new(definitions, combinations, maximum).map_err(Rel99PlanError::Dsp)
}

pub(crate) fn coded_format(
    channel: &DchTransport,
    format: DchFormat,
    multiplexed: bool,
) -> CodedBlockFormat {
    let coding = match channel.coding {
        transport::ChannelCoding::Convolutional(transport::ConvolutionalRate::Half) => {
            ChannelCoding::Convolutional(CodeRate::Half)
        }
        transport::ChannelCoding::Convolutional(transport::ConvolutionalRate::Third) => {
            ChannelCoding::Convolutional(CodeRate::Third)
        }
        transport::ChannelCoding::Turbo => ChannelCoding::Turbo,
    };
    let crc = match channel.crc {
        transport::CrcSize::None => TransportCrc::None,
        transport::CrcSize::Bits8 => TransportCrc::Crc8,
        transport::CrcSize::Bits12 => TransportCrc::Crc12,
        transport::CrcSize::Bits16 => TransportCrc::Crc16,
        transport::CrcSize::Bits24 => TransportCrc::Crc24,
    };
    CodedBlockFormat {
        block_bits: format.transport_bits(multiplexed),
        blocks: usize::from(format.blocks),
        crc,
        coding,
    }
}

pub(crate) fn transport_tti(interval: transport::TransportInterval) -> Rel99Tti {
    match interval {
        transport::TransportInterval::Ms10 => Rel99Tti::Ms10,
        transport::TransportInterval::Ms20 => Rel99Tti::Ms20,
        transport::TransportInterval::Ms40 => Rel99Tti::Ms40,
        transport::TransportInterval::Ms80 => Rel99Tti::Ms80,
    }
}

pub(crate) fn uplink_layouts(physical: &NormalDpch) -> Result<Vec<UplinkLayout>, DspError> {
    let mut layouts = Vec::new();
    for sf in [256, 128, 64, 32, 16, 8, 4] {
        if sf >= physical.uplink_minimum_sf {
            layouts.push(UplinkLayout::single(sf)?);
        }
    }
    for count in 2..=physical.uplink_dpdch_count {
        layouts.push(UplinkLayout::multicode(usize::from(count))?);
    }
    Ok(layouts)
}
