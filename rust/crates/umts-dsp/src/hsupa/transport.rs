// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    num::{NonZeroU8, NonZeroUsize},
    sync::Arc,
};

use umts_types::hsupa::{CodeLayout, Etfci, LayoutSet, Rel6Category, TransportBlockTable, Tti};

use super::HarqRateMatching;
use crate::{
    DspError,
    bits::Bit,
    crc::TransportCrc,
    interleaving::Interleaver,
    transport_blocks::BlockOutcome,
    turbo::{
        Crc24TurboDecoder, TurboBlockEncoder, TurboBlockFormat, TurboBlockPlan, TurboInterleaver,
    },
};

const REDUNDANCY_VERSIONS: usize = 4;
const MAX_CHANNELS: usize = 4;

pub struct TransportPlan {
    interval: Tti,
    turbo: TurboBlockPlan,
    channels: Vec<Channel>,
    physical_bits: usize,
    matching: [HarqRateMatching; REDUNDANCY_VERSIONS],
}

struct Channel {
    offset: usize,
    bits: usize,
    interleaver: Interleaver,
}

impl TransportPlan {
    pub fn decode_state_updates(&self, iterations: NonZeroU8) -> Result<usize, DspError> {
        self.turbo.crc_round_state_updates(iterations)
    }

    pub fn retained_bytes(&self) -> Option<usize> {
        let base = std::mem::size_of::<Self>().checked_add(
            self.channels
                .capacity()
                .checked_mul(std::mem::size_of::<Channel>())?,
        )?;
        self.channels
            .iter()
            .map(|channel| channel.interleaver.heap_bytes())
            .chain(self.matching.iter().map(HarqRateMatching::heap_bytes))
            .try_fold(base, usize::checked_add)
    }
    pub fn from_table(
        table: TransportBlockTable,
        format: Etfci,
        category: Rel6Category,
        layouts: &LayoutSet,
        maximum_bits: NonZeroUsize,
    ) -> Result<(Self, CodeLayout), DspError> {
        let payload_bits = table
            .bits(layouts.interval(), format)
            .ok_or(DspError::InvalidLength)?;
        let turbo = TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: payload_bits.get(),
                blocks: 1,
                crc: TransportCrc::Crc24,
            },
            maximum_bits,
        )?;
        let coded = NonZeroUsize::new(turbo.coded_bits()).ok_or(DspError::InvalidLength)?;
        let layout = layouts
            .select(coded)
            .map_err(|_| DspError::InvalidRateMatching)?;
        let plan = Self::new(
            payload_bits,
            layouts.interval(),
            category,
            layout.spreading_factors(),
            maximum_bits,
        )?;
        Ok((plan, layout))
    }

    pub fn new(
        payload_bits: NonZeroUsize,
        tti: Tti,
        category: Rel6Category,
        spreading_factors: &[u16],
        maximum_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        let limits = category.limits();
        if category
            .maximum_transport_bits(tti)
            .is_none_or(|max| payload_bits.get() > max)
            || spreading_factors.len() > usize::from(limits.maximum_codes)
        {
            return Err(DspError::InvalidLength);
        }
        // TS 25.212 4.8.4.1 lists these BPSK E-DPDCH sets, not arbitrary SF combinations.
        let legal = match spreading_factors {
            [sf] => (4..=256).contains(sf) && sf.is_power_of_two(),
            [4, 4] | [2, 2] | [2, 2, 4, 4] => true,
            _ => false,
        };
        if !legal
            || spreading_factors
                .iter()
                .any(|sf| *sf < limits.minimum_spreading_factor)
        {
            return Err(DspError::InvalidCode);
        }
        let turbo = TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: payload_bits.get(),
                blocks: 1,
                crc: TransportCrc::Crc24,
            },
            maximum_bits,
        )?;
        let mut channels = Vec::with_capacity(MAX_CHANNELS);
        let mut physical_bits = 0_usize;
        for sf in spreading_factors {
            let bits =
                usize::try_from(tti.chips() / u64::from(*sf)).map_err(|_| DspError::Capacity)?;
            let offset = physical_bits;
            physical_bits = physical_bits
                .checked_add(bits)
                .filter(|n| *n <= maximum_bits.get())
                .ok_or(DspError::Capacity)?;
            channels.push(Channel {
                offset,
                bits,
                interleaver: Interleaver::second(bits)?,
            });
        }
        let coded = NonZeroUsize::new(turbo.coded_bits()).ok_or(DspError::InvalidLength)?;
        let physical = NonZeroUsize::new(physical_bits).ok_or(DspError::InvalidLength)?;
        let make = |rv| HarqRateMatching::new(coded, physical, rv, maximum_bits);
        let matching = [make(0)?, make(1)?, make(2)?, make(3)?];
        Ok(Self {
            interval: tti,
            turbo,
            channels,
            physical_bits,
            matching,
        })
    }

    pub fn payload_bits(&self) -> usize {
        self.turbo.payload_bits()
    }
    pub fn interval(&self) -> Tti {
        self.interval
    }
    pub fn coded_bits(&self) -> usize {
        self.turbo.coded_bits()
    }
    pub fn physical_bits(&self) -> usize {
        self.physical_bits
    }
    pub fn channel_bits(&self) -> impl ExactSizeIterator<Item = usize> + '_ {
        self.channels.iter().map(|channel| channel.bits)
    }
    fn matching(&self, rv: u8) -> Result<&HarqRateMatching, DspError> {
        self.matching
            .get(usize::from(rv))
            .ok_or(DspError::InvalidRateMatching)
    }
}

