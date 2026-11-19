// SPDX-License-Identifier: AGPL-3.0-or-later

use super::ServingRuntime;
use crate::{dedicated_rx::ReceiveIdentity, epoch::EpochPhase, signalling::UeOverview};
use umts_core::Transport;
use umts_management::{bridge::Snapshot, proto};
use umts_rrc::{dedicated::Rel99Rlc, establishment::EstablishmentPhase};
use umts_types::{CnDomain, UeKey};
use umts_user_plane::packet::BoundDatagrams;

fn reference(key: UeKey) -> proto::UeRef {
    proto::UeRef {
        epoch: key.epoch.get(),
        slot: key.slot,
        generation: key.generation.get(),
    }
}

fn domain(connected: bool) -> i32 {
    if connected {
        proto::DomainState::Connected
    } else {
        proto::DomainState::Disconnected
    }
    .into()
}

fn ue_overview(value: UeOverview) -> proto::Ue {
    let rrc_state = if value.releasing {
        proto::RrcState::Releasing
    } else {
        match value.establishment {
            EstablishmentPhase::Prepared | EstablishmentPhase::AwaitingComplete => {
                proto::RrcState::Connecting
            }
            EstablishmentPhase::Established if value.dedicated.is_some() => {
                proto::RrcState::CellDch
            }
            EstablishmentPhase::Established => proto::RrcState::CellFach,
            EstablishmentPhase::Closed => proto::RrcState::Releasing,
        }
    };
    proto::Ue {
        rrc_reports: Some(rrc_reports(value.reports)),
        reference: Some(reference(value.key)),
        imsi: value.imsi.map(|imsi| {
            (0..imsi.digit_count())
                .map(|at| char::from(b'0' + imsi.digit(at).expect("validated IMSI")))
                .collect()
        }),
        rrc_state: rrc_state.into(),
        cs_state: domain(value.cs_connected),
        ps_state: domain(value.ps_connected),
        configuration: value
            .dedicated
            .map_or(0, |identity| identity.configuration.get()),
        pending_procedure: if value.bearer_change_pending {
            "bearer.change".into()
        } else {
            String::new()
        },
        ..Default::default()
    }
}

fn rrc_reports(reports: crate::signalling::Reports) -> proto::RrcReports {
    use umts_rrc::reports::ProtocolError as Error;
    proto::RrcReports {
        status_count: reports.status_count,
        measurement_count: reports.measurement_count,
        last_status: reports.last_status.map(|status| proto::RrcStatusReport {
            cause: match status.cause {
                Error::Encoding => proto::RrcProtocolError::Encoding,
                Error::MessageNotImplemented => proto::RrcProtocolError::MessageNotImplemented,
                Error::ReceiverState => proto::RrcProtocolError::ReceiverState,
                Error::ValueNotComprehended => proto::RrcProtocolError::ValueNotComprehended,
                Error::ConditionalInformationElement => {
                    proto::RrcProtocolError::ConditionalInformationElement
                }
                Error::ExtensionNotComprehended => {
                    proto::RrcProtocolError::ExtensionNotComprehended
                }
            }
            .into(),
            rejected_message_type: status.rejected.map(|message| message.kind as u32),
            transaction: status
                .rejected
                .map(|message| u32::from(message.transaction)),
        }),
        last_measurement: reports.last_measurement.map(measurement_report),
        last_secondary_measurement: reports.last_secondary_measurement.map(measurement_report),
        secondary_measurement_configured: reports.secondary_measurement_configured,
        secondary_measurement_failures: reports.secondary_measurement_failures,
        measurement_control_configured: reports.measurement_control_configured,
        measurement_control_failures: reports.measurement_control_failures,
        unsupported_control_count: reports.unsupported_control_count,
    }
}

fn measurement_report(report: umts_rrc::reports::MeasurementSummary) -> proto::MeasurementReport {
    use umts_rrc::reports::MeasurementKind as Kind;
    proto::MeasurementReport {
        identity: u32::from(report.identity),
        kind: match report.kind {
            Kind::Absent => proto::MeasurementKind::Absent,
            Kind::IntraFrequency => proto::MeasurementKind::IntraFrequency,
            Kind::InterFrequency => proto::MeasurementKind::InterFrequency,
            Kind::InterRat => proto::MeasurementKind::InterRat,
            Kind::TrafficVolume => proto::MeasurementKind::TrafficVolume,
            Kind::Quality => proto::MeasurementKind::Quality,
            Kind::Internal => proto::MeasurementKind::Internal,
            Kind::Positioning => proto::MeasurementKind::Positioning,
        }
        .into(),
        primary_intra_frequency_cells: report
            .cells
            .into_iter()
            .flatten()
            .map(|cell| proto::FddCellMeasurement {
                scrambling_code: u32::from(cell.scrambling_code),
                rscp_code: cell.rscp_code.map(u32::from),
                ec_n0_code: cell.ec_n0_code.map(u32::from),
                pathloss_code: cell.pathloss_code.map(u32::from),
            })
            .collect(),
        inter_frequency_cells: report
            .inter_frequency_cells
            .into_iter()
            .flatten()
            .map(|entry| proto::InterFrequencyCellMeasurement {
                uarfcn_dl: entry.uarfcn.map(u32::from),
                cell: Some(proto::FddCellMeasurement {
                    scrambling_code: u32::from(entry.cell.scrambling_code),
                    rscp_code: entry.cell.rscp_code.map(u32::from),
                    ec_n0_code: entry.cell.ec_n0_code.map(u32::from),
                    pathloss_code: entry.cell.pathloss_code.map(u32::from),
                }),
            })
            .collect(),
        other_cell_count: u32::from(report.other_cell_count),
        has_event: report.has_event,
        has_rach_results: report.has_rach_results,
        has_additional_results: report.has_additional_results,
        has_extensions: report.has_extensions,
    }
}

