// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_rx::CommonRx,
    controller::CommonController,
    dedicated_controller::{DedicatedController, DedicatedControllerLimits},
    dedicated_rx::{ReceiveResources, SampleRuntimeLimits, ShardLimits},
    dedicated_tx::{DownlinkControl, TransmitRuntimeLimits},
    radio_io::TxPreparation,
    rel99_admission::{AdmissionLimits, Rel99Admission},
};
use std::num::NonZeroUsize;
use umts_dsp::pulse_shaping::SamplesPerChip;
use umts_types::time::CHIPS_PER_FRAME;

const CONFIGURATIONS_PER_UE: usize = 2;
const MAXIMUM_DOWNLINK_CODES_PER_CONFIGURATION: usize = 8;
const MAXIMUM_TRANSPORTS_PER_CONFIGURATION: usize = 32;
const PREFILL_FRAMES: usize = 8;
const LAUNCH_LOOKAHEAD_FRAMES: usize = 8;
const HEALTH_INTERVAL_CHIPS: u64 = CHIPS_PER_FRAME;

#[derive(Clone, Copy)]
pub(super) struct DedicatedPlan {
    capacity: NonZeroUsize,
    channels: NonZeroUsize,
    retained_bits: NonZeroUsize,
    receive: SampleRuntimeLimits,
    transmit: TransmitRuntimeLimits,
    control: DedicatedControllerLimits,
    admission: AdmissionLimits,
    hsdpa_commands: Option<NonZeroUsize>,
    hsdpa_waveforms: Option<NonZeroUsize>,
}

fn nonzero(value: usize) -> Result<NonZeroUsize> {
    NonZeroUsize::new(value).context("dedicated serving limit must be nonzero")
}

