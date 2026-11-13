// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::rel99_planner::RabPlan;
use std::fmt;
use umts_iuh::{Domain, session::RabRequest};

const MAX_ACTIVE_RABS: usize = 16;

impl super::Rel99Admission {
    pub(crate) fn packet_modification_matches(&self, request: RabRequest, rabs: &[u8]) -> bool {
        if request.domain() != Domain::Ps || rabs.is_empty() || rabs.len() > MAX_ACTIVE_RABS {
            return false;
        }
        let Some(entry) = self
            .entries
            .get(request.key().slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.identity.ue == request.key())
        else {
            return false;
        };
        entry.inventory.as_ref().is_some_and(|inventory| {
            rabs.iter().all(|id| {
                inventory.active.iter().any(|active| {
                    active.plan.domain == umts_types::CnDomain::Ps
                        && active.plan.rab == *id
                        && active.request.same_connection(request)
                })
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InventoryError {
    Identity,
    Capacity,
    Duplicate,
    Phase,
    Incomplete,
}

#[derive(Clone, Debug)]
pub struct ActiveRab {
    request: RabRequest,
    plan: RabPlan,
}

impl ActiveRab {
    pub fn request(&self) -> RabRequest {
        self.request
    }

    pub fn plan(&self) -> &RabPlan {
        &self.plan
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RabOutcomeIds {
    resolved: Vec<u8>,
    successful: Vec<u8>,
}

impl RabOutcomeIds {
    pub(crate) fn record_resolved(&mut self, rab: u8, successful: bool) {
        if !self.resolved.contains(&rab) {
            self.resolved.push(rab);
        }
        if successful && !self.successful.contains(&rab) {
            self.successful.push(rab);
        }
    }

    pub fn resolved(&self) -> &[u8] {
        &self.resolved
    }

    pub fn successful(&self) -> &[u8] {
        &self.successful
    }
}

struct PendingInitial {
    request: RabRequest,
    plans: Vec<RabPlan>,
}

#[must_use = "Commit or roll back the prepared active-RAB candidate"]
pub struct PreparedRabInventory {
    base_generation: u64,
    request: RabRequest,
    plans: Vec<RabPlan>,
    rebind: bool,
}

#[must_use = "Commit or roll back the prepared active-RAB removal"]
pub struct PreparedRabRemoval<'a> {
    inventory: &'a mut RabInventory,
    next_generation: u64,
    targets: Vec<RemovalTarget>,
}

#[derive(Clone, Copy)]
struct RemovalTarget {
    request: RabRequest,
    rab: u8,
}

impl fmt::Debug for PreparedRabRemoval<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedRabRemoval")
            .field("targets", &self.targets.len())
            .finish()
    }
}

impl fmt::Debug for PreparedRabInventory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedRabInventory")
            .field("request", &self.request)
            .field("rabs", &self.plans.len())
            .field("rebind", &self.rebind)
            .finish()
    }
}

pub struct RabInventory {
    anchors: [Option<RabRequest>; 2],
    active: Vec<ActiveRab>,
    initial: Option<PendingInitial>,
    generation: u64,
}

impl fmt::Debug for RabInventory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RabInventory")
            .field("anchors", &self.anchors)
            .field("active", &self.active)
            .field("initial_pending", &self.initial.is_some())
            .field("generation", &self.generation)
            .finish()
    }
}

impl RabInventory {
    pub fn initial(request: RabRequest, plans: &[RabPlan]) -> Result<Self, InventoryError> {
        validate_plans(request.domain(), plans)?;
        Ok(Self {
            anchors: [
                match request.domain() {
                    Domain::Cs => Some(request),
                    Domain::Ps => None,
                },
                match request.domain() {
                    Domain::Cs => None,
                    Domain::Ps => Some(request),
                },
            ],
            active: Vec::new(),
            initial: Some(PendingInitial {
                request,
                plans: plans.to_vec(),
            }),
            generation: 0,
        })
    }

    pub fn active(&self) -> &[ActiveRab] {
        &self.active
    }

    pub fn active_plans(&self) -> Vec<RabPlan> {
        self.active.iter().map(|rab| rab.plan.clone()).collect()
    }

    pub fn initial_pending(&self) -> bool {
        self.initial.is_some()
    }

    pub fn connection_anchor(&self, domain: Domain) -> Option<RabRequest> {
        self.anchors[domain_index(domain)]
    }