fn find_ue(ues: &mut [proto::Ue], identity: ReceiveIdentity) -> Option<&mut proto::Ue> {
    let expected = reference(identity.ue);
    ues.iter_mut().find(|ue| {
        ue.reference == Some(expected) && ue.configuration == identity.configuration.get()
    })
}

pub(crate) fn counter(name: &str, value: u64) -> proto::Counter {
    proto::Counter {
        name: name.into(),
        value,
    }
}

fn append_hsdpa_carrier_progress(
    counters: &mut proto::Counters,
    carrier: umts_types::hsdpa::CarrierId,
    airtime: crate::hsdpa_session::AirtimeSummary,
    queued_pdus: usize,
    busy_processes: usize,
    selection: crate::hsdpa_session::SelectionCounts,
) {
    let values = [
        ("hsdpa.acknowledged_mac_bits", airtime.acknowledged_bits),
        ("hsdpa.qam64_new_blocks", airtime.qam64_new_blocks),
        (
            "hsdpa.qam64_acknowledged_bits",
            airtime.qam64_acknowledged_bits,
        ),
        ("hsdpa.busy_processes", busy_processes as u64),
        (
            "hsdpa.maximum_new_transport_bits",
            airtime.maximum_new_transport_bits,
        ),
        ("hsdpa.lead_chips", airtime.lead_chips),
        ("hsdpa.lead_count", airtime.lead_count),
        ("hsdpa.reuse_chips", airtime.reuse_chips),
        ("hsdpa.reuse_count", airtime.reuse_count),
        ("hsdpa.selection.calls", selection.calls),
        ("hsdpa.selection.selected", selection.selected),
        ("hsdpa.selection.no_fresh_cqi", selection.no_fresh_cqi),
        (
            "hsdpa.selection.empty_packet_queues",
            selection.empty_packet_queues,
        ),
        (
            "hsdpa.selection.no_available_process",
            selection.no_available_process,
        ),
        (
            "hsdpa.selection.budget_exhausted",
            selection.budget_exhausted,
        ),
        (
            "hsdpa.selection.calendar_capacity",
            selection.calendar_capacity,
        ),
        ("hsdpa.selection.no_format", selection.no_format),
    ];
    for (name, value) in values {
        if let Some(previous) = counters.values.iter_mut().find(|entry| entry.name == name) {
            previous.value = if name == "hsdpa.maximum_new_transport_bits" {
                previous.value.max(value)
            } else {
                previous.value.saturating_add(value)
            };
        } else {
            counters.values.push(counter(name, value));
        }
    }
    // Both links report one shared queue, so count it once.
    if carrier == umts_types::hsdpa::CarrierId::Primary {
        counters
            .values
            .push(counter("hsdpa.queued_pdus", queued_pdus as u64));
    }
}

fn append_hsupa_scheduling(
    counters: &mut proto::Counters,
    ingress: &crate::hsupa_rx::IngressCounters,
) {
    counters.values.push(counter(
        "hsupa.scheduling_reports",
        ingress.scheduling_reports,
    ));
    if let Some(report) = ingress.last_scheduling {
        counters.values.extend([
            counter("hsupa.scheduling.start_chip", report.start.index),
            counter(
                "hsupa.scheduling.power_headroom_index",
                u64::from(report.information.power_headroom_index),
            ),
            counter(
                "hsupa.scheduling.total_buffer_index",
                u64::from(report.information.total_buffer_index),
            ),
            counter(
                "hsupa.scheduling.priority_buffer_index",
                u64::from(report.information.priority_buffer_index),
            ),
            counter(
                "hsupa.scheduling.highest_priority_logical_channel",
                u64::from(report.information.highest_priority_logical_channel),
            ),
        ]);
    }
}

pub fn receive_metrics(ue: &mut proto::Ue, status: &crate::dedicated_rx::ReceivePlanStatus) {
    let values = &mut ue.counters.get_or_insert_default().values;
    append_receive_counters(values, &status.transports);
    for (name, value) in [
        (
            "rx.dch.hsupa_dropped_intervals",
            status.hsupa_dropped_intervals,
        ),
        (
            "downlink_power.feedback_dropped",
            status.samples.downlink_feedback_dropped,
        ),
    ] {
        if let Some(previous) = values.iter_mut().find(|entry| entry.name == name) {
            previous.value = value;
        } else {
            values.push(counter(name, value));
        }
    }
    let mut power = proto::PowerControl {
        target_sir_db: status.outer_loop.as_ref().map(|outer| outer.target.db()),
        tracking_losses: status.samples.tracking_losses,
        ..Default::default()
    };
    if let Some(control) = &status.power_control {
        power.sir_db = control
            .last_sir_linear
            .filter(|sir| sir.is_finite() && *sir > 0.0)
            .map(|sir| 10.0 * sir.log10());
        if let Some((at, control)) = control.last_observed_control {
            let pilot_power = control.pilot.channel.norm_sqr();
            power.despread_pilot_dbfs =
                (pilot_power.is_finite() && pilot_power > 0.0).then(|| 10.0 * pilot_power.log10());
            if control.pilot.observed_power.is_finite() && control.pilot.observed_power > 0.0 {
                power.pilot_coherence =
                    Some((pilot_power / control.pilot.observed_power).clamp(0.0, 1.0));
            }
            power.observed_chip = at.index;
        }
    }
    ue.power = Some(power);
}

