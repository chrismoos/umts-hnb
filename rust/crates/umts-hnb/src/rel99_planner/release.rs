// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_tx::DedicatedTransmitPort;
use crate::rel99_admission::ScheduledReleaseRequest;
use crate::{
    dedicated_rx::ReceiveIdentity, radio_resources::PreparedRadioChange,
    rel99_admission::ActiveRadio,
};
use umts_rrc::dedicated::{Combination, DchDirection, DchTfcs, Rel99Bearer};
use umts_rrc::dedicated::{ExistingDchRelease, RadioBearerReleaseProcedure, ScheduledActivation};
use umts_types::time::CONNECTION_FRAME_MODULUS;

const MAX_RELEASE_BEARERS: usize = 32;

#[cfg(test)]
mod tests;

pub struct PlannedRelease {
    previous: Arc<CompiledRel99>,
    compiled: Arc<CompiledRel99>,
    released: Vec<BearerId>,
    budgets: Budgets,
}
#[must_use = "Cancel the staged radio change or transfer it into the bearer transaction"]
#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct PreparedRelease {
    pub change: PreparedRadioChange,
    pub previous: Arc<CompiledRel99>,
    pub released: Vec<BearerId>,
    pub release: ExistingDchRelease,
}
#[must_use = "Retain any staged radio change until rollback completes"]
pub struct RejectedReleasePreparation {
    pub error: Error,
    pub change: Option<PreparedRadioChange>,
}
impl std::fmt::Debug for RejectedReleasePreparation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedReleasePreparation")
            .field("error", &self.error)
            .field("staged_radio_change", &self.change.is_some())
            .finish()
    }
}
#[must_use = "Transfer the request to admission or cancel its staged change through its original owner"]
pub struct BuiltRelease {
    pub request: Box<ScheduledReleaseRequest>,
}
impl BuiltRelease {}
#[must_use = "Retain the prepared release until its original owner cancels the staged change"]
pub struct RejectedReleaseOwners {
    pub error: OwnerBuildError,
    pub prepared: Box<PreparedRelease>,
}
impl std::fmt::Debug for RejectedReleaseOwners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedReleaseOwners")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl PlannedRelease {
    #[cfg(test)]
    pub fn previous(&self) -> &Arc<CompiledRel99> {
        &self.previous
    }
    pub fn compiled(&self) -> &Arc<CompiledRel99> {
        &self.compiled
    }
    #[cfg(test)]
    pub fn released(&self) -> &[BearerId] {
        &self.released
    }
    pub fn allocate(
        &self,
        active: ActiveRadio<'_>,
        signalling: &CommonSignalling,
        resources: &mut RadioResources,
        activation: ScheduledActivation,
    ) -> Result<PreparedRelease, Box<RejectedReleasePreparation>> {
        let rejected = |error| {
            Box::new(RejectedReleasePreparation {
                error,
                change: None,
            })
        };
        if !Arc::ptr_eq(active.plan(), &self.previous)
            || signalling.dedicated_route(active.identity().ue) != Some(active.identity())
        {
            return Err(rejected(super::trace_identity("release-rejected")));
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
        match ExistingDchRelease::new(
            self.previous.configuration().clone(),
            change.plan().configuration().clone(),
            activation,
        ) {
            Ok(release) => Ok(PreparedRelease {
                change,
                previous: self.previous.clone(),
                released: self.released.clone(),
                release,
            }),
            Err(error) => Err(Box::new(RejectedReleasePreparation {
                error: Error::Configuration(error),
                change: Some(change),
            })),
        }
    }
}
impl PreparedRelease {
    pub fn identity(&self) -> ReceiveIdentity {
        self.change.receive_identity()
    }
    pub fn plan(&self) -> &Arc<CompiledRel99> {
        self.change.plan()
    }
    pub fn set_activation(
        &mut self,
        activation: ScheduledActivation,
    ) -> Result<(), DedicatedError> {
        self.release = ExistingDchRelease::new(
            self.previous.configuration().clone(),
            self.plan().configuration().clone(),
            activation,
        )?
        .with_retained_hsdpa(self.release.retained_hsdpa().cloned());
        Ok(())
    }
    pub fn build_owners(
        self,
        active: &ActiveRadio<'_>,
        radio: &mut CommonSignalling,
        port: &DedicatedTransmitPort,
        settings: InitialOwnerSettings,
    ) -> Result<BuiltRelease, Box<RejectedReleaseOwners>> {
        let mut build = || {
            let previous = active.identity();
            let identity = self.identity();
            let plan = self.plan();
            let activation = self.release.activation();
            if identity.ue != previous.ue
                || identity.configuration <= previous.configuration
                || !Arc::ptr_eq(&self.previous, active.plan())
                || !Arc::ptr_eq(self.release.previous(), active.plan().configuration())
                || !Arc::ptr_eq(self.release.configuration(), plan.configuration())
                || settings.timing.downlink.time != activation.time()
                || settings.timing.downlink.cfn != activation.cfn()
                || settings.timing.uplink.time.epoch != previous.ue.epoch
                || settings.timing.uplink.cfn != activation.cfn()
                || settings.timing.uplink_frame % CONNECTION_FRAME_MODULUS
                    != u64::from(activation.cfn())
                || settings.timing.downlink_frame % CONNECTION_FRAME_MODULUS
                    != u64::from(activation.cfn())
            {
                return Err(OwnerBuildError::Identity);
            }
            if radio.dedicated_route(previous.ue) != Some(previous) {
                return Err(OwnerBuildError::Phase);
            }
            radio
                .validate_retained_configuration(previous.ue, plan.configuration())
                .map_err(OwnerBuildError::Signalling)?;
            let release = ExistingDchRelease::new(
                self.release.previous().clone(),
                plan.configuration().clone(),
                activation,
            )
            .map_err(OwnerBuildError::Procedure)?
            .with_retained_hsdpa(self.release.retained_hsdpa().cloned());
            let procedure = RadioBearerReleaseProcedure::new(
                previous.ue,
                radio
                    .next_bearer_release_transaction(previous.ue)
                    .map_err(OwnerBuildError::Signalling)?,
                release,
                settings.now,
                settings.publication_deadline,
                settings.response_timeout,
            )
            .map_err(OwnerBuildError::Procedure)?;
            let owners = super::admission::build_radio_owners(identity, plan, &settings)?;
            let pool = radio
                .allocate_frame_pool(identity, plan.clone(), port, settings.pool)
                .map_err(OwnerBuildError::Signalling)?;
            Ok((procedure, owners, pool))
        };
        match build() {
            Ok((procedure, owners, pool)) => Ok(BuiltRelease {
                request: Box::new(ScheduledReleaseRequest {
                    receiver: Box::new(owners.receiver),
                    transmit: Box::new(owners.transmit),
                    procedure: Box::new(procedure),
                    pool,
                    activation: settings.timing,
                    change: Some(self.change),
                }),
            }),
            Err(error) => Err(Box::new(RejectedReleaseOwners {
                error,
                prepared: Box::new(self),
            })),
        }
    }
}

