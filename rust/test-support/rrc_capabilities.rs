// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::types::BitString;
use umts_rrc_schema::{information_elements as ie, pdu_definitions as pdu};

fn radio() -> ie::UERadioAccessCapability {
    let mut cipher = BitString::repeat(false, 16);
    cipher.set(14, true);
    cipher.set(15, true);
    let mut integrity = BitString::repeat(false, 16);
    integrity.set(14, true);
    ie::UERadioAccessCapability::new(
        ie::PDCPCapability::new(false, ie::PDCPCapabilitySupportForRfc2507::notSupported(())),
        ie::RLCCapability::new(
            ie::TotalRLCAMBufferSize::kb50,
            ie::MaximumRLCWindowSize::mws2047,
            ie::MaximumAMEntityNumberRLCCap::am4,
        ),
        ie::TransportChannelCapability::new(
            ie::DLTransChCapability::new(
                ie::MaxNoBits::b10240,
                ie::MaxNoBits::b10240,
                ie::TurboSupport::supported(ie::MaxNoBits::b10240),
                ie::MaxSimultaneousTransChsDL::e4,
                ie::MaxSimultaneousCCTrCHCount(1),
                ie::MaxTransportBlocksDL::tb4,
                ie::MaxNumberOfTFCDL::tfc16,
                ie::MaxNumberOfTF::tf32,
            ),
            ie::ULTransChCapability::new(
                ie::MaxNoBits::b10240,
                ie::MaxNoBits::b10240,
                ie::TurboSupport::supported(ie::MaxNoBits::b10240),
                ie::MaxSimultaneousTransChsUL::e4,
                ie::ULTransChCapabilityModeSpecificInfo::fdd(()),
                ie::MaxTransportBlocksUL::tb4,
                ie::MaxNumberOfTFCUL::tfc16,
                ie::MaxNumberOfTF::tf32,
            ),
        ),
        ie::RFCapability::new(
            Some(ie::RFCapabilityFddRFCapability::new(
                ie::UEPowerClass(3),
                ie::TxRxFrequencySeparation::default_TxRx_separation,
            )),
            None,
        ),
        ie::PhysicalChannelCapability::new(
            Some(ie::PhysicalChannelCapabilityFddPhysChCapability::new(
                ie::DLPhysChCapabilityFDD::new(
                    1,
                    ie::MaxNoPhysChBitsReceived::b19200,
                    true,
                    false,
                    ie::SimultaneousSCCPCHDPCHReception::notSupported(()),
                ),
                ie::ULPhysChCapabilityFDD::new(ie::MaxNoDPDCHBitsTransmitted::b9600, false),
            )),
            None,
        ),
        ie::UEMultiModeRATCapability::new(
            ie::MultiRATCapability::new(false, false),
            ie::MultiModeCapability::fdd,
        ),
        ie::SecurityCapability::new(cipher, integrity),
        ie::UEPositioningCapability::new(
            false,
            false,
            ie::NetworkAssistedGPSSupported::noNetworkAssistedGPS,
            false,
            false,
        ),
        None,
    )
}
pub fn information() -> pdu::UECapabilityInformation {
    pdu::UECapabilityInformation::new(
        Some(ie::RRCTransactionIdentifier(2)),
        Some(radio()),
        None,
        None,
    )
}
