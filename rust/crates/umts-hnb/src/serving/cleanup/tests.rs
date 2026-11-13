// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    core_link::{
        CoreLink,
        tests::{Mock, ready, register},
    },
    rab_handoff::{PendingRabAssignment, RabHandoff},
    signalling::CommonSignalling,
    test_support::{self, count},
};
use umts_iuh::{
    Domain,
    ranap::rab::{Assignment, RabId, Release, Released},
    rua,
    session::RabOutcome,
    wire::Limits,
};

#[test]
fn setup_failure_reports_transmit_coordinates_without_replacing_the_owner_evidence() {
    use crate::dedicated_tx::{
        TransmitCounters, TransmitError, TransmitFailureEvidence, TransmitOperation, TransmitPhase,
        TransmitTimingStage,
    };
    let identity = ReceiveIdentity {
        ue: umts_types::UeKey {
            epoch: test_support::at(0).epoch,
            slot: 0,
            generation: std::num::NonZeroU64::MIN,
        },
        configuration: std::num::NonZeroU64::MIN,
    };
    let evidence = TransmitFailureEvidence {
        identity,
        operation: TransmitOperation::Arm,
        error: TransmitError::Late(TransmitTimingStage::Arm),
        render_cursor: test_support::at(2560),
        requested: test_support::at(0),
        anchor: crate::dedicated_rx::FrameAnchor {
            time: test_support::at(0),
            cfn: 0,
        },
        next_prepare: test_support::at(38_400),
        phase: TransmitPhase::Prepared,
        counters: TransmitCounters::default(),
        initial_tpc_maximum_slots: None,
    };
    let mut report = CleanupStep::default();
    let mut startup = crate::rel99_admission::StartupProgress {
        observed_at: test_support::at(2560),
        packets_prepared: true,
        circuits_prepared: true,
        bootstrap_submitted: false,
        bootstrap: None,
        publication_attempts: 0,
        publication_readiness: None,
        security_delivery: None,
        last_backpressure: None,
    };
    report.setup_retired(
        identity,
        umts_rrc::dedicated::SetupPhase::AbortedBeforePublication,
        &AdmissionError::Transmit(evidence.error.clone()),
        Some(&evidence),
        Some(crate::rel99_admission::AdmissionPhase::Starting),
        Some(&startup),
    );
    startup.publication_attempts = 7;
    let Some(CleanupFailure::DedicatedRetirement {
        transmit: Some(saved),
        admission_phase,
        startup: Some(saved_startup),
        ..
    }) = report.first_failure
    else {
        panic!("cleanup must preserve exact original TX evidence")
    };
    assert_eq!(saved.identity, identity);
    assert_eq!(saved.error, evidence.error);
    assert_eq!(saved.render_cursor, evidence.render_cursor);
    assert_eq!(saved.anchor.time, evidence.anchor.time);
    assert_eq!(
        admission_phase,
        Some(crate::rel99_admission::AdmissionPhase::Starting)
    );
    assert_eq!(saved_startup.publication_attempts, 0);
    assert_eq!(startup.publication_attempts, 7);
    let mut completed = CleanupStep::default();
    completed.setup_retired(
        identity,
        umts_rrc::dedicated::SetupPhase::Complete,
        &AdmissionError::Transmit(evidence.error.clone()),
        Some(&evidence),
        None,
        None,
    );
    assert_eq!(completed.failures, 1);
    assert!(matches!(
        completed.first_failure,
        Some(CleanupFailure::DedicatedRetirement {
            phase: umts_rrc::dedicated::SetupPhase::Complete,
            transmit: Some(_),
            ..
        })
    ));
}

#[test]
fn incomplete_setup_retirement_reports_exact_phase_and_error_without_consuming_cause() {
    use umts_rrc::dedicated::SetupPhase;
    let identity = ReceiveIdentity {
        ue: umts_types::UeKey {
            epoch: test_support::at(0).epoch,
            slot: 0,
            generation: std::num::NonZeroU64::MIN,
        },
        configuration: std::num::NonZeroU64::MIN,
    };
    for phase in [
        SetupPhase::Prepared,
        SetupPhase::AbortedBeforePublication,
        SetupPhase::RecoveryRequired,
    ] {
        let cause = AdmissionError::Transmit(crate::dedicated_tx::TransmitError::SubmissionLost);
        let mut step = CleanupStep::default();
        step.setup_retired(identity, phase, &cause, None, None, None);
        assert_eq!(step.failures, 1);
        assert!(
            matches!(step.first_failure, Some(CleanupFailure::DedicatedRetirement { identity: actual, phase: actual_phase, error, transmit: None, admission_phase: None, startup: None }) if actual == identity && actual_phase == phase && error == cause)
        );
        assert_eq!(
            cause,
            AdmissionError::Transmit(crate::dedicated_tx::TransmitError::SubmissionLost)
        );
    }
    let mut step = CleanupStep::default();
    step.setup_retired(
        identity,
        SetupPhase::Complete,
        &AdmissionError::Cancelled,
        None,
        None,
        None,
    );
    assert_eq!(step.failures, 0);
    assert!(step.first_failure.is_none());
}

pub(crate) fn assert_quarantine_recovery(owner: crate::rel99_admission::QuarantinedAdmission) {
    let identity = owner.identity();
    let error = owner.error().clone();
    let custody = owner.pending_receiver_commands() + owner.retained_receivers();
    let mut cleanup = Cleanup::new(count(1), count(1)).unwrap();
    assert_eq!(cleanup.recovery_error(), None);
    cleanup.quarantine(0, owner);
    assert_eq!(cleanup.recovery_error(), Some(&error));
    assert_eq!(cleanup.recovery_required(), Some(identity));
    assert_eq!(cleanup.pending(), 1);
    assert_eq!(cleanup.recovery_required(), Some(identity));
    assert!(custody > 0);
}