// These are cumulative transport outcomes for this receive configuration, not
// counts of payload frames or exclusively CRC-checked successes. In particular,
// `decoded` also includes transports configured without a CRC.
fn append_receive_counters(
    values: &mut Vec<proto::Counter>,
    rx: &crate::dedicated_rx::ReceiveCounters,
) {
    for (name, value) in [
        ("rx.dch.frames", rx.frames),
        ("rx.dch.missing_frames", rx.missing_frames),
        ("rx.dch.interrupted_ttis", rx.interrupted_ttis),
        ("rx.dch.skipped_ttis", rx.skipped_ttis),
        ("rx.dch.decoded", rx.decoded),
        ("rx.dch.absent", rx.absent),
        ("rx.dch.erased", rx.erased),
        ("rx.dch.failed_crc", rx.failed_crc),
        ("rx.dch.invalid_filler", rx.invalid_filler),
        ("rx.dch.delivered", rx.delivered),
        ("rx.dch.retired_results", rx.retired_results),
        ("rx.dch.retired_completed_ttis", rx.retired_completed_ttis),
    ] {
        if let Some(previous) = values.iter_mut().find(|entry| entry.name == name) {
            previous.value = value;
        } else {
            values.push(counter(name, value));
        }
    }
}

impl<T: Transport, D: BoundDatagrams> ServingRuntime<T, D> {
    pub fn management_snapshot(&mut self) -> Snapshot {
        let telemetry = self.epoch.telemetry();
        let air = self.air_origin();
        let phase = match self.phase() {
            EpochPhase::Starting => proto::Phase::Starting,
            EpochPhase::Running => proto::Phase::Running,
            EpochPhase::Stopping => proto::Phase::Stopping,
            EpochPhase::Stopped => proto::Phase::Stopped,
            EpochPhase::Failed => proto::Phase::Failed,
        };
        let mut snapshot = Snapshot {
            status: proto::StatusSnapshot {
                identity: Some(proto::SnapshotIdentity {
                    epoch: air.chip.epoch.get(),
                    ..Default::default()
                }),
                phase: phase.into(),
                core_connected: self.core_phase() == Some(umts_core::Phase::Serving),
                radio_ready: self.phase() == EpochPhase::Running,
                registered_ues: u32::try_from(self.registered_ues()).unwrap_or(u32::MAX),
                ..Default::default()
            },
            configuration: proto::Configuration {
                device_tick_rate_hz: Some(air.tick_rate.get()),
                ..Default::default()
            },
            ..Default::default()
        };
        if let Some(controller) = self.epoch.controller() {
            controller.radio().visit_ue_overviews(|overview| {
                let mut ue = ue_overview(overview);
                if let Some(identity) = overview.dedicated
                    && let Some(admission) = controller.admission()
                    && let Ok(active) = admission.committed_radio(identity)
                {
                    let configuration = active.plan().configuration();
                    ue.bearer_configuration_available = true;
                    let physical = configuration.physical();
                    ue.downlink_scrambling_code = Some(physical.primary_scrambling.into());
                    ue.downlink_spreading_factor = Some(physical.downlink_sf.into());
                    ue.downlink_channel_code =
                        physical.downlink_codes.first().copied().map(u32::from);
                    ue.uplink_scrambling_code = Some(physical.uplink_scrambling);
                    ue.bearers = configuration
                        .bearers()
                        .iter()
                        .map(|bearer| proto::Bearer {
                            id: bearer.id.into(),
                            domain: match bearer.domain {
                                None => proto::BearerDomain::Signalling,
                                Some(CnDomain::Cs) => proto::BearerDomain::Cs,
                                Some(CnDomain::Ps) => proto::BearerDomain::Ps,
                            }
                            .into(),
                            rlc_mode: match bearer.rlc {
                                Rel99Rlc::Transparent { .. } => proto::RlcMode::Tm,
                                Rel99Rlc::Unacknowledged => proto::RlcMode::Um,
                                Rel99Rlc::Acknowledged { .. } => proto::RlcMode::Am,
                            }
                            .into(),
                            state: proto::BearerState::Active.into(),
                            ..Default::default()
                        })
                        .collect();
                }
                snapshot.ues.push(ue);
            });
        }
        self.bearers.inactivity.visit(|key, idle_chips, blocker| {
            let expected = reference(key);
            if let Some(ue) = snapshot
                .ues
                .iter_mut()
                .find(|ue| ue.reference == Some(expected))
            {
                let counters = &mut ue.counters.get_or_insert_default().values;
                counters.push(counter("inactivity.idle_chips", idle_chips));
                if let Some(blocker) = blocker {
                    counters.push(counter(blocker.counter_name(), 1));
                }
            }
        });
        self.visit_downlink_power(|identity, power| {
            if let Some(ue) = find_ue(&mut snapshot.ues, identity) {
                ue.counters.get_or_insert_default().values.extend([
                    counter("downlink_power.up", power.up),
                    counter("downlink_power.down", power.down),
                    counter("downlink_power.hold", power.hold),
                    counter("downlink_power.clamped", power.clamped),
                    counter("downlink_power.stale", power.stale),
                    counter(
                        "downlink_power.attenuation_db",
                        u64::from(power.attenuation_db),
                    ),
                    counter(
                        "downlink_power.applied_attenuation_db",
                        u64::from(power.applied_attenuation_db),
                    ),
                    counter(
                        "downlink_power.maximum_measurement_to_preparation_chips",
                        power.maximum_measurement_to_preparation_chips,
                    ),
                ]);
            }
        });
        let hs_codes = self
            .hsdpa_codes()
            .map(|codes| u32::from(codes.data_count().get()));
        self.visit_hsdpa_diagnostics(|identity, phase, feedback| {
            if let Some(ue) = find_ue(&mut snapshot.ues, identity) {
                ue.downlink_feedback = Some(proto::Harq {
                    ack: feedback.outcomes.ack,
                    nack: feedback.outcomes.nack,
                    dtx: feedback.outcomes.dtx,
                    missing: feedback.outcomes.missing,
                    rejected: feedback.outcomes.rejected,
                });
                ue.cqi = feedback.cqi.and_then(|report| match report.result {
                    Ok(crate::hsdpa_rx::Measurement::Cqi(crate::hsdpa_rx::Evidence::Measured(
                        observation,
                    ))) => observation
                        .evidence
                        .candidate
                        .map(|cqi| u32::from(cqi.value())),
                    _ => None,
                });
                if phase == crate::hsdpa_session::Phase::Active {
                    ue.hs_pdsch_codes = hs_codes;
                    for bearer in &mut ue.bearers {
                        bearer.hsdpa = bearer.domain == i32::from(proto::BearerDomain::Ps);
                    }
                }
            }
        });
        if let Some(controller) = self.epoch.controller() {
            let secondary = self.bearers.secondary_hsdpa();
            controller.visit_hsdpa_carrier_diagnostics(
                |identity, phase, carrier, feedback, secondary_state| {
                    use umts_types::hsdpa::CarrierId;
                    let Some(ue) = find_ue(&mut snapshot.ues, identity) else {
                        return;
                    };
                    let airtime = feedback.formats.airtime_summary();
                    let cqi = feedback.cqi.and_then(|report| match report.result {
                        Ok(crate::hsdpa_rx::Measurement::Cqi(
                            crate::hsdpa_rx::Evidence::Measured(observation),
                        )) => observation
                            .evidence
                            .candidate
                            .map(|cqi| u32::from(cqi.value())),
                        Ok(crate::hsdpa_rx::Measurement::CqiCells(
                            crate::hsdpa_rx::Evidence::Measured(observation),
                        )) => observation
                            .evidence
                            .candidate
                            .and_then(|cqis| cqis.cells()[carrier.index()])
                            .map(|cqi| u32::from(cqi.value())),
                        _ => None,
                    });
                    if carrier == CarrierId::Primary {
                        ue.cqi = cqi;
                    }
                    let (role, scrambling, uarfcn, codes) = match carrier {
                        CarrierId::Primary => (
                            proto::HsdpaCarrierRole::Primary,
                            ue.downlink_scrambling_code,
                            None,
                            hs_codes,
                        ),
                        CarrierId::Secondary => (
                            proto::HsdpaCarrierRole::Secondary,
                            secondary.map(|value| u32::from(value.primary.index())),
                            secondary.map(|value| u32::from(value.uarfcn_dl)),
                            secondary.map(|value| u32::from(value.codes.data_count().get())),
                        ),
                    };
                    let state = match phase {
                        crate::hsdpa_session::Phase::RecoveryRequired => {
                            proto::HsdpaCarrierState::RecoveryRequired
                        }
                        crate::hsdpa_session::Phase::Active if carrier == CarrierId::Primary => {
                            proto::HsdpaCarrierState::Active
                        }
                        crate::hsdpa_session::Phase::Active => match secondary_state {
                            Some(crate::hsdpa_session::SecondaryState::Configured) => {
                                proto::HsdpaCarrierState::Configured
                            }
                            Some(crate::hsdpa_session::SecondaryState::Activating) => {
                                proto::HsdpaCarrierState::Activating
                            }
                            Some(crate::hsdpa_session::SecondaryState::Active) => {
                                proto::HsdpaCarrierState::Active
                            }
                            Some(crate::hsdpa_session::SecondaryState::Deactivating) => {
                                proto::HsdpaCarrierState::Deactivating
                            }
                            None => proto::HsdpaCarrierState::Unspecified,
                        },
                        crate::hsdpa_session::Phase::Retiring => {
                            proto::HsdpaCarrierState::Deactivating
                        }
                        _ => proto::HsdpaCarrierState::Configured,
                    };
                    ue.hsdpa_carriers.push(proto::HsdpaCarrier {
                        role: role.into(),
                        state: state.into(),
                        downlink_scrambling_code: scrambling,
                        uarfcn_dl: uarfcn,
                        cqi,
                        hs_pdsch_codes: codes,
                        feedback: Some(proto::Harq {
                            ack: feedback.outcomes.ack,
                            nack: feedback.outcomes.nack,
                            dtx: feedback.outcomes.dtx,
                            missing: feedback.outcomes.missing,
                            rejected: feedback.outcomes.rejected,
                        }),
                        maximum_new_transport_bits: airtime.maximum_new_transport_bits,
                        new_transport_bits: airtime.new_bits,
                        retry_transport_bits: airtime.retry_bits,
                        acknowledged_transport_bits: airtime.acknowledged_bits,
                    });
                },
            );
        }
        for ue in &mut snapshot.ues {
            if !ue.hsdpa_carriers.is_empty() {
                let mut total = proto::Harq::default();
                for carrier in &ue.hsdpa_carriers {
                    if let Some(feedback) = &carrier.feedback {
                        total.ack = total.ack.saturating_add(feedback.ack);
                        total.nack = total.nack.saturating_add(feedback.nack);
                        total.dtx = total.dtx.saturating_add(feedback.dtx);
                        total.missing = total.missing.saturating_add(feedback.missing);
                        total.rejected = total.rejected.saturating_add(feedback.rejected);
                    }
                }
                ue.downlink_feedback = Some(total);
            }
        }
        if let Some(controller) = self.epoch.controller() {
            controller.visit_hsdpa_carrier_progress(|identity, carrier, progress| {
                if let Some(ue) = find_ue(&mut snapshot.ues, identity) {
                    append_hsdpa_carrier_progress(
                        ue.counters.get_or_insert_default(),
                        carrier,
                        progress.airtime,
                        progress.queued_pdus,
                        progress.busy_processes,
                        progress.selection,
                    );
                }
            });
        }
        self.visit_hsupa_candidates(|identity, _, _, _, _, phase, ingress, _| {
            if let Some(ue) = find_ue(&mut snapshot.ues, identity) {
                if phase == Some(crate::hsupa_session::Phase::Active) {
                    for bearer in &mut ue.bearers {
                        bearer.hsupa = bearer.domain == i32::from(proto::BearerDomain::Ps);
                    }
                }
                if let Some(ingress) = ingress {
                    append_hsupa_scheduling(ue.counters.get_or_insert_default(), &ingress);
                    ue.counters.get_or_insert_default().values.extend([
                        counter(
                            "hsupa.decoded_mac_payload_bits",
                            ingress.decoded_payload_bits,
                        ),
                        counter("hsupa.decoded", ingress.decoded),
                        counter("hsupa.invalid_crc", ingress.invalid_crc),
                        counter("hsupa.erased", ingress.erased),
                        counter("hsupa.feedback_late", ingress.feedback_late),
                        counter("hsupa.feedback_lost", ingress.feedback_lost),
                    ]);
                }
            }
        });
        snapshot.status.active_bearers = snapshot
            .ues
            .iter()
            .flat_map(|ue| &ue.bearers)
            .filter(|bearer| bearer.domain != i32::from(proto::BearerDomain::Signalling))
            .count() as u32;
        snapshot.status.bearer_snapshot_complete = snapshot
            .ues
            .iter()
            .all(|ue| ue.configuration == 0 || ue.bearer_configuration_available);
        let counts = self.counters();
        snapshot.radio_counters.values = vec![
            counter(
                "inactivity.iu_requests",
                self.bearers.inactivity.counters.iu_requests,
            ),
            counter(
                "inactivity.rrc_releases",
                self.bearers.inactivity.counters.rrc_releases,
            ),
            counter(
                "inactivity.completed",
                self.bearers.inactivity.counters.completed,
            ),
            counter(
                "inactivity.timed_out",
                self.bearers.inactivity.counters.timed_out,
            ),
            counter("inactivity.failed", self.bearers.inactivity.counters.failed),
            counter("rach.transports", counts.rach_transports),
            counter("rrc.connection_requests", counts.connection_requests),
            counter("rrc.common_rejected", counts.rejected_common),
            counter("core.failed", counts.failed_core),
            counter("core.rejected", counts.rejected_core),
            counter("control.failed", counts.failed_control),
            counter("dedicated.failed", counts.failed_dedicated),
            counter("admission.failed", counts.failed_admission),
            counter("user_plane.failed", counts.failed_user_plane),
            counter("rab.accepted", counts.accepted_assignments),
            counter("rab.rejected", counts.rejected_assignments),
            counter("cleanup.pending", self.pending_cleanups() as u64),
            counter("radio.allocations", self.radio_allocations() as u64),
        ];
        append_telemetry(&mut snapshot.radio_counters, telemetry);
        snapshot.status.counters = Some(snapshot.radio_counters.clone());
        snapshot
    }
}

