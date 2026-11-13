// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_rx::{
        DedicatedReceivePort, HsupaAttachment, ReceiveCommandResult, ReceiveCommandTicket,
        ReceiveIdentity,
    },
    signalling::{
        AcceptedActivation, CommonSignalling, ControlSecurity, ReceivedControl, SignallingError,
    },
};
use anyhow::{Result, anyhow, ensure};
use umts_rrc::{
    dedicated::{ReconfigurationPhase, ReconfigurationResponse},
    hsupa::{DchRestoration, RestorationProcedure},
};
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestorationPhase {
    SchedulingReceive,
    Publishing,
    AwaitingComplete,
    AwaitingReceive,
    Committing,
    ReleasingReceipt,
    Detaching,
    Detached,
    RecoveryRequired,
}

pub struct Restoration {
    identity: ReceiveIdentity,
    procedure: Box<RestorationProcedure>,
    phase: RestorationPhase,
    receive: Option<ReceiveCommandTicket>,
    accepted: Option<AcceptedActivation<DchRestoration>>,
    applied: Option<ChipInstant>,
    detached: Option<HsupaAttachment>,
    unexpected: Option<Box<ReceiveCommandResult>>,
    deadline: ChipInstant,
    receive_boundary: ChipInstant,
}

impl Restoration {
    pub fn new(
        identity: ReceiveIdentity,
        procedure: Box<RestorationProcedure>,
        uplink: crate::dedicated_rx::FrameAnchor,
        deadline: ChipInstant,
    ) -> Result<Self, Box<RestorationProcedure>> {
        if procedure.key() != identity.ue
            || procedure.phase() != ReconfigurationPhase::Prepared
            || deadline.epoch != identity.ue.epoch
            || deadline.index <= procedure.construction().activation().time().index
        {
            return Err(procedure);
        }
        let Ok(receive_boundary) =
            super::timing::receive_activation(procedure.construction().activation(), uplink)
        else {
            return Err(procedure);
        };
        if deadline.index <= receive_boundary.index {
            return Err(procedure);
        }
        Ok(Self {
            identity,
            procedure,
            phase: RestorationPhase::SchedulingReceive,
            receive: None,
            accepted: None,
            applied: None,
            detached: None,
            unexpected: None,
            deadline,
            receive_boundary,
        })
    }

    pub fn phase(&self) -> RestorationPhase {
        self.phase
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }

    pub fn receive_boundary(&self) -> ChipInstant {
        self.receive_boundary
    }

    pub fn procedure(&self) -> &RestorationProcedure {
        &self.procedure
    }

    pub fn response(
        &mut self,
        received: &ReceivedControl<'_>,
        security: &mut ControlSecurity<'_>,
    ) -> Result<ReconfigurationResponse, SignallingError> {
        if let Some(accepted) = &self.accepted {
            received.hspa_activation_duplicate(&mut self.procedure, accepted)?;
            return Ok(ReconfigurationResponse::Duplicate);
        }
        if self.phase != RestorationPhase::AwaitingComplete {
            return Err(SignallingError::Phase);
        }
        let (response, accepted) = received.hspa_activation_response_with(
            &mut self.procedure,
            security,
            |counters, _| {
                if counters.transparent_cfn().is_some() {
                    return Err(SignallingError::Mapping);
                }
                Ok(())
            },
        )?;
        if let Some((accepted, ())) = accepted {
            self.accepted = Some(accepted);
            self.phase = RestorationPhase::AwaitingReceive;
        }
        Ok(response)
    }

    pub fn take_detached(&mut self) -> Option<HsupaAttachment> {
        if self.phase == RestorationPhase::Detached {
            self.detached.take()
        } else {
            None
        }
    }

    pub(super) fn reconcile_for_ue_release(&mut self) -> bool {
        if self.receive.is_some() {
            let Some(result) = self.poll_receive() else {
                return false;
            };
            if let ReceiveCommandResult::RestoredHsupaDetached {
                identity,
                receiver,
                restored,
            } = result
            {
                if identity == self.identity && self.detached.is_none() {
                    self.detached = Some(receiver);
                } else {
                    self.unexpected = Some(Box::new(ReceiveCommandResult::RestoredHsupaDetached {
                        identity,
                        receiver,
                        restored,
                    }));
                    return false;
                }
            } else {
                self.unexpected = Some(Box::new(result));
            }
        }
        match self.unexpected.as_deref() {
            None | Some(ReceiveCommandResult::Closed) => true,
            Some(
                ReceiveCommandResult::DchRestorationScheduled { identity, .. }
                | ReceiveCommandResult::RestoredHsupaDetachRejected { identity, .. }
                | ReceiveCommandResult::Observed { identity, .. },
            ) => *identity == self.identity,
            _ => false,
        }
    }

    pub(super) fn owns_detached(&self) -> bool {
        self.detached.is_some()
    }

    pub(super) fn take_for_ue_release(&mut self) -> Option<HsupaAttachment> {
        self.detached.take()
    }

    pub fn service(
        &mut self,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedReceivePort,
        now: ChipInstant,
        ingress_drained: bool,
    ) -> Result<()> {
        let result = self.step(radio, receive, now, ingress_drained);
        if result.is_err() {
            self.phase = RestorationPhase::RecoveryRequired;
            radio
                .require_dedicated_recovery(self.identity)
                .map_err(|error| anyhow!("restoration recovery marking: {error:?}"))?;
        }
        result
    }

