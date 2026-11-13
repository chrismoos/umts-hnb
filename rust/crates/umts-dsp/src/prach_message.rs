// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::pilots::UplinkPilots;
use crate::{
    DspError,
    bits::Bit,
    interleaving::Rel99Tti,
    prach::{PrachCode, PreambleSignature},
    prach_transport::{PrachTransportDecoder, PrachTransportPlan},
    tfci::Tfci,
    tfci::{TfciDecision, TfciDecoder, UPLINK_CODE_BITS},
};
pub use crate::{pilots::PilotEstimate, soft_bits::normalize_soft_bits};
use num_complex::{Complex32, Complex64};
use std::sync::Arc;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant, SLOTS_PER_FRAME};

const CONTROL_SF: usize = 256;
const PILOTS_PER_SLOT: usize = 8;
const CONTROL_BITS_PER_SLOT: usize = 10;
const MAX_MESSAGE_FRAMES: usize = 2;
const MAX_MESSAGE_SLOTS: usize = MAX_MESSAGE_FRAMES * SLOTS_PER_FRAME as usize;
const DATA_SFS: [u16; 4] = [256, 128, 64, 32];

pub struct PrachMessageCode {
    code: PrachCode,
    scrambling: Vec<Complex32>,
}

impl PrachMessageCode {
    pub fn new(code: PrachCode) -> Result<Self, DspError> {
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        code.write_message_scrambling(&mut scrambling)?;
        Ok(Self { code, scrambling })
    }
    pub fn code(&self) -> PrachCode {
        self.code
    }
}

pub struct PrachMessageControl {
    pilots: [PilotEstimate; MAX_MESSAGE_SLOTS],
    slots: usize,
    tfci: [f64; UPLINK_CODE_BITS],
}

impl PrachMessageControl {
    pub fn pilots(&self) -> &[PilotEstimate] {
        &self.pilots[..self.slots]
    }
    pub fn tfci(&self) -> &[f64; UPLINK_CODE_BITS] {
        &self.tfci
    }
    pub fn decode_tfci(&self, decoder: &TfciDecoder) -> Result<Option<TfciDecision>, DspError> {
        let mut normalized = [0.0; UPLINK_CODE_BITS];
        normalize_soft_bits(&self.tfci, &mut normalized)?;
        decoder.decode(&normalized)
    }
}

pub struct PrachMessageReceiver {
    code: Arc<PrachMessageCode>,
    signature: PreambleSignature,
    frames: usize,
    control_code: [f32; CONTROL_SF],
    data_codes: [Vec<f32>; DATA_SFS.len()],
    samples: Vec<Complex64>,
}

impl PrachMessageReceiver {
    pub fn new(
        code: Arc<PrachMessageCode>,
        signature: PreambleSignature,
        interval: Rel99Tti,
    ) -> Result<Self, DspError> {
        if !matches!(interval, Rel99Tti::Ms10 | Rel99Tti::Ms20) {
            return Err(DspError::InvalidLength);
        }
        let (_, control) = signature.message_codes(CONTROL_SF as u16)?;
        let mut control_code = [0.0; CONTROL_SF];
        control.write(&mut control_code)?;
        let mut data_codes: [Vec<f32>; DATA_SFS.len()] = std::array::from_fn(|_| Vec::new());
        for (sf, output) in DATA_SFS.into_iter().zip(&mut data_codes) {
            output.resize(usize::from(sf), 0.0);
            signature.message_codes(sf)?.0.write(output)?;
        }
        Ok(Self {
            code,
            signature,
            frames: interval.frames(),
            control_code,
            data_codes,
            samples: vec![Complex64::default(); interval.frames() * CHIPS_PER_FRAME as usize],
        })
    }

