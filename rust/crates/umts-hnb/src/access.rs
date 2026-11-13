// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;
use umts_dsp::{
    indicators::{ACCESS_SLOT_CHIPS, Acquisition, AcquisitionIndicators},
    prach::{PREAMBLE_CHIPS, PrachCode, PreambleSignature},
};
use umts_rrc::random_access::{AichTiming, PrachSettings, Signatures, Subchannels};
use umts_types::{RadioEpoch, time::ChipInstant};

pub const MAX_ACCESS_RESPONSES: usize = 64;
const SUBCHANNEL_COUNT: u64 = 12;

#[derive(Clone, Copy, Debug)]
pub struct AccessObservation {
    pub code: PrachCode,
    pub signature: PreambleSignature,
    pub nominal_start: ChipInstant,
    pub received_start: ChipInstant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessWindow {
    pub indicator_start: ChipInstant,
    pub message_start: ChipInstant,
    pub duplicate: bool,
}

// Access scheduling uses chip coordinates, independent of TX and RX sampling rates.
pub trait AccessTxTarget {
    fn information(&self) -> &crate::cell::CellInformation;
    fn next_chip(&self) -> ChipInstant;
    fn access_profile(&self, channel: usize) -> Option<&umts_rrc::EncodedInformation>;
    fn access_timing(&self, channel: usize) -> Option<AccessTiming>;
    fn publish_acquisition(
        &mut self,
        channel: usize,
        observation: AccessObservation,
        indicator: Acquisition,
        now: ChipInstant,
        minimum_tx_lead_chips: u64,
    ) -> Result<AcquisitionPublication, crate::common_tx::CommonTxError>;
}

#[must_use]
pub struct AcquisitionPublication {
    pub window: AccessWindow,
    pub transport: crate::tx_evidence::TransportSubmission,
    pub(crate) cancellation: crate::common_tx::AcquisitionCancellation,
}

impl AcquisitionPublication {
    #[cfg(test)]
    pub fn cancel_pending(&mut self) -> crate::common_tx::PlanStage {
        self.cancellation.cancel()
    }
    pub fn viable(&self) -> bool {
        self.cancellation.available() && self.transport.viable()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessScheduleError {
    Epoch,
    Code,
    Signature,
    Occasion,
    Arrival,
    IncompletePreamble,
    Late,
    Capacity,
    Conflict,
    NoIndicator,
}

struct PendingIndicators {
    start: ChipInstant,
    values: [Acquisition; 16],
}

#[derive(Clone, Copy)]
pub struct AccessTiming {
    epoch: RadioEpoch,
    code: PrachCode,
    signatures: Signatures,
    subchannels: Subchannels,
    timing: AichTiming,
    maximum_arrival_delay: u64,
}

pub struct AccessCalendar {
    profile: AccessTiming,
    pending: Vec<Option<PendingIndicators>>,
    expired: u64,
    consumed_before: u64,
}

#[cfg(test)]
mod tests;

impl AccessCalendar {
    pub fn new(
        epoch: RadioEpoch,
        code: PrachCode,
        settings: &PrachSettings,
        capacity: NonZeroUsize,
        maximum_arrival_delay: u64,
    ) -> Result<Self, AccessScheduleError> {
        if capacity.get() > MAX_ACCESS_RESPONSES {
            return Err(AccessScheduleError::Capacity);
        }
        if maximum_arrival_delay >= ACCESS_SLOT_CHIPS as u64 {
            return Err(AccessScheduleError::Arrival);
        }
        Ok(Self {
            profile: AccessTiming {
                epoch,
                code,
                signatures: settings.signatures,
                subchannels: settings.subchannels,
                timing: settings.aich.timing,
                maximum_arrival_delay,
            },
            pending: (0..capacity.get()).map(|_| None).collect(),
            expired: 0,
            consumed_before: 0,
        })
    }

    #[cfg(test)]
    pub fn expired(&self) -> u64 {
        self.expired
    }

    pub fn profile(&self) -> AccessTiming {
        self.profile
    }
    pub fn pending(&self) -> usize {
        self.pending.iter().filter(|entry| entry.is_some()).count()
    }

    // Positive indicators require the caller to reserve the corresponding message receive resources first.
    pub fn schedule(
        &mut self,
        observation: AccessObservation,
        indicator: Acquisition,
        now: ChipInstant,
        first_mutable: ChipInstant,
        minimum_tx_lead_chips: u64,
    ) -> Result<AccessWindow, AccessScheduleError> {
        let window = self.profile.preview(
            observation,
            indicator,
            now,
            first_mutable,
            minimum_tx_lead_chips,
        )?;
        let response = window.indicator_start.index;
        if response < self.consumed_before {
            return Err(AccessScheduleError::Late);
        }
        let signature = usize::from(observation.signature.number());
        let existing = self.pending.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|entry| entry.start.index == response)
        });
        let mut duplicate = false;
        if let Some(index) = existing {
            let values = &mut self.pending[index]
                .as_mut()
                .expect("occupied response")
                .values;
            if values[signature] == indicator {
                duplicate = true;
            } else if values[signature] != Acquisition::None {
                return Err(AccessScheduleError::Conflict);
            } else {
                values[signature] = indicator;
            }
        } else {
            let index = self
                .pending
                .iter()
                .position(Option::is_none)
                .ok_or(AccessScheduleError::Capacity)?;
            let mut values = [Acquisition::None; 16];
            values[signature] = indicator;
            self.pending[index] = Some(PendingIndicators {
                start: ChipInstant {
                    epoch: self.profile.epoch,
                    index: response,
                },
                values,
            });
        }
        Ok(AccessWindow {
            duplicate,
            ..window
        })
    }

