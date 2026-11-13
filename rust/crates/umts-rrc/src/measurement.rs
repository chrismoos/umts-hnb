// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::establishment::TransactionId;
use umts_rrc_schema::{
    class_definitions::{DLDCCHMessage, DLDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};

pub const CPICH_MEASUREMENT_ID: u8 = 16;
pub const CPICH_REPORTS: u8 = 32;
pub const BLER_MEASUREMENT_ID: u8 = 15;
pub const SECONDARY_CPICH_MEASUREMENT_ID: u8 = 14;
mod secondary;
pub use secondary::{SecondaryCpichTarget, secondary_cpich_setup};

#[derive(Clone, Debug)]
pub struct PeriodicalReporting {
    criteria: ie::PeriodicalReportingCriteria,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportingError {
    Count,
    Interval,
}

impl Default for PeriodicalReporting {
    fn default() -> Self {
        Self {
            criteria: ie::PeriodicalReportingCriteria::new(
                ie::ReportingAmount::ra32,
                ie::ReportingIntervalLong::ril1,
            ),
        }
    }
}

impl PeriodicalReporting {
    // TS 25.331 ReportingAmount and ReportingIntervalLong exclude arbitrary counts/cadences.
    pub fn new(count: Option<u8>, interval_ms: u32) -> Result<Self, ReportingError> {
        let amount = match count {
            None => ie::ReportingAmount::ra_Infinity,
            Some(1) => ie::ReportingAmount::ra1,
            Some(2) => ie::ReportingAmount::ra2,
            Some(4) => ie::ReportingAmount::ra4,
            Some(8) => ie::ReportingAmount::ra8,
            Some(16) => ie::ReportingAmount::ra16,
            Some(32) => ie::ReportingAmount::ra32,
            Some(64) => ie::ReportingAmount::ra64,
            Some(_) => return Err(ReportingError::Count),
        };
        let interval = match interval_ms {
            250 => ie::ReportingIntervalLong::ril0_25,
            500 => ie::ReportingIntervalLong::ril0_5,
            1000 => ie::ReportingIntervalLong::ril1,
            2000 => ie::ReportingIntervalLong::ril2,
            3000 => ie::ReportingIntervalLong::ril3,
            4000 => ie::ReportingIntervalLong::ril4,
            6000 => ie::ReportingIntervalLong::ril6,
            8000 => ie::ReportingIntervalLong::ril8,
            12000 => ie::ReportingIntervalLong::ril12,
            16000 => ie::ReportingIntervalLong::ril16,
            20000 => ie::ReportingIntervalLong::ril20,
            24000 => ie::ReportingIntervalLong::ril24,
            28000 => ie::ReportingIntervalLong::ril28,
            32000 => ie::ReportingIntervalLong::ril32,
            64000 => ie::ReportingIntervalLong::ril64,
            _ => return Err(ReportingError::Interval),
        };
        Ok(Self {
            criteria: ie::PeriodicalReportingCriteria::new(amount, interval),
        })
    }
}

pub fn bler_setup(transaction: TransactionId, reporting: &PeriodicalReporting) -> DLDCCHMessage {
    // TS 25.331 8.6.7.17: no channel list requests all eligible downlink channels.
    let measurement = ie::QualityMeasurement::new(
        Some(ie::QualityReportingQuantity::new(
            true,
            None,
            ie::QualityReportingQuantityModeSpecificInfo::fdd(()),
        )),
        ie::QualityReportCriteria::periodicalReportingCriteria(reporting.criteria.clone()),
    );
    DLDCCHMessage::new(
        None,
        DLDCCHMessageType::measurementControl(pdu::MeasurementControl::r3(
            pdu::MeasurementControlR3::new(
                pdu::MeasurementControlR3IEs::new(
                    ie::RRCTransactionIdentifier(transaction.get()),
                    ie::MeasurementIdentity(BLER_MEASUREMENT_ID),
                    ie::MeasurementCommand::setup(ie::MeasurementType::qualityMeasurement(
                        measurement,
                    )),
                    Some(ie::MeasurementReportingMode::new(
                        ie::TransferMode::acknowledgedModeRLC,
                        ie::PeriodicalOrEventTrigger::periodical,
                    )),
                    None,
                    None,
                ),
                None,
            ),
        )),
    )
}

// TS 25.331 8.4.1 requires CELL_DCH for this intra-frequency measurement setup.
pub fn cpich_setup(transaction: TransactionId, reporting: &PeriodicalReporting) -> DLDCCHMessage {
    let quantities = |enabled| {
        ie::CellReportingQuantities::new(
            ie::SFNSFNOTDType::noReport,
            false,
            false,
            ie::CellReportingQuantitiesModeSpecificInfo::fdd(
                ie::CellReportingQuantitiesModeSpecificInfoFdd::new(enabled, enabled, false),
            ),
        )
    };
    let measurement = ie::IntraFrequencyMeasurement::new(
        None,
        Some(ie::IntraFreqMeasQuantity::new(
            ie::FilterCoefficient::fc0,
            ie::IntraFreqMeasQuantityModeSpecificInfo::fdd(
                ie::IntraFreqMeasQuantityModeSpecificInfoFdd::new(
                    ie::IntraFreqMeasQuantityFDD::cpich_Ec_N0,
                ),
            ),
        )),
        Some(ie::IntraFreqReportingQuantity::new(
            quantities(true),
            quantities(false),
            None,
        )),
        Some(ie::MeasurementValidity::new(
            ie::MeasurementValidityUeState::cell_DCH,
        )),
        Some(ie::IntraFreqReportCriteria::periodicalReportingCriteria(
            ie::PeriodicalWithReportingCellStatus::new(
                reporting.criteria.clone(),
                Some(ie::ReportingCellStatus::withinActiveSet(
                    ie::MaxNumberOfReportingCellsType1::e1,
                )),
            ),
        )),
    );
    DLDCCHMessage::new(
        None,
        DLDCCHMessageType::measurementControl(pdu::MeasurementControl::r3(
            pdu::MeasurementControlR3::new(
                pdu::MeasurementControlR3IEs::new(
                    ie::RRCTransactionIdentifier(transaction.get()),
                    ie::MeasurementIdentity(CPICH_MEASUREMENT_ID),
                    ie::MeasurementCommand::setup(ie::MeasurementType::intraFrequencyMeasurement(
                        measurement,
                    )),
                    Some(ie::MeasurementReportingMode::new(
                        ie::TransferMode::acknowledgedModeRLC,
                        ie::PeriodicalOrEventTrigger::periodical,
                    )),
                    None,
                    None,
                ),
                None,
            ),
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criteria(message: DLDCCHMessage) -> ie::PeriodicalReportingCriteria {
        let encoded = umts_rrc_codec::encode_downlink_dedicated(&message).unwrap();
        let decoded =
            umts_rrc_codec::decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
        let DLDCCHMessageType::measurementControl(pdu::MeasurementControl::r3(control)) =
            decoded.value.message
        else {
            panic!("measurement control")
        };
        match control.measurement_control_r3.measurement_command {
            ie::MeasurementCommand::setup(ie::MeasurementType::qualityMeasurement(measurement)) => {
                let ie::QualityReportCriteria::periodicalReportingCriteria(criteria) =
                    measurement.report_criteria
                else {
                    panic!("periodical quality reporting")
                };
                criteria
            }
            ie::MeasurementCommand::setup(ie::MeasurementType::intraFrequencyMeasurement(
                measurement,
            )) => {
                let Some(ie::IntraFreqReportCriteria::periodicalReportingCriteria(criteria)) =
                    measurement.report_criteria
                else {
                    panic!("periodical CPICH reporting")
                };
                criteria.periodical_reporting_criteria
            }
            _ => panic!("unexpected measurement"),
        }
    }

    #[test]
    fn configurable_reporting_is_encoded_for_both_measurements() {
        for (count, interval, amount, cadence) in [
            (
                None,
                1000,
                ie::ReportingAmount::ra_Infinity,
                ie::ReportingIntervalLong::ril1,
            ),
            (
                Some(64),
                250,
                ie::ReportingAmount::ra64,
                ie::ReportingIntervalLong::ril0_25,
            ),
            (
                Some(1),
                64000,
                ie::ReportingAmount::ra1,
                ie::ReportingIntervalLong::ril64,
            ),
        ] {
            let reporting = PeriodicalReporting::new(count, interval).unwrap();
            let expected = ie::PeriodicalReportingCriteria::new(amount, cadence);
            let transaction = TransactionId::new(1).unwrap();
            assert_eq!(criteria(bler_setup(transaction, &reporting)), expected);
            assert_eq!(criteria(cpich_setup(transaction, &reporting)), expected);
        }
        assert_eq!(
            PeriodicalReporting::new(Some(3), 1000).unwrap_err(),
            ReportingError::Count
        );
        for interval in [0, 1, 750, 65000] {
            assert_eq!(
                PeriodicalReporting::new(None, interval).unwrap_err(),
                ReportingError::Interval
            );
        }
        for interval in [
            250, 500, 1000, 2000, 3000, 4000, 6000, 8000, 12000, 16000, 20000, 24000, 28000, 32000,
            64000,
        ] {
            for count in [
                None,
                Some(1),
                Some(2),
                Some(4),
                Some(8),
                Some(16),
                Some(32),
                Some(64),
            ] {
                assert!(PeriodicalReporting::new(count, interval).is_ok());
            }
        }
    }

    #[test]
    fn bler_control_requests_all_eligible_channels_periodically() {
        let message = bler_setup(
            TransactionId::new(1).unwrap(),
            &PeriodicalReporting::default(),
        );
        umts_rrc_codec::encode_downlink_dedicated(&message).unwrap();
        let DLDCCHMessageType::measurementControl(pdu::MeasurementControl::r3(control)) =
            message.message
        else {
            panic!("measurement control")
        };
        let control = control.measurement_control_r3;
        assert_eq!(control.measurement_identity.0, BLER_MEASUREMENT_ID);
        let ie::MeasurementCommand::setup(ie::MeasurementType::qualityMeasurement(measurement)) =
            control.measurement_command
        else {
            panic!("quality setup")
        };
        let quantity = measurement.quality_reporting_quantity.unwrap();
        assert!(quantity.dl_trans_ch_bler);
        assert!(quantity.bler_dl_trans_ch_id_list.is_none());
        assert_eq!(
            measurement.report_criteria,
            ie::QualityReportCriteria::periodicalReportingCriteria(
                ie::PeriodicalReportingCriteria::new(
                    ie::ReportingAmount::ra32,
                    ie::ReportingIntervalLong::ril1
                )
            )
        );
    }
}
