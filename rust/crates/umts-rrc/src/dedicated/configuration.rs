// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{DchDirection, DchTfcs, DedicatedError, NormalDpch};
use std::collections::BTreeSet;
use umts_rrc_schema::information_elements as ie;
use umts_types::CnDomain;

const FIRST_USER_BEARER: u8 = 5;
const MAX_BEARER: u8 = 32;
const MAX_LOGICAL: u8 = 15;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rel99Rlc {
    Transparent {
        uplink_segmentation: bool,
        downlink_segmentation: bool,
    },
    Unacknowledged,
    Acknowledged {
        uplink: ie::ULAMRLCMode,
        downlink: ie::DLAMRLCMode,
    },
}
impl Rel99Rlc {
    pub fn wire(&self) -> ie::RLCInfoChoice {
        let (uplink, downlink) = match self {
            Self::Transparent {
                uplink_segmentation,
                downlink_segmentation,
            } => (
                ie::ULRLCMode::ul_TM_RLC_Mode(ie::ULTMRLCMode::new(None, *uplink_segmentation)),
                ie::DLRLCMode::dl_TM_RLC_Mode(ie::DLTMRLCMode::new(*downlink_segmentation)),
            ),
            Self::Unacknowledged => (
                ie::ULRLCMode::ul_UM_RLC_Mode(ie::ULUMRLCMode::new(None)),
                ie::DLRLCMode::dl_UM_RLC_Mode(()),
            ),
            Self::Acknowledged { uplink, downlink } => (
                ie::ULRLCMode::ul_AM_RLC_Mode(uplink.clone()),
                ie::DLRLCMode::dl_AM_RLC_Mode(downlink.clone()),
            ),
        };
        ie::RLCInfoChoice::rlc_Info(ie::RLCInfo::new(Some(uplink), Some(downlink)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicalMapping {
    pub transport: u8,
    pub logical: Option<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UplinkLogicalMapping {
    pub transport: u8,
    pub logical: Option<u8>,
    pub priority: u8,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BearerMapping {
    pub uplink: Vec<UplinkLogicalMapping>,
    pub downlink: Vec<LogicalMapping>,
}
impl BearerMapping {
    pub(crate) fn wire_r7(&self) -> ie::RBMappingInfoR7 {
        let uplink: Vec<_> = self
            .uplink
            .iter()
            .map(|mapping| {
                ie::ULLogicalChannelMappingR6::new(
                    ie::ULLogicalChannelMappingR6UlTrCHType::dch_rach_usch(
                        ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUsch::new(
                            ie::ULTransportChannelType::dch(ie::TransportChannelIdentity(
                                mapping.transport,
                            )),
                            mapping.logical.map(ie::LogicalChannelIdentity),
                            ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUschRlcSizeList::allSizes(
                                (),
                            ),
                        ),
                    ),
                    ie::MACLogicalChannelPriority(mapping.priority),
                )
            })
            .collect();
        let uplink = match uplink.len() {
            0 => None,
            1 => Some(ie::ULLogicalChannelMappingsR6::oneLogicalChannel(
                uplink.into_iter().next().expect("one UL mapping"),
            )),
            _ => Some(ie::ULLogicalChannelMappingsR6::twoLogicalChannels(
                ie::ULLogicalChannelMappingListR6::new(true, uplink),
            )),
        };
        let downlink = (!self.downlink.is_empty()).then(|| {
            ie::DLLogicalChannelMappingListR7(
                self.downlink
                    .iter()
                    .map(|mapping| {
                        ie::DLLogicalChannelMappingR7::new(
                            ie::DLTransportChannelTypeR7::dch(ie::TransportChannelIdentity(
                                mapping.transport,
                            )),
                            mapping.logical.map(ie::LogicalChannelIdentity),
                        )
                    })
                    .collect(),
            )
        });
        ie::RBMappingInfoR7(vec![ie::RBMappingOptionR7::new(uplink, downlink)])
    }

    pub(crate) fn wire(&self) -> ie::RBMappingInfo {
        let uplink: Vec<_> = self
            .uplink
            .iter()
            .map(|mapping| {
                ie::ULLogicalChannelMapping::new(
                    ie::ULTransportChannelType::dch(ie::TransportChannelIdentity(
                        mapping.transport,
                    )),
                    mapping.logical.map(ie::LogicalChannelIdentity),
                    ie::ULLogicalChannelMappingRlcSizeList::allSizes(()),
                    ie::MACLogicalChannelPriority(mapping.priority),
                )
            })
            .collect();
        let uplink = match uplink.len() {
            0 => None,
            1 => Some(ie::ULLogicalChannelMappings::oneLogicalChannel(
                uplink.into_iter().next().expect("one UL mapping"),
            )),
            _ => Some(ie::ULLogicalChannelMappings::twoLogicalChannels(
                ie::ULLogicalChannelMappingList::new(true, uplink),
            )),
        };
        let downlink = (!self.downlink.is_empty()).then(|| {
            ie::DLLogicalChannelMappingList(
                self.downlink
                    .iter()
                    .map(|mapping| {
                        ie::DLLogicalChannelMapping::new(
                            ie::DLTransportChannelType::dch(ie::TransportChannelIdentity(
                                mapping.transport,
                            )),
                            mapping.logical.map(ie::LogicalChannelIdentity),
                        )
                    })
                    .collect(),
            )
        });
        ie::RBMappingInfo(vec![ie::RBMappingOption::new(uplink, downlink)])
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rel99Bearer {
    pub id: u8,
    pub domain: Option<CnDomain>,
    pub rlc: Rel99Rlc,
    pub mapping: BearerMapping,
}
impl Rel99Bearer {
    pub fn mappings(&self, direction: DchDirection) -> impl Iterator<Item = LogicalMapping> + '_ {
        self.mapping
            .uplink
            .iter()
            .filter(move |_| direction == DchDirection::Uplink)
            .map(|mapping| LogicalMapping {
                transport: mapping.transport,
                logical: mapping.logical,
            })
            .chain(
                self.mapping
                    .downlink
                    .iter()
                    .copied()
                    .filter(move |_| direction == DchDirection::Downlink),
            )
    }
}

pub struct Rel99Configuration {
    uplink: DchTfcs,
    downlink: DchTfcs,
    bearers: Vec<Rel99Bearer>,
    physical: NormalDpch,
}
impl Rel99Configuration {
    pub fn with_physical(&self, physical: NormalDpch) -> Result<Self, DedicatedError> {
        Self::new(
            DchTfcs::new(
                self.uplink.direction(),
                self.uplink.transports().to_vec(),
                self.uplink.combinations().to_vec(),
            )?,
            DchTfcs::new(
                self.downlink.direction(),
                self.downlink.transports().to_vec(),
                self.downlink.combinations().to_vec(),
            )?,
            self.bearers.clone(),
            physical,
        )
    }
    pub fn new(
        uplink: DchTfcs,
        downlink: DchTfcs,
        mut bearers: Vec<Rel99Bearer>,
        physical: NormalDpch,
    ) -> Result<Self, DedicatedError> {
        if uplink.direction() != DchDirection::Uplink
            || downlink.direction() != DchDirection::Downlink
            || bearers.is_empty()
            || bearers.len() > usize::from(MAX_BEARER)
            || bearers.windows(2).any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(DedicatedError::Bearer);
        }
        physical.validate()?;
        if !physical.uplink_tfci && uplink.combinations().len() != 1
            || !physical.downlink_tfci && downlink.combinations().len() != 1
        {
            return Err(DedicatedError::Physical);
        }
        for bearer in &bearers {
            if bearer.id == 0
                || bearer.id > MAX_BEARER
                || (bearer.id < FIRST_USER_BEARER) != bearer.domain.is_none()
                || bearer.id == 1 && !matches!(bearer.rlc, Rel99Rlc::Unacknowledged)
                || (2..FIRST_USER_BEARER).contains(&bearer.id)
                    && !matches!(bearer.rlc, Rel99Rlc::Acknowledged { .. })
            {
                return Err(DedicatedError::Bearer);
            }
            if bearer.mapping.uplink.len() > 2
                || bearer.mapping.downlink.len() > 2
                || bearer.mapping.uplink.is_empty()
                || bearer.mapping.downlink.is_empty()
                || !matches!(bearer.rlc, Rel99Rlc::Acknowledged { .. })
                    && (bearer.mapping.uplink.len() != 1 || bearer.mapping.downlink.len() != 1)
            {
                return Err(DedicatedError::Mapping);
            }
            if bearer
                .mapping
                .uplink
                .iter()
                .any(|mapping| !(1..=8).contains(&mapping.priority))
            {
                return Err(DedicatedError::Mapping);
            }
            for (tfcs, direction) in [
                (&uplink, DchDirection::Uplink),
                (&downlink, DchDirection::Downlink),
            ] {
                for mapping in bearer.mappings(direction) {
                    if mapping
                        .logical
                        .is_some_and(|logical| logical == 0 || logical > MAX_LOGICAL)
                    {
                        return Err(DedicatedError::Mapping);
                    }
                    let transport = tfcs
                        .transport(mapping.transport)
                        .ok_or(DedicatedError::Mapping)?;
                    if matches!(bearer.rlc, Rel99Rlc::Acknowledged { .. }) {
                        let mut sizes = transport.formats().map(|format| format.rlc_bits);
                        let first = sizes.next().ok_or(DedicatedError::Format)?;
                        if first == 0 || !first.is_multiple_of(8) || sizes.any(|bits| bits != first)
                        {
                            return Err(DedicatedError::Format);
                        }
                    }
                    if matches!(bearer.rlc, Rel99Rlc::Unacknowledged)
                        && transport.formats().any(|format| {
                            format.rlc_bits != 0 && !format.rlc_bits.is_multiple_of(8)
                        })
                    {
                        return Err(DedicatedError::Format);
                    }
                }
            }
        }
        for (tfcs, direction) in [
            (&uplink, DchDirection::Uplink),
            (&downlink, DchDirection::Downlink),
        ] {
            for transport in tfcs.transports() {
                let mut logicals = BTreeSet::new();
                let mut implicit = 0;
                let mut domains = Vec::new();
                let mut transparent = false;
                for bearer in &bearers {
                    for mapping in bearer
                        .mappings(direction)
                        .filter(|mapping| mapping.transport == transport.id)
                    {
                        if let Some(logical) = mapping.logical {
                            if !logicals.insert(logical) {
                                return Err(DedicatedError::Mapping);
                            }
                        } else {
                            implicit += 1;
                        }
                        if !domains.contains(&bearer.domain) {
                            domains.push(bearer.domain);
                        }
                        transparent |= matches!(bearer.rlc, Rel99Rlc::Transparent { .. });
                    }
                }
                if implicit + logicals.len() == 0
                    || implicit > 0 && (implicit != 1 || !logicals.is_empty())
                {
                    return Err(DedicatedError::Mapping);
                }
                // TS 25.331 8.6.4.8 leaves ciphered TM across CN domains or SRB/user mixtures unspecified.
                if transparent && (domains.len() > 1 || domains.contains(&None)) {
                    return Err(DedicatedError::Mapping);
                }
                // TS 25.331 8.5.21 and TS 25.321 9.2.1.1 require another logical channel for C/T multiplexing.
                if implicit + logicals.len() == 1 {
                    for bearer in &mut bearers {
                        match direction {
                            DchDirection::Uplink => {
                                for mapping in &mut bearer.mapping.uplink {
                                    if mapping.transport == transport.id {
                                        mapping.logical = None;
                                    }
                                }
                            }
                            DchDirection::Downlink => {
                                for mapping in &mut bearer.mapping.downlink {
                                    if mapping.transport == transport.id {
                                        mapping.logical = None;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(Self {
            uplink,
            downlink,
            bearers,
            physical,
        })
    }
    pub fn uplink(&self) -> &DchTfcs {
        &self.uplink
    }
    pub fn downlink(&self) -> &DchTfcs {
        &self.downlink
    }
    pub fn physical(&self) -> &NormalDpch {
        &self.physical
    }
    pub fn bearers(&self) -> &[Rel99Bearer] {
        &self.bearers
    }
    pub fn bearer(&self, id: u8) -> Option<&Rel99Bearer> {
        self.bearers
            .binary_search_by_key(&id, |bearer| bearer.id)
            .ok()
            .map(|index| &self.bearers[index])
    }
    pub fn multiplexed(&self, direction: DchDirection, id: u8) -> Result<bool, DedicatedError> {
        let tfcs = match direction {
            DchDirection::Uplink => &self.uplink,
            DchDirection::Downlink => &self.downlink,
        };
        tfcs.transport(id).ok_or(DedicatedError::Transport)?;
        Ok(self.bearers.iter().any(|bearer| {
            bearer
                .mappings(direction)
                .any(|mapping| mapping.transport == id && mapping.logical.is_some())
        }))
    }
}
