// SPDX-License-Identifier: AGPL-3.0-or-later

//! FACH/RACH multiplexing options for bearers that also have a dedicated or HSPA option.
//!
//! TS 25.331 8.5.21 selects a multiplexing option at every state transition. A bearer with no
//! FACH/RACH option makes the configuration invalid as soon as the UE is in CELL_FACH, which it
//! is during every cell update.

use umts_rrc_schema::information_elements as ie;

// TS 25.331 8.6.4.8 indexes the RACH TFS of the first PRACH in SIB5 and forbids "All" on RACH.
const FIRST_PRACH_RLC_SIZE_INDEX: u8 = 1;
const LAST_SIGNALLING_BEARER: u8 = 4;
const DATA_PRIORITY: u8 = 8;
const MAX_LOGICAL_CHANNEL: u8 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommonChannel {
    logical: u8,
    priority: u8,
}

impl CommonChannel {
    pub(crate) fn for_bearer(id: u8) -> Self {
        if id <= LAST_SIGNALLING_BEARER {
            return Self {
                logical: id,
                priority: id,
            };
        }
        // Logical channel identities end at 15 while bearer identities reach 32. Two data
        // bearers whose identities differ by 11 would share one, which the planner never assigns.
        let first = LAST_SIGNALLING_BEARER + 1;
        Self {
            logical: first + (id - first) % (MAX_LOGICAL_CHANNEL - LAST_SIGNALLING_BEARER),
            priority: DATA_PRIORITY,
        }
    }

    fn uplink(self) -> ie::ULLogicalChannelMapping {
        ie::ULLogicalChannelMapping::new(
            ie::ULTransportChannelType::rach(()),
            Some(ie::LogicalChannelIdentity(self.logical)),
            ie::ULLogicalChannelMappingRlcSizeList::explicitList(ie::RLCSizeExplicitList(vec![
                ie::RLCSizeInfo::new(FIRST_PRACH_RLC_SIZE_INDEX),
            ])),
            ie::MACLogicalChannelPriority(self.priority),
        )
    }

    fn uplink_r6(self) -> ie::ULLogicalChannelMappingsR6 {
        ie::ULLogicalChannelMappingsR6::oneLogicalChannel(ie::ULLogicalChannelMappingR6::new(
            ie::ULLogicalChannelMappingR6UlTrCHType::dch_rach_usch(
                ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUsch::new(
                    ie::ULTransportChannelType::rach(()),
                    Some(ie::LogicalChannelIdentity(self.logical)),
                    ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUschRlcSizeList::explicitList(
                        ie::RLCSizeExplicitList(vec![ie::RLCSizeInfo::new(
                            FIRST_PRACH_RLC_SIZE_INDEX,
                        )]),
                    ),
                ),
            ),
            ie::MACLogicalChannelPriority(self.priority),
        ))
    }

    fn downlink_r5(self) -> ie::DLLogicalChannelMappingListR5 {
        ie::DLLogicalChannelMappingListR5(vec![ie::DLLogicalChannelMappingR5::new(
            ie::DLTransportChannelTypeR5::fach(()),
            Some(ie::LogicalChannelIdentity(self.logical)),
        )])
    }

    pub(crate) fn option(self) -> ie::RBMappingOption {
        ie::RBMappingOption::new(
            Some(ie::ULLogicalChannelMappings::oneLogicalChannel(
                self.uplink(),
            )),
            Some(ie::DLLogicalChannelMappingList(vec![
                ie::DLLogicalChannelMapping::new(
                    ie::DLTransportChannelType::fach(()),
                    Some(ie::LogicalChannelIdentity(self.logical)),
                ),
            ])),
        )
    }

    pub(crate) fn option_r5(self) -> ie::RBMappingOptionR5 {
        ie::RBMappingOptionR5::new(
            Some(ie::ULLogicalChannelMappings::oneLogicalChannel(
                self.uplink(),
            )),
            Some(self.downlink_r5()),
        )
    }

    pub(crate) fn option_r6(self) -> ie::RBMappingOptionR6 {
        ie::RBMappingOptionR6::new(Some(self.uplink_r6()), Some(self.downlink_r5()))
    }

    pub(crate) fn option_r7(self) -> ie::RBMappingOptionR7 {
        ie::RBMappingOptionR7::new(
            Some(self.uplink_r6()),
            Some(ie::DLLogicalChannelMappingListR7(vec![
                ie::DLLogicalChannelMappingR7::new(
                    ie::DLTransportChannelTypeR7::fach(()),
                    Some(ie::LogicalChannelIdentity(self.logical)),
                ),
            ])),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_release_format_encodes_for_signalling_and_data_bearers() {
        for id in [1, 2, 3, 5, 15, 16, 32] {
            let common = CommonChannel::for_bearer(id);
            rasn::uper::encode(&ie::RBMappingInfo(vec![common.option()]))
                .unwrap_or_else(|error| panic!("r3 {id}: {error}"));
            rasn::uper::encode(&ie::RBMappingInfoR5(vec![common.option_r5()]))
                .unwrap_or_else(|error| panic!("r5 {id}: {error}"));
            rasn::uper::encode(&ie::RBMappingInfoR6(vec![common.option_r6()]))
                .unwrap_or_else(|error| panic!("r6 {id}: {error}"));
            rasn::uper::encode(&ie::RBMappingInfoR7(vec![common.option_r7()]))
                .unwrap_or_else(|error| panic!("r7 {id}: {error}"));
        }
    }
}
