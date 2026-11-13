// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_codec::EncodedRrc;
use umts_rrc_schema::class_definitions::DLDCCHMessage;
use umts_security::{
    contexts::{IntegrityContext, KeyOrigin},
    counters::{IntegrityCounter, IntegrityReceiver, PublishError, ReceiveError, Start},
};
use umts_types::Direction;

use super::{Error, MAX_SRB, Parameters, VerifiedUplink, protect_downlink};

pub struct DownlinkIntegrity {
    context: IntegrityContext,
    bearer: u8,
    fresh: u32,
    counter: IntegrityCounter,
}

impl DownlinkIntegrity {
    pub fn new(
        context: IntegrityContext,
        bearer: u8,
        fresh: u32,
        first_count: u32,
    ) -> Result<Self, Error> {
        if bearer > MAX_SRB {
            return Err(Error::Bearer);
        }
        let counter =
            IntegrityCounter::from_count(context.origin().ue, Direction::Downlink, first_count);
        Ok(Self {
            context,
            bearer,
            fresh,
            counter,
        })
    }

    pub fn origin(&self) -> KeyOrigin {
        self.context.origin()
    }

    pub fn algorithm(&self) -> umts_security::negotiation::IntegrityAlgorithm {
        self.context.kind()
    }

    pub fn bearer(&self) -> u8 {
        self.bearer
    }

    pub fn fresh(&self) -> u32 {
        self.fresh
    }

    pub fn next_count(&self) -> Result<u32, umts_security::Error> {
        self.counter.next()
    }

    pub fn publish_with<T, E>(
        &mut self,
        origin: KeyOrigin,
        message: &DLDCCHMessage,
        publish: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, StreamError<E>> {
        let algorithm = self
            .context
            .algorithm(origin)
            .map_err(StreamError::Security)?;
        self.counter
            .publish_with(origin.ue, Direction::Downlink, |count| {
                let protected = protect_downlink(
                    Parameters {
                        key: algorithm,
                        count,
                        fresh: self.fresh,
                        bearer: self.bearer,
                        direction: Direction::Downlink,
                    },
                    message,
                )
                .map_err(StreamError::Protection)?;
                publish(&protected).map_err(StreamError::Consumer)
            })
            .map_err(|error| match error {
                PublishError::Counter(error) => StreamError::Security(error),
                PublishError::Downstream(error) => error,
            })
    }
}

pub struct UplinkIntegrity {
    context: IntegrityContext,
    bearer: u8,
    fresh: u32,
    receiver: IntegrityReceiver,
}

impl UplinkIntegrity {
    pub(super) fn from_next_count(
        context: IntegrityContext,
        bearer: u8,
        fresh: u32,
        next_count: u32,
    ) -> Self {
        let receiver =
            IntegrityReceiver::from_next_count(context.origin().ue, Direction::Uplink, next_count);
        Self {
            context,
            bearer,
            fresh,
            receiver,
        }
    }

    pub fn last_count(&self) -> Option<u32> {
        self.receiver.last_count()
    }

    pub fn new(
        context: IntegrityContext,
        bearer: u8,
        fresh: u32,
        start: Start,
        previous_sequence: Option<u8>,
    ) -> Result<Self, Error> {
        if bearer > MAX_SRB {
            return Err(Error::Bearer);
        }
        let receiver = IntegrityReceiver::new(
            context.origin().ue,
            Direction::Uplink,
            start,
            previous_sequence,
        )
        .map_err(Error::Algorithm)?;
        Ok(Self {
            context,
            bearer,
            fresh,
            receiver,
        })
    }

    pub fn origin(&self) -> KeyOrigin {
        self.context.origin()
    }

    pub fn last_sequence(&self) -> Option<u8> {
        self.receiver.last_sequence()
    }

    pub fn receive_with<'a, T, E>(
        &mut self,
        origin: KeyOrigin,
        bytes: &'a [u8],
        scratch: &mut [u8],
        accept: impl FnOnce(AcceptedUplink<'a>) -> Result<T, E>,
    ) -> Result<T, StreamError<E>> {
        self.receive_bits_with(
            origin,
            bytes,
            bytes.len() * super::OCTET_BITS,
            scratch,
            accept,
        )
    }

    pub fn receive_bits_with<'a, T, E>(
        &mut self,
        origin: KeyOrigin,
        bytes: &'a [u8],
        bit_length: usize,
        scratch: &mut [u8],
        accept: impl FnOnce(AcceptedUplink<'a>) -> Result<T, E>,
    ) -> Result<T, StreamError<E>> {
        let algorithm = self
            .context
            .algorithm(origin)
            .map_err(StreamError::Security)?;
        let sequence = super::uplink_sequence(bytes).map_err(StreamError::Protection)?;
        self.receiver
            .receive_with(origin.ue, Direction::Uplink, sequence, |count| {
                let verified = super::verify_uplink_bits(
                    Parameters {
                        key: algorithm,
                        count,
                        fresh: self.fresh,
                        bearer: self.bearer,
                        direction: Direction::Uplink,
                    },
                    bytes,
                    bit_length,
                    scratch,
                )
                .map_err(StreamError::Protection)?;
                accept(AcceptedUplink {
                    origin,
                    bearer: self.bearer,
                    verified,
                })
                .map_err(StreamError::Consumer)
            })
            .map_err(|error| match error {
                ReceiveError::Counter(error) => StreamError::Security(error),
                ReceiveError::Duplicate => StreamError::Duplicate,
                ReceiveError::Rejected(error) => error,
            })
    }
}

pub struct AcceptedUplink<'a> {
    origin: KeyOrigin,
    bearer: u8,
    verified: VerifiedUplink<'a>,
}

impl<'a> AcceptedUplink<'a> {
    pub fn origin(&self) -> KeyOrigin {
        self.origin
    }
    pub fn bearer(&self) -> u8 {
        self.bearer
    }
    pub fn bytes(&self) -> &'a [u8] {
        self.verified.bytes()
    }
    pub fn count(&self) -> u32 {
        self.verified.count()
    }
    pub fn bit_length(&self) -> usize {
        self.verified.bit_length()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StreamError<E> {
    Security(umts_security::Error),
    Protection(Error),
    Duplicate,
    Consumer(E),
}

#[cfg(test)]
mod tests;
