// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_plan::CompiledRel99;

impl DedicatedBearers {
    pub(crate) fn additional_handoff_ready(
        &self,
        accepted: &crate::signalling::AcceptedAdditionalSetup,
        new: &Arc<CompiledRel99>,
    ) -> bool {
        let previous = accepted.previous_identity();
        let successor = accepted.identity();
        let (Ok(old_index), Ok(new_index)) =
            (self.mapping_index(previous), self.mapping_index(successor))
        else {
            return false;
        };
        let old = self.maps[old_index]
            .as_ref()
            .expect("registered predecessor");
        let next = self.maps[new_index].as_ref().expect("registered successor");
        let Some(original) = old.plan.as_ref() else {
            return false;
        };
        let Some(downlink) = next.downlink.as_ref() else {
            return false;
        };
        self.handoff_pair(previous, successor, original, new)
            && accepted.key() == previous.ue
            && next.additional
            && Arc::ptr_eq(original.configuration(), accepted.previous())
            && Arc::ptr_eq(new.configuration(), accepted.configuration())
            && downlink.anchor.time == accepted.activation().time()
            && downlink.anchor.cfn == accepted.activation().cfn()
            && next.anchor.cfn == accepted.activation().cfn()
            && matches!(old.phase, MappingPhase::Draining { uplink_end, downlink_end, .. }
                if uplink_end == next.anchor.time && downlink_end == downlink.anchor.time)
    }

    fn handoff_pair(
        &self,
        previous: ReceiveIdentity,
        successor: ReceiveIdentity,
        old: &Arc<CompiledRel99>,
        new: &Arc<CompiledRel99>,
    ) -> bool {
        if self.phase != BearerPhase::Active
            || self.identity() != previous
            || previous.ue != successor.ue
            || successor.configuration <= previous.configuration
        {
            return false;
        }
        let (Ok(old_index), Ok(new_index)) =
            (self.mapping_index(previous), self.mapping_index(successor))
        else {
            return false;
        };
        let old_map = self.maps[old_index]
            .as_ref()
            .expect("registered predecessor");
        let new_map = self.maps[new_index].as_ref().expect("registered successor");
        matches!(old_map.phase, MappingPhase::Draining { next, .. } if next == successor)
            && new_map.phase == (MappingPhase::Scheduled { previous })
            && old_map
                .plan
                .as_ref()
                .is_some_and(|plan| Arc::ptr_eq(plan, old))
            && new_map
                .plan
                .as_ref()
                .is_some_and(|plan| Arc::ptr_eq(plan, new))
    }
    pub(crate) fn packet_handoff_ready(
        &self,
        previous: ReceiveIdentity,
        successor: ReceiveIdentity,
        bearer: BearerId,
        old: &Arc<CompiledRel99>,
        new: &Arc<CompiledRel99>,
    ) -> bool {
        self.handoff_pair(previous, successor, old, new)
            && self.lookup[bearer.index()].is_some_and(|index| {
                let endpoint = &self.endpoints[index];
                endpoint.scope == Scope::User(umts_types::CnDomain::Ps)
                    && matches!(
                        endpoint.entity,
                        Entity::Acknowledged(_) | Entity::Unacknowledged { .. }
                    )
            })
            && old.configuration().bearer(bearer.get()).is_some_and(|old| {
                old.domain == Some(umts_types::CnDomain::Ps)
                    && new
                        .configuration()
                        .bearer(bearer.get())
                        .is_none_or(|new| new.domain == old.domain && new.rlc == old.rlc)
            })
    }
    pub(crate) fn packet_alias_drained(
        &self,
        previous: ReceiveIdentity,
        current: ReceiveIdentity,
        bearer: BearerId,
        old: &Arc<CompiledRel99>,
        new: &Arc<CompiledRel99>,
    ) -> Result<bool, BearerError> {
        if self.phase != BearerPhase::Active
            || self.identity() != current
            || previous.ue != current.ue
            || previous.configuration >= current.configuration
            || self.contains_identity(previous)
            || !self
                .current_map()
                .plan
                .as_ref()
                .is_some_and(|plan| Arc::ptr_eq(plan, new))
        {
            return Err(BearerError::Owner);
        }
        let endpoint = &self.endpoints[self.lookup[bearer.index()].ok_or(BearerError::Mapping)?];
        if endpoint.scope != Scope::User(umts_types::CnDomain::Ps)
            || !matches!(
                endpoint.entity,
                Entity::Acknowledged(_) | Entity::Unacknowledged { .. }
            )
            || !old
                .configuration()
                .bearer(bearer.get())
                .zip(new.configuration().bearer(bearer.get()))
                .is_some_and(|(old, new)| {
                    old.domain == Some(umts_types::CnDomain::Ps)
                        && old.domain == new.domain
                        && old.rlc == new.rlc
                })
        {
            return Err(BearerError::Mapping);
        }
        let pending_path = match &endpoint.entity {
            Entity::Acknowledged(entity) => entity.pending_receive_path(previous.configuration),
            _ => endpoint.pending_receive_data(),
        };
        Ok(!pending_path
            && self
                .pending
                .as_ref()
                .is_none_or(|pending| pending.decoded.identity != previous)
            && !self.output.slots.iter().any(|slot| {
                slot.metadata.is_some_and(|metadata| {
                    metadata.identity == previous && metadata.bearer == bearer
                })
            }))
    }
    pub(crate) fn retained_voice_handoff_ready(
        &self,
        previous: ReceiveIdentity,
        successor: ReceiveIdentity,
        old: &Arc<CompiledRel99>,
        new: &Arc<CompiledRel99>,
        bearers: &[BearerId],
    ) -> bool {
        self.handoff_pair(previous, successor, old, new)
            && self.pending.is_none()
            && bearers.iter().enumerate().all(|(index, bearer)| {
                !bearers[..index].contains(bearer)
                    && self.lookup[bearer.index()].is_some_and(|index| {
                        let endpoint = &self.endpoints[index];
                        endpoint.scope == Scope::User(umts_types::CnDomain::Cs)
                            && matches!(endpoint.entity, Entity::Transparent { .. })
                    })
                    && old
                        .configuration()
                        .bearer(bearer.get())
                        .zip(new.configuration().bearer(bearer.get()))
                        .is_some_and(|(old, new)| {
                            old.domain == Some(umts_types::CnDomain::Cs)
                                && old.domain == new.domain
                                && old.rlc == new.rlc
                        })
                    && !self.output.slots.iter().any(|slot| {
                        slot.metadata.is_some_and(|metadata| {
                            metadata.identity == previous && metadata.bearer == *bearer
                        })
                    })
            })
    }
}
