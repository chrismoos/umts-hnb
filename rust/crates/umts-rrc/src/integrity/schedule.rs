// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_codec::{CodecError, EncodedRrc, decode_uplink_dedicated, encode_downlink_dedicated};
use umts_rrc_schema::class_definitions::DLDCCHMessage;
use umts_security::{
    contexts::{IntegrityContext, KeyOrigin},
    counters::CounterInitialization,
};
use umts_types::{CnDomain, UeKey};

use super::{AcceptedUplink, DownlinkIntegrity, StreamError, UplinkIntegrity};
use crate::establishment::TransactionId;
use crate::security::{Completion, IntegrityChange, SignallingSet};

const BEARERS: usize = 5;
const EPOCHS: usize = 3;
const SPANS: usize = EPOCHS + 1;
const DOMAINS: usize = 2;
const CONTROL_BEARER: usize = 2;
const SEQUENCE_BITS: u32 = 4;
const SEQUENCE_MASK: u64 = (1 << SEQUENCE_BITS) - 1;
const FIRST_UPLINK_SEQUENCE: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleFailure {
    Owner,
    Bearer,
    Phase,
    Capacity,
    ReusedGeneration,
    Activation,
    Transaction,
    Suspended,
    Sequence,
    MissingEpoch,
    Counter(umts_security::Error),
    Integrity(super::Error),
    Codec(CodecError),
}
#[derive(Debug, PartialEq, Eq)]
pub enum ScheduleError<E> {
    Schedule(ScheduleFailure),
    Stream(StreamError<E>),
    Downstream(E),
}

pub(crate) struct IntegritySetup {
    pub context: IntegrityContext,
    pub counters: CounterInitialization,
    pub change: IntegrityChange,
    pub signalling: SignallingSet,
    pub transaction: TransactionId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryPlan {
    pub abandoned: KeyOrigin,
    pub previous_control_origin: Option<KeyOrigin>,
    pub next_control_count: Option<u32>,
    pub next_uplink_control_count: Option<u32>,
    pub release_connection: bool,
}

#[derive(Clone, Copy)]
struct Span {
    at: u64,
    origin: Option<KeyOrigin>,
}
#[derive(Clone, Copy)]
struct Timeline {
    spans: [Span; SPANS],
    length: usize,
}
impl Timeline {
    fn new() -> Self {
        Self {
            spans: [Span {
                at: 0,
                origin: None,
            }; SPANS],
            length: 1,
        }
    }
    fn selected(self, at: u64) -> Result<Option<KeyOrigin>, ScheduleFailure> {
        self.spans[..self.length]
            .iter()
            .rev()
            .find(|span| span.at <= at)
            .map(|span| span.origin)
            .ok_or(ScheduleFailure::Sequence)
    }
    fn assign(&mut self, origin: KeyOrigin, at: u64, floor: u64) -> Result<(), ScheduleFailure> {
        if at < floor {
            return Err(ScheduleFailure::Activation);
        }
        let last = self.spans[self.length - 1];
        if last.at > floor && last.at != at {
            return Err(ScheduleFailure::Activation);
        }
        if at < last.at {
            return Err(ScheduleFailure::Activation);
        }
        if last.at == at {
            self.spans[self.length - 1].origin = Some(origin);
        } else {
            if self.length == SPANS {
                return Err(ScheduleFailure::Capacity);
            }
            self.spans[self.length] = Span {
                at,
                origin: Some(origin),
            };
            self.length += 1;
        }
        Ok(())
    }
    fn prune(&mut self, floor: u64) {
        while self.length > 1 && self.spans[1].at <= floor {
            self.spans.copy_within(1..self.length, 0);
            self.length -= 1;
        }
    }
    fn retains(self, origin: KeyOrigin) -> bool {
        self.spans[..self.length]
            .iter()
            .any(|span| span.origin == Some(origin))
    }
}

#[derive(Default)]
struct Streams {
    tx: Option<DownlinkIntegrity>,
    rx: Option<UplinkIntegrity>,
}
struct Epoch {
    context: IntegrityContext,
    counters: CounterInitialization,
    change: IntegrityChange,
    streams: [Streams; BEARERS],
}
#[derive(Clone, Copy)]
struct Cursor {
    next: u64,
    previous_sequence: Option<u8>,
}
#[derive(Clone)]
struct Pending {
    origin: KeyOrigin,
    change: IntegrityChange,
    transaction: TransactionId,
    downlink: [u64; BEARERS],
    old_tx: [Timeline; BEARERS],
    old_rx: [Timeline; BEARERS],
    old_control_tx: Option<u32>,
    old_control_rx: Option<u32>,
    old_control_position: u64,
    started: bool,
}

#[derive(Clone)]
enum ExchangeState {
    Ready,
    Pending(Pending),
    Unconfirmed(Pending),
}
impl ExchangeState {
    fn pending(&self) -> Option<&Pending> {
        match self {
            Self::Pending(pending) => Some(pending),
            _ => None,
        }
    }
    fn unconfirmed(&self) -> Option<&Pending> {
        match self {
            Self::Unconfirmed(pending) => Some(pending),
            _ => None,
        }
    }
    fn ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

pub struct IntegritySchedule {
    key: UeKey,
    fresh: u32,
    signalling: SignallingSet,
    epochs: [Option<Epoch>; EPOCHS],
    highest_generation: [u64; DOMAINS],
    state: TransactionState,
}
#[derive(Clone)]
struct TransactionState {
    life: Life,
    tx: [Timeline; BEARERS],
    rx: [Timeline; BEARERS],
    tx_cursor: [Cursor; BEARERS],
    rx_cursor: [Cursor; BEARERS],
    exchange: ExchangeState,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Life {
    Unprotected,
    Active,
    ReleaseRequired,
}

impl IntegritySchedule {
    pub fn new(key: UeKey, fresh: u32, signalling: SignallingSet) -> Self {
        Self {
            key,
            fresh,
            signalling,
            epochs: std::array::from_fn(|_| None),
            highest_generation: [0; DOMAINS],
            state: TransactionState {
                life: Life::Unprotected,
                tx: [Timeline::new(); BEARERS],
                rx: [Timeline::new(); BEARERS],
                tx_cursor: [Cursor {
                    next: 0,
                    previous_sequence: None,
                }; BEARERS],
                rx_cursor: [Cursor {
                    next: 1,
                    previous_sequence: Some(0),
                }; BEARERS],
                exchange: ExchangeState::Ready,
            },
        }
    }
    pub fn pending_origin(&self) -> Option<KeyOrigin> {
        self.state.exchange.pending().map(|pending| pending.origin)
    }
    pub fn unconfirmed_origin(&self) -> Option<KeyOrigin> {
        self.state
            .exchange
            .unconfirmed()
            .map(|completed| completed.origin)
    }

