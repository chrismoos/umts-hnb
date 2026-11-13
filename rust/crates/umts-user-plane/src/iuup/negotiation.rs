// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use super::{
    Acknowledgment, Frame, PayloadCheck,
    initialization::{
        Combination, Formats, Initialization, MAX_CHAIN_FRAMES, MAX_RFCIS, ModeVersion, Versions,
    },
};
use crate::WireError;

const INITIALIZATION: u8 = 0;
const RATE_CONTROL: u8 = 1;
const TIME_ALIGNMENT: u8 = 2;
const ERROR_EVENT: u8 = 3;
// TS 25.415 6.6.3.16 error cause values.
const CAUSE_UNKNOWN_PROCEDURE: u8 = 5;
const CAUSE_TIME_ALIGNMENT_NOT_SUPPORTED: u8 = 47;
const IMPLEMENTED_VERSIONS: u16 = 3;
const ACK_SPARE_EXTENSION: usize = 32;
const ERROR_CAUSE_SHIFT: u8 = 2;
const RECEIVER_STORAGE_SETS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Cause {
    PayloadCrc = 1,
    FrameNumber = 2,
    ReservedValue = 6,
    TooShort = 8,
    UnexpectedProcedure = 18,
    Rfci = 19,
    Value = 20,
    Version = 49,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Time,
    Busy,
    NotRequest,
}

#[derive(Debug)]
pub enum PublishError<E> {
    Protocol(Error),
    Wire(WireError),
    Send(E),
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    timeout: Duration,
    repetitions: u8,
    supported: Versions,
}

impl Settings {
    // TINIT is operator configured, NINIT counts repetitions in TS 25.415 6.5.2/6.6.4.
    pub fn new(timeout: Duration, repetitions: u8, supported: Versions) -> Result<Self, Error> {
        if timeout.is_zero() || supported.mask() & !IMPLEMENTED_VERSIONS != 0 {
            return Err(Error::Configuration);
        }
        Ok(Self {
            timeout,
            repetitions,
            supported,
        })
    }

    pub fn supported(self) -> Versions {
        self.supported
    }

    pub fn restrict_to(self, supported: Versions) -> Result<Self, Error> {
        if self.supported.intersection(supported) != Some(supported) {
            return Err(Error::Configuration);
        }
        Ok(Self { supported, ..self })
    }
}

#[derive(Clone, Debug)]
pub struct Negotiated {
    mode: ModeVersion,
    formats: Arc<Formats>,
}

