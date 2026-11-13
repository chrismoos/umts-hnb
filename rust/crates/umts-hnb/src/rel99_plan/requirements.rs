// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CompiledRel99, DownlinkPlans, Rel99PlanError};
use crate::dedicated_bearers::{FramePoolLimits, storage_requirements};
use crate::dedicated_rx::{DedicatedReceiveSession, maximum_physical_bits};
use std::num::NonZeroUsize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanRequirements {
    pub maximum_uplink_transport_output_bits: usize,
    pub pending_uplink_decode_storage_bytes: usize,
    pub maximum_downlink_transport_output_bits: usize,
    pub maximum_uplink_demultiplexed_bits: usize,
    pub maximum_uplink_physical_bits: usize,
    pub maximum_downlink_matched_tti_bits: usize,
    pub uplink_coded_bits: usize,
    pub downlink_coded_bits: usize,
    pub downlink_channels: usize,
    pub downlink_frame_payload_bits: usize,
}

impl CompiledRel99 {
    pub fn requirements(&self) -> Result<PlanRequirements, Rel99PlanError> {
        let maximum_uplink_transport_output_bits =
            maximum_transport_output_bits(self.uplink_configured())?;
        let maximum_downlink_transport_output_bits =
            maximum_transport_output_bits(self.downlink_configured())?;
        let maximum_uplink_demultiplexed_bits =
            self.uplink().configured().combinations().iter().try_fold(
                0usize,
                |maximum, combination| {
                    let allocation = self
                        .uplink()
                        .allocation(combination.tfci)
                        .ok_or(Rel99PlanError::Capacity)?;
                    Ok::<usize, Rel99PlanError>(maximum.max(allocation.total_bits()))
                },
            )?;
        let maximum_uplink_physical_bits =
            maximum_physical_bits(self.uplink()).ok_or(Rel99PlanError::Capacity)?;

        let (downlink_frame_payload_bits, downlink_channels) = match self.downlink() {
            DownlinkPlans::Fixed(plan) => (
                plan.frame_capacity(),
                self.configuration.physical().downlink_codes.len(),
            ),
            DownlinkPlans::Flexible(plan) => (
                plan.frame_capacity(),
                self.configuration.physical().downlink_codes.len(),
            ),
        };
        Ok(PlanRequirements {
            maximum_uplink_transport_output_bits,
            pending_uplink_decode_storage_bytes:
                DedicatedReceiveSession::pending_output_storage_bytes(self.uplink_configured())
                    .map_err(|_| Rel99PlanError::Capacity)?,
            maximum_downlink_transport_output_bits,
            maximum_uplink_demultiplexed_bits,
            maximum_uplink_physical_bits,
            maximum_downlink_matched_tti_bits: self.maximum_downlink_matched_tti_bits()?,
            uplink_coded_bits: self.uplink_configured().total_coded_bits(),
            downlink_coded_bits: self.downlink_configured().total_coded_bits(),
            downlink_channels,
            downlink_frame_payload_bits,
        })
    }
}

impl PlanRequirements {
    pub fn receive_output_storage_bytes(
        self,
        output_slots: NonZeroUsize,
    ) -> Result<usize, Rel99PlanError> {
        DedicatedReceiveSession::output_storage_bytes(
            output_slots,
            self.maximum_uplink_transport_output_bits,
        )
        .map_err(|_| Rel99PlanError::Capacity)
        .and_then(|bytes| {
            bytes
                .checked_add(self.pending_uplink_decode_storage_bytes)
                .ok_or(Rel99PlanError::Capacity)
        })
    }

    pub fn transmit_frame_window_bits(
        self,
        frame_slots: NonZeroUsize,
    ) -> Result<usize, Rel99PlanError> {
        self.downlink_frame_payload_bits
            .checked_mul(frame_slots.get())
            .and_then(|bits| bits.checked_add(self.downlink_frame_payload_bits))
            .ok_or(Rel99PlanError::Capacity)
    }
}

impl CompiledRel99 {
    pub fn frame_pool_storage_bytes(
        &self,
        limits: FramePoolLimits,
    ) -> Result<usize, Rel99PlanError> {
        storage_requirements(self, limits)
            .map(|(_, bytes)| bytes)
            .map_err(|_| Rel99PlanError::Capacity)
    }
}

fn maximum_transport_output_bits(
    configured: &umts_dsp::dedicated_transport::ConfiguredTfcs,
) -> Result<usize, Rel99PlanError> {
    configured.transport_ids().try_fold(0usize, |maximum, id| {
        let definition = configured.definition(id).ok_or(Rel99PlanError::Capacity)?;
        let channel_maximum = definition
            .formats
            .iter()
            .try_fold(0usize, |maximum, format| {
                format
                    .block_bits
                    .checked_mul(format.blocks)
                    .map(|bits| maximum.max(bits))
                    .ok_or(Rel99PlanError::Capacity)
            })?;
        Ok(maximum.max(channel_maximum))
    })
}
