// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::collections::HashSet;
use umts_rrc::dedicated::DchDirection;
use umts_user_plane::packet::BoundDatagrams;

type Rows = [Option<[Geometry; MAX_SUBFLOWS]>; MAX_RFCIS];

struct Change<'a> {
    previous: ReceiveIdentity,
    successor: ReceiveIdentity,
    original: &'a Arc<umts_rrc::dedicated::Rel99Configuration>,
    configuration: &'a Arc<umts_rrc::dedicated::Rel99Configuration>,
    removals: bool,
}

struct PreparedRoute {
    handle: VoiceHandle,
    layout: Layout,
    candidate_rows: Option<Rows>,
}

#[must_use]
pub struct PreparedVoiceHandoff<'a, D: BoundDatagrams> {
    traffic: &'a mut VoiceTraffic,
    successor: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
    routes: Vec<PreparedRoute>,
    removed: Vec<VoiceHandle>,
    transports: Vec<&'a mut VoiceTransport<D>>,
    released_storage: usize,
    _bank: &'a DedicatedBearers,
}

impl<D: BoundDatagrams> PreparedVoiceHandoff<'_, D> {
    pub fn commit(self) -> usize {
        let count = self.routes.len();
        for transport in self.transports {
            transport.commit_radio_retirement();
        }
        for handle in self.removed {
            let binding = self
                .traffic
                .bindings
                .remove(&handle.key)
                .expect("exclusively borrowed removed voice route");
            for subflow in &binding.layout.subflows {
                self.traffic
                    .radio
                    .remove(&radio_key(binding.identity, subflow.bearer));
            }
        }
        self.traffic.reserved -= self.released_storage;
        for route in self.routes {
            let binding = self
                .traffic
                .bindings
                .get_mut(&route.handle.key)
                .expect("exclusively borrowed original voice route");
            for subflow in &binding.layout.subflows {
                self.traffic
                    .radio
                    .remove(&radio_key(binding.identity, subflow.bearer));
            }
            for (index, subflow) in route.layout.subflows.iter().enumerate() {
                self.traffic.radio.insert(
                    radio_key(self.successor, subflow.bearer),
                    (route.handle.key, index),
                );
            }
            binding.identity = self.successor;
            binding.plan = self.plan.clone();
            binding.layout = route.layout;
            if let Some(rows) = route.candidate_rows {
                binding
                    .candidate
                    .as_mut()
                    .expect("exclusively borrowed pending media")
                    .rows = rows;
            }
        }
        count
    }
}

