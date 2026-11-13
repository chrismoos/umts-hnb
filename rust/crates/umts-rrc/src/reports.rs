// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_schema::{information_elements as wire, pdu_definitions};

pub const MAX_REPORTED_CELLS: usize = 32;
const MAX_MEASUREMENT_ID: u8 = 16;
const FIRST_SPARE_MESSAGE: u8 = 26;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportError {
    Spare,
    Identity,
    Capacity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    Encoding,
    MessageNotImplemented,
    ReceiverState,
    ValueNotComprehended,
    ConditionalInformationElement,
    ExtensionNotComprehended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RejectedMessage {
    pub kind: wire::ReceivedMessageType,
    pub transaction: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusReport {
    pub cause: ProtocolError,
    pub rejected: Option<RejectedMessage>,
}

// TS 25.331 10.2.42 identifies the rejected message except for undecodable/unknown types.
pub fn status(value: &pdu_definitions::RRCStatus) -> Result<StatusReport, ReportError> {
    use wire::{
        ProtocolErrorMoreInformationDiagnosticsType as Diagnostic,
        ProtocolErrorMoreInformationDiagnosticsTypeType1 as Cause,
    };
    let Diagnostic::type1(cause) = &value.protocol_error_information.diagnostics_type else {
        return Err(ReportError::Spare);
    };
    let (cause, identity) = match cause {
        Cause::asn1_ViolationOrEncodingError(()) => (ProtocolError::Encoding, None),
        Cause::messageTypeNonexistent(()) => (ProtocolError::MessageNotImplemented, None),
        Cause::messageNotCompatibleWithReceiverState(id) => {
            (ProtocolError::ReceiverState, Some(id))
        }
        Cause::ie_ValueNotComprehended(id) => (ProtocolError::ValueNotComprehended, Some(id)),
        Cause::conditionalInformationElementError(id) => {
            (ProtocolError::ConditionalInformationElement, Some(id))
        }
        Cause::messageExtensionNotComprehended(id) => {
            (ProtocolError::ExtensionNotComprehended, Some(id))
        }
        Cause::spare1(()) | Cause::spare2(()) => return Err(ReportError::Spare),
    };
    let rejected = identity
        .map(|id| {
            if id.received_message_type as u8 >= FIRST_SPARE_MESSAGE {
                return Err(ReportError::Spare);
            }
            if id.rrc_transaction_identifier.0 > 3 {
                return Err(ReportError::Identity);
            }
            Ok(RejectedMessage {
                kind: id.received_message_type,
                transaction: id.rrc_transaction_identifier.0,
            })
        })
        .transpose()?;
    Ok(StatusReport { cause, rejected })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeasurementKind {
    Absent,
    IntraFrequency,
    InterFrequency,
    InterRat,
    TrafficVolume,
    Quality,
    Internal,
    Positioning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FddCellMeasurement {
    pub scrambling_code: u16,
    // TS 25.331 report codes, not point estimates in dBm/dB.
    pub rscp_code: Option<u8>,
    pub ec_n0_code: Option<u8>,
    pub pathloss_code: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeasurementSummary {
    pub identity: u8,
    pub kind: MeasurementKind,
    pub cells: [Option<FddCellMeasurement>; MAX_REPORTED_CELLS],
    pub inter_frequency_cells: [Option<InterFrequencyCellMeasurement>; MAX_REPORTED_CELLS],
    pub bler: [Option<TransportBlerMeasurement>; MAX_REPORTED_CELLS],
    pub other_cell_count: u8,
    pub has_event: bool,
    pub has_rach_results: bool,
    pub has_additional_results: bool,
    pub has_extensions: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterFrequencyCellMeasurement {
    // Missing frequency information must never be guessed to mean the secondary.
    pub uarfcn: Option<u16>,
    pub cell: FddCellMeasurement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportBlerMeasurement {
    pub transport_channel: u8,
    pub bler_code: Option<u8>,
}

pub fn measurement(
    value: &pdu_definitions::MeasurementReport,
) -> Result<MeasurementSummary, ReportError> {
    use wire::MeasuredResults as Results;
    if !(1..=MAX_MEASUREMENT_ID).contains(&value.measurement_identity.0) {
        return Err(ReportError::Identity);
    }
    let kind = match value.measured_results.as_ref() {
        None => MeasurementKind::Absent,
        Some(Results::intraFreqMeasuredResultsList(_)) => MeasurementKind::IntraFrequency,
        Some(Results::interFreqMeasuredResultsList(_)) => MeasurementKind::InterFrequency,
        Some(Results::interRATMeasuredResultsList(_)) => MeasurementKind::InterRat,
        Some(Results::trafficVolumeMeasuredResultsList(_)) => MeasurementKind::TrafficVolume,
        Some(Results::qualityMeasuredResults(_)) => MeasurementKind::Quality,
        Some(Results::ue_InternalMeasuredResults(_)) => MeasurementKind::Internal,
        Some(Results::ue_positioning_MeasuredResults(_)) => MeasurementKind::Positioning,
        Some(Results::spare(())) => return Err(ReportError::Spare),
    };
    let mut summary = MeasurementSummary {
        identity: value.measurement_identity.0,
        kind,
        cells: [None; MAX_REPORTED_CELLS],
        inter_frequency_cells: [None; MAX_REPORTED_CELLS],
        bler: [None; MAX_REPORTED_CELLS],
        other_cell_count: 0,
        has_event: value.event_results.is_some(),
        has_rach_results: value.measured_results_on_rach.is_some(),
        has_additional_results: value.additional_measured_results.is_some(),
        has_extensions: value.v390non_critical_extensions.is_some(),
    };
    if let Some(Results::intraFreqMeasuredResultsList(cells)) = &value.measured_results {
        if cells.0.len() > MAX_REPORTED_CELLS {
            return Err(ReportError::Capacity);
        }
        let mut output = summary.cells.iter_mut();
        for cell in &cells.0 {
            if let wire::CellMeasuredResultsModeSpecificInfo::fdd(cell) = &cell.mode_specific_info {
                *output.next().expect("bounded cell list") = Some(FddCellMeasurement {
                    scrambling_code: cell.primary_cpich_info.primary_scrambling_code.0,
                    rscp_code: cell.cpich_rscp.as_ref().map(|v| v.0),
                    ec_n0_code: cell.cpich_ec_n0.as_ref().map(|v| v.0),
                    pathloss_code: cell.pathloss.as_ref().map(|v| v.0),
                });
            } else {
                summary.other_cell_count += 1;
            }
        }
    }
    if let Some(Results::interFreqMeasuredResultsList(frequencies)) = &value.measured_results {
        if frequencies.0.len() > MAX_REPORTED_CELLS {
            return Err(ReportError::Capacity);
        }
        let mut count = 0;
        for frequency in &frequencies.0 {
            let uarfcn =
                frequency
                    .frequency_info
                    .as_ref()
                    .and_then(|info| match &info.mode_specific_info {
                        wire::FrequencyInfoModeSpecificInfo::fdd(fdd) => Some(fdd.uarfcn_dl.0),
                        _ => None,
                    });
            if let Some(cells) = &frequency.inter_freq_cell_measured_results_list {
                for cell in &cells.0 {
                    if count >= MAX_REPORTED_CELLS {
                        return Err(ReportError::Capacity);
                    }
                    if let wire::CellMeasuredResultsModeSpecificInfo::fdd(cell) =
                        &cell.mode_specific_info
                    {
                        summary.inter_frequency_cells[count] =
                            Some(InterFrequencyCellMeasurement {
                                uarfcn,
                                cell: FddCellMeasurement {
                                    scrambling_code: cell
                                        .primary_cpich_info
                                        .primary_scrambling_code
                                        .0,
                                    rscp_code: cell.cpich_rscp.as_ref().map(|value| value.0),
                                    ec_n0_code: cell.cpich_ec_n0.as_ref().map(|value| value.0),
                                    pathloss_code: cell.pathloss.as_ref().map(|value| value.0),
                                },
                            });
                    } else {
                        summary.other_cell_count += 1;
                    }
                    count += 1;
                }
            }
        }
    }
    if let Some(Results::qualityMeasuredResults(results)) = &value.measured_results
        && let Some(channels) = &results.bler_measurement_results_list
    {
        if channels.0.len() > MAX_REPORTED_CELLS {
            return Err(ReportError::Capacity);
        }
        for (output, channel) in summary.bler.iter_mut().zip(&channels.0) {
            *output = Some(TransportBlerMeasurement {
                transport_channel: channel.transport_channel_identity.0,
                bler_code: channel
                    .dl_transport_channel_bler
                    .as_ref()
                    .map(|value| value.0),
            });
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests;