impl DedicatedPlan {
    pub(super) fn new(config: &Config) -> Result<Self> {
        let limits = &config.receiver.dedicated;
        limits.validate_schema()?;
        let capacity = nonzero(usize::try_from(config.runtime.maximum_ues)?)?;
        let work = nonzero(usize::from(limits.work_per_cycle))?;
        let commands = nonzero(usize::from(limits.command_capacity))?;
        let configurations = capacity
            .get()
            .checked_mul(CONFIGURATIONS_PER_UE)
            .context("dedicated mapping capacity overflow")?;
        let history_samples = usize::try_from(CHIPS_PER_FRAME)?
            .checked_mul(SamplesPerChip::Two.get())
            .and_then(|count| count.checked_mul(usize::from(limits.history_frames)))
            .context("dedicated sample history overflow")?;
        let history_bytes = history_samples
            .checked_mul(2 * std::mem::size_of::<num_complex::Complex32>())
            .and_then(|bytes| {
                crate::dedicated_rx::DedicatedSampleRuntime::shared_queue_storage_bytes(
                    configurations,
                )
                .and_then(|queue| bytes.checked_add(queue))
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    umts_dsp::fractional_sampling::FractionalSampler::new().storage_bytes(),
                )
            })
            .and_then(|bytes| {
                umts_runtime::mailbox::retained_storage_bound::<DownlinkControl>(commands)
                    .and_then(|feedback| bytes.checked_add(feedback))
            })
            .context("dedicated shared storage overflow")?;
        ensure!(
            history_bytes <= limits.maximum_shared_bytes as usize,
            "receiver.dedicated.maximum_shared_bytes is smaller than history and feedback storage ({history_bytes} bytes)"
        );
        let frame_bits = nonzero(usize::try_from(limits.maximum_frame_bits)?)?;
        let output_slots = nonzero(usize::from(limits.output_slots))?;
        let output_reservation = frame_bits
            .get()
            .checked_mul(output_slots.get())
            .and_then(|bytes| bytes.checked_mul(std::mem::size_of::<umts_dsp::bits::Bit>()))
            .and_then(|bytes| {
                crate::dedicated_rx::DedicatedReceiveSession::output_storage_bytes(
                    output_slots,
                    frame_bits.get(),
                )
                .ok()
                .and_then(|internal| bytes.checked_add(internal))
            })
            .and_then(|bytes| bytes.checked_add(frame_bits.get()))
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<usize>()))
            .context("dedicated output reservation overflow")?;
        ensure!(
            output_reservation <= limits.maximum_output_bytes as usize,
            "receiver.dedicated.maximum_output_bytes cannot reserve one mapping's output queue"
        );
        ensure!(
            limits.maximum_frame_bits <= limits.maximum_transmit_bits,
            "receiver.dedicated.maximum_transmit_bits cannot retain one maximum-sized frame"
        );
        let retained_bits = nonzero(usize::try_from(limits.maximum_transmit_bits)?)?;
        Ok(Self {
            capacity,
            channels: nonzero(
                configurations
                    .checked_mul(MAXIMUM_DOWNLINK_CODES_PER_CONFIGURATION)
                    .context("dedicated channel capacity overflow")?,
            )?,
            retained_bits,
            receive: SampleRuntimeLimits {
                shard: ShardLimits {
                    maximum_ues: capacity,
                    maximum_sessions_per_step: work,
                    resources: ReceiveResources {
                        physical_sample_bytes: limits.maximum_sample_bytes as usize,
                        coded_bits: limits.maximum_coded_bits as usize,
                        output_bytes: limits.maximum_output_bytes as usize,
                        demultiplexed_bits: limits.maximum_demultiplexed_bits as usize,
                        channels: configurations
                            .checked_mul(MAXIMUM_TRANSPORTS_PER_CONFIGURATION)
                            .context("dedicated transport capacity overflow")?,
                    },
                },
                history_samples: nonzero(history_samples)?,
                maximum_shared_bytes: limits.maximum_shared_bytes as usize,
                command_capacity: commands,
                commands_per_step: work,
                output_slots_per_ue: output_slots,
                maximum_output_bits: frame_bits,
                maximum_output_bytes: limits.maximum_output_bytes as usize,
            },
            transmit: TransmitRuntimeLimits {
                command_capacity: commands,
                commands_per_step: work,
                feedback_capacity: commands,
                feedback_per_step: work,
                maximum_frame_payload_bits: frame_bits,
                maximum_inflight_retained_bits: retained_bits,
            },
            control: DedicatedControllerLimits {
                receipt_visits: work,
                service_visits: work,
                maximum_bearer_bytes: limits.maximum_bearer_bytes as usize,
            },
            admission: AdmissionLimits {
                visits: work,
                command_results: work,
                prefill_frames: nonzero(PREFILL_FRAMES)?,
                maximum_lead_frames: nonzero(PREFILL_FRAMES + LAUNCH_LOOKAHEAD_FRAMES)?,
                health_interval_chips: NonZeroU64::new(HEALTH_INTERVAL_CHIPS)
                    .expect("nonzero frame"),
            },
            hsdpa_commands: config.hspa.downlink.enabled.then_some(commands),
            hsdpa_waveforms: config.hspa.downlink.enabled.then_some(nonzero(usize::from(
                config.hspa.downlink.resources.pending_transmissions,
            ))?),
        })
    }

    pub(super) fn attach<T: Transport>(
        self,
        preparation: &mut TxPreparation,
        receive: &mut CommonRx,
        controller: &mut CommonController<T>,
    ) -> Result<u64> {
        let ports = preparation
            .dedicated_ports(
                self.capacity,
                self.channels,
                self.retained_bits,
                self.transmit,
            )
            .map_err(|error| anyhow!("prepare dedicated transmit owner: {error:?}"))?;
        let receive_port = receive
            .configure_dedicated(self.receive)
            .map_err(|error| anyhow!("prepare dedicated receive owner: {error:?}"))?;
        let feedback = preparation
            .dedicated_feedback(ports.feedback)
            .map_err(|_| anyhow!("pair dedicated feedback with paced transmit owner"))?;
        let lead = feedback.minimum_lead_chips();
        receive
            .configure_dedicated_feedback(feedback)
            .map_err(|_| anyhow!("pair dedicated feedback with receive clock"))?;
        let dedicated = DedicatedController::new(receive_port, self.control);
        let mut admission = Rel99Admission::new(ports.control, self.capacity, self.admission)
            .map_err(|(error, _)| anyhow!("prepare dedicated admission owner: {error:?}"))?;
        if let Some(capacity) = self.hsdpa_commands {
            admission
                .configure_hsdpa_control(capacity)
                .map_err(|error| anyhow!("prepare shared HSDPA command tracking: {error:?}"))?;
            admission
                .configure_hsdpa_feedback(capacity, capacity)
                .map_err(|error| anyhow!("prepare per-UE HSDPA feedback ownership: {error:?}"))?;
        }
        controller
            .attach_dedicated(dedicated)
            .map_err(|(error, _)| anyhow!("attach dedicated receive controller: {error:?}"))?;
        controller
            .attach_admission(admission)
            .map_err(|(error, _)| anyhow!("attach dedicated admission controller: {error:?}"))?;
        if self.hsdpa_commands.is_some() {
            controller
                .configure_hsdpa_sessions(self.admission.visits)
                .map_err(|error| anyhow!("prepare per-UE HSDPA sessions: {error:?}"))?;
            controller
                .configure_hsdpa_waveforms(
                    self.hsdpa_waveforms
                        .context("missing HSDPA waveform capacity")?,
                )
                .map_err(|error| anyhow!("prepare shared HSDPA waveform storage: {error:?}"))?;
        }
        Ok(lead)
    }
}

#[cfg(test)]
mod tests;