    /// Confirmation requires the procedure owner's completion-delivery evidence, not mere STATUS publication.
    pub fn confirm_delivery_with<T, E>(
        &mut self,
        key: UeKey,
        origin: KeyOrigin,
        confirm: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        self.owner(key).map_err(ScheduleError::Schedule)?;
        if self.unconfirmed_origin() != Some(origin) {
            return Err(ScheduleError::Schedule(ScheduleFailure::Phase));
        }
        let result = confirm().map_err(ScheduleError::Downstream)?;
        self.state.exchange = ExchangeState::Ready;
        Ok(result)
    }
    pub fn configurations(&self) -> usize {
        self.epochs.iter().flatten().count()
    }
    pub fn release_required(&self) -> bool {
        self.state.life == Life::ReleaseRequired
    }
    pub fn next_downlink_sequence(&self, key: UeKey, bearer: u8) -> Result<u8, ScheduleFailure> {
        let slot = self.bearer(key, bearer, true)?;
        Ok((self.state.tx_cursor[slot].next & SEQUENCE_MASK) as u8)
    }
    pub fn next_downlink_activation(&self, key: UeKey, bearer: u8) -> Result<u8, ScheduleFailure> {
        let slot = self.bearer(key, bearer, true)?;
        let timeline = self.state.tx[slot];
        let next = self.state.tx_cursor[slot].next;
        let at = timeline.spans[timeline.length - 1].at.max(next);
        Ok((at & SEQUENCE_MASK) as u8)
    }
    pub fn downlink_origin(
        &self,
        key: UeKey,
        bearer: u8,
    ) -> Result<Option<KeyOrigin>, ScheduleFailure> {
        let slot = self.bearer(key, bearer, false)?;
        self.downlink_selected(slot)
    }
    fn downlink_selected(&self, slot: usize) -> Result<Option<KeyOrigin>, ScheduleFailure> {
        self.state.downlink_selected(slot)
    }
    pub fn allows_unprotected_uplink(
        &self,
        key: UeKey,
        bearer: u8,
    ) -> Result<bool, ScheduleFailure> {
        let slot = self.bearer(key, bearer, false)?;
        // TS 25.331 8.5.10.2 applies uplink activation independently per SRB, with RB2 changing at Complete.
        if self.state.exchange.pending().is_some() && slot == CONTROL_BEARER {
            return Ok(false);
        }
        Ok(self.state.rx[slot]
            .selected(self.state.rx_cursor[slot].next)?
            .is_none())
    }
    pub fn uplink_suspended(&self, key: UeKey, bearer: u8) -> Result<bool, ScheduleFailure> {
        let slot = self.bearer(key, bearer, false)?;
        Ok(self.state.exchange.pending().is_some() && slot != CONTROL_BEARER)
    }

