// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    LengthIndicator, LiWidth, MAX_LIS, SEQUENCE_HEADER_BYTES, SN_MODULUS, UmConfig, UmError,
    UmLimits, UmPdu, UmRxCounters, parse, storage,
};
use std::ops::Range;

const MAX_ACTIONS: usize = 2 * MAX_LIS + 8;

#[derive(Clone, Copy)]
enum Action<'a> {
    Start,
    Lose,
    Append(&'a [u8]),
    Finish(usize),
}

struct Actions<'a> {
    values: [Action<'a>; MAX_ACTIONS],
    count: usize,
}
impl<'a> Actions<'a> {
    fn push(&mut self, action: Action<'a>) {
        self.values[self.count] = action;
        self.count += 1;
    }
}

fn actions<'a>(pdu: &UmPdu<'a>, width: LiWidth, gap: bool) -> Actions<'a> {
    let mut actions = Actions {
        values: [Action::Lose; MAX_ACTIONS],
        count: 0,
    };
    if gap {
        actions.push(Action::Lose);
    }
    let indicators = pdu.indicators();
    let complete = pdu.complete_without_li
        || matches!(
            indicators.first(),
            Some(LengthIndicator::Complete | LengthIndicator::CompleteOneShort)
        );
    if complete {
        // TS 25.322 9.2.2.8 permits omitting a previous-exact LI before a complete-SDU indication.
        actions.push(Action::Finish(0));
        let trim = usize::from(indicators.first() == Some(&LengthIndicator::CompleteOneShort));
        actions.push(Action::Append(&pdu.data[..pdu.data.len() - trim]));
        actions.push(Action::Finish(0));
        return actions;
    }
    let mut cursor = 0;
    let mut padding = false;
    let mut last_end = false;
    for indicator in indicators {
        match indicator {
            LengthIndicator::Start => actions.push(Action::Start),
            LengthIndicator::PreviousExact => actions.push(Action::Finish(0)),
            LengthIndicator::PreviousOneShort => actions.push(Action::Finish(1)),
            LengthIndicator::End(end) => {
                let end = usize::from(*end);
                actions.push(Action::Append(&pdu.data[cursor..end]));
                actions.push(Action::Finish(0));
                cursor = end;
                last_end = true;
            }
            LengthIndicator::Padding => {
                padding = true;
                last_end = false;
            }
            LengthIndicator::Middle => {}
            LengthIndicator::Complete | LengthIndicator::CompleteOneShort => {
                unreachable!("complete indication handled")
            }
        }
    }
    // TS 25.322 9.2.2.8 leaves one octet implicit padding after an ordinary 15-bit LI when no further LI fits.
    if !(padding || width == LiWidth::Fifteen && last_end && pdu.data.len() - cursor == 1) {
        actions.push(Action::Append(&pdu.data[cursor..]));
    }
    actions
}

pub struct ReceivedUm<'a> {
    pub sequence: u8,
    pub position: u64,
    pub gap: bool,
    data: &'a [u8],
    boundaries: &'a [Range<usize>],
}

#[derive(Debug, PartialEq, Eq)]
pub enum UmReceiveError<E> {
    MalformedPdu(UmError),
    Rlc(UmError),
    Admission(E),
}
#[derive(Debug, PartialEq, Eq)]
pub enum UmPreparedReceiveError<P, A> {
    MalformedPdu(UmError),
    Rlc(UmError),
    Preparation(P),
    Admission(A),
}
impl<'a> ReceivedUm<'a> {
    pub fn sdus(&self) -> impl ExactSizeIterator<Item = &'a [u8]> + '_ {
        self.boundaries
            .iter()
            .map(|range| &self.data[range.clone()])
    }
}

pub struct UmReceiver {
    config: UmConfig,
    limits: UmLimits,
    last_position: Option<u64>,
    initial_position: u64,
    partial: Vec<u8>,
    length: usize,
    aligned: bool,
    output: Vec<u8>,
    boundaries: Vec<Range<usize>>,
    counters: UmRxCounters,
}

