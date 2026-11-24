// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_bearers::{
        BearerEndpoint, BearerError, BearerPhase, BearerProgress, DedicatedBearers,
        MappingActivation, MappingRelease, MappingReleaseError, PreparedAddition, PreparedMapping,
        RejectedAddition, RejectedAdditionPreparation as RejectedBearerAdditionPreparation,
        RejectedMapping, ReleaseOutputPolicy, SduDelivery,
    },
    dedicated_rx::{
        DedicatedReceivePort, DedicatedSampleReceiver, ReceiveCommandResult, ReceiveCommandTicket,
        ReceiveError, ReceiveIdentity, ReceiveOwner,
    },
};
use std::{num::NonZeroUsize, sync::Arc};
use umts_types::{RadioEpoch, UeKey, time::ChipInstant};

#[cfg(test)]
mod tests;

pub struct PreparedDedicated {
    pub receiver: Box<DedicatedSampleReceiver>,
    pub bearers: Box<DedicatedBearers>,
}
pub struct RejectedDedicated {
    pub error: DedicatedControlError,
    pub prepared: Box<PreparedDedicated>,
}
#[must_use = "Recover the candidate endpoints before abandoning the addition"]
pub struct RejectedDedicatedAdditionPreparation {
    pub error: DedicatedControlError,
    pub endpoints: Vec<BearerEndpoint>,
}
impl std::fmt::Debug for RejectedDedicatedAdditionPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedDedicatedAdditionPreparation")
            .field("error", &self.error)
            .field("endpoints", &self.endpoints.len())
            .finish()
    }
}
impl std::fmt::Debug for RejectedDedicated {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedDedicated")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DedicatedControlError {
    Identity,
    Configuration,
    Capacity,
    Phase,
    Closed,
    Protocol,
    Time,
    Processing,
    Receive(ReceiveError),
    Bearer(BearerError),
}
#[derive(Debug, PartialEq, Eq)]
pub enum DedicatedReleaseError<E> {
    Control(DedicatedControlError),
    Release(MappingReleaseError<E>),
}
#[derive(Debug, PartialEq, Eq)]
pub enum DedicatedMappingCommitError<E> {
    Control(DedicatedControlError),
    Commit(E),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DedicatedPhase {
    Preparing,
    Prepared,
    Arming,
    Active,
    Removing,
    Rejected,
    ArmFailed,
    Retired,
    Faulted,
    Closed,
}
#[derive(Clone, Copy)]
pub struct DedicatedControllerLimits {
    pub receipt_visits: NonZeroUsize,
    pub service_visits: NonZeroUsize,
    pub maximum_bearer_bytes: usize,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DedicatedControllerCounters {
    pub staged: u64,
    pub admitted: u64,
    pub armed: u64,
    pub rejected: u64,
    pub routed: u64,
    pub backpressure: u64,
    pub stale: u64,
    pub discarded: u64,
    pub delivered: u64,
    pub failures: u64,
}
enum Operation {
    Install(ReceiveCommandTicket),
    Arm(ReceiveCommandTicket),
    Remove(ReceiveCommandTicket),
}
struct Entry {
    bank: Box<DedicatedBearers>,
    phase: DedicatedPhase,
    operation: Option<Operation>,
    receiver: Option<Box<DedicatedSampleReceiver>>,
    failure: Option<DedicatedControlError>,
    bytes: usize,
    mapping_removal: Option<MappingRemoval>,
    receive_status: Option<(ReceiveIdentity, crate::dedicated_rx::ReceivePlanStatus)>,
}
struct MappingRemoval {
    previous: ReceiveIdentity,
    successor: ReceiveIdentity,
    retired: [bool; 2],
    discarded: usize,
}
#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct RetiredDedicated {
    pub bank: Box<DedicatedBearers>,
    pub receiver: Option<Box<DedicatedSampleReceiver>>,
    pub discarded_transports: usize,
    pub failure: Option<DedicatedControlError>,
}
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct DedicatedStep<E> {
    pub receipts: usize,
    pub routed: bool,
    pub serviced: usize,
    pub delivered: usize,
    pub failure: Option<(ReceiveIdentity, E)>,
}

pub struct DedicatedController {
    port: DedicatedReceivePort,
    entries: Vec<Option<Entry>>,
    limits: DedicatedControllerLimits,
    used_bytes: usize,
    receipt_cursor: usize,
    service_cursor: usize,
    retire_cursor: usize,
    observation_cursor: usize,
    observation: Option<(ReceiveIdentity, ReceiveCommandTicket)>,
    previous: Option<ChipInstant>,
    counters: DedicatedControllerCounters,
}
impl DedicatedController {
    pub(crate) fn hsupa_source(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, DedicatedControlError> {
        let entry = self.entry_mut(identity)?;
        if entry.phase != DedicatedPhase::Active || entry.failure.is_some() {
            return Err(DedicatedControlError::Phase);
        }
        self.port
            .hsupa_source(identity)
            .map_err(|_| DedicatedControlError::Capacity)
    }
    pub fn new(port: DedicatedReceivePort, limits: DedicatedControllerLimits) -> Self {
        let entries = (0..port.capacity()).map(|_| None).collect();
        Self {
            port,
            entries,
            limits,
            used_bytes: 0,
            receipt_cursor: 0,
            service_cursor: 0,
            retire_cursor: 0,
            observation_cursor: 0,
            observation: None,
            previous: None,
            counters: DedicatedControllerCounters::default(),
        }
    }
    pub fn epoch(&self) -> RadioEpoch {
        self.port.epoch()
    }
    pub fn service_receive_diagnostics(
        &mut self,
        mut visit: impl FnMut(ReceiveIdentity, &crate::dedicated_rx::ReceivePlanStatus),
    ) -> usize {
        let mut completed = 0;
        if let Some((requested, ticket)) = &mut self.observation {
            let requested = *requested;
            let Some(result) = ticket.try_result() else {
                return 0;
            };
            self.observation = None;
            if let ReceiveCommandResult::Observed {
                identity,
                status: Some(status),
            } = result
                && identity == requested
                && self.phase(identity) == Some(DedicatedPhase::Active)
            {
                let entry = self.entry_mut(identity).expect("active observed receiver");
                let (_, status) = entry.receive_status.insert((identity, status));
                visit(identity, status);
                completed = 1;
            }
        }
        for _ in 0..self.limits.receipt_visits.get().min(self.entries.len()) {
            let index = self.observation_cursor;
            self.observation_cursor = (index + 1) % self.entries.len();
            let Some(entry) = &mut self.entries[index] else {
                continue;
            };
            if entry.phase != DedicatedPhase::Active
                || entry.failure.is_some()
                || entry.operation.is_some()
            {
                continue;
            }
            let identity = entry.bank.identity();
            if let Ok(ticket) = self.port.observe(identity) {
                self.observation = Some((identity, ticket));
            }
            break;
        }
        completed
    }
    pub fn visit_receive_diagnostics(
        &self,
        mut visit: impl FnMut(ReceiveIdentity, &crate::dedicated_rx::ReceivePlanStatus),
    ) {
        for entry in self.entries.iter().flatten() {
            if entry.phase == DedicatedPhase::Active
                && entry.failure.is_none()
                && let Some((identity, status)) = &entry.receive_status
                && entry.bank.identity() == *identity
            {
                visit(*identity, status);
            }
        }
    }
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn with_active_bank<R>(
        &mut self,
        identity: ReceiveIdentity,
        action: impl FnOnce(&mut DedicatedBearers) -> R,
    ) -> Result<R, DedicatedControlError> {
        let entry = self.entry_mut(identity)?;
        if entry.phase != DedicatedPhase::Active {
            return Err(DedicatedControlError::Phase);
        }
        Ok(action(&mut entry.bank))
    }
    #[cfg(test)]
    pub fn counters(&self) -> DedicatedControllerCounters {
        self.counters
    }
    pub fn service_hsupa_restoration(
        &mut self,
        session: &mut crate::hsupa_session::Session,
        radio: &mut crate::signalling::CommonSignalling,
        now: ChipInstant,
    ) -> anyhow::Result<()> {
        let entry = self
            .entry_mut(session.identity())
            .map_err(|error| anyhow::anyhow!("restoration receiver: {error:?}"))?;
        anyhow::ensure!(
            entry.phase == DedicatedPhase::Active && entry.failure.is_none(),
            "restoration requires an active receiver"
        );
        session.service_restoration(radio, &mut self.port, now)
    }

    pub fn service_hsupa_resources(
        &mut self,
        session: &mut crate::hsupa_session::Session,
        transmit: &mut impl crate::hsupa_tx::Control,
        air_now: ChipInstant,
    ) -> Result<crate::hsupa_session::Phase, DedicatedControlError> {
        use crate::hsupa_session::Phase;
        if air_now.epoch != session.identity().ue.epoch {
            return Err(DedicatedControlError::Phase);
        }
        if matches!(session.phase(), Phase::Cancelling | Phase::Cancelled) {
            session.service_cancellation(&mut self.port, transmit, air_now);
        } else {
            let entry = self.entry_mut(session.identity())?;
            if entry.phase != DedicatedPhase::Active || entry.failure.is_some() {
                return Err(DedicatedControlError::Phase);
            }
            session.preparation().service(&mut self.port, transmit);
        }
        Ok(session.phase())
    }
    pub(crate) fn with_active_mapping<R>(
        &mut self,
        identity: ReceiveIdentity,
        action: impl FnOnce(&mut DedicatedBearers) -> R,
    ) -> Result<R, DedicatedControlError> {
        let entry = self
            .entries
            .get_mut(identity.ue.slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| {
                entry.bank.identity().ue == identity.ue && entry.bank.contains_identity(identity)
            })
            .ok_or(DedicatedControlError::Identity)?;
        if entry.phase != DedicatedPhase::Active {
            return Err(DedicatedControlError::Phase);
        }
        Ok(action(&mut entry.bank))
    }
    #[cfg(test)]
    pub fn used_bearer_bytes(&self) -> usize {
        self.used_bytes
    }
    #[cfg(test)]
    pub fn reserved_output_bytes(&self) -> usize {
        self.port.reserved_output_bytes()
    }
    #[cfg(test)]
    pub fn prepare_mapping(
        &self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<crate::rel99_plan::CompiledRel99>,
    ) -> Result<PreparedMapping, DedicatedControlError> {
        let entry = self
            .entries
            .get(previous.ue.slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| {
                entry.bank.identity() == previous && entry.phase == DedicatedPhase::Active
            })
            .ok_or(DedicatedControlError::Identity)?;
        entry
            .bank
            .prepare_mapping(identity, activation, plan)
            .map_err(DedicatedControlError::Bearer)
    }
    pub fn prepare_addition(
        &self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<crate::rel99_plan::CompiledRel99>,
        endpoints: Vec<crate::dedicated_bearers::BearerEndpoint>,
    ) -> Result<PreparedAddition, RejectedDedicatedAdditionPreparation> {
        let Some(entry) = self
            .entries
            .get(previous.ue.slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| {
                entry.bank.identity() == previous
                    && entry.phase == DedicatedPhase::Active
                    && entry.operation.is_none()
            })
        else {
            return Err(RejectedDedicatedAdditionPreparation {
                error: DedicatedControlError::Identity,
                endpoints,
            });
        };
        let mut prepared = entry
            .bank
            .prepare_addition(identity, activation, plan, endpoints)
            .map_err(|rejected: RejectedBearerAdditionPreparation| {
                RejectedDedicatedAdditionPreparation {
                    error: DedicatedControlError::Bearer(rejected.error),
                    endpoints: rejected.endpoints,
                }
            })?;
        if let Err(error) = entry.bank.retain_hsdpa_for_addition(&mut prepared) {
            return Err(RejectedDedicatedAdditionPreparation {
                error: DedicatedControlError::Bearer(error),
                endpoints: prepared.into_endpoints(),
            });
        }
        Ok(prepared)
    }
    pub(crate) fn stage_mapping_receiver(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        receiver: Box<DedicatedSampleReceiver>,
    ) -> Result<ReceiveCommandTicket, Box<DedicatedSampleReceiver>> {
        let valid = self.entry_mut(previous).is_ok_and(|entry| {
            entry.phase == DedicatedPhase::Active
                && entry.operation.is_none()
                && receiver.session().identity() == identity
                && entry.bank.mapping_phase(identity)
                    == Ok(crate::dedicated_bearers::MappingPhase::Staged { previous })
                && entry
                    .bank
                    .mapping_receive_configuration(identity)
                    .is_ok_and(|(configured, anchor)| {
                        Arc::ptr_eq(configured, receiver.session().configured())
                            && anchor.time == receiver.session().frame_anchor().time
                            && anchor.cfn == receiver.session().frame_anchor().cfn
                    })
        });
        if !valid {
            return Err(receiver);
        }
        self.port.stage_replacement(previous, receiver)
    }
    pub(crate) fn commit_mapping_receiver(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, DedicatedControlError> {
        let entry = self.entry_mut(previous)?;
        if entry.phase != DedicatedPhase::Active
            || entry.operation.is_some()
            || !matches!(entry.bank.mapping_phase(identity),
                Ok(crate::dedicated_bearers::MappingPhase::Staged { previous: owner }
                    | crate::dedicated_bearers::MappingPhase::Scheduled { previous: owner }) if owner == previous)
        {
            return Err(DedicatedControlError::Phase);
        }
        self.port
            .commit_replacement(previous, identity)
            .map_err(|_| {
                if self.port.available() {
                    DedicatedControlError::Capacity
                } else {
                    DedicatedControlError::Closed
                }
            })
    }
    pub(crate) fn retire_mapping_receiver(
        &mut self,
        previous: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, DedicatedControlError> {
        let entry = self.entry_mut(previous)?;
        if entry.phase != DedicatedPhase::Active || entry.operation.is_some() {
            return Err(DedicatedControlError::Phase);
        }
        let crate::dedicated_bearers::MappingPhase::Draining { next, .. } = entry
            .bank
            .mapping_phase(previous)
            .map_err(DedicatedControlError::Bearer)?
        else {
            return Err(DedicatedControlError::Phase);
        };
        if entry.bank.mapping_phase(next)
            != Ok(crate::dedicated_bearers::MappingPhase::Scheduled { previous })
        {
            return Err(DedicatedControlError::Phase);
        }
        self.port.retire_drained(previous).map_err(|_| {
            if self.port.available() {
                DedicatedControlError::Capacity
            } else {
                DedicatedControlError::Closed
            }
        })
    }
    /// Removal returns a worker owner, not a drained-air or mapping-retirement proof.
    pub(crate) fn remove_mapping_receiver(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, DedicatedControlError> {
        self.validate_mapping_removal(previous, identity)?;
        self.port.remove(identity).map_err(|_| {
            if self.port.available() {
                DedicatedControlError::Capacity
            } else {
                DedicatedControlError::Closed
            }
        })
    }

    fn validate_mapping_removal(
        &self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<(), DedicatedControlError> {
        use crate::dedicated_bearers::MappingPhase;
        let entry = self
            .entries
            .get(previous.ue.slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.bank.identity() == previous)
            .ok_or(DedicatedControlError::Identity)?;
        if !matches!(
            entry.phase,
            DedicatedPhase::Active | DedicatedPhase::Faulted
        ) || entry.operation.is_some()
        {
            return Err(DedicatedControlError::Phase);
        }
        if identity.ue != previous.ue {
            return Err(DedicatedControlError::Identity);
        }
        let original = entry
            .bank
            .mapping_phase(previous)
            .map_err(DedicatedControlError::Bearer)?;
        let requested = entry
            .bank
            .mapping_phase(identity)
            .map_err(DedicatedControlError::Bearer)?;
        let valid = if identity == previous {
            matches!(
                original,
                MappingPhase::Active | MappingPhase::Draining { .. }
            )
        } else {
            match (original, requested) {
                (MappingPhase::Active, MappingPhase::Staged { previous: owner }) => {
                    owner == previous
                }
                (
                    MappingPhase::Draining { next, .. },
                    MappingPhase::Scheduled { previous: owner },
                ) => next == identity && owner == previous,
                _ => false,
            }
        };
        if valid {
            Ok(())
        } else {
            Err(DedicatedControlError::Phase)
        }
    }

    /// Retire the closed RX lane before canceling its still-registered mapping.
    pub(crate) fn retire_removed_mapping_output(
        &mut self,
        previous: ReceiveIdentity,
        result: &ReceiveCommandResult,
    ) -> Result<usize, DedicatedControlError> {
        let ReceiveCommandResult::Removed {
            identity,
            receiver: Some(receiver),
        } = result
        else {
            return Err(DedicatedControlError::Protocol);
        };
        self.validate_mapping_removal(previous, *identity)?;
        let entry = self.entries[previous.ue.slot as usize]
            .as_ref()
            .expect("validated mapping");
        let (plan, anchor) = entry
            .bank
            .mapping_receive_configuration(*identity)
            .map_err(DedicatedControlError::Bearer)?;
        if receiver.session().identity() != *identity
            || !Arc::ptr_eq(plan, receiver.session().configured())
            || anchor.time != receiver.session().frame_anchor().time
            || anchor.cfn != receiver.session().frame_anchor().cfn
        {
            return Err(DedicatedControlError::Identity);
        }
        self.port
            .retire_output(*identity)
            .map_err(DedicatedControlError::Receive)
    }

    /// Both RX owners stay in the caller's result values. This returns only their shared bank.
    pub(crate) fn take_removed_mapping_pair(
        &mut self,
        previous: ReceiveIdentity,
        original: &ReceiveCommandResult,
        successor: &ReceiveCommandResult,
    ) -> Result<RetiredDedicated, DedicatedControlError> {
        let ReceiveCommandResult::Removed {
            identity: next,
            receiver: Some(next_receiver),
        } = successor
        else {
            return Err(DedicatedControlError::Protocol);
        };
        if *next == previous {
            return Err(DedicatedControlError::Identity);
        }
        self.validate_mapping_removal(previous, previous)?;
        self.validate_mapping_removal(previous, *next)?;
        let (old_id, old_receiver) = match original {
            ReceiveCommandResult::Removed {
                identity,
                receiver: Some(receiver),
            } => (*identity, receiver),
            ReceiveCommandResult::Retired {
                identity,
                receiver,
                retirement,
            } if retirement.previous() == previous && retirement.successor() == *next => {
                let bank = &self.entries[previous.ue.slot as usize]
                    .as_ref()
                    .expect("validated bank")
                    .bank;
                let (_, anchor) = bank
                    .mapping_receive_configuration(previous)
                    .map_err(DedicatedControlError::Bearer)?;
                if anchor.time != retirement.old_anchor().time
                    || anchor.cfn != retirement.old_anchor().cfn
                    || !matches!(bank.mapping_phase(previous), Ok(crate::dedicated_bearers::MappingPhase::Draining {
                            next: target, uplink_end, .. }) if target == *next && uplink_end == retirement.boundary())
                {
                    return Err(DedicatedControlError::Identity);
                }
                (*identity, receiver)
            }
            _ => return Err(DedicatedControlError::Protocol),
        };
        if old_id != previous {
            return Err(DedicatedControlError::Identity);
        }
        let slot = previous.ue.slot as usize;
        let entry = self.entries[slot].as_mut().expect("validated bank");
        for (id, receiver) in [(previous, old_receiver), (*next, next_receiver)] {
            let (plan, anchor) = entry
                .bank
                .mapping_receive_configuration(id)
                .map_err(DedicatedControlError::Bearer)?;
            if receiver.session().identity() != id
                || !Arc::ptr_eq(plan, receiver.session().configured())
                || anchor.time != receiver.session().frame_anchor().time
                || anchor.cfn != receiver.session().frame_anchor().cfn
            {
                return Err(DedicatedControlError::Identity);
            }
        }
        if entry
            .mapping_removal
            .as_ref()
            .is_some_and(|progress| progress.previous != previous || progress.successor != *next)
        {
            return Err(DedicatedControlError::Identity);
        }
        let progress = entry.mapping_removal.get_or_insert(MappingRemoval {
            previous,
            successor: *next,
            retired: [false; 2],
            discarded: 0,
        });
        entry.phase = DedicatedPhase::Faulted;
        for (index, id) in [previous, *next].into_iter().enumerate() {
            if !progress.retired[index] {
                let discarded = self
                    .port
                    .retire_output(id)
                    .map_err(DedicatedControlError::Receive)?;
                progress.discarded = progress.discarded.saturating_add(discarded);
                progress.retired[index] = true;
            }
        }
        let discarded = progress.discarded;
        let mut entry = self.entries[slot]
            .take()
            .expect("fully reclaimed mapping pair");
        entry.bank.retire();
        self.used_bytes -= entry.bytes;
        self.counters.discarded = self.counters.discarded.saturating_add(discarded as u64);
        Ok(RetiredDedicated {
            bank: entry.bank,
            receiver: None,
            discarded_transports: discarded,
            failure: entry.failure,
        })
    }
    pub fn stage_mapping(
        &mut self,
        prepared: PreparedMapping,
    ) -> Result<ReceiveIdentity, RejectedMapping> {
        let identity = prepared.identity();
        let bytes = match prepared.storage_bytes() {
            Ok(bytes) => bytes,
            Err(error) => return Err(RejectedMapping { error, prepared }),
        };
        let Some(used) = self
            .used_bytes
            .checked_add(bytes)
            .filter(|used| *used <= self.limits.maximum_bearer_bytes)
        else {
            return Err(RejectedMapping {
                error: BearerError::Capacity,
                prepared,
            });
        };
        let Some(entry) = self
            .entries
            .get_mut(identity.ue.slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| entry.phase == DedicatedPhase::Active)
        else {
            return Err(RejectedMapping {
                error: BearerError::Phase,
                prepared,
            });
        };
        let identity = entry.bank.stage_mapping(prepared)?;
        entry.bytes = entry.bank.storage_bytes();
        self.used_bytes = used;
        Ok(identity)
    }
    pub fn stage_addition(
        &mut self,
        previous: ReceiveIdentity,
        prepared: PreparedAddition,
    ) -> Result<ReceiveIdentity, RejectedAddition> {
        let bytes = prepared.storage_bytes();
        let Some(used) = self
            .used_bytes
            .checked_add(bytes)
            .filter(|used| *used <= self.limits.maximum_bearer_bytes)
        else {
            return Err(RejectedAddition {
                error: BearerError::Capacity,
                prepared,
            });
        };
        let Some(entry) = self
            .entries
            .get_mut(previous.ue.slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| {
                entry.bank.identity() == previous
                    && entry.phase == DedicatedPhase::Active
                    && entry.operation.is_none()
            })
        else {
            return Err(RejectedAddition {
                error: BearerError::Phase,
                prepared,
            });
        };
        let identity = entry.bank.stage_addition(prepared)?;
        entry.bytes = entry.bank.storage_bytes();
        self.used_bytes = used;
        Ok(identity)
    }
    pub fn cancel_addition(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<PreparedAddition, DedicatedControlError> {
        let entry = self
            .entries
            .get_mut(previous.ue.slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| {
                entry.bank.identity() == previous
                    && entry.phase == DedicatedPhase::Active
                    && entry.operation.is_none()
            })
            .ok_or(DedicatedControlError::Identity)?;
        let before = entry.bytes;
        let prepared = entry
            .bank
            .cancel_addition(previous, identity)
            .map_err(DedicatedControlError::Bearer)?;
        entry.bytes = entry.bank.storage_bytes();
        self.used_bytes -= before - entry.bytes;
        Ok(prepared)
    }
    pub fn prepare_release_mapping(
        &self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<crate::rel99_plan::CompiledRel99>,
    ) -> Result<PreparedMapping, DedicatedControlError> {
        let entry = self
            .entries
            .get(previous.ue.slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| {
                entry.bank.identity() == previous
                    && entry.phase == DedicatedPhase::Active
                    && entry.operation.is_none()
            })
            .ok_or(DedicatedControlError::Identity)?;
        let mut prepared = entry
            .bank
            .prepare_release_mapping(identity, activation, plan)
            .map_err(DedicatedControlError::Bearer)?;
        entry
            .bank
            .retain_hsdpa_for_mapping(&mut prepared)
            .map_err(DedicatedControlError::Bearer)?;
        Ok(prepared)
    }
    /// Cancels only the staged bank mapping, not a separately owned RX replacement or published RRC procedure.
    pub fn cancel_release(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<PreparedMapping, DedicatedControlError> {
        let entry = self
            .entries
            .get_mut(previous.ue.slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| {
                entry.bank.identity() == previous
                    && entry.phase == DedicatedPhase::Active
                    && entry.operation.is_none()
            })
            .ok_or(DedicatedControlError::Identity)?;
        let before = entry.bytes;
        let prepared = entry
            .bank
            .cancel_release(previous, identity)
            .map_err(DedicatedControlError::Bearer)?;
        entry.bytes = entry.bank.storage_bytes();
        self.used_bytes -= before - entry.bytes;
        Ok(prepared)
    }
    pub(crate) fn retire_release_mapping_with_view<T, E>(
        &mut self,
        accepted: &crate::signalling::AcceptedBearerRelease,
        receive: &ReceiveCommandResult,
        transmit: &crate::dedicated_tx::TransmitCommandOutcome,
        output: ReleaseOutputPolicy,
        ciphers: &mut umts_security::ciphering::BearerCiphers,
        cleanup: impl FnOnce(&[crate::dedicated_bearers::BearerId], &DedicatedBearers) -> Result<T, E>,
    ) -> Result<MappingRelease<T>, DedicatedReleaseError<E>> {
        let entry = self
            .entries
            .get_mut(accepted.key().slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| {
                entry.bank.identity().ue == accepted.key()
                    && entry.phase == DedicatedPhase::Active
                    && entry.operation.is_none()
            })
            .ok_or(DedicatedReleaseError::Control(
                DedicatedControlError::Identity,
            ))?;
        let before = entry.bytes;
        let result = entry
            .bank
            .retire_release_mapping_with_view(accepted, receive, transmit, output, ciphers, cleanup)
            .map_err(DedicatedReleaseError::Release)?;
        entry.bytes = entry.bank.storage_bytes();
        self.used_bytes -= before - entry.bytes;
        Ok(result)
    }
    pub fn commit_mapping(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<(), DedicatedControlError> {
        let entry = self
            .entries
            .get_mut(identity.ue.slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| {
                entry.phase == DedicatedPhase::Active && entry.bank.contains_identity(identity)
            })
            .ok_or(DedicatedControlError::Identity)?;
        entry
            .bank
            .commit_mapping(identity)
            .map_err(DedicatedControlError::Bearer)
    }
    #[cfg(test)]
    pub fn retire_mapping(
        &mut self,
        previous: ReceiveIdentity,
        receive: &ReceiveCommandResult,
        transmit: &crate::dedicated_tx::TransmitCommandOutcome,
    ) -> Result<(), DedicatedControlError> {
        let entry = self.entry_mut(previous)?;
        let before = entry.bytes;
        entry
            .bank
            .retire_mapping(previous, receive, transmit)
            .map_err(DedicatedControlError::Bearer)?;
        entry.bytes = entry.bank.storage_bytes();
        let released = before - entry.bytes;
        self.used_bytes -= released;
        Ok(())
    }
    pub fn retire_mapping_with<T, E>(
        &mut self,
        previous: ReceiveIdentity,
        receive: &ReceiveCommandResult,
        transmit: &crate::dedicated_tx::TransmitCommandOutcome,
        commit: impl FnOnce(&DedicatedBearers) -> Result<T, E>,
    ) -> Result<T, DedicatedMappingCommitError<E>> {
        let entry = self
            .entry_mut(previous)
            .map_err(DedicatedMappingCommitError::Control)?;
        let before = entry.bytes;
        let result = entry
            .bank
            .retire_mapping_with(previous, receive, transmit, commit)
            .map_err(|error| match error {
                crate::dedicated_bearers::MappingRetirementError::Bearer(error) => {
                    DedicatedMappingCommitError::Control(DedicatedControlError::Bearer(error))
                }
                crate::dedicated_bearers::MappingRetirementError::Commit(error) => {
                    DedicatedMappingCommitError::Commit(error)
                }
            })?;
        entry.bytes = entry.bank.storage_bytes();
        self.used_bytes -= before - entry.bytes;
        Ok(result)
    }
    pub(crate) fn paired_with(&self, receive: &crate::common_rx::CommonRx) -> bool {
        receive.dedicated_paired_with(&self.port)
    }
    pub fn phase(&self, identity: ReceiveIdentity) -> Option<DedicatedPhase> {
        self.entries
            .get(identity.ue.slot as usize)?
            .as_ref()
            .filter(|entry| entry.bank.identity() == identity)
            .map(|entry| entry.phase)
    }
    pub fn failure(&self, identity: ReceiveIdentity) -> Option<&DedicatedControlError> {
        self.entries
            .get(identity.ue.slot as usize)?
            .as_ref()
            .filter(|entry| entry.bank.identity() == identity)?
            .failure
            .as_ref()
    }
    pub fn stage(
        &mut self,
        prepared: Box<PreparedDedicated>,
    ) -> Result<ReceiveIdentity, RejectedDedicated> {
        let identity = prepared.bearers.identity();
        let receive = prepared.receiver.session();
        let anchor = prepared.bearers.frame_anchor();
        let reject = if !self.port.available() {
            Some(DedicatedControlError::Closed)
        } else if identity.ue.epoch != self.epoch()
            || identity != receive.identity()
            || identity.ue.slot as usize >= self.entries.len()
        {
            Some(DedicatedControlError::Identity)
        } else if self.entries[identity.ue.slot as usize].is_some()
            || prepared.bearers.phase() != BearerPhase::Prepared
        {
            Some(DedicatedControlError::Phase)
        } else if prepared.bearers.owns_signalling_rlc()
            || anchor.time != receive.frame_anchor().time
            || anchor.cfn != receive.frame_anchor().cfn
            || !Arc::ptr_eq(prepared.bearers.configured(), receive.configured())
        {
            Some(DedicatedControlError::Configuration)
        } else {
            None
        };
        if let Some(error) = reject {
            return Err(RejectedDedicated { error, prepared });
        }
        let bytes = prepared.bearers.storage_bytes();
        let Some(used) = self
            .used_bytes
            .checked_add(bytes)
            .filter(|used| *used <= self.limits.maximum_bearer_bytes)
        else {
            return Err(RejectedDedicated {
                error: DedicatedControlError::Capacity,
                prepared,
            });
        };
        let PreparedDedicated { receiver, bearers } = *prepared;
        let ticket = match self.port.prepare(receiver) {
            Ok(ticket) => ticket,
            Err(receiver) => {
                return Err(RejectedDedicated {
                    error: DedicatedControlError::Capacity,
                    prepared: Box::new(PreparedDedicated { receiver, bearers }),
                });
            }
        };
        self.entries[identity.ue.slot as usize] = Some(Entry {
            bank: bearers,
            phase: DedicatedPhase::Preparing,
            operation: Some(Operation::Install(ticket)),
            receiver: None,
            failure: None,
            bytes,
            mapping_removal: None,
            receive_status: None,
        });
        self.used_bytes = used;
        self.counters.staged = self.counters.staged.saturating_add(1);
        Ok(identity)
    }
    fn entry_mut(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<&mut Entry, DedicatedControlError> {
        self.entries
            .get_mut(identity.ue.slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| entry.bank.identity() == identity)
            .ok_or(DedicatedControlError::Identity)
    }
    // RX arming is only one admission step. The RRC/TX coordinator must authorize it after its own preparation.
    pub fn arm(&mut self, identity: ReceiveIdentity) -> Result<(), DedicatedControlError> {
        if self.phase(identity) != Some(DedicatedPhase::Prepared) {
            return Err(DedicatedControlError::Phase);
        }
        let ticket = self
            .port
            .arm(identity)
            .map_err(|_| DedicatedControlError::Capacity)?;
        let entry = self.entry_mut(identity)?;
        entry.operation = Some(Operation::Arm(ticket));
        entry.phase = DedicatedPhase::Arming;
        Ok(())
    }
    pub fn remove(&mut self, identity: ReceiveIdentity) -> Result<(), DedicatedControlError> {
        let entry = self.entry_mut(identity)?;
        if entry.operation.is_some()
            || entry.mapping_removal.is_some()
            || !matches!(
                entry.phase,
                DedicatedPhase::Active
                    | DedicatedPhase::Prepared
                    | DedicatedPhase::ArmFailed
                    | DedicatedPhase::Faulted
            )
        {
            return Err(DedicatedControlError::Phase);
        }
        let ticket = self
            .port
            .remove(identity)
            .map_err(|_| DedicatedControlError::Capacity)?;
        let entry = self.entry_mut(identity)?;
        entry.operation = Some(Operation::Remove(ticket));
        entry.phase = DedicatedPhase::Removing;
        Ok(())
    }
    pub fn take_retired(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<RetiredDedicated, DedicatedControlError> {
        let entry = self.entry_mut(identity)?;
        if !matches!(
            entry.phase,
            DedicatedPhase::Rejected | DedicatedPhase::Retired | DedicatedPhase::Closed
        ) || entry.operation.is_some()
        {
            return Err(DedicatedControlError::Phase);
        }
        let discarded = self
            .port
            .retire_output(identity)
            .map_err(DedicatedControlError::Receive)?;
        let mut entry = self.entries[identity.ue.slot as usize]
            .take()
            .expect("retired dedicated owner");
        entry.bank.retire();
        self.used_bytes -= entry.bytes;
        self.counters.discarded = self.counters.discarded.saturating_add(discarded as u64);
        Ok(RetiredDedicated {
            bank: entry.bank,
            receiver: entry.receiver,
            discarded_transports: discarded,
            failure: entry.failure,
        })
    }
    pub fn poll_receipts(&mut self) -> usize {
        let mut completed = 0;
        for _ in 0..self.limits.receipt_visits.get().min(self.entries.len()) {
            let index = self.receipt_cursor;
            self.receipt_cursor = (self.receipt_cursor + 1) % self.entries.len();
            let Some(entry) = &mut self.entries[index] else {
                continue;
            };
            if !self.port.available()
                && entry.operation.is_none()
                && !matches!(
                    entry.phase,
                    DedicatedPhase::Retired | DedicatedPhase::Rejected | DedicatedPhase::Closed
                )
            {
                entry.phase = DedicatedPhase::Closed;
                entry.failure = Some(DedicatedControlError::Closed);
            }
            let result = match &mut entry.operation {
                Some(
                    Operation::Install(ticket) | Operation::Arm(ticket) | Operation::Remove(ticket),
                ) => ticket.try_result(),
                None => None,
            };
            let Some(result) = result else {
                continue;
            };
            let operation = entry.operation.take().expect("pending dedicated receipt");
            let identity = entry.bank.identity();
            let failure = match (operation, result) {
                (_, ReceiveCommandResult::Closed) => {
                    entry.phase = DedicatedPhase::Closed;
                    Some(DedicatedControlError::Closed)
                }
                (Operation::Install(_), ReceiveCommandResult::Installed(id)) if id == identity => {
                    entry.phase = DedicatedPhase::Prepared;
                    self.counters.admitted = self.counters.admitted.saturating_add(1);
                    None
                }
                (
                    Operation::Install(_),
                    ReceiveCommandResult::InstallRejected { error, receiver },
                ) if receiver.session().identity() == identity => {
                    entry.receiver = Some(receiver);
                    entry.phase = DedicatedPhase::Rejected;
                    self.counters.rejected = self.counters.rejected.saturating_add(1);
                    Some(DedicatedControlError::Receive(error))
                }
                (Operation::Arm(_), ReceiveCommandResult::Armed(id)) if id == identity => {
                    match entry.bank.activate(identity) {
                        Ok(()) => {
                            entry.phase = DedicatedPhase::Active;
                            self.counters.armed = self.counters.armed.saturating_add(1);
                            None
                        }
                        Err(error) => {
                            entry.phase = DedicatedPhase::Faulted;
                            Some(DedicatedControlError::Bearer(error))
                        }
                    }
                }
                (
                    Operation::Arm(_),
                    ReceiveCommandResult::ArmRejected {
                        identity: id,
                        error,
                    },
                ) if id == identity => {
                    entry.phase = DedicatedPhase::ArmFailed;
                    Some(DedicatedControlError::Receive(error))
                }
                (
                    Operation::Remove(_),
                    ReceiveCommandResult::Removed {
                        identity: id,
                        receiver: Some(receiver),
                    },
                ) if id == identity && receiver.session().identity() == identity => {
                    entry.receiver = Some(receiver);
                    entry.phase = DedicatedPhase::Retired;
                    None
                }
                _ => {
                    entry.phase = DedicatedPhase::Faulted;
                    Some(DedicatedControlError::Protocol)
                }
            };
            if let Some(failure) = failure {
                entry.failure = Some(failure);
            }
            completed += 1;
        }
        completed
    }
    pub fn retire_orphans_some(
        &mut self,
        budget: NonZeroUsize,
        mut keep: impl FnMut(UeKey) -> bool,
    ) -> usize {
        let mut retired = 0;
        for _ in 0..budget.get().min(self.entries.len()) {
            let index = self.retire_cursor;
            self.retire_cursor = (self.retire_cursor + 1) % self.entries.len();
            let Some(entry) = &self.entries[index] else {
                continue;
            };
            let identity = entry.bank.identity();
            if keep(identity.ue) || entry.operation.is_some() {
                continue;
            }
            if matches!(
                entry.phase,
                DedicatedPhase::Retired | DedicatedPhase::Rejected | DedicatedPhase::Closed
            ) {
                if self.take_retired(identity).is_ok() {
                    retired += 1;
                }
            } else {
                let _ = self.remove(identity);
            }
        }
        retired
    }
    pub fn step<E>(
        &mut self,
        now: ChipInstant,
        mut service: impl FnMut(UeKey, &mut DedicatedBearers) -> Result<BearerProgress, E>,
        mut upper: impl FnMut(SduDelivery<'_>) -> bool,
    ) -> Result<DedicatedStep<E>, DedicatedControlError> {
        if now.epoch != self.epoch()
            || self
                .previous
                .is_some_and(|previous| now.index < previous.index)
        {
            return Err(DedicatedControlError::Time);
        }
        self.previous = Some(now);
        let receipts = self.poll_receipts();
        let routed = self.port.route_one(|delivery| {
            let identity = delivery.decoded.identity;
            let Some(entry) = self
                .entries
                .get_mut(identity.ue.slot as usize)
                .and_then(Option::as_mut)
                .filter(|entry| entry.bank.contains_identity(identity))
            else {
                self.counters.stale = self.counters.stale.saturating_add(1);
                return true;
            };
            if entry.phase != DedicatedPhase::Active {
                return false;
            }
            match entry.bank.enqueue_transport(delivery) {
                Ok(()) => {
                    self.counters.routed = self.counters.routed.saturating_add(1);
                    true
                }
                Err(BearerError::Capacity) => {
                    self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                    false
                }
                Err(error) => {
                    entry.failure = Some(DedicatedControlError::Bearer(error));
                    entry.phase = DedicatedPhase::Faulted;
                    self.counters.failures = self.counters.failures.saturating_add(1);
                    self.counters.discarded = self.counters.discarded.saturating_add(1);
                    true
                }
            }
        });
        let mut report = DedicatedStep {
            receipts,
            routed,
            serviced: 0,
            delivered: 0,
            failure: None,
        };
        for _ in 0..self.limits.service_visits.get().min(self.entries.len()) {
            let index = self.service_cursor;
            self.service_cursor = (self.service_cursor + 1) % self.entries.len();
            let Some(entry) = &mut self.entries[index] else {
                continue;
            };
            if entry.phase != DedicatedPhase::Active {
                continue;
            }
            if entry.bank.process_one(&mut upper) {
                report.delivered += 1;
                self.counters.delivered = self.counters.delivered.saturating_add(1);
            }
            let identity = entry.bank.identity();
            match service(identity.ue, &mut entry.bank) {
                Ok(_) => {}
                Err(error) => {
                    for (bearer, reset) in entry.bank.reset_evidence() {
                        tracing::warn!(
                            "Dedicated bearer reset before failure: identity={identity:?} bearer={bearer:?} reset={reset:?}"
                        );
                    }
                    entry.phase = DedicatedPhase::Faulted;
                    entry.failure = Some(DedicatedControlError::Processing);
                    report.failure = Some((identity, error));
                    self.counters.failures = self.counters.failures.saturating_add(1);
                }
            }
            report.serviced += 1;
        }
        Ok(report)
    }
}