pub struct TransportEncoder {
    plan: Arc<TransportPlan>,
    encoder: TurboBlockEncoder,
    coded: Vec<Bit>,
    matched: Vec<Bit>,
}

impl TransportEncoder {
    pub fn new(plan: Arc<TransportPlan>) -> Result<Self, DspError> {
        Ok(Self {
            encoder: TurboBlockEncoder::new(plan.turbo)?,
            coded: vec![Bit::Zero; plan.coded_bits()],
            matched: vec![Bit::Zero; plan.physical_bits()],
            plan,
        })
    }
    // Output concatenates each E-DPDCH's interleaved bits in channel-number order.
    pub fn encode(&mut self, payload: &[Bit], rv: u8, output: &mut [Bit]) -> Result<(), DspError> {
        if output.len() != self.plan.physical_bits() {
            return Err(DspError::InvalidLength);
        }
        let matching = self.plan.matching(rv)?;
        self.encoder.encode(payload, &mut self.coded)?;
        matching.apply(&self.coded, &mut self.matched)?;
        for channel in &self.plan.channels {
            let range = channel.offset..channel.offset + channel.bits;
            channel
                .interleaver
                .interleave(&self.matched[range.clone()], &mut output[range])?;
        }
        Ok(())
    }
}

pub struct TransportDecoder {
    plans: Vec<DecoderPlan>,
    selected: usize,
    decoder: Crc24TurboDecoder,
    deinterleaved: Vec<f32>,
}

struct DecoderPlan {
    transport: Arc<TransportPlan>,
    interleaver: Option<Arc<TurboInterleaver>>,
}

impl TransportDecoder {
    // Charge each admitted plan once even if another owner shares its Arc.
    pub fn retained_bytes(&self) -> Option<usize> {
        let base = std::mem::size_of::<Self>()
            .checked_add(
                self.plans
                    .capacity()
                    .checked_mul(std::mem::size_of::<DecoderPlan>())?,
            )?
            .checked_add(
                self.deinterleaved
                    .capacity()
                    .checked_mul(std::mem::size_of::<f32>())?,
            )?
            .checked_add(self.decoder.heap_bytes())?;
        self.plans.iter().try_fold(base, |bytes, plan| {
            let interleaver = plan.interleaver.as_ref().map_or(Some(0), |value| {
                value
                    .len()
                    .checked_mul(std::mem::size_of::<usize>())?
                    .checked_add(
                        std::mem::size_of::<TurboInterleaver>() + 2 * std::mem::size_of::<usize>(),
                    )
            })?;
            bytes
                .checked_add(interleaver)?
                .checked_add(plan.transport.retained_bytes()?)?
                .checked_add(2 * std::mem::size_of::<usize>())
        })
    }
    pub fn new(plan: Arc<TransportPlan>, iterations: NonZeroU8) -> Result<Self, DspError> {
        Self::from_plans(vec![plan], iterations)
    }