impl UmReceiver {
    pub fn configuration(&self) -> UmConfig {
        self.config
    }
    pub fn limits(&self) -> UmLimits {
        self.limits
    }
    pub fn pending_receive_data(&self) -> bool {
        self.length != 0
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
        // Completed bytes can include one old partial SDU plus at most one received PDU's data.
        let output = limits
            .maximum_sdu_bytes
            .get()
            .checked_add(limits.maximum_pdu_bytes.get())
            .ok_or(UmError::Capacity)?;
        let partial = limits
            .maximum_sdu_bytes
            .get()
            .checked_add(1)
            .ok_or(UmError::Capacity)?;
        let total = limits.receive_buffer_bytes()?;
        if total > limits.maximum_buffer_bytes {
            return Err(UmError::Capacity);
        }
        Ok(Self {
            config,
            limits,
            last_position: None,
            initial_position: position,
            partial: storage(partial)?,
            length: 0,
            aligned: true,
            output: storage(output)?,
            boundaries: Vec::with_capacity(MAX_LIS + 1),
            counters: UmRxCounters::default(),
        })
    }
    pub fn expected_sequence(&self) -> u8 {
        self.last_position.map_or(0, |last| {
            ((last % u64::from(SN_MODULUS) + 1) % u64::from(SN_MODULUS)) as u8
        })
    }
    pub fn next_position(&self) -> Result<u64, UmError> {
        self.last_position
            .map_or(Ok(self.initial_position), |last| {
                last.checked_add(1).ok_or(UmError::CountExhausted)
            })
    }
    pub fn receive_position(&self, sequence: u8) -> Result<u64, UmError> {
        if sequence >= SN_MODULUS {
            return Err(UmError::Header);
        }
        let modulus = u64::from(SN_MODULUS);
        let base = self
            .last_position
            .map_or(self.initial_position, |last| last - last % modulus);
        let base = if self
            .last_position
            .is_some_and(|last| u64::from(sequence) < last % modulus)
        {
            base.checked_add(modulus).ok_or(UmError::CountExhausted)?
        } else {
            base
        };
        let position = base
            .checked_add(u64::from(sequence))
            .ok_or(UmError::CountExhausted)?;
        position.checked_add(1).ok_or(UmError::CountExhausted)?;
        Ok(position)
    }
    pub fn counters(&self) -> UmRxCounters {
        self.counters
    }