fn nanos(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn wall(
    counters: &mut proto::Counters,
    prefix: &str,
    summary: Option<crate::radio_io::WallTimeSummary>,
) {
    if let Some(summary) = summary {
        counters.values.extend([
            counter(&format!("{prefix}.calls"), summary.calls),
            counter(&format!("{prefix}.total_wall_ns"), nanos(summary.total_ns)),
            counter(
                &format!("{prefix}.maximum_wall_ns"),
                nanos(summary.maximum_ns),
            ),
        ]);
    }
}

fn append_telemetry(counters: &mut proto::Counters, telemetry: crate::radio_io::telemetry::Radio) {
    if let Some(observation) = telemetry.preparation {
        counters.values.extend([
            counter(
                "tx.preparation.observation_age_ms",
                nanos(observation.observed.elapsed().as_millis()),
            ),
            counter("tx.preparation.skipped_snapshots", observation.skipped),
            counter(
                "tx.preparation.prepared_batches",
                observation.value.prepared,
            ),
        ]);
        wall(counters, "tx.synthesis", observation.value.synthesis);
    }
    if let Some(observation) = telemetry.submission {
        let value = observation.value;
        counters.values.extend([
            counter(
                "tx.submission.observation_age_ms",
                nanos(observation.observed.elapsed().as_millis()),
            ),
            counter("tx.submission.skipped_snapshots", observation.skipped),
            counter("tx.submitted_batches", value.submitted),
            counter("tx.partial_calls", value.partial),
            counter("tx.preparation_late", value.recovery.preparation_late),
            counter("tx.submission_late", value.recovery.submission_late),
            counter("tx.dropped_batches", value.recovery.dropped_batches),
            counter("tx.dropped_samples", value.recovery.dropped_samples),
            counter("tx.driver_dropped", value.recovery.driver_dropped),
            counter("tx.amplitude_dropped", value.recovery.amplitude_dropped),
            counter(
                "tx.quality_muted_batches",
                value.recovery.quality_muted_batches,
            ),
            counter(
                "tx.quality_muted_samples",
                value.recovery.quality_muted_samples,
            ),
            counter("tx.retries_exhausted", value.recovery.retries_exhausted),
            counter("tx.health_warnings", value.recovery.health_warnings),
        ]);
        if let Some(health) = value.health {
            counters.values.extend([
                counter("tx.underflows", health.underflows),
                counter("tx.late_packets", health.late_packets),
                counter("tx.sequence_errors", health.sequence_errors),
                counter("tx.dropped_packets", health.dropped_packets),
            ]);
        }
        wall(counters, "tx.hal_write", value.write);
        wall(counters, "tx.ready_queue_residence", value.ready_queue);
    }
    if let Some(observation) = telemetry.receive {
        let value = observation.value;
        counters.values.extend([
            counter(
                "rx.observation_age_ms",
                nanos(observation.observed.elapsed().as_millis()),
            ),
            counter("rx.skipped_snapshots", observation.skipped),
            counter("rx.reads", value.radio.reads),
            counter("rx.timeouts", value.radio.timeouts),
            counter("rx.input_samples", value.radio.input_samples),
            counter("rx.output_chips", value.radio.output_chips),
            counter("rx.discontinuities", value.radio.discontinuities),
            counter("rx.missing_samples", value.radio.missing_samples),
            counter("rx.reported_overflows", value.radio.reported_overflows),
            counter(
                "rx.maximum_reader_queue_residence_wall_ns",
                nanos(value.radio.maximum_reader_queue_ns),
            ),
            counter("rx.clock_queries", value.radio.clock_queries),
            counter(
                "rx.clock_query_total_wall_ns",
                nanos(value.radio.clock_query_total_ns),
            ),
            counter(
                "rx.clock_query_maximum_wall_ns",
                nanos(value.radio.clock_query_maximum_ns),
            ),
            counter("prach.searched", value.access.searched),
            counter("prach.candidates", value.access.candidates),
            counter("prach.admitted", value.access.admitted),
            counter("prach.decoded", value.access.decoded),
            counter("prach.invalid_crc", value.access.failed_crc),
            counter("prach.published", value.published),
        ]);
        if let Some(reader) = value.radio.reader {
            counters.values.extend([
                counter("rx.reader.discarded_blocks", reader.discarded_blocks),
                counter("rx.reader.discarded_samples", reader.discarded_samples),
                counter("rx.reader.stale_blocks", reader.stale_blocks),
                counter("rx.reader.stale_samples", reader.stale_samples),
                counter(
                    "rx.reader.maximum_read_wall_ns",
                    nanos(reader.maximum_read_ns),
                ),
            ]);
        }
        wall(counters, "rx.cycle", value.cycle);
        wall(counters, "rx.read_filter", value.read_filter);
        wall(counters, "rx.dedicated_feed", value.dedicated_feed);
        wall(counters, "rx.access_search", value.access_search);
        wall(counters, "rx.dedicated_service", value.dedicated_service);
        wall(counters, "rx.access_decode", value.access_decode);
        wall(counters, "rx.clock_sample", value.clock_sample);
        if let Some(cpu) = value.dedicated_cpu_ns {
            counters
                .values
                .push(counter("rx.dedicated_cpu_ns", nanos(cpu)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use umts_types::{RadioEpoch, identity::Imsi};

    #[test]
    fn receive_transport_counters_replace_previous_configuration_without_accumulating() {
        let mut values = vec![counter("unrelated", 99)];
        let observed = crate::dedicated_rx::ReceiveCounters {
            frames: 100,
            missing_frames: 2,
            interrupted_ttis: 3,
            skipped_ttis: 4,
            decoded: 50,
            absent: 6,
            erased: 7,
            failed_crc: 8,
            invalid_filler: 9,
            delivered: 45,
            retired_results: 11,
            retired_completed_ttis: 12,
        };
        append_receive_counters(&mut values, &observed);
        let expected = [100, 2, 3, 4, 50, 6, 7, 8, 9, 45, 11, 12];
        assert_eq!(values.len(), 13);
        assert_eq!(values[0].name, "unrelated");
        assert_eq!(values[0].value, 99);
        assert_eq!(
            values[1..].iter().map(|v| v.value).collect::<Vec<_>>(),
            expected
        );
        append_receive_counters(&mut values, &observed);
        assert_eq!(values.len(), 13);
        assert_eq!(values[5].value, 50);
        // A replacement receiver starts new counters. Do not retain the retired
        // configuration's failures or add repeated snapshot totals together.
        append_receive_counters(&mut values, &Default::default());
        assert_eq!(values.len(), 13);
        assert_eq!(values[0].value, 99);
        assert!(values[1..].iter().all(|v| v.value == 0));
    }

    #[test]
    fn ue_hsdpa_counters_sum_carriers_but_not_shared_queues_or_tb_sizes() {
        use crate::hsdpa_session::AirtimeSummary;
        use umts_types::hsdpa::CarrierId;
        for order in [
            [CarrierId::Primary, CarrierId::Secondary],
            [CarrierId::Secondary, CarrierId::Primary],
        ] {
            let mut counters = proto::Counters::default();
            for carrier in order {
                let factor = if carrier == CarrierId::Primary { 1 } else { 2 };
                append_hsdpa_carrier_progress(
                    &mut counters,
                    carrier,
                    AirtimeSummary {
                        acknowledged_bits: 100 * factor,
                        qam64_new_blocks: 3 * factor,
                        qam64_acknowledged_bits: 80 * factor,
                        maximum_new_transport_bits: 1000 * factor,
                        ..Default::default()
                    },
                    7,
                    factor as usize,
                    crate::hsdpa_session::SelectionCounts {
                        calls: 10 * factor,
                        no_available_process: 4 * factor,
                        ..Default::default()
                    },
                );
            }
            let value = |name| {
                counters
                    .values
                    .iter()
                    .find(|entry| entry.name == name)
                    .unwrap()
                    .value
            };
            assert_eq!(value("hsdpa.acknowledged_mac_bits"), 300);
            assert_eq!(value("hsdpa.qam64_new_blocks"), 9);
            assert_eq!(value("hsdpa.qam64_acknowledged_bits"), 240);
            assert_eq!(value("hsdpa.busy_processes"), 3);
            assert_eq!(value("hsdpa.queued_pdus"), 7);
            assert_eq!(value("hsdpa.maximum_new_transport_bits"), 2000);
            assert_eq!(value("hsdpa.selection.calls"), 30);
            assert_eq!(value("hsdpa.selection.no_available_process"), 12);
            assert_eq!(counters.values.len(), 18);
        }
        let mut counters = proto::Counters::default();
        append_hsdpa_carrier_progress(
            &mut counters,
            CarrierId::Primary,
            AirtimeSummary {
                acknowledged_bits: u64::MAX,
                ..Default::default()
            },
            7,
            1,
            crate::hsdpa_session::SelectionCounts::default(),
        );
        assert_eq!(
            counters
                .values
                .iter()
                .find(|entry| entry.name == "hsdpa.acknowledged_mac_bits")
                .unwrap()
                .value,
            u64::MAX
        );
        append_hsdpa_carrier_progress(
            &mut counters,
            CarrierId::Secondary,
            AirtimeSummary {
                acknowledged_bits: 1,
                ..Default::default()
            },
            7,
            1,
            crate::hsdpa_session::SelectionCounts::default(),
        );
        assert_eq!(
            counters
                .values
                .iter()
                .find(|entry| entry.name == "hsdpa.acknowledged_mac_bits")
                .unwrap()
                .value,
            u64::MAX
        );
    }

    #[test]
    fn scheduling_snapshot_preserves_indices_and_observation_time_without_fabricating_reports() {
        let mut counters = proto::Counters::default();
        let mut ingress = crate::hsupa_rx::IngressCounters::default();
        append_hsupa_scheduling(&mut counters, &ingress);
        assert_eq!(
            counters.values,
            vec![counter("hsupa.scheduling_reports", 0)]
        );
        ingress.scheduling_reports = 2;
        ingress.last_scheduling = Some(crate::hsupa_rx::SchedulingReport {
            start: umts_types::time::ChipInstant {
                epoch: RadioEpoch::new(NonZeroU64::new(42).unwrap()),
                index: 7680,
            },
            information: umts_mac::hsupa::scheduling::SchedulingInformation {
                power_headroom_index: 23,
                total_buffer_index: 31,
                priority_buffer_index: 15,
                highest_priority_logical_channel: 4,
            },
        });
        counters.values.clear();
        append_hsupa_scheduling(&mut counters, &ingress);
        assert_eq!(
            counters.values,
            vec![
                counter("hsupa.scheduling_reports", 2),
                counter("hsupa.scheduling.start_chip", 7680),
                counter("hsupa.scheduling.power_headroom_index", 23),
                counter("hsupa.scheduling.total_buffer_index", 31),
                counter("hsupa.scheduling.priority_buffer_index", 15),
                counter("hsupa.scheduling.highest_priority_logical_channel", 4),
            ]
        );
    }

    #[test]
    fn worker_metrics_distinguish_age_wall_time_and_optional_measurements() {
        use crate::radio_io::{
            RxCounters, TxRecoveryCounters, WallTimeSummary,
            telemetry::{Observation, Preparation, Radio, Receive, Submission},
        };
        let observed = std::time::Instant::now();
        let mut counters = proto::Counters::default();
        append_telemetry(
            &mut counters,
            Radio {
                preparation: Some(Observation {
                    observed,
                    skipped: 2,
                    value: Preparation {
                        prepared: 100,
                        synthesis: None,
                    },
                }),
                submission: Some(Observation {
                    observed,
                    skipped: 0,
                    value: Submission {
                        submitted: 90,
                        recovery: TxRecoveryCounters {
                            dropped_batches: 3,
                            ..Default::default()
                        },
                        ready_queue: Some(WallTimeSummary {
                            calls: 90,
                            total_ns: 1000,
                            maximum_ns: 25,
                        }),
                        ..Default::default()
                    },
                }),
                receive: Some(Observation {
                    observed,
                    skipped: 0,
                    value: Receive {
                        radio: RxCounters {
                            missing_samples: 7,
                            maximum_reader_queue_ns: 150,
                            ..Default::default()
                        },
                        access: crate::common_rx::AccessCounters::default(),
                        published: 5,
                        cycle: None,
                        read_filter: Some(WallTimeSummary {
                            calls: 3,
                            total_ns: 30,
                            maximum_ns: 15,
                        }),
                        dedicated_feed: None,
                        access_search: None,
                        dedicated_service: None,
                        access_decode: None,
                        clock_sample: None,
                        dedicated_cpu_ns: Some(300),
                    },
                }),
            },
        );
        let value = |name: &str| {
            counters
                .values
                .iter()
                .find(|counter| counter.name == name)
                .map(|counter| counter.value)
        };
        assert_eq!(value("tx.preparation.skipped_snapshots"), Some(2));
        assert_eq!(value("tx.submitted_batches"), Some(90));
        assert_eq!(value("tx.dropped_batches"), Some(3));
        assert_eq!(value("tx.ready_queue_residence.maximum_wall_ns"), Some(25));
        assert_eq!(value("rx.missing_samples"), Some(7));
        assert_eq!(value("rx.dedicated_cpu_ns"), Some(300));
        assert_eq!(value("rx.read_filter.calls"), Some(3));
        assert_eq!(value("rx.read_filter.total_wall_ns"), Some(30));
        assert_eq!(value("rx.read_filter.maximum_wall_ns"), Some(15));
        assert_eq!(value("rx.access_search.calls"), None);
        assert_eq!(value("tx.synthesis.maximum_wall_ns"), None);
        assert!(value("rx.observation_age_ms").is_some());
    }

    #[test]
    fn overview_preserves_full_identity_and_separates_domain_from_bearer_state() {
        let key = UeKey {
            epoch: RadioEpoch::new(NonZeroU64::new(42).unwrap()),
            slot: 3,
            generation: NonZeroU64::new(7).unwrap(),
        };
        let overview = UeOverview {
            reports: crate::signalling::Reports::default(),
            key,
            imsi: Some(Imsi::from_digits("001010000000001").unwrap()),
            establishment: EstablishmentPhase::Established,
            dedicated: None,
            cs_connected: true,
            ps_connected: false,
            bearer_change_pending: false,
            releasing: false,
        };
        let fach = ue_overview(overview);
        assert_eq!(
            fach.reference,
            Some(proto::UeRef {
                epoch: 42,
                slot: 3,
                generation: 7
            })
        );
        assert_eq!(fach.imsi.as_deref(), Some("001010000000001"));
        assert_eq!(fach.rrc_state, i32::from(proto::RrcState::CellFach));
        assert_eq!(fach.cs_state, i32::from(proto::DomainState::Connected));
        assert_eq!(fach.ps_state, i32::from(proto::DomainState::Disconnected));
        assert!(fach.bearers.is_empty());
        let dedicated = ReceiveIdentity {
            ue: key,
            configuration: NonZeroU64::new(8).unwrap(),
        };
        let dch = ue_overview(UeOverview {
            dedicated: Some(dedicated),
            bearer_change_pending: true,
            ..overview
        });
        assert_eq!(dch.rrc_state, i32::from(proto::RrcState::CellDch));
        assert_eq!(dch.configuration, 8);
        assert_eq!(dch.pending_procedure, "bearer.change");
        let mut ues = [dch];
        assert!(find_ue(&mut ues, dedicated).is_some());
        assert!(
            find_ue(
                &mut ues,
                ReceiveIdentity {
                    configuration: NonZeroU64::new(9).unwrap(),
                    ..dedicated
                }
            )
            .is_none()
        );
    }
}
