// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::hsupa_session::candidate_admission::{AdmissionSettings, CandidateAdmission, Progress};
use crate::{hsupa_rx::ControlAdmission, hsupa_session::ReceiveSettings, hsupa_tx::TransmitBank};
use anyhow::{Context, Result, anyhow, ensure};
use std::num::{NonZeroU8, NonZeroU64, NonZeroUsize};
use umts_config::{Config, RustHsupaReceive};
use umts_dsp::dedicated::LikelihoodSettings;
use umts_types::time::CHIPS_PER_SECOND;

mod policy;
const RESPONSE_GUARD_SECONDS: u64 = 5;

#[cfg(test)]
pub(super) mod tests;

pub(super) struct SharedChannels {
    lease: Option<crate::radio_resources::HsupaCodeLease>,
    pending: NonZeroUsize,
    maximum_bytes: NonZeroUsize,
    pool: crate::radio_resources::HsupaPoolIdentity,
    admission: CandidateAdmission,
    policy: policy::Policy,
}

impl SharedChannels {
    pub(super) fn reserve(
        config: &Config,
        resources: &mut crate::radio_resources::RadioResources,
        power: Option<&super::downlink_power::Plan>,
    ) -> Result<Option<Self>> {
        if !config.hspa.uplink.enabled {
            return Ok(None);
        }
        validate(config)?;
        let settings = config
            .hspa
            .uplink
            .resources
            .as_ref()
            .context("HSUPA budgets missing")?;
        let pending = count(u32::from(settings.pending_transmissions))?;
        let maximum_bytes = count(settings.maximum_tx_bytes)?;
        let policy = policy::Policy::new(config, power)?;
        let admission = CandidateAdmission::new().context("start HSUPA receiver preparation")?;
        let lease = resources
            .reserve_hsupa()
            .map_err(|error| anyhow!("reserve HSUPA common control codes: {error:?}"))?;
        Ok(Some(Self {
            pool: lease.identity(),
            lease: Some(lease),
            pending,
            maximum_bytes,
            admission,
            policy,
        }))
    }

    pub(super) fn prepare_transmit(
        &mut self,
        source: &crate::common_tx::CommonTx,
    ) -> Result<Box<TransmitBank>> {
        let lease = self
            .lease
            .take()
            .context("HSUPA transmitter already prepared")?;
        match TransmitBank::new(source, lease, self.pending, self.maximum_bytes) {
            Ok(bank) => Ok(bank),
            Err((error, lease)) => {
                self.lease = Some(lease);
                Err(anyhow!("prepare HSUPA transmitter: {error:?}"))
            }
        }
    }

    pub(super) fn admit<T: umts_core::Transport>(
        &mut self,
        controller: &mut crate::controller::CommonController<T>,
        resources: &mut crate::radio_resources::RadioResources,
        launch: crate::rel99_admission::AdditionalLaunch,
    ) -> Result<Progress> {
        ensure!(self.lease.is_none(), "HSUPA transmitter is not prepared");
        controller
            .cleanup_cancelled_hsupa(resources)
            .map_err(|error| anyhow!("HSUPA canceled admission cleanup: {error:?}"))?;
        self.admission.select(controller, resources, self.pool)?;
        let policy = &self.policy;
        self.admission.step(
            controller,
            resources,
            AdmissionSettings {
                launch,
                response_guard: NonZeroU64::new(RESPONSE_GUARD_SECONDS * CHIPS_PER_SECOND)
                    .expect("positive response guard"),
                grant_value: policy.grant_value,
                grant_ceiling: policy.grant_ceiling,
                grant_amplitude: policy.grant_amplitude,
                grant_refresh: policy.grant_refresh,
            },
            |candidate, source| policy.request(candidate, source),
        )
    }
}

fn count(value: u32) -> Result<NonZeroUsize> {
    NonZeroUsize::new(usize::try_from(value)?).context("HSUPA resource limit is zero")
}

