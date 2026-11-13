// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::collections::BTreeSet;
mod physical;
mod restoration;
mod selection;
use physical::{DownlinkBudget, UplinkBudget};
use umts_rrc::capabilities::rel99::{DirectionLimits, TransportUsage};
use umts_rrc::dedicated::{
    BearerMapping, Combination, DchDirection, DchFormatGroup, DchTfcs, DchTransport,
    LogicalMapping, Rel99Bearer, UplinkLogicalMapping,
};

const FIRST_USER_BEARER: u8 = 5;
const MAX_BEARER: u8 = 32;
const MAX_TRANSPORT: u8 = 32;
const MAX_RETAINED_SRBS: usize = 4;
const MAX_SIGNALLING_BLOCKS: u16 = 31;
const SIGNALLING_PRIORITY: u8 = 1;
const CIRCUIT_PRIORITY: u8 = 2;
const PACKET_PRIORITY: u8 = 3;

#[derive(Clone, Copy)]
enum GroupPolicy {
    Retain,
    Signalling,
    Packet { minimum: [u16; 2] },
}

impl GroupPolicy {
    fn retain_all(self) -> bool {
        matches!(self, Self::Retain)
    }
}

struct Group {
    uplink: Vec<DchTransport>,
    downlink: Vec<DchTransport>,
    ul_rows: Vec<Combination>,
    dl_rows: Vec<Combination>,
    policy: GroupPolicy,
}
#[derive(Clone)]
struct ProductRow {
    combination: Combination,
    added_active: bool,
}
pub(super) struct Composition {
    groups: Vec<Group>,
    bearers: Vec<Rel99Bearer>,
    used_rb: BTreeSet<u8>,
    used_ul_transport: BTreeSet<u8>,
    used_dl_transport: BTreeSet<u8>,
    existing: bool,
}
fn transport(
    id: u8,
    interval: TransportInterval,
    coding: Coding,
    groups: Vec<DchFormatGroup>,
) -> DchTransport {
    DchTransport {
        id,
        interval,
        coding: coding.channel,
        crc: coding.crc,
        rate_matching_attribute: coding.rate_matching_attribute,
        groups,
    }
}
fn mapping(uplink: u8, downlink: u8, logical: Option<u8>, priority: u8) -> BearerMapping {
    BearerMapping {
        uplink: vec![UplinkLogicalMapping {
            transport: uplink,
            logical,
            priority,
        }],
        downlink: vec![LogicalMapping {
            transport: downlink,
            logical,
        }],
    }
}
impl Composition {
    pub(super) fn new(
        retained: &[RetainedBearer],
        policy: SignallingProfile,
    ) -> Result<Self, Error> {
        if retained.is_empty()
            || retained.len() > MAX_RETAINED_SRBS
            || policy.maximum_blocks == 0
            || policy.maximum_blocks > MAX_SIGNALLING_BLOCKS
        {
            return Err(Error::Capacity);
        }
        let mut result = Self {
            groups: Vec::new(),
            bearers: Vec::new(),
            used_rb: BTreeSet::new(),
            used_ul_transport: BTreeSet::new(),
            used_dl_transport: BTreeSet::new(),
            existing: false,
        };
        let mut sizes: Vec<(u16, u16, u8)> = Vec::new();
        for old in retained {
            if old.domain.is_some()
                || old.id == 0
                || old.id >= FIRST_USER_BEARER
                || result.bearers.iter().any(|bearer| bearer.id == old.id)
            {
                return Err(super::trace_identity("compose-1"));
            }
            let uplink_bits =
                if old.uplink_pdu_bits == 0 && matches!(old.rlc, Rel99Rlc::Unacknowledged) {
                    policy.um_uplink_bits
                } else {
                    old.uplink_pdu_bits
                };
            let downlink_bits =
                if old.downlink_pdu_bits == 0 && matches!(old.rlc, Rel99Rlc::Unacknowledged) {
                    policy.um_downlink_bits
                } else {
                    old.downlink_pdu_bits
                };
            if uplink_bits == 0 || downlink_bits == 0 {
                return Err(Error::Unsupported);
            }
            let id = match sizes
                .iter()
                .find(|(ul, dl, _)| (*ul, *dl) == (uplink_bits, downlink_bits))
            {
                Some((_, _, id)) => *id,
                None => {
                    let (id, downlink_id) = result.transport_ids()?;
                    let counts: Vec<u16> = (0..=policy.maximum_blocks).collect();
                    let rows: Vec<_> = (0..counts.len())
                        .map(|index| Combination {
                            formats: vec![index],
                            gain: None,
                        })
                        .collect();
                    result.groups.push(Group {
                        policy: GroupPolicy::Signalling,
                        uplink: vec![transport(
                            id,
                            policy.interval,
                            policy.uplink,
                            vec![DchFormatGroup {
                                rlc_bits: uplink_bits,
                                blocks: counts.clone(),
                            }],
                        )],
                        downlink: vec![transport(
                            downlink_id,
                            policy.interval,
                            policy.downlink,
                            vec![DchFormatGroup {
                                rlc_bits: downlink_bits,
                                blocks: counts,
                            }],
                        )],
                        ul_rows: rows.clone(),
                        dl_rows: rows,
                    });
                    sizes.push((uplink_bits, downlink_bits, id));
                    id
                }
            };
            result.bearers.push(Rel99Bearer {
                id: old.id,
                domain: None,
                rlc: old.rlc.clone(),
                mapping: mapping(id, id, Some(old.id), SIGNALLING_PRIORITY),
            });
            result.used_rb.insert(old.id);
        }
        Ok(result)
    }
    pub(super) fn from_existing(configuration: &Rel99Configuration) -> Result<Self, Error> {
        Ok(Self {
            groups: vec![Group {
                policy: GroupPolicy::Retain,
                uplink: configuration.uplink().transports().to_vec(),
                downlink: configuration.downlink().transports().to_vec(),
                ul_rows: configuration.uplink().combinations().to_vec(),
                dl_rows: configuration.downlink().combinations().to_vec(),
            }],
            bearers: configuration.bearers().to_vec(),
            used_rb: configuration
                .bearers()
                .iter()
                .map(|bearer| bearer.id)
                .collect(),
            used_ul_transport: configuration
                .uplink()
                .transports()
                .iter()
                .map(|channel| channel.id)
                .collect(),
            used_dl_transport: configuration
                .downlink()
                .transports()
                .iter()
                .map(|channel| channel.id)
                .collect(),
            existing: true,
        })
    }
    fn transport_ids(&mut self) -> Result<(u8, u8), Error> {
        let uplink = (1..=MAX_TRANSPORT)
            .find(|id| !self.used_ul_transport.contains(id))
            .ok_or(Error::Capacity)?;
        let downlink = (1..=MAX_TRANSPORT)
            .find(|id| !self.used_dl_transport.contains(id))
            .ok_or(Error::Capacity)?;
        self.used_ul_transport.insert(uplink);
        self.used_dl_transport.insert(downlink);
        Ok((uplink, downlink))
    }
    fn bearer_id(&mut self) -> Result<u8, Error> {
        let id = (FIRST_USER_BEARER..=MAX_BEARER)
            .find(|id| !self.used_rb.contains(id))
            .ok_or(Error::Capacity)?;
        self.used_rb.insert(id);
        Ok(id)
    }
    #[cfg(test)]
    pub(super) fn packet(
        &mut self,
        profile: &PacketProfile,
        blocks: [u16; 2],
    ) -> Result<u8, Error> {
        self.packet_with_minimum(profile, blocks, [0, 0])
    }

