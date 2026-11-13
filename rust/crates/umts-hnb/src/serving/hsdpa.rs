// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    common_tx::CommonTx,
    controller::{CommonController, HsdpaAdmissionRequest, HsdpaAdmissionStep},
    hsdpa_session::{QueueBudget, RetainedDpchPolicy},
    hsdpa_tx::TransmitBank,
    radio_resources::{HsdpaCodeLease, HsdpaCodeSet, RadioResources},
    rel99_admission::AdditionalLaunch,
};
use anyhow::{Context, Result, anyhow, ensure};
use std::num::{NonZeroU8, NonZeroU64, NonZeroUsize};
use umts_config::Config;
use umts_core::Transport;
use umts_types::time::{CHIPS_PER_SECOND, ChipInstant};

const RESPONSE_GUARD_SECONDS: u64 = 5;
const UNALLOCATED_MEASUREMENT_POWER_OFFSET_DB: f64 = 6.0;
mod scheduling;

pub(super) struct SharedChannels {
    secondary: Option<crate::controller::SecondaryHsdpaAdmission>,
    scheduling: Option<crate::hsdpa_session::SchedulingAdmission>,
    lease: Option<HsdpaCodeLease>,
    codes: HsdpaCodeSet,
    capacity: NonZeroUsize,
    maximum_bytes: NonZeroUsize,
    policy: RetainedDpchPolicy,
    queues: QueueBudget,
    downlink_origin: Option<ChipInstant>,
    primary: Option<umts_dsp::scrambling::PrimaryScramblingCode>,
    packet_preparation_guard: Option<NonZeroU64>,
}

fn initial_budget_available(scheduling: &crate::hsdpa_session::SchedulingAdmission) -> bool {
    scheduling
        .budget
        .for_carriers(1)
        .is_ok_and(|budget| scheduling.cell.available_bytes() >= budget.maximum_total_bytes.get())
}

impl SharedChannels {
    pub(super) fn initial_preparation_available<T: Transport>(
        &self,
        controller: &CommonController<T>,
    ) -> bool {
        self.scheduling.as_ref().is_none_or(|scheduling| {
            controller.initial_hsdpa_preparation_available() && initial_budget_available(scheduling)
        })
    }

    pub(super) fn prepare_initial<T: Transport>(
        &self,
        controller: &mut CommonController<T>,
        request: Box<crate::rel99_admission::PreparedRabAssignment>,
        resources: &mut RadioResources,
    ) -> Result<
        crate::controller::InitialCatalogAdmission,
        Box<crate::rel99_admission::RejectedRabAssignment>,
    > {
        match &self.scheduling {
            Some(scheduling) => controller.prepare_initial_hsdpa(
                request,
                &self.policy,
                self.codes,
                self.secondary,
                scheduling,
                resources,
            ),
            None => {
                if let Err(error) = self.policy.validate_rel99_packet_fallback(
                    request.radio().allocation.plan().configuration(),
                ) {
                    return Err(request.reject(
                        resources,
                        crate::rel99_admission::RabAdmissionError::Hsdpa(error.to_string()),
                    ));
                }
                Ok(crate::controller::InitialCatalogAdmission::Ready(request))
            }
        }
    }

