// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AmCounters, AmState, PublicationKind, StatusDeferral};
use umts_types::time::ChipInstant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmDeliveryEvidence {
    pub sequence: u64,
    pub header: u16,
    pub pdu_bytes: usize,
    pub first_indicator: Option<super::Indicator>,
    pub parse_error: Option<super::AmError>,
    pub partial_bytes: usize,
    pub partial_first: Option<u64>,
    pub preceding_buffer: Option<AmBoundaryEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmBoundaryEvidence {
    pub header: u16,
    pub received_at: ChipInstant,
    pub first_indicator: Option<super::Indicator>,
    pub last_indicator: Option<super::Indicator>,
    pub data_bytes: usize,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmRetainedPduEvidence {
    pub position: u64,
    pub boundary: AmBoundaryEvidence,
    pub one_byte_padding: bool,
    pub padding_octet: Option<u8>,
    pub first_data_octet: Option<u8>,
    pub last_data_octet: Option<u8>,
}

const CONTROL_PREFIX_BYTES: usize = 16;
const STATUS_HISTORY_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusPublication {
    pub at: ChipInstant,
    pub generation: u64,
    pub acknowledge_next: u64,
    pub receive_high: u64,
    pub delivered_next: u64,
    pub received_pdus: u64,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusHistory {
    pub records: [Option<StatusPublication>; STATUS_HISTORY_CAPACITY],
    pub published: u64,
}

impl Default for StatusHistory {
    fn default() -> Self {
        Self {
            records: [None; STATUS_HISTORY_CAPACITY],
            published: 0,
        }
    }
}

impl StatusHistory {
    pub(super) fn record(&mut self, value: StatusPublication) {
        let index = (self.published % STATUS_HISTORY_CAPACITY as u64) as usize;
        self.records[index] = Some(value);
        self.published = self.published.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetTrigger {
    Explicit,
    InvalidStatus {
        acknowledge: Option<u16>,
        negative: Option<u16>,
    },
    DataAttempts {
        sequence: u16,
        attempts: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetStart {
    pub at: ChipInstant,
    pub trigger: ResetTrigger,
    pub tx_base: u64,
    pub tx_next: u64,
    pub rx_next: u64,
    pub rx_high: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlEvidence {
    pub at: ChipInstant,
    pub kind: PublicationKind,
    pub bytes: usize,
    pub prefix: [u8; CONTROL_PREFIX_BYTES],
}
impl ControlEvidence {
    pub(super) fn new(at: ChipInstant, kind: PublicationKind, bytes: &[u8]) -> Self {
        let mut prefix = [0; CONTROL_PREFIX_BYTES];
        let retained = bytes.len().min(prefix.len());
        prefix[..retained].copy_from_slice(&bytes[..retained]);
        Self {
            at,
            kind,
            bytes: bytes.len(),
            prefix,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetProgress {
    pub sequence: bool,
    pub hfn: u32,
    pub attempts: u16,
    pub deadline: Option<ChipInstant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmDiagnostics {
    pub at: ChipInstant,
    pub state: AmState,
    pub tx_base: u64,
    pub tx_next: u64,
    pub rx_next: u64,
    pub rx_high: u64,
    pub counters: AmCounters,
    pub transmit_window: u16,
    pub pending_new_data: bool,
    pub pending_sdus: usize,
    pub status_pending: bool,
    pub status_deferral: Option<StatusDeferral>,
    pub status_prohibit: Option<ChipInstant>,
    pub poll_deadline: Option<ChipInstant>,
    pub poll_target: Option<u64>,
    pub reset: Option<ResetProgress>,
    pub history: ControlHistory,
    pub status_publications: Option<StatusHistory>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlHistory {
    pub reset_started: Option<ResetStart>,
    pub peer_reset: Option<PeerResetEvidence>,
    pub last_received: Option<ControlEvidence>,
    pub last_published: Option<ControlEvidence>,
    pub resets_published: u64,
    pub reset_acks_published: u64,
    pub reset_acks_received: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerResetEvidence {
    pub at: ChipInstant,
    pub sequence: bool,
    pub hfn: u32,
    pub tx_base: u64,
    pub tx_next: u64,
    pub rx_next: u64,
    pub rx_high: u64,
    pub rx_deliver: u64,
    pub status_pending: bool,
    pub status_prohibit: Option<u64>,
    pub counters: AmCounters,
    pub previous_received: Option<ControlEvidence>,
    pub previous_published: Option<ControlEvidence>,
}