    pub(super) fn packet_with_minimum(
        &mut self,
        profile: &PacketProfile,
        blocks: [u16; 2],
        minimum: [u16; 2],
    ) -> Result<u8, Error> {
        if minimum
            .iter()
            .zip(blocks)
            .any(|(minimum, maximum)| *minimum > maximum)
        {
            return Err(super::trace_identity("compose-2"));
        }
        let (uplink_id, downlink_id) = self.transport_ids()?;
        let rb = self.bearer_id()?;
        let make = |id, channel: PacketChannel, count: u16| {
            transport(
                id,
                channel.interval,
                channel.coding,
                vec![DchFormatGroup {
                    rlc_bits: channel.rlc_bits,
                    blocks: (0..=count).collect(),
                }],
            )
        };
        self.groups.push(Group {
            policy: GroupPolicy::Packet { minimum },
            uplink: vec![make(uplink_id, profile.uplink, blocks[0])],
            downlink: vec![make(downlink_id, profile.downlink, blocks[1])],
            ul_rows: (0..=usize::from(blocks[0]))
                .map(|index| Combination {
                    formats: vec![index],
                    gain: None,
                })
                .collect(),
            dl_rows: (0..=usize::from(blocks[1]))
                .map(|index| Combination {
                    formats: vec![index],
                    gain: None,
                })
                .collect(),
        });
        self.bearers.push(Rel99Bearer {
            id: rb,
            domain: Some(CnDomain::Ps),
            rlc: profile.rlc.clone(),
            mapping: mapping(uplink_id, downlink_id, None, PACKET_PRIORITY),
        });
        Ok(rb)
    }
    pub(super) fn circuit(
        &mut self,
        requirements: &Requirements<'_>,
        profile: &CircuitProfile,
    ) -> Result<Vec<u8>, Error> {
        let mut group = Group {
            policy: GroupPolicy::Retain,
            uplink: Vec::new(),
            downlink: Vec::new(),
            ul_rows: Vec::new(),
            dl_rows: Vec::new(),
        };
        let mut rows = vec![vec![0; profile.subflows.len()]; requirements.combinations.len() + 1];
        let mut ids = Vec::with_capacity(profile.subflows.len());
        for (index, subflow) in profile.subflows.iter().enumerate() {
            let (uplink_id, downlink_id) = self.transport_ids()?;
            let rb = self.bearer_id()?;
            ids.push(rb);
            let nonzero: Vec<u16> = requirements
                .combinations
                .iter()
                .map(|row| row.subflow_bits[index])
                .filter(|bits| *bits != 0)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let mut formats = match subflow.empty {
                EmptyTransport::NoBlock => vec![DchFormatGroup {
                    rlc_bits: nonzero.first().copied().unwrap_or(0),
                    blocks: vec![0],
                }],
                EmptyTransport::ZeroSizeBlock => vec![DchFormatGroup {
                    rlc_bits: 0,
                    blocks: vec![1],
                }],
            };
            for bits in nonzero {
                if formats[0].rlc_bits == bits {
                    formats[0].blocks.push(1)
                } else {
                    formats.push(DchFormatGroup {
                        rlc_bits: bits,
                        blocks: vec![1],
                    })
                }
            }
            let ul = transport(uplink_id, profile.interval, subflow.uplink, formats.clone());
            let dl = transport(downlink_id, profile.interval, subflow.downlink, formats);
            for (row, request) in rows.iter_mut().skip(1).zip(&requirements.combinations) {
                let size = request.subflow_bits[index];
                row[index] = if size == 0 {
                    0
                } else {
                    ul.formats()
                        .position(|format| format.rlc_bits == size && format.blocks == 1)
                        .ok_or(Error::Capacity)?
                };
            }
            group.uplink.push(ul);
            group.downlink.push(dl);
            self.bearers.push(Rel99Bearer {
                id: rb,
                domain: Some(CnDomain::Cs),
                rlc: Rel99Rlc::Transparent {
                    uplink_segmentation: false,
                    downlink_segmentation: false,
                },
                mapping: mapping(uplink_id, downlink_id, None, CIRCUIT_PRIORITY),
            });
        }
        let mut seen = BTreeSet::new();
        rows.retain(|row| seen.insert(row.clone()));
        group.ul_rows = rows
            .iter()
            .map(|formats| Combination {
                formats: formats.clone(),
                gain: None,
            })
            .collect();
        group.dl_rows = group.ul_rows.clone();
        self.groups.push(group);
        Ok(ids)
    }
    #[cfg(test)]
    fn direction(
        &self,
        direction: DchDirection,
        limit: usize,
        gain: GainFactors,
    ) -> Result<DchTfcs, Error> {
        self.direction_budgeted(direction, limit, gain, None)
    }

