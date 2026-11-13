// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    BearerEndpoint, BearerError, BearerPhase, DedicatedBearers, EndpointLayout, MAX_BEARERS,
    MappingPhase, ReceiveIdentity,
};
use crate::dedicated_bearers::{MappingActivation, mapping};
use crate::rel99_plan::CompiledRel99;
use std::sync::Arc;
use umts_rrc::dedicated::DchDirection;
use umts_security::counters::CipherMode;
use umts_types::UeKey;

#[must_use = "Retain the addition until it is staged or explicitly dropped"]
pub struct PreparedAddition {
    owner: Arc<()>,
    previous: ReceiveIdentity,
    map: Box<mapping::BearerMapping>,
    endpoints: Vec<BearerEndpoint>,
    storage_bytes: usize,
}

impl std::fmt::Debug for PreparedAddition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedAddition")
            .field("previous", &self.previous)
            .field("identity", &self.map.identity)
            .field("endpoints", &self.endpoints.len())
            .field("storage_bytes", &self.storage_bytes)
            .finish_non_exhaustive()
    }
}

impl PreparedAddition {
    pub fn into_endpoints(self) -> Vec<BearerEndpoint> {
        self.endpoints
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.map.identity
    }

    pub fn storage_bytes(&self) -> usize {
        self.storage_bytes
    }
}

#[must_use = "Recover or explicitly discard the new endpoint candidates"]
pub struct RejectedAdditionPreparation {
    pub error: BearerError,
    pub endpoints: Vec<BearerEndpoint>,
}

impl std::fmt::Debug for RejectedAdditionPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedAdditionPreparation")
            .field("error", &self.error)
            .field("endpoints", &self.endpoints.len())
            .finish()
    }
}

#[must_use = "Retry or roll back the staged addition"]
pub struct RejectedAddition {
    pub error: BearerError,
    pub prepared: PreparedAddition,
}