pub(super) fn reference_powers(config: &Config) -> Result<umts_rrc::hsupa::ReferencePowers> {
    use umts_rrc::hsupa::{PowerReference, ReferencePowers};
    let references = config
        .hspa
        .uplink
        .edpdch_references
        .iter()
        .map(|reference| {
            Ok(PowerReference {
                format: umts_types::hsupa::Etfci::new(reference.etfci)
                    .map_err(|error| anyhow!("HSUPA reference E-TFCI: {error:?}"))?,
                power_index: reference.power_offset,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    ReferencePowers::new(references).map_err(|error| anyhow!("HSUPA power references: {error:?}"))
}

pub(super) fn receive_settings(settings: &RustHsupaReceive) -> Result<ReceiveSettings> {
    settings.validate_schema()?;
    Ok(ReceiveSettings {
        likelihood: LikelihoodSettings::new(settings.likelihood_limit)
            .map_err(|error| anyhow!("HSUPA likelihood limit: {error:?}"))?,
        control_admission: ControlAdmission::new(
            settings.minimum_control_correlation,
            settings.minimum_control_margin,
        )
        .map_err(|error| anyhow!("HSUPA control admission: {error:?}"))?,
        iterations: NonZeroU8::new(settings.iterations).context("HSUPA iterations are zero")?,
        maximum_format_bits: count(settings.maximum_format_bits)?,
        maximum_catalog_bytes: count(settings.maximum_catalog_bytes)?,
        retained_sample_bytes: count(settings.retained_sample_bytes)?,
        soft_buffer_bytes: count(settings.soft_buffer_bytes)?,
        scratch_bytes: count(settings.scratch_bytes)?,
        maximum_receive_bytes: count(settings.maximum_receive_bytes)?,
        report_capacity: count(u32::from(settings.report_capacity))?,
        report_bytes: count(settings.report_bytes)?,
        packet_capacity: count(u32::from(settings.packet_capacity))?,
        packet_bytes: count(settings.packet_bytes)?,
        reorder_hold_ttis: NonZeroU64::new(u64::from(settings.reorder_hold_ttis))
            .context("HSUPA reorder hold is zero")?,
        feedback_capacity: count(u32::from(settings.feedback_capacity))?,
        feedback_amplitude: settings.feedback_amplitude,
    })
}

pub(super) fn validate(config: &Config) -> Result<()> {
    let Some(settings) = &config.hspa.uplink.resources else {
        ensure!(
            !config.hspa.uplink.enabled,
            "HSUPA requires explicit hspa.uplink.resources budgets"
        );
        return Ok(());
    };
    settings.validate_schema()?;
    let receive = receive_settings(&settings.receive)?;
    reference_powers(config)?;
    let required_tx =
        TransmitBank::required_storage_bytes(count(u32::from(settings.pending_transmissions))?)
            .map_err(|error| anyhow!("HSUPA transmitter storage: {error:?}"))?;
    ensure!(
        required_tx <= usize::try_from(settings.maximum_tx_bytes)?,
        "HSUPA transmitter requires at least {required_tx} bytes"
    );
    let per_ue = [
        receive.maximum_receive_bytes.get(),
        receive.maximum_catalog_bytes.get(),
        receive.report_bytes.get(),
        receive.packet_bytes.get(),
    ]
    .into_iter()
    .try_fold(0u64, |sum, bytes| {
        sum.checked_add(u64::try_from(bytes).ok()?)
    })
    .context("HSUPA per-UE memory limits overflow")?;
    let shared_decoder_bytes = if config.hspa.uplink.enabled {
        u64::try_from(
            crate::hsupa_rx::decode::Worker::required_storage_bytes(config.runtime.maximum_ues)
                .context("HSUPA decoder storage overflow")?,
        )
        .context("HSUPA decoder storage overflow")?
    } else {
        0
    };
    let cell = per_ue
        .checked_mul(u64::from(config.runtime.maximum_ues))
        .and_then(|bytes| bytes.checked_add(shared_decoder_bytes))
        .context("HSUPA cell memory limits overflow")?;
    ensure!(
        cell <= settings.maximum_cell_receive_bytes,
        "HSUPA cell budget must cover {cell} bytes of configured receive and packet limits"
    );
    ensure!(
        receive.retained_sample_bytes.get() <= receive.maximum_receive_bytes.get()
            && receive.soft_buffer_bytes.get() <= receive.maximum_receive_bytes.get()
            && receive.scratch_bytes.get() <= receive.maximum_receive_bytes.get(),
        "HSUPA receiver components exceed the total receive budget"
    );
    Ok(())
}
