// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result as AnyResult, bail};
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use umts_config::Config;
use umts_dsp::{
    carrier::CarrierOffsetHz,
    dedicated::{CarrierTrackingSettings, DpchAmplitudes, LikelihoodSettings, UplinkDpchCode},
    pulse_shaping::SamplesPerChip,
    uplink_scrambling::UplinkScramblingCode,
};
use umts_types::time::{CHIPS_PER_SECOND, ChipInstant};

use crate::{
    dedicated_bearers::{BearerLimits, FramePoolLimits},
    dedicated_rx::{
        PilotSirTarget, PilotTpcSettings, ReceiveLimits, SampleReceiveSettings, TfciAcceptance,
        storage_requirements,
    },
    dedicated_tx::{TpcInitialization, TransmitLimits},
    rel99_planner::{InitialOwnerSettings, InitialPowerControl, OwnerBuildError, PreparedInitial},
    serving::initial_timing,
};

const CONFIGURATIONS_PER_UE: usize = 2;
const FRAME_SLOTS: usize = 8;
const POOL_PAYLOAD_SLOTS: usize = 1;
const POOL_RECEIPT_SLOTS: usize = 32;
const RESPONSE_GUARD_SECONDS: u64 = 5;

pub(super) struct OwnerPlan {
    sample_rate: SamplesPerChip,
    search_radius_samples: usize,
    minimum_coherent_fraction: f64,
    radio_link: crate::dedicated_rx::RadioLinkSyncSettings,
    rx_delay_samples: f64,
    carrier: CarrierTrackingSettings,
    likelihood: LikelihoodSettings,
    tfci: TfciAcceptance,
    target: PilotSirTarget,
    outer_loop: crate::dedicated_rx::OuterLoopSettings,
    maximum_observation_age_chips: NonZeroU64,
    initial_tpc_maximum_slots: NonZeroU32,
    algorithm2_hold_groups: u8,
    amplitudes: DpchAmplitudes,
    packet_power_gain: f32,
    downlink_power: crate::dedicated_tx::DownlinkPowerSettings,
    coded_bits: NonZeroUsize,
    output_bytes: NonZeroUsize,
    sample_bytes: NonZeroUsize,
    demultiplexed_bits: NonZeroUsize,
    bearer_bytes: NonZeroUsize,
    transmit_bits: NonZeroUsize,
    queue_bytes: NonZeroUsize,
    maximum_frame_bits: NonZeroUsize,
    maximum_sdu_bytes: NonZeroUsize,
    output_slots: NonZeroUsize,
    work_per_cycle: NonZeroUsize,
}

