// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

impl Rel99Admission {
    #[cfg(test)]
    pub fn step(
        &mut self,
        now: ChipInstant,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        common: &mut CommonTxPort,
        select: impl FnMut(FrameOpportunity) -> Tfci,
        fill: impl FnMut(&mut DownlinkFrame<'_>, &mut CommonSignalling) -> Result<(), FrameBuildError>,
    ) -> Result<AdmissionStep, AdmissionError> {
        self.step_with_policy(
            now,
            radio,
            receive,
            common,
            &mut super::traffic::ClosurePolicy { select, fill },
        )
    }
    #[cfg(test)]
    pub fn step_with_policy(
        &mut self,
        now: ChipInstant,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        common: &mut CommonTxPort,
        policy: &mut impl FramePolicy,
    ) -> Result<AdmissionStep, AdmissionError> {
        // Core-correlated setup cannot advance without current Session authority.
        self.step_authorized(now, radio, receive, common, policy, |_| false)
    }

    pub fn step_with_core<T: umts_core::Transport>(
        &mut self,
        now: ChipInstant,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        common: &mut CommonTxPort,
        policy: &mut impl FramePolicy,
        gateway: &umts_core::Gateway<T>,
    ) -> Result<AdmissionStep, AdmissionError> {
        self.step_authorized(now, radio, receive, common, policy, |authority| {
            authority.authorized(gateway)
        })
    }

    fn step_authorized(
        &mut self,
        now: ChipInstant,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        common: &mut CommonTxPort,
        policy: &mut impl FramePolicy,
        authorized: impl Fn(super::release::Authority<'_>) -> bool,
    ) -> Result<AdmissionStep, AdmissionError> {
        if now.epoch != self.epoch
            || radio.epoch() != self.epoch
            || receive.epoch() != self.epoch
            || common.next_chip().epoch != self.epoch
        {
            return Err(AdmissionError::Identity);
        }
        if self
            .previous
            .is_some_and(|previous| now.index < previous.index)
        {
            return Err(AdmissionError::Time);
        }
        self.previous = Some(now);
        let mut report = AdmissionStep {
            command_results: self.poll(),
            ..AdmissionStep::default()
        };
        receive.poll_receipts();
        for _ in 0..self.limits.visits.get().min(self.entries.len()) {
            let slot = self.cursor;
            self.cursor = (slot + 1) % self.entries.len();
            let Some(entry) = &mut self.entries[slot] else {
                continue;
            };
            report.visits += 1;
            if entry.lifecycle.phase() == AdmissionPhase::Retired {
                continue;
            }
            if entry.quarantine_error.is_some() {
                continue;
            }
            if entry.lifecycle.recovery().is_none() {
                let extracted = if entry.lifecycle.addition().is_some() {
                    super::addition::extract_failed(entry)
                } else {
                    super::release::extract_failed(entry)
                };
                match extracted {
                    Ok(Some(recovery)) => {
                        *entry
                            .lifecycle
                            .transition_mut()
                            .expect("bearer change owner") =
                            Transition::Recovery(Box::new(recovery))
                    }
                    Ok(None) => (),
                    Err(error) => {
                        entry.setup_authority = false;
                        entry.quarantine_error = Some(error);
                        report.failure = Some((entry.identity, AdmissionError::Protocol));
                        continue;
                    }
                }
            }
            if entry.lifecycle.recovery().is_some() {
                if entry
                    .lifecycle
                    .recovery()
                    .is_some_and(super::recovery::JoinedRecovery::is_quarantined)
                {
                    continue;
                }
                if let Err(error) =
                    super::recovery::step(entry, now, &mut self.port, radio, receive)
                {
                    if super::recovery::retryable(&error) {
                        report.failure = Some((entry.identity, error));
                    } else {
                        entry
                            .lifecycle
                            .recovery_mut()
                            .expect("failed recovery owner")
                            .quarantine(error);
                        report.failure = Some((entry.identity, AdmissionError::Protocol));
                    }
                }
                if entry.lifecycle.phase() == AdmissionPhase::Stopping
                    && entry
                        .lifecycle
                        .recovery()
                        .is_some_and(super::recovery::JoinedRecovery::ready_to_stop)
                {
                    if let Err(error) = stop(entry, now, &mut self.port, radio, receive) {
                        if super::recovery::retryable(&error) {
                            report.failure = Some((entry.identity, error));
                        } else {
                            entry
                                .lifecycle
                                .recovery_mut()
                                .expect("terminal recovery owner")
                                .quarantine(error);
                            report.failure = Some((entry.identity, AdmissionError::Protocol));
                        }
                    }
                    if entry.lifecycle.phase() == AdmissionPhase::Retired {
                        report.retired += 1
                    }
                }
                continue;
            }
            if entry.lifecycle.addition().is_some() {
                let revoked = entry.lifecycle.addition().is_some_and(|addition| {
                    addition.phase != super::AdditionPhase::Committed
                        && addition.assignment.as_ref().is_some_and(|assignment| {
                            !authorized(super::release::Authority::Additional(assignment.pending()))
                        })
                });
                if revoked {
                    entry
                        .lifecycle
                        .addition_mut()
                        .expect("scheduled addition")
                        .fail(AdmissionError::Cancelled);
                }
                match super::addition::step_addition(
                    entry,
                    super::transition_frames::TransitionStep {
                        now,
                        limits: self.limits,
                        port: &mut self.port,
                        radio,
                        receive,
                        common,
                        policy,
                    },
                ) {
                    Ok(frames) => report.frames += frames,
                    Err(error) => entry
                        .lifecycle
                        .addition_mut()
                        .expect("scheduled addition")
                        .fail(error),
                }
                if entry
                    .lifecycle
                    .addition()
                    .is_some_and(|addition| addition.phase != super::AdditionPhase::Committed)
                {
                    continue;
                }
            }
            if entry.lifecycle.release().is_some() {
                let revoked = entry.lifecycle.release().is_some_and(|release| {
                    release.phase != super::ScheduledReleasePhase::Committed
                        && release.core.as_ref().is_some_and(|request| {
                            !authorized(super::release::Authority::Release(request))
                        })
                });
                if revoked {
                    super::release::fail_release(entry, AdmissionError::Cancelled);
                }
                match super::release::step_release(
                    entry,
                    super::transition_frames::TransitionStep {
                        now,
                        limits: self.limits,
                        port: &mut self.port,
                        radio,
                        receive,
                        common,
                        policy,
                    },
                ) {
                    Ok(frames) => report.frames += frames,
                    Err(error) => {
                        super::release::fail_release(entry, error);
                    }
                }
                if super::release::blocks_frames(entry) {
                    continue;
                }
            }
            if !matches!(
                entry.lifecycle.phase(),
                AdmissionPhase::Active | AdmissionPhase::Stopping
            ) && entry
                .rab_assignment
                .as_ref()
                .is_some_and(|pending| !authorized(super::release::Authority::Initial(pending)))
            {
                fail(entry, AdmissionError::Cancelled);
            }
            if !self.port.available()
                && matches!(
                    entry.transmit,
                    TransmitOwnership::Preparing
                        | TransmitOwnership::Prepared
                        | TransmitOwnership::Armed
                )
            {
                entry.transmit = TransmitOwnership::AwaitingJoinedOwner;
            }
            if radio.phase(entry.identity.ue).is_none() || !self.port.available() {
                fail(entry, AdmissionError::Closed);
            }
            if entry.failure.is_none() && entry.setup_authority {
                match entry.procedure.expire(now) {
                    Ok(true) => fail(entry, AdmissionError::Time),
                    Err(error) => fail(entry, AdmissionError::Procedure(error)),
                    Ok(false) => (),
                }
            }
            if entry.lifecycle.phase() == AdmissionPhase::Stopping {
                if let Err(error) = stop(entry, now, &mut self.port, radio, receive) {
                    report.failure = Some((entry.identity, error));
                }
                if entry.lifecycle.phase() == AdmissionPhase::Retired {
                    report.retired += 1;
                }
                continue;
            }
            if let Some(error) = receive.failure(entry.identity).cloned() {
                fail(entry, AdmissionError::Receive(error));
                continue;
            }
            if entry.pending.is_some() {
                continue;
            }
            match entry.lifecycle.phase() {
                AdmissionPhase::Preparing => {
                    let cursor = common.next_chip().index.max(now.index);
                    let required_end = entry
                        .next_frame
                        .index
                        .checked_add(self.limits.prefill_frames.get() as u64 * CHIPS_PER_FRAME)
                        .ok_or(AdmissionError::Time)?;
                    let latest = cursor
                        .checked_add(self.limits.maximum_lead_frames.get() as u64 * CHIPS_PER_FRAME)
                        .ok_or(AdmissionError::Time)?;
                    if entry.next_frame.index < cursor {
                        let error =
                            AdmissionError::PreparationDeadline(Box::new(PreparationDeadline {
                                first_frame: entry.next_frame,
                                transmit_cursor: common.next_chip(),
                                observed_at: now,
                            }));
                        fail(entry, error);
                        continue;
                    }
                    if required_end > latest {
                        continue;
                    }
                    if matches!(entry.receive, ReceiveOwnership::Local(_)) {
                        let ReceiveOwnership::Local(prepared) =
                            std::mem::replace(&mut entry.receive, ReceiveOwnership::Worker)
                        else {
                            unreachable!("local receiver owner")
                        };
                        match receive.stage(prepared) {
                            Ok(_) => (),
                            Err(rejected) => {
                                entry.receive = ReceiveOwnership::Local(rejected.prepared);
                                if rejected.error != DedicatedControlError::Capacity {
                                    fail(entry, AdmissionError::Receive(rejected.error));
                                }
                            }
                        }
                    }
                    if matches!(entry.receive, ReceiveOwnership::Worker)
                        && receive.phase(entry.identity) == Some(DedicatedPhase::Prepared)
                        && matches!(entry.transmit, TransmitOwnership::Local(_))
                    {
                        let TransmitOwnership::Local(session) =
                            std::mem::replace(&mut entry.transmit, TransmitOwnership::Preparing)
                        else {
                            unreachable!("local transmitter owner")
                        };
                        match self.port.prepare(session) {
                            Ok(id) => {
                                entry.pending = Some(PendingCommand {
                                    id,
                                    kind: CommandKind::Prepare,
                                })
                            }
                            Err(session) => entry.transmit = TransmitOwnership::Local(session),
                        }
                    }
                    if matches!(entry.transmit, TransmitOwnership::Prepared)
                        && receive.phase(entry.identity) == Some(DedicatedPhase::Prepared)
                    {
                        match receive.arm(entry.identity) {
                            Ok(()) => entry.lifecycle.advance(AdmissionPhase::ArmingReceive)?,
                            Err(DedicatedControlError::Capacity) => (),
                            Err(error) => fail(entry, AdmissionError::Receive(error)),
                        }
                    }
                }
                AdmissionPhase::ArmingReceive => {
                    if receive.phase(entry.identity) == Some(DedicatedPhase::Active) {
                        entry.lifecycle.advance(AdmissionPhase::Prefilling)?;
                    }
                }
                AdmissionPhase::Prefilling
                    if entry.prepared_frames >= self.limits.prefill_frames.get() =>
                {
                    if let Ok(id) = self.port.arm(entry.identity) {
                        entry.pending = Some(PendingCommand {
                            id,
                            kind: CommandKind::Arm,
                        });
                        entry.lifecycle.advance(AdmissionPhase::ArmingTransmit)?;
                    }
                }
                AdmissionPhase::ArmingTransmit
                    if matches!(entry.transmit, TransmitOwnership::Armed) =>
                {
                    entry.lifecycle.advance(AdmissionPhase::Starting)?
                }
                AdmissionPhase::Starting => {
                    let packets_prepared = entry
                        .packet_assignment
                        .as_ref()
                        .is_none_or(|packets| packets.prepared());
                    let circuits_prepared = entry
                        .circuit_assignment
                        .as_ref()
                        .is_none_or(|circuits| circuits.prepared());
                    let bootstrap_submitted = entry
                        .first
                        .as_ref()
                        .is_some_and(TransportSubmission::submitted);
                    let startup = entry.startup.get_or_insert(StartupProgress {
                        observed_at: now,
                        packets_prepared,
                        circuits_prepared,
                        bootstrap_submitted,
                        bootstrap: None,
                        publication_attempts: 0,
                        publication_readiness: None,
                        security_delivery: None,
                        last_backpressure: None,
                    });
                    startup.observed_at = now;
                    startup.packets_prepared = packets_prepared;
                    startup.circuits_prepared = circuits_prepared;
                    startup.bootstrap_submitted = bootstrap_submitted;
                    startup.bootstrap = entry.first.as_ref().map(TransportSubmission::snapshot);
                    if packets_prepared && circuits_prepared && bootstrap_submitted {
                        startup.publication_attempts =
                            startup.publication_attempts.saturating_add(1);
                        startup.publication_readiness =
                            radio.bearer_setup_readiness(entry.identity.ue);
                        startup.security_delivery =
                            radio.security_delivery(entry.identity.ue).ok().flatten();
                        // TS 25.331 8.2.2.2 starts the new radio links before transmitting setup.
                        match radio.publish_bearer_setup_on(
                            &mut entry.procedure,
                            entry.identity,
                            now,
                        ) {
                            Ok(_) => {
                                entry.lifecycle.advance(AdmissionPhase::AwaitingComplete)?;
                                report.published += 1;
                            }
                            Err(error) if crate::signalling::scheduling::backpressure(&error) => {
                                startup.last_backpressure = Some((now, error));
                            }
                            Err(error) => fail(entry, AdmissionError::Signalling(error)),
                        }
                    }
                }
                AdmissionPhase::AwaitingCommandAck => {
                    if radio.dedicated_route(entry.identity.ue) != Some(entry.identity) {
                        match radio.select_dedicated_route(entry.identity) {
                            Ok(()) => {
                                common.cancel_ue(entry.identity.ue);
                            }
                            Err(error) => {
                                fail(entry, AdmissionError::Signalling(error));
                                continue;
                            }
                        }
                    }
                    let receipt = entry.procedure.receipt().expect("completed setup receipt");
                    match radio.bearer_setup_transmission(entry.identity.ue, receipt) {
                        Ok(SduTransmission::Acknowledged) => {
                            match radio.release_bearer_setup_receipt(entry.identity.ue, receipt) {
                                Ok(()) => entry.lifecycle.advance(AdmissionPhase::Active)?,
                                Err(error) => fail(entry, AdmissionError::Signalling(error)),
                            }
                        }
                        Ok(SduTransmission::Queued | SduTransmission::InFlight { .. }) => (),
                        Ok(SduTransmission::Discarded) => fail(entry, AdmissionError::Protocol),
                        Err(error) => fail(entry, AdmissionError::Signalling(error)),
                    }
                }
                _ => (),
            }
            if !matches!(
                entry.lifecycle.phase(),
                AdmissionPhase::Prefilling
                    | AdmissionPhase::Starting
                    | AdmissionPhase::AwaitingComplete
                    | AdmissionPhase::AwaitingCommandAck
                    | AdmissionPhase::Active
            ) || entry.failure.is_some()
                || entry.pending.is_some()
            {
                continue;
            }
            if matches!(entry.transmit, TransmitOwnership::Armed)
                && now.index >= entry.next_health.index
            {
                if let Ok(id) = self.port.observe(entry.identity) {
                    entry.pending = Some(PendingCommand {
                        id,
                        kind: CommandKind::Observe,
                    });
                    entry.next_health.index = now
                        .index
                        .checked_add(self.limits.health_interval_chips.get())
                        .ok_or(AdmissionError::Time)?;
                }
                continue;
            }
            if entry.frame.is_none() {
                let horizon = common
                    .next_chip()
                    .index
                    .max(now.index)
                    .checked_add(
                        if entry.lifecycle.phase() == AdmissionPhase::Prefilling {
                            self.limits.maximum_lead_frames.get() as u64
                        } else {
                            self.limits.prefill_frames.get() as u64
                        } * CHIPS_PER_FRAME,
                    )
                    .ok_or(AdmissionError::Time)?;
                if entry.next_frame.index >= horizon {
                    continue;
                }
                let dedicated_signalling =
                    radio.dedicated_route(entry.identity.ue) == Some(entry.identity);
                let user_bearers_ready = receive
                    .with_active_bank(entry.identity, |bank| bank.user_bearers_ready())
                    .map_err(AdmissionError::Receive)?;
                let tfci = if dedicated_signalling {
                    let opportunity = FrameOpportunity {
                        identity: entry.identity,
                        start: entry.next_frame,
                        dedicated_signalling,
                        user_bearers_ready,
                        idle: entry.idle,
                    };
                    match receive.with_active_bank(entry.identity, |bank| {
                        policy.select(opportunity, now, bank, radio)
                    }) {
                        Ok(Ok(tfci)) => tfci,
                        Ok(Err(error)) => {
                            fail(entry, AdmissionError::Frame(error));
                            continue;
                        }
                        Err(error) => {
                            fail(entry, AdmissionError::Receive(error));
                            continue;
                        }
                    }
                } else {
                    entry.idle
                };
                let built = receive.with_active_bank(entry.identity, |bank| {
                    let mut draft = bank.begin_pooled_frame(
                        entry.pool.as_mut().expect("live pool"),
                        &self.port,
                        entry.next_frame,
                        tfci,
                    )?;
                    if dedicated_signalling {
                        policy.fill(&mut draft, now, radio)?;
                    }
                    if !draft.complete() {
                        return Err(FrameBuildError::Bearer(BearerError::Transport));
                    }
                    let proof = draft.submission();
                    let input = draft.finish().ok().expect("complete draft");
                    Ok((input, proof))
                });
                match built {
                    Ok(Ok((input, proof))) => {
                        entry.frame = Some(input);
                        retain_start_submission(&mut entry.first, proof);
                    }
                    Ok(Err(FrameBuildError::Pool(
                        crate::dedicated_bearers::FramePoolError::Capacity,
                    ))) => continue,
                    Ok(Err(error)) => {
                        fail(entry, AdmissionError::Frame(error));
                        continue;
                    }
                    Err(error) => {
                        fail(entry, AdmissionError::Receive(error));
                        continue;
                    }
                }
            }
            if let Some(input) = entry.frame.take() {
                match self.port.frame(input) {
                    Ok(id) => {
                        entry.pending = Some(PendingCommand {
                            id,
                            kind: CommandKind::Frame,
                        });
                        entry.next_frame.index = entry
                            .next_frame
                            .index
                            .checked_add(CHIPS_PER_FRAME)
                            .ok_or(AdmissionError::Time)?;
                        report.frames += 1;
                    }
                    Err(input) => entry.frame = Some(input),
                }
            }
        }
        Ok(report)
    }
}

fn retain_start_submission(
    first: &mut Option<TransportSubmission>,
    candidate: TransportSubmission,
) {
    if first.as_ref().is_none_or(|first| !first.viable()) {
        *first = Some(candidate);
    }
}

#[cfg(test)]
mod tests;

pub(super) fn fail(entry: &mut Entry, error: AdmissionError) {
    if entry.failure.is_none() {
        entry.failed_phase = Some(entry.lifecycle.phase());
        entry.failure = Some(error);
    }
    entry.lifecycle.stop();
}
fn stop(
    entry: &mut Entry,
    now: ChipInstant,
    port: &mut DedicatedTransmitPort,
    radio: &mut CommonSignalling,
    receive: &mut DedicatedController,
) -> Result<(), AdmissionError> {
    if !entry.recovery_marked {
        if entry.setup_authority {
            entry
                .procedure
                .local_failure(now)
                .map_err(AdmissionError::Procedure)?;
        }
        if entry.procedure.receipt().is_some() && radio.phase(entry.identity.ue).is_some() {
            if radio.dedicated_route(entry.identity.ue) == Some(entry.identity) {
                radio
                    .require_dedicated_recovery(entry.identity)
                    .map_err(AdmissionError::Signalling)?;
            } else {
                radio
                    .require_bearer_setup_recovery(&entry.procedure)
                    .map_err(AdmissionError::Signalling)?;
            }
        }
        entry.recovery_marked = true;
    }
    entry.first = None;
    if let Some(mut input) = entry.frame.take() {
        if let Some(emission) = &mut input.emission {
            emission.fail();
        }
        if let Err(input) = entry.pool.as_mut().expect("live pool").recycle(input) {
            entry.frame = Some(input);
            return Err(AdmissionError::Protocol);
        }
    }
    if entry.pending.is_some() {
        return Ok(());
    }
    if matches!(entry.receive, ReceiveOwnership::Worker) {
        match receive.phase(entry.identity) {
            Some(
                DedicatedPhase::Active
                | DedicatedPhase::Prepared
                | DedicatedPhase::ArmFailed
                | DedicatedPhase::Faulted,
            ) => match receive.remove(entry.identity) {
                Ok(()) | Err(DedicatedControlError::Capacity) => (),
                Err(error) => return Err(AdmissionError::Receive(error)),
            },
            Some(DedicatedPhase::Rejected | DedicatedPhase::Retired | DedicatedPhase::Closed) => {
                if let Ok(retired) = receive.take_retired(entry.identity) {
                    entry.receive = ReceiveOwnership::Returned(retired);
                }
            }
            _ => (),
        }
    }
    if matches!(
        entry.transmit,
        TransmitOwnership::Preparing | TransmitOwnership::Prepared | TransmitOwnership::Armed
    ) {
        if let Ok(id) = port.remove(entry.identity) {
            entry.pending = Some(PendingCommand {
                id,
                kind: CommandKind::Remove,
            });
        }
        return Ok(());
    }
    if matches!(entry.receive, ReceiveOwnership::Worker)
        || matches!(entry.transmit, TransmitOwnership::AwaitingJoinedOwner)
    {
        return Ok(());
    }
    if let TransmitOwnership::Returned(session) = &mut entry.transmit {
        entry
            .pool
            .as_mut()
            .expect("live pool")
            .reclaim_retired(session)
            .map_err(|error| AdmissionError::Frame(FrameBuildError::Pool(error)))?;
    }
    let pool = entry.pool.take().expect("live pool");
    match radio.retire_frame_pool(pool) {
        Ok(()) => entry.lifecycle.retire(),
        Err(rejected) => {
            entry.pool = Some(*rejected.pool);
            if rejected.error != crate::dedicated_bearers::FramePoolError::Pending {
                return Err(AdmissionError::Frame(FrameBuildError::Pool(rejected.error)));
            }
        }
    }
    Ok(())
}