    pub fn record_initial_outcomes(
        &mut self,
        request: RabRequest,
        outcomes: &RabOutcomeIds,
    ) -> Result<(), InventoryError> {
        let (pending_request, pending_plans) = {
            let pending = self.initial.as_ref().ok_or(InventoryError::Phase)?;
            (pending.request, pending.plans.clone())
        };
        if pending_request != request {
            return Err(InventoryError::Identity);
        }
        validate_outcomes(&pending_plans, outcomes)?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(InventoryError::Capacity)?;
        let mut active = Vec::new();
        active
            .try_reserve(outcomes.successful().len())
            .map_err(|_| InventoryError::Capacity)?;
        for plan in pending_plans {
            if outcomes.successful().contains(&plan.rab) {
                active.push(ActiveRab { request, plan });
            }
        }
        self.active = active;
        self.generation = generation;
        self.initial = None;
        Ok(())
    }

    #[cfg(test)]
    pub fn prepare_addition(
        &self,
        request: RabRequest,
        plans: &[RabPlan],
    ) -> Result<PreparedRabInventory, InventoryError> {
        self.prepare_addition_checked(request, plans, None)
    }

    /// Prepares a newer same-session request after the domain has no active RABs.
    /// The callback must prove that the request is current in the Gateway.
    pub fn prepare_addition_with_current_connection(
        &self,
        request: RabRequest,
        plans: &[RabPlan],
        is_current: impl FnOnce(RabRequest) -> bool,
    ) -> Result<PreparedRabInventory, InventoryError> {
        self.prepare_addition_checked(request, plans, Some(is_current(request)))
    }

    fn prepare_addition_checked(
        &self,
        request: RabRequest,
        plans: &[RabPlan],
        current: Option<bool>,
    ) -> Result<PreparedRabInventory, InventoryError> {
        let rebind = match self.validate_addition_request(request) {
            Ok(()) => false,
            Err(error) if error == InventoryError::Identity && current == Some(true) => {
                if !self.can_rebind(request) {
                    return Err(error);
                }
                true
            }
            Err(error) => return Err(error),
        };
        validate_plans(request.domain(), plans)?;
        if plans.len() > MAX_ACTIVE_RABS.saturating_sub(self.active.len()) {
            return Err(InventoryError::Capacity);
        }
        let highest_serial = self
            .active
            .iter()
            .filter(|active| active.request.domain() == request.domain())
            .map(|active| active.request.serial())
            .max();
        if highest_serial.is_some_and(|serial| request.serial() <= serial) {
            return Err(InventoryError::Identity);
        }
        if plans.iter().any(|plan| {
            self.active.iter().any(|active| {
                active.request.domain() == request.domain() && active.plan.rab == plan.rab
            })
        }) {
            return Err(InventoryError::Duplicate);
        }
        Ok(PreparedRabInventory {
            base_generation: self.generation,
            request,
            plans: plans.to_vec(),
            rebind,
        })
    }

    #[cfg(test)]
    pub fn commit_addition(
        &mut self,
        prepared: PreparedRabInventory,
        outcomes: &RabOutcomeIds,
    ) -> Result<(), (InventoryError, PreparedRabInventory)> {
        let current = !prepared.rebind;
        self.commit_addition_checked(prepared, outcomes, current)
    }

    /// Commits a rebinding candidate after rechecking Gateway currentness.
    pub fn commit_addition_with_current_connection(
        &mut self,
        prepared: PreparedRabInventory,
        outcomes: &RabOutcomeIds,
        is_current: impl FnOnce(RabRequest) -> bool,
    ) -> Result<(), (InventoryError, PreparedRabInventory)> {
        let current = !prepared.rebind || is_current(prepared.request);
        self.commit_addition_checked(prepared, outcomes, current)
    }

