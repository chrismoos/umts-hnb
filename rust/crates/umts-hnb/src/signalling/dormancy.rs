// SPDX-License-Identifier: AGPL-3.0-or-later

//! CELL_PCH and URA_PCH for a UE that keeps its packet bearers (TS 25.331 7.2.2.1).

use super::{CommonSignalling, Context, SignallingError, reports, transactions};
use umts_rrc::{
    mobility::{ConfirmTarget, ParkedState, UpdateCause, park_reconfiguration_message},
    nas::SignallingBearer,
};
use umts_rrc_schema::class_definitions::ULDCCHMessageType;
use umts_security::ciphering::Scope;
use umts_types::{
    UeKey,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

// TS 25.331 8.2.2.4: the UE leaves CELL_DCH only once RLC has confirmed its response. The
// dedicated channel stays up until the status report that acknowledges it has been on the air,
// plus this margin for the UE to act on it.
const ACKNOWLEDGEMENT_GUARD_FRAMES: u64 = 10;
// Longest wait for that status report before the channel is released anyway.
const ACKNOWLEDGEMENT_LIMIT_FRAMES: u64 = 60;
const FIRST_USER_BEARER: u8 = 5;
const LAST_BEARER: u8 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkProgress {
    Requested,
    Acknowledged,
    Failed,
}

#[derive(Clone, Copy)]
struct Request {
    transaction: u8,
    state: ParkedState,
    completed: Option<umts_rlc::acknowledged::ReceiveCoverage>,
    completed_at: Option<ChipInstant>,
    failed: bool,
}

#[derive(Default)]
pub(super) struct Dormancy {
    pub(super) parked: Option<ParkedState>,
    request: Option<Request>,
    /// A confirm that gave the UE a C-RNTI is unanswered. Its repeats carry the C-RNTI again.
    pub(super) confirm_unanswered: bool,
    pub(super) wake: bool,
    pub(super) drx_coefficient: u8,
    pub(super) updates: u64,
    /// A page is outstanding, so the next cell update is its answer whatever its cause.
    pub(super) paged: bool,
    pub(super) resuming: bool,
    pub(super) unanswered: bool,
    pub(super) resumable: bool,
    pub(super) release_paged: bool,
    /// The UE reported the end of its packet data session (TS 25.331 8.1.14.2), so it is
    /// parked without waiting for the inactivity time.
    pub(super) session_end: bool,
    /// A cell update whose confirm waits until the dedicated channel of the UE is prepared.
    /// TS 25.331 8.3.1.3: the UE waits T302 for the confirm and, having no C-RNTI, sends no
    /// data meanwhile.
    pub(super) deferred: Option<DeferredUpdate>,
}

pub(super) struct DeferredUpdate {
    pub(super) bytes: Vec<u8>,
    pub(super) bit_length: usize,
    pub(super) received: ChipInstant,
}

impl Dormancy {
    /// Call after integrity admission of an uplink DCCH message.
    pub(super) fn admit(
        &mut self,
        message: &ULDCCHMessageType,
        coverage: Option<umts_rlc::acknowledged::ReceiveCoverage>,
        now: ChipInstant,
    ) -> bool {
        match message {
            ULDCCHMessageType::physicalChannelReconfigurationComplete(complete) => {
                match self.request.as_mut() {
                    Some(request)
                        if request.transaction == complete.rrc_transaction_identifier.0 =>
                    {
                        request.completed_at.get_or_insert(now);
                        if let Some(coverage) = coverage {
                            request.completed.get_or_insert(coverage);
                        }
                        true
                    }
                    _ => false,
                }
            }
            ULDCCHMessageType::physicalChannelReconfigurationFailure(failure) => {
                match self.request.as_mut() {
                    Some(request)
                        if failure
                            .rrc_transaction_identifier
                            .as_ref()
                            .is_some_and(|value| value.0 == request.transaction) =>
                    {
                        request.failed = true;
                        true
                    }
                    _ => false,
                }
            }
            ULDCCHMessageType::utranMobilityInformationConfirm(_) if self.confirm_unanswered => {
                self.confirm_unanswered = false;
                true
            }
            _ => false,
        }
    }

    pub(super) fn defers(&self, cause: UpdateCause) -> bool {
        self.parked.is_some()
            && self.resumable
            && (self.paged
                || matches!(
                    cause,
                    UpdateCause::UplinkData | UpdateCause::PagingResponse | UpdateCause::Extended
                ))
    }

    pub(super) fn confirm_target(
        &mut self,
        cause: UpdateCause,
        crnti: u16,
        paged: bool,
    ) -> Option<ConfirmTarget> {
        let Some(state) = self.parked else {
            // TS 25.331 8.3.1.6: a UE whose earlier confirm was lost repeats the cell update.
            return self.confirm_unanswered.then_some(if self.resumable {
                ConfirmTarget::Wake { crnti }
            } else {
                ConfirmTarget::Fach {
                    new_crnti: Some(crnti),
                }
            });
        };
        self.updates = self.updates.saturating_add(1);
        // TS 25.331 8.3.1.2 ranks "uplink data transmission" above "paging response", and a
        // paged UE may answer with any cause.
        let leaving = paged
            || matches!(
                cause,
                UpdateCause::UplinkData | UpdateCause::PagingResponse | UpdateCause::Extended
            );
        Some(if leaving {
            self.parked = None;
            self.confirm_unanswered = true;
            self.wake = self.resumable;
            if self.resumable {
                ConfirmTarget::Wake { crnti }
            } else {
                ConfirmTarget::Fach {
                    new_crnti: Some(crnti),
                }
            }
        } else {
            ConfirmTarget::Parked {
                state,
                drx_coefficient: self.drx_coefficient,
            }
        })
    }
}

impl CommonSignalling {
    pub fn parked(&self, key: UeKey) -> Option<ParkedState> {
        self.registry.get(key)?.dormancy.parked
    }

    pub fn parked_updates(&self, key: UeKey) -> u64 {
        self.registry
            .get(key)
            .map_or(0, |context| context.dormancy.updates)
    }

    pub fn resumable(&self, key: UeKey) -> bool {
        self.registry
            .get(key)
            .is_some_and(|context| context.dormancy.resumable)
    }

    /// Sends the reconfiguration that moves a UE in CELL_DCH to a paging state. Returns false
    /// while another procedure or queued signalling prevents it.
    pub fn request_park(
        &mut self,
        key: UeKey,
        state: ParkedState,
        drx_coefficient: u8,
    ) -> Result<bool, SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        // A UE in CELL_FACH needs its C-RNTI, which a returning UE has once it answered.
        if context.dormancy.request.is_some()
            || context.dormancy.parked.is_some()
            || context.dormancy.confirm_unanswered
            || context.dormancy.deferred.is_some()
            || context.dormancy.resuming
            || !reports::ready_to_publish(context, key, SignallingBearer::ControlAm)?
        {
            return Ok(false);
        }
        let kind = transactions::MessageType::PhysicalChannelReconfiguration;
        let transaction = context.transactions.next(kind);
        let message = park_reconfiguration_message(transaction, state, drx_coefficient)
            .map_err(SignallingError::Mobility)?;
        reports::publish(context, key, SignallingBearer::ControlAm, &message)?;
        context.transactions.admitted(kind, transaction);
        context.dormancy.drx_coefficient = drx_coefficient;
        context.dormancy.request = Some(Request {
            transaction: transaction.get(),
            state,
            completed: None,
            completed_at: None,
            failed: false,
        });
        tracing::info!(
            "Park requested: slot={} generation={} state={state:?} drx_coefficient={drx_coefficient} transaction={}",
            key.slot,
            key.generation,
            transaction.get(),
        );
        Ok(true)
    }

    pub fn park_progress(&self, key: UeKey, now: ChipInstant) -> Option<ParkProgress> {
        let context = self.registry.get(key)?;
        let request = context.dormancy.request?;
        Some(if request.failed {
            ParkProgress::Failed
        } else if request.completed.is_some_and(|completed| {
            context
                .control
                .status_submission(key)
                .ok()
                .flatten()
                .is_some_and(|status| {
                    status.coverage.covers(completed)
                        && status.transport.submitted()
                        && now.epoch == status.transport.air_end().epoch
                        && now.index
                            >= status
                                .transport
                                .air_end()
                                .index
                                .saturating_add(ACKNOWLEDGEMENT_GUARD_FRAMES * CHIPS_PER_FRAME)
                })
        }) || request.completed_at.is_some_and(|at| {
            now.epoch == at.epoch
                && now.index
                    >= at
                        .index
                        .saturating_add(ACKNOWLEDGEMENT_LIMIT_FRAMES * CHIPS_PER_FRAME)
        }) {
            ParkProgress::Acknowledged
        } else {
            ParkProgress::Requested
        })
    }

    pub fn take_session_end(&mut self, key: UeKey) -> bool {
        self.registry
            .get_mut(key)
            .is_some_and(|context| std::mem::take(&mut context.dormancy.session_end))
    }

    pub fn abandon_park(&mut self, key: UeKey) {
        if let Some(context) = self.registry.get_mut(key) {
            context.dormancy.request = None;
        }
    }

    /// Records that the UE is in a paging state and has no dedicated channel. Returns the
    /// dedicated session to stop, if it had one. The user bearers' cipher contexts are dropped
    /// because TS 25.331 8.3.1.6 restarts their RLC entities when the UE returns.
    pub fn enter_parked(
        &mut self,
        key: UeKey,
        user_bearers: &[u8],
    ) -> Result<Option<crate::dedicated_rx::ReceiveIdentity>, SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let request = context
            .dormancy
            .request
            .filter(|request| request.completed_at.is_some() && !request.failed)
            .ok_or(SignallingError::Phase)?;
        let identity = context.dedicated_route;
        // Every user bearer context goes, including one a just-released bearer still holds.
        let registered: Vec<u8> = (FIRST_USER_BEARER..=LAST_BEARER)
            .filter(|bearer| {
                context
                    .protection
                    .ciphers
                    .layout(key, *bearer)
                    .is_ok_and(|(scope, _)| matches!(scope, Scope::User(_)))
            })
            .collect();
        if !registered.is_empty() {
            context
                .protection
                .ciphers
                .unregister_batch_with(key, &registered, || Ok::<_, SignallingError>(()))
                .map_err(|_| SignallingError::ConfigurationFailure)?;
        }
        context
            .protection
            .ciphers
            .forget_transparent_domains(key)
            .map_err(|_| SignallingError::ConfigurationFailure)?;
        context
            .um
            .address_by_urnti(true)
            .map_err(SignallingError::Um)?;
        park_context(context, request.state);
        context.dormancy.resumable = !user_bearers.is_empty();

        tracing::info!(
            "UE parked: slot={} generation={} state={:?}",
            key.slot,
            key.generation,
            request.state
        );
        Ok(identity)
    }

    pub fn wake_ready(&self, key: UeKey) -> bool {
        self.registry
            .get(key)
            .is_some_and(|context| context.dormancy.wake && context.dedicated_route.is_none())
    }

    pub fn wake_confirm_pending(&self, key: UeKey) -> bool {
        self.registry.get(key).is_some_and(|context| {
            context.dormancy.deferred.is_some() || context.dormancy.confirm_unanswered
        })
    }

    /// The age in chips of a deferred cell update, to answer it before T302 expires.
    pub fn deferred_confirm_age(&self, key: UeKey, now: ChipInstant) -> Option<u64> {
        let deferred = self.registry.get(key)?.dormancy.deferred.as_ref()?;
        (now.epoch == deferred.received.epoch)
            .then(|| now.index.saturating_sub(deferred.received.index))
    }

    pub fn abandon_resume(&mut self, key: UeKey) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        context.dormancy.wake = false;
        context.dormancy.resumable = false;
        self.send_deferred_confirm(key).map(|_| ())
    }

    pub fn wake_started(&mut self, key: UeKey) {
        if let Some(context) = self.registry.get_mut(key) {
            context.dormancy.wake = false;
            context.dormancy.resuming = true;
        }
    }

    pub fn resuming(&self, key: UeKey) -> bool {
        self.registry
            .get(key)
            .is_some_and(|context| context.dormancy.resuming)
    }
}

