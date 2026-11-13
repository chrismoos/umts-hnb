// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_schema::class_definitions::{ULDCCHMessageType, ULDCCHMessageTypeExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlMessageKind {
    ActiveSetUpdateComplete,
    ActiveSetUpdateFailure,
    CellChangeOrderFromUtranFailure,
    CounterCheckResponse,
    HandoverToUtranComplete,
    InitialDirectTransfer,
    HandoverFromUtranFailure,
    MeasurementControlFailure,
    MeasurementReport,
    PhysicalChannelReconfigurationComplete,
    PhysicalChannelReconfigurationFailure,
    RadioBearerReconfigurationComplete,
    RadioBearerReconfigurationFailure,
    RadioBearerReleaseComplete,
    RadioBearerReleaseFailure,
    RadioBearerSetupComplete,
    RadioBearerSetupFailure,
    RrcConnectionReleaseComplete,
    RrcConnectionSetupComplete,
    RrcStatus,
    SecurityModeComplete,
    SecurityModeFailure,
    SignallingConnectionReleaseIndication,
    TransportChannelReconfigurationComplete,
    TransportChannelReconfigurationFailure,
    TransportFormatCombinationControlFailure,
    UeCapabilityInformation,
    UplinkDirectTransfer,
    UtranMobilityInformationConfirm,
    UtranMobilityInformationFailure,
    MbmsModificationRequest,
    UeInformationResponse,
    Spare,
}

impl From<&ULDCCHMessageType> for ControlMessageKind {
    fn from(message: &ULDCCHMessageType) -> Self {
        match message {
            ULDCCHMessageType::activeSetUpdateComplete(_) => Self::ActiveSetUpdateComplete,
            ULDCCHMessageType::activeSetUpdateFailure(_) => Self::ActiveSetUpdateFailure,
            ULDCCHMessageType::cellChangeOrderFromUTRANFailure(_) => {
                Self::CellChangeOrderFromUtranFailure
            }
            ULDCCHMessageType::counterCheckResponse(_) => Self::CounterCheckResponse,
            ULDCCHMessageType::handoverToUTRANComplete(_) => Self::HandoverToUtranComplete,
            ULDCCHMessageType::initialDirectTransfer(_) => Self::InitialDirectTransfer,
            ULDCCHMessageType::handoverFromUTRANFailure(_) => Self::HandoverFromUtranFailure,
            ULDCCHMessageType::measurementControlFailure(_) => Self::MeasurementControlFailure,
            ULDCCHMessageType::measurementReport(_) => Self::MeasurementReport,
            ULDCCHMessageType::physicalChannelReconfigurationComplete(_) => {
                Self::PhysicalChannelReconfigurationComplete
            }
            ULDCCHMessageType::physicalChannelReconfigurationFailure(_) => {
                Self::PhysicalChannelReconfigurationFailure
            }
            ULDCCHMessageType::radioBearerReconfigurationComplete(_) => {
                Self::RadioBearerReconfigurationComplete
            }
            ULDCCHMessageType::radioBearerReconfigurationFailure(_) => {
                Self::RadioBearerReconfigurationFailure
            }
            ULDCCHMessageType::radioBearerReleaseComplete(_) => Self::RadioBearerReleaseComplete,
            ULDCCHMessageType::radioBearerReleaseFailure(_) => Self::RadioBearerReleaseFailure,
            ULDCCHMessageType::radioBearerSetupComplete(_) => Self::RadioBearerSetupComplete,
            ULDCCHMessageType::radioBearerSetupFailure(_) => Self::RadioBearerSetupFailure,
            ULDCCHMessageType::rrcConnectionReleaseComplete(_) => {
                Self::RrcConnectionReleaseComplete
            }
            ULDCCHMessageType::rrcConnectionSetupComplete(_) => Self::RrcConnectionSetupComplete,
            ULDCCHMessageType::rrcStatus(_) => Self::RrcStatus,
            ULDCCHMessageType::securityModeComplete(_) => Self::SecurityModeComplete,
            ULDCCHMessageType::securityModeFailure(_) => Self::SecurityModeFailure,
            ULDCCHMessageType::signallingConnectionReleaseIndication(_) => {
                Self::SignallingConnectionReleaseIndication
            }
            ULDCCHMessageType::transportChannelReconfigurationComplete(_) => {
                Self::TransportChannelReconfigurationComplete
            }
            ULDCCHMessageType::transportChannelReconfigurationFailure(_) => {
                Self::TransportChannelReconfigurationFailure
            }
            ULDCCHMessageType::transportFormatCombinationControlFailure(_) => {
                Self::TransportFormatCombinationControlFailure
            }
            ULDCCHMessageType::ueCapabilityInformation(_) => Self::UeCapabilityInformation,
            ULDCCHMessageType::uplinkDirectTransfer(_) => Self::UplinkDirectTransfer,
            ULDCCHMessageType::utranMobilityInformationConfirm(_) => {
                Self::UtranMobilityInformationConfirm
            }
            ULDCCHMessageType::utranMobilityInformationFailure(_) => {
                Self::UtranMobilityInformationFailure
            }
            ULDCCHMessageType::mbmsModificationRequest(_) => Self::MbmsModificationRequest,
            ULDCCHMessageType::ul_DCCH_MessageType_ext(extension) => match extension {
                ULDCCHMessageTypeExt::ueInformationResponse(_) => Self::UeInformationResponse,
                ULDCCHMessageTypeExt::spare15(())
                | ULDCCHMessageTypeExt::spare14(())
                | ULDCCHMessageTypeExt::spare13(())
                | ULDCCHMessageTypeExt::spare12(())
                | ULDCCHMessageTypeExt::spare11(())
                | ULDCCHMessageTypeExt::spare10(())
                | ULDCCHMessageTypeExt::spare9(())
                | ULDCCHMessageTypeExt::spare8(())
                | ULDCCHMessageTypeExt::spare7(())
                | ULDCCHMessageTypeExt::spare6(())
                | ULDCCHMessageTypeExt::spare5(())
                | ULDCCHMessageTypeExt::spare4(())
                | ULDCCHMessageTypeExt::spare3(())
                | ULDCCHMessageTypeExt::spare2(())
                | ULDCCHMessageTypeExt::spare1(()) => Self::Spare,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_rrc_schema::{
        information_elements::CNDomainIdentity,
        pdu_definitions::SignallingConnectionReleaseIndication,
    };

    #[test]
    fn release_indication_is_distinct_from_a_spare_message() {
        let release = ULDCCHMessageType::signallingConnectionReleaseIndication(
            SignallingConnectionReleaseIndication::new(CNDomainIdentity::ps_domain, None),
        );
        assert_eq!(
            ControlMessageKind::from(&release),
            ControlMessageKind::SignallingConnectionReleaseIndication
        );
        assert_eq!(
            ControlMessageKind::from(&ULDCCHMessageType::ul_DCCH_MessageType_ext(
                ULDCCHMessageTypeExt::spare1(()),
            )),
            ControlMessageKind::Spare
        );
    }
}
