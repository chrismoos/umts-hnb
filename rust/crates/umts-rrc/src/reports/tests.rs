// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rrc_schema::pdu_definitions::{MeasurementReport, RRCStatus};
use wire::*;

fn report(cause: ProtocolErrorMoreInformationDiagnosticsTypeType1) -> RRCStatus {
    RRCStatus::new(
        ProtocolErrorMoreInformation::new(ProtocolErrorMoreInformationDiagnosticsType::type1(
            cause,
        )),
        None,
    )
}

#[test]
fn status_preserves_cause_message_and_transaction() {
    use ProtocolErrorMoreInformationDiagnosticsTypeType1 as Cause;
    for (build, expected) in [
        (
            Cause::messageNotCompatibleWithReceiverState
                as fn(IdentificationOfReceivedMessage) -> Cause,
            ProtocolError::ReceiverState,
        ),
        (
            Cause::ie_ValueNotComprehended,
            ProtocolError::ValueNotComprehended,
        ),
        (
            Cause::conditionalInformationElementError,
            ProtocolError::ConditionalInformationElement,
        ),
        (
            Cause::messageExtensionNotComprehended,
            ProtocolError::ExtensionNotComprehended,
        ),
    ] {
        for transaction in 0..4 {
            let value = report(build(IdentificationOfReceivedMessage::new(
                RRCTransactionIdentifier(transaction),
                ReceivedMessageType::radioBearerSetup,
            )));
            assert_eq!(
                status(&value),
                Ok(StatusReport {
                    cause: expected,
                    rejected: Some(RejectedMessage {
                        kind: ReceivedMessageType::radioBearerSetup,
                        transaction
                    }),
                })
            );
        }
    }
    assert_eq!(
        status(&report(Cause::asn1_ViolationOrEncodingError(()))),
        Ok(StatusReport {
            cause: ProtocolError::Encoding,
            rejected: None
        })
    );
    assert_eq!(
        status(&report(Cause::messageTypeNonexistent(()))),
        Ok(StatusReport {
            cause: ProtocolError::MessageNotImplemented,
            rejected: None
        })
    );
    assert_eq!(status(&report(Cause::spare1(()))), Err(ReportError::Spare));
}

#[test]
fn invalid_status_identification_is_rejected() {
    use ProtocolErrorMoreInformationDiagnosticsTypeType1 as Cause;
    for (transaction, kind, error) in [
        (
            4,
            ReceivedMessageType::securityModeCommand,
            ReportError::Identity,
        ),
        (0, ReceivedMessageType::spare1, ReportError::Spare),
    ] {
        assert_eq!(
            status(&report(Cause::ie_ValueNotComprehended(
                IdentificationOfReceivedMessage::new(RRCTransactionIdentifier(transaction), kind)
            ))),
            Err(error)
        );
    }
}

fn cell(code: u16) -> CellMeasuredResults {
    CellMeasuredResults::new(
        None,
        None,
        None,
        CellMeasuredResultsModeSpecificInfo::fdd(CellMeasuredResultsModeSpecificInfoFdd::new(
            PrimaryCPICHInfo::new(PrimaryScramblingCode(code)),
            Some(CPICHEcN0(32)),
            Some(CPICHRSCP(71)),
            Some(Pathloss(90)),
        )),
    )
}