impl OwnerPlan {
    pub(super) fn new(
        config: &Config,
        power: Option<&super::downlink_power::Plan>,
    ) -> AnyResult<Self> {
        config.validate_schema()?;
        let mappings = (config.runtime.maximum_ues as usize)
            .checked_mul(CONFIGURATIONS_PER_UE)
            .context("dedicated owner mapping count overflows")?;
        let target_db = config
            .power_control
            .uplink
            .initial_sir_db
            .unwrap_or(config.power_control.uplink.sir_ceiling_db);
        let target = PilotSirTarget::from_db(target_db)
            .map_err(|error| anyhow::anyhow!("invalid dedicated SIR target: {error:?}"))?;
        let outer_loop = crate::dedicated_rx::OuterLoopSettings::new(
            config
                .power_control
                .uplink
                .sir_floor_db
                .unwrap_or(config.power_control.uplink.sir_ceiling_db),
            target_db,
            config.power_control.uplink.sir_ceiling_db,
            config.power_control.uplink.target_bler,
        )
        .map_err(|error| anyhow::anyhow!("invalid dedicated outer-loop policy: {error:?}"))?;
        let dedicated = &config.receiver.dedicated;
        let sample_rate = SamplesPerChip::Two;
        let carrier_initial = CarrierOffsetHz::new(dedicated.carrier_initial_hz)
            .map_err(|error| anyhow::anyhow!("invalid dedicated carrier offset: {error:?}"))?;
        let carrier = CarrierTrackingSettings {
            initial: carrier_initial,
            maximum_absolute_hz: dedicated.carrier_maximum_absolute_hz,
            update_fraction: dedicated.carrier_update_fraction,
            minimum_coherence: dedicated.carrier_minimum_coherence,
        };
        let nyquist = CHIPS_PER_SECOND as f64 / (2.0 * 256.0);
        if !carrier.maximum_absolute_hz.is_finite()
            || carrier.maximum_absolute_hz <= 0.0
            || 2.0 * carrier.maximum_absolute_hz >= nyquist
            || carrier.initial.hz().abs() > carrier.maximum_absolute_hz
            || !carrier.update_fraction.is_finite()
            || !(0.0..=1.0).contains(&carrier.update_fraction)
            || carrier.update_fraction == 0.0
            || !carrier.minimum_coherence.is_finite()
            || !(0.0..=1.0).contains(&carrier.minimum_coherence)
            || carrier.minimum_coherence == 0.0
        {
            bail!("runtime.serving.dedicated carrier settings are invalid");
        }
        let likelihood = LikelihoodSettings::new(dedicated.likelihood_maximum)
            .map_err(|error| anyhow::anyhow!("invalid dedicated likelihood setting: {error:?}"))?;
        let maximum_observation_age_chips = NonZeroU64::new(u64::from(
            config
                .power_control
                .downlink
                .dedicated
                .tpc_maximum_observation_age_chips,
        ))
        .context("dedicated TPC observation age must be nonzero")?;
        let initial_tpc_maximum_slots = NonZeroU32::new(
            config
                .power_control
                .downlink
                .dedicated
                .tpc_initial_maximum_slots,
        )
        .context("dedicated TPC initialization slots must be nonzero")?;
        let cpich_amplitude = power
            .map_or(config.power_control.downlink.cpich_amplitude, |power| {
                power.common.primary.cpich
            });
        if !(cpich_amplitude.is_finite() && cpich_amplitude > 0.0 && cpich_amplitude <= 0.25) {
            bail!("power_control.downlink.cpich_amplitude is outside (0, 0.25]");
        }
        let downlink_offset_db = power.map_or(
            config.power_control.downlink.dedicated.downlink_offset_db,
            |power| power.dedicated_offset_db,
        );
        if !downlink_offset_db.is_finite() || !(-20.0..=6.0).contains(&downlink_offset_db) {
            bail!("power_control.downlink.dedicated.downlink_offset_db is invalid");
        }
        let channel_amplitude = cpich_amplitude * 10.0_f32.powf(downlink_offset_db / 20.0);
        let tpc_amplitude = channel_amplitude
            * 10.0_f32.powf(config.power_control.downlink.dedicated.tpc_offset_db / 20.0);
        if [channel_amplitude, tpc_amplitude]
            .iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        {
            bail!("derived dedicated channel amplitude is invalid");
        }
        let amplitudes = DpchAmplitudes {
            data: channel_amplitude,
            tpc: tpc_amplitude,
            tfci: channel_amplitude,
            pilot: channel_amplitude,
        };
        let minimum_offset_db = power.map_or(
            config
                .power_control
                .downlink
                .dedicated
                .downlink_minimum_offset_db,
            |power| power.dedicated_minimum_offset_db,
        );
        if minimum_offset_db > downlink_offset_db {
            bail!("dedicated downlink minimum exceeds the initial/maximum CPICH-relative power");
        }
        let downlink_power = crate::dedicated_tx::DownlinkPowerSettings {
            initial_offset_db: downlink_offset_db,
            minimum_offset_db,
            maximum_age_chips: maximum_observation_age_chips.get(),
        };

        Ok(Self {
            sample_rate,
            search_radius_samples: usize::from(dedicated.search_radius_samples),
            minimum_coherent_fraction: dedicated.minimum_coherent_fraction,
            radio_link: crate::dedicated_rx::RadioLinkSyncSettings {
                out_of_sync_frames: NonZeroU32::new(dedicated.radio_link_out_of_sync_frames)
                    .context("radio link N_OUTSYNC_IND must be nonzero")?,
                in_sync_frames: NonZeroU32::new(dedicated.radio_link_in_sync_frames)
                    .context("radio link N_INSYNC_IND must be nonzero")?,
                failure_frames: dedicated.radio_link_failure_frames,
            },
            rx_delay_samples: config.radio.rx_delay_samples,
            carrier,
            likelihood,
            tfci: TfciAcceptance {
                minimum_score: dedicated.tfci_minimum_score,
                minimum_margin: dedicated.tfci_minimum_margin,
            },
            target,
            outer_loop,
            maximum_observation_age_chips,
            initial_tpc_maximum_slots,
            algorithm2_hold_groups: config.power_control.uplink.tpc_hold_groups,
            amplitudes,
            packet_power_gain: power.map_or(1.0, |power| power.packet_power_gain),
            downlink_power,
            coded_bits: per_mapping(dedicated.maximum_coded_bits, mappings, "coded bits")?,
            output_bytes: per_mapping(dedicated.maximum_output_bytes, mappings, "output bytes")?,
            sample_bytes: per_mapping(dedicated.maximum_sample_bytes, mappings, "sample bytes")?,
            demultiplexed_bits: per_mapping(
                dedicated.maximum_demultiplexed_bits,
                mappings,
                "demultiplexed bits",
            )?,
            bearer_bytes: per_mapping(dedicated.maximum_bearer_bytes, mappings, "bearer bytes")?,
            transmit_bits: per_mapping(dedicated.maximum_transmit_bits, mappings, "transmit bits")?,
            queue_bytes: per_mapping(config.runtime.maximum_queue_bytes, mappings, "queue bytes")?,
            maximum_frame_bits: NonZeroUsize::new(usize::try_from(dedicated.maximum_frame_bits)?)
                .context("dedicated maximum frame bits must be nonzero")?,
            maximum_sdu_bytes: NonZeroUsize::new(usize::try_from(
                config.runtime.maximum_sdu_bytes,
            )?)
            .context("serving maximum SDU bytes must be nonzero")?,
            output_slots: NonZeroUsize::new(usize::from(dedicated.output_slots))
                .context("dedicated output slots must be nonzero")?,
            work_per_cycle: NonZeroUsize::new(usize::from(dedicated.work_per_cycle))
                .context("dedicated work per cycle must be nonzero")?,
        })
    }

