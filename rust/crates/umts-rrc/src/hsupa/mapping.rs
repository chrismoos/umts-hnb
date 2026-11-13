// SPDX-License-Identifier: AGPL-3.0-or-later

use super::ConfigurationError;
use umts_rrc_schema::information_elements as ie;

const MAX_FLOW: u8 = 7;
const MAX_RETRANSMISSIONS: u8 = 15;
const MAX_POWER: u8 = 6;
const FIRST_LOGICAL: u8 = 1;
const LAST_LOGICAL: u8 = 15;
const MAX_DDI: u8 = 62;
const MAX_PRIORITY: u8 = 16;

#[derive(Clone, Copy, Debug)]
pub struct ScheduledFlow {
    identity: u8,
    power: u8,
    retransmissions: u8,
}

impl ScheduledFlow {
    pub fn new(identity: u8, power: u8, retransmissions: u8) -> Result<Self, ConfigurationError> {
        if identity > MAX_FLOW || power > MAX_POWER || retransmissions > MAX_RETRANSMISSIONS {
            return Err(ConfigurationError::Format);
        }
        Ok(Self {
            identity,
            power,
            retransmissions,
        })
    }

    pub fn identity(self) -> u8 {
        self.identity
    }

    pub fn power_offset(self) -> u8 {
        self.power
    }

    pub fn wire(self) -> ie::EDCHAddReconfMACDFlow {
        let mut multiplexing = rasn::types::FixedBitString::<8>::default();
        multiplexing.set(usize::from(self.identity), true);
        ie::EDCHAddReconfMACDFlow::new(
            ie::EDCHMACDFlowIdentity(self.identity),
            Some(ie::EDCHMACDFlowPowerOffset(self.power)),
            Some(ie::EDCHMACDFlowMaxRetrans(self.retransmissions)),
            Some(ie::EDCHMACDFlowMultiplexingList(multiplexing)),
            Some(
                ie::EDCHAddReconfMACDFlowTransmissionGrantType::scheduledTransmissionGrantInfo(()),
            ),
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UplinkMapping {
    logical: u8,
    flow: u8,
    ddi: u8,
    pdu_bits: u16,
    priority: u8,
    report: bool,
}

impl UplinkMapping {
    pub fn new(
        logical: u8,
        flow: u8,
        ddi: u8,
        pdu_bits: u16,
        priority: u8,
        report: bool,
    ) -> Result<Self, ConfigurationError> {
        if !(FIRST_LOGICAL..=LAST_LOGICAL).contains(&logical)
            || flow > MAX_FLOW
            || ddi > MAX_DDI
            || priority == 0
            || priority > MAX_PRIORITY
        {
            return Err(ConfigurationError::Format);
        }
        pdu_size(pdu_bits)?;
        Ok(Self {
            logical,
            flow,
            ddi,
            pdu_bits,
            priority,
            report,
        })
    }
    pub fn logical(self) -> u8 {
        self.logical
    }
    pub fn flow(self) -> u8 {
        self.flow
    }
    pub fn ddi(self) -> u8 {
        self.ddi
    }
    pub fn pdu_bits(self) -> u16 {
        self.pdu_bits
    }

    pub fn wire(self) -> Result<ie::ULLogicalChannelMappingsR6, ConfigurationError> {
        Ok(ie::ULLogicalChannelMappingsR6::oneLogicalChannel(
            ie::ULLogicalChannelMappingR6::new(
                ie::ULLogicalChannelMappingR6UlTrCHType::e_dch(
                    ie::ULLogicalChannelMappingR6UlTrCHTypeEDch::new(
                        ie::LogicalChannelIdentity(self.logical),
                        ie::EDCHMACDFlowIdentity(self.flow),
                        ie::DDI(self.ddi),
                        ie::RLCPDUSizeList(vec![ie::RLCPDUSize(pdu_size(self.pdu_bits)?)]),
                        self.report,
                    ),
                ),
                ie::MACLogicalChannelPriority(self.priority),
            ),
        ))
    }
}

fn pdu_size(bits: u16) -> Result<ie::OctetModeRLCSizeInfoType1, ConfigurationError> {
    crate::transport::octet_rlc_size(bits).map_err(|_| ConfigurationError::Format)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_supported_octet_size_encodes_its_exact_bit_length() {
        for bits in (16..=5000).step_by(8) {
            let wire = pdu_size(bits).unwrap();
            rasn::uper::encode(&wire).unwrap();
            let actual = match wire {
                ie::OctetModeRLCSizeInfoType1::sizeType1(n) => 8 * u16::from(n) + 16,
                ie::OctetModeRLCSizeInfoType1::sizeType2(n) => {
                    32 * u16::from(n.part1) + 272 + 8 * u16::from(n.part2.unwrap_or(0))
                }
                ie::OctetModeRLCSizeInfoType1::sizeType3(n) => {
                    64 * u16::from(n.part1) + 1040 + 8 * u16::from(n.part2.unwrap_or(0))
                }
            };
            assert_eq!(actual, bits);
        }
        assert!(pdu_size(15).is_err());
        assert!(pdu_size(5008).is_err());
        assert!(UplinkMapping::new(1, 0, 63, 320, 1, true).is_err());
    }
}
