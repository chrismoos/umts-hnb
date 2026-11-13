// SPDX-License-Identifier: AGPL-3.0-or-later

mod diagnostics;
mod entity;
mod receipts;
mod segment;
mod status;
mod wire;

pub use diagnostics::{
    AmBoundaryEvidence, AmDeliveryEvidence, AmDiagnostics, AmRetainedPduEvidence, ControlEvidence,
    ControlHistory, PeerResetEvidence, ResetProgress, ResetStart, ResetTrigger, StatusHistory,
    StatusPublication,
};

pub use entity::{
    AmEntity, AmReceive, AmState, Delivery, DeliveryForecast, NewSduRange, PreparedReceive,
    Publication, PublicationKind, PublishError, ReceiveError, ReceivePreparation, ResetCounters,
    ResetTransition, StatusDeferral, WindowPositions,
};
pub use receipts::{ReceiveCoverage, SduReceipt, SduTransmission, StatusCoverage};
pub use status::{SnSet, Status, parse_status};
pub use wire::{AmdHeader, AmdPdu, Indicator, Reset, parse_amd, parse_amd_header, parse_reset};

use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveTime {
    received: umts_types::time::ChipInstant,
    serviced: umts_types::time::ChipInstant,
    provenance: ReceiveProvenance,
    air_start: Option<umts_types::time::ChipInstant>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReceiveProvenance {
    #[default]
    Unspecified,
    Path(NonZeroU64),
    Mixed,
}
impl ReceiveProvenance {
    fn combine(self, other: Self) -> Self {
        if self == other { self } else { Self::Mixed }
    }
}
impl ReceiveTime {
    pub fn new(
        received: umts_types::time::ChipInstant,
        serviced: umts_types::time::ChipInstant,
    ) -> Result<Self, AmError> {
        if received.epoch != serviced.epoch || received.index > serviced.index {
            return Err(AmError::Time);
        }
        Ok(Self {
            received,
            serviced,
            provenance: ReceiveProvenance::Unspecified,
            air_start: None,
        })
    }
    pub fn simultaneous(now: umts_types::time::ChipInstant) -> Self {
        Self {
            received: now,
            serviced: now,
            provenance: ReceiveProvenance::Unspecified,
            air_start: None,
        }
    }
    pub fn received(self) -> umts_types::time::ChipInstant {
        self.received
    }
    pub fn serviced(self) -> umts_types::time::ChipInstant {
        self.serviced
    }
    /// This is local reception metadata, not an RLC wire field or authentication result.
    pub fn with_provenance(mut self, provenance: ReceiveProvenance) -> Self {
        self.provenance = provenance;
        self
    }
    pub fn provenance(self) -> ReceiveProvenance {
        self.provenance
    }
    pub fn air_start(self) -> Option<umts_types::time::ChipInstant> {
        self.air_start
    }
    pub fn with_air_start(mut self, start: umts_types::time::ChipInstant) -> Result<Self, AmError> {
        if start.epoch != self.received.epoch || start.index > self.received.index {
            return Err(AmError::Time);
        }
        self.air_start = Some(start);
        Ok(self)
    }

    pub fn serviced_at(self, serviced: umts_types::time::ChipInstant) -> Result<Self, AmError> {
        if serviced.epoch != self.serviced.epoch || serviced.index < self.serviced.index {
            return Err(AmError::Time);
        }
        Ok(Self { serviced, ..self })
    }
}

pub const SN_BITS: usize = 12;
pub const SN_MODULUS: u16 = 1 << SN_BITS;
const SN_MASK: u16 = SN_MODULUS - 1;
const HFN_BITS: usize = 20;
const MAX_HFN: u32 = (1 << HFN_BITS) - 1;
const MAX_WINDOW: u16 = SN_MODULUS / 2 - 1;
const MAX_QUEUED_SDUS: usize = 256;
const MAX_LIS: usize = 128;
pub const MAX_DELIVERY_SDUS: usize = MAX_LIS + 1;
const MAX_PDU_BYTES: usize = 2048;
const AMD_HEADER_BYTES: usize = 2;
const SEVEN_BIT_MAX_PDU: usize = 126;
const MIN_CONTROL_PDU_BYTES: usize =
    (status::CONTROL_HEADER_BITS + status::SINGLE_MISSING_BITS + status::ACK_BITS)
        .div_ceil(umts_types::bits::BITS_PER_BYTE);
const DATA_FLAG: u16 = 0x8000;
const POLL_FLAG: u16 = 4;

/// TS 25.322 11.3.2 defines a STATUS PDU containing only NO_MORE as AM padding.
pub fn write_padding(output: &mut [u8]) -> Result<(), AmError> {
    if output.is_empty() || output.len() > MAX_PDU_BYTES {
        return Err(AmError::Length);
    }
    output.fill(0);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmError {
    Capacity,
    Length,
    Header,
    Unsupported,
    InconsistentStatus,
    ErroneousSequence,
    Time,
    State,
    CountExhausted,
    ResetLimit,
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
    fn for_fixed_size(bytes: usize) -> Self {
        if bytes <= SEVEN_BIT_MAX_PDU {
            Self::Seven
        } else {
            Self::Fifteen
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Polling {
    pub every_pdus: Option<NonZeroU16>,
    pub last_transmission: bool,
    pub last_retransmission: bool,
    pub timeout_chips: Option<NonZeroU64>,
    pub prohibit_chips: Option<NonZeroU64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmConfig {
    pub transmit_pdu_bytes: NonZeroUsize,
    pub receive_pdu_bytes: NonZeroUsize,
    pub transmit_window: NonZeroU16,
    pub receive_window: NonZeroU16,
    pub maximum_data_attempts: NonZeroU16,
    pub maximum_reset_attempts: NonZeroU16,
    pub reset_timeout_chips: NonZeroU64,
    pub polling: Polling,
    pub status_prohibit_chips: Option<NonZeroU64>,
    pub missing_pdu_indicator: bool,
    pub accept_special_he: bool,
    pub initial_tx_hfn: u32,
    pub initial_rx_hfn: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct AmLimits {
    pub maximum_sdu_bytes: NonZeroUsize,
    pub queued_sdus: NonZeroUsize,
    // Includes the entity, allocated slot metadata and retained buffers.
    pub maximum_buffer_bytes: usize,
}

impl AmConfig {
    pub fn buffer_bytes(self, limits: AmLimits) -> Result<usize, AmError> {
        if self.transmit_window.get() > MAX_WINDOW
            || self.receive_window.get() > MAX_WINDOW
            || limits.queued_sdus.get() > MAX_QUEUED_SDUS
            || self.initial_tx_hfn > MAX_HFN
            || self.initial_rx_hfn > MAX_HFN
        {
            return Err(AmError::Capacity);
        }
        for bytes in [self.transmit_pdu_bytes.get(), self.receive_pdu_bytes.get()] {
            if !(MIN_CONTROL_PDU_BYTES
                .max(AMD_HEADER_BYTES + 2 * LiWidth::for_fixed_size(bytes).octets())
                ..=MAX_PDU_BYTES)
                .contains(&bytes)
            {
                return Err(AmError::Capacity);
            }
        }
        let terms = [
            (1, std::mem::size_of::<AmEntity>()),
            (
                self.transmit_window.get() as usize,
                std::mem::size_of::<entity::TxSlot>(),
            ),
            (
                self.receive_window.get() as usize,
                std::mem::size_of::<entity::RxSlot>(),
            ),
            (
                limits.queued_sdus.get(),
                std::mem::size_of::<segment::Sdu>(),
            ),
            (
                limits.queued_sdus.get(),
                std::mem::size_of::<Option<receipts::Tracking>>(),
            ),
            (
                self.transmit_window.get() as usize,
                self.transmit_pdu_bytes.get(),
            ),
            (
                self.receive_window.get() as usize,
                self.receive_pdu_bytes.get(),
            ),
            (limits.queued_sdus.get(), limits.maximum_sdu_bytes.get()),
            (3, self.transmit_pdu_bytes.get()),
            (
                2,
                limits
                    .maximum_sdu_bytes
                    .get()
                    .checked_add(1)
                    .ok_or(AmError::Capacity)?,
            ),
            (
                1,
                limits
                    .maximum_sdu_bytes
                    .get()
                    .checked_add(self.receive_pdu_bytes.get())
                    .ok_or(AmError::Capacity)?,
            ),
        ];
        terms.into_iter().try_fold(0usize, |sum, (count, size)| {
            sum.checked_add(count.checked_mul(size).ok_or(AmError::Capacity)?)
                .ok_or(AmError::Capacity)
        })
    }
}

fn storage(length: usize) -> Result<Vec<u8>, AmError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| AmError::Capacity)?;
    bytes.resize(length, 0);
    Ok(bytes)
}
fn sn(value: u64) -> u16 {
    (value & u64::from(SN_MASK)) as u16
}
fn distance(base: u16, value: u16) -> u16 {
    value.wrapping_sub(base) & SN_MASK
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AmCounters {
    pub transmitted: u64,
    pub retransmitted: u64,
    pub statuses: u64,
    pub received: u64,
    pub duplicate: u64,
    pub outside_window: u64,
    pub delivered_sdus: u64,
    pub publication_failures: u64,
    pub resets: u64,
    pub repeated_one_short_markers: u64,
    pub malformed_pdus: u64,
}

#[cfg(test)]
mod tests;
