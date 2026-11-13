// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_bearers::{BearerError, BearerId, UserRlcPolicy, UserRlcProfiles},
    radio_resources::{AllocatedRadio, RadioResources},
    rel99_plan::{CompiledRel99, Rel99PlanError},
    signalling::CommonSignalling,
};
use std::{num::NonZeroUsize, sync::Arc};
use umts_dsp::dedicated_transport::TransportPlanLimits;
use umts_iuh::ranap::rab::{SetupOrModify, qos};
use umts_rlc::transparent::ErrorDelivery;
use umts_rrc::{
    capabilities::rel99::Rel99Limits,
    dedicated::{
        DchDirection, DedicatedError, GainFactors, InitialDchSetup, NewRab, NormalDpch,
        Rel99Configuration, Rel99Rlc, RetainedBearer,
    },
    transport::{ChannelCoding, CrcSize, TransportInterval},
};
use umts_types::{CnDomain, UeKey};

mod admission;
mod compose;
#[cfg(test)]
pub(crate) use admission::build_radio_owners;
pub use admission::{BuiltExisting, InitialOwnerSettings, InitialPowerControl, OwnerBuildError};
mod configured;
mod simultaneous;
pub use configured::ConfiguredPolicy;
mod existing;
mod release;
#[cfg(test)]
pub use existing::plan_existing;
pub use existing::{PlannedExisting, PreparedExisting, plan_existing_for_ue};
pub use release::{BuiltRelease, PreparedRelease, RejectedReleaseOwners, plan_release_for_ue};
mod catalog;
pub use catalog::{ConfiguredServices, SelectionError};
mod requirements;
mod service;
pub use requirements::Error as RequestError;
use requirements::Requirements;
use service::{validate_circuit, validate_packet};

const MAX_RABS: usize = 16;
const MAX_PHYSICAL_CANDIDATES: usize = 16;
const MAX_COMBINATIONS: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct Coding {
    pub channel: ChannelCoding,
    pub crc: CrcSize,
    pub rate_matching_attribute: u16,
}
#[derive(Clone, Copy, Debug)]
pub enum EmptyTransport {
    NoBlock,
    ZeroSizeBlock,
}
#[derive(Clone, Debug)]
pub struct Reliability {
    pub residual: qos::ResidualError,
    pub sdu: Option<qos::SduError>,
    pub delivery: qos::Delivery,
}
#[derive(Clone, Debug)]
pub struct Contract {
    pub traffic_class: qos::TrafficClass,
    pub delivery_order: qos::DeliveryOrder,
    pub maximum_transfer_delay_ms: Option<u16>,
    pub subflows: Vec<Reliability>,
}
#[derive(Clone, Copy, Debug)]
pub struct CircuitSubflow {
    pub uplink: Coding,
    pub downlink: Coding,
    pub empty: EmptyTransport,
}
pub struct CircuitProfile {
    pub contract: Contract,
    pub interval: TransportInterval,
    pub subflows: Vec<CircuitSubflow>,
    pub supported_formats: Vec<Vec<u16>>,
    pub versions: u16,
}
#[derive(Clone, Copy, Debug)]
pub struct PacketChannel {
    pub interval: TransportInterval,
    pub rlc_bits: u16,
    /// A service-profile payload budget, not the RLC PDU width or measured goodput.
    pub payload_budget_bits: u16,
    pub maximum_blocks: u16,
    pub coding: Coding,
}
pub struct PacketProfile {
    pub contract: Contract,
    pub rlc: Rel99Rlc,
    pub policy: UserRlcPolicy,
    pub packet_capacity: NonZeroUsize,
    pub uplink: PacketChannel,
    pub downlink: PacketChannel,
    /// The same service with the downlink RLC sized for HS-DSCH, used for HSDPA-capable UEs.
    pub high_speed: Option<Box<PacketProfile>>,
}
/// Downlink AM RLC geometry for packet bearers whose downlink rides HS-DSCH.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighSpeedDownlink {
    pub rlc_bits: u16,
    pub receive_window: u16,
}
#[derive(Clone, Copy)]
pub enum Service<'a> {
    Circuit(&'a CircuitProfile),
    Packet(&'a PacketProfile),
}
pub struct Rab<'a> {
    pub domain: CnDomain,
    pub request: &'a SetupOrModify,
    pub service: Service<'a>,
}
#[derive(Clone, Copy, Debug)]
pub struct SignallingProfile {
    pub uplink: Coding,
    pub downlink: Coding,
    pub interval: TransportInterval,
    pub maximum_blocks: u16,
    pub um_uplink_bits: u16,
    pub um_downlink_bits: u16,
}
#[derive(Clone, Copy)]
pub struct Policy<'a> {
    pub hsdpa: bool,
    pub hsupa: bool,
    pub signalling: SignallingProfile,
    pub physical: &'a [NormalDpch],
    pub uplink_gains: GainFactors,
    pub maximum_combinations: NonZeroUsize,
    pub maximum_total_coded_bits: NonZeroUsize,
    pub maximum_user_storage_bytes: NonZeroUsize,
    pub transport: TransportPlanLimits,
}
#[derive(Debug)]
pub enum Error {
    Identity,
    Capacity,
    CombinationBudget {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        direction: DchDirection,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        required: usize,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        maximum: usize,
    },
    CombinationClosure {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        direction: DchDirection,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        interval: TransportInterval,
    },
    Unsupported,
    Contract {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        rab: u8,
    },
    Rate {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        rab: u8,
    },
    Requirements(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        RequestError,
    ),
    Capability(umts_rrc::capabilities::rel99::Error),
    Configuration(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        DedicatedError,
    ),
    Plan(Rel99PlanError),
    Rlc(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        BearerError,
    ),
    Resources(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        crate::radio_resources::Error,
    ),
    Signalling(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::signalling::SignallingError,
    ),
}
impl From<DedicatedError> for Error {
    fn from(error: DedicatedError) -> Self {
        Self::Configuration(error)
    }
}