impl Negotiated {
    pub fn mode(&self) -> ModeVersion {
        self.mode
    }
    pub fn formats(&self) -> &Formats {
        &self.formats
    }
    pub fn same_session(&self, other: &Self) -> bool {
        self.mode == other.mode && Arc::ptr_eq(&self.formats, &other.formats)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryReason {
    Timeout,
    Negative,
    Erroneous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    pub frame: u8,
    pub reason: RetryReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckEffect {
    Ignored,
    Retry,
    Next,
    Complete,
}

#[derive(Clone, Copy)]
struct Attempt {
    frame: usize,
    transmissions: u16,
    chosen: Option<ModeVersion>,
}

enum SendState {
    First(Attempt),
    Awaiting { attempt: Attempt, deadline: Instant },
    Retry(Attempt),
    Active(Negotiated),
    Failed(Failure),
}

pub struct Sender {
    payloads: Box<[Box<[u8]>]>,
    formats: Arc<Formats>,
    offered: Versions,
    encoding: ModeVersion,
    settings: Settings,
    state: SendState,
    last: Option<Instant>,
}

impl Sender {
    pub fn new(
        frames: &[Initialization],
        encoding: ModeVersion,
        settings: Settings,
    ) -> Result<Self, Error> {
        let first = frames.first().ok_or(Error::Configuration)?;
        if frames.len() > MAX_CHAIN_FRAMES
            || !settings.supported.supports(encoding)
            || first.versions().intersection(settings.supported) != Some(first.versions())
        {
            return Err(Error::Configuration);
        }
        let mut formats = Formats::from_initialization(first);
        let mut payloads = Vec::with_capacity(frames.len());
        for (index, frame) in frames.iter().enumerate() {
            if frame.chained() != (index + 1 < frames.len()) || frame.versions() != first.versions()
            {
                return Err(Error::Configuration);
            }
            if index != 0 {
                formats = formats.append(frame).map_err(|_| Error::Configuration)?;
            }
            let mut payload = vec![0; frame.encoded_bytes()];
            frame
                .encode(&mut payload)
                .map_err(|_| Error::Configuration)?;
            payloads.push(payload.into_boxed_slice());
        }
        Ok(Self {
            payloads: payloads.into_boxed_slice(),
            formats: Arc::new(formats),
            offered: first.versions(),
            encoding,
            settings,
            state: SendState::First(Attempt {
                frame: 0,
                transmissions: 0,
                chosen: None,
            }),
            last: None,
        })
    }

    fn observe(&mut self, now: Instant) -> Result<(), Error> {
        if self.last.is_some_and(|last| now < last) {
            return Err(Error::Time);
        }
        self.last = Some(now);
        Ok(())
    }

    fn retry(&mut self, attempt: Attempt, reason: RetryReason) {
        self.state = if attempt.transmissions > u16::from(self.settings.repetitions) {
            SendState::Failed(Failure {
                frame: attempt.frame as u8,
                reason,
            })
        } else {
            SendState::Retry(attempt)
        };
    }

    pub fn tick(&mut self, now: Instant) -> Result<(), Error> {
        self.observe(now)?;
        if let SendState::Awaiting { attempt, deadline } = self.state
            && now >= deadline
        {
            self.retry(attempt, RetryReason::Timeout);
        }
        Ok(())
    }

    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            SendState::Awaiting { deadline, .. } => Some(deadline),
            _ => None,
        }
    }

    pub fn proposed_formats(&self) -> &Formats {
        &self.formats
    }

    pub fn maximum_frame_bytes(&self) -> usize {
        self.payloads
            .iter()
            .map(|payload| payload.len())
            .max()
            .unwrap_or(0)
            + super::CHECKED_DATA_HEADER_BYTES
    }

    pub fn storage_bytes(&self) -> usize {
        self.payloads
            .iter()
            .map(|payload| payload.len())
            .sum::<usize>()
            + std::mem::size_of_val(self.payloads.as_ref())
            + std::mem::size_of::<Formats>()
            + (self.formats.rows().count() - 1)
                * std::mem::size_of::<super::initialization::Combination>()
    }

    pub fn negotiated(&self) -> Option<&Negotiated> {
        match &self.state {
            SendState::Active(value) => Some(value),
            _ => None,
        }
    }

    pub fn failure(&self) -> Option<Failure> {
        match self.state {
            SendState::Failed(value) => Some(value),
            _ => None,
        }
    }

    /// Successful publication must mean the datagram was accepted by its transport.
    pub fn publish<E>(
        &mut self,
        now: Instant,
        output: &mut [u8],
        send: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<bool, PublishError<E>> {
        self.tick(now).map_err(PublishError::Protocol)?;
        let attempt = match self.state {
            SendState::First(attempt) | SendState::Retry(attempt) => attempt,
            _ => return Ok(false),
        };
        let deadline = now
            .checked_add(self.settings.timeout)
            .ok_or(PublishError::Protocol(Error::Time))?;
        let bytes = super::encode(
            Frame::Control {
                number: attempt.frame as u8,
                acknowledgment: Acknowledgment::Procedure,
                mode: self.encoding.wire(),
                procedure: INITIALIZATION,
                payload_crc: PayloadCheck::Valid,
                payload: &self.payloads[attempt.frame],
            },
            output,
        )
        .map_err(PublishError::Wire)?;
        send(&output[..bytes]).map_err(PublishError::Send)?;
        self.state = SendState::Awaiting {
            attempt: Attempt {
                transmissions: attempt.transmissions + 1,
                ..attempt
            },
            deadline,
        };
        Ok(true)
    }

    pub fn acknowledgment(&mut self, frame: Frame<'_>, now: Instant) -> Result<AckEffect, Error> {
        self.observe(now)?;
        let Frame::Control {
            number,
            acknowledgment,
            mode,
            procedure: INITIALIZATION,
            payload_crc,
            payload,
        } = frame
        else {
            return Ok(AckEffect::Ignored);
        };
        if acknowledgment == Acknowledgment::Procedure {
            return Ok(AckEffect::Ignored);
        }
        let attempt = match self.state {
            SendState::Awaiting { attempt, .. } | SendState::Retry(attempt) => attempt,
            _ => return Ok(AckEffect::Ignored),
        };
        let version = ModeVersion::from_wire(mode).ok();
        let valid = number as usize == attempt.frame
            && payload_crc == PayloadCheck::Absent
            && version.is_some_and(|value| {
                self.offered.supports(value) && attempt.chosen.is_none_or(|chosen| value == chosen)
            })
            && payload.len() <= ACK_SPARE_EXTENSION;
        if acknowledgment == Acknowledgment::Positive && valid {
            let mode = version.ok_or(Error::Configuration)?;
            if attempt.frame + 1 == self.payloads.len() {
                self.state = SendState::Active(Negotiated {
                    mode,
                    formats: self.formats.clone(),
                });
                Ok(AckEffect::Complete)
            } else {
                self.state = SendState::First(Attempt {
                    frame: attempt.frame + 1,
                    transmissions: 0,
                    chosen: Some(mode),
                });
                Ok(AckEffect::Next)
            }
        } else {
            self.retry(
                attempt,
                if acknowledgment == Acknowledgment::Negative {
                    RetryReason::Negative
                } else {
                    RetryReason::Erroneous
                },
            );
            Ok(AckEffect::Retry)
        }
    }
}

#[derive(Clone)]
struct Accepted {
    frames: Box<[Initialization]>,
    negotiated: Negotiated,
}

#[derive(Clone)]
enum Received {
    Empty,
    Chain(Accepted),
    Active(Accepted),
}

impl Received {
    fn record(&self) -> Option<&Accepted> {
        match self {
            Self::Empty => None,
            Self::Chain(value) | Self::Active(value) => Some(value),
        }
    }
}

struct Response {
    number: u8,
    mode: ModeVersion,
    cause: Option<[u8; 1]>,
}

impl Response {
    fn frame(&self) -> Frame<'_> {
        Frame::Control {
            number: self.number,
            acknowledgment: if self.cause.is_some() {
                Acknowledgment::Negative
            } else {
                Acknowledgment::Positive
            },
            mode: self.mode.wire(),
            procedure: INITIALIZATION,
            payload_crc: PayloadCheck::Absent,
            payload: self.cause.as_ref().map_or(&[], |cause| cause.as_slice()),
        }
    }
}

/// TS 25.415 6.5.3 to 6.5.5: a reply owed for a non-initialization control frame. These
/// procedures never change the negotiated Iu-UP state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlReply {
    number: u8,
    mode: u8,
    procedure: u8,
    cause: Option<[u8; 1]>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlProcedure {
    /// TS 25.415 6.5.3.1 RATE CONTROL: acknowledged. The requested rate is not applied.
    RateControl(ControlReply),
    /// TS 25.415 6.5.4.2 TIME ALIGNMENT: refused with cause 47, no IPTIs are offered.
    TimeAlignment(ControlReply),
    /// TS 25.415 6.5.5 ERROR EVENT: no reply.
    ErrorEvent,
    /// TS 25.415 6.6.3.16 cause 5: refused as an unknown procedure.
    Unknown(ControlReply),
}

impl ControlProcedure {
    /// `None` for data frames, acknowledgments and INITIALISATION.
    pub fn classify(frame: Frame<'_>) -> Option<Self> {
        let Frame::Control {
            number,
            acknowledgment: Acknowledgment::Procedure,
            mode,
            procedure,
            ..
        } = frame
        else {
            return None;
        };
        let reply = |cause: Option<u8>| ControlReply {
            number,
            mode,
            procedure,
            cause: cause.map(|cause| [cause << ERROR_CAUSE_SHIFT]),
        };
        Some(match procedure {
            INITIALIZATION => return None,
            RATE_CONTROL => Self::RateControl(reply(None)),
            TIME_ALIGNMENT => Self::TimeAlignment(reply(Some(CAUSE_TIME_ALIGNMENT_NOT_SUPPORTED))),
            ERROR_EVENT => Self::ErrorEvent,
            _ => Self::Unknown(reply(Some(CAUSE_UNKNOWN_PROCEDURE))),
        })
    }
    pub fn reply(self) -> Option<ControlReply> {
        match self {
            Self::RateControl(reply) | Self::TimeAlignment(reply) | Self::Unknown(reply) => {
                Some(reply)
            }
            Self::ErrorEvent => None,
        }
    }
}

impl ControlReply {
    pub fn encode(&self, output: &mut [u8]) -> Result<usize, crate::WireError> {
        super::encode(self.frame(), output)
    }
    fn frame(&self) -> Frame<'_> {
        Frame::Control {
            number: self.number,
            acknowledgment: if self.cause.is_some() {
                Acknowledgment::Negative
            } else {
                Acknowledgment::Positive
            },
            mode: self.mode,
            procedure: self.procedure,
            payload_crc: PayloadCheck::Absent,
            payload: self.cause.as_ref().map_or(&[], |cause| cause.as_slice()),
        }
    }
}

