// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{TransportId, UplinkAllocation, UplinkTfcs, allocation::MAX_TRANSPORTS};
use crate::{
    DspError,
    bits::{Bit, TxBit},
    dedicated::DownlinkFormat,
    interleaving::Interleaver,
};
use std::num::NonZeroUsize;
use umts_types::time::SLOTS_PER_FRAME;

pub struct UplinkComposite {
    allocation: UplinkAllocation,
    second: Option<Interleaver>,
    frame: Vec<f64>,
}
impl UplinkComposite {
    pub fn new(allocation: UplinkAllocation) -> Result<Self, DspError> {
        let bits = allocation.layout().channel(0).map_or(0, |channel| {
            channel.bits_per_slot() * SLOTS_PER_FRAME as usize
        });
        let second = if bits == 0 {
            None
        } else {
            Some(Interleaver::second(bits)?)
        };
        let frame = vec![0.0; allocation.total_bits()];
        Ok(Self {
            allocation,
            second,
            frame,
        })
    }
    pub fn allocation(&self) -> &UplinkAllocation {
        &self.allocation
    }

    // TS 25.212 4.2.10/4.2.11 segment into contiguous PhCH blocks before interleaving each block independently.
    pub fn receive(&mut self, physical: &[&[f64]]) -> Result<UplinkTransportFrame<'_>, DspError> {
        let layout = self.allocation.layout();
        let bits = layout.channel(0).map_or(0, |channel| {
            channel.bits_per_slot() * SLOTS_PER_FRAME as usize
        });
        if physical.len() != layout.len() || physical.iter().any(|input| input.len() != bits) {
            return Err(DspError::InvalidLength);
        }
        if physical
            .iter()
            .any(|input| input.iter().any(|value| !value.is_finite()))
        {
            return Err(DspError::NonFiniteEvidence);
        }
        if let Some(second) = &self.second {
            for (input, output) in physical.iter().zip(self.frame.chunks_exact_mut(bits)) {
                second.deinterleave(input, output)?;
            }
        }
        Ok(UplinkTransportFrame {
            allocation: &self.allocation,
            frame: &self.frame,
        })
    }
}

pub struct UplinkTransportFrame<'a> {
    allocation: &'a UplinkAllocation,
    frame: &'a [f64],
}

pub struct UplinkDemultiplexer {
    second: Vec<(usize, Interleaver)>,
    frame: Vec<f64>,
}
impl UplinkDemultiplexer {
    pub fn new(tfcs: &UplinkTfcs, maximum_frame_bits: NonZeroUsize) -> Result<Self, DspError> {
        let mut second = Vec::new();
        let mut maximum = 0;
        for combination in tfcs.configured().combinations() {
            let allocation = tfcs
                .allocation(combination.tfci)
                .expect("configured combination");
            maximum = maximum.max(allocation.total_bits());
            if maximum > maximum_frame_bits.get() {
                return Err(DspError::Capacity);
            }
            if let Some(channel) = allocation.layout().channel(0) {
                let bits = channel.bits_per_slot() * SLOTS_PER_FRAME as usize;
                if !second.iter().any(|(length, _)| *length == bits) {
                    second.push((bits, Interleaver::second(bits)?));
                }
            }
        }
        Ok(Self {
            second,
            frame: vec![0.0; maximum],
        })
    }
    pub fn maximum_frame_bits(&self) -> usize {
        self.frame.len()
    }
    pub fn receive<'a>(
        &'a mut self,
        allocation: &'a UplinkAllocation,
        physical: &[&[f64]],
    ) -> Result<UplinkTransportFrame<'a>, DspError> {
        let layout = allocation.layout();
        let bits = layout.channel(0).map_or(0, |channel| {
            channel.bits_per_slot() * SLOTS_PER_FRAME as usize
        });
        if allocation.total_bits() > self.frame.len()
            || physical.len() != layout.len()
            || physical.iter().any(|input| input.len() != bits)
        {
            return Err(DspError::InvalidLength);
        }
        if physical
            .iter()
            .any(|input| input.iter().any(|value| !value.is_finite()))
        {
            return Err(DspError::NonFiniteEvidence);
        }
        if bits != 0 {
            let second = &self
                .second
                .iter()
                .find(|(length, _)| *length == bits)
                .ok_or(DspError::InvalidCode)?
                .1;
            for (input, output) in physical
                .iter()
                .zip(self.frame[..allocation.total_bits()].chunks_exact_mut(bits))
            {
                second.deinterleave(input, output)?;
            }
        }
        Ok(UplinkTransportFrame {
            allocation,
            frame: &self.frame[..allocation.total_bits()],
        })
    }
}
impl UplinkTransportFrame<'_> {
    pub fn get(&self, id: TransportId) -> Option<&[f64]> {
        let mut offset = 0;
        for channel in self.allocation.channels() {
            let end = offset + channel.output_bits;
            if channel.id == id {
                return Some(&self.frame[offset..end]);
            }
            offset = end;
        }
        None
    }
}

