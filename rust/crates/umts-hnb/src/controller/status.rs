// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ControllerStep, DedicatedProcessingError, UplinkFailure};
use crate::{
    dedicated_rx::ReceiveIdentity,
    serving::{AccessDiagnostics, ControlFailureStage, DeferredCore},
    signalling::CommonSignalling,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub received_iu_releases: u64,
    pub completed_iu_releases: u64,
    pub rach_transports: u64,
    pub connection_requests: u64,
    pub rejected_common: u64,
    pub deferred_core: u64,
    pub failed_core: u64,
    pub rejected_core: u64,
    pub failed_control: u64,
    pub failed_dedicated: u64,
    pub failed_admission: u64,
    pub failed_hsdpa_control: u64,
    pub hsdpa_feedback_requests: u64,
    pub hsdpa_published: u64,
    pub hsdpa_cancelled: u64,
    pub hsdpa_stale_cancelled: u64,
    pub hsdpa_command_rejected: u64,
    pub hsdpa_last_command_rejection: Option<crate::hsdpa_tx::Error>,
    pub hsdpa_late_rejections: u64,
    pub hsdpa_late_chips_total: u64,
    pub hsdpa_late_chips_maximum: u64,
    pub hsdpa_last_stale_downlink_floor:
        Option<(umts_types::time::ChipInstant, umts_types::time::ChipInstant)>,
    pub hsdpa_publication_backpressure: u64,
    pub hsdpa_delivered: u64,
    pub hsdpa_retry: u64,
    pub hsdpa_not_transmitted: u64,
    pub hsdpa_feedback_reports: u64,
    pub hsdpa_feedback_backpressure: u64,
    pub hsdpa_packet_pdus: u64,
    pub hsdpa_packet_backpressure: u64,
    pub failed_hsupa_control: u64,
    pub hsupa_control_visits: u64,
    pub hsupa_control_replies: u64,
    pub hsupa_packet_pdus: u64,
    pub hsupa_packet_backpressure: u64,
    pub failed_user_plane: u64,
    pub accepted_assignments: u64,
    pub rejected_assignments: u64,
    pub completed_cleanups: u64,
    pub failed_bearers: u64,
}

#[derive(Default)]
pub(crate) struct ControlStatus {
    pub(crate) counters: Counters,
    pub(crate) access_diagnostics: AccessDiagnostics,
    uplink_error: Option<UplinkFailure<crate::rel99_traffic::Error>>,
    dedicated_processing_error: Option<(ReceiveIdentity, DedicatedProcessingError)>,
    hsupa_control_error: Option<String>,
    hsdpa_control_error: Option<String>,
}