    pub(super) fn reserve(
        config: &Config,
        resources: &mut RadioResources,
        power: Option<&super::downlink_power::Plan>,
    ) -> Result<Option<Self>> {
        if !config.hspa.downlink.enabled {
            return Ok(None);
        }
        let settings = &config.hspa.downlink.resources;
        settings.validate_schema()?;
        let scheduling = scheduling::admission(config, power)?;
        let packet_preparation_guard = settings.scheduling.as_ref().map(|settings| {
            NonZeroU64::new(
                (u64::from(settings.preparation_guard_us) * CHIPS_PER_SECOND).div_ceil(1_000_000),
            )
            .expect("validated positive HSDPA preparation guard")
        });
        let allocated_gamma_db = power.and_then(|power| power.gamma_db);
        let measurement_gamma_db = config
            .hspa
            .downlink
            .measurement_power_offset_db
            .or(allocated_gamma_db)
            .unwrap_or(UNALLOCATED_MEASUREMENT_POWER_OFFSET_DB);
        let transmit_gamma_db = allocated_gamma_db.unwrap_or(measurement_gamma_db);
        let policy = RetainedDpchPolicy::new(&config.hspa.downlink, measurement_gamma_db)?
            .with_packet_modes(config.cell.packet_modes.as_deref());
        tracing::info!(
            transmit_gamma_db,
            measurement_gamma_db,
            "HSDPA transmit and CQI reference power"
        );
        let queues = QueueBudget {
            pdus_per_queue: NonZeroUsize::new(usize::from(settings.pdus_per_queue))
                .context("HSDPA queue capacity is zero")?,
            total_bytes: NonZeroUsize::new(settings.maximum_queue_bytes_per_ue as usize)
                .context("HSDPA per-UE queue budget is zero")?,
        };
        let all_queue_bytes = u64::from(config.runtime.maximum_ues)
            .checked_mul(u64::from(settings.maximum_queue_bytes_per_ue))
            .context("HSDPA cell queue budget overflows")?;
        ensure!(
            all_queue_bytes <= u64::from(settings.maximum_queue_bytes),
            "HSDPA cell queue budget cannot cover the configured UE count"
        );
        let capacity = NonZeroUsize::new(usize::from(settings.pending_transmissions))
            .context("HSDPA pending transmission capacity is zero")?;
        let maximum_bytes = NonZeroUsize::new(settings.maximum_tx_bytes as usize)
            .context("HSDPA TX storage budget is zero")?;
        let required = TransmitBank::required_storage_bytes(capacity)
            .map_err(|error| anyhow!("HSDPA TX storage calculation: {error:?}"))?;
        ensure!(
            required <= maximum_bytes.get(),
            "hspa.downlink.resources.maximum_tx_bytes must reserve {required} bytes for the pending transmissions"
        );
        ensure!(
            crate::hsdpa_tx::retained_transmission_bits()
                <= config.receiver.dedicated.maximum_transmit_bits as usize,
            "dedicated.maximum_transmit_bits cannot retain one HSDPA waveform command"
        );
        let data = NonZeroU8::new(settings.data_codes).context("HSDPA data code count is zero")?;
        let control =
            NonZeroU8::new(settings.control_codes).context("HSDPA control code count is zero")?;
        let lease = resources.reserve_hsdpa(data, control).map_err(|error| {
            anyhow!("reserve shared HSDPA codes alongside common/Rel99 channels: {error:?}")
        })?;
        Ok(Some(Self {
            secondary: None,
            scheduling,
            codes: *lease.codes(),
            lease: Some(lease),
            capacity,
            maximum_bytes,
            policy,
            queues,
            downlink_origin: None,
            primary: None,
            packet_preparation_guard,
        }))
    }

    pub(super) fn configure_secondary(
        &mut self,
        secondary: crate::controller::SecondaryHsdpaAdmission,
    ) {
        self.secondary = Some(secondary);
    }

    pub(super) fn codes(&self) -> HsdpaCodeSet {
        self.codes
    }

