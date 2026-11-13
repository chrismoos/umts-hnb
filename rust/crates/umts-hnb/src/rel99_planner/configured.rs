// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    Coding, Error, PlannedExisting, PlannedInitial, Policy, Rab, RabPlan, SignallingProfile,
    plan_existing_for_ue, plan_initial_for_ue,
};
use crate::signalling::CommonSignalling;
use anyhow::{Context, Result, anyhow, ensure};
use std::num::{NonZeroU8, NonZeroUsize};
use umts_config::{Config, R99Coding, R99CodingKind, R99TpcAlgorithm};
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rrc::{
    dedicated::{GainFactors, NormalDpch, UplinkPowerControl},
    transport::{ChannelCoding, ConvolutionalRate, CrcSize, TransportInterval},
};
use umts_types::UeKey;

const CHIPS_PER_FRAME: u32 = 38_400;
const DOFF_QUANTUM_CHIPS: u32 = 512;
const FRAME_OFFSET_QUANTUM_CHIPS: u32 = 256;

pub struct ConfiguredPolicy {
    rel99_packet_allowed: bool,
    hsdpa: bool,
    hsupa: bool,
    services: super::ConfiguredServices,
    signalling: SignallingProfile,
    physical: Vec<NormalDpch>,
    uplink_gains: GainFactors,
    maximum_combinations: NonZeroUsize,
    maximum_total_coded_bits: NonZeroUsize,
    maximum_user_storage_bytes: NonZeroUsize,
    transport: TransportPlanLimits,
}