impl ControlStatus {
    pub(crate) fn observe<T>(
        &mut self,
        report: ControllerStep<T, DeferredCore, crate::rel99_traffic::Error>,
        radio: &CommonSignalling,
    ) -> bool {
        let published = report
            .admission
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .is_some_and(|admission| admission.published != 0);
        let counters = &mut self.counters;
        if let Some(common) = &report.common {
            self.access_diagnostics.common(common, radio);
        }
        self.access_diagnostics.identities(radio.counters());
        self.access_diagnostics.fach(&report.fach);
        if let Some(event) = report.transmit {
            self.access_diagnostics.plan(event);
        }
        counters.failed_user_plane = counters
            .failed_user_plane
            .saturating_add(report.uplink_failures as u64);
        counters.failed_dedicated = counters.failed_dedicated.saturating_add(u64::from(
            report
                .dedicated_bearers
                .as_ref()
                .is_some_and(|result| match result {
                    Ok(step) => step.failure.is_some(),
                    Err(_) => true,
                }),
        ));
        if self.dedicated_processing_error.is_none() {
            self.dedicated_processing_error = report
                .dedicated_bearers
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(|step| step.failure.clone());
        }
        counters.failed_admission = counters.failed_admission.saturating_add(u64::from(
            report.admission.as_ref().is_some_and(Result::is_err),
        ));
        counters.failed_hsdpa_control = counters
            .failed_hsdpa_control
            .saturating_add(report.hsdpa.failures as u64)
            .saturating_add(report.hsdpa.feedback_failures as u64)
            .saturating_add(u64::from(report.hsdpa.transmit.failure.is_some()));
        if self.hsdpa_control_error.is_none() {
            if let Some(failure) = &report.hsdpa.first_failure {
                self.hsdpa_control_error = Some(format!("{failure:?}"));
            } else if let Some(failure) = &report.hsdpa.transmit.failure {
                self.hsdpa_control_error = Some(format!("transmit: {failure:?}"));
            }
        }
        if let Some(result) = &report.hsupa {
            match result {
                Ok(step) => {
                    counters.hsupa_packet_pdus = counters
                        .hsupa_packet_pdus
                        .saturating_add(step.packet_pdus as u64);
                    counters.hsupa_packet_backpressure = counters
                        .hsupa_packet_backpressure
                        .saturating_add(step.packet_backpressure as u64);
                    if self.hsupa_control_error.is_none() {
                        self.hsupa_control_error = step.first_failure.clone();
                    }
                    counters.failed_hsupa_control = counters
                        .failed_hsupa_control
                        .saturating_add(step.failures as u64);
                    counters.hsupa_control_visits = counters
                        .hsupa_control_visits
                        .saturating_add(step.visited as u64);
                    counters.hsupa_control_replies = counters
                        .hsupa_control_replies
                        .saturating_add(step.replies as u64);
                }
                Err(error) => {
                    if self.hsupa_control_error.is_none() {
                        self.hsupa_control_error = Some(format!("{error:#}"));
                    }
                    counters.failed_hsupa_control = counters.failed_hsupa_control.saturating_add(1);
                }
            }
        }
        counters.hsdpa_published = counters
            .hsdpa_published
            .saturating_add(report.hsdpa.transmit.published as u64);
        counters.hsdpa_cancelled = counters
            .hsdpa_cancelled
            .saturating_add(report.hsdpa.transmit.cancelled as u64);
        counters.hsdpa_stale_cancelled = counters
            .hsdpa_stale_cancelled
            .saturating_add(report.hsdpa.transmit.stale_cancelled as u64);
        counters.hsdpa_command_rejected = counters
            .hsdpa_command_rejected
            .saturating_add(report.hsdpa.transmit.rejected as u64);
        if let Some(reason) = report.hsdpa.transmit.last_rejection {
            counters.hsdpa_last_command_rejection = Some(reason);
        }
        counters.hsdpa_late_rejections = counters
            .hsdpa_late_rejections
            .saturating_add(report.hsdpa.transmit.late_rejections as u64);
        counters.hsdpa_late_chips_total = counters
            .hsdpa_late_chips_total
            .saturating_add(report.hsdpa.transmit.late_chips_total);
        counters.hsdpa_late_chips_maximum = counters
            .hsdpa_late_chips_maximum
            .max(report.hsdpa.transmit.late_chips_maximum);
        if let Some(window) = report.hsdpa.transmit.last_stale_downlink_floor {
            counters.hsdpa_last_stale_downlink_floor = Some(window);
        }
        counters.hsdpa_publication_backpressure = counters
            .hsdpa_publication_backpressure
            .saturating_add(report.hsdpa.transmit.publication_backpressure as u64);
        counters.hsdpa_feedback_requests = counters
            .hsdpa_feedback_requests
            .saturating_add(report.hsdpa.feedback_requests as u64);
        counters.hsdpa_delivered = counters
            .hsdpa_delivered
            .saturating_add(report.hsdpa.delivered as u64);
        counters.hsdpa_retry = counters
            .hsdpa_retry
            .saturating_add(report.hsdpa.retry as u64);
        counters.hsdpa_not_transmitted = counters
            .hsdpa_not_transmitted
            .saturating_add(report.hsdpa.not_transmitted as u64);
        counters.hsdpa_feedback_reports = counters
            .hsdpa_feedback_reports
            .saturating_add(report.hsdpa.feedback_reports as u64);
        counters.hsdpa_feedback_backpressure = counters
            .hsdpa_feedback_backpressure
            .saturating_add(report.hsdpa.feedback_backpressure as u64);
        counters.hsdpa_packet_pdus = counters
            .hsdpa_packet_pdus
            .saturating_add(report.hsdpa.packet_pdus as u64);
        counters.hsdpa_packet_backpressure = counters
            .hsdpa_packet_backpressure
            .saturating_add(report.hsdpa.packet_backpressure as u64);
        use crate::{
            core_link::{DispatchError, Error},
            signalling::CommonIngressWork,
        };
        counters.rach_transports = counters
            .rach_transports
            .saturating_add(u64::from(report.rach.is_some()));
        match report.common.as_ref().map(|common| &common.work) {
            Some(CommonIngressWork::ConnectionRequested(_)) => {
                counters.connection_requests = counters.connection_requests.saturating_add(1);
            }
            Some(CommonIngressWork::Rejected(_)) => {
                counters.rejected_common = counters.rejected_common.saturating_add(1);
            }
            _ => (),
        }
        match &report.core {
            Err(Error::Drive(umts_core::DriveError::Upper(DispatchError::Other(_)))) => {
                counters.deferred_core = counters.deferred_core.saturating_add(1);
            }
            Err(error) => {
                self.access_diagnostics.core_failure(error);
                counters.failed_core = counters.failed_core.saturating_add(1);
            }
            Ok(step) => {
                counters.rejected_core = counters
                    .rejected_core
                    .saturating_add(u64::from(step.uplink_rejected));
            }
        }
        let rejected = |error: &crate::signalling::SignallingError| {
            !matches!(
                error,
                crate::signalling::SignallingError::Capacity
                    | crate::signalling::SignallingError::RegistrationPending
            )
        };
        let failure = [
            (
                ControlFailureStage::Collection,
                report.collected.as_ref().err(),
            ),
            (
                ControlFailureStage::Dedicated,
                report
                    .dedicated
                    .as_ref()
                    .and_then(|result| result.as_ref().err()),
            ),
            (
                ControlFailureStage::Security,
                report
                    .security
                    .as_ref()
                    .and_then(|result| result.as_ref().err()),
            ),
            (
                ControlFailureStage::CommonTransmit,
                report.fach.as_ref().err(),
            ),
            (ControlFailureStage::Timers, report.timers.as_ref().err()),
            (
                ControlFailureStage::Retirement,
                report.retired.as_ref().err(),
            ),
        ]
        .into_iter()
        .find_map(|(stage, error)| {
            error
                .filter(|error| rejected(error))
                .map(|error| (stage, error))
        });
        if let Some((stage, error)) = failure {
            self.access_diagnostics.control_failure(stage, error);
        }
        counters.failed_control = counters
            .failed_control
            .saturating_add(u64::from(failure.is_some()));
        if self.uplink_error.is_none() {
            self.uplink_error = report.uplink_failure;
        }
        published
    }

    pub(crate) fn report_failures(&mut self) {
        if let Some(failure) = self.uplink_error.take() {
            tracing::warn!(
                "Uplink SDU delivery failed: metadata={:?} error={:?}",
                failure.metadata,
                failure.error
            );
        }
        if let Some(failure) = self.dedicated_processing_error.take() {
            tracing::warn!("Dedicated bearer processing failed: {failure:?}");
        }
        if let Some(failure) = self.hsupa_control_error.take() {
            tracing::warn!("HSUPA control service failed: {failure}");
        }
        if let Some(failure) = self.hsdpa_control_error.take() {
            tracing::warn!("HSDPA control service failed: {failure}");
        }
    }
}