    pub(super) fn prepare_transmit(&mut self, source: &CommonTx) -> Result<TransmitBank> {
        let origin = ChipInstant {
            index: source
                .next_chip()
                .index
                .checked_add(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                .context("HSDPA data origin overflows")?,
            ..source.next_chip()
        };
        let lease = self
            .lease
            .take()
            .context("HSDPA TX owner already prepared")?;
        match TransmitBank::new(source, lease, self.capacity, self.maximum_bytes) {
            Ok(bank) => {
                self.downlink_origin = Some(origin);
                self.primary = Some(source.information().primary_code());
                Ok(bank)
            }
            Err((error, lease)) => {
                self.lease = Some(lease);
                Err(anyhow!("prepare shared HSDPA transmit owner: {error:?}"))
            }
        }
    }

    pub(super) fn admit<T: Transport>(
        &self,
        controller: &mut CommonController<T>,
        launch: AdditionalLaunch,
    ) -> Result<HsdpaAdmissionStep> {
        let mut report = controller.admit_hsdpa_sessions(HsdpaAdmissionRequest {
            secondary: self.secondary,
            scheduling: self.scheduling.as_ref(),
            primary: self
                .primary
                .context("HSDPA transmit primary code is unavailable")?,
            policy: &self.policy,
            codes: self.codes,
            launch,
            response_guard: NonZeroU64::new(RESPONSE_GUARD_SECONDS * CHIPS_PER_SECOND)
                .expect("nonzero HSDPA response guard"),
            queues: QueueBudget {
                pdus_per_queue: self.queues.pdus_per_queue,
                total_bytes: self.queues.total_bytes,
            },
            downlink_origin: self
                .downlink_origin
                .context("HSDPA transmit owner has not been attached")?,
        })?;
        if let Some(guard) = self.packet_preparation_guard {
            let (cursor, _) = controller.dedicated_launch_cursor();
            let threshold = super::launch::packet_threshold(cursor, launch.now, guard)
                .map_err(|error| anyhow!("HSDPA packet preparation time: {error:?}"))?;
            report.scheduling = controller
                .schedule_hsdpa_some(launch.now, threshold)
                .map_err(|error| anyhow!("HSDPA scheduling: {error:?}"))?;
        }
        report.failures += report.scheduling.failures;
        if report.first_failure.is_none() {
            report.first_failure = report.scheduling.first_failure.take();
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{at, count, source};

    #[test]
    fn initial_preflight_uses_one_carrier_component_bound_not_aggregate_ue_limit() {
        let config: Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/hsdpa.yaml"
        )))
        .unwrap();
        let mut scheduling = scheduling::admission(&config, None).unwrap().unwrap();
        scheduling.budget.maximum_total_bytes = count(640_000_000);
        let single = scheduling
            .budget
            .for_carriers(1)
            .unwrap()
            .maximum_total_bytes
            .get();
        assert!(single < scheduling.budget.maximum_total_bytes.get());
        scheduling.cell = crate::hsdpa_session::CellSchedulingBudget::new(count(single));
        assert!(initial_budget_available(&scheduling));
        scheduling.cell = crate::hsdpa_session::CellSchedulingBudget::new(count(single - 1));
        assert!(!initial_budget_available(&scheduling));
    }