impl ConfiguredPolicy {
    pub fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        let input = &config.rel99.physical.planner;
        let signalling = SignallingProfile {
            uplink: coding(&input.signalling.uplink)?,
            downlink: coding(&input.signalling.downlink)?,
            interval: interval(input.signalling.interval_ms)?,
            maximum_blocks: input.signalling.maximum_blocks,
            um_uplink_bits: input.signalling.um_uplink_bits,
            um_downlink_bits: input.signalling.um_downlink_bits,
        };
        let physical = input
            .physical_candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let physical = NormalDpch {
                    primary_scrambling: config.cell.physical.downlink_scrambling_code,
                    secondary_scrambling: None,
                    uplink_scrambling: config.rel99.physical.uplink_scrambling_code_base,
                    uplink_minimum_sf: candidate.uplink_minimum_sf,
                    uplink_dpdch_count: candidate.uplink_dpdch_count,
                    uplink_tfci: candidate.uplink_tfci,
                    puncturing_percent: config.rel99.physical.initial_ul_puncturing_limit_percent,
                    dpcch_offset_db: candidate.dpcch_offset_db,
                    pc_preamble_frames: candidate.pc_preamble_frames,
                    srb_delay_frames: candidate.srb_delay_frames,
                    power_control: match candidate.tpc_algorithm {
                        R99TpcAlgorithm::Algorithm1 => UplinkPowerControl::Algorithm1 {
                            step_db: candidate.tpc_step_db,
                        },
                        R99TpcAlgorithm::Algorithm2 => UplinkPowerControl::Algorithm2,
                    },
                    maximum_uplink_dbm: config.rrc.max_allowed_ul_tx_power_dbm,
                    downlink_sf: candidate.downlink_sf,
                    downlink_pilot_bits: candidate.downlink_pilot_bits,
                    downlink_tfci: candidate.downlink_tfci,
                    // RadioResources replaces these template indices with cell-admitted per-UE codes.
                    downlink_codes: (0..u16::from(candidate.downlink_code_count)).collect(),
                    flexible_position: candidate.flexible_position,
                    pilot_offset_quarter_db: candidate.pilot_offset_quarter_db,
                    default_offset_512_chips: candidate.default_offset_512_chips,
                    // TS 25.331 8.6.6.14 binds normal DPCH frame offset to DOFF modulo one frame.
                    frame_offset_256_chips: u8::try_from(
                        (u32::from(candidate.default_offset_512_chips) * DOFF_QUANTUM_CHIPS
                            % CHIPS_PER_FRAME)
                            / FRAME_OFFSET_QUANTUM_CHIPS,
                    )
                    .expect("frame offset modulo one frame fits u8"),
                };
                physical.validate().map_err(|error| {
                    anyhow!("rel99.physical.planner.physical_candidates[{index}]: {error:?}")
                })?;
                Ok(physical)
            })
            .collect::<Result<Vec<_>>>()?;
        let budgets = &input.budgets;
        let iterations = u8::try_from(config.receiver.turbo_iterations)
            .context("receiver.turbo_iterations must fit the decoder budget")?;
        let decode = DecodeBudget::new(
            NonZeroU8::new(iterations)
                .ok_or_else(|| anyhow!("receiver.turbo_iterations must be nonzero"))?,
        )
        .map_err(|error| anyhow!("receiver.turbo_iterations: {error:?}"))?;
        Ok(Self {
            rel99_packet_allowed: config
                .cell
                .packet_modes
                .as_ref()
                .is_none_or(|modes| modes.contains(&umts_config::PacketMode::Rel99)),
            hsdpa: config.hspa.downlink.enabled,
            hsupa: config.hspa.downlink.enabled
                && config.hspa.uplink.enabled
                && config.hspa.uplink.scheduled,
            services: super::ConfiguredServices::with_high_speed_downlink(
                &input.services,
                count(config.runtime.maximum_sdu_bytes).context("runtime.maximum_sdu_bytes")?,
                config
                    .hspa
                    .downlink
                    .enabled
                    .then_some(super::HighSpeedDownlink {
                        rlc_bits: config.hspa.downlink.rlc_pdu_bits,
                        receive_window: config.hspa.downlink.rlc_receive_window,
                    }),
            )?,
            signalling,
            physical,
            uplink_gains: GainFactors {
                control: input.uplink_control_gain,
                data: input.uplink_data_gain,
            },
            maximum_combinations: count(u32::from(budgets.maximum_combinations))
                .context("rel99.physical.planner.budgets.maximum_combinations")?,
            maximum_total_coded_bits: count(budgets.maximum_total_coded_bits)
                .context("rel99.physical.planner.budgets.maximum_total_coded_bits")?,
            maximum_user_storage_bytes: count(budgets.maximum_user_storage_bytes)
                .context("rel99.physical.planner.budgets.maximum_user_storage_bytes")?,
            transport: TransportPlanLimits {
                maximum_coded_bits: count(budgets.maximum_transport_coded_bits)
                    .context("rel99.physical.planner.budgets.maximum_transport_coded_bits")?,
                maximum_mapping_entries: count(budgets.maximum_mapping_entries)
                    .context("rel99.physical.planner.budgets.maximum_mapping_entries")?,
                decode,
            },
        })
    }

    pub fn validate_rel99_packet_setup(&self, domain: umts_types::CnDomain) -> Result<()> {
        ensure!(
            domain != umts_types::CnDomain::Ps || self.rel99_packet_allowed,
            "cell.packet_modes excludes Rel99 packet setup on this transition"
        );
        Ok(())
    }

    pub fn policy(&self) -> Policy<'_> {
        Policy {
            hsdpa: self.hsdpa,
            hsupa: self.hsupa,
            signalling: self.signalling,
            physical: &self.physical,
            uplink_gains: self.uplink_gains,
            maximum_combinations: self.maximum_combinations,
            maximum_total_coded_bits: self.maximum_total_coded_bits,
            maximum_user_storage_bytes: self.maximum_user_storage_bytes,
            transport: self.transport,
        }
    }

    pub fn services(&self) -> &super::ConfiguredServices {
        &self.services
    }

    #[cfg(test)]
    pub fn plan_initial(
        &self,
        key: UeKey,
        signalling: &CommonSignalling,
        rabs: &[Rab<'_>],
    ) -> Result<PlannedInitial, Error> {
        plan_initial_for_ue(key, signalling, rabs, &self.policy())
    }

    pub fn plan_initial_with_resources(
        &self,
        key: UeKey,
        signalling: &CommonSignalling,
        rabs: &[Rab<'_>],
        resources: &crate::radio_resources::RadioResources,
    ) -> Result<PlannedInitial, Error> {
        let mut failure = Error::Capacity;
        let mut secondary = None;
        let report = signalling.capabilities(key).ok_or(Error::Resources(
            crate::radio_resources::Error::MissingCapabilities,
        ))?;
        let mut candidates: Vec<_> = self.physical.iter().collect();
        if !super::simultaneous::hsdpa(report, &self.policy())
            && rabs
                .iter()
                .any(|rab| rab.domain == umts_types::CnDomain::Ps)
        {
            candidates.sort_by_key(|candidate| candidate.downlink_sf);
        }
        for physical in candidates {
            let policy = Policy {
                physical: std::slice::from_ref(physical),
                ..self.policy()
            };
            let candidate = plan_initial_for_ue(key, signalling, rabs, &policy).and_then(|plan| {
                resources
                    .check_downlink_space(plan.setup().configuration().physical())
                    .map_err(Error::Resources)?;
                Ok(plan)
            });
            match candidate {
                Ok(plan) => {
                    if resources
                        .check_primary_downlink_space(plan.setup().configuration().physical())
                        .is_ok()
                    {
                        return Ok(plan);
                    }
                    secondary.get_or_insert(plan);
                }
                Err(error) => failure = error,
            }
        }
        secondary.ok_or(failure)
    }

    pub fn plan_existing(
        &self,
        active: &crate::rel99_admission::ActiveRadio<'_>,
        signalling: &CommonSignalling,
        active_rabs: &[RabPlan],
        rabs: &[Rab<'_>],
    ) -> Result<PlannedExisting, Error> {
        plan_existing_for_ue(active, signalling, active_rabs, rabs, &self.policy())
    }
}

fn count(value: u32) -> Result<NonZeroUsize> {
    usize::try_from(value)
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| anyhow!("budget must be nonzero and fit the host address space"))
}

pub(super) fn interval(value: u8) -> Result<TransportInterval> {
    match value {
        10 => Ok(TransportInterval::Ms10),
        20 => Ok(TransportInterval::Ms20),
        40 => Ok(TransportInterval::Ms40),
        80 => Ok(TransportInterval::Ms80),
        _ => Err(anyhow!(
            "rel99.physical.planner.signalling.interval_ms must be 10, 20, 40 or 80"
        )),
    }
}

pub(super) fn coding(config: &R99Coding) -> Result<Coding> {
    Ok(Coding {
        channel: match config.kind {
            R99CodingKind::ConvolutionalHalf => {
                ChannelCoding::Convolutional(ConvolutionalRate::Half)
            }
            R99CodingKind::ConvolutionalThird => {
                ChannelCoding::Convolutional(ConvolutionalRate::Third)
            }
            R99CodingKind::Turbo => ChannelCoding::Turbo,
        },
        crc: match config.crc_bits {
            0 => CrcSize::None,
            8 => CrcSize::Bits8,
            12 => CrcSize::Bits12,
            16 => CrcSize::Bits16,
            24 => CrcSize::Bits24,
            _ => return Err(anyhow!("crc_bits must be 0, 8, 12, 16 or 24")),
        },
        rate_matching_attribute: config.rate_matching_attribute,
    })
}

#[cfg(test)]
mod tests;