    pub fn from_plans(
        plans: Vec<Arc<TransportPlan>>,
        iterations: NonZeroU8,
    ) -> Result<Self, DspError> {
        if plans.is_empty() || plans.len() > super::MAX_FORMATS {
            return Err(DspError::Capacity);
        }
        let maximum_physical = plans
            .iter()
            .map(|plan| plan.physical_bits())
            .max()
            .ok_or(DspError::Capacity)?;
        let turbo: Vec<_> = plans.iter().map(|plan| plan.turbo).collect();
        let mut decoder = Crc24TurboDecoder::new(&turbo, iterations)?;
        let plans = plans
            .into_iter()
            .map(|transport| {
                Ok(DecoderPlan {
                    interleaver: transport.turbo.interleaver()?,
                    transport,
                })
            })
            .collect::<Result<Vec<_>, DspError>>()?;
        decoder.configure(plans[0].transport.turbo, plans[0].interleaver.as_ref())?;
        Ok(Self {
            decoder,
            deinterleaved: vec![0.0; maximum_physical],
            plans,
            selected: 0,
        })
    }

    pub fn select(&mut self, index: usize) -> Result<(), DspError> {
        let plan = self.plans.get(index).ok_or(DspError::InvalidCode)?;
        self.decoder
            .configure(plan.transport.turbo, plan.interleaver.as_ref())?;
        self.selected = index;
        Ok(())
    }

    // HARQ owns accumulation and retransmission identity. This emits only this TTI's evidence.
    pub fn unmatch(&mut self, physical: &[f32], rv: u8, coded: &mut [f32]) -> Result<(), DspError> {
        let plan = &self.plans[self.selected].transport;
        if physical.len() != plan.physical_bits() || coded.len() != plan.coded_bits() {
            return Err(DspError::InvalidLength);
        }
        let matching = plan.matching(rv)?;
        if physical.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        for channel in &plan.channels {
            let range = channel.offset..channel.offset + channel.bits;
            channel
                .interleaver
                .deinterleave(&physical[range.clone()], &mut self.deinterleaved[range])?;
        }
        matching.unmatch(&self.deinterleaved[..plan.physical_bits()], coded)
    }

    pub fn decode_combined(
        &mut self,
        coded: &[f32],
        payload: &mut [Bit],
    ) -> Result<BlockOutcome, DspError> {
        self.decoder.decode(coded, payload)
    }

    pub fn begin_combined(
        &mut self,
        coded: &[f32],
        payload: &mut [Bit],
    ) -> Result<crate::coding::DecodeProgress, DspError> {
        self.decoder.begin(coded, payload)
    }

    pub fn step_combined(
        &mut self,
        coded: &[f32],
        payload: &mut [Bit],
        work: &mut crate::coding::DecodeWork,
    ) -> Result<crate::coding::DecodeProgress, DspError> {
        self.decoder.step(coded, payload, work)
    }

    pub fn cancel(&mut self) {
        self.decoder.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_decoder_reuses_reserved_storage_across_transport_sizes() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let plans: Vec<_> = [(120, &[16][..]), (11484, &[2, 2, 4, 4][..])]
            .into_iter()
            .map(|(bits, sfs)| {
                Arc::new(
                    TransportPlan::new(
                        nz(bits),
                        Tti::TwoMilliseconds,
                        Rel6Category::new(6).unwrap(),
                        sfs,
                        nz(100_000),
                    )
                    .unwrap(),
                )
            })
            .collect();
        let mut decoder =
            TransportDecoder::from_plans(plans.clone(), NonZeroU8::new(6).unwrap()).unwrap();
        let scratch = decoder.deinterleaved.as_ptr();
        let capacity = decoder.deinterleaved.capacity();
        for index in [0, 1, 0, 1] {
            let plan = &plans[index];
            let payload: Vec<_> = (0..plan.payload_bits())
                .map(|bit| Bit::from(bit % 3 == 1))
                .collect();
            let mut physical = vec![Bit::Zero; plan.physical_bits()];
            TransportEncoder::new(Arc::clone(plan))
                .unwrap()
                .encode(&payload, 0, &mut physical)
                .unwrap();
            let soft: Vec<_> = physical
                .iter()
                .map(|bit| if *bit == Bit::Zero { 4.0 } else { -4.0 })
                .collect();
            let mut evidence = vec![0.0; plan.coded_bits()];
            let mut result = vec![Bit::Zero; plan.payload_bits()];
            decoder.select(index).unwrap();
            decoder.unmatch(&soft, 0, &mut evidence).unwrap();
            assert_eq!(
                decoder.decode_combined(&evidence, &mut result).unwrap(),
                BlockOutcome::Decoded
            );
            assert_eq!(result, payload);
            assert_eq!(decoder.deinterleaved.as_ptr(), scratch);
            assert_eq!(decoder.deinterleaved.capacity(), capacity);
        }
        assert!(decoder.select(plans.len()).is_err());
        assert_eq!(decoder.selected, 1);
    }