    fn commit_addition_checked(
        &mut self,
        prepared: PreparedRabInventory,
        outcomes: &RabOutcomeIds,
        current: bool,
    ) -> Result<(), (InventoryError, PreparedRabInventory)> {
        if prepared.rebind && !current {
            return Err((InventoryError::Identity, prepared));
        }
        if prepared.base_generation != self.generation
            || prepared.plans.len() > MAX_ACTIVE_RABS.saturating_sub(self.active.len())
            || prepared.plans.iter().any(|plan| {
                self.active.iter().any(|active| {
                    active.request.domain() == prepared.request.domain()
                        && active.plan.rab == plan.rab
                })
            })
        {
            return Err((InventoryError::Phase, prepared));
        }
        if let Err(error) = self.validate_addition_request(prepared.request)
            && !(prepared.rebind && self.can_rebind(prepared.request))
        {
            return Err((error, prepared));
        }
        if let Err(error) = validate_outcomes(&prepared.plans, outcomes) {
            return Err((error, prepared));
        }
        let generation = match self.generation.checked_add(1) {
            Some(generation) => generation,
            None => return Err((InventoryError::Capacity, prepared)),
        };
        let PreparedRabInventory { request, plans, .. } = prepared;
        self.active.extend(
            plans
                .into_iter()
                .filter(|plan| outcomes.successful().contains(&plan.rab))
                .map(|plan| ActiveRab { request, plan }),
        );
        self.anchors[domain_index(request.domain())] = Some(request);
        self.generation = generation;
        Ok(())
    }

    pub fn prepare_release(
        &mut self,
        released_rb_ids: &[u8],
    ) -> Result<PreparedRabRemoval<'_>, InventoryError> {
        if released_rb_ids.is_empty()
            || released_rb_ids
                .iter()
                .enumerate()
                .any(|(index, rb)| released_rb_ids[..index].contains(rb))
        {
            return Err(InventoryError::Duplicate);
        }
        let mut targets = Vec::new();
        targets
            .try_reserve(self.active.len())
            .map_err(|_| InventoryError::Capacity)?;
        for active in &self.active {
            if active.plan.bearers.is_empty() {
                return Err(InventoryError::Identity);
            }
            let overlap = active
                .plan
                .bearers
                .iter()
                .filter(|rb| released_rb_ids.contains(rb))
                .count();
            if overlap != 0 && overlap != active.plan.bearers.len() {
                return Err(InventoryError::Identity);
            }
            if overlap == active.plan.bearers.len() {
                targets.push(RemovalTarget {
                    request: active.request,
                    rab: active.plan.rab,
                });
            }
        }
        if targets.is_empty()
            || released_rb_ids.iter().any(|rb| {
                let matches = self
                    .active
                    .iter()
                    .filter(|active| active.plan.bearers.contains(rb))
                    .count();
                matches != 1
            })
        {
            return Err(InventoryError::Identity);
        }
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or(InventoryError::Capacity)?;
        Ok(PreparedRabRemoval {
            inventory: self,
            next_generation,
            targets,
        })
    }

    fn validate_addition_request(&self, request: RabRequest) -> Result<(), InventoryError> {
        if self.initial.is_some() {
            return Err(InventoryError::Phase);
        }
        if let Some(anchor) = self.connection_anchor(request.domain()) {
            if !same_session(anchor, request) || request.serial() <= anchor.serial() {
                return Err(InventoryError::Identity);
            }
            return Ok(());
        }
        if self
            .anchors
            .iter()
            .flatten()
            .any(|anchor| same_session(*anchor, request))
        {
            Ok(())
        } else {
            Err(InventoryError::Phase)
        }
    }

    fn can_rebind(&self, request: RabRequest) -> bool {
        self.connection_anchor(request.domain())
            .is_some_and(|anchor| {
                anchor.same_ue_session(request)
                    && request.serial() > anchor.serial()
                    && !self
                        .active
                        .iter()
                        .any(|active| active.request.domain() == request.domain())
            })
    }
}

impl PreparedRabRemoval<'_> {
    pub fn commit(self) {
        let Self {
            inventory,
            next_generation,
            targets,
        } = self;
        inventory.active.retain(|active| {
            !targets
                .iter()
                .any(|target| target.request == active.request && target.rab == active.plan.rab)
        });
        inventory.generation = next_generation;
    }
}

fn same_session(left: RabRequest, right: RabRequest) -> bool {
    left.same_ue_session(right) && (left.same_connection(right) || left.domain() != right.domain())
}

fn domain_index(domain: Domain) -> usize {
    match domain {
        Domain::Cs => 0,
        Domain::Ps => 1,
    }
}

fn validate_plans(domain: Domain, plans: &[RabPlan]) -> Result<(), InventoryError> {
    if plans.is_empty() || plans.len() > MAX_ACTIVE_RABS {
        return Err(InventoryError::Capacity);
    }
    let domain = domain.into();
    for (index, plan) in plans.iter().enumerate() {
        if plan.domain != domain {
            return Err(InventoryError::Identity);
        }
        if plans[..index]
            .iter()
            .any(|previous| previous.rab == plan.rab)
        {
            return Err(InventoryError::Duplicate);
        }
    }
    Ok(())
}