pub fn plan_release_for_ue(
    active: &ActiveRadio<'_>,
    signalling: &CommonSignalling,
    active_rabs: &[RabPlan],
    released: &[BearerId],
    policy: &Policy<'_>,
) -> Result<PlannedRelease, Error> {
    let identity = active.identity();
    if signalling.dedicated_route(identity.ue) != Some(identity) {
        return Err(super::trace_identity("release-1"));
    }
    let report = signalling
        .capabilities(identity.ue)
        .ok_or(Error::Resources(
            crate::radio_resources::Error::MissingCapabilities,
        ))?;
    if report.key() != identity.ue {
        return Err(super::trace_identity("release-2"));
    }
    let combined = super::simultaneous::combined(
        active_rabs
            .iter()
            .filter(|rab| {
                rab.bearers
                    .iter()
                    .any(|id| !released.iter().any(|released| released.get() == *id))
            })
            .map(|rab| rab.domain),
    );
    let hsdpa = super::simultaneous::hsdpa(report, policy);
    let (capabilities, mut physical) = super::simultaneous::resolve(report, policy, combined)?;
    super::simultaneous::prefer_packet_capacity(
        &mut physical,
        hsdpa,
        active_rabs
            .iter()
            .filter(|rab| {
                rab.bearers
                    .iter()
                    .any(|id| !released.iter().any(|released| released.get() == *id))
            })
            .map(|rab| rab.domain),
    );
    let policy = Policy {
        physical: &physical,
        hsdpa,
        ..*policy
    };
    let planned = plan_release_restoring(
        active.plan().clone(),
        active_rabs,
        released,
        &capabilities,
        &policy,
    )?;
    signalling
        .validate_retained_configuration(identity.ue, planned.compiled.configuration())
        .map_err(Error::Signalling)?;
    Ok(planned)
}

