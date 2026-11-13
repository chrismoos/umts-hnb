// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_plan::CompiledRel99;
use std::num::{NonZeroU16, NonZeroU64};
use umts_rlc::acknowledged::{AmConfig, AmLimits, Polling};
use umts_rlc::unacknowledged::{EInterpretation, LiWidth, UmConfig, UmLimits};
use umts_rrc::dedicated::{DchDirection, DchTransport, Rel99Bearer, Rel99Rlc};
use umts_rrc_schema::information_elements as ie;
use umts_security::counters::Start;

const SEVEN_BIT_UM_MAXIMUM_OCTETS: usize = 125;
const UM_MINIMUM_RECEIVE_OCTETS: usize = 2;
const UM_MINIMUM_TRANSMIT_OCTETS: usize = 3;

#[derive(Clone, Copy)]
pub struct NetworkAmPolicy {
    pub maximum_data_attempts: NonZeroU16,
    pub maximum_reset_attempts: NonZeroU16,
    pub reset_timeout_chips: NonZeroU64,
    pub polling: Polling,
    pub status_prohibit_chips: Option<NonZeroU64>,
    pub missing_pdu_indicator: bool,
}

#[derive(Clone, Copy)]
pub enum UserRlcPolicy {
    Acknowledged {
        network: NetworkAmPolicy,
        limits: AmLimits,
    },
    Unacknowledged {
        uplink: UmLimits,
        downlink: UmLimits,
    },
    Transparent {
        errors: ErrorDelivery,
        maximum_sdu_bits: NonZeroUsize,
    },
}