    #[cfg(test)]
    fn direction_budgeted(
        &self,
        direction: DchDirection,
        limit: usize,
        gain: GainFactors,
        limits: Option<DirectionLimits>,
    ) -> Result<DchTfcs, Error> {
        self.direction_planned(direction, limit, gain, limits, None, None)
    }

    fn direction_planned(
        &self,
        direction: DchDirection,
        limit: usize,
        gain: GainFactors,
        limits: Option<DirectionLimits>,
        uplink: Option<&UplinkBudget>,
        downlink: Option<&DownlinkBudget>,
    ) -> Result<DchTfcs, Error> {
        let product = self.groups.iter().try_fold(1usize, |count, group| {
            count.checked_mul(match direction {
                DchDirection::Uplink => group.ul_rows.len(),
                DchDirection::Downlink => group.dl_rows.len(),
            })
        });
        let complete = if product.is_some_and(|count| count <= limit) {
            Some(self.product_rows(direction, limit, gain)?)
        } else {
            None
        };
        let rows = if let Some(rows) = complete.filter(|rows| {
            limits.is_none_or(|limits| {
                let channels: Vec<_> = self
                    .groups
                    .iter()
                    .flat_map(|group| match direction {
                        DchDirection::Uplink => &group.uplink,
                        DchDirection::Downlink => &group.downlink,
                    })
                    .collect();
                channels.len() <= limits.transport_channels
                    && channels
                        .iter()
                        .map(|channel| channel.formats().count())
                        .sum::<usize>()
                        <= limits.formats
                    && rows.iter().all(|row| {
                        self.usage(direction, &row.combination.formats)
                            .is_ok_and(|usage| limits.validate_usage(direction, usage).is_ok())
                    })
            }) && uplink.is_none_or(|budget| {
                rows.iter().all(|row| {
                    self.uplink_inputs(&row.combination.formats, budget)
                        .and_then(|inputs| budget.check(inputs))
                        .is_ok()
                })
            }) && downlink.is_none_or(|budget| budget.check(self, rows).is_ok())
        }) {
            rows
        } else {
            selection::budgeted_rows(self, direction, limit, gain, limits, uplink, downlink)?
        };
        self.assemble(direction, rows)
    }

