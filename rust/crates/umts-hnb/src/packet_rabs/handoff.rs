// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::DedicatedBearers,
    packet_bearers::{PacketHandoff, PreparedPacketHandoff},
    signalling::AcceptedBearerRelease,
};

pub struct PreparedPacketRabHandoff<'a, T: BoundDatagrams> {
    bearers: PreparedPacketHandoff<'a, T>,
    active: &'a mut HashMap<RouteKey, Active>,
    reports: &'a mut Vec<RouteKey>,
    plan: Arc<CompiledRel99>,
}
impl<T: BoundDatagrams> PreparedPacketRabHandoff<'_, T> {
    pub fn removed(&self) -> &[RouteHandle] {
        self.bearers.removed()
    }
    pub fn commit(self) -> PacketHandoff {
        for handle in self.bearers.removed() {
            let active = self
                .active
                .remove(&handle.key())
                .expect("reserved removed packet RAB");
            if active.requested.second.reporting == Some(umts_iuh::ranap::rab::Reporting::Report) {
                if self.reports.len() == self.reports.capacity() {
                    self.reports.remove(0);
                }
                self.reports.push(handle.key());
            }
        }
        for handle in self.bearers.retained() {
            self.active
                .get_mut(&handle.key())
                .expect("reserved retained packet RAB")
                .plan = self.plan.clone();
        }
        self.bearers.commit()
    }
}
impl<T: BoundDatagrams> PacketRabs<T> {
    pub fn prepare_release_handoff<'a>(
        &'a mut self,
        accepted: &AcceptedBearerRelease,
        plan: Arc<CompiledRel99>,
        bank: &'a DedicatedBearers,
    ) -> Result<PreparedPacketRabHandoff<'a, T>, Error> {
        if self
            .pending
            .keys()
            .any(|key| key.connection.ue == accepted.key())
        {
            return Err(Error::Conflict);
        }
        let bearers = self
            .bearers
            .prepare_release_handoff(accepted, plan.clone(), bank)
            .map_err(Error::Binding)?;
        Self::prepare_registered_handoff(
            &mut self.active,
            &mut self.release_reports,
            accepted.key(),
            bearers,
            plan,
        )
    }
    pub fn prepare_additional_handoff<'a>(
        &'a mut self,
        accepted: &crate::signalling::AcceptedAdditionalSetup,
        plan: Arc<CompiledRel99>,
        bank: &'a DedicatedBearers,
    ) -> Result<PreparedPacketRabHandoff<'a, T>, Error> {
        if self
            .pending
            .keys()
            .any(|key| key.connection.ue == accepted.key())
        {
            return Err(Error::Conflict);
        }
        let bearers = self
            .bearers
            .prepare_additional_handoff(accepted, plan.clone(), bank)
            .map_err(Error::Binding)?;
        Self::prepare_registered_handoff(
            &mut self.active,
            &mut self.release_reports,
            accepted.key(),
            bearers,
            plan,
        )
    }
    fn prepare_registered_handoff<'a>(
        active: &'a mut HashMap<RouteKey, Active>,
        reports: &'a mut Vec<RouteKey>,
        key: umts_types::UeKey,
        bearers: PreparedPacketHandoff<'a, T>,
        plan: Arc<CompiledRel99>,
    ) -> Result<PreparedPacketRabHandoff<'a, T>, Error> {
        let affected = active
            .iter()
            .filter(|(route, _)| route.connection.ue == key);
        if affected.clone().count() != bearers.retained().len() + bearers.removed().len() {
            return Err(Error::Mapping);
        }
        for (key, active) in affected {
            if active.ticket.key() != *key
                || !bearers.retained().contains(&active.ticket.route)
                    && !bearers.removed().contains(&active.ticket.route)
                || bearers
                    .original_plan(active.ticket.route)
                    .is_none_or(|previous| !Arc::ptr_eq(previous, &active.plan))
            {
                return Err(Error::Mapping);
            }
        }
        Ok(PreparedPacketRabHandoff {
            bearers,
            active,
            reports,
            plan,
        })
    }
}