    // Input starts at the acquired message origin after receive filtering and CFO correction.
    pub fn receive(
        &mut self,
        start: ChipInstant,
        input: &[Complex32],
    ) -> Result<PrachMessage<'_>, DspError> {
        if input.len() != self.samples.len() {
            return Err(DspError::InvalidLength);
        }
        start
            .index
            .checked_add(input.len() as u64)
            .ok_or(DspError::InvalidLength)?;
        if input
            .iter()
            .any(|value| !value.re.is_finite() || !value.im.is_finite())
        {
            return Err(DspError::NonFiniteEvidence);
        }
        for (index, (sample, value)) in self.samples.iter_mut().zip(input).enumerate() {
            let scrambling = self.code.scrambling[index % CHIPS_PER_FRAME as usize];
            *sample = Complex64::new(f64::from(value.re), f64::from(value.im))
                * Complex64::new(f64::from(scrambling.re), -f64::from(scrambling.im))
                * 0.5;
        }
        let slots = self.frames * SLOTS_PER_FRAME as usize;
        let mut control = PrachMessageControl {
            pilots: [PilotEstimate::default(); MAX_MESSAGE_SLOTS],
            slots,
            tfci: [0.0; UPLINK_CODE_BITS],
        };
        for (slot, samples) in self
            .samples
            .chunks_exact(CHIPS_PER_SLOT as usize)
            .enumerate()
        {
            let symbols: [Complex64; CONTROL_BITS_PER_SLOT] = std::array::from_fn(|symbol| {
                let value = samples[symbol * CONTROL_SF..(symbol + 1) * CONTROL_SF]
                    .iter()
                    .zip(&self.control_code)
                    .map(|(value, code)| value * f64::from(*code))
                    .sum::<Complex64>()
                    / CONTROL_SF as f64;
                // TS 25.213 Figure 2 places PRACH control on Q, not I.
                Complex64::new(value.im, -value.re)
            });
            let pilot_word = UplinkPilots::Eight
                .word((slot % SLOTS_PER_FRAME as usize) as u8)
                .expect("frame slot");
            let pilots: [Complex64; PILOTS_PER_SLOT] = std::array::from_fn(|bit| {
                symbols[bit]
                    * if pilot_word & (1 << (PILOTS_PER_SLOT - 1 - bit)) == 0 {
                        1.0
                    } else {
                        -1.0
                    }
            });
            let channel = pilots.iter().sum::<Complex64>() / PILOTS_PER_SLOT as f64;
            control.pilots[slot] = PilotEstimate {
                channel,
                observed_power: pilots.iter().map(|value| value.norm_sqr()).sum::<f64>()
                    / PILOTS_PER_SLOT as f64,
                residual_power: pilots
                    .iter()
                    .map(|value| (*value - channel).norm_sqr())
                    .sum::<f64>()
                    / PILOTS_PER_SLOT as f64,
            };
            for (bit, value) in symbols[PILOTS_PER_SLOT..].iter().enumerate() {
                control.tfci[(slot % SLOTS_PER_FRAME as usize) * 2 + bit] +=
                    (value * channel.conj()).re;
            }
        }
        Ok(PrachMessage {
            receiver: self,
            start,
            control,
        })
    }
}

pub struct PrachMessage<'a> {
    receiver: &'a PrachMessageReceiver,
    start: ChipInstant,
    control: PrachMessageControl,
}

impl PrachMessage<'_> {
    pub fn start(&self) -> ChipInstant {
        self.start
    }
    pub fn code(&self) -> PrachCode {
        self.receiver.code.code
    }
    pub fn signature(&self) -> PreambleSignature {
        self.receiver.signature
    }
    pub fn control(&self) -> &PrachMessageControl {
        &self.control
    }

    // Projections retain one common scale across the complete TTI, including both frames of a 20 ms message.
    pub fn despread_data(&self, spreading_factor: u16, output: &mut [f64]) -> Result<(), DspError> {
        let index = DATA_SFS
            .iter()
            .position(|sf| *sf == spreading_factor)
            .ok_or(DspError::InvalidCode)?;
        let sf = usize::from(spreading_factor);
        if output.len() != self.receiver.samples.len() / sf {
            return Err(DspError::InvalidLength);
        }
        let code = &self.receiver.data_codes[index];
        for (symbol, (samples, out)) in self
            .receiver
            .samples
            .chunks_exact(sf)
            .zip(output)
            .enumerate()
        {
            let value = samples
                .iter()
                .zip(code)
                .map(|(value, code)| value * f64::from(*code))
                .sum::<Complex64>()
                / sf as f64;
            *out = (value
                * self.control.pilots[symbol * sf / CHIPS_PER_SLOT as usize]
                    .channel
                    .conj())
            .re;
        }
        Ok(())
    }
}

