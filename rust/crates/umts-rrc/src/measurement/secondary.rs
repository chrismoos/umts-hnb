// SPDX-License-Identifier: AGPL-3.0-or-later
use super::*;

const MAXIMUM_UARFCN: u16 = 16383;
const MAXIMUM_PRIMARY_SCRAMBLING_CODE: u16 = 511;
const SECONDARY_OBJECT_INDEX: u8 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecondaryCpichTarget {
    uarfcn: u16,
    scrambling_code: u16,
}

impl SecondaryCpichTarget {
    pub fn new(uarfcn: u16, scrambling_code: u16) -> Option<Self> {
        (uarfcn <= MAXIMUM_UARFCN && scrambling_code <= MAXIMUM_PRIMARY_SCRAMBLING_CODE).then_some(
            Self {
                uarfcn,
                scrambling_code,
            },
        )
    }
    pub fn uarfcn(self) -> u16 {
        self.uarfcn
    }
    pub fn scrambling_code(self) -> u16 {
        self.scrambling_code
    }
}

// TS 25.331 8.6.7.14/.24: both quantities are mandatory for setup.
// adjacentFrequencyIndex names the explicitly numbered cell and requires the matching UE capability.
pub fn secondary_cpich_setup(
    transaction: TransactionId,
    reporting: &PeriodicalReporting,
    target: SecondaryCpichTarget,
) -> DLDCCHMessage {
    let cell = ie::NewInterFreqCellR8::new(
        Some(ie::InterFreqCellID(SECONDARY_OBJECT_INDEX)),
        Some(ie::FrequencyInfo::new(
            ie::FrequencyInfoModeSpecificInfo::fdd(ie::FrequencyInfoFDD::new(
                None,
                ie::UARFCN(target.uarfcn),
            )),
        )),
        ie::CellInfoR4::new(
            ie::CellIndividualOffset(0),
            None,
            ie::CellInfoR4ModeSpecificInfo::fdd(ie::CellInfoR4ModeSpecificInfoFdd::new(
                Some(ie::PrimaryCPICHInfo::new(ie::PrimaryScramblingCode(
                    target.scrambling_code,
                ))),
                None,
                false,
                false,
            )),
        ),
        ie::NewInterFreqCellR8ModeSpecificInfo::noInfo(()),
    );
    let quantities = ie::CellReportingQuantities::new(
        ie::SFNSFNOTDType::noReport,
        false,
        false,
        ie::CellReportingQuantitiesModeSpecificInfo::fdd(
            ie::CellReportingQuantitiesModeSpecificInfoFdd::new(true, true, false),
        ),
    );
    let quantity = ie::InterFreqMeasQuantity::new(
        ie::InterFreqMeasQuantityReportingCriteria::interFreqReportingCriteria(
            ie::InterFreqMeasQuantityReportingCriteriaInterFreqReportingCriteria::new(ie::FilterCoefficient::fc0,
                ie::InterFreqMeasQuantityReportingCriteriaInterFreqReportingCriteriaModeSpecificInfo::fdd(
                    ie::InterFreqMeasQuantityReportingCriteriaInterFreqReportingCriteriaModeSpecificInfoFdd::new(
                        ie::FreqQualityEstimateQuantityFDD::cpich_Ec_N0)))));
    let measurement = ie::InterFrequencyMeasurementR8::new(
        ie::InterFreqCellInfoListR8::new(None, Some(ie::NewInterFreqCellListR8(vec![cell])), None),
        Some(quantity),
        Some(ie::InterFreqReportingQuantity::new(
            false, false, quantities,
        )),
        Some(ie::MeasurementValidity::new(
            ie::MeasurementValidityUeState::cell_DCH,
        )),
        None,
        Some(SECONDARY_OBJECT_INDEX),
        ie::InterFreqReportCriteriaR7::periodicalReportingCriteria(
            ie::PeriodicalWithReportingCellStatus::new(
                reporting.criteria.clone(),
                Some(ie::ReportingCellStatus::withinMonitoredSetNonUsedFreq(
                    ie::MaxNumberOfReportingCellsType1::e1,
                )),
            ),
        ),
    );
    use pdu::{
        MeasurementControlLaterThanR3CriticalExtensionsLaterThanR4CriticalExtensions as R6,
        MeasurementControlLaterThanR3CriticalExtensionsLaterThanR4CriticalExtensionsCriticalExtensions as R7,
        MeasurementControlLaterThanR3CriticalExtensionsLaterThanR4CriticalExtensionsCriticalExtensionsCriticalExtensions as R8,
        MeasurementControlLaterThanR3CriticalExtensionsLaterThanR4CriticalExtensionsCriticalExtensionsCriticalExtensionsR8 as Body,
    };
    let body = pdu::MeasurementControlR8IEs::new(
        ie::MeasurementIdentity(SECONDARY_CPICH_MEASUREMENT_ID),
        ie::MeasurementCommandR8::setup(ie::MeasurementTypeR8::interFrequencyMeasurement(
            measurement,
        )),
        Some(ie::MeasurementReportingMode::new(
            ie::TransferMode::acknowledgedModeRLC,
            ie::PeriodicalOrEventTrigger::periodical,
        )),
        None,
        None,
    );
    DLDCCHMessage::new(
        None,
        DLDCCHMessageType::measurementControl(pdu::MeasurementControl::later_than_r3(
            pdu::MeasurementControlLaterThanR3::new(
                ie::RRCTransactionIdentifier(transaction.get()),
                pdu::MeasurementControlLaterThanR3CriticalExtensions::later_than_r4(
                    pdu::MeasurementControlLaterThanR3CriticalExtensionsLaterThanR4::new(
                        ie::RRCTransactionIdentifier(0),
                        R6::criticalExtensions(R7::criticalExtensions(R8::r8(Body::new(
                            body, None, None,
                        )))),
                    ),
                ),
            ),
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secondary_periodic_control_round_trips_with_distinct_identity() {
        let target = SecondaryCpichTarget::new(10862, 12).unwrap();
        let message = secondary_cpich_setup(
            TransactionId::new(2).unwrap(),
            &PeriodicalReporting::default(),
            target,
        );
        let encoded = umts_rrc_codec::encode_downlink_dedicated(&message).unwrap();
        let decoded =
            umts_rrc_codec::decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
        assert_eq!(*decoded.value, message);
        assert!(SecondaryCpichTarget::new(16384, 12).is_none());
        assert!(SecondaryCpichTarget::new(10862, 512).is_none());
    }
}