pub struct DownlinkTransportFrame<'a, T> {
    pub id: TransportId,
    pub bits: &'a [T],
}

pub struct DownlinkComposite {
    format: DownlinkFormat,
    channels: NonZeroUsize,
    ids: Vec<TransportId>,
    second: Interleaver,
    scratch: Vec<TxBit>,
}
impl DownlinkComposite {
    pub fn new(
        format: DownlinkFormat,
        channels: NonZeroUsize,
        ids: &[TransportId],
        maximum_frame_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if ids.is_empty()
            || ids.len() > MAX_TRANSPORTS
            || ids.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(DspError::InvalidCode);
        }
        let bits = format.data_bits() * SLOTS_PER_FRAME as usize;
        let total = channels
            .get()
            .checked_mul(bits)
            .filter(|total| *total <= maximum_frame_bits.get())
            .ok_or(DspError::Capacity)?;
        Ok(Self {
            format,
            channels,
            ids: ids.to_vec(),
            second: Interleaver::second(bits)?,
            scratch: vec![TxBit::Dtx; total],
        })
    }
    pub fn format(&self) -> DownlinkFormat {
        self.format
    }
    pub fn channels(&self) -> usize {
        self.channels.get()
    }
    pub fn frame_bits(&self) -> usize {
        self.scratch.len()
    }

    // Inputs are ordered TrCH frames after first interleaving. Fixed-position frames already contain their first DTX insertion.
    pub fn compose(
        &mut self,
        transports: &[DownlinkTransportFrame<'_, TxBit>],
        output: &mut [TxBit],
    ) -> Result<(), DspError> {
        if transports.len() != self.ids.len()
            || output.len() != self.scratch.len()
            || transports
                .iter()
                .zip(&self.ids)
                .any(|(transport, id)| transport.id != *id)
        {
            return Err(DspError::InvalidLength);
        }
        let used = transports
            .iter()
            .try_fold(0_usize, |total, transport| {
                total.checked_add(transport.bits.len())
            })
            .filter(|total| *total <= self.scratch.len())
            .ok_or(DspError::Capacity)?;
        let mut offset = 0;
        for transport in transports {
            self.scratch[offset..offset + transport.bits.len()].copy_from_slice(transport.bits);
            offset += transport.bits.len();
        }
        self.scratch[used..].fill(TxBit::Dtx);
        let width = self.format.data_bits() * SLOTS_PER_FRAME as usize;
        for (input, output) in self
            .scratch
            .chunks_exact(width)
            .zip(output.chunks_exact_mut(width))
        {
            self.second.interleave(input, output)?;
        }
        Ok(())
    }

    pub fn compose_flexible(
        &mut self,
        transports: &[DownlinkTransportFrame<'_, Bit>],
        output: &mut [TxBit],
    ) -> Result<(), DspError> {
        if transports.len() != self.ids.len()
            || output.len() != self.scratch.len()
            || transports
                .iter()
                .zip(&self.ids)
                .any(|(transport, id)| transport.id != *id)
        {
            return Err(DspError::InvalidLength);
        }
        let used = transports
            .iter()
            .try_fold(0_usize, |total, transport| {
                total.checked_add(transport.bits.len())
            })
            .filter(|total| *total <= self.scratch.len())
            .ok_or(DspError::Capacity)?;
        let mut offset = 0;
        for transport in transports {
            for (bit, output) in transport
                .bits
                .iter()
                .zip(&mut self.scratch[offset..offset + transport.bits.len()])
            {
                *output = TxBit::Data(*bit);
            }
            offset += transport.bits.len();
        }
        self.scratch[used..].fill(TxBit::Dtx);
        let width = self.format.data_bits() * SLOTS_PER_FRAME as usize;
        for (input, output) in self
            .scratch
            .chunks_exact(width)
            .zip(output.chunks_exact_mut(width))
        {
            self.second.interleave(input, output)?;
        }
        Ok(())
    }
}