    fn assemble(&self, direction: DchDirection, rows: Vec<ProductRow>) -> Result<DchTfcs, Error> {
        let transports: Vec<_> = self
            .groups
            .iter()
            .flat_map(|group| match direction {
                DchDirection::Uplink => &group.uplink,
                DchDirection::Downlink => &group.downlink,
            })
            .cloned()
            .collect();
        let mut order: Vec<usize> = (0..transports.len()).collect();
        order.sort_by_key(|&index| transports[index].id);
        let sorted = order
            .iter()
            .map(|&index| transports[index].clone())
            .collect();
        let combinations = rows
            .into_iter()
            .map(|row| Combination {
                formats: order
                    .iter()
                    .map(|&index| row.combination.formats[index])
                    .collect(),
                gain: row.combination.gain,
            })
            .collect();
        let (sorted, combinations) = self.trim_unused(direction, sorted, combinations)?;
        Ok(DchTfcs::new(direction, sorted, combinations)?)
    }

    fn usage(&self, direction: DchDirection, formats: &[usize]) -> Result<TransportUsage, Error> {
        let mut cursor = 0;
        let mut usage = TransportUsage::default();
        for group in &self.groups {
            let channels = match direction {
                DchDirection::Uplink => &group.uplink,
                DchDirection::Downlink => &group.downlink,
            };
            for channel in channels {
                let index = *formats.get(cursor).ok_or_else(|| {
                    tracing::debug!(
                        "compose usage mismatch: direction={direction:?} formats={formats:?} groups={:?}",
                        self.groups
                            .iter()
                            .map(|group| match direction {
                                DchDirection::Uplink => group.uplink.iter().map(|channel| channel.id).collect::<Vec<_>>(),
                                DchDirection::Downlink => group.downlink.iter().map(|channel| channel.id).collect::<Vec<_>>(),
                            })
                            .collect::<Vec<_>>()
                    );
                    super::trace_identity("compose-3")
                })?;
                usage = usage
                    .checked_add(self.channel_usage(direction, channel, index)?)
                    .map_err(Error::Capability)?;
                cursor += 1;
            }
        }
        if cursor != formats.len() {
            return Err(super::trace_identity("compose-4"));
        }
        Ok(usage)
    }