    pub(crate) fn begin_with<T, E>(
        &mut self,
        setup: IntegritySetup,
        publish: impl FnOnce(&mut DownlinkIntegrity) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        let fail = ScheduleError::Schedule;
        let origin = setup.context.origin();
        self.owner(origin.ue).map_err(fail)?;
        if setup.signalling != self.signalling {
            return Err(fail(ScheduleFailure::Activation));
        }
        if !self.state.exchange.ready() {
            return Err(fail(ScheduleFailure::Phase));
        }
        match (self.state.life == Life::Active, setup.change) {
            (false, IntegrityChange::Start { fresh }) if fresh == self.fresh => (),
            (true, IntegrityChange::Modify(activation))
                if activation.signalling() == self.signalling => {}
            _ => return Err(fail(ScheduleFailure::Activation)),
        }
        let domain = domain_index(origin.domain);
        if origin.generation.get() <= self.highest_generation[domain] {
            return Err(fail(ScheduleFailure::ReusedGeneration));
        }
        let slot = self
            .epochs
            .iter()
            .position(Option::is_none)
            .ok_or(fail(ScheduleFailure::Capacity))?;
        let old_control_origin = self.state.tx[CONTROL_BEARER]
            .selected(self.state.tx_cursor[CONTROL_BEARER].next)
            .map_err(fail)?;
        let old_control_tx = old_control_origin
            .map(|origin| self.tx_count(origin, CONTROL_BEARER))
            .transpose()
            .map_err(fail)?;
        let mut candidate = self.state.clone();
        let mut downlink = [0; BEARERS];
        for (bearer, at) in downlink
            .iter_mut()
            .enumerate()
            .take(self.signalling.count())
        {
            *at = if bearer == CONTROL_BEARER {
                self.state.tx_cursor[bearer].next
            } else {
                match setup.change {
                    IntegrityChange::Start { .. } => self.state.tx_cursor[bearer].next,
                    IntegrityChange::Modify(activation) => occurrence(
                        activation
                            .sequence(bearer as u8)
                            .map_err(|_| fail(ScheduleFailure::Activation))?,
                        self.state.tx_cursor[bearer].next,
                    )
                    .map_err(fail)?,
                }
            };
            candidate.tx[bearer]
                .assign(origin, *at, self.state.tx_cursor[bearer].next)
                .map_err(fail)?;
        }
        let mut epoch = Epoch {
            context: setup.context,
            counters: setup.counters,
            change: setup.change,
            streams: std::array::from_fn(|_| Streams::default()),
        };
        candidate.exchange = ExchangeState::Pending(Pending {
            origin,
            change: setup.change,
            transaction: setup.transaction,
            downlink,
            old_tx: self.state.tx,
            old_rx: self.state.rx,
            old_control_tx,
            old_control_rx: None,
            old_control_position: self.state.tx_cursor[CONTROL_BEARER].next,
            started: self.state.life == Life::Active,
        });
        let count = setup
            .counters
            .integrity_first_count(
                (self.state.tx_cursor[CONTROL_BEARER].next & SEQUENCE_MASK) as u8,
                self.state.tx_cursor[CONTROL_BEARER].previous_sequence,
            )
            .map_err(|error| fail(ScheduleFailure::Counter(error)))?;
        let mut stream = DownlinkIntegrity::new(
            epoch.context.clone(),
            CONTROL_BEARER as u8,
            self.fresh,
            count,
        )
        .map_err(|error| fail(ScheduleFailure::Integrity(error)))?;
        let next = candidate.tx_cursor[CONTROL_BEARER]
            .next
            .checked_add(1)
            .ok_or(fail(ScheduleFailure::Sequence))?;
        let result = publish(&mut stream).map_err(ScheduleError::Downstream)?;
        epoch.streams[CONTROL_BEARER].tx = Some(stream);
        self.epochs[slot] = Some(epoch);
        candidate.tx_cursor[CONTROL_BEARER] = Cursor {
            next,
            previous_sequence: Some(((next - 1) & SEQUENCE_MASK) as u8),
        };
        self.highest_generation[domain] = origin.generation.get();
        candidate.life = Life::Active;
        self.state = candidate;
        Ok(result)
    }
    pub fn publish_with<T, E>(
        &mut self,
        key: UeKey,
        bearer: u8,
        message: &DLDCCHMessage,
        publish: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        PublicationSchedule {
            key: self.key,
            fresh: self.fresh,
            signalling: self.signalling,
            state: &mut self.state,
            epochs: &mut self.epochs,
        }
        .publish_with(key, bearer, message, publish)
    }
    pub fn receive_with<'a, T, E>(
        &mut self,
        key: UeKey,
        bearer: u8,
        bytes: &'a [u8],
        scratch: &mut [u8],
        accept: impl FnOnce(&mut CompletionAdmission<'_>, AcceptedUplink<'a>) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        self.receive_inner(key, bearer, None, bytes, scratch, accept)
    }