#[test]
fn inter_frequency_results_preserve_frequency_identity_and_total_cell_bound() {
    let frequency = |uarfcn: Option<u16>, cells: Vec<CellMeasuredResults>| {
        InterFreqMeasuredResults::new(
            uarfcn.map(|value| {
                FrequencyInfo::new(FrequencyInfoModeSpecificInfo::fdd(FrequencyInfoFDD::new(
                    None,
                    UARFCN(value),
                )))
            }),
            None,
            Some(InterFreqCellMeasuredResultsList(cells)),
        )
    };
    let report = |frequencies| {
        MeasurementReport::new(
            MeasurementIdentity(14),
            Some(MeasuredResults::interFreqMeasuredResultsList(
                InterFreqMeasuredResultsList(frequencies),
            )),
            None,
            None,
            None,
            None,
        )
    };
    let value = report(vec![
        frequency(Some(10862), vec![cell(12)]),
        frequency(None, vec![cell(12)]),
    ]);
    let result = measurement(&value).unwrap();
    assert_eq!(result.kind, MeasurementKind::InterFrequency);
    assert_eq!(result.inter_frequency_cells[0].unwrap().uarfcn, Some(10862));
    assert_eq!(
        result.inter_frequency_cells[0].unwrap().cell.rscp_code,
        Some(71)
    );
    assert_eq!(result.inter_frequency_cells[1].unwrap().uarfcn, None);
    assert!(result.cells.iter().all(Option::is_none));
    assert_eq!(
        measurement(&report(vec![
            frequency(Some(10862), (0..32).map(cell).collect()),
            frequency(Some(10837), vec![cell(1)])
        ])),
        Err(ReportError::Capacity)
    );
}

#[test]
fn measurement_retains_encoded_bins_and_bounded_cells() {
    let mut value = MeasurementReport::new(
        MeasurementIdentity(1),
        Some(MeasuredResults::intraFreqMeasuredResultsList(
            IntraFreqMeasuredResultsList((0..32).map(cell).collect()),
        )),
        None,
        None,
        None,
        None,
    );
    let summary = measurement(&value).unwrap();
    assert_eq!(summary.kind, MeasurementKind::IntraFrequency);
    assert_eq!(
        summary.cells[31],
        Some(FddCellMeasurement {
            scrambling_code: 31,
            rscp_code: Some(71),
            ec_n0_code: Some(32),
            pathloss_code: Some(90),
        })
    );
    if let Some(MeasuredResults::intraFreqMeasuredResultsList(cells)) = &mut value.measured_results
    {
        cells.0.push(cell(32));
    }
    assert_eq!(measurement(&value), Err(ReportError::Capacity));
    value.measurement_identity = MeasurementIdentity(0);
    assert_eq!(measurement(&value), Err(ReportError::Identity));
}

#[test]
fn absent_base_results_do_not_invent_radio_measurements() {
    let value = MeasurementReport::new(MeasurementIdentity(16), None, None, None, None, None);
    let summary = measurement(&value).unwrap();
    assert_eq!(summary.kind, MeasurementKind::Absent);
    assert!(summary.cells.iter().all(Option::is_none));
}

#[test]
fn quality_report_preserves_bler_bins_and_absent_results() {
    let mut value = MeasurementReport::new(
        MeasurementIdentity(crate::measurement::BLER_MEASUREMENT_ID),
        Some(MeasuredResults::qualityMeasuredResults(
            QualityMeasuredResults::new(
                Some(BLERMeasurementResultsList(vec![
                    BLERMeasurementResults::new(
                        TransportChannelIdentity(1),
                        Some(DLTransportChannelBLER(63)),
                    ),
                    BLERMeasurementResults::new(TransportChannelIdentity(2), None),
                ])),
                QualityMeasuredResultsModeSpecificInfo::fdd(()),
            ),
        )),
        None,
        None,
        None,
        None,
    );
    let decoded = measurement(&value).unwrap();
    assert_eq!(decoded.kind, MeasurementKind::Quality);
    assert_eq!(
        decoded.bler[0],
        Some(TransportBlerMeasurement {
            transport_channel: 1,
            bler_code: Some(63)
        })
    );
    assert_eq!(
        decoded.bler[1],
        Some(TransportBlerMeasurement {
            transport_channel: 2,
            bler_code: None
        })
    );
    if let Some(MeasuredResults::qualityMeasuredResults(results)) = &mut value.measured_results {
        results.bler_measurement_results_list = Some(BLERMeasurementResultsList(vec![
                BLERMeasurementResults::new(
                    TransportChannelIdentity(1),
                    None
                );
                33
            ]));
    }
    assert_eq!(measurement(&value), Err(ReportError::Capacity));
}