fn plan_release_restoring(
    previous: Arc<CompiledRel99>,
    active_rabs: &[RabPlan],
    released: &[BearerId],
    capabilities: &Rel99Limits,
    policy: &Policy<'_>,
) -> Result<PlannedRelease, Error> {
    super::existing::validate_active(&previous, active_rabs)?;
    let projected = plan_release(previous.clone(), released, capabilities, policy)?;
    let retained: Vec<_> = active_rabs
        .iter()
        .filter(|rab| {
            rab.bearers
                .iter()
                .any(|id| projected.compiled.configuration().bearer(*id).is_some())
        })
        .cloned()
        .collect();
    let composition = super::compose::Composition::restore_packets(
        projected.compiled.configuration(),
        &retained,
    )?;
    let old = previous.configuration().physical();
    let lent = crate::radio_resources::lent_data_codes();
    let borrows = old.secondary_scrambling.is_none()
        && old.downlink_codes.iter().any(|code| {
            let data_code = u32::from(*code) * 16 / u32::from(old.downlink_sf.max(16));
            lent & (1 << data_code) != 0
        });
    let mut candidates: Vec<_> = policy
        .physical
        .iter()
        .filter(|_| borrows)
        .map(|template| {
            let mut physical = template.clone();
            physical.primary_scrambling = old.primary_scrambling;
            physical.secondary_scrambling = old.secondary_scrambling;
            physical.uplink_scrambling = old.uplink_scrambling;
            physical.default_offset_512_chips = old.default_offset_512_chips;
            physical.frame_offset_256_chips = old.frame_offset_256_chips;
            physical
        })
        .filter(|physical| physical.downlink_sf > old.downlink_sf)
        .collect();
    candidates.sort_by_key(|physical| std::cmp::Reverse(physical.downlink_sf));
    candidates.push(old.clone());
    let mut failure = Error::Capacity;
    for physical in candidates {
        let compose = || -> Result<Arc<CompiledRel99>, Error> {
            let configuration =
                Arc::new(composition.configuration(physical, capabilities, policy)?);
            capabilities
                .validate_dch(&configuration)
                .map_err(Error::Capability)?;
            service::validate_guaranteed(&configuration, retained.iter())?;
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
            Ok(compiled)
        };
        match compose() {
            Ok(compiled) => {
                return Ok(PlannedRelease {
                    compiled,
                    ..projected
                });
            }
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

pub fn plan_release(
    previous: Arc<CompiledRel99>,
    released: &[BearerId],
    capabilities: &Rel99Limits,
    policy: &Policy<'_>,
) -> Result<PlannedRelease, Error> {
    if released.is_empty()
        || released.len() > MAX_RELEASE_BEARERS
        || policy.maximum_combinations.get() > MAX_COMBINATIONS
    {
        return Err(Error::Capacity);
    }
    let old = previous.configuration();
    for (index, id) in released.iter().enumerate() {
        if released[..index].contains(id)
            || old.bearer(id.get()).is_none_or(|rb| rb.domain.is_none())
        {
            return Err(super::trace_identity("release-3"));
        }
    }
    let bearers: Vec<_> = old
        .bearers()
        .iter()
        .filter(|rb| !released.iter().any(|id| id.get() == rb.id))
        .cloned()
        .collect();
    let uplink = project(old.uplink(), DchDirection::Uplink, &bearers, policy)?;
    let downlink = project(old.downlink(), DchDirection::Downlink, &bearers, policy)?;
    let configuration = Arc::new(Rel99Configuration::new(
        uplink,
        downlink,
        bearers,
        old.physical().clone(),
    )?);
    capabilities
        .validate_dch(&configuration)
        .map_err(Error::Capability)?;
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
    Ok(PlannedRelease {
        previous,
        compiled,
        released: released.to_vec(),
        budgets: Budgets {
            maximum_total_coded_bits: policy.maximum_total_coded_bits,
            maximum_user_storage_bytes: policy.maximum_user_storage_bytes,
            transport: policy.transport,
        },
    })
}

fn project(
    old: &DchTfcs,
    direction: DchDirection,
    bearers: &[Rel99Bearer],
    policy: &Policy<'_>,
) -> Result<DchTfcs, Error> {
    let columns: Vec<_> = old
        .transports()
        .iter()
        .enumerate()
        .filter(|(_, transport)| {
            bearers.iter().any(|rb| {
                rb.mappings(direction)
                    .any(|mapping| mapping.transport == transport.id)
            })
        })
        .map(|(index, _)| index)
        .collect();
    let transports = columns
        .iter()
        .map(|index| old.transports()[*index].clone())
        .collect();
    let mut combinations: Vec<Combination> = Vec::new();
    combinations
        .try_reserve_exact(
            old.combinations()
                .len()
                .min(policy.maximum_combinations.get()),
        )
        .map_err(|_| Error::Capacity)?;
    for row in old.combinations() {
        let formats: Vec<_> = columns.iter().map(|index| row.formats[*index]).collect();
        if combinations.iter().any(|old| old.formats == formats) {
            continue;
        }
        if combinations.len() == policy.maximum_combinations.get() {
            return Err(Error::Capacity);
        }
        // TS 25.331 10.3.5.8 permits explicit gain factors for each TFC of the reconfigured TFCS.
        combinations.push(Combination {
            formats,
            gain: (direction == DchDirection::Uplink).then_some(policy.uplink_gains),
        });
    }
    DchTfcs::new(direction, transports, combinations).map_err(Error::Configuration)
}