enum ReceiveState {
    Ready(Received),
    Reply {
        previous: Received,
        next: Option<Received>,
        response: Response,
    },
}

pub struct Receiver {
    supported: Versions,
    state: ReceiveState,
}

impl Receiver {
    // This includes bounded previous/candidate/decoded control data, not allocator bookkeeping.
    pub fn storage_ceiling() -> usize {
        std::mem::size_of::<Self>()
            + RECEIVER_STORAGE_SETS
                * (std::mem::size_of::<Accepted>()
                    + MAX_CHAIN_FRAMES * std::mem::size_of::<Initialization>()
                    + std::mem::size_of::<Formats>()
                    + 2 * MAX_RFCIS * std::mem::size_of::<Combination>())
    }

    pub fn new(supported: Versions) -> Result<Self, Error> {
        if supported.mask() & !IMPLEMENTED_VERSIONS != 0 {
            return Err(Error::Configuration);
        }
        Ok(Self {
            supported,
            state: ReceiveState::Ready(Received::Empty),
        })
    }

    /// The original negotiated format identity must survive a rejected peer reinitialization.
    pub fn from_negotiated(supported: Versions, active: Negotiated) -> Result<Self, Error> {
        let mut receiver = Self::new(supported)?;
        if !supported.supports(active.mode()) {
            return Err(Error::Configuration);
        }
        receiver.state = ReceiveState::Ready(Received::Active(Accepted {
            frames: Box::new([]),
            negotiated: active,
        }));
        Ok(receiver)
    }

