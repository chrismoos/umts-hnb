// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::radio_resources::PreparedRadioChange;
use crate::rel99_admission::ActiveRadio;
use umts_rrc::dedicated::{ExistingDchSetup, ScheduledActivation};

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct PlannedExisting {
    previous: Arc<CompiledRel99>,
    compiled: Arc<CompiledRel99>,
    profiles: Arc<UserRlcProfiles>,
    policies: Vec<(BearerId, UserRlcPolicy)>,
    new_rabs: Vec<NewRab>,
    rabs: Vec<RabPlan>,
    retained_rabs: Vec<RabPlan>,
    budgets: Budgets,
}

#[must_use = "Cancel the staged radio change or transfer it into the bearer transaction"]
#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct PreparedExisting {
    pub change: PreparedRadioChange,
    pub setup: ExistingDchSetup,
    pub profiles: Arc<UserRlcProfiles>,
    pub rabs: Vec<RabPlan>,
    pub retained_rabs: Vec<RabPlan>,
}

impl std::fmt::Debug for PreparedExisting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedExisting")
            .field("identity", &self.change.receive_identity())
            .field("rabs", &self.rabs)
            .finish_non_exhaustive()
    }
}

#[must_use = "Retain any staged radio change until rollback completes"]
pub struct RejectedExistingPreparation {
    pub error: Error,
    pub change: Option<PreparedRadioChange>,
}

impl std::fmt::Debug for RejectedExistingPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedExistingPreparation")
            .field("error", &self.error)
            .field("staged_radio_change", &self.change.is_some())
            .finish()
    }
}

impl PlannedExisting {
    #[cfg(test)]
    pub fn previous(&self) -> &Arc<CompiledRel99> {
        &self.previous
    }
    pub fn compiled(&self) -> &Arc<CompiledRel99> {
        &self.compiled
    }
    #[cfg(test)]
    pub fn profiles(&self) -> &Arc<UserRlcProfiles> {
        &self.profiles
    }
    pub fn rabs(&self) -> &[RabPlan] {
        &self.rabs
    }
    #[cfg(test)]
    pub fn retained_rabs(&self) -> &[RabPlan] {
        &self.retained_rabs
    }

    pub fn allocate(
        &self,
        active: ActiveRadio<'_>,
        signalling: &CommonSignalling,
        resources: &mut RadioResources,
        activation: ScheduledActivation,
    ) -> Result<PreparedExisting, Box<RejectedExistingPreparation>> {
        let rejected = |error| {
            Box::new(RejectedExistingPreparation {
                error,
                change: None,
            })
        };
        let previous = active.identity();
        if !Arc::ptr_eq(active.plan(), &self.previous)
            || signalling.dedicated_route(previous.ue) != Some(previous)
        {
            return Err(rejected(super::trace_identity("existing-rejected")));
        }
        let change = resources
            .prepare_reconfiguration_for_ue(
                active.lease(),
                &self.previous,
                self.compiled.configuration(),
                signalling,
                self.budgets.maximum_total_coded_bits,
                self.budgets.transport,
            )
            .map_err(|error| rejected(Error::Resources(error)))?;
        let result = (|| {
            let setup = ExistingDchSetup::new(
                self.previous.configuration().clone(),
                change.plan().configuration().clone(),
                self.new_rabs.clone(),
                activation,
            )?;
            let profiles = UserRlcProfiles::additional(
                change.plan().clone(),
                &self.previous,
                &self.policies,
                self.budgets.maximum_user_storage_bytes,
            )
            .map_err(Error::Rlc)?;
            Ok((setup, Arc::new(profiles)))
        })();
        match result {
            Ok((setup, profiles)) => Ok(PreparedExisting {
                change,
                setup,
                profiles,
                rabs: self.rabs.clone(),
                retained_rabs: self.retained_rabs.clone(),
            }),
            Err(error) => Err(Box::new(RejectedExistingPreparation {
                error,
                change: Some(change),
            })),
        }
    }
}

