// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    FixedDownlink, FixedDownlinkPlan, FixedDownlinkSettings, FlexibleDownlink,
    FlexibleDownlinkPlan, PuncturingLimit, TransportId, TransportPlanLimits, UplinkAllocation,
    UplinkRateInput, UplinkTransportPlan, allocation::MAX_TRANSPORTS,
};
use crate::{
    DspError,
    coding::{CodedBlockFormat, CodedBlockPlan},
    dedicated::{DownlinkFormat, UplinkLayout},
    interleaving::Rel99Tti,
    tfci::{Tfci, TfciDecoder},
};
use std::{
    collections::BTreeSet,
    num::{NonZeroU16, NonZeroUsize},
    sync::Arc,
};
use umts_types::time::SLOTS_PER_FRAME;

const MAX_FORMATS: usize = 32;
const MAX_COMBINATIONS: usize = 1024;
const MAX_ATTRIBUTE: u16 = 256;
const FRAME_DENOMINATOR: u128 = 8;

pub struct TransportDefinition {
    pub id: TransportId,
    pub interval: Rel99Tti,
    pub attribute: NonZeroU16,
    pub formats: Vec<CodedBlockFormat>,
}
pub struct CombinationDefinition {
    pub tfci: Tfci,
    pub formats: Vec<usize>,
}

struct Transport {
    definition: TransportDefinition,
    blocks: Vec<CodedBlockPlan>,
}
pub struct ConfiguredTfcs {
    transports: Vec<Transport>,
    combinations: Vec<CombinationDefinition>,
    total_coded: usize,
}
impl ConfiguredTfcs {
    pub fn new(
        definitions: Vec<TransportDefinition>,
        combinations: Vec<CombinationDefinition>,
        maximum_total_coded_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if definitions.is_empty()
            || definitions.len() > MAX_TRANSPORTS
            || definitions.windows(2).any(|pair| pair[0].id >= pair[1].id)
            || combinations.is_empty()
            || combinations.len() > MAX_COMBINATIONS
            || combinations
                .windows(2)
                .any(|pair| pair[0].tfci.index() >= pair[1].tfci.index())
            || definitions.iter().any(|definition| {
                definition.formats.is_empty()
                    || definition.formats.len() > MAX_FORMATS
                    || definition.attribute.get() > MAX_ATTRIBUTE
            })
        {
            return Err(DspError::InvalidCode);
        }
        let mut unique = BTreeSet::new();
        for combination in &combinations {
            if combination.formats.len() != definitions.len()
                || combination
                    .formats
                    .iter()
                    .zip(&definitions)
                    .any(|(&index, definition)| index >= definition.formats.len())
                || !unique.insert(combination.formats.as_slice())
            {
                return Err(DspError::InvalidCode);
            }
        }
        let mut total_coded = 0_usize;
        let mut transports = Vec::with_capacity(definitions.len());
        for definition in definitions {
            let mut blocks = Vec::with_capacity(definition.formats.len());
            let mut seen = BTreeSet::new();
            for format in &definition.formats {
                // A DCH TFS has semi-static coding/CRC. Dynamic formats change payload size/count only.
                let first = definition.formats[0];
                if coding_key(format) != coding_key(&first)
                    || !seen.insert((format.block_bits, format.blocks))
                {
                    return Err(DspError::InvalidCode);
                }
                let plan = CodedBlockPlan::new(*format, maximum_total_coded_bits)?;
                total_coded = total_coded
                    .checked_add(plan.coded_bits())
                    .filter(|total| *total <= maximum_total_coded_bits.get())
                    .ok_or(DspError::Capacity)?;
                blocks.push(plan);
            }
            transports.push(Transport { definition, blocks });
        }
        Ok(Self {
            transports,
            combinations,
            total_coded,
        })
    }
    pub fn transport_count(&self) -> usize {
        self.transports.len()
    }
    pub fn transport_ids(&self) -> impl Iterator<Item = TransportId> + '_ {
        self.transports
            .iter()
            .map(|transport| transport.definition.id)
    }
    pub fn total_coded_bits(&self) -> usize {
        self.total_coded
    }
    pub fn definition(&self, id: TransportId) -> Option<&TransportDefinition> {
        self.transport_index(id)
            .map(|index| &self.transports[index].definition)
    }
    pub fn block_plan(&self, id: TransportId, format: usize) -> Option<CodedBlockPlan> {
        self.transports
            .get(self.transport_index(id)?)?
            .blocks
            .get(format)
            .copied()
    }
    pub fn combination(&self, tfci: Tfci) -> Option<&CombinationDefinition> {
        self.combination_index(tfci)
            .map(|index| &self.combinations[index])
    }
    pub fn combinations(&self) -> &[CombinationDefinition] {
        &self.combinations
    }

    fn transport_index(&self, id: TransportId) -> Option<usize> {
        self.transports
            .binary_search_by_key(&id, |transport| transport.definition.id)
            .ok()
    }
    fn combination_index(&self, tfci: Tfci) -> Option<usize> {
        self.combinations
            .binary_search_by_key(&tfci.index(), |combination| combination.tfci.index())
            .ok()
    }
    fn weighted(&self, indexes: &[usize]) -> Result<Vec<u128>, DspError> {
        self.transports
            .iter()
            .zip(indexes)
            .map(|(transport, &index)| {
                (transport.blocks[index].coded_bits() as u128)
                    .checked_mul(FRAME_DENOMINATOR / transport.definition.interval.frames() as u128)
                    .and_then(|bits| {
                        bits.checked_mul(u128::from(transport.definition.attribute.get()))
                    })
                    .ok_or(DspError::Capacity)
            })
            .collect()
    }
    fn used(&self, channel: usize, format: usize) -> bool {
        self.combinations
            .iter()
            .any(|combination| combination.formats[channel] == format)
    }
}