#[derive(Clone, Debug)]
pub struct PacketCapacity {
    pub maximum_blocks: [u16; 2],
    pub minimum_blocks: [u16; 2],
}

#[derive(Clone, Debug)]
pub struct RabPlan {
    pub domain: CnDomain,
    pub rab: u8,
    pub bearers: Vec<u8>,
    pub maximum: qos::Rates,
    pub guaranteed: Option<qos::Rates>,
    /// Uplink then downlink payload bits credited per packet transport block.
    pub packet_payload_budget_bits: Option<[u16; 2]>,
    pub packet_capacity: Option<PacketCapacity>,
    pub maximum_sdu_bits: u16,
    pub circuit_interval: Option<TransportInterval>,
    pub circuit_versions: Option<u16>,
    pub combinations: Vec<qos::Combination>,
}
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct PlannedInitial {
    setup: InitialDchSetup,
    compiled: Arc<CompiledRel99>,
    profiles: Arc<UserRlcProfiles>,
    policies: Vec<(BearerId, UserRlcPolicy)>,
    rabs: Vec<RabPlan>,
    retained: Vec<RetainedBearer>,
    budgets: Budgets,
}
#[derive(Clone, Copy)]
struct Budgets {
    maximum_total_coded_bits: NonZeroUsize,
    maximum_user_storage_bytes: NonZeroUsize,
    transport: TransportPlanLimits,
}
pub struct PreparedInitial {
    pub allocation: AllocatedRadio,
    pub setup: InitialDchSetup,
    pub profiles: Arc<UserRlcProfiles>,
    pub rabs: Vec<RabPlan>,
}
impl PlannedInitial {
    pub fn setup(&self) -> &InitialDchSetup {
        &self.setup
    }
    #[cfg(test)]
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

    pub fn allocate(
        &self,
        key: UeKey,
        signalling: &CommonSignalling,
        resources: &mut RadioResources,
    ) -> Result<PreparedInitial, Error> {
        if signalling.dedicated_route(key).is_some() {
            return Err(Error::Signalling(crate::signalling::SignallingError::Phase));
        }
        let retained = signalling
            .retained_signalling(key)
            .map_err(Error::Signalling)?;
        if retained.len() != self.retained.len()
            || self.retained.iter().any(|old| {
                !retained.iter().any(|current| {
                    old.id == current.id
                        && old.domain == current.domain
                        && old.rlc == current.rlc
                        && old.uplink_pdu_bits == current.uplink_pdu_bits
                        && old.downlink_pdu_bits == current.downlink_pdu_bits
                })
            })
        {
            tracing::debug!(
                "Rel99 planner identity rejection: initial-retained planned={:?} current={retained:?}",
                self.retained
            );
            return Err(trace_identity("planner-1"));
        }
        signalling
            .validate_retained_configuration(key, self.setup.configuration())
            .map_err(Error::Signalling)?;
        let allocation = resources
            .prepare_for_ue(
                key,
                self.setup.configuration(),
                signalling,
                self.budgets.maximum_total_coded_bits,
                self.budgets.transport,
            )
            .map_err(Error::Resources)?;
        let construction = (|| {
            let setup = InitialDchSetup::new(
                allocation.plan().configuration().clone(),
                retained.to_vec(),
                self.setup.rabs().to_vec(),
            )?;
            let profiles = UserRlcProfiles::new(
                allocation.plan().clone(),
                &self.policies,
                self.budgets.maximum_user_storage_bytes,
            )
            .map_err(Error::Rlc)?;
            Ok((setup, Arc::new(profiles)))
        })();
        let (setup, profiles) = match construction {
            Ok(value) => value,
            Err(error) => {
                resources.cancel(allocation).expect(
                    "same exclusively borrowed allocator created this unpublished allocation",
                );
                return Err(error);
            }
        };
        Ok(PreparedInitial {
            allocation,
            setup,
            profiles,
            rabs: self.rabs.clone(),
        })
    }
}