    pub fn allows_unprotected_common(&self, key: UeKey) -> Result<bool, ScheduleFailure> {
        self.owner(key)?;
        let timeline = self.common_receive_timeline();
        Ok(timeline.spans[timeline.length - 1].origin.is_none())
    }

    /// Cell Update retains the prior protected RB0 configuration during unfinished rekeying.
    pub fn receive_common_with<'a, T, E>(
        &mut self,
        key: UeKey,
        bytes: &'a [u8],
        scratch: &mut [u8],
        accept: impl FnOnce(AcceptedUplink<'a>) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        self.receive_common_bits_with(key, bytes, bytes.len() * super::OCTET_BITS, scratch, accept)
    }

    pub fn receive_common_bits_with<'a, T, E>(
        &mut self,
        key: UeKey,
        bytes: &'a [u8],
        bit_length: usize,
        scratch: &mut [u8],
        accept: impl FnOnce(AcceptedUplink<'a>) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        self.receive_inner(key, 0, Some(bit_length), bytes, scratch, |_, accepted| {
            accept(accepted)
        })
    }

    pub fn receive_common_response_with<'a, T, E>(
        &mut self,
        key: UeKey,
        bytes: &'a [u8],
        bit_length: usize,
        scratch: &mut [u8],
        accept: impl FnOnce(&mut CompletionAdmission<'_>, AcceptedUplink<'a>) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        self.receive_inner(key, 0, Some(bit_length), bytes, scratch, accept)
    }

    fn common_receive_timeline(&self) -> Timeline {
        self.state
            .exchange
            .pending()
            .or_else(|| self.state.exchange.unconfirmed())
            .map_or(self.state.rx[0], |pending| pending.old_rx[0])
    }

    fn protected_common_receive_timeline(&self) -> Timeline {
        match &self.state.exchange {
            // TS 25.331 8.1.12.2.2 Note 2 permits rollback, but initial Start has no old integrity key.
            ExchangeState::Unconfirmed(pending)
                if matches!(pending.change, IntegrityChange::Start { .. }) =>
            {
                self.state.rx[0]
            }
            _ => self.common_receive_timeline(),
        }
    }

    fn receive_inner<'a, T, E>(
        &mut self,
        key: UeKey,
        bearer: u8,
        common_bits: Option<usize>,
        bytes: &'a [u8],
        scratch: &mut [u8],
        accept: impl FnOnce(&mut CompletionAdmission<'_>, AcceptedUplink<'a>) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        let fail = ScheduleError::Schedule;
        let common = common_bits.is_some();
        let slot = self.bearer(key, bearer, common).map_err(fail)?;
        if self.state.exchange.pending().is_some() && slot != CONTROL_BEARER && !common {
            return Err(fail(ScheduleFailure::Suspended));
        }
        let sequence = super::uplink_sequence(bytes)
            .map_err(|error| fail(ScheduleFailure::Integrity(error)))?;
        let cursor = self.state.rx_cursor[slot];
        if !common && cursor.previous_sequence == Some(sequence) {
            return Err(ScheduleError::Stream(StreamError::Duplicate));
        }
        let timeline = if common {
            self.protected_common_receive_timeline()
        } else {
            self.state.rx[slot]
        };
        let activation = timeline.spans[timeline.length - 1].at;
        let floor = if common {
            cursor.next.max(activation)
        } else {
            cursor.next
        };
        let position = occurrence(sequence, floor).map_err(fail)?;
        let next = position
            .checked_add(1)
            .ok_or(fail(ScheduleFailure::Sequence))?;
        let completion_origin =
            if !common && slot == CONTROL_BEARER {
                self.state
                    .exchange
                    .pending()
                    .map(|pending| {
                        let message =
                            decode_uplink_dedicated(bytes, bytes.len() * super::OCTET_BITS)
                                .map_err(ScheduleFailure::Codec)?;
                        Ok(matches!(message.value.message,
                    umts_rrc_schema::class_definitions::ULDCCHMessageType::securityModeComplete(_)
                )
                        .then_some(pending.origin))
                    })
                    .transpose()
                    .map_err(fail)?
                    .flatten()
            } else {
                None
            };
        let origin = completion_origin
            .or(timeline.selected(position).map_err(fail)?)
            .ok_or(fail(ScheduleFailure::MissingEpoch))?;
        let epoch_slot = self.epoch_slot(origin).map_err(fail)?;
        let old_control_rx = if slot == CONTROL_BEARER {
            self.old_control_receive_count().map_err(fail)?
        } else {
            None
        };
        let mut candidate = self.state.clone();
        let (mut stream, initialized) = self
            .take_rx_stream(
                epoch_slot,
                slot,
                common.then_some((activation & SEQUENCE_MASK) as u8),
            )
            .map_err(fail)?;
        let result = stream
            .receive_bits_with(
                origin,
                bytes,
                common_bits.unwrap_or(bytes.len() * super::OCTET_BITS),
                scratch,
                |accepted| {
                    accept(
                        &mut CompletionAdmission {
                            state: &mut candidate,
                            epochs: &mut self.epochs,
                            key: self.key,
                            fresh: self.fresh,
                            signalling: self.signalling,
                            old_control_rx,
                            control_activation: completion_origin.map(|_| position),
                        },
                        accepted,
                    )
                },
            )
            .map_err(stream_error);
        if initialized || result.is_ok() {
            self.epochs[epoch_slot]
                .as_mut()
                .expect("retained RX epoch")
                .streams[slot]
                .rx = Some(stream);
        }
        let result = result?;
        candidate.rx_cursor[slot] = Cursor {
            next,
            previous_sequence: Some(sequence),
        };
        self.state = candidate;
        Ok(result)
    }
    fn complete_state(
        signalling: SignallingSet,
        state: &mut TransactionState,
        completion: Completion,
        old_control_rx: Option<u32>,
        control_activation: u64,
    ) -> Result<(), ScheduleFailure> {
        let pending = state.exchange.pending().ok_or(ScheduleFailure::Phase)?;
        if completion.origin() != pending.origin {
            return Err(ScheduleFailure::Owner);
        }
        if completion.transaction() != pending.transaction {
            return Err(ScheduleFailure::Transaction);
        }
        if completion.integrity().signalling() != signalling {
            return Err(ScheduleFailure::Activation);
        }
        let mut rx = state.rx;
        // TS 25.331 8.5.10.2 changes RB2 uplink integrity at the response, not command publication.
        rx[CONTROL_BEARER].assign(
            pending.origin,
            control_activation,
            state.rx_cursor[CONTROL_BEARER].next,
        )?;
        for (bearer, timeline) in rx.iter_mut().enumerate().take(signalling.count()) {
            if bearer == CONTROL_BEARER {
                continue;
            }
            let sequence = completion
                .integrity()
                .sequence(bearer as u8)
                .map_err(|_| ScheduleFailure::Activation)?;
            let floor = if bearer == 0 {
                // TS 25.331 8.1.12.3 advances RB0 to activation even when it sends no message before another rekey.
                state.rx_cursor[bearer]
                    .next
                    .max(timeline.spans[timeline.length - 1].at)
            } else {
                state.rx_cursor[bearer].next
            };
            let at = match pending.change {
                IntegrityChange::Start { .. } => {
                    if sequence > FIRST_UPLINK_SEQUENCE {
                        return Err(ScheduleFailure::Activation);
                    }
                    // Both boundaries protect the first uplink SN1 (TS 25.331 8.6.3.5.1 and 8.5.10.2).
                    floor.max(u64::from(sequence))
                }
                IntegrityChange::Modify(_) => occurrence(sequence, floor)?,
            };
            timeline.assign(pending.origin, at, floor)?;
        }
        state.rx = rx;
        let ExchangeState::Pending(mut pending) =
            std::mem::replace(&mut state.exchange, ExchangeState::Ready)
        else {
            unreachable!("validated pending integrity exchange");
        };
        pending.old_control_rx = old_control_rx;
        state.exchange = ExchangeState::Unconfirmed(pending);
        Ok(())
    }
    /// Old references and the completion's L2 acknowledgment must be settled before retirement.
    pub fn retire(&mut self, key: UeKey, origin: KeyOrigin) -> Result<(), ScheduleFailure> {
        self.owner(key)?;
        if !self.state.exchange.ready() {
            return Err(ScheduleFailure::Phase);
        }
        let mut candidate = self.state.clone();
        for bearer in 0..self.signalling.count() {
            candidate.tx[bearer].prune(self.state.tx_cursor[bearer].next);
            candidate.rx[bearer].prune(self.receive_retirement_floor(bearer));
            if candidate.tx[bearer].retains(origin) || candidate.rx[bearer].retains(origin) {
                return Err(ScheduleFailure::Activation);
            }
        }
        let slot = self.epoch_slot(origin)?;
        self.epochs[slot] = None;
        self.state = candidate;
        Ok(())
    }
    /// The caller must settle peer fallback and old lower-layer references before collecting unused epochs.
    pub fn collect_unused(&mut self, key: UeKey) -> Result<usize, ScheduleFailure> {
        self.owner(key)?;
        if !self.state.exchange.ready() {
            return Err(ScheduleFailure::Phase);
        }
        let mut candidate = self.state.clone();
        for bearer in 0..self.signalling.count() {
            candidate.tx[bearer].prune(candidate.tx_cursor[bearer].next);
            candidate.rx[bearer].prune(self.receive_retirement_floor(bearer));
        }
        let mut removed = 0;
        for epoch in &mut self.epochs {
            if epoch.as_ref().is_some_and(|epoch| {
                let origin = epoch.context.origin();
                !(0..self.signalling.count()).any(|bearer| {
                    candidate.tx[bearer].retains(origin) || candidate.rx[bearer].retains(origin)
                })
            }) {
                *epoch = None;
                removed += 1;
            }
        }
        self.state = candidate;
        Ok(removed)
    }

    fn receive_retirement_floor(&self, bearer: usize) -> u64 {
        let next = self.state.rx_cursor[bearer].next;
        if bearer == 0 {
            // TS 25.331 8.1.12.3 advances idle RB0 to its new activation after settled completion.
            let timeline = self.state.rx[bearer];
            next.max(timeline.spans[timeline.length - 1].at)
        } else {
            next
        }
    }
    pub fn abort_with<T, E>(
        &mut self,
        key: UeKey,
        origin: KeyOrigin,
        recover: impl FnOnce(RecoveryPlan) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        self.recover_with(key, origin, false, recover)
    }

    /// The owner must admit matching cipher, core and lower-layer recovery before committing integrity restoration.
    pub fn recover_completed_with<T, E>(
        &mut self,
        key: UeKey,
        origin: KeyOrigin,
        recover: impl FnOnce(RecoveryPlan) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        self.recover_with(key, origin, true, recover)
    }

    fn recover_with<T, E>(
        &mut self,
        key: UeKey,
        origin: KeyOrigin,
        completed: bool,
        recover: impl FnOnce(RecoveryPlan) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        let fail = ScheduleError::Schedule;
        self.owner(key).map_err(fail)?;
        let pending = if completed {
            self.state.exchange.unconfirmed()
        } else {
            self.state.exchange.pending()
        }
        .filter(|pending| pending.origin == origin)
        .ok_or(fail(ScheduleFailure::Phase))?;
        let previous = pending.old_tx[CONTROL_BEARER]
            .selected(pending.old_control_position)
            .map_err(fail)?;
        let skipped = self.state.tx_cursor[CONTROL_BEARER]
            .next
            .checked_sub(pending.old_control_position)
            .ok_or(fail(ScheduleFailure::Sequence))?;
        let next_count = pending
            .old_control_tx
            .map(|count| {
                count
                    .checked_add(u32::try_from(skipped).map_err(|_| ScheduleFailure::Sequence)?)
                    .ok_or(ScheduleFailure::Counter(umts_security::Error::Exhausted))
            })
            .transpose()
            .map_err(fail)?;
        let next_uplink_count = if completed {
            pending
                .old_control_rx
                .map(|count| {
                    count
                        .checked_add(2)
                        .ok_or(ScheduleFailure::Counter(umts_security::Error::Exhausted))
                })
                .transpose()
                .map_err(fail)?
        } else {
            None
        };
        let restored_rx = if let Some(count) = next_uplink_count {
            let old_receive_origin = pending.old_rx[CONTROL_BEARER]
                .selected(self.state.rx_cursor[CONTROL_BEARER].next)
                .map_err(fail)?
                .ok_or(fail(ScheduleFailure::MissingEpoch))?;
            let slot = self.epoch_slot(old_receive_origin).map_err(fail)?;
            let epoch = self.epochs[slot]
                .as_ref()
                .ok_or(fail(ScheduleFailure::MissingEpoch))?;
            Some((
                slot,
                UplinkIntegrity::from_next_count(
                    epoch.context.clone(),
                    CONTROL_BEARER as u8,
                    self.fresh,
                    count,
                ),
            ))
        } else {
            None
        };
        let mut candidate = self.state.clone();
        candidate.tx = pending.old_tx;
        candidate.rx = pending.old_rx;
        candidate.life = if pending.started {
            Life::Active
        } else {
            Life::ReleaseRequired
        };
        candidate.exchange = ExchangeState::Ready;
        let restored = if let (Some(previous), Some(count)) = (previous, next_count) {
            let slot = self.epoch_slot(previous).map_err(fail)?;
            let epoch = self.epochs[slot].as_ref().expect("retained recovery epoch");
            let stream = DownlinkIntegrity::new(
                epoch.context.clone(),
                CONTROL_BEARER as u8,
                self.fresh,
                count,
            )
            .map_err(|error| fail(ScheduleFailure::Integrity(error)))?;
            Some((slot, stream))
        } else {
            None
        };
        let abandoned = self.epoch_slot(origin).map_err(fail)?;
        let result = recover(RecoveryPlan {
            abandoned: origin,
            previous_control_origin: previous,
            next_control_count: next_count,
            next_uplink_control_count: next_uplink_count,
            release_connection: !pending.started,
        })
        .map_err(ScheduleError::Downstream)?;
        if let Some((slot, stream)) = restored {
            self.epochs[slot]
                .as_mut()
                .expect("retained recovery epoch")
                .streams[CONTROL_BEARER]
                .tx = Some(stream);
        }
        if let Some((slot, stream)) = restored_rx {
            self.epochs[slot]
                .as_mut()
                .expect("retained recovery epoch")
                .streams[CONTROL_BEARER]
                .rx = Some(stream);
        }
        self.epochs[abandoned] = None;
        self.state = candidate;
        Ok(result)
    }
    fn old_control_receive_count(&self) -> Result<Option<u32>, ScheduleFailure> {
        let Some(pending) = self.state.exchange.pending() else {
            return Ok(None);
        };
        let Some(origin) =
            pending.old_rx[CONTROL_BEARER].selected(self.state.rx_cursor[CONTROL_BEARER].next)?
        else {
            return Ok(None);
        };
        let epoch = self.epochs[self.epoch_slot(origin)?]
            .as_ref()
            .ok_or(ScheduleFailure::MissingEpoch)?;
        Ok(epoch.streams[CONTROL_BEARER]
            .rx
            .as_ref()
            .and_then(UplinkIntegrity::last_count))
    }

    fn tx_count(&self, origin: KeyOrigin, bearer: usize) -> Result<u32, ScheduleFailure> {
        if let Some(stream) = &self.epochs[self.epoch_slot(origin)?]
            .as_ref()
            .ok_or(ScheduleFailure::MissingEpoch)?
            .streams[bearer]
            .tx
        {
            return stream.next_count().map_err(ScheduleFailure::Counter);
        }
        let epoch = self.epochs[self.epoch_slot(origin)?]
            .as_ref()
            .ok_or(ScheduleFailure::MissingEpoch)?;
        epoch
            .counters
            .integrity_first_count(
                (self.state.tx_cursor[bearer].next & SEQUENCE_MASK) as u8,
                self.state.tx_cursor[bearer].previous_sequence,
            )
            .map_err(ScheduleFailure::Counter)
    }
    fn take_rx_stream(
        &mut self,
        epoch_slot: usize,
        bearer: usize,
        common_activation: Option<u8>,
    ) -> Result<(UplinkIntegrity, bool), ScheduleFailure> {
        let epoch = self.epochs[epoch_slot]
            .as_mut()
            .ok_or(ScheduleFailure::MissingEpoch)?;
        if let Some(stream) = epoch.streams[bearer].rx.take() {
            return Ok((stream, true));
        }
        if let Some(sequence) = common_activation
            && matches!(epoch.change, IntegrityChange::Modify(_))
        {
            // TS 25.331 8.1.12.3 sets RB0's next SN to activation. Activation zero increments initialized HFN.
            let previous = sequence.wrapping_sub(1) & SEQUENCE_MASK as u8;
            let first = epoch
                .counters
                .integrity_first_count(sequence, Some(previous))
                .map_err(ScheduleFailure::Counter)?;
            return Ok((
                UplinkIntegrity::from_next_count(
                    epoch.context.clone(),
                    bearer as u8,
                    self.fresh,
                    first,
                ),
                false,
            ));
        }
        UplinkIntegrity::new(
            epoch.context.clone(),
            bearer as u8,
            self.fresh,
            epoch.counters.start(),
            self.state.rx_cursor[bearer].previous_sequence,
        )
        .map(|stream| (stream, false))
        .map_err(ScheduleFailure::Integrity)
    }
    fn epoch_slot(&self, origin: KeyOrigin) -> Result<usize, ScheduleFailure> {
        if origin.ue != self.key {
            return Err(ScheduleFailure::Owner);
        }
        self.epochs
            .iter()
            .position(|epoch| {
                epoch
                    .as_ref()
                    .is_some_and(|epoch| epoch.context.origin() == origin)
            })
            .ok_or(ScheduleFailure::MissingEpoch)
    }
    fn owner(&self, key: UeKey) -> Result<(), ScheduleFailure> {
        if key != self.key {
            Err(ScheduleFailure::Owner)
        } else if self.state.life == Life::ReleaseRequired {
            Err(ScheduleFailure::Phase)
        } else {
            Ok(())
        }
    }
    fn bearer(&self, key: UeKey, bearer: u8, model_only: bool) -> Result<usize, ScheduleFailure> {
        self.owner(key)?;
        let bearer = usize::from(bearer);
        if bearer >= self.signalling.count() || (!model_only && bearer == 0) {
            return Err(ScheduleFailure::Bearer);
        }
        Ok(bearer)
    }
}

