// SPDX-License-Identifier: AGPL-3.0-or-later

mod receive;
mod transmit;
mod wire;

pub use receive::{ReceivedUm, UmPreparedReceiveError, UmReceiveError, UmReceiver};
pub use transmit::{UmPublication, UmPublishError, UmTransmitter};
pub use wire::{LengthIndicator, UmPdu, parse};

use std::num::NonZeroUsize;

const MAX_LIS: usize = 128;
const MAX_QUEUED_SDUS: usize = 256;
const SN_MODULUS: u8 = 128;
const SEQUENCE_HEADER_BYTES: usize = 1;
const SEVEN_BIT_MAX_PDU: usize = 125;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UmError {
    Capacity,
    Length,
    Header,
    Unsupported,
    CountExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiWidth {
    Seven,
    Fifteen,
}

impl LiWidth {
    pub fn octets(self) -> usize {
        match self {
            Self::Seven => 1,
            Self::Fifteen => 2,
        }
    }
    fn mask(self) -> u16 {
        match self {
            Self::Seven => 0x7f,
            Self::Fifteen => 0x7fff,
        }
    }
    fn ordinary_max(self) -> usize {
        usize::from(self.mask()) - if self == Self::Seven { 4 } else { 6 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EInterpretation {
    Normal,
    Alternative,
}

#[derive(Clone, Copy, Debug)]
pub struct UmConfig {
    pub li_width: LiWidth,
    pub interpretation: EInterpretation,
}

#[derive(Clone, Copy, Debug)]
pub struct UmLimits {
    pub maximum_pdu_bytes: NonZeroUsize,
    pub maximum_sdu_bytes: NonZeroUsize,
    pub queued_sdus: NonZeroUsize,
    // Sample-independent RLC data buffers only, excluding fixed metadata.
    pub maximum_buffer_bytes: usize,
}

impl UmLimits {
    pub fn transmit_buffer_bytes(self) -> Result<usize, UmError> {
        self.queued_sdus
            .get()
            .checked_mul(self.maximum_sdu_bytes.get())
            .and_then(|queued| {
                self.maximum_pdu_bytes
                    .get()
                    .checked_mul(2)
                    .and_then(|working| queued.checked_add(working))
            })
            .ok_or(UmError::Capacity)
    }
    pub fn receive_buffer_bytes(self) -> Result<usize, UmError> {
        self.maximum_sdu_bytes
            .get()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(self.maximum_pdu_bytes.get()))
            .and_then(|bytes| bytes.checked_add(1))
            .ok_or(UmError::Capacity)
    }
    pub fn validate(self, config: UmConfig) -> Result<(), UmError> {
        let maximum = match config.li_width {
            LiWidth::Seven => SEVEN_BIT_MAX_PDU,
            LiWidth::Fifteen => config.li_width.ordinary_max() + SEQUENCE_HEADER_BYTES,
        };
        if self.maximum_pdu_bytes.get() > maximum || self.queued_sdus.get() > MAX_QUEUED_SDUS {
            return Err(UmError::Capacity);
        }
        Ok(())
    }
}

fn validate_initial_position(position: u64) -> Result<(), UmError> {
    if !position.is_multiple_of(u64::from(SN_MODULUS)) {
        return Err(UmError::Header);
    }
    position
        .checked_add(u64::from(SN_MODULUS) - 1)
        .ok_or(UmError::CountExhausted)?;
    Ok(())
}

fn storage(length: usize) -> Result<Vec<u8>, UmError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| UmError::Capacity)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UmTxCounters {
    pub submitted: u64,
    pub completed_sdus: u64,
    pub publication_failures: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UmRxCounters {
    pub received: u64,
    pub gaps: u64,
    pub delivered: u64,
    pub rejected: u64,
}

#[cfg(test)]
mod tests;
