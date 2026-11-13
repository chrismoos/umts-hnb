// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_plan::{coded_format, transport_tti, uplink_layouts};
use std::num::NonZeroU16;
use umts_dsp::{
    coding::CodedBlockPlan,
    dedicated::UplinkLayout,
    dedicated_transport::{PuncturingLimit, TransportId, UplinkAllocation, UplinkRateInput},
};

pub(super) struct DownlinkBudget {
    physical: umts_dsp::dedicated::DownlinkFormat,
    channels: NonZeroUsize,
    flexible: bool,
    limits: TransportPlanLimits,
}

impl DownlinkBudget {
    pub(super) fn new(physical: &NormalDpch, limits: TransportPlanLimits) -> Result<Self, Error> {
        let format = (0..=16)
            .find_map(|number| {
                umts_dsp::dedicated::DownlinkFormat::new(number, physical.downlink_tfci)
                    .ok()
                    .filter(|format| {
                        format.spreading_factor() == physical.downlink_sf
                            && format.pilots().bits() == usize::from(physical.downlink_pilot_bits)
                    })
            })
            .ok_or_else(|| crate::rel99_planner::trace_identity("physical-1"))?;
        Ok(Self {
            physical: format,
            channels: NonZeroUsize::new(physical.downlink_codes.len())
                .ok_or_else(|| crate::rel99_planner::trace_identity("physical-2"))?,
            flexible: physical.flexible_position,
            limits,
        })
    }

    pub(super) fn check(
        &self,
        composition: &Composition,
        rows: &[ProductRow],
    ) -> Result<(), Error> {
        let tfcs = composition.assemble(DchDirection::Downlink, rows.to_vec())?;
        let configured =
            crate::rel99_plan::compile_tfcs(&tfcs, self.limits.maximum_coded_bits, |id| {
                Ok(composition.multiplexed(DchDirection::Downlink, id))
            })
            .map_err(Error::Plan)?;
        use umts_dsp::dedicated_transport::{FixedDownlinkTfcs, FlexibleDownlinkTfcs};
        let result = if self.flexible {
            FlexibleDownlinkTfcs::validate_rate_matching(
                &configured,
                self.physical,
                self.channels,
                self.limits,
            )
        } else {
            FixedDownlinkTfcs::new(
                Arc::new(configured),
                self.physical,
                self.channels,
                self.limits,
            )
            .map(|_| ())
        };
        result.map_err(|error| Error::Plan(Rel99PlanError::Downlink(error)))
    }
}

pub(super) struct UplinkBudget {
    layouts: Vec<UplinkLayout>,
    puncturing: PuncturingLimit,
    maximum: NonZeroUsize,
}

impl UplinkBudget {
    pub(super) fn new(physical: &NormalDpch, maximum: NonZeroUsize) -> Result<Self, Error> {
        Ok(Self {
            layouts: uplink_layouts(physical)
                .map_err(|error| Error::Plan(Rel99PlanError::Uplink(error)))?,
            puncturing: PuncturingLimit::new(physical.puncturing_percent)
                .map_err(|error| Error::Plan(Rel99PlanError::Uplink(error)))?,
            maximum,
        })
    }

    pub(super) fn input(
        &self,
        channel: &DchTransport,
        index: usize,
        multiplexed: bool,
    ) -> Result<UplinkRateInput, Error> {
        let format = channel
            .formats()
            .nth(index)
            .ok_or_else(|| crate::rel99_planner::trace_identity("physical-3"))?;
        let plan = CodedBlockPlan::new(coded_format(channel, format, multiplexed), self.maximum)
            .map_err(|error| Error::Plan(Rel99PlanError::Uplink(error)))?;
        Ok(UplinkRateInput {
            id: TransportId::new(channel.id)
                .map_err(|error| Error::Plan(Rel99PlanError::Uplink(error)))?,
            frame_bits: plan
                .coded_bits()
                .div_ceil(transport_tti(channel.interval).frames()),
            attribute: NonZeroU16::new(channel.rate_matching_attribute)
                .ok_or_else(|| crate::rel99_planner::trace_identity("physical-4"))?,
        })
    }

    pub(super) fn check(&self, mut inputs: Vec<UplinkRateInput>) -> Result<(), Error> {
        inputs.sort_unstable_by_key(|input| input.id.get());
        UplinkAllocation::new(&inputs, &self.layouts, self.puncturing)
            .map(|_| ())
            .map_err(|error| Error::Plan(Rel99PlanError::Uplink(error)))
    }
}