impl TransactionState {
    fn downlink_selected(&self, slot: usize) -> Result<Option<KeyOrigin>, ScheduleFailure> {
        let cursor = self.tx_cursor[slot];
        if let Some(pending) = self.exchange.pending()
            && (slot == CONTROL_BEARER
                || matches!(pending.change, IntegrityChange::Start { .. })
                || cursor.next >= pending.downlink[slot])
        {
            return Err(ScheduleFailure::Suspended);
        }
        self.tx[slot].selected(cursor.next)
    }
}

struct PublicationSchedule<'a> {
    key: UeKey,
    fresh: u32,
    signalling: SignallingSet,
    state: &'a mut TransactionState,
    epochs: &'a mut [Option<Epoch>; EPOCHS],
}
impl PublicationSchedule<'_> {
    fn publish_with<T, E>(
        &mut self,
        key: UeKey,
        bearer: u8,
        message: &DLDCCHMessage,
        publish: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        let fail = ScheduleError::Schedule;
        if key != self.key {
            return Err(fail(ScheduleFailure::Owner));
        }
        if self.state.life == Life::ReleaseRequired {
            return Err(fail(ScheduleFailure::Phase));
        }
        let slot = usize::from(bearer);
        if slot == 0 || slot >= self.signalling.count() {
            return Err(fail(ScheduleFailure::Bearer));
        }
        let cursor = self.state.tx_cursor[slot];
        let Some(origin) = self.state.downlink_selected(slot).map_err(fail)? else {
            let encoded = encode_downlink_dedicated(message)
                .map_err(|error| fail(ScheduleFailure::Codec(error)))?;
            return publish(&encoded).map_err(ScheduleError::Downstream);
        };
        let next = cursor
            .next
            .checked_add(1)
            .ok_or(fail(ScheduleFailure::Sequence))?;
        let epoch_slot = self
            .epochs
            .iter()
            .position(|epoch| {
                epoch
                    .as_ref()
                    .is_some_and(|epoch| epoch.context.origin() == origin)
            })
            .ok_or(fail(ScheduleFailure::MissingEpoch))?;
        let (mut stream, initialized) = self.take_tx_stream(epoch_slot, slot).map_err(fail)?;
        let result = stream
            .publish_with(origin, message, publish)
            .map_err(stream_error);
        if initialized || result.is_ok() {
            self.epochs[epoch_slot]
                .as_mut()
                .expect("retained TX epoch")
                .streams[slot]
                .tx = Some(stream);
        }
        let result = result?;
        self.state.tx_cursor[slot] = Cursor {
            next,
            previous_sequence: Some((cursor.next & SEQUENCE_MASK) as u8),
        };
        Ok(result)
    }
    fn take_tx_stream(
        &mut self,
        epoch_slot: usize,
        bearer: usize,
    ) -> Result<(DownlinkIntegrity, bool), ScheduleFailure> {
        let epoch = self.epochs[epoch_slot]
            .as_mut()
            .ok_or(ScheduleFailure::MissingEpoch)?;
        if let Some(stream) = epoch.streams[bearer].tx.take() {
            return Ok((stream, true));
        }
        let count = epoch
            .counters
            .integrity_first_count(
                (self.state.tx_cursor[bearer].next & SEQUENCE_MASK) as u8,
                self.state.tx_cursor[bearer].previous_sequence,
            )
            .map_err(ScheduleFailure::Counter)?;
        DownlinkIntegrity::new(epoch.context.clone(), bearer as u8, self.fresh, count)
            .map(|stream| (stream, false))
            .map_err(ScheduleFailure::Integrity)
    }
}