fn coding_key(format: &CodedBlockFormat) -> (u8, usize) {
    use crate::{coding::ChannelCoding, convolutional::CodeRate};
    let coding = match format.coding {
        ChannelCoding::Convolutional(CodeRate::Half) => 0,
        ChannelCoding::Convolutional(CodeRate::Third) => 1,
        ChannelCoding::Turbo => 2,
    };
    (coding, format.crc.width())
}

pub struct UplinkTfcs {
    configured: Arc<ConfiguredTfcs>,
    allocations: Vec<UplinkAllocation>,
    plans: Vec<Vec<Option<Arc<UplinkTransportPlan>>>>,
    decoder: TfciDecoder,
}
impl UplinkTfcs {
    pub fn new(
        configured: Arc<ConfiguredTfcs>,
        allowed: &[UplinkLayout],
        puncturing: PuncturingLimit,
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        if configured.total_coded > limits.maximum_coded_bits.get() {
            return Err(DspError::Capacity);
        }
        if configured
            .transports
            .iter()
            .any(|transport| matches!(transport.definition.interval, Rel99Tti::Ms80))
        {
            return Err(DspError::InvalidCode);
        }
        let mut outputs: Vec<Vec<Vec<usize>>> = configured
            .transports
            .iter()
            .map(|transport| vec![Vec::new(); transport.blocks.len()])
            .collect();
        let mut allocations = Vec::with_capacity(configured.combinations.len());
        for combination in &configured.combinations {
            let inputs: Vec<_> = configured
                .transports
                .iter()
                .zip(&combination.formats)
                .map(|(transport, &index)| UplinkRateInput {
                    id: transport.definition.id,
                    frame_bits: transport.blocks[index]
                        .coded_bits()
                        .div_ceil(transport.definition.interval.frames()),
                    attribute: transport.definition.attribute,
                })
                .collect();
            let allocation = UplinkAllocation::new(&inputs, allowed, puncturing)?;
            for (channel, (&format, matched)) in combination
                .formats
                .iter()
                .zip(allocation.channels())
                .enumerate()
            {
                outputs[channel][format].push(matched.output_bits);
            }
            allocations.push(allocation);
        }
        let mut entries = 0_usize;
        for (transport, outputs) in configured.transports.iter().zip(&mut outputs) {
            let frames = transport.definition.interval.frames();
            for (index, outputs) in outputs.iter_mut().enumerate() {
                outputs.sort_unstable();
                outputs.dedup();
                if outputs.is_empty() {
                    continue;
                }
                let equalized = transport.blocks[index]
                    .coded_bits()
                    .div_ceil(frames)
                    .checked_mul(frames)
                    .ok_or(DspError::Capacity)?;
                entries = outputs
                    .iter()
                    .try_fold(entries, |sum, bits| {
                        bits.checked_mul(frames)
                            .and_then(|bits| sum.checked_add(bits))
                    })
                    .and_then(|sum| sum.checked_add(equalized))
                    .filter(|sum| *sum <= limits.maximum_mapping_entries.get())
                    .ok_or(DspError::Capacity)?;
            }
        }
        let mut plans = Vec::with_capacity(configured.transports.len());
        for (transport, outputs) in configured.transports.iter().zip(outputs) {
            let plans_for_transport = outputs
                .iter()
                .zip(&transport.definition.formats)
                .map(|(outputs, format)| {
                    if outputs.is_empty() {
                        Ok(None)
                    } else {
                        UplinkTransportPlan::new(
                            transport.definition.id,
                            *format,
                            transport.definition.interval,
                            outputs,
                            limits,
                        )
                        .map(Arc::new)
                        .map(Some)
                    }
                })
                .collect::<Result<_, DspError>>()?;
            plans.push(plans_for_transport);
        }
        let allowed: Vec<_> = configured
            .combinations
            .iter()
            .map(|combination| combination.tfci)
            .collect();
        let decoder = TfciDecoder::new(&allowed)?;
        Ok(Self {
            configured,
            allocations,
            plans,
            decoder,
        })
    }
    pub fn configured(&self) -> &Arc<ConfiguredTfcs> {
        &self.configured
    }
    pub fn decoder(&self) -> &TfciDecoder {
        &self.decoder
    }
    pub fn allocation(&self, tfci: Tfci) -> Option<&UplinkAllocation> {
        self.allocations
            .get(self.configured.combination_index(tfci)?)
    }
    pub fn receiver_plan(
        &self,
        id: TransportId,
        format: usize,
    ) -> Option<&Arc<UplinkTransportPlan>> {
        self.plans
            .get(self.configured.transport_index(id)?)?
            .get(format)?
            .as_ref()
    }
}

