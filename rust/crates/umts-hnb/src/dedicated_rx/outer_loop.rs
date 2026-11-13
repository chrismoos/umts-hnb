// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{PilotSirTarget, PowerControlError, ReceiveIdentity};
use umts_types::{UeKey, time::ChipInstant};

const ERROR_STEP_DB: f64 = 0.25;
const MAXIMUM_TARGET_DB: f64 = 45.0;
const DCH_TRANSPORTS: usize = 32;
const EDCH_STREAM: usize = DCH_TRANSPORTS;
const CRC_STREAMS: usize = DCH_TRANSPORTS + 1;
const LINK_CONFIGURATIONS: usize = super::shard::CONFIGURATIONS_PER_UE;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OuterLoopSettings {
    floor: f64,
    initial: f64,
    ceiling: f64,
    target_bler: f64,
}

impl OuterLoopSettings {
    pub fn new(
        floor: f64,
        initial: f64,
        ceiling: f64,
        target_bler: f64,
    ) -> Result<Self, PowerControlError> {
        if ![floor, initial, ceiling, target_bler]
            .into_iter()
            .all(f64::is_finite)
            || floor < 0.0
            || floor > initial
            || initial > ceiling
            || ceiling > MAXIMUM_TARGET_DB
            || !(0.01..=0.5).contains(&target_bler)
        {
            return Err(PowerControlError::Configuration);
        }
        Ok(Self {
            floor,
            initial,
            ceiling,
            target_bler,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrcStream {
    Dch(std::num::NonZeroU8),
    Edch,
}

impl CrcStream {
    fn index(self) -> Option<usize> {
        match self {
            Self::Dch(id) if usize::from(id.get()) <= DCH_TRANSPORTS => {
                Some(usize::from(id.get()) - 1)
            }
            Self::Dch(_) => None,
            Self::Edch => Some(EDCH_STREAM),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrcEligibility {
    AirLink,
    LocalLoss,
    Retransmission,
    NoTransmission,
    Unchecked,
    UncertainControl,
}

#[derive(Clone, Copy, Debug)]
pub struct LinkCrcObservation {
    pub identity: ReceiveIdentity,
    pub start: ChipInstant,
    pub stream: CrcStream,
    pub crc_counts: umts_dsp::transport_blocks::CrcCounts,
    pub eligibility: CrcEligibility,
}

impl LinkCrcObservation {
    pub fn from_decode(observation: super::UplinkDecodeObservation) -> Self {
        use umts_dsp::transport_blocks::BlockOutcome;
        match observation {
            super::UplinkDecodeObservation::Dch(observation) => {
                let transport = observation.transport;
                let eligibility = match observation.outcome {
                    BlockOutcome::Erased => CrcEligibility::LocalLoss,
                    BlockOutcome::Absent => CrcEligibility::NoTransmission,
                    BlockOutcome::Decoded | BlockOutcome::InvalidCrc { .. }
                        if transport.crc_checked && transport.blocks != 0 =>
                    {
                        CrcEligibility::AirLink
                    }
                    _ => CrcEligibility::Unchecked,
                };
                Self {
                    identity: transport.identity,
                    start: transport.start,
                    stream: CrcStream::Dch(
                        std::num::NonZeroU8::new(transport.transport.get())
                            .expect("validated transport identity"),
                    ),
                    crc_counts: observation.crc_counts,
                    eligibility,
                }
            }
            super::UplinkDecodeObservation::Edch {
                identity,
                start,
                control,
                feedback,
                outcome,
                presence_log_likelihood,
            } => Self {
                identity,
                start,
                stream: CrcStream::Edch,
                crc_counts: umts_dsp::transport_blocks::CrcCounts {
                    passed: usize::from(outcome == BlockOutcome::Decoded),
                    failed: usize::from(matches!(outcome, BlockOutcome::InvalidCrc { .. })),
                },
                eligibility: if control.information.sequence.value() != 0
                    || matches!(feedback, umts_mac::hsupa::Reception::Ack { deliver: false })
                {
                    CrcEligibility::Retransmission
                } else {
                    match outcome {
                        BlockOutcome::Erased => CrcEligibility::LocalLoss,
                        BlockOutcome::Absent => CrcEligibility::NoTransmission,
                        BlockOutcome::InvalidFiller => CrcEligibility::Unchecked,
                        BlockOutcome::Decoded | BlockOutcome::InvalidCrc { .. }
                            if presence_log_likelihood
                                .is_some_and(|value| value.is_finite() && value > 0.0) =>
                        {
                            CrcEligibility::AirLink
                        }
                        _ => CrcEligibility::UncertainControl,
                    }
                },
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OuterLoopCounters {
    pub passed: u64,
    pub failed: u64,
    pub local_loss: u64,
    pub retransmission: u64,
    pub no_transmission: u64,
    pub unchecked: u64,
    pub uncertain_control: u64,
    pub stale: u64,
}

#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct OuterLoopStatus {
    pub ue: UeKey,
    pub target: PilotSirTarget,
    pub counters: OuterLoopCounters,
}

pub struct LinkOuterLoop {
    ue: UeKey,
    settings: OuterLoopSettings,
    target: PilotSirTarget,
    last: [Option<(u64, std::num::NonZeroU64)>; CRC_STREAMS],
    counters: OuterLoopCounters,
}

impl LinkOuterLoop {
    pub fn new(ue: UeKey, settings: OuterLoopSettings) -> Result<Self, PowerControlError> {
        Ok(Self {
            ue,
            settings,
            target: PilotSirTarget::from_db(settings.initial)?,
            last: [None; CRC_STREAMS],
            counters: OuterLoopCounters::default(),
        })
    }

    pub fn status(&self) -> OuterLoopStatus {
        OuterLoopStatus {
            ue: self.ue,
            target: self.target,
            counters: self.counters,
        }
    }

    pub fn observe(&mut self, observation: LinkCrcObservation) -> Result<(), PowerControlError> {
        if observation.identity.ue != self.ue {
            return Err(PowerControlError::Identity);
        }
        if observation.start.epoch != self.ue.epoch {
            return Err(PowerControlError::Epoch);
        }
        let index = observation
            .stream
            .index()
            .ok_or(PowerControlError::Configuration)?;
        if self.last[index].is_some_and(|(at, configuration)| {
            observation.start.index <= at || observation.identity.configuration < configuration
        }) {
            self.counters.stale = self.counters.stale.saturating_add(1);
            return Ok(());
        }
        self.last[index] = Some((observation.start.index, observation.identity.configuration));
        let excluded = match observation.eligibility {
            CrcEligibility::AirLink => None,
            CrcEligibility::LocalLoss => Some(&mut self.counters.local_loss),
            CrcEligibility::Retransmission => Some(&mut self.counters.retransmission),
            CrcEligibility::NoTransmission => Some(&mut self.counters.no_transmission),
            CrcEligibility::Unchecked => Some(&mut self.counters.unchecked),
            CrcEligibility::UncertainControl => Some(&mut self.counters.uncertain_control),
        };
        if let Some(counter) = excluded {
            *counter = counter.saturating_add(1);
            return Ok(());
        }
        self.counters.passed = self
            .counters
            .passed
            .saturating_add(observation.crc_counts.passed as u64);
        self.counters.failed = self
            .counters
            .failed
            .saturating_add(observation.crc_counts.failed as u64);
        let success_step =
            ERROR_STEP_DB * self.settings.target_bler / (1.0 - self.settings.target_bler);
        let delta = ERROR_STEP_DB * observation.crc_counts.failed as f64
            - success_step * observation.crc_counts.passed as f64;
        let target = (self.target.db() + delta).clamp(self.settings.floor, self.settings.ceiling);
        if target != self.target.db() {
            self.target = PilotSirTarget::from_db(target)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ConfigurationWindow {
    identity: ReceiveIdentity,
    start: Option<u64>,
    end: Option<u64>,
}

struct LinkEntry {
    controller: LinkOuterLoop,
    configurations: [Option<ConfigurationWindow>; LINK_CONFIGURATIONS],
}

pub struct LinkPowerRegistry {
    entries: Vec<Option<LinkEntry>>,
}

impl LinkPowerRegistry {
    pub fn storage_bytes(maximum_ues: std::num::NonZeroUsize) -> Option<usize> {
        maximum_ues
            .get()
            .checked_mul(std::mem::size_of::<Option<LinkEntry>>())
    }
    pub fn new(maximum_ues: std::num::NonZeroUsize) -> Result<Self, PowerControlError> {
        if maximum_ues.get() > super::shard::MAX_SHARD_UES {
            return Err(PowerControlError::Configuration);
        }
        Ok(Self {
            entries: (0..maximum_ues.get()).map(|_| None).collect(),
        })
    }

    pub fn attach(
        &mut self,
        identity: ReceiveIdentity,
        settings: OuterLoopSettings,
    ) -> Result<(), PowerControlError> {
        let slot = self
            .entries
            .get_mut(identity.ue.slot as usize)
            .ok_or(PowerControlError::Identity)?;
        let window = ConfigurationWindow {
            identity,
            start: None,
            end: None,
        };
        if let Some(entry) = slot {
            if entry.controller.ue != identity.ue {
                return Err(PowerControlError::Identity);
            }
            if entry.controller.settings != settings {
                return Err(PowerControlError::Configuration);
            }
            if entry
                .configurations
                .iter()
                .flatten()
                .any(|owner| owner.identity == identity)
            {
                return Err(PowerControlError::Configuration);
            }
            let vacant = entry
                .configurations
                .iter_mut()
                .find(|owner| owner.is_none())
                .ok_or(PowerControlError::Configuration)?;
            *vacant = Some(window);
        } else {
            let controller = LinkOuterLoop::new(identity.ue, settings)?;
            let mut configurations = [None; LINK_CONFIGURATIONS];
            configurations[0] = Some(window);
            *slot = Some(LinkEntry {
                controller,
                configurations,
            });
        }
        Ok(())
    }

    fn entry_mut(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<&mut LinkEntry, PowerControlError> {
        self.entries
            .get_mut(identity.ue.slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| entry.controller.ue == identity.ue)
            .ok_or(PowerControlError::Identity)
    }

    pub fn activate(
        &mut self,
        identity: ReceiveIdentity,
        at: ChipInstant,
    ) -> Result<(), PowerControlError> {
        if at.epoch != identity.ue.epoch {
            return Err(PowerControlError::Epoch);
        }
        let entry = self.entry_mut(identity)?;
        if entry
            .configurations
            .iter()
            .flatten()
            .any(|owner| owner.start.is_some() && owner.end.is_none())
        {
            return Err(PowerControlError::Configuration);
        }
        let owner = entry
            .configurations
            .iter_mut()
            .flatten()
            .find(|owner| owner.identity == identity)
            .ok_or(PowerControlError::Identity)?;
        if owner.start.is_some() || owner.end.is_some() {
            return Err(PowerControlError::Configuration);
        }
        owner.start = Some(at.index);
        Ok(())
    }

    pub fn validate_replacement(
        &self,
        previous: ReceiveIdentity,
        next: ReceiveIdentity,
        at: ChipInstant,
    ) -> Result<(), PowerControlError> {
        if previous.ue != next.ue || previous == next {
            return Err(PowerControlError::Identity);
        }
        if at.epoch != previous.ue.epoch {
            return Err(PowerControlError::Epoch);
        }
        let entry = self
            .entries
            .get(previous.ue.slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.controller.ue == previous.ue)
            .ok_or(PowerControlError::Identity)?;
        let old = entry
            .configurations
            .iter()
            .flatten()
            .find(|owner| owner.identity == previous)
            .ok_or(PowerControlError::Identity)?;
        let new = entry
            .configurations
            .iter()
            .flatten()
            .find(|owner| owner.identity == next)
            .ok_or(PowerControlError::Identity)?;
        if old.start.is_none_or(|start| at.index < start)
            || old.end.is_some()
            || new.start.is_some()
            || new.end.is_some()
        {
            return Err(PowerControlError::Configuration);
        }
        Ok(())
    }

    pub fn replace(
        &mut self,
        previous: ReceiveIdentity,
        next: ReceiveIdentity,
        at: ChipInstant,
    ) -> Result<(), PowerControlError> {
        self.validate_replacement(previous, next, at)?;
        let entry = self.entry_mut(previous)?;
        for owner in entry.configurations.iter_mut().flatten() {
            if owner.identity == previous {
                owner.end = Some(at.index);
            }
            if owner.identity == next {
                owner.start = Some(at.index);
            }
        }
        Ok(())
    }

    pub fn retire(&mut self, identity: ReceiveIdentity) -> Result<(), PowerControlError> {
        let entry = self.entry_mut(identity)?;
        let owner = entry
            .configurations
            .iter_mut()
            .find(|owner| owner.is_some_and(|owner| owner.identity == identity))
            .ok_or(PowerControlError::Identity)?;
        *owner = None;
        if entry.configurations.iter().all(Option::is_none) {
            self.entries[identity.ue.slot as usize] = None;
        }
        Ok(())
    }

    pub fn status(&self, identity: ReceiveIdentity) -> Result<OuterLoopStatus, PowerControlError> {
        let entry = self
            .entries
            .get(identity.ue.slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.controller.ue == identity.ue)
            .ok_or(PowerControlError::Identity)?;
        if !entry
            .configurations
            .iter()
            .flatten()
            .any(|owner| owner.identity == identity)
        {
            return Err(PowerControlError::Identity);
        }
        Ok(entry.controller.status())
    }

    pub fn observe(&mut self, observation: LinkCrcObservation) -> Result<(), PowerControlError> {
        if observation.start.epoch != observation.identity.ue.epoch {
            return Err(PowerControlError::Epoch);
        }
        let entry = self.entry_mut(observation.identity)?;
        let owner = entry
            .configurations
            .iter()
            .flatten()
            .find(|owner| owner.identity == observation.identity)
            .ok_or(PowerControlError::Identity)?;
        if owner
            .start
            .is_none_or(|start| observation.start.index < start)
            || owner.end.is_some_and(|end| observation.start.index >= end)
        {
            entry.controller.counters.stale = entry.controller.counters.stale.saturating_add(1);
            return Ok(());
        }
        entry.controller.observe(observation)
    }
}