    fn channel_usage(
        &self,
        direction: DchDirection,
        channel: &DchTransport,
        index: usize,
    ) -> Result<TransportUsage, Error> {
        let format = channel.formats().nth(index).ok_or_else(|| {
            tracing::debug!(
                "compose format mismatch: direction={direction:?} transport={} index={index} formats={}",
                channel.id,
                channel.formats().count()
            );
            super::trace_identity("compose-5")
        })?;
        let multiplexed = self.multiplexed(direction, channel.id);
        let bits = format
            .transport_bits(multiplexed)
            .checked_mul(usize::from(format.blocks))
            .ok_or(Error::Capacity)?;
        Ok(TransportUsage::transport(
            channel.coding,
            bits,
            usize::from(format.blocks),
        ))
    }

    fn multiplexed(&self, direction: DchDirection, id: u8) -> bool {
        self.bearers
            .iter()
            .flat_map(|bearer| bearer.mappings(direction))
            .filter(|mapping| mapping.transport == id)
            .take(2)
            .count()
            > 1
    }

    fn uplink_inputs(
        &self,
        formats: &[usize],
        budget: &UplinkBudget,
    ) -> Result<Vec<umts_dsp::dedicated_transport::UplinkRateInput>, Error> {
        self.groups
            .iter()
            .flat_map(|group| &group.uplink)
            .zip(formats)
            .map(|(channel, index)| {
                budget.input(
                    channel,
                    *index,
                    self.multiplexed(DchDirection::Uplink, channel.id),
                )
            })
            .collect()
    }