pub struct FixedDownlinkTfcs {
    configured: Arc<ConfiguredTfcs>,
    physical: DownlinkFormat,
    capacity: usize,
    reserved: Vec<usize>,
    plans: Vec<Vec<Option<Arc<FixedDownlinkPlan>>>>,
}
impl FixedDownlinkTfcs {
    pub fn new(
        configured: Arc<ConfiguredTfcs>,
        physical: DownlinkFormat,
        channels: NonZeroUsize,
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        let capacity = downlink_capacity(&configured, physical, channels, limits)?;
        let maximum: Vec<_> = configured
            .transports
            .iter()
            .map(|transport| {
                transport
                    .blocks
                    .iter()
                    .map(|plan| plan.coded_bits())
                    .max()
                    .expect("nonempty TFS")
            })
            .collect();
        let weighted: Vec<_> = configured
            .transports
            .iter()
            .zip(&maximum)
            .map(|(transport, maximum)| {
                *maximum as u128
                    * (FRAME_DENOMINATOR / transport.definition.interval.frames() as u128)
                    * u128::from(transport.definition.attribute.get())
            })
            .collect();
        let reserved = allocate(&weighted, capacity)?;
        let budgeted = reserved
            .iter()
            .zip(&configured.transports)
            .enumerate()
            .map(|(channel, (reserved, transport))| {
                let total = reserved
                    .checked_mul(transport.definition.interval.frames())
                    .ok_or(DspError::Capacity)?;
                Ok((0..transport.blocks.len())
                    .map(|format| {
                        if configured.used(channel, format) {
                            total
                        } else {
                            0
                        }
                    })
                    .collect())
            })
            .collect::<Result<Vec<Vec<usize>>, DspError>>()?;
        check_downlink_budget(&configured, &budgeted, limits)?;
        let plans = configured
            .transports
            .iter()
            .enumerate()
            .map(|(index, transport)| {
                transport
                    .definition
                    .formats
                    .iter()
                    .enumerate()
                    .map(|(format, blocks)| {
                        if !configured.used(index, format) {
                            return Ok(None);
                        }
                        FixedDownlinkPlan::new(
                            FixedDownlinkSettings {
                                blocks: *blocks,
                                interval: transport.definition.interval,
                                maximum_coded_tti_bits: maximum[index],
                                reserved_frame_bits: reserved[index],
                                physical,
                            },
                            limits,
                        )
                        .map(Arc::new)
                        .map(Some)
                    })
                    .collect::<Result<_, DspError>>()
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            configured,
            physical,
            capacity,
            reserved,
            plans,
        })
    }
    pub fn frame_capacity(&self) -> usize {
        self.capacity
    }
    pub fn reserved_frame_bits(&self, id: TransportId) -> Option<usize> {
        self.reserved
            .get(self.configured.transport_index(id)?)
            .copied()
    }
    pub fn matched_tti_bits(&self, id: TransportId, format: usize) -> Option<usize> {
        self.plans
            .get(self.configured.transport_index(id)?)?
            .get(format)?
            .as_ref()
            .map(|plan| plan.matched_tti_bits())
    }
    pub fn encoder(&self, id: TransportId, format: usize) -> Result<FixedDownlink, DspError> {
        let index = self
            .configured
            .transport_index(id)
            .ok_or(DspError::InvalidCode)?;
        FixedDownlink::from_plan(Arc::clone(
            self.plans[index]
                .get(format)
                .and_then(Option::as_ref)
                .ok_or(DspError::InvalidCode)?,
        ))
    }
    pub fn physical(&self) -> DownlinkFormat {
        self.physical
    }
}