    fn step(
        &mut self,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedReceivePort,
        now: ChipInstant,
        ingress_drained: bool,
    ) -> Result<()> {
        ensure!(
            now.epoch == self.identity.ue.epoch,
            "restoration uses another radio epoch"
        );
        ensure!(
            self.phase != RestorationPhase::RecoveryRequired,
            "restoration requires recovery"
        );
        if self.phase == RestorationPhase::Detached {
            return Ok(());
        }
        ensure!(
            now.index < self.deadline.index,
            "restoration drain deadline expired in phase {:?} (receive pending {}, procedure {:?}, ingress drained {ingress_drained}, unexpected {:?})",
            self.phase,
            self.receive.is_some(),
            self.procedure.phase(),
            self.unexpected.as_deref().map(std::mem::discriminant)
        );
        self.procedure
            .expire(now)
            .map_err(|error| anyhow!("restoration expiry: {error:?}"))?;
        ensure!(
            !matches!(
                self.procedure.phase(),
                ReconfigurationPhase::FailureReported
                    | ReconfigurationPhase::RecoveryRequired
                    | ReconfigurationPhase::AbortedBeforePublication
            ),
            "restoration procedure failed"
        );
        let boundary = self.receive_boundary;
        match self.phase {
            RestorationPhase::SchedulingReceive => {
                if self.receive.is_none() {
                    self.receive = receive.restore_dch_at(self.identity, boundary).ok();
                    return Ok(());
                }
                let Some(result) = self.poll_receive() else {
                    return Ok(());
                };
                match result {
                    ReceiveCommandResult::DchRestorationScheduled {
                        identity,
                        activation,
                        result: Ok(()),
                    } if identity == self.identity && activation == boundary => {
                        self.phase = RestorationPhase::Publishing
                    }
                    other => return self.reject_receive(other),
                }
            }
            RestorationPhase::Publishing => {
                radio
                    .publish_hspa_activation_on(&mut self.procedure, self.identity, now)
                    .map_err(|error| anyhow!("restoration publication: {error:?}"))?;
                self.phase = RestorationPhase::AwaitingComplete;
            }
            RestorationPhase::AwaitingComplete => (),
            RestorationPhase::AwaitingReceive => {
                if self.receive.is_none() {
                    self.receive = receive.observe(self.identity).ok();
                    return Ok(());
                }
                let Some(result) = self.poll_receive() else {
                    return Ok(());
                };
                match result {
                    ReceiveCommandResult::Observed {
                        identity,
                        status: Some(status),
                    } if identity == self.identity => {
                        if let Some(applied) = status.restored_dch_at {
                            ensure!(
                                applied.epoch == boundary.epoch
                                    && applied.index >= boundary.index
                                    && (applied.index - boundary.index)
                                        .is_multiple_of(CHIPS_PER_FRAME),
                                "receiver restoration boundary mismatch"
                            );
                            self.applied = Some(applied);
                            if !status.hsupa_pending && ingress_drained {
                                self.phase = RestorationPhase::Committing;
                            }
                        }
                    }
                    other => return self.reject_receive(other),
                }
            }
            RestorationPhase::Committing => {
                let accepted = self.accepted.as_ref().expect("accepted restoration");
                radio
                    .commit_hspa_activation_with(accepted, |_| Ok::<_, SignallingError>(()))
                    .map_err(|error| anyhow!("restoration commit: {error:?}"))?;
                self.phase = RestorationPhase::ReleasingReceipt;
            }
            RestorationPhase::ReleasingReceipt => {
                match radio.release_hspa_activation_receipt(
                    self.accepted.as_ref().expect("committed restoration"),
                ) {
                    Ok(()) => self.phase = RestorationPhase::Detaching,
                    Err(SignallingError::Capacity) => (),
                    Err(error) => return Err(anyhow!("restoration receipt: {error:?}")),
                }
            }
            RestorationPhase::Detaching => {
                let applied = self.applied.expect("observed receiver restoration");
                if self.receive.is_none() {
                    self.receive = receive.detach_restored_hsupa(self.identity, applied).ok();
                    return Ok(());
                }
                let Some(result) = self.poll_receive() else {
                    return Ok(());
                };
                match result {
                    ReceiveCommandResult::RestoredHsupaDetached {
                        identity,
                        restored,
                        receiver,
                    } if identity == self.identity && restored == applied => {
                        self.detached = Some(receiver);
                        self.phase = RestorationPhase::Detached;
                    }
                    other => return self.reject_receive(other),
                }
            }
            RestorationPhase::Detached | RestorationPhase::RecoveryRequired => unreachable!(),
        }
        Ok(())
    }

    fn poll_receive(&mut self) -> Option<ReceiveCommandResult> {
        let result = self.receive.as_mut()?.try_result()?;
        self.receive = None;
        Some(result)
    }

    fn reject_receive(&mut self, result: ReceiveCommandResult) -> Result<()> {
        self.unexpected = Some(Box::new(result));
        Err(anyhow!("unexpected restoration receive result retained"))
    }
}