pub struct CompletionAdmission<'a> {
    state: &'a mut TransactionState,
    epochs: &'a mut [Option<Epoch>; EPOCHS],
    key: UeKey,
    fresh: u32,
    signalling: SignallingSet,
    old_control_rx: Option<u32>,
    control_activation: Option<u64>,
}
impl CompletionAdmission<'_> {
    /// Response publication and uplink admission commit only when the enqueue callback succeeds.
    pub fn publish_response_with<T, E>(
        &mut self,
        bearer: u8,
        message: &DLDCCHMessage,
        publish: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, ScheduleError<E>> {
        PublicationSchedule {
            key: self.key,
            fresh: self.fresh,
            signalling: self.signalling,
            state: self.state,
            epochs: self.epochs,
        }
        .publish_with(self.key, bearer, message, publish)
    }
    pub fn complete(&mut self, completion: Completion) -> Result<(), ScheduleFailure> {
        IntegritySchedule::complete_state(
            self.signalling,
            self.state,
            completion,
            self.old_control_rx,
            self.control_activation.ok_or(ScheduleFailure::Phase)?,
        )
    }
}
fn stream_error<E>(error: StreamError<E>) -> ScheduleError<E> {
    match error {
        StreamError::Consumer(error) => ScheduleError::Downstream(error),
        error => ScheduleError::Stream(error),
    }
}
fn occurrence(sequence: u8, floor: u64) -> Result<u64, ScheduleFailure> {
    if u64::from(sequence) > SEQUENCE_MASK {
        return Err(ScheduleFailure::Sequence);
    }
    floor
        .checked_add(u64::from(sequence).wrapping_sub(floor) & SEQUENCE_MASK)
        .ok_or(ScheduleFailure::Sequence)
}
fn domain_index(domain: CnDomain) -> usize {
    match domain {
        CnDomain::Cs => 0,
        CnDomain::Ps => 1,
    }
}

#[cfg(test)]
mod tests;