pub struct FlexibleDownlinkTfcs {
    configured: Arc<ConfiguredTfcs>,
    physical: DownlinkFormat,
    capacity: usize,
    matched: Vec<Vec<usize>>,
    plans: Vec<Vec<Option<Arc<FlexibleDownlinkPlan>>>>,
}
impl FlexibleDownlinkTfcs {
    pub fn validate_rate_matching(
        configured: &ConfiguredTfcs,
        physical: DownlinkFormat,
        channels: NonZeroUsize,
        limits: TransportPlanLimits,
    ) -> Result<(), DspError> {
        let (_, matched) = flexible_downlink_allocation(configured, physical, channels, limits)?;
        for (index, transport) in configured.transports.iter().enumerate() {
            for (format, blocks) in transport.blocks.iter().enumerate() {
                if configured.used(index, format) {
                    blocks.matching_flexible(
                        matched[index][format],
                        limits.maximum_mapping_entries,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn new(
        configured: Arc<ConfiguredTfcs>,
        physical: DownlinkFormat,
        channels: NonZeroUsize,
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        let (capacity, matched) =
            flexible_downlink_allocation(&configured, physical, channels, limits)?;
        let plans = configured
            .transports
            .iter()
            .enumerate()
            .map(|(index, transport)| {
                transport
                    .blocks
                    .iter()
                    .enumerate()
                    .map(|(format, &blocks)| {
                        if !configured.used(index, format) {
                            return Ok(None);
                        }
                        FlexibleDownlinkPlan::new(
                            blocks,
                            transport.definition.interval,
                            matched[index][format],
                            physical,
                            limits,
                        )
                        .map(Arc::new)
                        .map(Some)
                    })
                    .collect::<Result<_, DspError>>()
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            configured,
            physical,
            capacity,
            matched,
            plans,
        })
    }
    pub fn frame_capacity(&self) -> usize {
        self.capacity
    }
    pub fn matched_tti_bits(&self, id: TransportId, format: usize) -> Option<usize> {
        self.matched
            .get(self.configured.transport_index(id)?)?
            .get(format)
            .copied()
    }
    pub fn encoder(&self, id: TransportId, format: usize) -> Result<FlexibleDownlink, DspError> {
        let index = self
            .configured
            .transport_index(id)
            .ok_or(DspError::InvalidCode)?;
        FlexibleDownlink::from_plan(Arc::clone(
            self.plans[index]
                .get(format)
                .and_then(Option::as_ref)
                .ok_or(DspError::InvalidCode)?,
        ))
    }
    pub fn physical(&self) -> DownlinkFormat {
        self.physical
    }
}

fn flexible_downlink_allocation(
    configured: &ConfiguredTfcs,
    physical: DownlinkFormat,
    channels: NonZeroUsize,
    limits: TransportPlanLimits,
) -> Result<(usize, Vec<Vec<usize>>), DspError> {
    let capacity = downlink_capacity(configured, physical, channels, limits)?;
    let weights: Vec<_> = configured
        .combinations
        .iter()
        .map(|combination| configured.weighted(&combination.formats))
        .collect::<Result<_, _>>()?;
    let maximum = weights
        .iter()
        .map(|weights| weights.iter().sum::<u128>())
        .max()
        .expect("nonempty TFCS");
    let mut matched = Vec::with_capacity(configured.transports.len());
    for transport in &configured.transports {
        let frames = transport.definition.interval.frames();
        let formats = transport
            .blocks
            .iter()
            .map(|plan| {
                if maximum == 0 {
                    return Ok(0);
                }
                let numerator = (capacity as u128)
                    .checked_mul(u128::from(transport.definition.attribute.get()))
                    .and_then(|value| value.checked_mul(plan.coded_bits() as u128))
                    .and_then(|value| value.checked_mul(FRAME_DENOMINATOR))
                    .ok_or(DspError::Capacity)?;
                let denominator = maximum
                    .checked_mul(frames as u128)
                    .ok_or(DspError::Capacity)?;
                usize::try_from(numerator.div_ceil(denominator))
                    .ok()
                    .and_then(|bits| bits.checked_mul(frames))
                    .ok_or(DspError::Capacity)
            })
            .collect::<Result<Vec<_>, _>>()?;
        matched.push(formats);
    }
    // TS 25.212 4.2.7.2.2.1 corrects shared TF outputs in ascending signaled TFCI order.
    for (combination, weighted) in configured.combinations.iter().zip(weights) {
        let total = combination
            .formats
            .iter()
            .zip(&matched)
            .zip(&configured.transports)
            .try_fold(0_usize, |sum, ((&format, matched), transport)| {
                sum.checked_add(matched[format] / transport.definition.interval.frames())
            })
            .ok_or(DspError::Capacity)?;
        if total > capacity {
            for (index, bits) in allocate(&weighted, capacity)?.into_iter().enumerate() {
                let format = combination.formats[index];
                let correction = bits
                    .checked_mul(configured.transports[index].definition.interval.frames())
                    .ok_or(DspError::Capacity)?;
                matched[index][format] = matched[index][format].min(correction);
            }
        }
    }
    let budgeted: Vec<Vec<usize>> = matched
        .iter()
        .enumerate()
        .map(|(channel, formats)| {
            formats
                .iter()
                .enumerate()
                .map(|(format, bits)| {
                    if configured.used(channel, format) {
                        *bits
                    } else {
                        0
                    }
                })
                .collect()
        })
        .collect();
    check_downlink_budget(configured, &budgeted, limits)?;
    Ok((capacity, matched))
}

fn downlink_capacity(
    configured: &ConfiguredTfcs,
    physical: DownlinkFormat,
    channels: NonZeroUsize,
    limits: TransportPlanLimits,
) -> Result<usize, DspError> {
    if configured.total_coded > limits.maximum_coded_bits.get() {
        return Err(DspError::Capacity);
    }
    if physical.spreading_factor() != 512
        && configured
            .transports
            .iter()
            .any(|transport| matches!(transport.definition.interval, Rel99Tti::Ms80))
    {
        return Err(DspError::InvalidCode);
    }
    physical
        .data_bits()
        .checked_mul(SLOTS_PER_FRAME as usize)
        .and_then(|bits| bits.checked_mul(channels.get()))
        .filter(|bits| *bits <= limits.maximum_mapping_entries.get())
        .ok_or(DspError::Capacity)
}

fn check_downlink_budget(
    configured: &ConfiguredTfcs,
    matched: &[Vec<usize>],
    limits: TransportPlanLimits,
) -> Result<(), DspError> {
    for (channel, (transport, outputs)) in configured.transports.iter().zip(matched).enumerate() {
        for (format, (plan, &output)) in transport.blocks.iter().zip(outputs).enumerate() {
            // Fewer transmitted bits than independent payload bits cannot encode every transport block.
            if configured.used(channel, format) && output < plan.payload_bits() {
                return Err(DspError::InvalidRateMatching);
            }
        }
    }
    let _total = configured
        .transports
        .iter()
        .zip(matched)
        .try_fold(0_usize, |total, (_, formats)| {
            formats.iter().try_fold(total, |sum, bits| {
                bits.checked_mul(2).and_then(|bits| sum.checked_add(bits))
            })
        })
        .filter(|total| *total <= limits.maximum_mapping_entries.get())
        .ok_or(DspError::Capacity)?;
    Ok(())
}

fn allocate(weights: &[u128], capacity: usize) -> Result<Vec<usize>, DspError> {
    let total: u128 = weights.iter().sum();
    let mut prefix = 0_u128;
    let mut previous = 0;
    weights
        .iter()
        .map(|weight| {
            prefix += weight;
            let end = usize::try_from(
                prefix
                    .checked_mul(capacity as u128)
                    .ok_or(DspError::Capacity)?
                    .checked_div(total)
                    .unwrap_or(0),
            )
            .map_err(|_| DspError::Capacity)?;
            let bits = end - previous;
            previous = end;
            Ok(bits)
        })
        .collect()
}