impl std::fmt::Debug for RejectedAddition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedAddition")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl DedicatedBearers {
    pub fn prepare_addition(
        &self,
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<CompiledRel99>,
        endpoints: Vec<BearerEndpoint>,
    ) -> Result<PreparedAddition, RejectedAdditionPreparation> {
        let mut endpoints = endpoints;
        let result = self.prepare_addition_inner(identity, activation, plan, &endpoints);
        let map = match result {
            Ok(map) => map,
            Err(error) => {
                return Err(RejectedAdditionPreparation { error, endpoints });
            }
        };
        let endpoint_bytes = match endpoints.iter().try_fold(0usize, |bytes, endpoint| {
            bytes
                .checked_add(endpoint.storage_bytes)
                .ok_or(BearerError::Capacity)
        }) {
            Ok(bytes) => bytes,
            Err(error) => return Err(RejectedAdditionPreparation { error, endpoints }),
        };
        let map_bytes = match map.storage_bytes() {
            Ok(bytes) => bytes,
            Err(error) => return Err(RejectedAdditionPreparation { error, endpoints }),
        };
        let storage_bytes = match endpoint_bytes.checked_add(map_bytes) {
            Some(bytes) => bytes,
            None => {
                return Err(RejectedAdditionPreparation {
                    error: BearerError::Capacity,
                    endpoints,
                });
            }
        };
        if self
            .storage_bytes
            .checked_add(storage_bytes)
            .is_none_or(|bytes| bytes > self.maximum_buffer_bytes)
        {
            return Err(RejectedAdditionPreparation {
                error: BearerError::Capacity,
                endpoints,
            });
        }
        let prepared = PreparedAddition {
            owner: self.owner_tag.clone(),
            previous: self.identity(),
            map,
            endpoints: std::mem::take(&mut endpoints),
            storage_bytes,
        };
        Ok(prepared)
    }

    fn prepare_addition_inner(
        &self,
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<CompiledRel99>,
        endpoints: &[BearerEndpoint],
    ) -> Result<Box<mapping::BearerMapping>, BearerError> {
        if self.downlink_recovery_required {
            return Err(BearerError::RecoveryRequired);
        }
        let old = self.current_map();
        if self.phase != BearerPhase::Active
            || old.phase != MappingPhase::Active
            || self.maps.iter().flatten().count() != 1
        {
            return Err(BearerError::Phase);
        }
        if identity.ue != self.owner
            || identity.configuration <= old.identity.configuration
            || endpoints.is_empty()
            || endpoints.len() > MAX_BEARERS.saturating_sub(self.endpoints.len())
        {
            return Err(BearerError::Owner);
        }
        validate_new_endpoints(self.owner, &self.lookup, endpoints)?;
        let mut profiles = None;
        for endpoint in endpoints {
            let super::Entity::AwaitingUser {
                profiles: candidate,
                ..
            } = &endpoint.entity
            else {
                return Err(BearerError::Phase);
            };
            if !Arc::ptr_eq(candidate.plan(), &plan)
                || profiles
                    .as_ref()
                    .is_some_and(|previous| !Arc::ptr_eq(previous, candidate))
            {
                return Err(BearerError::Mapping);
            }
            profiles = Some(candidate.clone());
        }
        let previous = old
            .plan
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .configuration();
        let next = plan.configuration();
        if previous.bearers().iter().any(|bearer| {
            next.bearer(bearer.id)
                .is_none_or(|candidate| candidate != bearer)
        }) {
            return Err(BearerError::UnsupportedBinding);
        }
        if next.bearers().iter().any(|bearer| {
            previous.bearer(bearer.id).is_none()
                && !endpoints
                    .iter()
                    .any(|endpoint| endpoint.id.get() == bearer.id)
        }) {
            return Err(BearerError::Mapping);
        }
        if previous.physical().default_offset_512_chips != next.physical().default_offset_512_chips
            || previous.physical().frame_offset_256_chips != next.physical().frame_offset_256_chips
        {
            return Err(BearerError::UnsupportedBinding);
        }
        if endpoints
            .iter()
            .any(|endpoint| endpoint.mode() == CipherMode::Transparent)
            && activation.uplink_frame != activation.downlink_frame
        {
            return Err(BearerError::Time);
        }
        let downlink = old.downlink.as_ref().ok_or(BearerError::Mapping)?;
        mapping::boundary(
            old.anchor,
            old.first_frame,
            activation.uplink,
            activation.uplink_frame,
            mapping::longest(&old.uplink, plan.uplink_configured()),
        )?;
        mapping::boundary(
            downlink.anchor,
            downlink.first_frame,
            activation.downlink,
            activation.downlink_frame,
            mapping::longest(&downlink.formats, plan.downlink_configured()),
        )?;
        if activation.uplink.cfn != activation.downlink.cfn
            || activation.uplink_frame.checked_sub(old.first_frame)
                != activation.downlink_frame.checked_sub(downlink.first_frame)
        {
            return Err(BearerError::Time);
        }
        let layout = EndpointLayout {
            endpoints: &self.endpoints,
            additional: endpoints,
            lookup: &self.lookup,
        };
        mapping::validate_layout(&plan, layout, mapping::Change::Rebind)?;
        let uplink_bindings = mapping::bindings(&plan, DchDirection::Uplink)?;
        mapping::validate_uplink(
            plan.uplink_configured(),
            &uplink_bindings,
            layout,
            self.input_bits,
        )?;
        let downlink = super::transmit::DownlinkBindings::build(
            self.owner,
            layout,
            activation.downlink,
            activation.downlink_frame,
            plan.downlink_configured().clone(),
            mapping::bindings(&plan, DchDirection::Downlink)?,
        )?;
        let last_transport = vec![None; uplink_bindings.len()];
        let mut added_bearers = Vec::new();
        added_bearers
            .try_reserve_exact(endpoints.len())
            .map_err(|_| BearerError::Capacity)?;
        added_bearers.extend(endpoints.iter().map(|endpoint| endpoint.id));
        let map = Box::new(mapping::BearerMapping {
            identity,
            anchor: activation.uplink,
            first_frame: activation.uplink_frame,
            uplink: plan.uplink_configured().clone(),
            bindings: uplink_bindings,
            last_transport,
            downlink: Some(downlink),
            plan: Some(plan),
            phase: MappingPhase::Staged {
                previous: old.identity,
            },
            additional: true,
            added_bearers,
        });
        self.check_boundary(&map)?;
        Ok(map)
    }

    /// The corresponding RRC construction must retain the serving HS link and MAC-d mappings.
    pub fn retain_hsdpa_for_addition(
        &self,
        prepared: &mut PreparedAddition,
    ) -> Result<(), BearerError> {
        if !Arc::ptr_eq(&self.owner_tag, &prepared.owner) || self.identity() != prepared.previous {
            return Err(BearerError::Owner);
        }
        prepared
            .map
            .downlink
            .as_mut()
            .ok_or(BearerError::Mapping)?
            .inherit_hsdpa(
                self.current_map()
                    .downlink
                    .as_ref()
                    .ok_or(BearerError::Mapping)?,
            )
    }

    pub fn stage_addition(
        &mut self,
        prepared: PreparedAddition,
    ) -> Result<ReceiveIdentity, RejectedAddition> {
        let result = (|| {
            let slot = self.maps[1 - self.current].as_ref();
            if !Arc::ptr_eq(&self.owner_tag, &prepared.owner)
                || self.identity() != prepared.previous
                || self.phase != BearerPhase::Active
                || self.maps.iter().flatten().count() != 1
                || slot.is_some()
                || prepared.map.phase
                    != (MappingPhase::Staged {
                        previous: prepared.previous,
                    })
                || !prepared.map.additional
            {
                return Err(BearerError::Phase);
            }
            validate_new_endpoints(self.owner, &self.lookup, &prepared.endpoints)?;
            self.check_boundary(&prepared.map)?;
            self.check_hsdpa_successor(&prepared.map)?;
            self.storage_bytes
                .checked_add(prepared.storage_bytes)
                .filter(|bytes| *bytes <= self.maximum_buffer_bytes)
                .ok_or(BearerError::Capacity)?;
            self.endpoints
                .try_reserve_exact(prepared.endpoints.len())
                .map_err(|_| BearerError::Capacity)?;
            Ok(())
        })();
        if let Err(error) = result {
            return Err(RejectedAddition { error, prepared });
        }
        let identity = prepared.identity();
        let PreparedAddition {
            map,
            endpoints,
            storage_bytes,
            ..
        } = prepared;
        for endpoint in endpoints {
            self.lookup[endpoint.id.index()] = Some(self.endpoints.len());
            self.endpoints.push(endpoint);
        }
        self.maps[1 - self.current] = Some(map);
        self.storage_bytes += storage_bytes;
        Ok(identity)
    }

    pub fn cancel_addition(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<PreparedAddition, BearerError> {
        if self.phase != BearerPhase::Active
            || self.identity() != previous
            || self.maps.iter().flatten().count() != 2
        {
            return Err(BearerError::Phase);
        }
        let index = self.mapping_index(identity)?;
        let map = self.maps[index].as_ref().ok_or(BearerError::Mapping)?;
        if !map.additional
            || map.phase != (MappingPhase::Staged { previous })
            || map.added_bearers.is_empty()
        {
            return Err(BearerError::Phase);
        }
        let base = self
            .endpoints
            .len()
            .checked_sub(map.added_bearers.len())
            .ok_or(BearerError::Mapping)?;
        for (offset, id) in map.added_bearers.iter().enumerate() {
            if self.lookup[id.index()] != Some(base + offset)
                || self
                    .endpoints
                    .get(base + offset)
                    .is_none_or(|endpoint| endpoint.id != *id)
            {
                return Err(BearerError::Mapping);
            }
        }
        let endpoint_bytes =
            self.endpoints[base..]
                .iter()
                .try_fold(0usize, |bytes, endpoint| {
                    bytes
                        .checked_add(endpoint.storage_bytes)
                        .ok_or(BearerError::Capacity)
                })?;
        let map_bytes = map.storage_bytes()?;
        let storage_bytes = endpoint_bytes
            .checked_add(map_bytes)
            .ok_or(BearerError::Capacity)?;
        let remaining = self
            .storage_bytes
            .checked_sub(storage_bytes)
            .ok_or(BearerError::Capacity)?;
        let map = self.maps[index].take().expect("checked staged addition");
        let endpoints = self.endpoints.split_off(base);
        for endpoint in &endpoints {
            self.lookup[endpoint.id.index()] = None;
        }
        self.storage_bytes = remaining;
        Ok(PreparedAddition {
            owner: self.owner_tag.clone(),
            previous,
            map,
            endpoints,
            storage_bytes,
        })
    }
}

