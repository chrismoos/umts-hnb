// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use num_complex::Complex32;
use umts_dsp::{
    DspError,
    bits::Bit,
    convolutional::CodeRate,
    crc::TransportCrc,
    indicators::Acquisition,
    interleaving::Rel99Tti,
    prach::{PrachCode, PreambleSignature},
    prach_message::{
        DecodedPrachMessage, PrachMessageCode, PrachMessageDecoder, PrachMessageReceiver,
    },
    prach_transport::{PrachFormat, PrachTransportPlan},
    pulse_shaping::SamplesPerChip,
    tfci::Tfci,
};
use umts_rrc::transport::{ChannelCoding, ConvolutionalRate, CrcSize, TransportInterval};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

use crate::{
    access::{
        AccessObservation, AccessTxTarget, AccessWindow, AcquisitionPublication,
        MAX_ACCESS_RESPONSES,
    },
    common_tx::CommonTxError,
    radio_io::FilteredRx,
};

const SIGNATURES: usize = 16;
const MAX_INPUT_CHIPS: usize = CHIPS_PER_FRAME as usize;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageTicket {
    epoch: RadioEpoch,
    owner: u64,
    serial: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageError {
    Capacity,
    Profile,
    Epoch,
    Time,
    Phase,
    Stale,
    Gap,
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    UnknownTicket,
    Dsp(DspError),
    Transmit(CommonTxError),
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct MessageAdmission {
    pub ticket: MessageTicket,
    pub window: AccessWindow,
}

#[derive(Clone, Copy, Debug)]
pub struct MessageLimits {
    pub capacity: NonZeroUsize,
    pub maximum_payload_bits: NonZeroUsize,
    pub maximum_formats: NonZeroUsize,
    // Retained sample/payload storage only. Prepared DSP state is bounded separately by signature, format and payload limits.
    pub maximum_buffer_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageCounters {
    pub admitted: u64,
    pub duplicates: u64,
    pub rejected: u64,
    pub transmit_rejected: u64,
    pub discontinuities: u64,
    pub discarded: u64,
    pub decoded: u64,
    pub failed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Vacant,
    Collecting,
    Ready,
}

struct Reservation {
    state: State,
    serial: u64,
    observation: AccessObservation,
    window: AccessWindow,
    start: ChipInstant,
    filled: usize,
    chips: Vec<Complex32>,
    payload: Vec<Bit>,
    response: Option<AcquisitionPublication>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct MessageDelivery<'a> {
    pub ticket: MessageTicket,
    pub decoded: DecodedPrachMessage,
    pub block_bits: usize,
    pub blocks: usize,
    pub payload: &'a [Bit],
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub enum MessageOutcome<'a> {
    Decoded(MessageDelivery<'a>),
    Rejected(MessageTicket),
}

pub struct AccessMessages {
    epoch: RadioEpoch,
    owner: u64,
    channel: usize,
    profile: Vec<u8>,
    profile_bits: usize,
    code: PrachCode,
    rate: usize,
    message_delay: u64,
    next_serial: u64,
    newest: [Option<u64>; SIGNATURES],
    receivers: [Option<PrachMessageReceiver>; SIGNATURES],
    decoder: PrachMessageDecoder,
    layouts: Vec<(u16, usize, usize)>,
    slots: Vec<Reservation>,
    generation: Option<u64>,
    next_chip: Option<u64>,
    counters: MessageCounters,
}

impl AccessMessages {
    pub fn new(
        source: &dyn AccessTxTarget,
        channel: usize,
        rate: SamplesPerChip,
        limits: MessageLimits,
    ) -> Result<Self, MessageError> {
        let capacity = limits.capacity;
        if capacity.get() > MAX_ACCESS_RESPONSES {
            return Err(MessageError::Capacity);
        }
        let information = source.information();
        let access = information
            .sib5()
            .prach()
            .get(channel)
            .ok_or(MessageError::Profile)?;
        let settings = access.settings();
        let transport = access.transport();
        let interval = match transport.interval {
            TransportInterval::Ms10 => Rel99Tti::Ms10,
            TransportInterval::Ms20 => Rel99Tti::Ms20,
            _ => return Err(MessageError::Profile),
        };
        let rate_code = match transport.coding {
            ChannelCoding::Convolutional(ConvolutionalRate::Half) => CodeRate::Half,
            ChannelCoding::Convolutional(ConvolutionalRate::Third) => CodeRate::Third,
            _ => return Err(MessageError::Profile),
        };
        let crc = match transport.crc {
            CrcSize::Bits8 => TransportCrc::Crc8,
            CrcSize::Bits12 => TransportCrc::Crc12,
            CrcSize::Bits16 => TransportCrc::Crc16,
            CrcSize::Bits24 => TransportCrc::Crc24,
            CrcSize::None => return Err(MessageError::Profile),
        };
        let code = PrachCode::for_cell(
            information.primary_code(),
            settings.preamble_scrambling_code,
        )
        .map_err(MessageError::Dsp)?;
        let formats: Vec<_> = transport.formats().collect();
        if access.combinations().ctfc_values().len() > limits.maximum_formats.get() {
            return Err(MessageError::Capacity);
        }
        let maximum_payload = access
            .combinations()
            .ctfc_values()
            .iter()
            .map(|ctfc| {
                let &(bits, blocks) = formats.get(*ctfc as usize).ok_or(MessageError::Profile)?;
                if blocks == 0 {
                    return Err(MessageError::Profile);
                }
                Ok(usize::from(bits) * usize::from(blocks))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .ok_or(MessageError::Profile)?;
        let chips = interval.frames() * CHIPS_PER_FRAME as usize;
        let retained = chips
            .checked_mul(size_of::<Complex32>())
            .and_then(|bytes| bytes.checked_add(maximum_payload * size_of::<Bit>()))
            .and_then(|bytes| bytes.checked_mul(capacity.get()))
            .ok_or(MessageError::Capacity)?;
        if maximum_payload > limits.maximum_payload_bits.get()
            || retained > limits.maximum_buffer_bytes
        {
            return Err(MessageError::Capacity);
        }
        let mut plans = Vec::new();
        let mut layouts = Vec::new();
        for (tfci, ctfc) in access.combinations().ctfc_values().iter().enumerate() {
            let &(bits, blocks) = formats.get(*ctfc as usize).ok_or(MessageError::Profile)?;
            if blocks == 0 {
                return Err(MessageError::Profile);
            }
            let format = PrachFormat {
                block_bits: usize::from(bits),
                blocks: usize::from(blocks),
                crc,
                rate: rate_code,
                interval,
            };
            let plan = PrachTransportPlan::new(
                format,
                settings.minimum_spreading_factor.chips(),
                settings.puncturing_limit.percent(),
            )
            .map_err(MessageError::Dsp)?;
            layouts.push((tfci as u16, format.block_bits, format.blocks));
            plans.push((Tfci::new(tfci as u16).map_err(MessageError::Dsp)?, plan));
        }
        let signatures = (0..SIGNATURES)
            .filter(|index| settings.signatures.contains(*index as u8))
            .fold(0u16, |mask, index| mask | (1 << index));
        let decoder =
            PrachMessageDecoder::new(code, signatures, plans).map_err(MessageError::Dsp)?;
        let shared_code = Arc::new(PrachMessageCode::new(code).map_err(MessageError::Dsp)?);
        let mut receivers = std::array::from_fn(|_| None);
        for (index, receiver) in receivers.iter_mut().enumerate() {
            if signatures & (1 << index) != 0 {
                *receiver = Some(
                    PrachMessageReceiver::new(
                        shared_code.clone(),
                        PreambleSignature::new(index as u8).map_err(MessageError::Dsp)?,
                        interval,
                    )
                    .map_err(MessageError::Dsp)?,
                );
            }
        }
        let epoch = source.next_chip().epoch;
        let origin = ChipInstant { epoch, index: 0 };
        let empty_observation = AccessObservation {
            code,
            signature: PreambleSignature::new(0).map_err(MessageError::Dsp)?,
            nominal_start: origin,
            received_start: origin,
        };
        let empty_window = AccessWindow {
            indicator_start: origin,
            message_start: origin,
            duplicate: false,
        };
        let slots = (0..capacity.get())
            .map(|_| Reservation {
                state: State::Vacant,
                serial: 0,
                observation: empty_observation,
                window: empty_window,
                start: origin,
                filled: 0,
                chips: vec![Complex32::default(); chips],
                payload: vec![Bit::Zero; maximum_payload],
                response: None,
            })
            .collect();
        let profile = source
            .access_profile(channel)
            .ok_or(MessageError::Profile)?;
        Ok(Self {
            epoch,
            owner: NEXT_OWNER
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |owner| {
                    owner.checked_add(1)
                })
                .map_err(|_| MessageError::Capacity)?,
            channel,
            profile: profile.bytes.clone(),
            profile_bits: profile.bit_length,
            code,
            rate: rate.get(),
            message_delay: u64::from(settings.aich.timing.preamble_to_message_chips()),
            next_serial: 1,
            newest: [None; SIGNATURES],
            receivers,
            decoder,
            layouts,
            slots,
            generation: None,
            next_chip: None,
            counters: MessageCounters::default(),
        })
    }

    #[cfg(test)]
    pub fn counters(&self) -> MessageCounters {
        self.counters
    }

    pub fn maximum_payload_bits(&self) -> usize {
        self.slots[0].payload.len()
    }
    pub fn ready(&self) -> bool {
        self.slots.iter().any(|slot| slot.state == State::Ready)
    }

    pub fn reset_receive(&mut self) {
        self.discard_collecting();
    }

    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.state != State::Vacant)
            .count()
    }

    pub fn admit(
        &mut self,
        source: &mut dyn AccessTxTarget,
        observation: AccessObservation,
        now: ChipInstant,
        minimum_tx_lead_chips: u64,
    ) -> Result<MessageAdmission, MessageError> {
        self.reconcile_transmit();
        let result = self.admit_inner(source, observation, now, minimum_tx_lead_chips);
        if result.is_err() {
            self.counters.rejected = self.counters.rejected.saturating_add(1);
        }
        result
    }

    fn admit_inner(
        &mut self,
        source: &mut dyn AccessTxTarget,
        observation: AccessObservation,
        now: ChipInstant,
        minimum_tx_lead_chips: u64,
    ) -> Result<MessageAdmission, MessageError> {
        if source.next_chip().epoch != self.epoch
            || observation.nominal_start.epoch != self.epoch
            || observation.received_start.epoch != self.epoch
            || now.epoch != self.epoch
        {
            return Err(MessageError::Epoch);
        }
        let profile = source
            .access_profile(self.channel)
            .ok_or(MessageError::Profile)?;
        if profile.bit_length != self.profile_bits
            || profile.bytes != self.profile
            || observation.code != self.code
        {
            return Err(MessageError::Profile);
        }
        let signature = usize::from(observation.signature.number());
        if self.receivers[signature].is_none() {
            return Err(MessageError::Profile);
        }
        if let Some(slot) = self.slots.iter().find(|slot| {
            slot.state != State::Vacant
                && slot.observation.signature == observation.signature
                && slot.observation.nominal_start == observation.nominal_start
        }) {
            if slot.observation.received_start != observation.received_start {
                return Err(MessageError::Stale);
            }
            self.counters.duplicates = self.counters.duplicates.saturating_add(1);
            return Ok(MessageAdmission {
                ticket: MessageTicket {
                    epoch: self.epoch,
                    owner: self.owner,
                    serial: slot.serial,
                },
                window: AccessWindow {
                    duplicate: true,
                    ..slot.window
                },
            });
        }
        if self.newest[signature].is_some_and(|last| observation.nominal_start.index <= last) {
            return Err(MessageError::Stale);
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.state == State::Vacant)
            .ok_or(MessageError::Capacity)?;
        let serial = self.next_serial;
        let next_serial = serial.checked_add(1).ok_or(MessageError::Time)?;
        // All receive resources and fallible arithmetic must be admitted before publishing a positive AI.
        let message_start = observation
            .received_start
            .index
            .checked_add(self.message_delay)
            .ok_or(MessageError::Time)?;
        message_start
            .checked_add(slot.chips.len() as u64)
            .and_then(|end| end.checked_mul(self.rate as u64))
            .ok_or(MessageError::Time)?;
        if self.next_chip.is_some_and(|next| message_start < next) {
            return Err(MessageError::Stale);
        }
        let response = source
            .publish_acquisition(
                self.channel,
                observation,
                Acquisition::Positive,
                now,
                minimum_tx_lead_chips,
            )
            .map_err(MessageError::Transmit)?;
        let window = response.window;
        // TS 25.211 7.3 preserves the observed propagation offset from preamble to message part.
        let start = ChipInstant {
            epoch: self.epoch,
            index: message_start,
        };
        slot.start = start;
        slot.window = window;
        slot.observation = observation;
        slot.serial = serial;
        slot.filled = 0;
        slot.state = State::Collecting;
        slot.response = Some(response);
        self.next_serial = next_serial;
        self.newest[signature] = Some(observation.nominal_start.index);
        self.counters.admitted = self.counters.admitted.saturating_add(1);
        Ok(MessageAdmission {
            ticket: MessageTicket {
                epoch: self.epoch,
                owner: self.owner,
                serial,
            },
            window,
        })
    }

    #[cfg(test)]
    pub fn cancel(&mut self, ticket: MessageTicket) -> Result<(), MessageError> {
        if ticket.epoch != self.epoch {
            return Err(MessageError::Epoch);
        }
        if ticket.owner != self.owner {
            return Err(MessageError::UnknownTicket);
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.state != State::Vacant && slot.serial == ticket.serial)
            .ok_or(MessageError::UnknownTicket)?;
        slot.state = State::Vacant;
        slot.response = None;
        self.counters.discarded = self.counters.discarded.saturating_add(1);
        Ok(())
    }

    pub fn expire_before(&mut self, now: ChipInstant) -> Result<usize, MessageError> {
        if now.epoch != self.epoch {
            return Err(MessageError::Epoch);
        }
        self.reconcile_transmit();
        let mut expired = 0;
        for slot in &mut self.slots {
            if slot.state == State::Collecting
                && slot.start.index + slot.chips.len() as u64 <= now.index
            {
                slot.state = State::Vacant;
                slot.response = None;
                expired += 1;
            }
        }
        self.counters.discarded = self.counters.discarded.saturating_add(expired as u64);
        Ok(expired)
    }

    fn discard_collecting(&mut self) {
        for slot in &mut self.slots {
            if slot.state == State::Collecting {
                slot.state = State::Vacant;
                slot.response = None;
                self.counters.discarded = self.counters.discarded.saturating_add(1);
            }
        }
        self.next_chip = None;
        self.generation = None;
    }

    pub fn feed(&mut self, input: FilteredRx<'_>) -> Result<(), MessageError> {
        self.reconcile_transmit();
        let result = self.feed_inner(input);
        if result.is_err() {
            self.discard_collecting();
        }
        result
    }

    fn feed_inner(&mut self, input: FilteredRx<'_>) -> Result<(), MessageError> {
        if input.window.next_input.epoch != self.epoch {
            return Err(MessageError::Epoch);
        }
        if input.window.sample_stride != self.rate {
            return Err(MessageError::Phase);
        }
        if input.chips.len() > MAX_INPUT_CHIPS {
            return Err(MessageError::Capacity);
        }
        if input
            .chips
            .iter()
            .any(|value| !value.re.is_finite() || !value.im.is_finite())
        {
            return Err(MessageError::Dsp(DspError::NonFiniteEvidence));
        }
        if self
            .generation
            .is_some_and(|generation| generation != input.generation || input.discontinuity)
        {
            self.counters.discontinuities = self.counters.discontinuities.saturating_add(1);
            return Err(MessageError::Gap);
        }
        let Some(first) = input.window.first_center else {
            return if input.chips.is_empty() {
                Ok(())
            } else {
                Err(MessageError::Time)
            };
        };
        if first.epoch != self.epoch {
            return Err(MessageError::Epoch);
        }
        if !first.index.is_multiple_of(self.rate as u64) {
            return Err(MessageError::Phase);
        }
        let first = first.index / self.rate as u64;
        if self.next_chip.is_some_and(|next| next != first) {
            return Err(MessageError::Gap);
        }
        let end = first
            .checked_add(input.chips.len() as u64)
            .ok_or(MessageError::Time)?;
        self.generation = Some(input.generation);
        self.next_chip = Some(end);
        for slot in &mut self.slots {
            if slot.state != State::Collecting {
                continue;
            }
            let required = slot.start.index + slot.filled as u64;
            if first > required {
                slot.state = State::Vacant;
                slot.response = None;
                self.counters.discarded = self.counters.discarded.saturating_add(1);
                continue;
            }
            if required >= end {
                continue;
            }
            let skip = (required - first) as usize;
            let count = (slot.chips.len() - slot.filled).min(input.chips.len() - skip);
            slot.chips[slot.filled..slot.filled + count]
                .copy_from_slice(&input.chips[skip..skip + count]);
            slot.filled += count;
            if slot.filled == slot.chips.len() {
                slot.state = State::Ready;
            }
        }
        Ok(())
    }

    // Exactly one complete TTI is decoded per call, even when every reserved slot is ready.
    pub fn decode_one(&mut self) -> Result<Option<MessageOutcome<'_>>, MessageError> {
        self.reconcile_transmit();
        let Some(slot) = self
            .slots
            .iter_mut()
            .filter(|slot| slot.state == State::Ready)
            .min_by_key(|slot| (slot.start.index, slot.serial))
        else {
            return Ok(None);
        };
        slot.state = State::Vacant;
        slot.response = None;
        let ticket = MessageTicket {
            epoch: self.epoch,
            owner: self.owner,
            serial: slot.serial,
        };
        let receiver = self.receivers[usize::from(slot.observation.signature.number())]
            .as_mut()
            .ok_or(MessageError::Profile)?;
        let result = receiver
            .receive(slot.start, &slot.chips)
            .and_then(|message| self.decoder.decode(&message, &mut slot.payload));
        match result {
            Ok(Some(decoded)) => {
                let &(_, block_bits, blocks) = self
                    .layouts
                    .iter()
                    .find(|(tfci, _, _)| *tfci == decoded.tfci.tfci.index())
                    .ok_or(MessageError::Profile)?;
                self.counters.decoded = self.counters.decoded.saturating_add(1);
                Ok(Some(MessageOutcome::Decoded(MessageDelivery {
                    ticket,
                    decoded,
                    block_bits,
                    blocks,
                    payload: &slot.payload[..decoded.payload_bits],
                })))
            }
            Ok(None) => {
                self.counters.failed = self.counters.failed.saturating_add(1);
                Ok(Some(MessageOutcome::Rejected(ticket)))
            }
            Err(error) => {
                self.counters.failed = self.counters.failed.saturating_add(1);
                Err(MessageError::Dsp(error))
            }
        }
    }

    pub fn reconcile_transmit(&mut self) -> usize {
        let mut discarded = 0;
        for slot in &mut self.slots {
            if slot.state != State::Vacant
                && slot
                    .response
                    .as_ref()
                    .is_some_and(|response| !response.viable())
            {
                slot.state = State::Vacant;
                slot.response = None;
                discarded += 1;
            }
        }
        self.counters.transmit_rejected = self
            .counters
            .transmit_rejected
            .saturating_add(discarded as u64);
        self.counters.discarded = self.counters.discarded.saturating_add(discarded as u64);
        discarded
    }
}

#[cfg(test)]
pub(crate) mod tests;