pub fn plan_initial_for_ue(
    key: UeKey,
    signalling: &CommonSignalling,
    rabs: &[Rab<'_>],
    policy: &Policy<'_>,
) -> Result<PlannedInitial, Error> {
    if signalling.dedicated_route(key).is_some() {
        return Err(Error::Signalling(crate::signalling::SignallingError::Phase));
    }
    let retained = signalling
        .retained_signalling(key)
        .map_err(Error::Signalling)?;
    let report = signalling.capabilities(key).ok_or(Error::Resources(
        crate::radio_resources::Error::MissingCapabilities,
    ))?;
    if report.key() != key {
        return Err(trace_identity("planner-2"));
    }
    let hsdpa = simultaneous::hsdpa(report, policy);
    let (capabilities, mut physical) = simultaneous::resolve(report, policy, false)?;
    simultaneous::prefer_packet_capacity(&mut physical, hsdpa, rabs.iter().map(|rab| rab.domain));
    let policy = Policy {
        physical: &physical,
        hsdpa,
        ..*policy
    };
    let plan = plan_initial(&retained, rabs, &capabilities, &policy)?;
    signalling
        .validate_retained_configuration(key, plan.setup.configuration())
        .map_err(Error::Signalling)?;
    Ok(plan)
}

pub fn plan_initial(
    retained: &[RetainedBearer],
    rabs: &[Rab<'_>],
    capabilities: &Rel99Limits,
    policy: &Policy<'_>,
) -> Result<PlannedInitial, Error> {
    if rabs.is_empty()
        || rabs.len() > MAX_RABS
        || policy.physical.is_empty()
        || policy.physical.len() > MAX_PHYSICAL_CANDIDATES
        || policy.maximum_combinations.get() > MAX_COMBINATIONS
    {
        return Err(Error::Capacity);
    }
    validate_new_rab_domains(rabs)?;
    let composition = compose::Composition::new(retained, policy.signalling)?;
    let services = compose_services(composition, rabs, policy.hsdpa)?;
    finish_initial(retained, capabilities, policy, services)
}

fn validate_new_rab_domains(rabs: &[Rab<'_>]) -> Result<(), Error> {
    let Some(first) = rabs.first() else {
        return Err(Error::Capacity);
    };
    if rabs.iter().any(|rab| rab.domain != first.domain) {
        return Err(Error::Unsupported);
    }
    Ok(())
}

struct ComposedServices {
    composition: compose::Composition,
    new_rabs: Vec<NewRab>,
    metadata: Vec<RabPlan>,
    policies: Vec<(BearerId, UserRlcPolicy)>,
}

fn compose_services(
    mut composition: compose::Composition,
    rabs: &[Rab<'_>],
    hsdpa: bool,
) -> Result<ComposedServices, Error> {
    if rabs.is_empty() || rabs.len() > MAX_RABS {
        return Err(Error::Capacity);
    }
    let mut new_rabs = Vec::with_capacity(rabs.len());
    let mut metadata = Vec::with_capacity(rabs.len());
    let mut policies = Vec::new();
    for rab in rabs {
        // A fixed-size AM entity keeps one PDU size for its lifetime, so the choice follows
        // the UE's HSDPA admission and not the bearer's current transport mapping.
        let service = match rab.service {
            Service::Packet(profile) if hsdpa => {
                Service::Packet(profile.high_speed.as_deref().unwrap_or(profile))
            }
            service => service,
        };
        let requirements =
            Requirements::new(rab.domain, rab.request).map_err(Error::Requirements)?;
        if metadata
            .iter()
            .any(|old: &RabPlan| old.domain == rab.domain && old.rab == requirements.rab)
        {
            return Err(trace_identity("planner-3"));
        }
        let packet_capacity = match service {
            Service::Packet(profile) => {
                let mut maximum_blocks = validate_packet(&requirements, profile)?;
                let mut minimum_blocks = service::packet_minimum(&requirements, profile)?;
                if hsdpa {
                    maximum_blocks[1] = 0;
                    minimum_blocks[1] = 0;
                } else if maximum_blocks[1] == 0 {
                    return Err(Error::Unsupported);
                } else {
                    minimum_blocks[0] = minimum_blocks[0].max(1);
                    minimum_blocks[1] = minimum_blocks[1].max(1);
                }
                Some(PacketCapacity {
                    maximum_blocks,
                    minimum_blocks,
                })
            }
            Service::Circuit(_) => None,
        };
        let ids = match service {
            Service::Circuit(profile) => {
                validate_circuit(&requirements, profile)?;
                let ids = composition.circuit(&requirements, profile)?;
                for (index, id) in ids.iter().copied().enumerate() {
                    let maximum = requirements
                        .combinations
                        .iter()
                        .map(|row| usize::from(row.subflow_bits[index]))
                        .max()
                        .ok_or(Error::Capacity)?
                        .max(1);
                    let errors = match profile.contract.subflows[index].delivery {
                        qos::Delivery::No => ErrorDelivery::Discard,
                        qos::Delivery::Yes => ErrorDelivery::Indicate,
                        qos::Delivery::NoDetection => ErrorDelivery::NoDetect,
                    };
                    policies.push((
                        BearerId::new(id).map_err(Error::Rlc)?,
                        UserRlcPolicy::Transparent {
                            errors,
                            maximum_sdu_bits: NonZeroUsize::new(maximum)
                                .expect("positive subflow budget"),
                        },
                    ));
                }
                ids
            }
            Service::Packet(profile) => {
                let capacity = packet_capacity.as_ref().expect("packet capacity validated");
                let id = composition.packet_with_minimum(
                    profile,
                    capacity.maximum_blocks,
                    capacity.minimum_blocks,
                )?;
                policies.push((
                    BearerId::new(id).map_err(Error::Rlc)?,
                    service::packet_policy(&requirements, profile)?,
                ));
                vec![id]
            }
        };
        new_rabs.push(NewRab {
            identity: requirements.rab,
            domain: rab.domain,
            nas_synchronization: requirements.nas,
            bearers: ids.clone(),
        });
        metadata.push(RabPlan {
            domain: rab.domain,
            rab: requirements.rab,
            bearers: ids,
            maximum: requirements.maximum,
            guaranteed: requirements.guaranteed,
            packet_payload_budget_bits: match service {
                Service::Packet(profile) => Some([
                    profile.uplink.payload_budget_bits,
                    profile.downlink.payload_budget_bits,
                ]),
                Service::Circuit(_) => None,
            },
            packet_capacity,
            maximum_sdu_bits: requirements.parameters.maximum_sdu_bits.get(),
            circuit_interval: match service {
                Service::Circuit(profile) => Some(profile.interval),
                Service::Packet(_) => None,
            },
            circuit_versions: match service {
                Service::Circuit(profile) => {
                    Some(profile.versions & requirements.user_plane.version_mask())
                }
                Service::Packet(_) => None,
            },
            combinations: requirements.combinations,
        });
    }
    Ok(ComposedServices {
        composition,
        new_rabs,
        metadata,
        policies,
    })
}

fn finish_initial(
    retained: &[RetainedBearer],
    capabilities: &Rel99Limits,
    policy: &Policy<'_>,
    services: ComposedServices,
) -> Result<PlannedInitial, Error> {
    let ComposedServices {
        composition,
        new_rabs,
        metadata,
        policies,
    } = services;
    let mut failure = Error::Capacity;
    for physical in policy.physical {
        let result = (|| {
            let configuration =
                Arc::new(composition.configuration(physical.clone(), capabilities, policy)?);
            capabilities
                .validate_dch(&configuration)
                .map_err(Error::Capability)?;
            service::validate_guaranteed(&configuration, &metadata)?;
            let compiled = Arc::new(
                CompiledRel99::new(
                    configuration.clone(),
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
            let setup = InitialDchSetup::new(configuration, retained.to_vec(), new_rabs.clone())?;
            let profiles = Arc::new(
                UserRlcProfiles::new(
                    compiled.clone(),
                    &policies,
                    policy.maximum_user_storage_bytes,
                )
                .map_err(Error::Rlc)?,
            );
            Ok(PlannedInitial {
                setup,
                compiled,
                profiles,
                policies: policies.clone(),
                rabs: metadata.clone(),
                retained: retained.to_vec(),
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

#[cfg(test)]
pub(crate) mod tests;

/// The label names which identity check rejected a bearer re-plan.
pub(crate) fn trace_identity(label: &str) -> Error {
    tracing::debug!("Rel99 planner identity rejection: {label}");
    Error::Identity
}