    fn trim_unused(
        &self,
        direction: DchDirection,
        mut transports: Vec<DchTransport>,
        mut combinations: Vec<Combination>,
    ) -> Result<(Vec<DchTransport>, Vec<Combination>), Error> {
        let fixed: BTreeSet<_> = self
            .groups
            .iter()
            .filter(|group| group.policy.retain_all())
            .flat_map(|group| match direction {
                DchDirection::Uplink => &group.uplink,
                DchDirection::Downlink => &group.downlink,
            })
            .map(|channel| channel.id)
            .collect();
        for (column, channel) in transports.iter_mut().enumerate() {
            if fixed.contains(&channel.id) {
                continue;
            }
            let used: BTreeSet<_> = combinations.iter().map(|row| row.formats[column]).collect();
            let mut remapping = vec![None; channel.formats().count()];
            let (mut old, mut new) = (0, 0);
            for group in &mut channel.groups {
                group.blocks.retain(|_| {
                    let keep = used.contains(&old);
                    if keep {
                        remapping[old] = Some(new);
                        new += 1;
                    }
                    old += 1;
                    keep
                });
            }
            channel.groups.retain(|group| !group.blocks.is_empty());
            for row in &mut combinations {
                row.formats[column] = remapping
                    .get(row.formats[column])
                    .copied()
                    .flatten()
                    .ok_or_else(|| {
                        tracing::debug!(
                            "compose trim mismatch: direction={direction:?} column={column} transport={} row_formats={:?} remapping={remapping:?} used={used:?} fixed={fixed:?}",
                            channel.id, row.formats
                        );
                        super::trace_identity("compose-6")
                    })?;
            }
        }
        Ok((transports, combinations))
    }
    fn product_rows(
        &self,
        direction: DchDirection,
        limit: usize,
        gain: GainFactors,
    ) -> Result<Vec<ProductRow>, Error> {
        let mut rows = vec![ProductRow {
            combination: Combination {
                formats: Vec::new(),
                gain: (direction == DchDirection::Uplink).then_some(gain),
            },
            added_active: false,
        }];
        for (group_index, group) in self.groups.iter().enumerate() {
            let (channels, choices) = match direction {
                DchDirection::Uplink => (&group.uplink, &group.ul_rows),
                DchDirection::Downlink => (&group.downlink, &group.dl_rows),
            };
            let count = rows
                .len()
                .checked_mul(choices.len())
                .filter(|count| *count <= limit)
                .ok_or(Error::Capacity)?;
            let mut next = Vec::with_capacity(count);
            let mut append = |old: &ProductRow, choice: &Combination, choice_active: bool| {
                next.push(join_row(old, choice, direction, gain, choice_active));
            };
            // TS 25.331 8.6.5.2 treats synchronous TM subflows as one set, not independent TFS choices.
            if self.existing {
                for choice in choices {
                    let active = group_index != 0
                        && channels
                            .iter()
                            .zip(&choice.formats)
                            .any(|(channel, index)| {
                                channel.formats().nth(*index).is_some_and(|format| {
                                    format.blocks != 0
                                        && (format.rlc_bits != 0
                                            || !matches!(channel.crc, CrcSize::None))
                                })
                            });
                    for old in &rows {
                        append(old, choice, active);
                    }
                }
            } else {
                for old in &rows {
                    for choice in choices {
                        append(old, choice, false);
                    }
                }
            }
            rows = next;
        }
        Ok(rows)
    }
    pub(super) fn configuration(
        &self,
        physical: NormalDpch,
        capabilities: &Rel99Limits,
        policy: &Policy<'_>,
    ) -> Result<Rel99Configuration, Error> {
        let limit = policy.maximum_combinations.get();
        let uplink = UplinkBudget::new(&physical, policy.transport.maximum_coded_bits)?;
        let ul = self.direction_planned(
            DchDirection::Uplink,
            limit.min(capabilities.uplink.combinations),
            policy.uplink_gains,
            Some(capabilities.uplink),
            Some(&uplink),
            None,
        )?;
        let downlink = DownlinkBudget::new(&physical, policy.transport)?;
        let dl = self.direction_planned(
            DchDirection::Downlink,
            limit.min(capabilities.downlink.combinations),
            policy.uplink_gains,
            Some(capabilities.downlink),
            None,
            Some(&downlink),
        )?;
        let mut bearers = self.bearers.clone();
        bearers.sort_by_key(|bearer| bearer.id);
        Ok(Rel99Configuration::new(ul, dl, bearers, physical)?)
    }
}

fn join_row(
    old: &ProductRow,
    choice: &Combination,
    direction: DchDirection,
    gain: GainFactors,
    choice_active: bool,
) -> ProductRow {
    let mut formats = Vec::with_capacity(old.combination.formats.len() + choice.formats.len());
    formats.extend_from_slice(&old.combination.formats);
    formats.extend_from_slice(&choice.formats);
    let added_active = old.added_active || choice_active;
    // Existing gains apply only when added TrCHs emit no bits, including CRC parity on zero-size blocks.
    let selected_gain = if added_active {
        (direction == DchDirection::Uplink).then_some(gain)
    } else {
        choice.gain.or(old.combination.gain)
    };
    ProductRow {
        combination: Combination {
            formats,
            gain: selected_gain,
        },
        added_active,
    }
}

#[cfg(test)]
mod tests;
