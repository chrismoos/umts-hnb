// SPDX-License-Identifier: AGPL-3.0-or-later

use super::SignallingError;
use umts_rrc::{
    nas::SignallingBearer,
    reports::{self, MeasurementSummary, StatusReport},
};
use umts_rrc_schema::class_definitions::ULDCCHMessageType;
mod layout;

#[derive(Clone, Copy, Debug, Default)]
pub struct Reports {
    pub status_count: u64,
    pub measurement_count: u64,
    cpich_measurement_count: u64,
    pub last_status: Option<StatusReport>,
    pub last_measurement: Option<MeasurementSummary>,
    pub last_secondary_measurement: Option<MeasurementSummary>,
    pub secondary_measurement_configured: bool,
    pub secondary_measurement_failures: u64,
    secondary_scope: Option<(
        crate::dedicated_rx::ReceiveIdentity,
        umts_rrc::measurement::SecondaryCpichTarget,
    )>,
    secondary_requested: Option<(
        umts_rrc::establishment::TransactionId,
        umts_rrc::measurement::SecondaryCpichTarget,
    )>,
    pub measurement_control_configured: bool,
    pub measurement_control_failures: u64,
    pub unsupported_control_count: u64,
    process_allocation: Option<(u8, usize)>,
    requested: Option<umts_rrc::establishment::TransactionId>,
    bler_requested: Option<umts_rrc::establishment::TransactionId>,
    latest_announced_layout: [Option<layout::TransportDescription>; layout::MAX_TRANSPORTS],
}

static PROCESS_ALLOCATIONS: std::sync::Mutex<
    Vec<(
        umts_types::UeKey,
        u8,
        std::sync::Arc<umts_rrc::hsupa::UplinkActivation>,
    )>,
> = std::sync::Mutex::new(Vec::new());

pub(crate) fn desire_process_allocation(
    key: umts_types::UeKey,
    processes: u8,
    configuration: &std::sync::Arc<umts_rrc::hsupa::UplinkActivation>,
) {
    let mut all = PROCESS_ALLOCATIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    all.retain(|(other, _, _)| other.epoch == key.epoch && *other != key);
    all.push((key, processes, std::sync::Arc::clone(configuration)));
    if all.len() > 64 {
        all.remove(0);
    }
}

pub(crate) fn retain_process_allocations(active: impl Fn(umts_types::UeKey) -> bool) {
    PROCESS_ALLOCATIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|(key, _, _)| active(*key));
}

pub(super) fn request_process_allocation(
    context: &mut super::Context,
    key: umts_types::UeKey,
) -> Result<(), SignallingError> {
    let desired = PROCESS_ALLOCATIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .find(|(other, _, _)| *other == key)
        .map(|(_, processes, configuration)| (*processes, std::sync::Arc::clone(configuration)));
    let Some((processes, configuration)) = desired else {
        return Ok(());
    };
    let setup = std::sync::Arc::as_ptr(&configuration) as usize;
    let current = context
        .reports
        .process_allocation
        .filter(|(_, sent)| *sent == setup)
        .map_or(0xff, |(processes, _)| processes);
    if current == processes || !can_publish(context, key, SignallingBearer::ControlAm)? {
        return Ok(());
    }
    let kind = super::transactions::MessageType::PhysicalChannelReconfiguration;
    let transaction = context.transactions.next(kind);
    let message = configuration
        .process_allocation(transaction, processes)
        .map_err(|_| SignallingError::Mapping)?;
    publish(context, key, SignallingBearer::ControlAm, &message)?;
    context.transactions.admitted(kind, transaction);
    context.reports.process_allocation = Some((processes, setup));
    tracing::info!(
        "HARQ process allocation sent: slot={} generation={} processes={processes:#04x} transaction={}",
        key.slot,
        key.generation,
        transaction.get(),
    );
    Ok(())
}