    pub fn negotiated(&self) -> Option<&Negotiated> {
        match &self.state {
            ReceiveState::Ready(Received::Active(value)) => Some(&value.negotiated),
            _ => None,
        }
    }

    pub fn response_pending(&self) -> bool {
        matches!(self.state, ReceiveState::Reply { .. })
    }

    pub fn pending_accepted(&self) -> bool {
        matches!(self.state, ReceiveState::Reply { next: Some(_), .. })
    }

    pub fn pending_negotiated(&self) -> Option<&Negotiated> {
        match &self.state {
            ReceiveState::Reply {
                next: Some(Received::Active(value)),
                ..
            } => Some(&value.negotiated),
            _ => None,
        }
    }

    pub fn reject_pending(&mut self, cause: Cause) -> Result<(), Error> {
        let ReceiveState::Reply { next, response, .. } = &mut self.state else {
            return Err(Error::NotRequest);
        };
        *next = None;
        response.cause = Some([(cause as u8) << ERROR_CAUSE_SHIFT]);
        Ok(())
    }

    pub fn request(&mut self, frame: Frame<'_>) -> Result<(), Error> {
        self.request_with(frame, |_| Ok(()))
    }

    /// Admission must validate the proposed formats against the actual RAB before ACK publication.
    pub fn request_with(
        &mut self,
        frame: Frame<'_>,
        admit: impl FnOnce(&Formats) -> Result<(), Cause>,
    ) -> Result<(), Error> {
        let ReceiveState::Ready(previous) = &self.state else {
            return Err(Error::Busy);
        };
        let Frame::Control {
            number,
            acknowledgment: Acknowledgment::Procedure,
            mode,
            procedure: INITIALIZATION,
            payload_crc,
            payload,
        } = frame
        else {
            return Err(Error::NotRequest);
        };
        let decoded = Initialization::decode(payload);
        let response_mode = decoded
            .as_ref()
            .ok()
            .and_then(|value| value.versions().intersection(self.supported))
            .unwrap_or(self.supported)
            .highest();
        let candidate = (|| {
            if payload_crc != PayloadCheck::Valid {
                return Err(Cause::PayloadCrc);
            }
            let frame = decoded.map_err(|error| match error {
                WireError::Length => Cause::TooShort,
                WireError::Unsupported(_) => Cause::ReservedValue,
                _ => Cause::Value,
            })?;
            if !ModeVersion::from_wire(mode).is_ok_and(|mode| self.supported.supports(mode)) {
                return Err(Cause::Version);
            }
            let chosen = frame
                .versions()
                .intersection(self.supported)
                .ok_or(Cause::Version)?
                .highest();
            if number as usize >= MAX_CHAIN_FRAMES
                || (usize::from(number) + 1 == MAX_CHAIN_FRAMES && frame.chained())
            {
                return Err(Cause::FrameNumber);
            }
            if let Some(record) = previous.record()
                && let Some(old) = record.frames.get(usize::from(number))
                && old == &frame
            {
                return Ok(previous.clone());
            }
            let record = match previous {
                Received::Chain(record) => {
                    if usize::from(number) != record.frames.len() {
                        return Err(Cause::FrameNumber);
                    }
                    if !frame.versions().supports(record.negotiated.mode) {
                        return Err(Cause::Version);
                    }
                    let formats = record
                        .negotiated
                        .formats
                        .append(&frame)
                        .map_err(|_| Cause::Rfci)?;
                    let mut frames = Vec::with_capacity(record.frames.len() + 1);
                    frames.extend(record.frames.iter().cloned());
                    frames.push(frame.clone());
                    Accepted {
                        frames: frames.into_boxed_slice(),
                        negotiated: Negotiated {
                            mode: record.negotiated.mode,
                            formats: Arc::new(formats),
                        },
                    }
                }
                Received::Empty | Received::Active(_) => {
                    if number != 0 {
                        return Err(Cause::FrameNumber);
                    }
                    Accepted {
                        frames: vec![frame.clone()].into_boxed_slice(),
                        negotiated: Negotiated {
                            mode: chosen,
                            formats: Arc::new(Formats::from_initialization(&frame)),
                        },
                    }
                }
            };
            admit(record.negotiated.formats())?;
            Ok(if frame.chained() {
                Received::Chain(record)
            } else {
                Received::Active(record)
            })
        })();
        let (next, cause, mode) = match candidate {
            Ok(next) => {
                let mode = next.record().ok_or(Error::Configuration)?.negotiated.mode;
                (Some(next), None, mode)
            }
            Err(cause) => (
                None,
                Some([(cause as u8) << ERROR_CAUSE_SHIFT]),
                response_mode,
            ),
        };
        let previous =
            match std::mem::replace(&mut self.state, ReceiveState::Ready(Received::Empty)) {
                ReceiveState::Ready(previous) => previous,
                ReceiveState::Reply { .. } => unreachable!("validated ready receiver"),
            };
        self.state = ReceiveState::Reply {
            previous,
            next,
            response: Response {
                number,
                mode,
                cause,
            },
        };
        Ok(())
    }

    pub fn publish<E>(
        &mut self,
        output: &mut [u8],
        send: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<bool, PublishError<E>> {
        let ReceiveState::Reply { response, .. } = &self.state else {
            return Ok(false);
        };
        let bytes = super::encode(response.frame(), output).map_err(PublishError::Wire)?;
        send(&output[..bytes]).map_err(PublishError::Send)?;
        let ReceiveState::Reply { previous, next, .. } =
            std::mem::replace(&mut self.state, ReceiveState::Ready(Received::Empty))
        else {
            unreachable!("published pending response");
        };
        self.state = ReceiveState::Ready(next.unwrap_or(previous));
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