#[derive(Clone, Copy)]
enum PreparedEntity {
    Acknowledged {
        config: AmConfig,
        limits: AmLimits,
    },
    Unacknowledged {
        receive: UmConfig,
        transmit: UmConfig,
        uplink: UmLimits,
        downlink: UmLimits,
    },
    Transparent(TransparentSettings),
}
struct PreparedEndpoint {
    id: BearerId,
    scope: Scope,
    entity: PreparedEntity,
    receive_bits: usize,
    transmit_bytes: usize,
    storage_bytes: usize,
    maximum_sdu_bytes: usize,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct UserRlcProfiles {
    plan: Arc<CompiledRel99>,
    endpoints: Vec<PreparedEndpoint>,
    storage_bytes: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Geometry {
    minimum_bits: Option<usize>,
    maximum_bits: usize,
    maximum_blocks: usize,
    maximum_tti_bits: usize,
}

impl UserRlcProfiles {
    pub fn placeholders(self: &Arc<Self>, key: UeKey) -> Vec<BearerEndpoint> {
        self.endpoints
            .iter()
            .enumerate()
            .map(|(index, prepared)| BearerEndpoint {
                ue: key,
                id: prepared.id,
                scope: prepared.scope,
                entity: Entity::AwaitingUser {
                    profiles: self.clone(),
                    index,
                },
                raw: Vec::new(),
                plain: Vec::new(),
                transmit: Vec::new(),
                mac: Vec::new(),
                maximum_receive_bits: prepared.receive_bits,
                maximum_sdu_bytes: prepared.maximum_sdu_bytes,
                storage_bytes: prepared.storage_bytes + std::mem::size_of::<PreparedEndpoint>(),
                next_tm_transmit: None,
                next_tm_receive: None,
            })
            .collect()
    }
    pub(super) fn mode(&self, index: usize) -> CipherMode {
        match self.endpoints[index].entity {
            PreparedEntity::Acknowledged { .. } => CipherMode::Acknowledged,
            PreparedEntity::Unacknowledged { .. } => CipherMode::Unacknowledged,
            PreparedEntity::Transparent(_) => CipherMode::Transparent,
        }
    }
    pub(super) fn transmit_bytes(&self, index: usize) -> usize {
        self.endpoints[index].transmit_bytes
    }
    pub fn instantiate_accepted(
        &self,
        accepted: &crate::signalling::AcceptedUserSetup,
        now: ChipInstant,
    ) -> Result<Vec<BearerEndpoint>, BearerError> {
        if !Arc::ptr_eq(self.plan.configuration(), accepted.configuration()) {
            return Err(BearerError::Mapping);
        }
        if now.epoch != accepted.key().epoch || now.index < accepted.time().serviced().index {
            return Err(BearerError::Time);
        }
        let start = accepted.counters().new_bearer_start();
        self.instantiate(accepted.key(), [start; 2], now)
    }
    pub fn new(
        plan: Arc<CompiledRel99>,
        policies: &[(BearerId, UserRlcPolicy)],
        maximum_storage_bytes: NonZeroUsize,
    ) -> Result<Self, BearerError> {
        Self::build(plan, None, policies, maximum_storage_bytes)
    }

    pub fn additional(
        plan: Arc<CompiledRel99>,
        previous: &Arc<CompiledRel99>,
        policies: &[(BearerId, UserRlcPolicy)],
        maximum_storage_bytes: NonZeroUsize,
    ) -> Result<Self, BearerError> {
        for old in previous.configuration().bearers() {
            if plan.configuration().bearer(old.id) != Some(old) {
                return Err(BearerError::UnsupportedBinding);
            }
            for direction in [DchDirection::Uplink, DchDirection::Downlink] {
                let before = match direction {
                    DchDirection::Uplink => previous.configuration().uplink(),
                    DchDirection::Downlink => previous.configuration().downlink(),
                };
                let after = match direction {
                    DchDirection::Uplink => plan.configuration().uplink(),
                    DchDirection::Downlink => plan.configuration().downlink(),
                };
                for mapping in old.mappings(direction) {
                    let before = before
                        .transport(mapping.transport)
                        .ok_or(BearerError::Mapping)?;
                    let after = after
                        .transport(mapping.transport)
                        .ok_or(BearerError::Mapping)?;
                    if !same_transport(before, after)
                        || previous
                            .configuration()
                            .multiplexed(direction, mapping.transport)
                            .map_err(|_| BearerError::Mapping)?
                            != plan
                                .configuration()
                                .multiplexed(direction, mapping.transport)
                                .map_err(|_| BearerError::Mapping)?
                    {
                        return Err(BearerError::UnsupportedBinding);
                    }
                }
                if geometry(previous, old, direction)? != geometry(&plan, old, direction)? {
                    return Err(BearerError::UnsupportedBinding);
                }
            }
        }
        if plan
            .configuration()
            .bearers()
            .iter()
            .any(|rb| previous.configuration().bearer(rb.id).is_none() && rb.domain.is_none())
        {
            return Err(BearerError::UnsupportedBinding);
        }
        Self::build(plan, Some(previous), policies, maximum_storage_bytes)
    }

    fn build(
        plan: Arc<CompiledRel99>,
        previous: Option<&Arc<CompiledRel99>>,
        policies: &[(BearerId, UserRlcPolicy)],
        maximum_storage_bytes: NonZeroUsize,
    ) -> Result<Self, BearerError> {
        let users = plan
            .configuration()
            .bearers()
            .iter()
            .filter(|rb| {
                rb.domain.is_some()
                    && previous.is_none_or(|old| old.configuration().bearer(rb.id).is_none())
            })
            .count();
        if users == 0 || users != policies.len() || users > MAX_BEARERS {
            return Err(BearerError::Mapping);
        }
        let mut seen = [false; MAX_BEARERS];
        let mut endpoints = Vec::with_capacity(users);
        let mut storage_bytes = users
            .checked_mul(
                std::mem::size_of::<BearerEndpoint>() + std::mem::size_of::<PreparedEndpoint>(),
            )
            .ok_or(BearerError::Capacity)?;
        for (id, policy) in policies {
            if std::mem::replace(&mut seen[id.index()], true)
                || previous.is_some_and(|old| old.configuration().bearer(id.get()).is_some())
            {
                return Err(BearerError::Mapping);
            }
            let rb = plan
                .configuration()
                .bearer(id.get())
                .ok_or(BearerError::Mapping)?;
            let scope = Scope::User(rb.domain.ok_or(BearerError::Mapping)?);
            let uplink = geometry(&plan, rb, DchDirection::Uplink)?;
            let downlink = geometry(&plan, rb, DchDirection::Downlink)?;
            let (entity, rx_bits, tx_bytes, entity_bytes) = match (&rb.rlc, *policy) {
                (
                    Rel99Rlc::Acknowledged {
                        uplink: ul,
                        downlink: dl,
                    },
                    UserRlcPolicy::Acknowledged { network, limits },
                ) => {
                    if !matches!(
                        ul.transmission_rlc_discard,
                        ie::TransmissionRLCDiscard::noDiscard(_)
                    ) {
                        return Err(BearerError::UnsupportedBinding);
                    }
                    let config = AmConfig {
                        transmit_pdu_bytes: fixed_octets(downlink)?,
                        receive_pdu_bytes: fixed_octets(uplink)?,
                        // TS 25.331 10.3.4.23 makes each network window equal to its opposite UE window.
                        transmit_window: downlink_window(dl.receiving_window_size)?,
                        receive_window: uplink_window(ul.transmission_window_size)?,
                        maximum_data_attempts: network.maximum_data_attempts,
                        maximum_reset_attempts: network.maximum_reset_attempts,
                        reset_timeout_chips: network.reset_timeout_chips,
                        polling: network.polling,
                        status_prohibit_chips: network.status_prohibit_chips,
                        missing_pdu_indicator: network.missing_pdu_indicator,
                        accept_special_he: false,
                        initial_tx_hfn: 0,
                        initial_rx_hfn: 0,
                    };
                    let bytes = config.buffer_bytes(limits).map_err(BearerError::Am)?;
                    if bytes > limits.maximum_buffer_bytes
                        || limits.maximum_sdu_bytes.get() > MAX_SDU_BYTES
                    {
                        return Err(BearerError::Capacity);
                    }
                    (
                        PreparedEntity::Acknowledged { config, limits },
                        uplink.maximum_bits,
                        config.transmit_pdu_bytes.get(),
                        bytes,
                    )
                }
                (
                    Rel99Rlc::Unacknowledged,
                    UserRlcPolicy::Unacknowledged {
                        uplink: receive_limits,
                        downlink: transmit_limits,
                    },
                ) => {
                    let receive_limits =
                        um_limits(uplink, receive_limits, UM_MINIMUM_RECEIVE_OCTETS)?;
                    let transmit_limits =
                        um_limits(downlink, transmit_limits, UM_MINIMUM_TRANSMIT_OCTETS)?;
                    // TS 25.322 9.2.2.8 uses the largest configured UL PDU, not each received PDU.
                    let receive = UmConfig {
                        li_width: if receive_limits.maximum_pdu_bytes.get()
                            <= SEVEN_BIT_UM_MAXIMUM_OCTETS
                        {
                            LiWidth::Seven
                        } else {
                            LiWidth::Fifteen
                        },
                        interpretation: EInterpretation::Normal,
                    };
                    // TS 25.331 8.6.4 uses seven-bit DL LI and normal E when the optional fields are absent.
                    let transmit = UmConfig {
                        li_width: LiWidth::Seven,
                        interpretation: EInterpretation::Normal,
                    };
                    if transmit_limits.maximum_pdu_bytes.get() > SEVEN_BIT_UM_MAXIMUM_OCTETS {
                        return Err(BearerError::UnsupportedBinding);
                    }
                    receive_limits.validate(receive).map_err(BearerError::Um)?;
                    transmit_limits
                        .validate(transmit)
                        .map_err(BearerError::Um)?;
                    let rx = receive_limits
                        .receive_buffer_bytes()
                        .map_err(BearerError::Um)?;
                    let tx = transmit_limits
                        .transmit_buffer_bytes()
                        .map_err(BearerError::Um)?;
                    if rx > receive_limits.maximum_buffer_bytes
                        || tx > transmit_limits.maximum_buffer_bytes
                    {
                        return Err(BearerError::Capacity);
                    }
                    (
                        PreparedEntity::Unacknowledged {
                            receive,
                            transmit,
                            uplink: receive_limits,
                            downlink: transmit_limits,
                        },
                        uplink.maximum_bits,
                        transmit_limits.maximum_pdu_bytes.get(),
                        rx.checked_add(tx).ok_or(BearerError::Capacity)?,
                    )
                }
                (
                    Rel99Rlc::Transparent {
                        uplink_segmentation,
                        downlink_segmentation,
                    },
                    UserRlcPolicy::Transparent {
                        errors,
                        maximum_sdu_bits,
                    },
                ) => {
                    let required_ul = if *uplink_segmentation {
                        uplink.maximum_tti_bits
                    } else {
                        uplink.maximum_bits
                    };
                    let required_dl = if *downlink_segmentation {
                        downlink.maximum_tti_bits
                    } else {
                        downlink.maximum_bits
                    };
                    if maximum_sdu_bits.get() < required_ul.max(required_dl)
                        || maximum_sdu_bits.get() > umts_security::algorithms::MAX_F8_BITS
                    {
                        return Err(BearerError::Capacity);
                    }
                    let settings = TransparentSettings {
                        uplink_segmentation: segmentation(*uplink_segmentation),
                        downlink_segmentation: segmentation(*downlink_segmentation),
                        errors,
                        limits: TmLimits {
                            maximum_pdus: nonzero(
                                uplink.maximum_blocks.max(downlink.maximum_blocks).max(1),
                            )?,
                            // TS 25.212 4.2.1.1 permits zero-size TM blocks. This is a storage ceiling, not a wire width.
                            maximum_pdu_bits: nonzero(
                                uplink.maximum_bits.max(downlink.maximum_bits).max(1),
                            )?,
                            maximum_sdu_bits,
                        },
                    };
                    let bits = settings
                        .limits
                        .maximum_pdus
                        .get()
                        .checked_mul(settings.limits.maximum_pdu_bits.get())
                        .filter(|bits| *bits <= umts_security::algorithms::MAX_F8_BITS)
                        .ok_or(BearerError::Capacity)?;
                    (
                        PreparedEntity::Transparent(settings),
                        bits,
                        bits.div_ceil(BITS_PER_BYTE),
                        maximum_sdu_bits.get().div_ceil(BITS_PER_BYTE),
                    )
                }
                _ => return Err(BearerError::UnsupportedBinding),
            };
            let endpoint_bytes =
                BearerEndpoint::storage_requirement(rx_bits, tx_bytes, entity_bytes)?;
            storage_bytes = storage_bytes
                .checked_add(endpoint_bytes)
                .filter(|bytes| *bytes <= maximum_storage_bytes.get())
                .ok_or(BearerError::Capacity)?;
            endpoints.push(PreparedEndpoint {
                id: *id,
                scope,
                entity,
                receive_bits: rx_bits,
                transmit_bytes: tx_bytes,
                storage_bytes: endpoint_bytes,
                maximum_sdu_bytes: match entity {
                    PreparedEntity::Acknowledged { limits, .. } => limits.maximum_sdu_bytes.get(),
                    PreparedEntity::Unacknowledged { uplink, .. } => uplink.maximum_sdu_bytes.get(),
                    PreparedEntity::Transparent(settings) => settings
                        .limits
                        .maximum_sdu_bits
                        .get()
                        .div_ceil(BITS_PER_BYTE),
                },
            });
        }
        Ok(Self {
            plan,
            endpoints,
            storage_bytes,
        })
    }

    pub fn plan(&self) -> &Arc<CompiledRel99> {
        &self.plan
    }
    #[cfg(test)]
    pub fn storage_bytes(&self) -> usize {
        self.storage_bytes
    }

    /// START must be the setup response's domain value, not the security command's initialization.
    pub fn instantiate(
        &self,
        key: UeKey,
        starts: [Option<Start>; 2],
        now: ChipInstant,
    ) -> Result<Vec<BearerEndpoint>, BearerError> {
        if now.epoch != key.epoch {
            return Err(BearerError::Time);
        }
        let mut result = Vec::with_capacity(self.endpoints.len());
        for prepared in &self.endpoints {
            let endpoint = match prepared.entity {
                PreparedEntity::Acknowledged { mut config, limits } => {
                    let Scope::User(domain) = prepared.scope else {
                        unreachable!("user profile")
                    };
                    let start = starts[match domain {
                        umts_types::CnDomain::Cs => 0,
                        umts_types::CnDomain::Ps => 1,
                    }]
                    .ok_or(BearerError::Phase)?;
                    config.initial_tx_hfn = start.get();
                    config.initial_rx_hfn = start.get();
                    BearerEndpoint::acknowledged(
                        key,
                        prepared.id,
                        prepared.scope,
                        AmEntity::new(config, limits, now).map_err(BearerError::Am)?,
                    )
                    .map_err(|rejected| rejected.error)?
                }
                PreparedEntity::Unacknowledged {
                    receive,
                    transmit,
                    uplink,
                    downlink,
                } => {
                    let Scope::User(domain) = prepared.scope else {
                        unreachable!("user profile")
                    };
                    let start = starts[match domain {
                        umts_types::CnDomain::Cs => 0,
                        umts_types::CnDomain::Ps => 1,
                    }]
                    .ok_or(BearerError::Phase)?;
                    let position = u64::from(start.initial_count());
                    BearerEndpoint::unacknowledged(
                        key,
                        prepared.id,
                        prepared.scope,
                        UmTransmitter::with_initial_position(transmit, downlink, position)
                            .map_err(BearerError::Um)?,
                        UmReceiver::with_initial_position(receive, uplink, position)
                            .map_err(BearerError::Um)?,
                    )
                    .map_err(|rejected| rejected.error)?
                }
                PreparedEntity::Transparent(settings) => {
                    BearerEndpoint::transparent(key, prepared.id, prepared.scope, settings)?
                }
            };
            result.push(endpoint);
        }
        Ok(result)
    }
}

fn same_transport(before: &DchTransport, after: &DchTransport) -> bool {
    before.id == after.id
        && before.interval == after.interval
        && before.coding == after.coding
        && before.crc == after.crc
        && before.rate_matching_attribute == after.rate_matching_attribute
        && before.groups.len() == after.groups.len()
        && before
            .groups
            .iter()
            .zip(&after.groups)
            .all(|(before, after)| {
                before.rlc_bits == after.rlc_bits && before.blocks == after.blocks
            })
}

fn segmentation(enabled: bool) -> Segmentation {
    if enabled {
        Segmentation::Enabled
    } else {
        Segmentation::Disabled
    }
}
fn nonzero(value: usize) -> Result<NonZeroUsize, BearerError> {
    NonZeroUsize::new(value).ok_or(BearerError::Transport)
}
fn fixed_octets(geometry: Geometry) -> Result<NonZeroUsize, BearerError> {
    if geometry.minimum_bits != Some(geometry.maximum_bits)
        || !geometry.maximum_bits.is_multiple_of(BITS_PER_BYTE)
    {
        return Err(BearerError::UnsupportedBinding);
    }
    nonzero(geometry.maximum_bits / BITS_PER_BYTE)
}
fn um_limits(
    geometry: Geometry,
    mut limits: UmLimits,
    minimum_octets: usize,
) -> Result<UmLimits, BearerError> {
    let minimum = geometry.minimum_bits.ok_or(BearerError::Transport)?;
    if !minimum.is_multiple_of(BITS_PER_BYTE)
        || !geometry.maximum_bits.is_multiple_of(BITS_PER_BYTE)
        || minimum / BITS_PER_BYTE < minimum_octets
    {
        return Err(BearerError::UnsupportedBinding);
    }
    let maximum = nonzero(geometry.maximum_bits / BITS_PER_BYTE)?;
    if maximum > limits.maximum_pdu_bytes || limits.maximum_sdu_bytes.get() > MAX_SDU_BYTES {
        return Err(BearerError::Capacity);
    }
    limits.maximum_pdu_bytes = maximum;
    Ok(limits)
}
fn geometry(
    plan: &CompiledRel99,
    rb: &Rel99Bearer,
    direction: DchDirection,
) -> Result<Geometry, BearerError> {
    let formats = match direction {
        DchDirection::Uplink => plan.uplink_configured(),
        DchDirection::Downlink => plan.downlink_configured(),
    };
    let mut result = Geometry {
        minimum_bits: None,
        maximum_bits: 0,
        maximum_blocks: 0,
        maximum_tti_bits: 0,
    };
    for mapping in rb.mappings(direction) {
        let definition = formats
            .definition(TransportId::new(mapping.transport).map_err(|_| BearerError::Transport)?)
            .ok_or(BearerError::Transport)?;
        for format in &definition.formats {
            if format.blocks > MAX_BLOCKS {
                return Err(BearerError::Capacity);
            }
            result.maximum_blocks = result.maximum_blocks.max(format.blocks);
            // A declared block size with zero blocks still fixes the RLC PDU size: that is a
            // downlink channel whose data rides HS-DSCH only.
            if format.block_bits == 0 {
                continue;
            }
            let bits = format
                .block_bits
                .checked_sub(if mapping.logical.is_some() {
                    C_T_BITS
                } else {
                    0
                })
                .filter(|bits| *bits != 0)
                .ok_or(BearerError::Transport)?;
            result.minimum_bits = Some(result.minimum_bits.map_or(bits, |old| old.min(bits)));
            result.maximum_bits = result.maximum_bits.max(bits);
            result.maximum_blocks = result.maximum_blocks.max(format.blocks);
            result.maximum_tti_bits = result.maximum_tti_bits.max(
                bits.checked_mul(format.blocks)
                    .ok_or(BearerError::Capacity)?,
            );
        }
    }
    if result.maximum_bits == 0 && !matches!(rb.rlc, Rel99Rlc::Transparent { .. }) {
        return Err(BearerError::Transport);
    }
    Ok(result)
}

pub(super) fn uplink_window(window: ie::TransmissionWindowSize) -> Result<NonZeroU16, BearerError> {
    use ie::TransmissionWindowSize::*;
    window_size(match window {
        tw1 => 1,
        tw8 => 8,
        tw16 => 16,
        tw32 => 32,
        tw64 => 64,
        tw128 => 128,
        tw256 => 256,
        tw512 => 512,
        tw768 => 768,
        tw1024 => 1024,
        tw1536 => 1536,
        tw2047 => 2047,
        _ => return Err(BearerError::UnsupportedBinding),
    })
}
pub(super) fn downlink_window(window: ie::ReceivingWindowSize) -> Result<NonZeroU16, BearerError> {
    use ie::ReceivingWindowSize::*;
    window_size(match window {
        rw1 => 1,
        rw8 => 8,
        rw16 => 16,
        rw32 => 32,
        rw64 => 64,
        rw128 => 128,
        rw256 => 256,
        rw512 => 512,
        rw768 => 768,
        rw1024 => 1024,
        rw1536 => 1536,
        rw2047 => 2047,
        _ => return Err(BearerError::UnsupportedBinding),
    })
}
fn window_size(value: u16) -> Result<NonZeroU16, BearerError> {
    NonZeroU16::new(value).ok_or(BearerError::Mapping)
}

#[cfg(test)]
mod additional_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod um_tests;