pub(super) fn request_secondary_cpich(
    context: &mut super::Context,
    key: umts_types::UeKey,
    reporting: &umts_rrc::measurement::PeriodicalReporting,
    target: umts_rrc::measurement::SecondaryCpichTarget,
) -> Result<(), SignallingError> {
    let scope = context
        .dedicated_route
        .filter(|_| context.idle_release.is_none())
        .map(|identity| (identity, target));
    context.reports.refresh_secondary_scope(scope);
    if context.reports.secondary_requested.is_some()
        || context.reports.bler_requested.is_none()
        || !context.capabilities.report().is_some_and(|capability| {
            capability.hsdpa_dual_cell().is_some()
                && capability.adjacent_measurements_without_gaps()
        })
        || !can_publish(context, key, SignallingBearer::ControlAm)?
    {
        return Ok(());
    }
    let kind = super::transactions::MessageType::MeasurementControl;
    let transaction = context.transactions.next(kind);
    let message = umts_rrc::measurement::secondary_cpich_setup(transaction, reporting, target);
    publish(context, key, SignallingBearer::ControlAm, &message)?;
    context.transactions.admitted(kind, transaction);
    context.reports.secondary_requested = Some((transaction, target));
    Ok(())
}

pub(super) fn request_cpich(
    context: &mut super::Context,
    key: umts_types::UeKey,
    reporting: &umts_rrc::measurement::PeriodicalReporting,
) -> Result<(), SignallingError> {
    use super::transactions::MessageType;
    if !can_publish(context, key, SignallingBearer::ControlAm)? {
        return Ok(());
    }
    if context.reports.requested.is_some() {
        if context.reports.bler_requested.is_none() {
            let kind = MessageType::MeasurementControl;
            let transaction = context.transactions.next(kind);
            let message = umts_rrc::measurement::bler_setup(transaction, reporting);
            publish(context, key, SignallingBearer::ControlAm, &message)?;
            context.transactions.admitted(kind, transaction);
            context.reports.bler_requested = Some(transaction);
            tracing::debug!(
                "BLER measurement request: slot={} generation={} identity={} reporting={reporting:?} all_eligible_downlink_channels=true",
                key.slot,
                key.generation,
                umts_rrc::measurement::BLER_MEASUREMENT_ID
            );
        }
        return Ok(());
    }
    let kind = MessageType::MeasurementControl;
    let transaction = context.transactions.next(kind);
    let message = umts_rrc::measurement::cpich_setup(transaction, reporting);
    publish(context, key, SignallingBearer::ControlAm, &message)?;
    context.transactions.admitted(kind, transaction);
    context.reports.requested = Some(transaction);
    tracing::debug!(
        "CPICH measurement request: slot={} generation={} identity={} transaction={} reporting={reporting:?}",
        key.slot,
        key.generation,
        umts_rrc::measurement::CPICH_MEASUREMENT_ID,
        transaction.get(),
    );
    Ok(())
}

fn can_publish(
    context: &super::Context,
    key: umts_types::UeKey,
    bearer: SignallingBearer,
) -> Result<bool, SignallingError> {
    if context.dedicated_route.is_none()
        || context.setup.phase() != umts_rrc::establishment::EstablishmentPhase::Established
        || context.bearer_change.is_some()
        || context.idle_release.is_some()
        || context.control.pending_new_data()
        || context.nas_bearer.pending_new_data()
        || context.protection.integrity.pending_origin().is_some()
        || context.protection.integrity.unconfirmed_origin().is_some()
        || context.protection.bearer_reconfiguration_block().is_some()
        || [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps]
            .into_iter()
            .any(|domain| context.security.phase(domain) != crate::connection_security::Phase::Idle)
        || context
            .protection
            .integrity
            .downlink_origin(key, bearer.identity())
            .map_err(SignallingError::Integrity)?
            .is_none()
    {
        return Ok(false);
    }
    Ok(true)
}