fn validate_outcomes(plans: &[RabPlan], outcomes: &RabOutcomeIds) -> Result<(), InventoryError> {
    if outcomes
        .resolved()
        .iter()
        .any(|rab| !plans.iter().any(|plan| plan.rab == *rab))
        || outcomes
            .successful()
            .iter()
            .any(|rab| !outcomes.resolved().contains(rab))
        || outcomes.resolved().len() != plans.len()
        || plans
            .iter()
            .any(|plan| !outcomes.resolved().contains(&plan.rab))
    {
        return Err(InventoryError::Incomplete);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_iuh::ranap::rab::qos::Rates;
    use umts_types::CnDomain;

    fn plan(domain: CnDomain, rab: u8) -> RabPlan {
        RabPlan {
            domain,
            rab,
            bearers: Vec::new(),
            maximum: Rates::Symmetric { both: 1 },
            guaranteed: None,
            packet_payload_budget_bits: None,
            packet_capacity: None,
            maximum_sdu_bits: 1,
            circuit_interval: None,
            circuit_versions: None,
            combinations: Vec::new(),
        }
    }

    fn plan_with_bearer(domain: CnDomain, rab: u8, bearer: u8) -> RabPlan {
        let mut plan = plan(domain, rab);
        plan.bearers.push(bearer);
        plan
    }

    #[test]
    fn outcomes_keep_success_separate_from_resolved_failures() {
        let mut outcomes = RabOutcomeIds::default();
        outcomes.record_resolved(5, true);
        outcomes.record_resolved(7, false);
        outcomes.record_resolved(5, true);
        assert_eq!(outcomes.resolved(), &[5, 7]);
        assert_eq!(outcomes.successful(), &[5]);
    }

    #[test]
    fn plan_ids_are_bounded_and_domain_checked() {
        assert_eq!(
            validate_plans(Domain::Ps, &[]),
            Err(InventoryError::Capacity)
        );
    }

    #[test]
    fn outcome_validation_requires_every_planned_rab() {
        let plans = [plan(CnDomain::Ps, 5), plan(CnDomain::Ps, 7)];
        let mut outcomes = RabOutcomeIds::default();
        outcomes.record_resolved(5, true);
        assert_eq!(
            validate_outcomes(&plans, &outcomes),
            Err(InventoryError::Incomplete)
        );
        outcomes.record_resolved(7, false);
        assert_eq!(validate_outcomes(&plans, &outcomes), Ok(()));
        assert_eq!(outcomes.successful(), &[5]);
    }

    #[test]
    fn same_rab_number_is_valid_per_core_domain() {
        assert_eq!(validate_plans(Domain::Cs, &[plan(CnDomain::Cs, 5)]), Ok(()));
        assert_eq!(validate_plans(Domain::Ps, &[plan(CnDomain::Ps, 5)]), Ok(()));
    }

    #[test]
    fn cross_domain_addition_rejects_foreign_gateway_with_identical_external_identity() {
        use crate::{
            common_tx::CommonTx,
            core_link::{
                CoreLink,
                tests::{Mock, Peer, ready, register},
            },
            rab_handoff::RabHandoff,
            signalling::CommonSignalling,
        };
        use umts_iuh::{
            ContextId,
            ranap::rab::{Assignment, First, Second, SetupOrModify},
            rua,
            session::ControlTime,
            wire::Limits,
        };

        fn connected() -> (
            CommonTx,
            CommonSignalling,
            CoreLink<Mock>,
            Peer,
            umts_types::UeKey,
            ContextId,
        ) {
            let source = crate::test_support::source(false);
            let mut radio = crate::signalling::tests::controller(&source);
            let (mut core, peer) = ready(&mut radio);
            let (key, context, _) = register(&mut core, &peer, &mut radio, &source, 0, 3);
            (source, radio, core, peer, key, context)
        }
        fn offer(
            core: &mut CoreLink<Mock>,
            radio: &mut CommonSignalling,
            peer: &Peer,
            context: ContextId,
            domain: Domain,
            id: u8,
            now: u64,
        ) -> RabRequest {
            let ranap = Assignment {
                setup: vec![SetupOrModify {
                    first: First::modification(id),
                    second: Second::default(),
                }],
                release: vec![],
            }
            .encode(Limits::default())
            .unwrap();
            peer.rua(match domain {
                Domain::Cs => rua::Message::Direct {
                    domain,
                    context,
                    ranap,
                },
                Domain::Ps => rua::Message::Connect {
                    domain,
                    context,
                    ranap,
                    cause: rua::EstablishmentCause::Normal,
                    selector: None,
                    membership: None,
                },
            });
            let mut handoff = RabHandoff::new(Limits::default());
            core.drive(ControlTime(now), radio, |_, event| {
                handoff.offer(event).map(|_| ())
            })
            .unwrap();
            handoff.take().unwrap().unwrap().request()
        }

        let (_first_source, mut first_radio, mut first, first_peer, first_key, first_context) =
            connected();
        let (_second_source, mut second_radio, mut second, second_peer, second_key, second_context) =
            connected();
        assert_eq!((first_key, first_context), (second_key, second_context));
        let cs = offer(
            &mut first,
            &mut first_radio,
            &first_peer,
            first_context,
            Domain::Cs,
            5,
            6,
        );
        let foreign_ps = offer(
            &mut second,
            &mut second_radio,
            &second_peer,
            second_context,
            Domain::Ps,
            7,
            6,
        );
        let local_ps = offer(
            &mut first,
            &mut first_radio,
            &first_peer,
            first_context,
            Domain::Ps,
            7,
            7,
        );
        assert_eq!(cs.association(), foreign_ps.association());
        assert!(!cs.same_ue_session(foreign_ps));
        assert!(cs.same_ue_session(local_ps));
        let mut inventory = RabInventory::initial(cs, &[plan(CnDomain::Cs, 5)]).unwrap();
        let mut outcomes = RabOutcomeIds::default();
        outcomes.record_resolved(5, true);
        inventory.record_initial_outcomes(cs, &outcomes).unwrap();
        assert!(matches!(
            inventory.prepare_addition(foreign_ps, &[plan(CnDomain::Ps, 7)]),
            Err(InventoryError::Phase)
        ));
        assert_eq!(inventory.active().len(), 1);
        let candidate = inventory
            .prepare_addition(local_ps, &[plan(CnDomain::Ps, 7)])
            .unwrap();
        let mut outcomes = RabOutcomeIds::default();
        outcomes.record_resolved(7, true);
        inventory.commit_addition(candidate, &outcomes).unwrap();
        assert_eq!(inventory.active().len(), 2);
        assert_eq!(inventory.active()[1].request(), local_ps);
    }

    #[test]
    fn domain_anchor_survives_empty_release_and_accepts_same_connection_addition() {
        use crate::{
            common_tx::CommonTx,
            core_link::{
                CoreLink,
                tests::{Mock, Peer, ready, register},
            },
            rab_handoff::RabHandoff,
            signalling::CommonSignalling,
        };
        use umts_iuh::{
            ContextId,
            ranap::rab::{Assignment, First, Second, SetupOrModify},
            rua,
            session::ControlTime,
            wire::Limits,
        };

        fn connected() -> (
            CommonTx,
            CommonSignalling,
            CoreLink<Mock>,
            Peer,
            umts_types::UeKey,
            ContextId,
        ) {
            let source = crate::test_support::source(false);
            let mut radio = crate::signalling::tests::controller(&source);
            let (mut core, peer) = ready(&mut radio);
            let (key, context, _) = register(&mut core, &peer, &mut radio, &source, 0, 3);
            (source, radio, core, peer, key, context)
        }
        fn offer(
            core: &mut CoreLink<Mock>,
            radio: &mut CommonSignalling,
            peer: &Peer,
            context: ContextId,
            domain: Domain,
            id: u8,
            now: u64,
        ) -> RabRequest {
            let ranap = Assignment {
                setup: vec![SetupOrModify {
                    first: First::modification(id),
                    second: Second::default(),
                }],
                release: vec![],
            }
            .encode(Limits::default())
            .unwrap();
            peer.rua(match domain {
                Domain::Cs => rua::Message::Direct {
                    domain,
                    context,
                    ranap,
                },
                Domain::Ps => rua::Message::Connect {
                    domain,
                    context,
                    ranap,
                    cause: rua::EstablishmentCause::Normal,
                    selector: None,
                    membership: None,
                },
            });
            let mut handoff = RabHandoff::new(Limits::default());
            core.drive(ControlTime(now), radio, |_, event| {
                handoff.offer(event).map(|_| ())
            })
            .unwrap();
            handoff.take().unwrap().unwrap().request()
        }
        fn close(
            core: &mut CoreLink<Mock>,
            radio: &mut CommonSignalling,
            peer: &Peer,
            context: ContextId,
            now: u64,
        ) {
            peer.rua(rua::Message::Disconnect {
                domain: Domain::Cs,
                context,
                cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
                ranap: None,
            });
            crate::core_link::tests::finish_peer_disconnect(core, radio, now);
        }
        fn reopen_offer(
            core: &mut CoreLink<Mock>,
            radio: &mut CommonSignalling,
            peer: &Peer,
            context: ContextId,
            id: u8,
            now: u64,
        ) -> RabRequest {
            let ranap = Assignment {
                setup: vec![SetupOrModify {
                    first: First::modification(id),
                    second: Second::default(),
                }],
                release: vec![],
            }
            .encode(Limits::default())
            .unwrap();
            peer.rua(rua::Message::Connect {
                domain: Domain::Cs,
                context,
                ranap,
                cause: rua::EstablishmentCause::Normal,
                selector: None,
                membership: None,
            });
            let mut handoff = RabHandoff::new(Limits::default());
            core.drive(ControlTime(now), radio, |_, event| {
                handoff.offer(event).map(|_| ())
            })
            .unwrap();
            handoff.take().unwrap().unwrap().request()
        }

        let (_source, mut radio, mut core, peer, _key, context) = connected();
        let cs = offer(&mut core, &mut radio, &peer, context, Domain::Cs, 5, 6);
        let ps = offer(&mut core, &mut radio, &peer, context, Domain::Ps, 7, 7);

        let mut inventory =
            RabInventory::initial(cs, &[plan_with_bearer(CnDomain::Cs, 5, 5)]).unwrap();
        let mut outcomes = RabOutcomeIds::default();
        outcomes.record_resolved(5, true);
        inventory.record_initial_outcomes(cs, &outcomes).unwrap();
        let removal = inventory.prepare_release(&[5]).unwrap();
        removal.commit();
        assert!(inventory.active().is_empty());
        assert_eq!(inventory.connection_anchor(Domain::Cs), Some(cs));

        assert!(matches!(
            inventory.prepare_addition(cs, &[plan(CnDomain::Cs, 6)]),
            Err(InventoryError::Identity)
        ));
        assert!(matches!(
            inventory.prepare_addition_with_current_connection(
                cs,
                &[plan(CnDomain::Cs, 6)],
                |_| true,
            ),
            Err(InventoryError::Identity)
        ));

        close(&mut core, &mut radio, &peer, context, 8);
        let fresh_cs = reopen_offer(&mut core, &mut radio, &peer, context, 6, 9);
        assert!(!cs.same_connection(fresh_cs));
        assert!(cs.same_ue_session(fresh_cs));
        assert!(fresh_cs.serial() > cs.serial());
        assert!(core.gateway().rab_connection_current(fresh_cs));
        assert!(matches!(
            inventory.prepare_addition(fresh_cs, &[plan(CnDomain::Cs, 6)]),
            Err(InventoryError::Identity)
        ));

        let candidate = inventory
            .prepare_addition_with_current_connection(
                fresh_cs,
                &[plan(CnDomain::Cs, 6)],
                |request| core.gateway().rab_connection_current(request),
            )
            .unwrap();
        let mut outcomes = RabOutcomeIds::default();
        outcomes.record_resolved(6, true);
        inventory
            .commit_addition_with_current_connection(candidate, &outcomes, |request| {
                core.gateway().rab_connection_current(request)
            })
            .unwrap();
        assert_eq!(inventory.connection_anchor(Domain::Cs), Some(fresh_cs));

        let candidate = inventory
            .prepare_addition(ps, &[plan(CnDomain::Ps, 7)])
            .unwrap();
        let mut outcomes = RabOutcomeIds::default();
        outcomes.record_resolved(7, true);
        inventory.commit_addition(candidate, &outcomes).unwrap();
        assert_eq!(inventory.active().len(), 2);
        assert_eq!(inventory.active()[0].request(), fresh_cs);
        assert_eq!(inventory.active()[1].request(), ps);
    }
}
