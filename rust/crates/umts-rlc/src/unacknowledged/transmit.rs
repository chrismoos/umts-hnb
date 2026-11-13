// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    EInterpretation, LengthIndicator, MAX_LIS, SEQUENCE_HEADER_BYTES, SN_MODULUS, UmConfig,
    UmError, UmLimits, UmTxCounters, storage,
};

struct Sdu {
    bytes: Vec<u8>,
    length: usize,
    consumed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    None,
    Exact,
    OneShort,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UmPublishError<E> {
    Rlc(UmError),
    Downstream(E),
}
pub struct UmPublication<'a> {
    pub bytes: &'a [u8],
    pub position: u64,
}

pub struct UmTransmitter {
    config: UmConfig,
    limits: UmLimits,
    sdus: Vec<Sdu>,
    head: usize,
    count: usize,
    position: u64,
    boundary: Boundary,
    output: Vec<u8>,
    data: Vec<u8>,
    counters: UmTxCounters,
}

impl UmTransmitter {
    pub fn limits(&self) -> UmLimits {
        self.limits
    }
    pub fn minimum_pdu_bytes(&self) -> usize {
        SEQUENCE_HEADER_BYTES + 2 * self.config.li_width.octets()
    }
    pub fn new(config: UmConfig, limits: UmLimits) -> Result<Self, UmError> {
        Self::with_initial_position(config, limits, 0)
    }
    pub fn with_initial_position(
        config: UmConfig,
        limits: UmLimits,
        position: u64,
    ) -> Result<Self, UmError> {
        super::validate_initial_position(position)?;
        limits.validate(config)?;
        if config.interpretation != EInterpretation::Normal {
            return Err(UmError::Unsupported);
        }
        let bytes = limits.transmit_buffer_bytes()?;
        if bytes > limits.maximum_buffer_bytes {
            return Err(UmError::Capacity);
        }
        let sdus = (0..limits.queued_sdus.get())
            .map(|_| {
                Ok(Sdu {
                    bytes: storage(limits.maximum_sdu_bytes.get())?,
                    length: 0,
                    consumed: 0,
                })
            })
            .collect::<Result<_, UmError>>()?;
        Ok(Self {
            config,
            limits,
            sdus,
            head: 0,
            count: 0,
            position,
            boundary: Boundary::None,
            output: storage(limits.maximum_pdu_bytes.get())?,
            data: storage(limits.maximum_pdu_bytes.get())?,
            counters: UmTxCounters::default(),
        })
    }

    pub fn pending_sdus(&self) -> usize {
        self.count
    }
    pub fn has_pending(&self) -> bool {
        self.count != 0 || self.boundary != Boundary::None
    }
    pub fn sequence(&self) -> u8 {
        (self.position % u64::from(SN_MODULUS)) as u8
    }
    pub fn next_position(&self) -> u64 {
        self.position
    }
    pub fn counters(&self) -> UmTxCounters {
        self.counters
    }
    pub fn enqueue(&mut self, input: &[u8]) -> Result<(), UmError> {
        if input.is_empty() {
            return Err(UmError::Length);
        }
        if input.len() > self.limits.maximum_sdu_bytes.get() || self.count == self.sdus.len() {
            return Err(UmError::Capacity);
        }
        let slot = &mut self.sdus[(self.head + self.count) % self.limits.queued_sdus.get()];
        slot.bytes[..input.len()].copy_from_slice(input);
        slot.length = input.len();
        slot.consumed = 0;
        self.count += 1;
        Ok(())
    }