fn publish(
    context: &mut super::Context,
    key: umts_types::UeKey,
    bearer: SignallingBearer,
    message: &umts_rrc_schema::class_definitions::DLDCCHMessage,
) -> Result<(), SignallingError> {
    use super::am_admission_error;
    use crate::common_am::CommonAmError;
    use umts_types::bits::Bits;
    let queue = match bearer {
        SignallingBearer::ControlAm => &mut context.control,
        SignallingBearer::NasAm => &mut context.nas_bearer,
        _ => return Err(SignallingError::Mapping),
    };
    context
        .protection
        .integrity
        .publish_with(key, bearer.identity(), message, |encoded| {
            queue.enqueue(
                key,
                Bits::new(&encoded.bytes, encoded.bit_length)
                    .map_err(|_| CommonAmError::Payload)?,
            )
        })
        .map_err(|error| super::protection::schedule_error(error, am_admission_error))
}

impl Reports {
    fn refresh_secondary_scope(
        &mut self,
        scope: Option<(
            crate::dedicated_rx::ReceiveIdentity,
            umts_rrc::measurement::SecondaryCpichTarget,
        )>,
    ) {
        if self.secondary_scope == scope {
            return;
        }
        self.secondary_scope = scope;
        self.secondary_requested = None;
        self.last_secondary_measurement = None;
        self.secondary_measurement_configured = false;
        self.secondary_measurement_failures = 0;
    }
    pub(super) fn describe_configuration(
        &mut self,
        configuration: &umts_rrc::dedicated::Rel99Configuration,
    ) {
        self.latest_announced_layout = layout::describe(configuration);
    }

    pub(super) fn log_measurement(
        &self,
        key: umts_types::UeKey,
        time: umts_rlc::acknowledged::ReceiveTime,
        transition_pending: bool,
    ) {
        let Some(report) = self.last_measurement else {
            return;
        };
        tracing::debug!(
            "UE downlink measurement: key={key:?} report_seq={} received={time:?} identity={} kind={:?} transition_pending={transition_pending}",
            self.measurement_count,
            report.identity,
            report.kind
        );
        for cell in report.cells.iter().flatten() {
            tracing::debug!(
                "UE downlink quality: key={key:?} measurement={} psc={} rscp_code={:?} ec_no_code={:?}",
                report.identity,
                cell.scrambling_code,
                cell.rscp_code,
                cell.ec_n0_code
            );
        }
        if report.kind != reports::MeasurementKind::Quality {
            return;
        }
        let mut channels = [None; layout::MAX_TRANSPORTS];
        for channel in report.bler.iter().flatten() {
            if let Some(output) = channel
                .transport_channel
                .checked_sub(1)
                .and_then(|id| channels.get_mut(usize::from(id)))
            {
                *output = Some(channel);
            }
        }
        for (index, description) in self.latest_announced_layout.iter().enumerate() {
            let transport = index as u8 + 1;
            let channel = channels[index];
            if description.is_none() && channel.is_none() {
                continue;
            }
            let code = channel.and_then(|channel| channel.bler_code);
            let availability = match channel {
                None => "not_listed",
                Some(channel) if channel.bler_code.is_none() => "not_provided",
                Some(_) => "reported",
            };
            let role = description.map_or("unknown", |description| description.role());
            let bearer_ids: Vec<_> = description
                .iter()
                .flat_map(|description| {
                    (1u8..=32).filter(move |id| description.bearer_mask & (1u32 << (id - 1)) != 0)
                })
                .collect();
            let crc_bits = description.map(|description| description.crc_bits);
            let percent_bounds = code.map(|code| match code {
                0 => (0.0, 0.0),
                1 => (0.0, 100.0 * 10.0_f64.powf(-4.03)),
                _ => (
                    100.0 * 10.0_f64.powf(-0.065 * f64::from(64 - code)),
                    100.0 * 10.0_f64.powf(-0.065 * f64::from(63 - code)),
                ),
            });
            tracing::debug!(
                "UE downlink BLER: key={key:?} report_seq={} measurement={} transport={transport} role={role} rb_ids={bearer_ids:?} crc_bits={crc_bits:?} availability={availability} bler_log_code={code:?} percent_bounds={percent_bounds:?} layout_source=latest_announced transition_pending={transition_pending}",
                self.measurement_count,
                report.identity
            );
        }
    }
    // Call after integrity admission so discarding a payload still commits its COUNT-I.
    pub(super) fn consume_unsupported(
        &mut self,
        result: Result<(), SignallingError>,
    ) -> Result<(), SignallingError> {
        match result {
            Err(SignallingError::UnsupportedControl(_)) => {
                self.unsupported_control_count = self.unsupported_control_count.saturating_add(1);
                Ok(())
            }
            result => result,
        }
    }