fn park_context(context: &mut Context, state: ParkedState) {
    context.dormancy.request = None;
    context.dormancy.parked = Some(state);
    context.dormancy.wake = false;
    context.dormancy.confirm_unanswered = false;
    // A page planned for a later occasion may have been sent after its answer arrived.
    context.dormancy.paged = false;
    context.dormancy.unanswered = false;
    context.dedicated_route = None;
    context.reports = Default::default();
}

pub(super) fn answered(context: &mut Context) {
    if !context.dormancy.confirm_unanswered {
        // The UE has its C-RNTI again (TS 25.331 8.3.1.7).
        let _ = context.um.address_by_urnti(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_rrc_schema::{information_elements as ie, pdu_definitions as pdu};

    const CRNTI: u16 = 7;

    fn parked(resumable: bool) -> Dormancy {
        Dormancy {
            parked: Some(ParkedState::CellPch),
            resumable,
            drx_coefficient: 6,
            ..Dormancy::default()
        }
    }

    #[test]
    fn periodic_and_reselection_updates_keep_the_ue_parked_without_a_response() {
        for cause in [
            UpdateCause::Periodic,
            UpdateCause::CellReselection,
            UpdateCause::ReenteredService,
        ] {
            let mut dormancy = parked(true);
            assert!(!dormancy.defers(cause));
            assert_eq!(
                dormancy.confirm_target(cause, CRNTI, false),
                Some(ConfirmTarget::Parked {
                    state: ParkedState::CellPch,
                    drx_coefficient: 6,
                })
            );
            assert!(dormancy.parked.is_some() && !dormancy.confirm_unanswered);
        }
    }

    #[test]
    fn data_and_page_answers_leave_the_paging_state_with_a_crnti() {
        for (cause, paged) in [
            (UpdateCause::UplinkData, false),
            (UpdateCause::PagingResponse, false),
            // TS 25.331 8.3.1.2 lets a paged UE answer with another cause.
            (UpdateCause::CellReselection, true),
        ] {
            let mut dormancy = parked(true);
            dormancy.paged = paged;
            assert!(dormancy.defers(cause));
            assert_eq!(
                dormancy.confirm_target(cause, CRNTI, paged),
                Some(ConfirmTarget::Wake { crnti: CRNTI })
            );
            assert!(dormancy.parked.is_none() && dormancy.confirm_unanswered && dormancy.wake);
            assert_eq!(
                dormancy.confirm_target(cause, CRNTI, false),
                Some(ConfirmTarget::Wake { crnti: CRNTI })
            );
        }
    }

    #[test]
    fn signalling_only_ue_returns_to_cell_fach_and_is_never_deferred() {
        let mut dormancy = parked(false);
        assert!(!dormancy.defers(UpdateCause::UplinkData));
        assert_eq!(
            dormancy.confirm_target(UpdateCause::UplinkData, CRNTI, false),
            Some(ConfirmTarget::Fach {
                new_crnti: Some(CRNTI)
            })
        );
        assert!(!dormancy.wake);
    }

    #[test]
    fn ue_that_was_never_parked_is_left_to_the_common_channel_rules() {
        let mut dormancy = Dormancy::default();
        assert_eq!(
            dormancy.confirm_target(UpdateCause::Periodic, CRNTI, false),
            None
        );
    }

    #[test]
    fn mobility_confirm_ends_the_unanswered_confirm_once() {
        let mut dormancy = parked(true);
        dormancy.confirm_target(UpdateCause::UplinkData, CRNTI, false);
        let confirm = ULDCCHMessageType::utranMobilityInformationConfirm(
            pdu::UTRANMobilityInformationConfirm::new(
                ie::RRCTransactionIdentifier(0),
                None,
                None,
                None,
                None,
                None,
            ),
        );
        let now = crate::test_support::at(0);
        assert!(dormancy.admit(&confirm, None, now));
        assert!(!dormancy.confirm_unanswered);
        assert!(!dormancy.admit(&confirm, None, now));
    }
}