    pub(super) fn settings(
        &self,
        prepared: &PreparedInitial,
        now: ChipInstant,
        threshold: ChipInstant,
        feedback_lead_chips: u64,
    ) -> std::result::Result<InitialOwnerSettings, OwnerBuildError> {
        let mut settings = self.settings_for_plan(
            prepared.allocation.plan(),
            now,
            threshold,
            feedback_lead_chips,
        )?;
        // TS 25.331 8.2.2.2 starts new radio links before sending initial bearer setup.
        settings.publication_deadline.index = settings
            .timing
            .downlink
            .time
            .index
            .checked_add(settings.response_timeout.get())
            .ok_or_else(|| configuration("initial setup publication deadline overflows"))?;
        Ok(settings)
    }

    pub(super) fn existing_settings(
        &self,
        prepared: &crate::rel99_planner::PreparedExisting,
        delivery: &crate::rel99_admission::AdditionalDelivery,
        now: ChipInstant,
        feedback_lead_chips: u64,
    ) -> std::result::Result<InitialOwnerSettings, OwnerBuildError> {
        let mut settings = self.settings_for_plan(
            prepared.change.plan(),
            now,
            delivery.activation.time(),
            feedback_lead_chips,
        )?;
        if settings.timing.downlink.time != delivery.activation.time()
            || settings.timing.downlink.cfn != delivery.activation.cfn()
        {
            return Err(OwnerBuildError::Identity);
        }
        settings.publication_deadline = delivery.publication_deadline;
        if let Some(power) = settings.power_control.as_mut() {
            power.initialization.first_radio_link_set = false;
        }
        Ok(settings)
    }

    pub(super) fn release_settings(
        &self,
        prepared: &crate::rel99_planner::PreparedRelease,
        delivery: &crate::rel99_admission::ReleaseDelivery,
        now: ChipInstant,
        feedback_lead_chips: u64,
    ) -> std::result::Result<InitialOwnerSettings, OwnerBuildError> {
        let mut settings = self.settings_for_plan(
            prepared.plan(),
            now,
            delivery.activation.time(),
            feedback_lead_chips,
        )?;
        if settings.timing.downlink.time != delivery.activation.time()
            || settings.timing.downlink.cfn != delivery.activation.cfn()
        {
            return Err(OwnerBuildError::Identity);
        }
        settings.publication_deadline = delivery.publication_deadline;
        if let Some(power) = settings.power_control.as_mut() {
            power.initialization.first_radio_link_set = false;
        }
        Ok(settings)
    }