    pub fn receive(&mut self, input: &[u8]) -> Result<ReceivedUm<'_>, UmError> {
        match self.receive_admitted(input, |_, _| Ok::<_, std::convert::Infallible>(())) {
            Ok(received) => Ok(received),
            Err(UmReceiveError::Rlc(error) | UmReceiveError::MalformedPdu(error)) => Err(error),
            Err(UmReceiveError::Admission(never)) => match never {},
        }
    }
    /// Preparation must preserve the clear sequence octet and runs before encrypted LI parsing.
    pub fn receive_prepared_admitted<'a, P, A>(
        &mut self,
        input: &[u8],
        prepare: impl FnOnce(u64) -> Result<&'a [u8], P>,
        admit: impl FnOnce(usize, usize) -> Result<(), A>,
    ) -> Result<ReceivedUm<'_>, UmPreparedReceiveError<P, A>> {
        if input.is_empty() || input.len() > self.limits.maximum_pdu_bytes.get() {
            return Err(UmPreparedReceiveError::Rlc(UmError::Length));
        }
        let position = self
            .receive_position(input[0] >> 1)
            .map_err(UmPreparedReceiveError::Rlc)?;
        let plain = prepare(position).map_err(UmPreparedReceiveError::Preparation)?;
        if plain.len() != input.len()
            || plain[..SEQUENCE_HEADER_BYTES] != input[..SEQUENCE_HEADER_BYTES]
        {
            return Err(UmPreparedReceiveError::Rlc(UmError::Header));
        }
        self.receive_admitted(plain, admit)
            .map_err(|error| match error {
                UmReceiveError::MalformedPdu(error) => UmPreparedReceiveError::MalformedPdu(error),
                UmReceiveError::Rlc(error) => UmPreparedReceiveError::Rlc(error),
                UmReceiveError::Admission(error) => UmPreparedReceiveError::Admission(error),
            })
    }

    // Admission reserves the entire delivered batch before sequence or reassembly state changes.
    pub fn receive_admitted<E>(
        &mut self,
        input: &[u8],
        admit: impl FnOnce(usize, usize) -> Result<(), E>,
    ) -> Result<ReceivedUm<'_>, UmReceiveError<E>> {
        if input.len() > self.limits.maximum_pdu_bytes.get() {
            self.counters.rejected = self.counters.rejected.saturating_add(1);
            return Err(UmReceiveError::Rlc(UmError::Capacity));
        }
        let pdu = match parse(input, self.config) {
            Ok(pdu) => pdu,
            Err(error) => {
                self.counters.rejected = self.counters.rejected.saturating_add(1);
                return Err(match error {
                    UmError::Header | UmError::Length => UmReceiveError::MalformedPdu(error),
                    error => UmReceiveError::Rlc(error),
                });
            }
        };
        let position = self
            .receive_position(pdu.sequence)
            .map_err(UmReceiveError::Rlc)?;
        let gap = pdu.sequence != self.expected_sequence();
        let actions = actions(&pdu, self.config.li_width, gap);
        let (deliveries, bytes) = self.preflight(&actions).map_err(|error| {
            self.counters.rejected = self.counters.rejected.saturating_add(1);
            UmReceiveError::Rlc(error)
        })?;
        admit(deliveries, bytes).map_err(UmReceiveError::Admission)?;
        self.boundaries.clear();
        let mut written = 0;
        for action in &actions.values[..actions.count] {
            match action {
                Action::Start => {
                    self.length = 0;
                    self.aligned = true;
                }
                Action::Lose => {
                    self.length = 0;
                    self.aligned = false;
                }
                Action::Append(bytes) if self.aligned => {
                    self.partial[self.length..self.length + bytes.len()].copy_from_slice(bytes);
                    self.length += bytes.len();
                }
                Action::Finish(trim) => {
                    if self.aligned && self.length > *trim {
                        let length = self.length - trim;
                        self.output[written..written + length]
                            .copy_from_slice(&self.partial[..length]);
                        self.boundaries.push(written..written + length);
                        written += length;
                    }
                    self.length = 0;
                    self.aligned = true;
                }
                _ => {}
            }
        }
        self.last_position = Some(position);
        self.counters.received = self.counters.received.saturating_add(1);
        self.counters.gaps = self.counters.gaps.saturating_add(u64::from(gap));
        self.counters.delivered = self
            .counters
            .delivered
            .saturating_add(self.boundaries.len() as u64);
        Ok(ReceivedUm {
            sequence: pdu.sequence,
            position,
            gap,
            data: &self.output[..written],
            boundaries: &self.boundaries,
        })
    }

    fn preflight(&self, actions: &Actions<'_>) -> Result<(usize, usize), UmError> {
        let mut length = self.length;
        let mut aligned = self.aligned;
        let mut deliveries = 0;
        let mut output = 0;
        for action in &actions.values[..actions.count] {
            match action {
                Action::Start => {
                    length = 0;
                    aligned = true;
                }
                Action::Lose => {
                    length = 0;
                    aligned = false;
                }
                Action::Append(bytes) if aligned => {
                    length = length.checked_add(bytes.len()).ok_or(UmError::Capacity)?;
                    if length > self.partial.len() {
                        return Err(UmError::Capacity);
                    }
                }
                Action::Finish(trim) => {
                    if aligned && length.saturating_sub(*trim) > self.limits.maximum_sdu_bytes.get()
                    {
                        return Err(UmError::Capacity);
                    }
                    if aligned && length > *trim {
                        deliveries += 1;
                        output += length - trim;
                    }
                    length = 0;
                    aligned = true;
                }
                _ => {}
            }
        }
        if deliveries > MAX_LIS + 1 || output > self.output.len() {
            return Err(UmError::Capacity);
        }
        Ok((deliveries, output))
    }
}