    // Invoke only after the connection's integrity policy has admitted this message.
    pub(super) fn admit(
        &mut self,
        bearer: SignallingBearer,
        message: &ULDCCHMessageType,
    ) -> Result<bool, SignallingError> {
        match message {
            ULDCCHMessageType::rrcStatus(status) => {
                if bearer != SignallingBearer::ControlAm {
                    return Err(SignallingError::Mapping);
                }
                let status = reports::status(status).map_err(|_| SignallingError::Message)?;
                self.last_status = Some(status);
                self.status_count = self.status_count.saturating_add(1);
            }
            ULDCCHMessageType::measurementReport(report) => {
                if !matches!(
                    bearer,
                    SignallingBearer::ControlAm | SignallingBearer::Unacknowledged
                ) {
                    return Err(SignallingError::Mapping);
                }
                let report = reports::measurement(report).map_err(|_| SignallingError::Message)?;
                if self.secondary_requested.is_some_and(|(_, target)| {
                    report.identity == umts_rrc::measurement::SECONDARY_CPICH_MEASUREMENT_ID
                        && report.kind == reports::MeasurementKind::InterFrequency
                        && report.inter_frequency_cells.iter().flatten().any(|cell| {
                            cell.uarfcn == Some(target.uarfcn())
                                && cell.cell.scrambling_code == target.scrambling_code()
                        })
                }) {
                    self.last_secondary_measurement = Some(report);
                    self.secondary_measurement_configured =
                        self.secondary_measurement_failures == 0;
                }
                if report.identity == umts_rrc::measurement::CPICH_MEASUREMENT_ID {
                    self.cpich_measurement_count = self.cpich_measurement_count.saturating_add(1);
                }
                if self.requested.is_some()
                    && report.identity == umts_rrc::measurement::CPICH_MEASUREMENT_ID
                    && self.measurement_control_failures == 0
                {
                    self.measurement_control_configured = true;
                }
                self.last_measurement = Some(report);
                self.measurement_count = self.measurement_count.saturating_add(1);
            }
            ULDCCHMessageType::measurementControlFailure(failure) => {
                if bearer != SignallingBearer::ControlAm {
                    return Err(SignallingError::Mapping);
                }
                tracing::warn!(
                    "UE measurement control failure: transaction={} cause={:?}",
                    failure.rrc_transaction_identifier.0,
                    failure
                );
                if self.requested.is_some_and(|transaction| {
                    transaction.get() == failure.rrc_transaction_identifier.0
                }) {
                    self.measurement_control_configured = false;
                    self.measurement_control_failures =
                        self.measurement_control_failures.saturating_add(1);
                }
                if self.secondary_requested.is_some_and(|(transaction, _)| {
                    transaction.get() == failure.rrc_transaction_identifier.0
                }) {
                    self.secondary_measurement_configured = false;
                    self.secondary_measurement_failures =
                        self.secondary_measurement_failures.saturating_add(1);
                }
            }
            _ => return Ok(false),
        }
        // Reports do not acknowledge procedures or authorize bearer/security transitions.
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_rrc_schema::{information_elements::*, pdu_definitions::*};

    #[test]
    fn unsupported_policy_preserves_backpressure_and_other_errors() {
        let mut reports = Reports::default();
        for error in [
            SignallingError::Capacity,
            SignallingError::Mapping,
            SignallingError::Phase,
        ] {
            assert_eq!(reports.consume_unsupported(Err(error.clone())), Err(error));
        }
        assert_eq!(reports.unsupported_control_count, 0);
    }

    #[test]
    fn configuration_requires_requested_identity_and_matching_failure_disables_it() {
        use umts_rrc::establishment::TransactionId;
        let mut reports = Reports::default();
        let measurement = |identity| {
            ULDCCHMessageType::measurementReport(MeasurementReport::new(
                MeasurementIdentity(identity),
                None,
                None,
                None,
                None,
                None,
            ))
        };
        let expected = umts_rrc::measurement::CPICH_MEASUREMENT_ID;
        reports
            .admit(SignallingBearer::ControlAm, &measurement(expected))
            .unwrap();
        assert!(!reports.measurement_control_configured);
        reports.requested = Some(TransactionId::new(2).unwrap());
        reports
            .admit(SignallingBearer::ControlAm, &measurement(expected - 1))
            .unwrap();
        assert!(!reports.measurement_control_configured);
        reports
            .admit(SignallingBearer::ControlAm, &measurement(expected))
            .unwrap();
        assert!(reports.measurement_control_configured);
        let failure = |transaction| {
            ULDCCHMessageType::measurementControlFailure(MeasurementControlFailure::new(
                RRCTransactionIdentifier(transaction),
                FailureCauseWithProtErr::unsupportedMeasurement(()),
                None,
            ))
        };
        reports
            .admit(SignallingBearer::ControlAm, &failure(1))
            .unwrap();
        assert!(reports.measurement_control_configured);
        assert_eq!(reports.measurement_control_failures, 0);
        assert!(
            reports
                .admit(SignallingBearer::Unacknowledged, &failure(2))
                .is_err()
        );
        assert!(reports.measurement_control_configured);
        reports
            .admit(SignallingBearer::ControlAm, &failure(2))
            .unwrap();
        assert!(!reports.measurement_control_configured);
        assert_eq!(reports.measurement_control_failures, 1);
        reports
            .admit(SignallingBearer::ControlAm, &measurement(expected))
            .unwrap();
        assert!(!reports.measurement_control_configured);
    }

    #[test]
    fn secondary_control_failure_does_not_disable_primary_measurements() {
        use umts_rrc::{establishment::TransactionId, measurement::SecondaryCpichTarget};
        let mut reports = Reports {
            requested: Some(TransactionId::new(0).unwrap()),
            measurement_control_configured: true,
            secondary_requested: Some((
                TransactionId::new(2).unwrap(),
                SecondaryCpichTarget::new(10862, 12).unwrap(),
            )),
            ..Default::default()
        };
        reports
            .admit(
                SignallingBearer::ControlAm,
                &ULDCCHMessageType::measurementControlFailure(MeasurementControlFailure::new(
                    RRCTransactionIdentifier(2),
                    FailureCauseWithProtErr::unsupportedMeasurement(()),
                    None,
                )),
            )
            .unwrap();
        assert!(reports.measurement_control_configured);
        assert_eq!(reports.measurement_control_failures, 0);
        assert_eq!(reports.secondary_measurement_failures, 1);
        assert!(!reports.secondary_measurement_configured);
    }

    #[test]
    fn secondary_measurement_failure_is_scoped_to_radio_configuration() {
        use std::num::NonZeroU64;
        let identity = crate::dedicated_rx::ReceiveIdentity {
            ue: umts_types::UeKey {
                epoch: crate::test_support::at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::MIN,
        };
        let target = umts_rrc::measurement::SecondaryCpichTarget::new(10862, 12).unwrap();
        let mut reports = Reports::default();
        reports.refresh_secondary_scope(Some((identity, target)));
        reports.secondary_requested = Some((
            umts_rrc::establishment::TransactionId::new(2).unwrap(),
            target,
        ));
        reports.secondary_measurement_failures = 1;
        reports.refresh_secondary_scope(Some((identity, target)));
        assert!(reports.secondary_requested.is_some());
        assert_eq!(reports.secondary_measurement_failures, 1);
        let next = crate::dedicated_rx::ReceiveIdentity {
            configuration: NonZeroU64::new(2).unwrap(),
            ..identity
        };
        reports.refresh_secondary_scope(Some((next, target)));
        assert!(reports.secondary_requested.is_none());
        assert_eq!(reports.secondary_measurement_failures, 0);
        reports.secondary_measurement_configured = true;
        reports.refresh_secondary_scope(None);
        assert!(!reports.secondary_measurement_configured);
    }

    #[test]
    fn secondary_report_requires_requested_frequency_and_scrambling_code() {
        use umts_rrc_schema::information_elements as ie;
        let mut reports = Reports {
            secondary_requested: Some((
                umts_rrc::establishment::TransactionId::new(2).unwrap(),
                umts_rrc::measurement::SecondaryCpichTarget::new(10862, 12).unwrap(),
            )),
            ..Default::default()
        };
        let message = |uarfcn, psc| {
            let cell = ie::CellMeasuredResults::new(
                None,
                None,
                None,
                ie::CellMeasuredResultsModeSpecificInfo::fdd(
                    ie::CellMeasuredResultsModeSpecificInfoFdd::new(
                        ie::PrimaryCPICHInfo::new(ie::PrimaryScramblingCode(psc)),
                        Some(ie::CPICHEcN0(32)),
                        Some(ie::CPICHRSCP(71)),
                        None,
                    ),
                ),
            );
            let frequency = ie::InterFreqMeasuredResults::new(
                Some(ie::FrequencyInfo::new(
                    ie::FrequencyInfoModeSpecificInfo::fdd(ie::FrequencyInfoFDD::new(
                        None,
                        ie::UARFCN(uarfcn),
                    )),
                )),
                None,
                Some(ie::InterFreqCellMeasuredResultsList(vec![cell])),
            );
            ULDCCHMessageType::measurementReport(MeasurementReport::new(
                ie::MeasurementIdentity(umts_rrc::measurement::SECONDARY_CPICH_MEASUREMENT_ID),
                Some(ie::MeasuredResults::interFreqMeasuredResultsList(
                    ie::InterFreqMeasuredResultsList(vec![frequency]),
                )),
                None,
                None,
                None,
                None,
            ))
        };
        reports
            .admit(SignallingBearer::ControlAm, &message(10837, 12))
            .unwrap();
        reports
            .admit(SignallingBearer::ControlAm, &message(10862, 13))
            .unwrap();
        assert!(!reports.secondary_measurement_configured);
        reports
            .admit(SignallingBearer::ControlAm, &message(10862, 12))
            .unwrap();
        assert!(reports.secondary_measurement_configured);
        assert!(reports.last_secondary_measurement.is_some());
    }

    fn status() -> ULDCCHMessageType {
        ULDCCHMessageType::rrcStatus(RRCStatus::new(
            ProtocolErrorMoreInformation::new(ProtocolErrorMoreInformationDiagnosticsType::type1(
                ProtocolErrorMoreInformationDiagnosticsTypeType1::messageTypeNonexistent(()),
            )),
            None,
        ))
    }

    #[test]
    fn wrong_bearer_leaves_report_state_unchanged() {
        let mut reports = Reports::default();
        assert!(
            reports
                .admit(SignallingBearer::Unacknowledged, &status())
                .is_err()
        );
        assert_eq!(reports.status_count, 0);
        assert_eq!(reports.last_status, None);
        assert_eq!(
            reports.admit(SignallingBearer::ControlAm, &status()),
            Ok(true)
        );
        assert_eq!(reports.status_count, 1);
        let measurement = ULDCCHMessageType::measurementReport(MeasurementReport::new(
            MeasurementIdentity(1),
            None,
            None,
            None,
            None,
            None,
        ));
        for bearer in [
            SignallingBearer::Unacknowledged,
            SignallingBearer::ControlAm,
        ] {
            assert_eq!(reports.admit(bearer, &measurement), Ok(true));
        }
        assert_eq!(reports.measurement_count, 2);
    }
}