fn release_rejection() -> (CoreLink<Mock>, CommonSignalling, PendingRabAssignment) {
    let source = test_support::source(false);
    let mut radio = crate::signalling::tests::controller(&source);
    let (mut core, peer) = ready(&mut radio);
    let (_, context, _) = register(&mut core, &peer, &mut radio, &source, 0, 3);
    let mut setup = crate::rel99_planner::tests::ps_request(12_800);
    setup.first.id = RabId::new(10);
    peer.rua(rua::Message::Connect {
        domain: Domain::Ps,
        context,
        cause: rua::EstablishmentCause::Normal,
        selector: None,
        membership: None,
        ranap: Assignment {
            setup: vec![setup],
            release: vec![Release::new(9, cause())],
        }
        .encode(Limits::default())
        .unwrap(),
    });
    let mut handoff = RabHandoff::new(Limits::default());
    core.drive(ControlTime(6), &mut radio, |_, event| {
        handoff.offer(event).map(|_| ())
    })
    .unwrap();
    (core, radio, handoff.take().unwrap().unwrap())
}

fn rejected(pending: PendingRabAssignment) -> Box<RejectedReleaseAssignment> {
    Box::new(RejectedReleaseAssignment {
        error: ReleaseAdmissionError::Admission(AdmissionError::Cancelled),
        pending,
        pool: None,
        change: None,
        rollback_error: None,
    })
}

#[test]
fn release_reporting_preserves_already_recorded_sibling_outcome() {
    let (mut core, mut radio, pending) = release_rejection();
    let request = pending.request();
    core.gateway_mut()
        .record_rab_outcome(
            ControlTime(7),
            request,
            RabOutcome::Released(Released::new(9)),
        )
        .unwrap();
    let mut report = CleanupStep::default();
    let Some(State::Reporting(mut reporting)) =
        report_rejected_release(rejected(pending), core.gateway(), &mut report)
    else {
        panic!("rolled back release must retain canonical reporting owner");
    };
    assert!(
        matches!(report.first_failure, Some(CleanupFailure::RejectedRelease { request: actual, .. }) if actual == request)
    );
    assert_eq!(core.gateway().queued_frames(), 0);
    let result = reporting
        .record(&mut core, ControlTime(8), count(2))
        .unwrap();
    assert_eq!((result.recorded, result.superseded), (1, 1));
    assert!(reporting.is_complete());
    assert!(core.gateway().has_pending_rab_request(request));
    assert!(
        core.drive(ControlTime(9), &mut radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    assert!(!core.gateway().has_pending_rab_request(request));
}

#[test]
fn failed_release_reporting_keeps_exact_request_for_original_gateway_retry() {
    let (core, _, pending) = release_rejection();
    let request = pending.request();
    let (foreign, _, _) = release_rejection();
    let mut report = CleanupStep::default();
    let Some(State::RejectedRelease(retained)) =
        report_rejected_release(rejected(pending), foreign.gateway(), &mut report)
    else {
        panic!("foreign gateway must not consume the original rejected owner");
    };
    assert_eq!(retained.pending.request(), request);
    assert!(retained.pool.is_none() && retained.change.is_none());
    assert!(matches!(
        report.first_failure,
        Some(CleanupFailure::Disposition {
            error: FailureDispositionError::Authority,
            ..
        })
    ));
    let mut retry = CleanupStep::default();
    assert!(matches!(
        report_rejected_release(retained, core.gateway(), &mut retry),
        Some(State::Reporting(_))
    ));
}

#[test]
fn absent_media_does_not_settle_a_nonempty_original_request() {
    assert!(!outcomes_settled(Some(1), None, None));
    assert!(outcomes_settled(None, None, None));
}

#[test]
fn all_original_items_must_have_recorded_or_superseded_outcomes() {
    let mut packet = PacketProgress {
        items: 2,
        retired: 2,
        recorded: 1,
        ..PacketProgress::default()
    };
    assert!(!outcomes_settled(Some(2), Some(packet), None));
    packet.superseded = 1;
    assert!(outcomes_settled(Some(2), Some(packet), None));
    assert!(!outcomes_settled(Some(3), Some(packet), None));
    assert!(!outcomes_settled(Some(1), Some(packet), None));
}

#[test]
fn terminal_negotiation_failure_still_requires_its_canonical_outcome() {
    let mut circuit = CircuitProgress {
        items: 1,
        retired: 1,
        failed: 1,
        ..CircuitProgress::default()
    };
    assert!(!outcomes_settled(Some(1), None, Some(circuit)));
    circuit.recorded = 1;
    assert!(outcomes_settled(Some(1), None, Some(circuit)));
}

#[test]
fn settlement_accounts_for_both_original_media_owners() {
    let packet = PacketProgress {
        items: 1,
        recorded: 1,
        retired: 1,
        ..PacketProgress::default()
    };
    let mut circuit = CircuitProgress {
        items: 1,
        retired: 1,
        ..CircuitProgress::default()
    };
    assert!(!outcomes_settled(Some(2), Some(packet), Some(circuit)));
    circuit.superseded = 1;
    assert!(outcomes_settled(Some(2), Some(packet), Some(circuit)));
}

#[test]
fn ordinary_cleanup_has_no_recovery_requirement() {
    let cleanup = Cleanup::new(count(2), count(1)).unwrap();
    assert_eq!(cleanup.recovery_required(), None);
}