impl VoiceTraffic {
    /// Prepare inside the validated bank-retirement callback, without servicing that bank before commit.
    pub fn prepare_release_handoff<'a, D: BoundDatagrams + 'a>(
        &'a mut self,
        accepted: &crate::signalling::AcceptedBearerRelease,
        successor_plan: Arc<CompiledRel99>,
        bank: &'a DedicatedBearers,
        removed: impl IntoIterator<Item = &'a mut VoiceTransport<D>>,
    ) -> Result<PreparedVoiceHandoff<'a, D>, Error> {
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
            successor_plan,
            bank,
            removed,
        )
    }

    pub fn prepare_additional_handoff<'a>(
        &'a mut self,
        accepted: &crate::signalling::AcceptedAdditionalSetup,
        successor_plan: Arc<CompiledRel99>,
        bank: &'a DedicatedBearers,
    ) -> Result<PreparedVoiceHandoff<'a, umts_user_plane::iu_cs::RtpEndpoint>, Error> {
        if accepted.key() != accepted.previous_identity().ue
            || !bank.additional_handoff_ready(accepted, &successor_plan)
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
            successor_plan,
            bank,
            std::iter::empty(),
        )
    }

    fn prepare_handoff<'a, D: BoundDatagrams + 'a>(
        &'a mut self,
        change: Change<'_>,
        successor_plan: Arc<CompiledRel99>,
        bank: &'a DedicatedBearers,
        removed: impl IntoIterator<Item = &'a mut VoiceTransport<D>>,
    ) -> Result<PreparedVoiceHandoff<'a, D>, Error> {
        let previous = change.previous;
        let successor = change.successor;
        if previous.ue != successor.ue
            || successor.configuration <= previous.configuration
            || !Arc::ptr_eq(change.configuration, successor_plan.configuration())
            || !change.removals
                && change
                    .original
                    .bearers()
                    .iter()
                    .any(|old| change.configuration.bearer(old.id) != Some(old))
            || bank.identity() != previous
            || self
                .bindings
                .values()
                .any(|binding| binding.identity.ue == previous.ue && binding.identity != previous)
        {
            return Err(Error::Mapping);
        }
        let mut handles = Vec::new();
        let mut removed_handles = Vec::new();
        handles
            .try_reserve(self.bindings.len())
            .map_err(|_| Error::Capacity)?;
        removed_handles
            .try_reserve(self.bindings.len())
            .map_err(|_| Error::Capacity)?;
        for binding in self
            .bindings
            .values()
            .filter(|binding| binding.identity == previous)
        {
            if !Arc::ptr_eq(binding.plan.configuration(), change.original)
                || !bank.retained_voice_handoff_ready(
                    previous,
                    successor,
                    &binding.plan,
                    &successor_plan,
                    &[],
                )
            {
                return Err(Error::Mapping);
            }
            let retained = binding
                .layout
                .subflows
                .iter()
                .filter(|subflow| {
                    successor_plan
                        .configuration()
                        .bearer(subflow.bearer.get())
                        .is_some()
                })
                .count();
            if retained != 0 && retained != binding.layout.subflows.len()
                || !change.removals && retained != binding.layout.subflows.len()
            {
                return Err(Error::Mapping);
            }
            if retained != 0 {
                if !matches!(binding.radio, RadioBinding::Active { .. }) {
                    return Err(Error::Radio);
                }
                handles.push(binding.handle);
            } else {
                removed_handles.push(binding.handle);
            }
        }
        let mut transports: Vec<&'a mut VoiceTransport<D>> = Vec::new();
        transports
            .try_reserve_exact(removed_handles.len())
            .map_err(|_| Error::Capacity)?;
        let mut released_storage = 0usize;
        for transport in removed {
            if transports.len() == removed_handles.len()
                || !removed_handles.contains(&transport.handle())
                || transports
                    .iter()
                    .any(|old| old.handle() == transport.handle())
            {
                return Err(Error::Mapping);
            }
            released_storage = released_storage
                .checked_add(transport.radio_retirement_storage(self)?)
                .ok_or(Error::Capacity)?;
            transports.push(transport);
        }
        for handle in &removed_handles {
            let binding = self
                .bindings
                .get(&handle.key)
                .expect("validated removed voice route");
            if binding.transport_capacity.is_some()
                != transports
                    .iter()
                    .any(|transport| transport.handle() == *handle)
            {
                return Err(Error::Attached);
            }
            released_storage = released_storage
                .checked_add(self.limits.required_route_storage_bytes()?)
                .and_then(|bytes| bytes.checked_add(binding.initialization_storage))
                .and_then(|bytes| bytes.checked_add(binding.contract_storage))
                .ok_or(Error::Capacity)?;
        }
        if released_storage > self.reserved {
            return Err(Error::Capacity);
        }
        let mut routes = Vec::new();
        routes
            .try_reserve_exact(handles.len())
            .map_err(|_| Error::Capacity)?;
        let mut radio_entries = 0usize;
        for handle in &handles {
            let binding = self
                .bindings
                .get(&handle.key)
                .expect("validated original voice route");
            if !binding
                .pending_uplink
                .as_ref()
                .expect("voice pending owner")
                .accepts(successor)
            {
                return Err(Error::Mapping);
            }
            let mut bearers = [binding.layout.subflows[0].bearer; MAX_SUBFLOWS];
            for (target, subflow) in bearers.iter_mut().zip(&binding.layout.subflows) {
                *target = subflow.bearer;
            }
            let bearers = &bearers[..binding.layout.subflows.len()];
            if !bank.retained_voice_handoff_ready(
                previous,
                successor,
                &binding.plan,
                &successor_plan,
                bearers,
            ) {
                return Err(Error::Radio);
            }
            let mut layout = Layout::new(&successor_plan, bearers)?;
            validate_joint_formats(&binding.plan, &successor_plan, &binding.layout, &layout)?;
            for subflow in &layout.subflows {
                if self
                    .radio
                    .contains_key(&radio_key(successor, subflow.bearer))
                {
                    return Err(Error::Mapping);
                }
            }
            let validate = |formats: &Formats| {
                layout
                    .validate(&successor_plan, formats)
                    .map_err(|_| Error::Mapping)
            };
            match &binding.negotiation {
                Negotiation::Sender(sender) => {
                    validate(sender.proposed_formats())?;
                }
                Negotiation::Receiver(receiver) => {
                    validate_receiver(receiver, &validate)?;
                }
                Negotiation::Reinitializing { sender, receiver } => {
                    validate(sender.proposed_formats())?;
                    validate_receiver(receiver, &validate)?;
                }
            }
            if let Some(active) = binding.negotiation.negotiated() {
                validate(active.formats())?;
            }
            if let Some(uplink) = &binding.uplink {
                validate(uplink.negotiated().formats())?;
            }
            let candidate_rows = binding
                .candidate
                .as_ref()
                .map(|candidate| validate(candidate.queue.negotiated().formats()))
                .transpose()?;
            if let Some(queue) = &binding.queue {
                layout.rows = validate(queue.negotiated().formats())?;
            }
            radio_entries = radio_entries
                .checked_add(bearers.len())
                .ok_or(Error::Capacity)?;
            routes.push(PreparedRoute {
                handle: *handle,
                layout,
                candidate_rows,
            });
        }
        self.radio
            .try_reserve(radio_entries)
            .map_err(|_| Error::Capacity)?;
        Ok(PreparedVoiceHandoff {
            traffic: self,
            successor,
            plan: successor_plan,
            routes,
            removed: removed_handles,
            transports,
            released_storage,
            _bank: bank,
        })
    }
}