    fn settings_for_plan(
        &self,
        plan: &Arc<crate::rel99_plan::CompiledRel99>,
        now: ChipInstant,
        threshold: ChipInstant,
        feedback_lead_chips: u64,
    ) -> std::result::Result<InitialOwnerSettings, OwnerBuildError> {
        let requirements = plan
            .requirements()
            .map_err(|error| configuration(format!("compiled plan requirements: {error:?}")))?;
        let timing = initial_timing::initial_timing(
            plan,
            now,
            threshold,
            self.rx_delay_samples,
            self.sample_rate,
        )
        .map_err(|error| configuration(format!("initial dedicated timing: {error:?}")))?;
        if timing.mapping.downlink.time.index <= now.index {
            return Err(configuration(
                "initial publication deadline must follow current radio time",
            ));
        }
        let feedback_lead_chips = NonZeroU64::new(feedback_lead_chips)
            .ok_or_else(|| configuration("dedicated feedback lead must be nonzero"))?;
        ensure_capacity(
            requirements.uplink_coded_bits,
            self.coded_bits.get(),
            "uplink coded-bit",
        )?;
        ensure_capacity(
            requirements.maximum_uplink_demultiplexed_bits,
            self.demultiplexed_bits.get(),
            "uplink demultiplexed-bit",
        )?;
        ensure_capacity(
            requirements.downlink_frame_payload_bits,
            self.maximum_frame_bits.get(),
            "downlink physical frame",
        )?;
        ensure_capacity(
            requirements.maximum_downlink_transport_output_bits,
            self.maximum_frame_bits.get(),
            "downlink transport",
        )?;
        ensure_capacity(
            requirements.maximum_uplink_transport_output_bits,
            self.maximum_frame_bits.get(),
            "uplink transport",
        )?;
        let output_storage = requirements
            .receive_output_storage_bytes(self.output_slots)
            .map_err(|error| configuration(format!("receive output storage: {error:?}")))?;
        ensure_capacity(output_storage, self.output_bytes.get(), "receive output")?;
        let code = UplinkScramblingCode::new(plan.configuration().physical().uplink_scrambling)
            .and_then(UplinkDpchCode::new)
            .map(Arc::new)
            .map_err(|error| configuration(format!("uplink code storage: {error:?}")))?;
        let sample_storage = storage_requirements(
            plan.uplink(),
            code,
            plan.uplink_control(),
            self.carrier,
            self.sample_rate,
            self.search_radius_samples,
            true,
        )
        .map_err(|error| configuration(format!("receive sample storage: {error:?}")))?;
        ensure_capacity(sample_storage, self.sample_bytes.get(), "receive sample")?;
        let transmit_window = requirements
            .transmit_frame_window_bits(NonZeroUsize::new(FRAME_SLOTS).expect("nonzero"))
            .map_err(|error| configuration(format!("transmit frame window: {error:?}")))?;
        ensure_capacity(transmit_window, self.transmit_bits.get(), "transmit frame")?;
        let pool = FramePoolLimits {
            payload_slots: NonZeroUsize::new(POOL_PAYLOAD_SLOTS).expect("nonzero payload slots"),
            receipt_slots: NonZeroUsize::new(POOL_RECEIPT_SLOTS).expect("nonzero receipt slots"),
            maximum_buffer_bytes: self.queue_bytes.get(),
        };
        plan.frame_pool_storage_bytes(pool)
            .map_err(|error| configuration(format!("frame pool storage: {error:?}")))?;

        let response_timeout = NonZeroU64::new(
            CHIPS_PER_SECOND
                .checked_mul(RESPONSE_GUARD_SECONDS)
                .ok_or_else(|| configuration("response guard overflows"))?,
        )
        .expect("nonzero response guard");
        let dedicated_packet = plan
            .configuration()
            .bearers()
            .iter()
            .filter(|bearer| bearer.domain == Some(umts_types::CnDomain::Ps))
            .flat_map(|bearer| &bearer.mapping.downlink)
            .any(|mapping| {
                plan.configuration()
                    .downlink()
                    .transports()
                    .iter()
                    .find(|transport| transport.id == mapping.transport)
                    .is_some_and(|transport| {
                        transport
                            .formats()
                            .any(|format| format.blocks > 0 && format.rlc_bits > 0)
                    })
            });
        let gain = if dedicated_packet {
            self.packet_power_gain
        } else {
            1.0
        };
        let amplitudes = DpchAmplitudes {
            data: self.amplitudes.data * gain,
            pilot: self.amplitudes.pilot * gain,
            tfci: self.amplitudes.tfci * gain,
            tpc: self.amplitudes.tpc * gain,
        };
        let mut downlink_power = self.downlink_power;
        downlink_power.initial_offset_db += 20.0 * gain.log10();
        downlink_power.minimum_offset_db += 20.0 * gain.log10();
        Ok(InitialOwnerSettings {
            timing: timing.mapping,
            samples: SampleReceiveSettings {
                rate: self.sample_rate,
                first_receive: timing.first_receive,
                search_radius_samples: self.search_radius_samples,
                minimum_coherent_fraction: self.minimum_coherent_fraction,
                radio_link: self.radio_link,
            },
            carrier: self.carrier,
            likelihood: self.likelihood,
            tfci: self.tfci,
            maximum_physical_bits: nonzero(
                requirements.maximum_uplink_physical_bits,
                "maximum physical bits",
            )?,
            receive: ReceiveLimits {
                output_slots: self.output_slots,
                maximum_output_bits: nonzero(
                    requirements.maximum_uplink_transport_output_bits,
                    "maximum output bits",
                )?,
                maximum_output_bytes: self.output_bytes.get(),
                maximum_coded_bits: self.coded_bits,
                maximum_frame_bits: self.demultiplexed_bits,
                maximum_decodes_per_step: self.work_per_cycle,
            },
            bearers: BearerLimits {
                // The bearer bank survives additions whose transports exceed the initial plan.
                maximum_transport_bits: self.maximum_frame_bits,
                output_slots: self.output_slots,
                maximum_sdu_bytes: self.maximum_sdu_bytes,
                maximum_buffer_bytes: self.bearer_bytes.get(),
            },
            transmit: TransmitLimits {
                frame_slots: NonZeroUsize::new(FRAME_SLOTS).expect("nonzero frame slots"),
                maximum_frame_bits: nonzero(
                    requirements.downlink_frame_payload_bits,
                    "maximum downlink frame bits",
                )?,
                maximum_retained_bits: self.transmit_bits,
            },
            amplitudes,
            downlink_power: Some(crate::dedicated_tx::DownlinkPowerSettings {
                // The TX frontier includes lookahead, unlike the RX observation-age check.
                maximum_age_chips: self
                    .downlink_power
                    .maximum_age_chips
                    .checked_add(feedback_lead_chips.get())
                    .and_then(|age| age.checked_add(umts_types::time::CHIPS_PER_SLOT))
                    .ok_or_else(|| configuration("downlink feedback horizon overflows"))?,
                ..downlink_power
            }),
            pool,
            now,
            publication_deadline: timing.mapping.downlink.time,
            response_timeout,
            power_control: Some(InitialPowerControl {
                outer_loop: Some(self.outer_loop),
                algorithm2_hold_groups: self.algorithm2_hold_groups,
                pilot: PilotTpcSettings {
                    target: self.target,
                    minimum_coherent_fraction: self.minimum_coherent_fraction,
                    uplink_anchor: timing.mapping.uplink.time,
                    downlink_anchor: timing.mapping.downlink.time,
                    rate: self.sample_rate,
                    lead_chips: feedback_lead_chips,
                    maximum_observation_age_chips: self.maximum_observation_age_chips,
                },
                initialization: TpcInitialization {
                    first_radio_link_set: true,
                    // TS 25.214 5.1.2.2.3 raises power only on five equal commands, so a
                    // pattern with any "01" pair never raises a handset that starts too low.
                    pattern_01_count: 0,
                    maximum_slots: self.initial_tpc_maximum_slots,
                },
            }),
        })
    }
}

fn per_mapping(value: u32, mappings: usize, name: &str) -> AnyResult<NonZeroUsize> {
    if mappings == 0 {
        bail!("dedicated {name} mapping count is zero");
    }
    let value = usize::try_from(value)?;
    let share = value / mappings;
    NonZeroUsize::new(share)
        .with_context(|| format!("dedicated {name} ceiling is zero per mapping"))
}

fn ensure_capacity(
    required: usize,
    available: usize,
    name: &str,
) -> std::result::Result<(), OwnerBuildError> {
    if required > available {
        return Err(configuration(format!(
            "{name} requirement {required} exceeds per-mapping ceiling {available}"
        )));
    }
    Ok(())
}

fn nonzero(value: usize, name: &str) -> std::result::Result<NonZeroUsize, OwnerBuildError> {
    NonZeroUsize::new(value).ok_or_else(|| configuration(format!("{name} must be nonzero")))
}

fn configuration(message: impl Into<String>) -> OwnerBuildError {
    OwnerBuildError::Configuration(message.into())
}

#[cfg(test)]
mod tests;