    // TS 25.322 9.4 advances VT(US) on submission, not preparation or a rejected downstream publication.
    pub fn transmit_with<T, E>(
        &mut self,
        pdu_bytes: usize,
        publish: impl FnOnce(&[u8]) -> Result<T, E>,
    ) -> Result<Option<T>, UmPublishError<E>> {
        self.transmit_positioned_with(pdu_bytes, |publication| publish(publication.bytes))
    }
    pub fn transmit_positioned_with<T, E>(
        &mut self,
        pdu_bytes: usize,
        publish: impl FnOnce(UmPublication<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, UmPublishError<E>> {
        if self.count == 0 && self.boundary == Boundary::None {
            return Ok(None);
        }
        self.transmit_or_padding_positioned_with(pdu_bytes, publish)
    }

    /// TS 25.322 9.2.2.8: an allocated transport block may contain only padding, with its own SN.
    pub fn transmit_or_padding_positioned_with<T, E>(
        &mut self,
        pdu_bytes: usize,
        publish: impl FnOnce(UmPublication<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, UmPublishError<E>> {
        let next = self
            .position
            .checked_add(1)
            .ok_or(UmPublishError::Rlc(UmError::CountExhausted))?;
        let width = self.config.li_width.octets();
        if pdu_bytes < SEQUENCE_HEADER_BYTES + 2 * width
            || pdu_bytes > self.limits.maximum_pdu_bytes.get()
        {
            return Err(UmPublishError::Rlc(UmError::Capacity));
        }
        let mut indicators = [LengthIndicator::Padding; MAX_LIS];
        let mut li_count = 0;
        let mut boundary = Boundary::None;
        match self.boundary {
            Boundary::Exact => {
                indicators[li_count] = LengthIndicator::PreviousExact;
                li_count += 1;
            }
            Boundary::OneShort => {
                indicators[li_count] = LengthIndicator::PreviousOneShort;
                li_count += 1;
            }
            Boundary::None if self.count > 0 && self.sdus[self.head].consumed == 0 => {
                indicators[li_count] = LengthIndicator::Start;
                li_count += 1;
            }
            Boundary::None => {}
        }
        let mut copied = 0;
        let mut finished = 0;
        let mut partial_consumed = None;
        while finished < self.count && li_count + 1 < MAX_LIS {
            let slot = &self.sdus[(self.head + finished) % self.sdus.len()];
            let remaining = slot.length - slot.consumed;
            let free = pdu_bytes - SEQUENCE_HEADER_BYTES - li_count * width - copied;
            if remaining >= free {
                self.data[copied..copied + free]
                    .copy_from_slice(&slot.bytes[slot.consumed..slot.consumed + free]);
                copied += free;
                if remaining == free {
                    finished += 1;
                    boundary = Boundary::Exact;
                } else {
                    partial_consumed = Some(slot.consumed + free);
                }
                break;
            }
            if width == 2 && remaining + 1 == free {
                self.data[copied..copied + remaining]
                    .copy_from_slice(&slot.bytes[slot.consumed..slot.length]);
                copied += remaining;
                self.data[copied] = 0;
                copied += 1;
                finished += 1;
                boundary = Boundary::OneShort;
                break;
            }
            indicators[li_count] = LengthIndicator::End((copied + remaining) as u16);
            li_count += 1;
            self.data[copied..copied + remaining]
                .copy_from_slice(&slot.bytes[slot.consumed..slot.length]);
            copied += remaining;
            finished += 1;
            let free = pdu_bytes - SEQUENCE_HEADER_BYTES - li_count * width - copied;
            if free < width {
                self.data[copied..copied + free].fill(0);
                copied += free;
                break;
            }
        }
        if boundary == Boundary::None {
            let free = pdu_bytes - SEQUENCE_HEADER_BYTES - li_count * width - copied;
            if free >= width {
                indicators[li_count] = LengthIndicator::Padding;
                li_count += 1;
            }
        }
        let header = SEQUENCE_HEADER_BYTES + li_count * width;
        let data_bytes = pdu_bytes - header;
        if copied > data_bytes {
            return Err(UmPublishError::Rlc(UmError::Header));
        }
        let sequence = self.sequence();
        self.output[..pdu_bytes].fill(0);
        self.output[0] = (sequence << 1) | u8::from(li_count != 0);
        for (index, indicator) in indicators[..li_count].iter().enumerate() {
            let word =
                (indicator.value(self.config.li_width) << 1) | u16::from(index + 1 != li_count);
            let cursor = SEQUENCE_HEADER_BYTES + index * width;
            if width == 1 {
                self.output[cursor] = word as u8;
            } else {
                self.output[cursor..cursor + width].copy_from_slice(&word.to_be_bytes());
            }
        }
        self.output[header..header + copied].copy_from_slice(&self.data[..copied]);
        let result = match publish(UmPublication {
            bytes: &self.output[..pdu_bytes],
            position: self.position,
        }) {
            Ok(result) => result,
            Err(error) => {
                self.counters.publication_failures =
                    self.counters.publication_failures.saturating_add(1);
                return Err(UmPublishError::Downstream(error));
            }
        };
        for index in 0..finished {
            let slot = &mut self.sdus[(self.head + index) % self.limits.queued_sdus.get()];
            slot.length = 0;
            slot.consumed = 0;
        }
        self.head = (self.head + finished) % self.sdus.len();
        self.count -= finished;
        if let Some(consumed) = partial_consumed {
            self.sdus[self.head].consumed = consumed;
        }
        self.boundary = boundary;
        self.position = next;
        self.counters.submitted = self.counters.submitted.saturating_add(1);
        self.counters.completed_sdus = self.counters.completed_sdus.saturating_add(finished as u64);
        Ok(Some(result))
    }
}