fn validate_receiver(
    receiver: &Receiver,
    validate: &impl Fn(&Formats) -> Result<Rows, Error>,
) -> Result<(), Error> {
    if let Some(active) = receiver.negotiated() {
        validate(active.formats())?;
    }
    if let Some(pending) = receiver.pending_negotiated() {
        validate(pending.formats())?;
    }
    Ok(())
}

fn validate_joint_formats(
    old: &CompiledRel99,
    next: &CompiledRel99,
    before: &Layout,
    after: &Layout,
) -> Result<(), Error> {
    for (left, right) in before.subflows.iter().zip(&after.subflows) {
        let old_rb = old
            .configuration()
            .bearer(left.bearer.get())
            .ok_or(Error::Mapping)?;
        let next_rb = next
            .configuration()
            .bearer(right.bearer.get())
            .ok_or(Error::Mapping)?;
        if old_rb.domain != next_rb.domain || old_rb.rlc != next_rb.rlc {
            return Err(Error::Mapping);
        }
    }
    // A chained Receiver prefix is private protocol state, so preserve every previously legal joint format.
    for direction in [DchDirection::Uplink, DchDirection::Downlink] {
        let (old, next) = match direction {
            DchDirection::Uplink => (old.uplink_configured(), next.uplink_configured()),
            DchDirection::Downlink => (old.downlink_configured(), next.downlink_configured()),
        };
        let index = |subflow: &Subflow| match direction {
            DchDirection::Uplink => subflow.uplink_transport,
            DchDirection::Downlink => subflow.transport,
        };
        for (left, right) in before.subflows.iter().zip(&after.subflows) {
            let left_id = old.transport_ids().nth(index(left)).ok_or(Error::Mapping)?;
            let right_id = next
                .transport_ids()
                .nth(index(right))
                .ok_or(Error::Mapping)?;
            let left = old.definition(left_id).ok_or(Error::Mapping)?;
            let right = next.definition(right_id).ok_or(Error::Mapping)?;
            if left.interval.frames() != right.interval.frames()
                || left.formats.len() != right.formats.len()
                || left
                    .formats
                    .iter()
                    .zip(&right.formats)
                    .any(|(left, right)| {
                        left.block_bits != right.block_bits || left.blocks != right.blocks
                    })
            {
                return Err(Error::Mapping);
            }
        }
        let mut rows = HashSet::new();
        rows.try_reserve(next.combinations().len())
            .map_err(|_| Error::Capacity)?;
        for combination in next.combinations() {
            let mut row = [0; MAX_SUBFLOWS];
            for (target, subflow) in row.iter_mut().zip(&after.subflows) {
                *target = combination.formats[index(subflow)];
            }
            rows.insert(row);
        }
        for combination in old.combinations() {
            let mut row = [0; MAX_SUBFLOWS];
            for (target, subflow) in row.iter_mut().zip(&before.subflows) {
                *target = combination.formats[index(subflow)];
            }
            if !rows.contains(&row) {
                return Err(Error::Mapping);
            }
        }
    }
    Ok(())
}