pub fn plan_existing_for_ue(
    active: &ActiveRadio<'_>,
    signalling: &CommonSignalling,
    active_rabs: &[RabPlan],
    rabs: &[Rab<'_>],
    policy: &Policy<'_>,
) -> Result<PlannedExisting, Error> {
    let identity = active.identity();
    if signalling.dedicated_route(identity.ue) != Some(identity) {
        return Err(Error::Signalling(crate::signalling::SignallingError::Phase));
    }
    let report = signalling
        .capabilities(identity.ue)
        .ok_or(Error::Resources(
            crate::radio_resources::Error::MissingCapabilities,
        ))?;
    if report.key() != identity.ue {
        return Err(super::trace_identity("existing-1"));
    }
    let combined = super::simultaneous::combined(
        active_rabs
            .iter()
            .map(|rab| rab.domain)
            .chain(rabs.iter().map(|rab| rab.domain)),
    );
    let hsdpa = super::simultaneous::hsdpa(report, policy);
    let (capabilities, mut physical) = super::simultaneous::resolve(report, policy, combined)?;
    super::simultaneous::prefer_packet_capacity(
        &mut physical,
        hsdpa,
        active_rabs
            .iter()
            .map(|rab| rab.domain)
            .chain(rabs.iter().map(|rab| rab.domain)),
    );
    let policy = Policy {
        physical: &physical,
        hsdpa,
        ..*policy
    };
    let result = plan_existing(
        active.plan().clone(),
        active_rabs,
        rabs,
        &capabilities,
        &policy,
    )?;
    signalling
        .validate_retained_configuration(identity.ue, result.compiled.configuration())
        .map_err(Error::Signalling)?;
    Ok(result)
}

pub fn plan_existing(
    previous: Arc<CompiledRel99>,
    active_rabs: &[RabPlan],
    rabs: &[Rab<'_>],
    capabilities: &Rel99Limits,
    policy: &Policy<'_>,
) -> Result<PlannedExisting, Error> {
    if rabs.is_empty()
        || rabs.len() > MAX_RABS
        || policy.physical.is_empty()
        || policy.physical.len() > MAX_PHYSICAL_CANDIDATES
        || policy.maximum_combinations.get() > MAX_COMBINATIONS
    {
        return Err(Error::Capacity);
    }
    super::validate_new_rab_domains(rabs)?;
    validate_active(&previous, active_rabs)?;
    if rabs.iter().any(|rab| {
        active_rabs
            .iter()
            .any(|old| old.domain == rab.domain && old.rab == rab.request.first.id.get())
    }) {
        return Err(Error::Unsupported);
    }
    let ComposedServices {
        composition,
        new_rabs,
        metadata,
        policies,
    } = compose_services(
        compose::Composition::from_existing(previous.configuration())?,
        rabs,
        policy.hsdpa,
    )?;
    let mut failure = Error::Capacity;
    for template in policy.physical {
        let result = (|| {
            let old = previous.configuration().physical();
            let mut physical = template.clone();
            physical.primary_scrambling = old.primary_scrambling;
            physical.uplink_scrambling = old.uplink_scrambling;
            physical.default_offset_512_chips = old.default_offset_512_chips;
            physical.frame_offset_256_chips = old.frame_offset_256_chips;
            if physical.downlink_sf == old.downlink_sf
                && physical.downlink_codes.len() == old.downlink_codes.len()
            {
                physical.downlink_codes.clone_from(&old.downlink_codes);
            }
            let configuration =
                Arc::new(composition.configuration(physical, capabilities, policy)?);
            capabilities
                .validate_dch(&configuration)
                .map_err(Error::Capability)?;
            service::validate_guaranteed(&configuration, metadata.iter().chain(active_rabs))?;
            let compiled = Arc::new(
                CompiledRel99::new(
                    configuration,
                    policy.maximum_total_coded_bits,
                    policy.transport,
                )
                .map_err(Error::Plan)?,
            );
            capabilities
                .validate_downlink_matched_bits(
                    compiled
                        .maximum_downlink_matched_tti_bits()
                        .map_err(Error::Plan)?,
                )
                .map_err(Error::Capability)?;
            let profiles = Arc::new(
                UserRlcProfiles::additional(
                    compiled.clone(),
                    &previous,
                    &policies,
                    policy.maximum_user_storage_bytes,
                )
                .map_err(Error::Rlc)?,
            );
            Ok(PlannedExisting {
                previous: previous.clone(),
                compiled,
                profiles,
                policies: policies.clone(),
                new_rabs: new_rabs.clone(),
                rabs: metadata.clone(),
                retained_rabs: active_rabs.to_vec(),
                budgets: Budgets {
                    maximum_total_coded_bits: policy.maximum_total_coded_bits,
                    maximum_user_storage_bytes: policy.maximum_user_storage_bytes,
                    transport: policy.transport,
                },
            })
        })();
        match result {
            Ok(plan) => return Ok(plan),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

pub(super) fn validate_active(previous: &CompiledRel99, rabs: &[RabPlan]) -> Result<(), Error> {
    let mut bearers = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for rab in rabs {
        let domain = match rab.domain {
            CnDomain::Cs => 0,
            CnDomain::Ps => 1,
        };
        if !identities.insert((domain, rab.rab)) || rab.bearers.is_empty() {
            return Err(super::trace_identity("existing-2"));
        }
        for id in &rab.bearers {
            if !bearers.insert(*id)
                || previous
                    .configuration()
                    .bearer(*id)
                    .is_none_or(|old| old.domain != Some(rab.domain))
            {
                return Err(super::trace_identity("existing-3"));
            }
        }
    }
    if previous
        .configuration()
        .bearers()
        .iter()
        .filter(|rb| rb.domain.is_some())
        .count()
        != bearers.len()
    {
        return Err(super::trace_identity("existing-4"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
