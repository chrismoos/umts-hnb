// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_security::{
    contexts::{IntegrityContext, KeyOrigin},
    counters::Start,
};
use umts_types::{CnDomain, UeKey};

use super::{DownlinkIntegrity, Error, MAX_SRB, UplinkIntegrity};

const SRB_COUNT: usize = MAX_SRB as usize + 1;
// TS 25.331 8.1.12.2 limits overlapping integrity configurations to three.
const MAX_INTEGRITY_EPOCHS: usize = 3;
const CN_DOMAINS: usize = 2;

#[derive(Clone, Copy)]
pub struct IntegritySeed {
    pub downlink_first_count: u32,
    pub uplink_start: Start,
    pub uplink_previous_sequence: Option<u8>,
}

struct Bearer {
    downlink: DownlinkIntegrity,
    uplink: UplinkIntegrity,
}

struct Epoch {
    origin: KeyOrigin,
    bearers: [Bearer; SRB_COUNT],
}

pub struct IntegrityEpochs {
    ue: UeKey,
    fresh: u32,
    highest_generation: [u64; CN_DOMAINS],
    epochs: [Option<Epoch>; MAX_INTEGRITY_EPOCHS],
}

impl IntegrityEpochs {
    pub fn new(ue: UeKey, fresh: u32) -> Self {
        Self {
            ue,
            fresh,
            highest_generation: [0; CN_DOMAINS],
            epochs: std::array::from_fn(|_| None),
        }
    }

    pub fn len(&self) -> usize {
        self.epochs.iter().flatten().count()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insertion retains existing epochs and never selects an active security configuration.
    pub fn insert(
        &mut self,
        context: IntegrityContext,
        seeds: [IntegritySeed; SRB_COUNT],
    ) -> Result<(), EpochError> {
        let origin = context.origin();
        if origin.ue != self.ue {
            return Err(EpochError::Owner);
        }
        let domain = domain_index(origin.domain);
        if origin.generation.get() <= self.highest_generation[domain] {
            return Err(EpochError::ReusedGeneration);
        }
        let slot = self
            .epochs
            .iter()
            .position(Option::is_none)
            .ok_or(EpochError::Capacity)?;
        let build = |bearer: u8| -> Result<Bearer, EpochError> {
            let seed = seeds[usize::from(bearer)];
            Ok(Bearer {
                downlink: DownlinkIntegrity::new(
                    context.clone(),
                    bearer,
                    self.fresh,
                    seed.downlink_first_count,
                )
                .map_err(EpochError::Stream)?,
                uplink: UplinkIntegrity::new(
                    context.clone(),
                    bearer,
                    self.fresh,
                    seed.uplink_start,
                    seed.uplink_previous_sequence,
                )
                .map_err(EpochError::Stream)?,
            })
        };
        let bearers = [build(0)?, build(1)?, build(2)?, build(3)?, build(4)?];
        self.epochs[slot] = Some(Epoch { origin, bearers });
        self.highest_generation[domain] = origin.generation.get();
        Ok(())
    }

    pub fn downlink(
        &mut self,
        origin: KeyOrigin,
        bearer: u8,
    ) -> Result<&mut DownlinkIntegrity, EpochError> {
        Ok(&mut self.bearer(origin, bearer)?.downlink)
    }

    pub fn uplink(
        &mut self,
        origin: KeyOrigin,
        bearer: u8,
    ) -> Result<&mut UplinkIntegrity, EpochError> {
        Ok(&mut self.bearer(origin, bearer)?.uplink)
    }

    /// Retire only after the procedure owner has removed all activation and retransmission references.
    pub fn retire(&mut self, origin: KeyOrigin) -> Result<(), EpochError> {
        if origin.ue != self.ue {
            return Err(EpochError::Owner);
        }
        let slot = self
            .epochs
            .iter()
            .position(|epoch| epoch.as_ref().is_some_and(|epoch| epoch.origin == origin))
            .ok_or(EpochError::Unknown)?;
        self.epochs[slot] = None;
        Ok(())
    }

    fn bearer(&mut self, origin: KeyOrigin, bearer: u8) -> Result<&mut Bearer, EpochError> {
        if origin.ue != self.ue {
            return Err(EpochError::Owner);
        }
        if bearer > MAX_SRB {
            return Err(EpochError::Bearer);
        }
        let epoch = self
            .epochs
            .iter_mut()
            .flatten()
            .find(|epoch| epoch.origin == origin)
            .ok_or(EpochError::Unknown)?;
        Ok(&mut epoch.bearers[usize::from(bearer)])
    }
}

fn domain_index(domain: CnDomain) -> usize {
    match domain {
        CnDomain::Cs => 0,
        CnDomain::Ps => 1,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochError {
    Owner,
    ReusedGeneration,
    Capacity,
    Unknown,
    Bearer,
    Stream(Error),
}

#[cfg(test)]
mod tests;