const MAX_TFCI: usize = 1024;

struct PreparedFormat {
    sf: u16,
    bits: usize,
    decoder: PrachTransportDecoder,
    soft: Vec<f64>,
    normalized: Vec<f32>,
}

pub struct PrachMessageDecoder {
    code: PrachCode,
    available: u16,
    frames: usize,
    control: TfciDecoder,
    indices: [Option<usize>; MAX_TFCI],
    formats: Vec<PreparedFormat>,
}

#[derive(Clone, Copy, Debug)]
pub struct DecodedPrachMessage {
    pub start: ChipInstant,
    pub code: PrachCode,
    pub signature: PreambleSignature,
    pub tfci: TfciDecision,
    pub payload_bits: usize,
}

impl PrachMessageDecoder {
    pub fn new(
        code: PrachCode,
        available: u16,
        plans: Vec<(Tfci, PrachTransportPlan)>,
    ) -> Result<Self, DspError> {
        if available == 0 || plans.is_empty() || plans.len() > MAX_TFCI {
            return Err(DspError::InvalidCode);
        }
        let frames = plans[0].1.frames();
        if plans.iter().any(|(_, plan)| plan.frames() != frames) {
            return Err(DspError::InvalidLength);
        }
        let allowed: Vec<_> = plans.iter().map(|(tfci, _)| *tfci).collect();
        let control = TfciDecoder::new(&allowed)?;
        let mut indices = [None; MAX_TFCI];
        let mut formats = Vec::with_capacity(plans.len());
        for (tfci, plan) in plans {
            indices[usize::from(tfci.index())] = Some(formats.len());
            let count = plan.frame_bits() * frames;
            formats.push(PreparedFormat {
                sf: plan.spreading_factor(),
                bits: plan.payload_bits(),
                soft: vec![0.0; count],
                normalized: vec![0.0; count],
                decoder: PrachTransportDecoder::new(plan),
            });
        }
        Ok(Self {
            code,
            available,
            frames,
            control,
            indices,
            formats,
        })
    }

    // The selected TFCI is decoded once. CRC failure never triggers a hunt through other formats.
    pub fn decode(
        &mut self,
        message: &PrachMessage<'_>,
        output: &mut [Bit],
    ) -> Result<Option<DecodedPrachMessage>, DspError> {
        if message.code() != self.code || self.available & (1 << message.signature().number()) == 0
        {
            return Err(DspError::InvalidCode);
        }
        if message.receiver.frames != self.frames {
            return Err(DspError::InvalidLength);
        }
        let Some(decision) = message.control.decode_tfci(&self.control)? else {
            return Ok(None);
        };
        let index =
            self.indices[usize::from(decision.tfci.index())].ok_or(DspError::InvalidCode)?;
        let format = &mut self.formats[index];
        if output.len() < format.bits {
            return Err(DspError::InvalidLength);
        }
        message.despread_data(format.sf, &mut format.soft)?;
        normalize_soft_bits(&format.soft, &mut format.normalized)?;
        if !format
            .decoder
            .decode(&format.normalized, &mut output[..format.bits])?
        {
            return Ok(None);
        }
        Ok(Some(DecodedPrachMessage {
            start: message.start(),
            code: self.code,
            signature: message.signature(),
            tfci: decision,
            payload_bits: format.bits,
        }))
    }
}

#[cfg(test)]
mod tests;
