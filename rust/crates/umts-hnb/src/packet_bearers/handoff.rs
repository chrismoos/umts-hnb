// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{dedicated_bearers::DedicatedBearers, signalling::AcceptedBearerRelease};
use umts_user_plane::packet::{Phase, PreparedRouteRelease};

struct Change<'a> {
    previous: ReceiveIdentity,
    successor: ReceiveIdentity,
    original: &'a Arc<umts_rrc::dedicated::Rel99Configuration>,
    configuration: &'a Arc<umts_rrc::dedicated::Rel99Configuration>,
    removals: bool,
}

pub struct PreparedPacketHandoff<'a, T: Datagrams> {
    plane: PreparedRouteRelease<'a, T>,
    bindings: &'a mut BTreeMap<Teid, Binding>,
    uplink: &'a mut HashMap<RadioKey, RouteHandle>,
    retained: Vec<RouteHandle>,
    previous: ReceiveIdentity,
    identity: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
    _bank: &'a DedicatedBearers,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketHandoff {
    pub retained: usize,
    pub removed: usize,
}
impl<T: Datagrams> PreparedPacketHandoff<'_, T> {
    pub fn retained(&self) -> &[RouteHandle] {
        &self.retained
    }
    pub fn removed(&self) -> &[RouteHandle] {
        self.plane.handles()
    }
    pub(crate) fn original_plan(&self, handle: RouteHandle) -> Option<&Arc<CompiledRel99>> {
        self.bindings
            .get(&handle.local_teid())
            .filter(|binding| binding.handle == handle)
            .map(|binding| &binding.plan)
    }
    pub fn commit(self) -> PacketHandoff {
        let result = PacketHandoff {
            retained: self.retained.len(),
            removed: self.plane.handles().len(),
        };
        for handle in self.plane.handles() {
            let binding = self
                .bindings
                .remove(&handle.local_teid())
                .expect("reserved removed packet binding");
            self.uplink
                .remove(&radio_key(binding.identity, binding.bearer));
            if let Some(previous) = binding.previous {
                self.uplink
                    .remove(&radio_key(previous.identity, binding.bearer));
            }
        }
        for handle in self.retained {
            let binding = self
                .bindings
                .get_mut(&handle.local_teid())
                .expect("reserved packet binding");
            let previous_plan = std::mem::replace(&mut binding.plan, self.plan.clone());
            binding.previous = Some(RadioAlias {
                identity: self.previous,
                plan: previous_plan,
            });
            binding.identity = self.identity;
            self.uplink
                .insert(radio_key(self.identity, binding.bearer), handle);
        }
        self.plane.commit();
        result
    }
}
impl<T: Datagrams> PacketBearers<T> {
    pub(super) fn retire_next_alias(
        &mut self,
        receive: &mut DedicatedController,
    ) -> Result<bool, Error> {
        use std::ops::Bound::{Excluded, Unbounded};

        let next = self
            .alias_cursor
            .and_then(|cursor| self.bindings.range((Excluded(cursor), Unbounded)).next())
            .or_else(|| self.bindings.first_key_value())
            .map(|(key, binding)| {
                (
                    *key,
                    binding.handle,
                    binding.identity,
                    binding.previous.as_ref().map(|previous| previous.identity),
                )
            });
        let Some((key, handle, identity, previous)) = next else {
            self.alias_cursor = None;
            return Ok(false);
        };
        self.alias_cursor = Some(key);
        let Some(previous) = previous else {
            return Ok(false);
        };
        receive
            .with_active_mapping(identity, |bank| {
                if bank.identity() != identity || bank.contains_identity(previous) {
                    return Ok(false);
                }
                self.retire_handoff_alias(handle, bank)
            })
            .unwrap_or(Ok(false))
    }
    pub fn prepare_release_handoff<'a>(
        &'a mut self,
        accepted: &AcceptedBearerRelease,
        plan: Arc<CompiledRel99>,
        bank: &'a DedicatedBearers,
    ) -> Result<PreparedPacketHandoff<'a, T>, Error> {
        if accepted.key() != accepted.previous_identity().ue {
            return Err(Error::Mapping);
        }
        self.prepare_handoff(
            Change {
                previous: accepted.previous_identity(),
                successor: accepted.identity(),
                original: accepted.previous(),
                configuration: accepted.configuration(),
                removals: true,
            },
            plan,
            bank,
        )
    }
    pub fn prepare_additional_handoff<'a>(
        &'a mut self,
        accepted: &crate::signalling::AcceptedAdditionalSetup,
        plan: Arc<CompiledRel99>,
        bank: &'a DedicatedBearers,
    ) -> Result<PreparedPacketHandoff<'a, T>, Error> {
        if accepted.key() != accepted.previous_identity().ue
            || !bank.additional_handoff_ready(accepted, &plan)
        {
            return Err(Error::Mapping);
        }
        self.prepare_handoff(
            Change {
                previous: accepted.previous_identity(),
                successor: accepted.identity(),
                original: accepted.previous(),
                configuration: accepted.configuration(),
                removals: false,
            },
            plan,
            bank,
        )
    }
    fn prepare_handoff<'a>(
        &'a mut self,
        change: Change<'_>,
        plan: Arc<CompiledRel99>,
        bank: &'a DedicatedBearers,
    ) -> Result<PreparedPacketHandoff<'a, T>, Error> {
        if change.previous.ue != change.successor.ue
            || change.successor.configuration <= change.previous.configuration
            || !Arc::ptr_eq(change.configuration, plan.configuration())
            || !change.removals
                && change
                    .original
                    .bearers()
                    .iter()
                    .any(|old| change.configuration.bearer(old.id) != Some(old))
        {
            return Err(Error::Mapping);
        }
        let mut retained = Vec::new();
        let mut removed = Vec::new();
        let affected = self
            .bindings
            .values()
            .filter(|binding| binding.identity.ue == change.previous.ue)
            .count();
        retained
            .try_reserve_exact(affected)
            .map_err(|_| Error::Capacity)?;
        removed
            .try_reserve_exact(affected)
            .map_err(|_| Error::Capacity)?;
        for binding in self
            .bindings
            .values()
            .filter(|binding| binding.identity.ue == change.previous.ue)
        {
            if binding.identity != change.previous
                || !Arc::ptr_eq(binding.plan.configuration(), change.original)
                || self
                    .uplink
                    .get(&radio_key(binding.identity, binding.bearer))
                    != Some(&binding.handle)
            {
                return Err(Error::Mapping);
            }
            if self.plane.phase(binding.handle).map_err(Error::Transport)? != Phase::Active {
                return Err(Error::Mapping);
            }
            let old = change
                .original
                .bearer(binding.bearer.get())
                .ok_or(Error::Mapping)?;
            if !bank.packet_handoff_ready(
                binding.identity,
                change.successor,
                binding.bearer,
                &binding.plan,
                &plan,
            ) {
                return Err(Error::Radio);
            }
            let Some(next) = change.configuration.bearer(binding.bearer.get()) else {
                if !change.removals {
                    return Err(Error::Mapping);
                }
                removed.push(binding.handle);
                continue;
            };
            if let Some(previous) = &binding.previous {
                return Err(Error::PendingAlias {
                    current: binding.identity,
                    previous: previous.identity,
                });
            }
            if old.domain != Some(CnDomain::Ps)
                || next.domain != old.domain
                || next.rlc != old.rlc
                || self
                    .uplink
                    .contains_key(&radio_key(change.successor, binding.bearer))
            {
                return Err(Error::Mapping);
            }
            retained.push(binding.handle);
        }
        self.uplink
            .try_reserve(retained.len())
            .map_err(|_| Error::Capacity)?;
        let plane = self
            .plane
            .prepare_release_batch(removed)
            .map_err(Error::Transport)?;
        Ok(PreparedPacketHandoff {
            plane,
            bindings: &mut self.bindings,
            uplink: &mut self.uplink,
            retained,
            previous: change.previous,
            identity: change.successor,
            plan,
            _bank: bank,
        })
    }
    pub fn retire_handoff_alias(
        &mut self,
        handle: RouteHandle,
        bank: &DedicatedBearers,
    ) -> Result<bool, Error> {
        let binding = self
            .bindings
            .get(&handle.local_teid())
            .filter(|binding| binding.handle == handle)
            .ok_or(Error::Mapping)?;
        let Some(previous) = &binding.previous else {
            return Ok(false);
        };
        if !bank
            .packet_alias_drained(
                previous.identity,
                binding.identity,
                binding.bearer,
                &previous.plan,
                &binding.plan,
            )
            .map_err(|_| Error::Radio)?
        {
            return Ok(false);
        }
        let key = radio_key(previous.identity, binding.bearer);
        self.uplink.remove(&key);
        self.bindings
            .get_mut(&handle.local_teid())
            .expect("validated packet alias")
            .previous = None;
        Ok(true)
    }
}