    #[test]
    fn signalled_table_and_puncturing_select_the_maximum_category_six_layout() {
        use umts_types::hsupa::PuncturingLimit;
        let category = Rel6Category::new(6).unwrap();
        let allowed = [
            CodeLayout::new(&[4]).unwrap(),
            CodeLayout::new(&[4, 4]).unwrap(),
            CodeLayout::new(&[2, 2]).unwrap(),
            CodeLayout::new(&[2, 2, 4, 4]).unwrap(),
        ];
        let layouts = LayoutSet::new(
            Tti::TwoMilliseconds,
            category,
            &allowed,
            PuncturingLimit::new(44).unwrap(),
        )
        .unwrap();
        let (plan, layout) = TransportPlan::from_table(
            TransportBlockTable::Zero,
            Etfci::new(127).unwrap(),
            category,
            &layouts,
            NonZeroUsize::new(100_000).unwrap(),
        )
        .unwrap();
        assert_eq!(plan.payload_bits(), 11484);
        assert_eq!(layout.spreading_factors(), &[2, 2, 4, 4]);
        assert_eq!(plan.physical_bits(), 11520);
        assert!(
            TransportPlan::from_table(
                TransportBlockTable::Zero,
                Etfci::new(120).unwrap(),
                category,
                &layouts,
                NonZeroUsize::new(100_000).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn transport_preserves_payload_crc_through_each_rv_and_code_segmentation() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        for (bits, sf) in [
            (120, &[16][..]),
            (4000, &[2, 2][..]),
            (11484, &[2, 2, 4, 4][..]),
        ] {
            let plan = Arc::new(
                TransportPlan::new(
                    nz(bits),
                    Tti::TwoMilliseconds,
                    Rel6Category::new(6).unwrap(),
                    sf,
                    nz(100_000),
                )
                .unwrap(),
            );
            let mut encoder = TransportEncoder::new(Arc::clone(&plan)).unwrap();
            let mut decoder =
                TransportDecoder::new(Arc::clone(&plan), NonZeroU8::new(6).unwrap()).unwrap();
            let payload: Vec<_> = (0..bits)
                .map(|index| Bit::from((index * 17 + index / 11) % 7 < 3))
                .collect();
            let mut physical = vec![Bit::Zero; plan.physical_bits()];
            let mut combined = vec![0.0; plan.coded_bits()];
            let mut evidence = vec![0.0; plan.coded_bits()];
            let mut result = vec![Bit::Zero; bits];
            for rv in [0, 2, 1, 3] {
                encoder.encode(&payload, rv, &mut physical).unwrap();
                let soft: Vec<_> = physical
                    .iter()
                    .map(|bit| if *bit == Bit::Zero { 4.0 } else { -4.0 })
                    .collect();
                decoder.unmatch(&soft, rv, &mut evidence).unwrap();
                for (sum, value) in combined.iter_mut().zip(&evidence) {
                    *sum += value;
                }
            }
            assert_eq!(
                decoder.decode_combined(&combined, &mut result).unwrap(),
                BlockOutcome::Decoded
            );
            assert_eq!(result, payload);
        }
    }
}
