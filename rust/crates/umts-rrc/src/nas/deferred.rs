// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CoreNas, ForwardedNas, InitialNasMetadata, InspectedNas, MAX_NAS_BYTES, NasPublishError,
    NasRouter,
    pending::{Error, Hold, PublishError},
};
use std::num::NonZeroUsize;
use umts_types::{CnDomain, UeKey};

struct Entry {
    domain: CnDomain,
    initial: Option<InitialNasMetadata>,
    bytes: Vec<u8>,
    length: usize,
}

#[cfg(test)]
mod tests;

pub struct DeferredNas {
    key: UeKey,
    entries: Vec<Entry>,
    head: usize,
    length: usize,
    retired: bool,
    interworking_null: Option<(usize, [u8; 2])>,
}

impl DeferredNas {
    pub fn new(key: UeKey, capacity: NonZeroUsize, maximum: NonZeroUsize) -> Result<Self, Error> {
        if maximum.get() > MAX_NAS_BYTES {
            return Err(Error::Capacity);
        }
        capacity
            .get()
            .checked_mul(maximum.get())
            .ok_or(Error::Capacity)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity.get())
            .map_err(|_| Error::Capacity)?;
        for _ in 0..capacity.get() {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(maximum.get())
                .map_err(|_| Error::Capacity)?;
            bytes.resize(maximum.get(), 0);
            entries.push(Entry {
                domain: CnDomain::Cs,
                initial: None,
                bytes,
                length: 0,
            });
        }
        Ok(Self {
            key,
            entries,
            head: 0,
            length: 0,
            retired: false,
            interworking_null: None,
        })
    }

    pub fn len(&self) -> usize {
        self.length + usize::from(self.interworking_null.is_some())
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0 && self.interworking_null.is_none()
    }
    pub fn retained_payload_bytes(&self) -> usize {
        self.entries.len() * self.entries[0].bytes.len()
    }

    pub fn hold(&mut self, key: UeKey, message: InspectedNas<'_>) -> Result<Hold, Error> {
        if key != self.key {
            return Err(Error::Origin);
        }
        if self.retired {
            return Err(Error::Retired);
        }
        if message.nas.is_empty() || message.nas.len() > self.entries[0].bytes.len() {
            return Err(Error::Capacity);
        }
        // Only an exact retained Initial Direct Transfer is idempotent. Direct NAS can repeat legitimately.
        if message.initial.is_some()
            && (0..self.length).any(|offset| {
                let entry = &self.entries[(self.head + offset) % self.entries.len()];
                entry.domain == message.domain
                    && entry.initial == message.initial
                    && entry.bytes[..entry.length] == *message.nas
            })
        {
            return Ok(Hold::Duplicate);
        }
        if self.length == self.entries.len() {
            return Err(Error::Capacity);
        }
        let slot = (self.head + self.length) % self.entries.len();
        let entry = &mut self.entries[slot];
        entry.domain = message.domain;
        entry.initial = message.initial;
        entry.bytes[..message.nas.len()].copy_from_slice(message.nas);
        entry.length = message.nas.len();
        self.length += 1;
        Ok(Hold::Stored)
    }

    pub fn hold_interworking_null(&mut self, key: UeKey, nas: &[u8; 2]) -> Result<Hold, Error> {
        if key != self.key {
            return Err(Error::Origin);
        }
        if self.retired {
            return Err(Error::Retired);
        }
        if self.interworking_null.is_some() {
            return Err(Error::Capacity);
        }
        // A full deferred queue must not prevent the identity response that unlocks registration.
        self.interworking_null = Some((self.length, *nas));
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
        if let Some((0, nas)) = self.interworking_null {
            let result = router
                .forward_parts(self.key, CnDomain::Cs, &nas, None, publish)
                .map_err(|error| match error {
                    NasPublishError::Procedure(error) => PublishError::Nas(error),
                    NasPublishError::Downstream(error) => PublishError::Downstream(error),
                })?;
            self.interworking_null = None;
            return Ok(Some(result));
        }
        if self.length == 0 {
            return Ok(None);
        }
        let entry = &mut self.entries[self.head];
        let result = router
            .forward_parts(
                self.key,
                entry.domain,
                &entry.bytes[..entry.length],
                entry.initial.clone(),
                publish,
            )
            .map_err(|error| match error {
                NasPublishError::Procedure(error) => PublishError::Nas(error),
                NasPublishError::Downstream(error) => PublishError::Downstream(error),
            })?;
        entry.bytes[..entry.length].fill(0);
        entry.length = 0;
        entry.initial = None;
        self.head = (self.head + 1) % self.entries.len();
        self.length -= 1;
        if let Some((remaining, _)) = self.interworking_null.as_mut() {
            *remaining -= 1;
        }
        Ok(Some(result))
    }

    pub fn retire(&mut self, key: UeKey) -> Result<(), Error> {
        if key != self.key {
            return Err(Error::Origin);
        }
        for entry in &mut self.entries {
            entry.bytes[..entry.length].fill(0);
            entry.length = 0;
            entry.initial = None;
        }
        self.length = 0;
        self.interworking_null = None;
        self.retired = true;
        Ok(())
    }
}
