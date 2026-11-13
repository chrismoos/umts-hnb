// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CoreNas, ForwardedNas, InitialNasMetadata, InspectedNas, MAX_NAS_BYTES, NasError,
    NasPublishError, NasRouter, index,
};
use std::num::NonZeroUsize;
use umts_types::{CnDomain, UeKey};

const CN_DOMAINS: usize = 2;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Origin,
    Capacity,
    NoInitialMetadata,
    ConflictingInitial,
    SequenceExhausted,
    Retired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    Stored,
    Duplicate,
}
#[derive(Debug, PartialEq, Eq)]
pub enum PublishError<E> {
    Owner(Error),
    Nas(NasError),
    Downstream(E),
}
struct Slot {
    bytes: Vec<u8>,
    length: usize,
    metadata: Option<InitialNasMetadata>,
    sequence: u64,
}
pub struct PendingInitials {
    key: UeKey,
    slots: [Slot; CN_DOMAINS],
    next_sequence: u64,
    retired: bool,
}
impl PendingInitials {
    pub fn new(key: UeKey, maximum: NonZeroUsize) -> Result<Self, Error> {
        if maximum.get() > MAX_NAS_BYTES {
            return Err(Error::Capacity);
        }
        fn slot(maximum: usize) -> Result<Slot, Error> {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(maximum)
                .map_err(|_| Error::Capacity)?;
            bytes.resize(maximum, 0);
            Ok(Slot {
                bytes,
                length: 0,
                metadata: None,
                sequence: 0,
            })
        }
        Ok(Self {
            key,
            slots: [slot(maximum.get())?, slot(maximum.get())?],
            next_sequence: 0,
            retired: false,
        })
    }
    pub fn retained_payload_bytes(&self) -> usize {
        self.slots[0].bytes.len() * CN_DOMAINS
    }
    pub fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.metadata.is_some())
            .count()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn hold(&mut self, key: UeKey, message: InspectedNas<'_>) -> Result<Hold, Error> {
        self.store(key, message, false)
    }
    pub fn replace_initial(
        &mut self,
        key: UeKey,
        message: InspectedNas<'_>,
    ) -> Result<Hold, Error> {
        self.store(key, message, true)
    }
    fn store(
        &mut self,
        key: UeKey,
        message: InspectedNas<'_>,
        replace: bool,
    ) -> Result<Hold, Error> {
        if key != self.key {
            return Err(Error::Origin);
        }
        if self.retired {
            return Err(Error::Retired);
        }
        let metadata = message.initial.ok_or(Error::NoInitialMetadata)?;
        let slot = &mut self.slots[index(message.domain)];
        if message.nas.is_empty() || message.nas.len() > slot.bytes.len() {
            return Err(Error::Capacity);
        }
        if let Some(previous) = &slot.metadata {
            if previous == &metadata && slot.bytes[..slot.length] == *message.nas {
                return Ok(Hold::Duplicate);
            }
            if !replace {
                return Err(Error::ConflictingInitial);
            }
        }
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        slot.bytes[..message.nas.len()].copy_from_slice(message.nas);
        if slot.length > message.nas.len() {
            slot.bytes[message.nas.len()..slot.length].fill(0);
        }
        slot.length = message.nas.len();
        slot.metadata = Some(metadata);
        slot.sequence = self.next_sequence;
        self.next_sequence = next;
        Ok(Hold::Stored)
    }
    pub fn forward_one_with<T, E>(
        &mut self,
        router: &mut NasRouter,
        publish: impl FnOnce(CoreNas<'_>) -> Result<T, E>,
    ) -> Result<Option<(ForwardedNas, T)>, PublishError<E>> {
        if self.retired {
            return Err(PublishError::Owner(Error::Retired));
        }
        let Some(selected) = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.metadata.is_some())
            .min_by_key(|(_, slot)| slot.sequence)
            .map(|(index, _)| index)
        else {
            return Ok(None);
        };
        let domain = match selected {
            0 => CnDomain::Cs,
            _ => CnDomain::Ps,
        };
        let slot = &mut self.slots[selected];
        let result = router
            .forward_parts(
                self.key,
                domain,
                &slot.bytes[..slot.length],
                slot.metadata.clone(),
                publish,
            )
            .map_err(|error| match error {
                NasPublishError::Procedure(error) => PublishError::Nas(error),
                NasPublishError::Downstream(error) => PublishError::Downstream(error),
            })?;
        slot.bytes[..slot.length].fill(0);
        slot.length = 0;
        slot.metadata = None;
        Ok(Some(result))
    }
    pub fn retire(&mut self, key: UeKey) -> Result<(), Error> {
        if key != self.key {
            return Err(Error::Origin);
        }
        self.retired = true;
        for slot in &mut self.slots {
            slot.bytes[..slot.length].fill(0);
            slot.length = 0;
            slot.metadata = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