    #[test]
    fn packet_guard_is_configured_without_changing_bearer_launch_guard() {
        let mut config: Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/hsdpa.yaml"
        )))
        .unwrap();
        let source = source(true);
        for (micros, chips) in [(20000, 76800), (4000, 15360)] {
            config
                .hspa
                .downlink
                .resources
                .scheduling
                .as_mut()
                .unwrap()
                .preparation_guard_us = micros;
            let mut resources =
                RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
            let channels = SharedChannels::reserve(&config, &mut resources, None)
                .unwrap()
                .unwrap();
            let guard = channels.packet_preparation_guard.unwrap();
            assert_eq!(guard.get(), chips);
            assert_eq!(
                super::super::launch::packet_threshold(at(1000), at(500), guard).unwrap(),
                at((500 + chips).max(1000))
            );
            assert_eq!(
                super::super::launch::threshold(at(1000), at(500), NonZeroU64::new(76800).unwrap())
                    .unwrap(),
                at(77800)
            );
        }
        config
            .hspa
            .downlink
            .resources
            .scheduling
            .as_mut()
            .unwrap()
            .preparation_guard_us = 0;
        assert!(config.validate().is_err());
    }

    pub(super) fn config() -> Config {
        serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/rel99.yaml"
        )))
        .unwrap()
    }

    #[test]
    fn shared_pool_reserves_once_and_transfers_the_original_lease_to_tx() {
        let mut config = config();
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        assert!(
            SharedChannels::reserve(&config, &mut resources, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(resources.outstanding(), 0);
        config.hspa.downlink.enabled = true;
        let mut shared = SharedChannels::reserve(&config, &mut resources, None)
            .unwrap()
            .unwrap();
        assert_eq!(resources.outstanding(), 1);
        assert_eq!(
            shared.codes().data_count().get(),
            config.hspa.downlink.resources.data_codes
        );
        assert_eq!(
            shared.codes().control_codes().count(),
            usize::from(config.hspa.downlink.resources.control_codes)
        );
        assert!(SharedChannels::reserve(&config, &mut resources, None).is_err());
        let bank = shared.prepare_transmit(&source).unwrap();
        assert_eq!(bank.codes(), shared.codes());
        assert!(shared.prepare_transmit(&source).is_err());
        let (lease, pending) = bank.into_retained();
        assert_eq!(pending.count(), 0);
        resources.release_hsdpa(lease).unwrap();
        assert_eq!(resources.outstanding(), 0);
    }

    #[test]
    fn storage_and_command_limits_are_checked_before_code_reservation() {
        let mut config = config();
        config.hspa.downlink.enabled = true;
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let required = TransmitBank::required_storage_bytes(count(usize::from(
            config.hspa.downlink.resources.pending_transmissions,
        )))
        .unwrap();
        config.hspa.downlink.resources.maximum_tx_bytes = u32::try_from(required - 1).unwrap();
        assert!(SharedChannels::reserve(&config, &mut resources, None).is_err());
        assert_eq!(resources.outstanding(), 0);
        config.hspa.downlink.resources.maximum_tx_bytes = u32::try_from(required).unwrap();
        let previous = config.receiver.dedicated.maximum_transmit_bits;
        config.receiver.dedicated.maximum_transmit_bits = 1;
        assert!(SharedChannels::reserve(&config, &mut resources, None).is_err());
        assert_eq!(resources.outstanding(), 0);
        config.receiver.dedicated.maximum_transmit_bits = previous;
        let mut shared = SharedChannels::reserve(&config, &mut resources, None)
            .unwrap()
            .unwrap();
        assert_eq!(resources.outstanding(), 1);
        let bank = shared.prepare_transmit(&source).unwrap();
        let (lease, pending) = bank.into_retained();
        assert_eq!(pending.count(), 0);
        resources.release_hsdpa(lease).unwrap();
        assert_eq!(resources.outstanding(), 0);
    }

    #[test]
    fn cell_queue_budget_covers_all_configured_users_before_reserving_codes() {
        let mut config = config();
        config.hspa.downlink.enabled = true;
        config.runtime.maximum_ues = 2;
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let required = 2 * config.hspa.downlink.resources.maximum_queue_bytes_per_ue;
        config.hspa.downlink.resources.maximum_queue_bytes = required - 1;
        assert!(SharedChannels::reserve(&config, &mut resources, None).is_err());
        assert_eq!(resources.outstanding(), 0);
        config.hspa.downlink.resources.maximum_queue_bytes = required;
        let mut shared = SharedChannels::reserve(&config, &mut resources, None)
            .unwrap()
            .unwrap();
        let bank = shared.prepare_transmit(&source).unwrap();
        assert_eq!(
            shared.downlink_origin,
            Some(ChipInstant {
                index: source.next_chip().index + crate::hsdpa_tx::CONTROL_LEAD_CHIPS,
                ..source.next_chip()
            })
        );
        let (lease, pending) = bank.into_retained();
        assert_eq!(pending.count(), 0);
        resources.release_hsdpa(lease).unwrap();
    }
}