    pub fn take(
        &mut self,
        start: ChipInstant,
    ) -> Result<Option<AcquisitionIndicators>, AccessScheduleError> {
        if start.epoch != self.profile.epoch {
            return Err(AccessScheduleError::Epoch);
        }
        if !start.index.is_multiple_of(ACCESS_SLOT_CHIPS as u64) {
            return Err(AccessScheduleError::Occasion);
        }
        if start.index < self.consumed_before {
            return Err(AccessScheduleError::Late);
        }
        let next = start
            .index
            .checked_add(ACCESS_SLOT_CHIPS as u64)
            .ok_or(AccessScheduleError::Occasion)?;
        let mut result = None;
        for entry in &mut self.pending {
            if entry
                .as_ref()
                .is_some_and(|entry| entry.start.index < start.index)
            {
                *entry = None;
                self.expired = self.expired.saturating_add(1);
            } else if entry.as_ref().is_some_and(|entry| entry.start == start) {
                let entry = entry.take().expect("matching response");
                let mask = (0..16)
                    .filter(|number| self.profile.signatures.contains(*number))
                    .fold(0, |mask, number| mask | (1 << number));
                let mut indicators =
                    AcquisitionIndicators::new(mask).expect("validated signature set");
                for (signature, value) in entry.values.into_iter().enumerate() {
                    if value != Acquisition::None {
                        indicators
                            .set(signature as u8, value)
                            .expect("configured response signature");
                    }
                }
                result = Some(indicators);
            }
        }
        self.consumed_before = next;
        Ok(result)
    }

    pub fn cancel(
        &mut self,
        start: ChipInstant,
        signature: PreambleSignature,
    ) -> Result<bool, AccessScheduleError> {
        if start.epoch != self.profile.epoch {
            return Err(AccessScheduleError::Epoch);
        }
        if start.index < self.consumed_before {
            return Err(AccessScheduleError::Late);
        }
        let Some(entry) = self
            .pending
            .iter_mut()
            .find(|entry| entry.as_ref().is_some_and(|pending| pending.start == start))
        else {
            return Ok(false);
        };
        let values = &mut entry.as_mut().expect("matching access response").values;
        let number = usize::from(signature.number());
        let removed = values[number] != Acquisition::None;
        values[number] = Acquisition::None;
        if values.iter().all(|value| *value == Acquisition::None) {
            *entry = None;
        }
        Ok(removed)
    }
}

