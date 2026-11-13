// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    common_tx::{PlanEvent, PlanStage},
    signalling::{
        CommonIngressStep, CommonIngressWork, CommonMessageKind, CommonSignalling, FachTarget,
        FachWork, SignallingError, SubscriberHint,
    },
};
use umts_rrc::establishment::EstablishmentPhase;
use umts_types::{UeKey, time::ChipInstant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    Origin,
    Message,
    Codec,
    Identification,
    Configuration,
    Capacity,
    Common,
    Phase,
    Nas(umts_rrc::nas::NasError),
    Security,
    Integrity(umts_rrc::integrity::ScheduleFailure),
    Other,
}

impl From<&SignallingError> for FailureClass {
    fn from(error: &SignallingError) -> Self {
        match error {
            SignallingError::Origin => Self::Origin,
            SignallingError::Message | SignallingError::UnsupportedControl(_) => Self::Message,
            SignallingError::Codec(_) => Self::Codec,
            SignallingError::Identification(_) => Self::Identification,
            SignallingError::ConfigurationFailure => Self::Configuration,
            SignallingError::Capacity => Self::Capacity,
            SignallingError::Common(_) => Self::Common,
            SignallingError::Phase => Self::Phase,
            SignallingError::Nas(error) => Self::Nas(*error),
            SignallingError::SecurityProcedure(_) => Self::Security,
            SignallingError::Integrity(error) => Self::Integrity(*error),
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreFailure {
    Command(umts_core::Error),
    Owner(umts_core::Error),
    Transport,
    Radio(FailureClass),
    Consumer,
    Security(FailureClass),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionRequestEvidence {
    pub start: ChipInstant,
    pub key: UeKey,
    pub subscriber: Option<SubscriberHint>,
    pub phase: Option<EstablishmentPhase>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonRejectionEvidence {
    pub start: ChipInstant,
    pub message: CommonMessageKind,
    pub error: FailureClass,
    pub detail: SignallingError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellUpdateReleaseEvidence {
    pub start: ChipInstant,
    pub key: UeKey,
    pub update: umts_rrc::mobility::CellUpdate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlFailureStage {
    Collection,
    Dedicated,
    Security,
    CommonTransmit,
    Timers,
    Retirement,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlFailureEvidence {
    pub stage: ControlFailureStage,
    pub error: SignallingError,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessDiagnostics {
    pub paging: crate::signalling::PagingCounters,
    pub revision: u64,
    pub last_request: Option<ConnectionRequestEvidence>,
    pub last_cell_update_release: Option<CellUpdateReleaseEvidence>,
    pub last_cell_update_confirm: Option<CellUpdateReleaseEvidence>,
    pub cell_updates_confirmed: u64,
    pub connection_rejections: u64,
    pub last_rejection: Option<CommonRejectionEvidence>,
    pub unknown_context_releases_queued: u64,
    pub last_unknown_context_urnti: Option<u32>,
    pub protocol_error_rejects_queued: u64,
    pub confirmed_core_identities: u64,
    pub conflicting_core_identities: u64,
    pub last_deferred_core: Option<umts_iuh::session::EventKind>,
    pub last_core_failure: Option<CoreFailure>,
    pub last_control_failure: Option<ControlFailureEvidence>,
    pub fach_planned: u64,
    pub fach_blocked: u64,
    pub last_fach_target: Option<FachTarget>,
    pub last_fach_error: Option<FailureClass>,
    pub plans_committed: u64,
    pub plans_rejected: u64,
    pub last_plan: Option<PlanEvent>,
}

impl AccessDiagnostics {
    pub(super) fn control_failure(&mut self, stage: ControlFailureStage, error: &SignallingError) {
        if self
            .last_control_failure
            .as_ref()
            .is_none_or(|old| old.stage != stage || &old.error != error)
        {
            self.last_control_failure = Some(ControlFailureEvidence {
                stage,
                error: error.clone(),
            });
            self.revision = self.revision.saturating_add(1);
        }
    }
    pub(super) fn core_failure<T, U>(&mut self, error: &crate::core_link::Error<T, U>) {
        use crate::core_link::{DispatchError, Error};
        use umts_core::DriveError;
        let failure = match error {
            Error::Command(error) => CoreFailure::Command(error.clone()),
            Error::Drive(DriveError::Owner(error)) => CoreFailure::Owner(error.clone()),
            Error::Drive(DriveError::Transport(_)) => CoreFailure::Transport,
            Error::Drive(DriveError::Upper(DispatchError::Radio(error))) => {
                CoreFailure::Radio(error.into())
            }
            Error::Drive(DriveError::Upper(DispatchError::Other(_))) => CoreFailure::Consumer,
            Error::Security(error) => CoreFailure::Security(error.into()),
        };
        if self.last_core_failure.as_ref() != Some(&failure) {
            self.last_core_failure = Some(failure);
            self.revision = self.revision.saturating_add(1);
        }
    }
    #[cfg(test)]
    pub(super) fn deferred_core(&mut self, kind: umts_iuh::session::EventKind) {
        if self.last_deferred_core != Some(kind) {
            self.last_deferred_core = Some(kind);
            self.revision = self.revision.saturating_add(1);
        }
    }

    pub(super) fn identities(&mut self, counters: crate::signalling::SignallingCounters) {
        if self.paging != counters.paging {
            self.paging = counters.paging;
            self.revision = self.revision.saturating_add(1);
        }
        if self.confirmed_core_identities != counters.confirmed_core_identities
            || self.conflicting_core_identities != counters.conflicting_core_identities
        {
            self.confirmed_core_identities = counters.confirmed_core_identities;
            self.conflicting_core_identities = counters.conflicting_core_identities;
            self.revision = self.revision.saturating_add(1);
        }
    }

    pub(super) fn common(&mut self, step: &CommonIngressStep, radio: &CommonSignalling) {
        match &step.work {
            CommonIngressWork::ConnectionRequested(key) => {
                self.last_request = Some(ConnectionRequestEvidence {
                    start: step.transport.start,
                    key: *key,
                    subscriber: radio.subscriber_hint(*key),
                    phase: radio.phase(*key),
                });
            }
            CommonIngressWork::Rejected(error) => {
                self.last_rejection = Some(CommonRejectionEvidence {
                    start: step.transport.start,
                    message: step.message,
                    error: error.into(),
                    detail: error.clone(),
                });
            }
            CommonIngressWork::ConnectionReleased { key, update } => {
                self.last_cell_update_release = Some(CellUpdateReleaseEvidence {
                    start: step.transport.start,
                    key: *key,
                    update: *update,
                });
            }
            CommonIngressWork::UpdateConfirmed { key, update } => {
                self.cell_updates_confirmed = self.cell_updates_confirmed.saturating_add(1);
                self.last_cell_update_confirm = Some(CellUpdateReleaseEvidence {
                    start: step.transport.start,
                    key: *key,
                    update: *update,
                });
            }
            CommonIngressWork::ConnectionRejected { .. } => {
                self.connection_rejections = self.connection_rejections.saturating_add(1);
            }
            CommonIngressWork::UnknownContextReleaseQueued { urnti } => {
                self.unknown_context_releases_queued =
                    self.unknown_context_releases_queued.saturating_add(1);
                self.last_unknown_context_urnti = Some(*urnti);
            }
            _ => return,
        }
        self.revision = self.revision.saturating_add(1);
    }

    pub(super) fn fach(&mut self, work: &Result<FachWork, SignallingError>) {
        match work {
            Ok(FachWork::Idle) => return,
            Ok(FachWork::Published(target)) => {
                self.fach_planned = self.fach_planned.saturating_add(1);
                self.last_fach_target = Some(*target);
                self.last_fach_error = None;
            }
            Ok(FachWork::Blocked { target, error }) => {
                self.fach_blocked = self.fach_blocked.saturating_add(1);
                self.last_fach_target = Some(*target);
                self.last_fach_error = Some(error.into());
            }
            Err(error) => self.last_fach_error = Some(error.into()),
        }
        self.revision = self.revision.saturating_add(1);
    }

    pub(super) fn plan(&mut self, event: PlanEvent) {
        match event.stage {
            PlanStage::Committed => self.plans_committed = self.plans_committed.saturating_add(1),
            PlanStage::Rejected(_) => self.plans_rejected = self.plans_rejected.saturating_add(1),
            _ => (),
        }
        self.last_plan = Some(event);
        self.revision = self.revision.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_rejections_distinguish_missing_context_from_bad_mac() {
        use umts_rrc::integrity::{Error, ScheduleFailure};
        for reason in [
            ScheduleFailure::MissingEpoch,
            ScheduleFailure::Integrity(Error::Authentication),
        ] {
            assert_eq!(
                FailureClass::from(&SignallingError::Integrity(reason)),
                FailureClass::Integrity(reason)
            );
        }
    }

    #[test]
    fn control_failure_retains_exact_stage_and_reason_without_repeated_revisions() {
        let mut diagnostic = AccessDiagnostics::default();
        let error = SignallingError::Nas(umts_rrc::nas::NasError::SecurityRequired);
        diagnostic.control_failure(ControlFailureStage::Dedicated, &error);
        diagnostic.control_failure(ControlFailureStage::Dedicated, &error);
        assert_eq!(diagnostic.revision, 1);
        assert_eq!(
            diagnostic.last_control_failure,
            Some(ControlFailureEvidence {
                stage: ControlFailureStage::Dedicated,
                error: error.clone(),
            })
        );
        diagnostic.control_failure(ControlFailureStage::Timers, &error);
        assert_eq!(diagnostic.revision, 2);
        assert_eq!(
            diagnostic.last_control_failure.unwrap().stage,
            ControlFailureStage::Timers
        );
    }

    #[test]
    fn core_failure_retains_stage_and_safe_reason_without_repeated_revisions() {
        use crate::core_link::{DispatchError, Error};
        use umts_core::DriveError;
        let mut diagnostic = AccessDiagnostics::default();
        let command: Error<(), ()> =
            Error::Command(umts_core::Error::Session(umts_iuh::Error::State));
        diagnostic.core_failure(&command);
        diagnostic.core_failure(&command);
        assert_eq!(diagnostic.revision, 1);
        assert_eq!(
            diagnostic.last_core_failure,
            Some(CoreFailure::Command(umts_core::Error::Session(
                umts_iuh::Error::State
            )))
        );
        let radio: Error<(), ()> = Error::Drive(DriveError::Upper(DispatchError::Radio(
            SignallingError::Nas(umts_rrc::nas::NasError::Context),
        )));
        diagnostic.core_failure(&radio);
        assert_eq!(diagnostic.revision, 2);
        assert_eq!(
            diagnostic.last_core_failure,
            Some(CoreFailure::Radio(FailureClass::Nas(
                umts_rrc::nas::NasError::Context
            )))
        );
    }

    #[test]
    fn repeated_unsupported_core_event_does_not_generate_repeated_diagnostic_revisions() {
        use umts_iuh::session::EventKind;
        let mut diagnostic = AccessDiagnostics::default();
        let release = EventKind::Ranap {
            procedure: umts_iuh::ranap::IU_RELEASE,
        };
        diagnostic.deferred_core(release);
        diagnostic.deferred_core(release);
        assert_eq!(diagnostic.last_deferred_core, Some(release));
        assert_eq!(diagnostic.revision, 1);
        diagnostic.deferred_core(EventKind::Disconnected);
        assert_eq!(diagnostic.last_deferred_core, Some(EventKind::Disconnected));
        assert_eq!(diagnostic.revision, 2);
    }

    #[test]
    fn core_identity_counters_change_revision_without_storing_subscriber_values() {
        let mut diagnostic = AccessDiagnostics::default();
        let mut counts = crate::signalling::SignallingCounters::default();
        diagnostic.identities(counts);
        assert_eq!(diagnostic.revision, 0);
        counts.confirmed_core_identities = 2;
        counts.conflicting_core_identities = 1;
        diagnostic.identities(counts);
        assert_eq!(diagnostic.revision, 1);
        assert_eq!(diagnostic.confirmed_core_identities, 2);
        assert_eq!(diagnostic.conflicting_core_identities, 1);
        diagnostic.identities(counts);
        assert_eq!(diagnostic.revision, 1);
    }

    #[test]
    fn unsupported_control_reports_the_exact_procedure_without_payloads() {
        use crate::signalling::ControlMessageKind;
        let error = SignallingError::UnsupportedControl(
            ControlMessageKind::SignallingConnectionReleaseIndication,
        );
        let mut diagnostic = AccessDiagnostics::default();
        diagnostic.control_failure(ControlFailureStage::Dedicated, &error);
        assert_eq!(
            diagnostic.last_control_failure,
            Some(ControlFailureEvidence {
                stage: ControlFailureStage::Dedicated,
                error: error.clone(),
            })
        );
        assert_eq!(FailureClass::from(&error), FailureClass::Message);
        diagnostic.control_failure(ControlFailureStage::Dedicated, &error);
        assert_eq!(diagnostic.revision, 1);
    }

    #[test]
    fn idle_fach_does_not_change_diagnostic_revision() {
        let mut diagnostic = AccessDiagnostics::default();
        diagnostic.fach(&Ok(FachWork::Idle));
        assert_eq!(diagnostic, AccessDiagnostics::default());
        diagnostic.fach(&Ok(FachWork::Published(FachTarget::Common)));
        assert_eq!(diagnostic.fach_planned, 1);
        assert_eq!(diagnostic.plans_committed, 0);
        diagnostic.fach(&Ok(FachWork::Blocked {
            target: FachTarget::Common,
            error: SignallingError::Capacity,
        }));
        assert_eq!(diagnostic.fach_blocked, 1);
        assert_eq!(diagnostic.last_fach_error, Some(FailureClass::Capacity));
        assert_eq!(diagnostic.revision, 2);
    }
}
