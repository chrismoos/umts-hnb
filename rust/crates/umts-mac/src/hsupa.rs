// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_types::{
    UeKey,
    hsupa::{Etfci, Rsn, Tti},
};

pub mod payload;
pub mod reorder;
pub mod scheduling;

const MAX_PROCESSES: usize = 8;
const MAX_MISSED_TRANSMISSIONS: u64 = 3;
const SATURATED_RSN: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarqError {
    Identity,
    Time,
    Pending,
    StaleResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoftBufferAction {
    Reset,
    Combine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reception {
    Ack { deliver: bool },
    Nack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    ue: UeKey,
    tti: u64,
    process: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_retransmission_follows_an_undelivered_attempt_and_saturates() {
        let ue = UeKey {
            epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::new(1).unwrap()),
            slot: 0,
            generation: std::num::NonZeroU64::new(1).unwrap(),
        };
        let mut entity = HarqEntity::new(ue, Tti::TwoMilliseconds);
        let format = Etfci::new(40).unwrap();
        assert_eq!(entity.expected_retransmission(16), None);
        let (attempt, action) = entity.begin(ue, 16, format, Rsn::new(0).unwrap()).unwrap();
        assert_eq!(action, SoftBufferAction::Reset);
        assert_eq!(
            entity.expected_retransmission(24),
            None,
            "pending attempts expect nothing"
        );
        entity.complete(attempt, false).unwrap();
        assert_eq!(
            entity.expected_retransmission(24),
            Some((format, Rsn::new(1).unwrap()))
        );
        assert_eq!(
            entity.expected_retransmission(25),
            None,
            "other processes have no history"
        );
        assert_eq!(
            entity.expected_retransmission(32),
            Some((format, Rsn::new(2).unwrap()))
        );
        assert_eq!(
            entity.expected_retransmission(48),
            Some((format, Rsn::new(3).unwrap()))
        );
        assert_eq!(
            entity.expected_retransmission(56),
            None,
            "too many missed attempts"
        );
        let (attempt, action) = entity.begin(ue, 24, format, Rsn::new(1).unwrap()).unwrap();
        assert_eq!(action, SoftBufferAction::Combine);
        entity.complete(attempt, true).unwrap();
        assert_eq!(
            entity.expected_retransmission(32),
            None,
            "delivered blocks expect nothing"
        );
    }
    use std::num::NonZeroU64;
    use umts_types::RadioEpoch;

    fn ue() -> UeKey {
        UeKey {
            epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
            slot: 0,
            generation: NonZeroU64::new(1).unwrap(),
        }
    }

    #[test]
    fn retries_do_not_deliver_twice_and_old_results_cannot_complete_new_work() {
        for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
            let key = ue();
            let mut entity = HarqEntity::new(key, interval);
            let format = Etfci::new(10).unwrap();
            let stride = u64::from(interval.processes());
            let (first, action) = entity.begin(key, 0, format, Rsn::new(0).unwrap()).unwrap();
            assert_eq!(action, SoftBufferAction::Reset);
            assert_eq!(
                entity.complete(first, true),
                Ok(Reception::Ack { deliver: true })
            );
            let (retry, action) = entity
                .begin(key, stride, format, Rsn::new(1).unwrap())
                .unwrap();
            assert_eq!(action, SoftBufferAction::Combine);
            assert_eq!(entity.complete(first, true), Err(HarqError::StaleResult));
            assert_eq!(
                entity.complete(retry, true),
                Ok(Reception::Ack { deliver: false })
            );
            let (new, action) = entity
                .begin(key, 2 * stride, format, Rsn::new(0).unwrap())
                .unwrap();
            assert_eq!(action, SoftBufferAction::Reset);
            entity.cancel(new).unwrap();
            assert_eq!(entity.complete(new, true), Err(HarqError::StaleResult));
            let (_, action) = entity
                .begin(key, 3 * stride, format, Rsn::new(1).unwrap())
                .unwrap();
            assert_eq!(action, SoftBufferAction::Reset);
        }
    }

    #[test]
    fn incompatible_or_ambiguous_history_resets_without_cross_ue_mutation() {
        let key = ue();
        let mut entity = HarqEntity::new(key, Tti::TwoMilliseconds);
        let format = Etfci::new(10).unwrap();
        let (first, _) = entity
            .begin(key, 1272, format, Rsn::new(0).unwrap())
            .unwrap();
        let other = UeKey { slot: 1, ..key };
        assert_eq!(
            entity.begin(other, 1280, format, Rsn::new(1).unwrap()),
            Err(HarqError::Identity)
        );
        assert_eq!(entity.complete(first, false), Ok(Reception::Nack));
        let (retry, action) = entity
            .begin(key, 1280, format, Rsn::new(1).unwrap())
            .unwrap();
        assert_eq!(action, SoftBufferAction::Combine);
        entity.complete(retry, false).unwrap();
        let (_, action) = entity
            .begin(key, 1320, format, Rsn::new(3).unwrap())
            .unwrap();
        assert_eq!(action, SoftBufferAction::Reset);
    }
}

impl Attempt {
    pub fn tti(self) -> u64 {
        self.tti
    }
    pub fn process(self) -> usize {
        self.process
    }
}

#[derive(Clone, Copy)]
struct Process {
    last_tti: u64,
    format: Etfci,
    rsn: Rsn,
    delivered: bool,
    pending: bool,
}