fn validate_new_endpoints(
    owner: UeKey,
    lookup: &[Option<usize>; MAX_BEARERS],
    endpoints: &[BearerEndpoint],
) -> Result<(), BearerError> {
    let mut seen = [false; MAX_BEARERS];
    for endpoint in endpoints {
        if endpoint.ue != owner
            || lookup[endpoint.id.index()].is_some()
            || std::mem::replace(&mut seen[endpoint.id.index()], true)
        {
            return Err(BearerError::Owner);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::{NonZeroU64, NonZeroUsize};
    use umts_rlc::transparent::{ErrorDelivery, Segmentation, TmLimits};
    use umts_types::{CnDomain, RadioEpoch};

    fn owner() -> UeKey {
        UeKey {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            slot: 0,
            generation: NonZeroU64::MIN,
        }
    }

    fn endpoint(id: u8) -> BearerEndpoint {
        BearerEndpoint::transparent(
            owner(),
            super::super::BearerId::new(id).unwrap(),
            super::super::Scope::User(CnDomain::Ps),
            super::super::TransparentSettings {
                uplink_segmentation: Segmentation::Disabled,
                downlink_segmentation: Segmentation::Disabled,
                errors: ErrorDelivery::Indicate,
                limits: TmLimits {
                    maximum_pdus: NonZeroUsize::new(1).unwrap(),
                    maximum_pdu_bits: NonZeroUsize::new(8).unwrap(),
                    maximum_sdu_bits: NonZeroUsize::new(8).unwrap(),
                },
            },
        )
        .unwrap()
    }

    #[test]
    fn additional_endpoint_validation_rejects_existing_and_duplicate_ids() {
        let mut lookup = [None; MAX_BEARERS];
        lookup[super::super::BearerId::new(5).unwrap().index()] = Some(0);
        assert_eq!(
            validate_new_endpoints(owner(), &lookup, &[endpoint(5)]),
            Err(BearerError::Owner)
        );
        assert_eq!(
            validate_new_endpoints(owner(), &lookup, &[endpoint(6), endpoint(6)]),
            Err(BearerError::Owner)
        );
    }

    #[test]
    fn additional_endpoint_validation_rejects_foreign_owner() {
        let lookup = [None; MAX_BEARERS];
        let mut candidate = endpoint(6);
        candidate.ue = UeKey { slot: 1, ..owner() };
        assert_eq!(
            validate_new_endpoints(owner(), &lookup, &[candidate]),
            Err(BearerError::Owner)
        );
    }
}