impl AccessTiming {
    pub fn maximum_arrival_delay(self) -> u64 {
        self.maximum_arrival_delay
    }
    pub fn indicator_delay(self) -> u64 {
        u64::from(self.timing.preamble_to_indicator_chips())
    }
    pub fn code(self) -> PrachCode {
        self.code
    }
    pub fn signature_mask(self) -> u16 {
        (0..16)
            .filter(|number| self.signatures.contains(*number))
            .fold(0, |mask, number| mask | (1 << number))
    }

    pub fn next_preamble(self, first: ChipInstant) -> Result<ChipInstant, AccessScheduleError> {
        if first.epoch != self.epoch {
            return Err(AccessScheduleError::Epoch);
        }
        let earliest = first
            .index
            .checked_add(self.indicator_delay())
            .ok_or(AccessScheduleError::Occasion)?;
        let slot = earliest.div_ceil(ACCESS_SLOT_CHIPS as u64);
        for offset in 0..SUBCHANNEL_COUNT {
            let candidate = slot
                .checked_add(offset)
                .ok_or(AccessScheduleError::Occasion)?;
            if self
                .subchannels
                .contains((candidate % SUBCHANNEL_COUNT) as u8)
            {
                let response = candidate
                    .checked_mul(ACCESS_SLOT_CHIPS as u64)
                    .ok_or(AccessScheduleError::Occasion)?;
                return Ok(ChipInstant {
                    epoch: self.epoch,
                    index: response - self.indicator_delay(),
                });
            }
        }
        Err(AccessScheduleError::Occasion)
    }

    pub fn preview(
        self,
        observation: AccessObservation,
        indicator: Acquisition,
        now: ChipInstant,
        first_mutable: ChipInstant,
        minimum_tx_lead_chips: u64,
    ) -> Result<AccessWindow, AccessScheduleError> {
        if [
            observation.nominal_start.epoch,
            observation.received_start.epoch,
            now.epoch,
            first_mutable.epoch,
        ]
        .iter()
        .any(|epoch| *epoch != self.epoch)
        {
            return Err(AccessScheduleError::Epoch);
        }
        if observation.code != self.code {
            return Err(AccessScheduleError::Code);
        }
        if !self.signatures.contains(observation.signature.number()) {
            return Err(AccessScheduleError::Signature);
        }
        if indicator == Acquisition::None {
            return Err(AccessScheduleError::NoIndicator);
        }
        let nominal = observation.nominal_start.index;
        let arrival = observation
            .received_start
            .index
            .checked_sub(nominal)
            .filter(|delay| *delay <= self.maximum_arrival_delay)
            .ok_or(AccessScheduleError::Arrival)?;
        let response = nominal
            .checked_add(u64::from(self.timing.preamble_to_indicator_chips()))
            .ok_or(AccessScheduleError::Occasion)?;
        // TS 25.211 7.3 and TS 25.214 6.1.1/Table 7 bind the subchannel to the corresponding DL access slot.
        if !response.is_multiple_of(ACCESS_SLOT_CHIPS as u64)
            || !self
                .subchannels
                .contains(((response / ACCESS_SLOT_CHIPS as u64) % SUBCHANNEL_COUNT) as u8)
        {
            return Err(AccessScheduleError::Occasion);
        }
        let preamble_end = nominal
            .checked_add(arrival)
            .and_then(|start| start.checked_add(PREAMBLE_CHIPS as u64))
            .ok_or(AccessScheduleError::Arrival)?;
        if now.index < preamble_end {
            return Err(AccessScheduleError::IncompletePreamble);
        }
        let earliest = now
            .index
            .checked_add(minimum_tx_lead_chips)
            .ok_or(AccessScheduleError::Late)?
            .max(first_mutable.index);
        if response < earliest {
            return Err(AccessScheduleError::Late);
        }
        let message = nominal
            .checked_add(u64::from(self.timing.preamble_to_message_chips()))
            .ok_or(AccessScheduleError::Occasion)?;
        Ok(AccessWindow {
            indicator_start: ChipInstant {
                epoch: self.epoch,
                index: response,
            },
            message_start: ChipInstant {
                epoch: self.epoch,
                index: message,
            },
            duplicate: false,
        })
    }
}
