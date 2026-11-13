// SPDX-License-Identifier: AGPL-3.0-or-later

use super::diagnostics::{
    AmDiagnostics, ControlEvidence, ControlHistory, PeerResetEvidence, ResetProgress, ResetStart,
    ResetTrigger, StatusHistory, StatusPublication,
};
use super::{
    AmConfig, AmCounters, AmError, AmLimits, Indicator, LiWidth, MAX_HFN, MAX_LIS, POLL_FLAG,
    ReceiveCoverage, Reset, SN_BITS, SN_MASK, SduReceipt, SduTransmission, Status, StatusCoverage,
    distance, parse_amd, parse_amd_header, parse_reset, parse_status,
    receipts::{self, Tracking},
    segment::Queue,
    sn,
    status::{CONTROL_HEADER_BITS, MAX_MISSING_RUN, Writer},
    storage,
    wire::{HE_LI, HE_MASK, data_header, write_reset},
};
use umts_types::time::ChipInstant;

mod confirmation;
mod scan;
use confirmation::StatusConfirmation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmState {
    Ready,
    ResetPending,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmReceive {
    Stored,
    Duplicate,
    OutsideWindow,
    StatusApplied,
    ResetStarted,
    Ignored,
    ResetApplied { tx_hfn: u32, rx_hfn: u32 },
    ResetAcknowledged { tx_hfn: u32, rx_hfn: u32 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationKind {
    Data {
        sequence: u16,
        count: u32,
        retransmission: bool,
    },
    Status,
    Reset,
    ResetAck,
}
pub struct Publication<'a> {
    pub bytes: &'a [u8],
    pub kind: PublicationKind,
    pub position: Option<u64>,
    pub status_coverage: Option<StatusCoverage>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum PublishError<E> {
    Rlc(AmError),
    Downstream(E),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceivePreparation {
    Data {
        sequence: u16,
        position: u64,
    },
    Control,
    Reset {
        message: Reset,
        transition: Option<ResetTransition>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetTransition {
    pub tx_hfn: u32,
    pub rx_hfn: u32,
    pub tx_position: u64,
    pub rx_position: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResetCounters {
    pub transmit_hfn: Option<u32>,
    pub receive_hfn: Option<u32>,
}

pub struct PreparedReceive<'a> {
    pub plaintext: &'a [u8],
    pub reset_counters: Option<ResetCounters>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPositions {
    pub next_transmit: u64,
    pub oldest_transmit: u64,
    pub next_receive: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewSduRange {
    pub first: u64,
    pub next: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryForecast {
    pub queue_revision: u64,
    pub queued_pdus: usize,
    pub appended_pdus: usize,
    pub total_new_pdus: usize,
    pub outstanding_pdus: usize,
    pub first_position: u64,
    pub next_position: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusDeferral {
    UndeliveredData,
    UpperAdmission,
}
enum StatusAdmission {
    Immediate,
    Delivered { upper_pending: bool },
}

#[derive(Clone, Copy)]
struct CounterBasis {
    position: u64,
    count: u32,
}

impl CounterBasis {
    fn at(self, position: u64) -> Result<u32, AmError> {
        let delta = position
            .checked_sub(self.position)
            .ok_or(AmError::CountExhausted)?;
        self.count
            .checked_add(u32::try_from(delta).map_err(|_| AmError::CountExhausted)?)
            .ok_or(AmError::CountExhausted)
    }
}

fn reset_position(next: u64, origin: u64, hfn: u32) -> Result<u64, AmError> {
    let highest = if next > origin { next - 1 } else { origin };
    let block = (highest >> SN_BITS)
        .checked_add(1)
        .ok_or(AmError::CountExhausted)?;
    let extended = block
        .checked_mul(u64::from(super::SN_MODULUS))
        .ok_or(AmError::CountExhausted)?;
    Ok(extended.max(u64::from(hfn) << SN_BITS))
}

enum MappedError<M, E> {
    Rlc(AmError),
    Mapping(M),
    Downstream(E),
}
impl<M, E> From<AmError> for MappedError<M, E> {
    fn from(error: AmError) -> Self {
        Self::Rlc(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReceiveError<E> {
    Rlc(AmError),
    Preparation(E),
}
impl<E> From<AmError> for PublishError<E> {
    fn from(value: AmError) -> Self {
        Self::Rlc(value)
    }
}

pub(super) struct TxSlot {
    sequence: Option<u64>,
    bytes: Vec<u8>,
    attempts: u16,
    acknowledged: bool,
    retransmit: bool,
}
pub(super) struct RxSlot {
    sequence: Option<u64>,
    retained_sequence: Option<u64>,
    received_at: ChipInstant,
    provenance: super::ReceiveProvenance,
    air_start: Option<ChipInstant>,
    bytes: Vec<u8>,
}
#[derive(Clone, Copy)]
struct LocalReset {
    initialization: ResetCounters,
    message: Reset,
    attempts: u16,
    deadline: Option<u64>,
}

pub struct Delivery<'a> {
    bytes: &'a [u8],
    ends: &'a [usize],
    coverage: &'a [ReceiveCoverage],
}
impl Delivery<'_> {
    pub fn sdus(&self) -> impl Iterator<Item = &[u8]> {
        let mut start = 0;
        self.ends.iter().map(move |&end| {
            let bytes = &self.bytes[start..end];
            start = end;
            bytes
        })
    }
    pub fn count(&self) -> usize {
        self.ends.len()
    }
    pub fn covered_sdus(&self) -> impl Iterator<Item = (&[u8], ReceiveCoverage)> {
        self.sdus().zip(self.coverage.iter().copied())
    }
}

pub struct AmEntity {
    config: AmConfig,
    limits: AmLimits,
    now: ChipInstant,
    queue: Queue,
    tx: Vec<TxSlot>,
    // Counts occupied slots with retransmit set and acknowledged clear.
    retransmit_count: usize,
    rx: Vec<RxSlot>,
    tx_count: CounterBasis,
    rx_count: CounterBasis,
    highest_tx_count: u32,
    tx_base: u64,
    tx_next: u64,
    tx_window: u16,
    rx_next: u64,
    rx_high: u64,
    rx_deliver: u64,
    partial: Vec<u8>,
    partial_length: usize,
    partial_first: Option<u64>,
    previous_one_short_delimited: bool,
    partial_first_received: Option<ChipInstant>,
    partial_air_start: Option<ChipInstant>,
    partial_provenance: Option<super::ReceiveProvenance>,
    partial_work: Vec<u8>,
    output: Vec<u8>,
    ends: [usize; MAX_LIS + 1],
    coverage: [ReceiveCoverage; MAX_LIS + 1],
    receipt_owner: u64,
    receive_generation: u64,
    published_status: Option<StatusCoverage>,
    status_confirmation: Option<StatusConfirmation>,
    acknowledged_status: Option<StatusCoverage>,
    status_admission: StatusAdmission,
    next_receipt: u64,
    tracking: Vec<Option<Tracking>>,
    scratch: Vec<u8>,
    poll_counter: u16,
    poll_requested: bool,
    poll_deadline: Option<u64>,
    pending_mrw_ack: Option<(u8, u16)>,
    poll_target: Option<u64>,
    poll_prohibit: Option<u64>,
    status_pending: bool,
    status_cursor: Option<u64>,
    status_prohibit: Option<u64>,
    local_reset: Option<LocalReset>,
    next_reset_sequence: bool,
    peer_reset_ack: Option<Reset>,
    last_peer_reset: Option<Reset>,
    failed: bool,
    counters: AmCounters,
    control_history: ControlHistory,
    status_history: Option<Box<StatusHistory>>,
}

impl AmEntity {
    pub fn configuration(&self) -> AmConfig {
        self.config
    }
    pub fn limits(&self) -> AmLimits {
        self.limits
    }
    pub fn new(config: AmConfig, limits: AmLimits, now: ChipInstant) -> Result<Self, AmError> {
        if config.buffer_bytes(limits)? > limits.maximum_buffer_bytes {
            return Err(AmError::Capacity);
        }
        let tx_origin = u64::from(config.initial_tx_hfn) << SN_BITS;
        let rx_origin = u64::from(config.initial_rx_hfn) << SN_BITS;
        let receipt_owner = receipts::new_owner()?;
        let tx = (0..config.transmit_window.get())
            .map(|_| {
                Ok(TxSlot {
                    sequence: None,
                    bytes: storage(config.transmit_pdu_bytes.get())?,
                    attempts: 0,
                    acknowledged: false,
                    retransmit: false,
                })
            })
            .collect::<Result<_, AmError>>()?;
        let rx = (0..config.receive_window.get())
            .map(|_| {
                Ok(RxSlot {
                    sequence: None,
                    retained_sequence: None,
                    received_at: now,
                    provenance: super::ReceiveProvenance::Unspecified,
                    air_start: None,
                    bytes: storage(config.receive_pdu_bytes.get())?,
                })
            })
            .collect::<Result<_, AmError>>()?;
        Ok(Self {
            config,
            limits,
            now,
            queue: Queue::new(config.transmit_pdu_bytes.get(), limits)?,
            tx,
            retransmit_count: 0,
            rx,
            tx_count: CounterBasis {
                position: tx_origin,
                count: config.initial_tx_hfn << SN_BITS,
            },
            rx_count: CounterBasis {
                position: rx_origin,
                count: config.initial_rx_hfn << SN_BITS,
            },
            highest_tx_count: config.initial_tx_hfn << SN_BITS,
            tx_base: tx_origin,
            tx_next: tx_origin,
            tx_window: config.transmit_window.get(),
            rx_next: rx_origin,
            rx_high: rx_origin,
            rx_deliver: rx_origin,
            partial: storage(
                limits
                    .maximum_sdu_bytes
                    .get()
                    .checked_add(1)
                    .ok_or(AmError::Capacity)?,
            )?,
            partial_length: 0,
            partial_first: None,
            previous_one_short_delimited: false,
            partial_first_received: None,
            partial_air_start: None,
            partial_provenance: None,
            partial_work: storage(
                limits
                    .maximum_sdu_bytes
                    .get()
                    .checked_add(1)
                    .ok_or(AmError::Capacity)?,
            )?,
            output: storage(
                limits
                    .maximum_sdu_bytes
                    .get()
                    .checked_add(config.receive_pdu_bytes.get())
                    .ok_or(AmError::Capacity)?,
            )?,
            ends: [0; MAX_LIS + 1],
            coverage: [ReceiveCoverage {
                owner: receipt_owner,
                generation: 0,
                first: 0,
                last: 0,
                first_received: now,
                provenance: super::ReceiveProvenance::Unspecified,
                air_start: None,
            }; MAX_LIS + 1],
            receipt_owner,
            receive_generation: 0,
            published_status: None,
            status_confirmation: None,
            acknowledged_status: None,
            status_admission: StatusAdmission::Immediate,
            next_receipt: 1,
            tracking: vec![None; limits.queued_sdus.get()],
            scratch: storage(config.transmit_pdu_bytes.get())?,
            poll_counter: 0,
            poll_requested: false,
            poll_deadline: None,
            pending_mrw_ack: None,
            poll_target: None,
            poll_prohibit: None,
            status_pending: false,
            status_cursor: None,
            status_prohibit: None,
            local_reset: None,
            next_reset_sequence: false,
            peer_reset_ack: None,
            last_peer_reset: None,
            failed: false,
            counters: AmCounters::default(),
            control_history: ControlHistory::default(),
            status_history: None,
        })
    }
    pub fn state(&self) -> AmState {
        if self.failed {
            AmState::Failed
        } else if self.local_reset.is_some() {
            AmState::ResetPending
        } else {
            AmState::Ready
        }
    }
    pub fn counters(&self) -> AmCounters {
        self.counters
    }
    pub fn retained_pdu_evidence(&self, position: u64) -> Option<super::AmRetainedPduEvidence> {
        let slot = &self.rx[(position % self.rx.len() as u64) as usize];
        if slot.retained_sequence != Some(position) {
            return None;
        }
        let pdu = parse_amd(
            &slot.bytes,
            LiWidth::for_fixed_size(slot.bytes.len()),
            self.config.accept_special_he,
        )
        .ok()?;
        Some(super::AmRetainedPduEvidence {
            position,
            boundary: super::AmBoundaryEvidence {
                header: u16::from_be_bytes([slot.bytes[0], slot.bytes[1]]),
                received_at: slot.received_at,
                first_indicator: pdu.indicators().first().copied(),
                last_indicator: pdu.indicators().last().copied(),
                data_bytes: pdu.data.len(),
                complete: pdu.complete,
            },
            one_byte_padding: pdu.one_byte_padding,
            padding_octet: pdu.one_byte_padding.then(|| *slot.bytes.last().unwrap()),
            first_data_octet: pdu.data.first().copied(),
            last_data_octet: pdu.data.last().copied(),
        })
    }
    pub fn status_history_enabled(&self) -> bool {
        self.status_history.is_some()
    }
    pub fn delivery_evidence(&self) -> Option<super::AmDeliveryEvidence> {
        if self.state() != AmState::Ready || self.rx_deliver >= self.rx_next {
            return None;
        }
        let index = (self.rx_deliver % self.rx.len() as u64) as usize;
        let slot = &self.rx[index];
        if slot.sequence != Some(self.rx_deliver) {
            return None;
        }
        let parsed = parse_amd(
            &slot.bytes,
            LiWidth::for_fixed_size(self.config.receive_pdu_bytes.get()),
            self.config.accept_special_he,
        );
        let preceding_buffer = self.rx_deliver.checked_sub(1).and_then(|previous| {
            let previous = &self.rx[(previous % self.rx.len() as u64) as usize];
            if previous.sequence.is_some() {
                return None;
            }
            let header = parse_amd_header(&previous.bytes, self.config.accept_special_he).ok()?;
            if header.sequence != sn(self.rx_deliver - 1) {
                return None;
            }
            let pdu = parse_amd(
                &previous.bytes,
                LiWidth::for_fixed_size(previous.bytes.len()),
                self.config.accept_special_he,
            )
            .ok()?;
            Some(super::AmBoundaryEvidence {
                header: u16::from_be_bytes([previous.bytes[0], previous.bytes[1]]),
                received_at: previous.received_at,
                first_indicator: pdu.indicators().first().copied(),
                last_indicator: pdu.indicators().last().copied(),
                data_bytes: pdu.data.len(),
                complete: pdu.complete,
            })
        });
        Some(super::AmDeliveryEvidence {
            sequence: self.rx_deliver,
            header: u16::from_be_bytes([slot.bytes[0], slot.bytes[1]]),
            pdu_bytes: slot.bytes.len(),
            first_indicator: parsed
                .as_ref()
                .ok()
                .and_then(|pdu| pdu.indicators().first().copied()),
            parse_error: parsed.err(),
            partial_bytes: self.partial_length,
            partial_first: self.partial_first,
            preceding_buffer,
        })
    }
    pub fn diagnostics(&self) -> AmDiagnostics {
        AmDiagnostics {
            at: self.now,
            state: self.state(),
            tx_base: self.tx_base,
            tx_next: self.tx_next,
            rx_next: self.rx_next,
            rx_high: self.rx_high,
            counters: self.counters,
            transmit_window: self.tx_window,
            pending_new_data: self.pending_new_data(),
            pending_sdus: self.queue.pending_sdus(),
            status_pending: self.status_pending,
            status_deferral: self.status_deferral(),
            status_prohibit: self
                .status_prohibit
                .map(|index| ChipInstant { index, ..self.now }),
            poll_deadline: self
                .poll_deadline
                .map(|index| ChipInstant { index, ..self.now }),
            poll_target: self.poll_target,
            reset: self.local_reset.map(|reset| ResetProgress {
                sequence: reset.message.sequence,
                hfn: reset.message.hfn,
                attempts: reset.attempts,
                deadline: reset
                    .deadline
                    .map(|index| ChipInstant { index, ..self.now }),
            }),
            history: self.control_history,
            status_publications: self.status_history.as_deref().copied(),
        }
    }
    pub fn enable_status_history(&mut self) {
        self.status_history
            .get_or_insert_with(|| Box::new(StatusHistory::default()));
    }
    /// Accepted lower-queue publication is not evidence of RF transmission or UE reception.
    pub fn published_status(&self) -> Option<StatusCoverage> {
        self.published_status
    }
    pub fn require_delivery_admission(&mut self) {
        if matches!(self.status_admission, StatusAdmission::Immediate) {
            self.status_admission = StatusAdmission::Delivered {
                upper_pending: false,
            };
        }
    }
    pub fn upper_admission_pending(&mut self, pending: bool) {
        if let StatusAdmission::Delivered { upper_pending } = &mut self.status_admission {
            *upper_pending = pending;
        }
    }
    pub fn status_deferral(&self) -> Option<StatusDeferral> {
        match self.status_admission {
            StatusAdmission::Immediate => None,
            StatusAdmission::Delivered { .. } if self.rx_deliver < self.rx_next => {
                Some(StatusDeferral::UndeliveredData)
            }
            StatusAdmission::Delivered {
                upper_pending: true,
            } => Some(StatusDeferral::UpperAdmission),
            StatusAdmission::Delivered {
                upper_pending: false,
            } => None,
        }
    }
    pub fn send_sequence(&self) -> u16 {
        sn(self.tx_next)
    }
    pub fn acknowledge_sequence(&self) -> u16 {
        sn(self.tx_base)
    }
    pub fn receive_sequence(&self) -> u16 {
        sn(self.rx_next)
    }
    pub fn outstanding_pdus(&self) -> usize {
        (self.tx_next - self.tx_base) as usize
    }
    #[cfg(test)]
    pub(super) fn check_retransmission_count(&self) {
        assert_eq!(
            self.retransmit_count,
            self.tx
                .iter()
                .filter(|slot| slot.sequence.is_some() && slot.retransmit && !slot.acknowledged)
                .count()
        );
    }
    /// Includes a deferred end-of-SDU LI even when no payload bytes remain queued.
    pub fn pending_new_data(&self) -> bool {
        self.queue.pending()
    }
    pub fn forecast_delivery(&self, appended_bytes: usize) -> Result<DeliveryForecast, AmError> {
        if self.state() != AmState::Ready {
            return Err(AmError::State);
        }
        let outstanding_pdus = self.outstanding_pdus();
        let available = usize::from(self.tx_window)
            .checked_sub(outstanding_pdus)
            .ok_or(AmError::Capacity)?;
        let counts = self.queue.forecast(appended_bytes, available)?;
        if counts.total_pdus > available {
            return Err(AmError::Capacity);
        }
        let next_position = self
            .tx_next
            .checked_add(u64::try_from(counts.total_pdus).map_err(|_| AmError::CountExhausted)?)
            .ok_or(AmError::CountExhausted)?;
        Ok(DeliveryForecast {
            queue_revision: counts.queue_revision,
            queued_pdus: counts.queued_pdus,
            appended_pdus: counts
                .total_pdus
                .checked_sub(counts.queued_pdus)
                .ok_or(AmError::State)?,
            total_new_pdus: counts.total_pdus,
            outstanding_pdus,
            first_position: self.tx_next,
            next_position,
        })
    }
    pub fn publication_ready(&mut self, now: ChipInstant) -> Result<bool, AmError> {
        self.tick(now)?;
        if self.failed {
            return Err(AmError::State);
        }
        if self.peer_reset_ack.is_some() {
            return Ok(true);
        }
        if let Some(reset) = self.local_reset {
            return Ok(reset.deadline.is_none_or(|deadline| now.index >= deadline));
        }
        Ok(self.status_ready(now) || self.data_choice(now)?.is_some())
    }
    fn status_ready(&self, now: ChipInstant) -> bool {
        self.status_pending
            && self.status_deferral().is_none()
            && (self.status_cursor.is_some()
                || self
                    .status_prohibit
                    .is_none_or(|deadline| now.index >= deadline))
    }
    fn data_choice(&self, now: ChipInstant) -> Result<Option<(u64, bool)>, AmError> {
        let tx_limit = self
            .tx_base
            .checked_add(u64::from(self.tx_window))
            .ok_or(AmError::CountExhausted)?;
        let eligible = |absolute: u64| absolute < tx_limit || absolute + 1 == self.tx_next;
        let retransmit = if self.retransmit_count == 0 {
            None
        } else {
            scan::forward(&self.tx, self.tx_base..self.tx_next)
                .find(|(absolute, slot)| {
                    eligible(*absolute) && slot.retransmit && !slot.acknowledged
                })
                .map(|(absolute, _)| absolute)
        };
        let new = retransmit.is_none() && self.queue.pending() && self.tx_next < tx_limit;
        let selected = retransmit
            .or(if new { Some(self.tx_next) } else { None })
            .or_else(|| {
                if self.poll_requested
                    && self
                        .poll_prohibit
                        .is_none_or(|deadline| now.index >= deadline)
                {
                    scan::backward(&self.tx, self.tx_base..self.tx_next)
                        .find(|(absolute, slot)| eligible(*absolute) && !slot.acknowledged)
                        .map(|(absolute, _)| absolute)
                } else {
                    None
                }
            });
        Ok(selected.map(|absolute| (absolute, new)))
    }
    /// Includes out-of-order PDUs and partial SDUs whose in-order fragments have already been consumed.
    pub fn pending_receive_data(&self) -> bool {
        self.rx_deliver < self.rx_high || self.partial_first.is_some()
    }
    /// Buffered PDUs and ambiguous partial provenance conservatively retain every path.
    pub fn pending_receive_path(&self, path: std::num::NonZeroU64) -> bool {
        self.rx_deliver < self.rx_high
            || self.partial_first.is_some()
                && !matches!(self.partial_provenance,
                    Some(super::ReceiveProvenance::Path(other)) if other != path)
    }
    /// Predicts an isolated SDU including its deferred LI. This is not publication or acknowledgment evidence.
    pub fn isolated_sdu_range(&self, bytes: usize) -> Result<NewSduRange, AmError> {
        if self.state() != AmState::Ready {
            return Err(AmError::State);
        }
        let pdus = self.queue.isolated_pdus(bytes)?;
        let next = self
            .tx_next
            .checked_add(u64::try_from(pdus).map_err(|_| AmError::CountExhausted)?)
            .ok_or(AmError::CountExhausted)?;
        Ok(NewSduRange {
            first: self.tx_next,
            next,
        })
    }
    pub fn receive_count(&self, sequence: u16) -> Result<u32, AmError> {
        self.rx_count.at(self.receive_position(sequence)?)
    }
    pub fn window_positions(&self) -> WindowPositions {
        WindowPositions {
            next_transmit: self.tx_next,
            oldest_transmit: self.tx_base,
            next_receive: self.rx_next,
        }
    }
    /// Re-seeding is allowed only before first data or after an actual reset, never to rewind a live window.
    pub fn reseed_idle_counters(&mut self, tx_hfn: u32, rx_hfn: u32) -> Result<(), AmError> {
        if tx_hfn > MAX_HFN || rx_hfn > MAX_HFN {
            return Err(AmError::CountExhausted);
        }
        if self.failed
            || self.local_reset.is_some()
            || self.tx_next != self.tx_count.position
            || self.tx_base != self.tx_next
            || self.rx_next != self.rx_count.position
            || self.rx_high != self.rx_next
            || self.rx_deliver != self.rx_next
        {
            return Err(AmError::State);
        }
        self.tx_count.count = tx_hfn << SN_BITS;
        self.rx_count.count = rx_hfn << SN_BITS;
        self.highest_tx_count = tx_hfn << SN_BITS;
        Ok(())
    }
    // TS 25.322 11.6.5: the Receiver moves VR(R) to SN_MRW_LENGTH, discards what lies below
    // it and answers with MRW_ACK. 11.6.6.1: an obsolete request is acknowledged with N = 0 and
    // SN_ACK = VR(R).
    fn move_receiving_window(&mut self, last: u16, n_length: u8) -> Result<(), AmError> {
        let target = match self.receive_position(last) {
            Ok(position) if position > self.rx_next => Some(position),
            Ok(_) => None,
            Err(AmError::ErroneousSequence) => None,
            Err(error) => return Err(error),
        };
        let n = if let Some(position) = target {
            let mut cursor = self.rx_next;
            while cursor < position {
                let index = (cursor % self.rx.len() as u64) as usize;
                if self.rx[index].sequence == Some(cursor) {
                    self.rx[index].sequence = None;
                    self.rx[index].retained_sequence = None;
                }
                cursor += 1;
            }
            self.partial_length = 0;
            self.partial_first = None;
            self.partial_first_received = None;
            self.partial_air_start = None;
            self.partial_provenance = None;
            self.previous_one_short_delimited = false;
            self.rx_next = position;
            self.rx_deliver = self.rx_deliver.max(position);
            self.rx_high = self.rx_high.max(position);
            n_length
        } else {
            0
        };
        self.pending_mrw_ack = Some((n, sn(self.rx_next)));
        self.status_pending = true;
        Ok(())
    }
    pub fn receive_position(&self, sequence: u16) -> Result<u64, AmError> {
        if sequence > SN_MASK {
            return Err(AmError::Header);
        }
        let offset = distance(sn(self.rx_next), sequence);
        if offset >= self.config.receive_window.get() {
            return Err(AmError::ErroneousSequence);
        }
        self.rx_next
            .checked_add(u64::from(offset))
            .filter(|position| *position != u64::MAX)
            .ok_or(AmError::CountExhausted)
    }
    pub fn enqueue(&mut self, bytes: &[u8]) -> Result<(), AmError> {
        if self.failed {
            return Err(AmError::State);
        }
        self.queue.enqueue(bytes)
    }
    pub fn enqueue_tracked(&mut self, bytes: &[u8]) -> Result<SduReceipt, AmError> {
        if self.failed {
            return Err(AmError::State);
        }
        let slot = self
            .tracking
            .iter()
            .position(Option::is_none)
            .ok_or(AmError::Capacity)?;
        let next = self
            .next_receipt
            .checked_add(1)
            .ok_or(AmError::CountExhausted)?;
        self.queue.enqueue_tracked(bytes, Some(slot))?;
        let serial = self.next_receipt;
        self.tracking[slot] = Some(Tracking {
            serial,
            state: SduTransmission::Queued,
        });
        self.next_receipt = next;
        Ok(SduReceipt {
            owner: self.receipt_owner,
            serial,
            slot,
        })
    }
    pub fn transmission(&self, receipt: SduReceipt) -> Result<SduTransmission, AmError> {
        if receipt.owner != self.receipt_owner {
            return Err(AmError::State);
        }
        let tracked = self
            .tracking
            .get(receipt.slot)
            .and_then(Option::as_ref)
            .filter(|tracked| tracked.serial == receipt.serial)
            .ok_or(AmError::State)?;
        Ok(
            if self.failed && !matches!(tracked.state, SduTransmission::Acknowledged) {
                SduTransmission::Discarded
            } else {
                tracked.state
            },
        )
    }
    pub fn release_receipt(&mut self, receipt: SduReceipt) -> Result<(), AmError> {
        match self.transmission(receipt)? {
            SduTransmission::Acknowledged | SduTransmission::Discarded => {
                self.tracking[receipt.slot] = None;
                Ok(())
            }
            _ => Err(AmError::State),
        }
    }
    fn time(&self, now: ChipInstant) -> Result<(), AmError> {
        if now.epoch != self.now.epoch || now.index < self.now.index {
            Err(AmError::Time)
        } else {
            Ok(())
        }
    }
    fn deadline(
        now: ChipInstant,
        duration: Option<std::num::NonZeroU64>,
    ) -> Result<Option<u64>, AmError> {
        duration
            .map(|duration| now.index.checked_add(duration.get()).ok_or(AmError::Time))
            .transpose()
    }
    pub fn tick(&mut self, now: ChipInstant) -> Result<(), AmError> {
        self.time(now)?;
        if self
            .poll_deadline
            .is_some_and(|deadline| now.index >= deadline)
        {
            self.poll_requested = true;
            self.poll_deadline = None;
        }
        self.now = now;
        Ok(())
    }
    fn highest_tx_hfn(&self) -> u32 {
        self.highest_tx_count >> SN_BITS
    }
    fn begin_reset(&mut self, now: ChipInstant, trigger: ResetTrigger) {
        if self.local_reset.is_none() && !self.failed {
            self.control_history.reset_started = Some(ResetStart {
                at: now,
                trigger,
                tx_base: self.tx_base,
                tx_next: self.tx_next,
                rx_next: self.rx_next,
                rx_high: self.rx_high,
            });
            self.local_reset = Some(LocalReset {
                initialization: ResetCounters::default(),
                message: Reset {
                    acknowledge: false,
                    sequence: self.next_reset_sequence,
                    hfn: self.highest_tx_hfn(),
                },
                attempts: 0,
                deadline: None,
            });
            self.poll_deadline = None;
            self.poll_requested = false;
        }
    }
    pub fn initiate_reset(&mut self, now: ChipInstant) -> Result<(), AmError> {
        self.time(now)?;
        if self.failed {
            return Err(AmError::State);
        }
        self.begin_reset(now, ResetTrigger::Explicit);
        self.now = now;
        Ok(())
    }
    fn reset_plan(&self, peer_hfn: u32, increment: bool) -> Result<ResetTransition, AmError> {
        self.receive_generation
            .checked_add(1)
            .ok_or(AmError::CountExhausted)?;
        let advance = u32::from(increment);
        let initialization = self
            .local_reset
            .map(|local| local.initialization)
            .unwrap_or_default();
        let tx_hfn = initialization.transmit_hfn.map_or_else(
            || {
                self.highest_tx_hfn()
                    .checked_add(advance)
                    .filter(|&hfn| hfn <= MAX_HFN)
                    .ok_or(AmError::CountExhausted)
            },
            Ok,
        )?;
        let rx_hfn = initialization.receive_hfn.map_or_else(
            || {
                peer_hfn
                    .checked_add(advance)
                    .filter(|&hfn| hfn <= MAX_HFN)
                    .ok_or(AmError::CountExhausted)
            },
            Ok,
        )?;
        let tx_position = reset_position(self.tx_next, self.tx_count.position, tx_hfn)?;
        let rx_position = reset_position(self.rx_high, self.rx_count.position, rx_hfn)?;
        Ok(ResetTransition {
            tx_hfn,
            rx_hfn,
            tx_position,
            rx_position,
        })
    }
    fn reset_data(
        &mut self,
        peer_hfn: u32,
        increment: bool,
        counters: Option<ResetCounters>,
    ) -> Result<(u32, u32), AmError> {
        let receive_generation = self
            .receive_generation
            .checked_add(1)
            .ok_or(AmError::CountExhausted)?;
        let ResetTransition {
            tx_hfn: synchronized_tx_hfn,
            rx_hfn: synchronized_rx_hfn,
            tx_position,
            rx_position,
        } = self.reset_plan(peer_hfn, increment)?;
        let counters = counters.unwrap_or_default();
        let tx_hfn = counters.transmit_hfn.unwrap_or(synchronized_tx_hfn);
        let rx_hfn = counters.receive_hfn.unwrap_or(synchronized_rx_hfn);
        // A crossed RESET cannot discard cipher initialization belonging to an already transmitted local RESET.
        if let Some(local) = &mut self.local_reset
            && local.attempts != 0
        {
            local.initialization.transmit_hfn =
                counters.transmit_hfn.or(local.initialization.transmit_hfn);
            local.initialization.receive_hfn =
                counters.receive_hfn.or(local.initialization.receive_hfn);
        }
        self.tx_count = CounterBasis {
            position: tx_position,
            count: tx_hfn << SN_BITS,
        };
        self.rx_count = CounterBasis {
            position: rx_position,
            count: rx_hfn << SN_BITS,
        };
        self.highest_tx_count = tx_hfn << SN_BITS;
        self.tx_base = tx_position;
        self.tx_next = tx_position;
        self.rx_next = rx_position;
        self.rx_high = self.rx_next;
        self.rx_deliver = self.rx_next;
        for slot in &mut self.tx {
            slot.sequence = None;
            slot.retransmit = false;
        }
        self.retransmit_count = 0;
        for slot in &mut self.rx {
            slot.sequence = None;
            slot.retained_sequence = None;
        }
        self.tx_window = self.config.transmit_window.get();
        self.queue.reset_transmitted();
        self.receive_generation = receive_generation;
        self.published_status = None;
        self.status_confirmation = None;
        self.acknowledged_status = None;
        for tracked in self.tracking.iter_mut().flatten() {
            if matches!(tracked.state, SduTransmission::InFlight { .. }) {
                tracked.state = SduTransmission::Discarded;
            }
        }
        self.partial_length = 0;
        self.partial_first = None;
        self.previous_one_short_delimited = false;
        self.partial_first_received = None;
        self.partial_air_start = None;
        self.partial_provenance = None;
        self.poll_counter = 0;
        self.poll_requested = false;
        self.poll_deadline = None;
        self.poll_target = None;
        self.poll_prohibit = None;
        self.status_pending = false;
        self.status_cursor = None;
        self.status_prohibit = None;
        self.counters.resets = self.counters.resets.saturating_add(1);
        Ok((tx_hfn, rx_hfn))
    }
    pub fn receive(&mut self, now: ChipInstant, input: &[u8]) -> Result<AmReceive, AmError> {
        let basis = self.rx_count;
        match self.receive_prepared(now, input, |request| {
            if let ReceivePreparation::Data { position, .. } = request {
                basis.at(position)?;
            }
            Ok::<_, AmError>(input)
        }) {
            Ok(result) => Ok(result),
            Err(ReceiveError::Rlc(error)) => Err(error),
            Err(ReceiveError::Preparation(error)) => Err(error),
        }
    }

    /// Preparation returns plaintext with the original clear header before any LI or piggybacked STATUS parsing.
    pub fn receive_prepared<'a, E>(
        &mut self,
        now: ChipInstant,
        input: &[u8],
        prepare: impl FnOnce(ReceivePreparation) -> Result<&'a [u8], E>,
    ) -> Result<AmReceive, ReceiveError<E>> {
        self.receive_prepared_with_reset(now, input, |request| {
            prepare(request).map(|plaintext| PreparedReceive {
                plaintext,
                reset_counters: None,
            })
        })
    }

    /// TS 25.331 8.1.12.5 may initialize a pending cipher's HFN at the admitted reset without changing transport order.
    pub fn receive_prepared_with_reset<'a, E>(
        &mut self,
        now: ChipInstant,
        input: &[u8],
        prepare: impl FnOnce(ReceivePreparation) -> Result<PreparedReceive<'a>, E>,
    ) -> Result<AmReceive, ReceiveError<E>> {
        self.receive_prepared_timed(super::ReceiveTime::simultaneous(now), input, prepare)
    }

    pub fn receive_prepared_timed<'a, E>(
        &mut self,
        time: super::ReceiveTime,
        input: &[u8],
        prepare: impl FnOnce(ReceivePreparation) -> Result<PreparedReceive<'a>, E>,
    ) -> Result<AmReceive, ReceiveError<E>> {
        let now = time.serviced();
        let fail = ReceiveError::Rlc;
        self.time(now).map_err(fail)?;
        if self.failed {
            return Err(fail(AmError::State));
        }
        if input.len() != self.config.receive_pdu_bytes.get() {
            return Err(fail(AmError::Length));
        }
        let preparation = if input[0] & (super::DATA_FLAG >> u8::BITS) as u8 != 0 {
            if self.local_reset.is_some() {
                return Ok(AmReceive::Ignored);
            }
            let Some(header) = self
                .admit_wire(parse_amd_header(input, self.config.accept_special_he))
                .map_err(fail)?
            else {
                return Ok(AmReceive::Ignored);
            };
            let offset = distance(sn(self.rx_next), header.sequence);
            if offset >= self.config.receive_window.get() {
                self.now = now;
                if header.poll {
                    self.status_pending = true
                }
                self.counters.outside_window = self.counters.outside_window.saturating_add(1);
                return Ok(AmReceive::OutsideWindow);
            }
            ReceivePreparation::Data {
                sequence: header.sequence,
                position: self.receive_position(header.sequence).map_err(fail)?,
            }
        } else if input[0] >> CONTROL_HEADER_BITS == 0 {
            ReceivePreparation::Control
        } else {
            let Some(message) = self.admit_wire(parse_reset(input)).map_err(fail)? else {
                return Ok(AmReceive::Ignored);
            };
            let reset = if message.acknowledge {
                self.local_reset.is_some_and(|local| {
                    local.attempts != 0 && local.message.sequence == message.sequence
                })
            } else {
                self.last_peer_reset
                    .is_none_or(|previous| previous.sequence != message.sequence)
            };
            let transition = reset
                .then(|| {
                    self.reset_plan(
                        message.hfn,
                        message.acknowledge || self.local_reset.is_none(),
                    )
                })
                .transpose()
                .map_err(|error| {
                    self.now = now;
                    self.failed = true;
                    fail(error)
                })?;
            ReceivePreparation::Reset {
                message,
                transition,
            }
        };
        let PreparedReceive {
            plaintext: plain,
            reset_counters,
        } = prepare(preparation).map_err(ReceiveError::Preparation)?;
        if let Some(counters) = reset_counters {
            if !matches!(
                preparation,
                ReceivePreparation::Reset {
                    transition: Some(_),
                    ..
                }
            ) {
                return Err(fail(AmError::State));
            }
            if counters.transmit_hfn.is_some_and(|hfn| hfn > MAX_HFN)
                || counters.receive_hfn.is_some_and(|hfn| hfn > MAX_HFN)
            {
                return Err(fail(AmError::CountExhausted));
            }
        }
        if plain.len() != input.len() {
            return Err(fail(AmError::Length));
        }
        let clear = match preparation {
            ReceivePreparation::Data { .. } => super::AMD_HEADER_BYTES,
            ReceivePreparation::Control | ReceivePreparation::Reset { .. } => input.len(),
        };
        if plain[..clear] != input[..clear] {
            return Err(fail(AmError::Header));
        }
        self.receive_plain(time, plain, reset_counters)
            .map_err(fail)
    }

    // TS 25.322 10.3 discards invalid wire values, not internal state or preparation errors.
    fn admit_wire<T>(&mut self, parsed: Result<T, AmError>) -> Result<Option<T>, AmError> {
        match parsed {
            Ok(value) => Ok(Some(value)),
            Err(AmError::Header | AmError::Length) => {
                self.counters.malformed_pdus = self.counters.malformed_pdus.saturating_add(1);
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn receive_plain(
        &mut self,
        time: super::ReceiveTime,
        input: &[u8],
        reset_counters: Option<ResetCounters>,
    ) -> Result<AmReceive, AmError> {
        let now = time.serviced();
        if input[0] & (super::DATA_FLAG >> 8) as u8 != 0 {
            let Some(pdu) = self.admit_wire(parse_amd(
                input,
                LiWidth::for_fixed_size(input.len()),
                self.config.accept_special_he,
            ))?
            else {
                return Ok(AmReceive::Ignored);
            };
            let offset = distance(sn(self.rx_next), pdu.sequence);
            if let Some(bytes) = pdu.piggybacked_status {
                let Some(status) = self.admit_wire(parse_status(bytes))? else {
                    return Ok(AmReceive::Ignored);
                };
                self.control_history.last_received =
                    Some(ControlEvidence::new(now, PublicationKind::Status, bytes));
                if self.apply_status(status, now)? {
                    return Ok(AmReceive::ResetStarted);
                }
            }
            self.now = now;
            if pdu.poll {
                self.status_pending = true;
            }
            let absolute = self.rx_next + u64::from(offset);
            let index = (absolute % self.rx.len() as u64) as usize;
            if self.rx[index].sequence == Some(absolute) {
                self.counters.duplicate = self.counters.duplicate.saturating_add(1);
                return Ok(AmReceive::Duplicate);
            }
            if self.rx[index].sequence.is_some() {
                self.rx_high = self.rx_high.max(absolute + 1);
                if self.config.missing_pdu_indicator {
                    self.status_pending = true;
                }
                return Err(AmError::Capacity);
            }
            self.rx[index].bytes.copy_from_slice(input);
            self.rx[index].sequence = Some(absolute);
            self.rx[index].retained_sequence = Some(absolute);
            self.rx[index].received_at = time.received();
            self.rx[index].provenance = time.provenance();
            self.rx[index].air_start = time.air_start();
            self.rx_high = self.rx_high.max(absolute + 1);
            if offset != 0 && self.config.missing_pdu_indicator {
                self.status_pending = true;
            }
            for _ in 0..self.rx.len() {
                let index = (self.rx_next % self.rx.len() as u64) as usize;
                if self.rx[index].sequence != Some(self.rx_next) {
                    break;
                }
                self.rx_next += 1;
            }
            self.counters.received = self.counters.received.saturating_add(1);
            return Ok(AmReceive::Stored);
        }
        if input[0] >> CONTROL_HEADER_BITS == 0 {
            if self.local_reset.is_some() {
                return Ok(AmReceive::Ignored);
            }
            let Some(status) = self.admit_wire(parse_status(input))? else {
                return Ok(AmReceive::Ignored);
            };
            self.control_history.last_received =
                Some(ControlEvidence::new(now, PublicationKind::Status, input));
            let reset = self.apply_status(status, now)?;
            self.now = now;
            return Ok(if reset {
                AmReceive::ResetStarted
            } else {
                AmReceive::StatusApplied
            });
        }
        let Some(reset) = self.admit_wire(parse_reset(input))? else {
            return Ok(AmReceive::Ignored);
        };
        let previous_received = self.control_history.last_received;
        self.control_history.last_received = Some(ControlEvidence::new(
            now,
            if reset.acknowledge {
                PublicationKind::ResetAck
            } else {
                PublicationKind::Reset
            },
            input,
        ));
        self.now = now;
        if reset.acknowledge {
            self.control_history.reset_acks_received =
                self.control_history.reset_acks_received.saturating_add(1);
            let Some(local) = self.local_reset else {
                return Ok(AmReceive::Ignored);
            };
            if local.attempts == 0 || local.message.sequence != reset.sequence {
                return Ok(AmReceive::Ignored);
            }
            let (tx_hfn, rx_hfn) = self
                .reset_data(reset.hfn, true, reset_counters)
                .inspect_err(|_| self.failed = true)?;
            self.local_reset = None;
            Ok(AmReceive::ResetAcknowledged { tx_hfn, rx_hfn })
        } else {
            if let Some(previous) = self
                .last_peer_reset
                .filter(|previous| previous.sequence == reset.sequence)
            {
                self.peer_reset_ack = Some(previous);
                return Ok(AmReceive::Ignored);
            }
            let acknowledge = Reset {
                acknowledge: true,
                sequence: reset.sequence,
                hfn: self.highest_tx_hfn(),
            };
            let evidence = PeerResetEvidence {
                at: now,
                sequence: reset.sequence,
                hfn: reset.hfn,
                tx_base: self.tx_base,
                tx_next: self.tx_next,
                rx_next: self.rx_next,
                rx_high: self.rx_high,
                rx_deliver: self.rx_deliver,
                status_pending: self.status_pending,
                status_prohibit: self.status_prohibit,
                counters: self.counters,
                previous_received,
                previous_published: self.control_history.last_published,
            };
            let (tx_hfn, rx_hfn) = self
                .reset_data(reset.hfn, self.local_reset.is_none(), reset_counters)
                .inspect_err(|_| self.failed = true)?;
            self.control_history.peer_reset = Some(evidence);
            self.last_peer_reset = Some(acknowledge);
            self.peer_reset_ack = Some(acknowledge);
            Ok(AmReceive::ResetApplied { tx_hfn, rx_hfn })
        }
    }

    fn apply_status(&mut self, status: Status, now: ChipInstant) -> Result<bool, AmError> {
        // TS 25.322 9.2.2.11.9: a POLL SUFI asks for a status report.
        if status.poll.is_some() {
            self.status_pending = true;
        }
        // TS 25.322 11.6.6.3: an MRW_ACK without an ongoing explicit discard is discarded.
        if let Some((last, n_length)) = status.move_window {
            self.move_receiving_window(last, n_length)?;
        }
        let outstanding = self.tx_next - self.tx_base;
        let ack_offset = status
            .acknowledge
            .map(|ack| u64::from(distance(sn(self.tx_base), ack)));
        let invalid_negative = status
            .negative
            .iter()
            .find(|sequence| u64::from(distance(sn(self.tx_base), *sequence)) >= outstanding);
        if ack_offset.is_some_and(|offset| offset > outstanding) || invalid_negative.is_some() {
            self.begin_reset(
                now,
                ResetTrigger::InvalidStatus {
                    acknowledge: status.acknowledge,
                    negative: invalid_negative,
                },
            );
            return Ok(true);
        }
        let mut advance = ack_offset;
        for absolute in self.tx_base..self.tx_next {
            let index = (absolute % self.tx.len() as u64) as usize;
            let slot = &mut self.tx[index];
            if slot.sequence != Some(absolute) {
                return Err(AmError::State);
            }
            if status.negative.contains(sn(absolute)) {
                self.retransmit_count += usize::from(!slot.retransmit);
                slot.retransmit = true;
                slot.acknowledged = false;
                advance = advance.map(|end| end.min(absolute - self.tx_base));
            } else if status.positive.contains(sn(absolute))
                || ack_offset.is_some_and(|end| absolute - self.tx_base < end)
            {
                slot.acknowledged = true;
                self.retransmit_count -= usize::from(slot.retransmit);
                slot.retransmit = false;
            }
        }
        self.observe_status_confirmation();
        if let Some(advance) = advance {
            let capacity = self.tx.len() as u64;
            for absolute in self.tx_base..self.tx_base + advance {
                self.tx[(absolute % capacity) as usize].sequence = None;
            }
            self.tx_base += advance;
        }
        if let Some(window) = status.window {
            self.tx_window = window.min(self.config.transmit_window.get());
        }
        self.confirm_sdus();
        if let Some(target) = self.poll_target {
            let answered = status.negative.contains(sn(target))
                || (self.tx_base..=target).all(|absolute| {
                    let slot = &self.tx[(absolute % self.tx.len() as u64) as usize];
                    slot.sequence == Some(absolute) && slot.acknowledged
                });
            if answered {
                self.poll_target = None;
                self.poll_deadline = None;
                self.poll_requested = false;
            }
        }
        Ok(false)
    }

    fn confirm_sdus(&mut self) {
        for tracked in self.tracking.iter_mut().flatten() {
            let SduTransmission::InFlight {
                first,
                last,
                delimited: true,
            } = tracked.state
            else {
                continue;
            };
            if (first.max(self.tx_base)..=last).all(|absolute| {
                let slot = &self.tx[(absolute % self.tx.len() as u64) as usize];
                slot.sequence == Some(absolute) && slot.acknowledged
            }) {
                tracked.state = SduTransmission::Acknowledged;
            }
        }
    }

    fn status_packet(&mut self) -> Result<u64, AmError> {
        self.scratch.fill(0);
        let mut writer = Writer::new(&mut self.scratch);
        let mut cursor = self.status_cursor.unwrap_or(self.rx_next).max(self.rx_next);
        while cursor < self.rx_high {
            let slot = &self.rx[(cursor % self.rx.len() as u64) as usize];
            if slot.sequence != Some(cursor) {
                if !writer.can_missing() {
                    break;
                }
                let first = cursor;
                cursor += 1;
                while cursor < self.rx_high
                    && cursor - first < u64::from(MAX_MISSING_RUN)
                    && self.rx[(cursor % self.rx.len() as u64) as usize].sequence != Some(cursor)
                {
                    cursor += 1;
                }
                writer.missing_run(sn(first), (cursor - first) as u8)?;
            } else {
                cursor += 1;
            }
        }
        if let Some((n, sequence)) = self.pending_mrw_ack
            && writer.mrw_ack(n, sequence).is_ok()
        {
            self.pending_mrw_ack = None;
        }
        writer.ack(sn(self.rx_next))?;
        Ok(cursor)
    }

    pub fn publish_next<T, E>(
        &mut self,
        now: ChipInstant,
        publish: impl FnOnce(Publication<'_>) -> Result<T, E>,
    ) -> Result<Option<(PublicationKind, T)>, PublishError<E>> {
        let basis = self.tx_count;
        self.publish_inner(now, move |position| basis.at(position), publish)
            .map_err(|error| match error {
                MappedError::Rlc(error) | MappedError::Mapping(error) => PublishError::Rlc(error),
                MappedError::Downstream(error) => PublishError::Downstream(error),
            })
    }

    /// The COUNT mapper runs before publication and must retain old mappings for retransmissions.
    pub fn publish_next_mapped<T, E>(
        &mut self,
        now: ChipInstant,
        map_count: impl FnMut(u64) -> Result<u32, E>,
        publish: impl FnOnce(Publication<'_>) -> Result<T, E>,
    ) -> Result<Option<(PublicationKind, T)>, PublishError<E>> {
        self.publish_inner(now, map_count, publish)
            .map_err(|error| match error {
                MappedError::Rlc(error) => PublishError::Rlc(error),
                MappedError::Mapping(error) | MappedError::Downstream(error) => {
                    PublishError::Downstream(error)
                }
            })
    }

    fn publish_inner<T, M, E>(
        &mut self,
        now: ChipInstant,
        mut map_count: impl FnMut(u64) -> Result<u32, M>,
        publish: impl FnOnce(Publication<'_>) -> Result<T, E>,
    ) -> Result<Option<(PublicationKind, T)>, MappedError<M, E>> {
        self.tick(now)?;
        if self.failed {
            return Err(AmError::State.into());
        }
        if let Some(message) = self.peer_reset_ack {
            write_reset(&mut self.scratch, message)?;
            let kind = PublicationKind::ResetAck;
            let result = publish(Publication {
                bytes: &self.scratch,
                kind,
                position: None,
                status_coverage: None,
            })
            .map_err(|error| {
                self.counters.publication_failures =
                    self.counters.publication_failures.saturating_add(1);
                MappedError::Downstream(error)
            })?;
            self.peer_reset_ack = None;
            self.control_history.last_published =
                Some(ControlEvidence::new(now, kind, &self.scratch));
            self.control_history.reset_acks_published =
                self.control_history.reset_acks_published.saturating_add(1);
            return Ok(Some((kind, result)));
        }
        if let Some(mut reset) = self.local_reset {
            if reset.deadline.is_some_and(|deadline| now.index < deadline) {
                return Ok(None);
            }
            if reset.attempts + 1 == self.config.maximum_reset_attempts.get() {
                self.failed = true;
                self.local_reset = None;
                return Err(AmError::ResetLimit.into());
            }
            let deadline = Self::deadline(now, Some(self.config.reset_timeout_chips))?;
            if reset.attempts == 0 {
                // TS 25.322 11.4.2.1 samples HFNI at first submission, not at a deferred local trigger.
                reset.message.hfn = self.highest_tx_hfn();
            }
            write_reset(&mut self.scratch, reset.message)?;
            let kind = PublicationKind::Reset;
            let result = publish(Publication {
                bytes: &self.scratch,
                kind,
                position: None,
                status_coverage: None,
            })
            .map_err(|error| {
                self.counters.publication_failures =
                    self.counters.publication_failures.saturating_add(1);
                MappedError::Downstream(error)
            })?;
            if reset.attempts == 0 {
                self.next_reset_sequence = !self.next_reset_sequence;
            }
            reset.attempts += 1;
            reset.deadline = deadline;
            self.local_reset = Some(reset);
            self.control_history.last_published =
                Some(ControlEvidence::new(now, kind, &self.scratch));
            self.control_history.resets_published =
                self.control_history.resets_published.saturating_add(1);
            return Ok(Some((kind, result)));
        }
        // TS 25.322 9.2.2.11.2 forbids LSN below VR(R), so admission defers STATUS instead of lowering ACK.
        if self.status_ready(now) {
            let cursor = self.status_packet()?;
            let complete = cursor == self.rx_high;
            let prohibit = if complete {
                Self::deadline(now, self.config.status_prohibit_chips)?
            } else {
                None
            };
            let kind = PublicationKind::Status;
            let coverage = StatusCoverage {
                owner: self.receipt_owner,
                generation: self.receive_generation,
                next: self.rx_next,
            };
            let result = publish(Publication {
                bytes: &self.scratch,
                kind,
                position: None,
                status_coverage: Some(coverage),
            })
            .map_err(|error| {
                self.counters.publication_failures =
                    self.counters.publication_failures.saturating_add(1);
                MappedError::Downstream(error)
            })?;
            self.status_pending = !complete;
            self.published_status = Some(coverage);
            self.status_cursor = if complete { None } else { Some(cursor) };
            self.status_prohibit = prohibit;
            self.counters.statuses = self.counters.statuses.saturating_add(1);
            if let Some(history) = self.status_history.as_mut() {
                history.record(StatusPublication {
                    at: now,
                    generation: self.receive_generation,
                    acknowledge_next: self.rx_next,
                    receive_high: self.rx_high,
                    delivered_next: self.rx_deliver,
                    received_pdus: self.counters.received,
                    complete,
                });
            }
            self.control_history.last_published =
                Some(ControlEvidence::new(now, kind, &self.scratch));
            return Ok(Some((kind, result)));
        }
        let tx_limit = self
            .tx_base
            .checked_add(u64::from(self.tx_window))
            .ok_or(AmError::CountExhausted)?;
        let poll_allowed = self
            .poll_prohibit
            .is_none_or(|deadline| now.index >= deadline);
        let Some((absolute, new)) = self.data_choice(now)? else {
            return Ok(None);
        };
        if new && self.confirmation_prohibited(now) {
            return Ok(None);
        }
        let index = (absolute % self.tx.len() as u64) as usize;
        let attempts = if new { 0 } else { self.tx[index].attempts };
        if attempts + 1 == self.config.maximum_data_attempts.get() {
            self.begin_reset(
                now,
                ResetTrigger::DataAttempts {
                    sequence: sn(absolute),
                    attempts,
                },
            );
            return Ok(None);
        }
        absolute.checked_add(1).ok_or(AmError::CountExhausted)?;
        let count = map_count(absolute).map_err(MappedError::Mapping)?;
        if count & u32::from(SN_MASK) != u32::from(sn(absolute)) {
            return Err(AmError::Header.into());
        }
        let prepared = if new {
            Some(self.queue.prepare()?)
        } else {
            None
        };
        let last = prepared
            .as_ref()
            .is_some_and(|prepared| self.queue.last(prepared))
            || (new && absolute + 1 == tx_limit);
        let last_retransmit = !new
            && (self.retransmit_count == usize::from(self.tx[index].retransmit)
                || !scan::forward(&self.tx, absolute + 1..self.tx_next).any(|(next, slot)| {
                    (next < tx_limit || next + 1 == self.tx_next)
                        && slot.retransmit
                        && !slot.acknowledged
                }));
        let trigger = self.poll_requested
            || self
                .config
                .polling
                .every_pdus
                .is_some_and(|every| self.poll_counter + 1 >= every.get())
            || (last && self.config.polling.last_transmission)
            || (last_retransmit && self.config.polling.last_retransmission);
        let poll = trigger && poll_allowed;
        let deadline = if poll {
            Self::deadline(now, self.config.polling.timeout_chips)?
        } else {
            self.poll_deadline
        };
        let prohibit = if poll {
            Self::deadline(now, self.config.polling.prohibit_chips)?
        } else {
            self.poll_prohibit
        };
        if new {
            self.scratch.copy_from_slice(&self.queue.packet);
            let indicators = u16::from(self.scratch[1]) & HE_MASK == HE_LI;
            data_header(&mut self.scratch, sn(absolute), poll, indicators);
        } else {
            self.scratch.copy_from_slice(&self.tx[index].bytes);
            self.scratch[1] =
                (self.scratch[1] & !(POLL_FLAG as u8)) | if poll { POLL_FLAG as u8 } else { 0 };
        }
        let kind = PublicationKind::Data {
            sequence: sn(absolute),
            count,
            retransmission: !new,
        };
        let confirmation = self.piggyback_status_confirmation(absolute, new)?;
        let confirmation_prohibit = if confirmation.is_some() && new {
            Self::deadline(now, self.config.status_prohibit_chips)?
        } else {
            self.status_prohibit
        };
        let result = publish(Publication {
            bytes: &self.scratch,
            kind,
            position: Some(absolute),
            status_coverage: confirmation,
        })
        .map_err(|error| {
            self.counters.publication_failures =
                self.counters.publication_failures.saturating_add(1);
            MappedError::Downstream(error)
        })?;
        if new && let Some(coverage) = confirmation {
            self.record_status_confirmation(absolute, coverage);
            self.status_prohibit = confirmation_prohibit;
        }
        if let Some(prepared) = prepared {
            self.highest_tx_count = count;
            for contribution in prepared.contributions.iter().flatten() {
                let tracked = self.tracking[contribution.slot]
                    .as_mut()
                    .expect("retained SDU receipt");
                let first = match tracked.state {
                    SduTransmission::Queued => absolute,
                    SduTransmission::InFlight { first, .. } => first,
                    _ => unreachable!("unsettled SDU receipt"),
                };
                tracked.state = SduTransmission::InFlight {
                    first,
                    last: absolute,
                    delimited: contribution.delimited,
                };
            }
            self.queue.commit(prepared);
            self.tx_next += 1;
            self.tx[index].sequence = Some(absolute);
            self.tx[index].attempts = 0;
            self.tx[index].acknowledged = false;
            self.tx[index].bytes.copy_from_slice(&self.scratch);
        } else {
            self.counters.retransmitted = self.counters.retransmitted.saturating_add(1);
        }
        self.tx[index].attempts += 1;
        self.retransmit_count -= usize::from(self.tx[index].retransmit);
        self.tx[index].retransmit = false;
        self.poll_counter = if self
            .config
            .polling
            .every_pdus
            .is_some_and(|every| self.poll_counter + 1 >= every.get())
        {
            0
        } else {
            self.poll_counter.saturating_add(1)
        };
        self.poll_requested = trigger && !poll;
        self.poll_deadline = deadline;
        self.poll_prohibit = prohibit;
        if poll {
            self.poll_target = Some(self.tx_next - 1);
        }
        self.counters.transmitted = self.counters.transmitted.saturating_add(1);
        Ok(Some((kind, result)))
    }

    // One buffered PDU is reassembled per call. A rejected upper queue leaves the PDU and partial SDU untouched.
    pub fn deliver_with<T, E>(
        &mut self,
        publish: impl FnOnce(Delivery<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, PublishError<E>> {
        if self.state() != AmState::Ready || self.rx_deliver >= self.rx_next {
            return Ok(None);
        }
        let index = (self.rx_deliver % self.rx.len() as u64) as usize;
        if self.rx[index].sequence != Some(self.rx_deliver) {
            return Err(AmError::State.into());
        }
        let pdu = parse_amd(
            &self.rx[index].bytes,
            LiWidth::for_fixed_size(self.config.receive_pdu_bytes.get()),
            self.config.accept_special_he,
        )?;
        let mut partial_length = self.partial_length;
        let mut partial_first = self.partial_first;
        let pdu_received = self.rx[index].received_at;
        let pdu_provenance = self.rx[index].provenance;
        let pdu_air_start = self.rx[index].air_start;
        let mut air_start = if self.partial_first.is_some() {
            match (self.partial_air_start, pdu_air_start) {
                (Some(a), Some(b)) => Some(if a.index < b.index { a } else { b }),
                _ => None,
            }
        } else {
            pdu_air_start
        };
        let mut provenance = self
            .partial_provenance
            .map_or(pdu_provenance, |partial| partial.combine(pdu_provenance));
        let mut first_received = self.partial_first_received.map_or(pdu_received, |old| {
            if old.index < pdu_received.index {
                old
            } else {
                pdu_received
            }
        });
        self.partial_work[..partial_length].copy_from_slice(&self.partial[..partial_length]);
        let mut cursor = 0;
        let mut emitted = 0;
        let mut count = 0;
        let mut repeated_one_short = false;
        for &indicator in pdu.indicators() {
            match indicator {
                Indicator::PreviousExact | Indicator::PreviousOneShort => {
                    // An explicit LI already delivered the preceding one-byte-padded SDU.
                    if indicator == Indicator::PreviousOneShort
                        && partial_length == 0
                        && partial_first.is_none()
                        && self.previous_one_short_delimited
                    {
                        repeated_one_short = true;
                        continue;
                    }
                    if indicator == Indicator::PreviousOneShort {
                        partial_length = partial_length.checked_sub(1).ok_or(AmError::Header)?;
                    }
                    if partial_length > self.limits.maximum_sdu_bytes.get() {
                        return Err(AmError::Capacity.into());
                    }
                    emit(
                        &self.partial_work,
                        partial_length,
                        &mut self.output,
                        &mut emitted,
                        &mut self.ends,
                        &mut count,
                    )?;
                    self.coverage[count - 1] = ReceiveCoverage {
                        owner: self.receipt_owner,
                        generation: self.receive_generation,
                        first: partial_first.take().ok_or(AmError::Header)?,
                        last: self.rx_deliver,
                        first_received,
                        provenance,
                        air_start,
                    };
                    first_received = pdu_received;
                    provenance = pdu_provenance;
                    air_start = pdu_air_start;
                    partial_length = 0;
                }
                Indicator::End(end) => {
                    let end = usize::from(end);
                    if end > cursor && partial_first.is_none() {
                        partial_first = Some(self.rx_deliver);
                    }
                    append(
                        &mut self.partial_work,
                        &mut partial_length,
                        &pdu.data[cursor..end],
                    )?;
                    if partial_length > self.limits.maximum_sdu_bytes.get() {
                        return Err(AmError::Capacity.into());
                    }
                    emit(
                        &self.partial_work,
                        partial_length,
                        &mut self.output,
                        &mut emitted,
                        &mut self.ends,
                        &mut count,
                    )?;
                    self.coverage[count - 1] = ReceiveCoverage {
                        owner: self.receipt_owner,
                        generation: self.receive_generation,
                        first: partial_first.take().ok_or(AmError::Header)?,
                        last: self.rx_deliver,
                        first_received,
                        provenance,
                        air_start,
                    };
                    first_received = pdu_received;
                    provenance = pdu_provenance;
                    air_start = pdu_air_start;
                    partial_length = 0;
                    cursor = end;
                }
                Indicator::Padding | Indicator::Status => {}
            }
        }
        if cursor < pdu.data.len() && partial_first.is_none() {
            partial_first = Some(self.rx_deliver);
        }
        append(
            &mut self.partial_work,
            &mut partial_length,
            &pdu.data[cursor..],
        )?;
        if pdu.complete {
            if partial_length > self.limits.maximum_sdu_bytes.get() {
                return Err(AmError::Capacity.into());
            }
            emit(
                &self.partial_work,
                partial_length,
                &mut self.output,
                &mut emitted,
                &mut self.ends,
                &mut count,
            )?;
            self.coverage[count - 1] = ReceiveCoverage {
                owner: self.receipt_owner,
                generation: self.receive_generation,
                first: partial_first.take().ok_or(AmError::Header)?,
                last: self.rx_deliver,
                first_received,
                provenance,
                air_start,
            };
            partial_length = 0;
        }
        let result = publish(Delivery {
            bytes: &self.output[..emitted],
            ends: &self.ends[..count],
            coverage: &self.coverage[..count],
        })
        .map_err(PublishError::Downstream)?;
        std::mem::swap(&mut self.partial, &mut self.partial_work);
        self.partial_length = partial_length;
        self.partial_first = partial_first;
        self.previous_one_short_delimited = pdu.one_byte_padding;
        self.partial_first_received = partial_first.map(|_| first_received);
        self.partial_air_start = partial_first.and(air_start);
        self.partial_provenance = partial_first.map(|_| provenance);
        self.rx[index].sequence = None;
        self.rx_deliver += 1;
        self.counters.delivered_sdus = self.counters.delivered_sdus.saturating_add(count as u64);
        self.counters.repeated_one_short_markers = self
            .counters
            .repeated_one_short_markers
            .saturating_add(u64::from(repeated_one_short));
        Ok(Some(result))
    }
}

fn append(output: &mut [u8], length: &mut usize, bytes: &[u8]) -> Result<(), AmError> {
    let end = length.checked_add(bytes.len()).ok_or(AmError::Capacity)?;
    output
        .get_mut(*length..end)
        .ok_or(AmError::Capacity)?
        .copy_from_slice(bytes);
    *length = end;
    Ok(())
}
fn emit(
    partial: &[u8],
    length: usize,
    output: &mut [u8],
    emitted: &mut usize,
    ends: &mut [usize],
    count: &mut usize,
) -> Result<(), AmError> {
    if length == 0 {
        return Err(AmError::Header);
    }
    append(output, emitted, &partial[..length])?;
    *ends.get_mut(*count).ok_or(AmError::Capacity)? = *emitted;
    *count += 1;
    Ok(())
}