/// Why an attempt started a fresh soft buffer instead of combining.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetReason {
    FirstTransmission,
    NoHistory,
    FormatMismatch,
    Stale,
    RsnMismatch,
    /// A neighboring process slot holds the matching history, i.e. the TTI number is off by one.
    NeighborSlot,
}

impl ResetReason {
    pub const COUNT: usize = 6;
    pub fn index(self) -> usize {
        self as usize
    }
}

pub struct HarqEntity {
    ue: UeKey,
    interval: Tti,
    latest_tti: Option<u64>,
    processes: [Option<Process>; MAX_PROCESSES],
    last_reset: Option<ResetReason>,
}

impl HarqEntity {
    pub fn new(ue: UeKey, interval: Tti) -> Self {
        Self {
            ue,
            interval,
            latest_tti: None,
            last_reset: None,
            processes: [None; MAX_PROCESSES],
        }
    }

    // Absolute TTI numbers must retain CFN wraps within this UE generation.
    pub fn begin(
        &mut self,
        ue: UeKey,
        tti: u64,
        format: Etfci,
        rsn: Rsn,
    ) -> Result<(Attempt, SoftBufferAction), HarqError> {
        if ue != self.ue {
            return Err(HarqError::Identity);
        }
        if self.latest_tti.is_some_and(|last| tti <= last) {
            return Err(HarqError::Time);
        }
        let count = u64::from(self.interval.processes());
        let index = (tti % count) as usize;
        let previous = self.processes[index];
        if previous.is_some_and(|process| process.pending) {
            return Err(HarqError::Pending);
        }
        let continuation = previous.is_some_and(|process| {
            let distance = (tti - process.last_tti) / count;
            // TS 25.212 4.9.2.2 permits flushing ambiguous RSN/timing histories.
            let expected = u64::from(process.rsn.value())
                .saturating_add(distance)
                .min(u64::from(SATURATED_RSN)) as u8;
            rsn.value() != 0
                && process.format == format
                && distance <= MAX_MISSED_TRANSMISSIONS + 1
                && rsn.value() == expected
        });
        self.last_reset = if continuation {
            None
        } else {
            let strict = match previous {
                _ if rsn.value() == 0 => ResetReason::FirstTransmission,
                None => ResetReason::NoHistory,
                Some(process) if process.format != format => ResetReason::FormatMismatch,
                Some(process)
                    if (tti - process.last_tti) / count > MAX_MISSED_TRANSMISSIONS + 1 =>
                {
                    ResetReason::Stale
                }
                Some(_) => ResetReason::RsnMismatch,
            };
            let neighbor = strict != ResetReason::FirstTransmission
                && [count as usize - 1, 1].iter().any(|offset| {
                    self.processes[(index + offset) % count as usize].is_some_and(|process| {
                        process.format == format && process.rsn.value() + 1 == rsn.value()
                    })
                });
            Some(if neighbor {
                ResetReason::NeighborSlot
            } else {
                strict
            })
        };
        self.processes[index] = Some(Process {
            last_tti: tti,
            format,
            rsn,
            delivered: continuation && previous.is_some_and(|process| process.delivered),
            pending: true,
        });
        self.latest_tti = Some(tti);
        Ok((
            Attempt {
                ue,
                tti,
                process: index,
            },
            if continuation {
                SoftBufferAction::Combine
            } else {
                SoftBufferAction::Reset
            },
        ))
    }

    pub fn last_reset_reason(&self) -> Option<ResetReason> {
        self.last_reset
    }

    /// The expected retransmission format and RSN on `tti`'s process if the previous
    /// attempt is undelivered and recent enough to combine with.
    pub fn expected_retransmission(&self, tti: u64) -> Option<(Etfci, Rsn)> {
        let count = u64::from(self.interval.processes());
        let process = self.processes[(tti % count) as usize]?;
        if process.delivered || process.pending || tti <= process.last_tti {
            return None;
        }
        let distance = (tti - process.last_tti) / count;
        if distance == 0 || distance > MAX_MISSED_TRANSMISSIONS + 1 {
            return None;
        }
        let rsn = u64::from(process.rsn.value())
            .saturating_add(distance)
            .min(u64::from(SATURATED_RSN)) as u8;
        Some((process.format, Rsn::new(rsn).ok()?))
    }

    // Only a CRC-valid complete transport block permits ACK, never E-DPCCH detection alone.
    pub fn complete(&mut self, attempt: Attempt, crc_valid: bool) -> Result<Reception, HarqError> {
        if attempt.ue != self.ue {
            return Err(HarqError::Identity);
        }
        let process = self
            .processes
            .get_mut(attempt.process)
            .and_then(Option::as_mut)
            .filter(|process| process.pending && process.last_tti == attempt.tti)
            .ok_or(HarqError::StaleResult)?;
        process.pending = false;
        if crc_valid {
            let deliver = !process.delivered;
            process.delivered = true;
            Ok(Reception::Ack { deliver })
        } else {
            Ok(Reception::Nack)
        }
    }

    // Canceled decoding invalidates soft-buffer continuity and cannot manufacture feedback.
    pub fn cancel(&mut self, attempt: Attempt) -> Result<(), HarqError> {
        if attempt.ue != self.ue {
            return Err(HarqError::Identity);
        }
        let process = self
            .processes
            .get_mut(attempt.process)
            .ok_or(HarqError::StaleResult)?;
        if !process.is_some_and(|process| process.pending && process.last_tti == attempt.tti) {
            return Err(HarqError::StaleResult);
        }
        *process = None;
        Ok(())
    }
}
