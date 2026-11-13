// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;

use umts_types::{RadioEpoch, UeKey};

struct Entry<T> {
    generation: NonZeroU64,
    exhausted: bool,
    value: Option<T>,
}

pub struct UeRegistry<T> {
    epoch: RadioEpoch,
    entries: Vec<Entry<T>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegistryInsertError<E> {
    Capacity,
    Construction(E),
}

impl<T> UeRegistry<T> {
    pub fn epoch(&self) -> RadioEpoch {
        self.epoch
    }
    pub fn capacity(&self) -> u32 {
        self.entries.len() as u32
    }

    pub fn at_slot(&self, slot: u32) -> Option<(UeKey, &T)> {
        let entry = self.entries.get(slot as usize)?;
        Some((
            UeKey {
                epoch: self.epoch,
                slot,
                generation: entry.generation,
            },
            entry.value.as_ref()?,
        ))
    }

    // Build all resources before the final publication. After publication succeeds the constructor must not fail.
    pub fn try_insert_with<E>(
        &mut self,
        construct: impl FnOnce(UeKey) -> Result<T, E>,
    ) -> Result<UeKey, RegistryInsertError<E>> {
        let (slot, entry) = self
            .entries
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| entry.value.is_none() && !entry.exhausted)
            .ok_or(RegistryInsertError::Capacity)?;
        let key = UeKey {
            epoch: self.epoch,
            slot: slot as u32,
            generation: entry.generation,
        };
        match construct(key) {
            Ok(value) => {
                entry.value = Some(value);
                Ok(key)
            }
            Err(error) => {
                match entry.generation.checked_add(1) {
                    Some(generation) => entry.generation = generation,
                    None => entry.exhausted = true,
                }
                Err(RegistryInsertError::Construction(error))
            }
        }
    }
    pub fn new(epoch: RadioEpoch, capacity: u32) -> Self {
        Self {
            epoch,
            entries: (0..capacity)
                .map(|_| Entry {
                    generation: NonZeroU64::MIN,
                    exhausted: false,
                    value: None,
                })
                .collect(),
        }
    }

    #[cfg(test)]
    fn insert(&mut self, value: T) -> Result<UeKey, T> {
        let Some((slot, entry)) = self
            .entries
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| entry.value.is_none() && !entry.exhausted)
        else {
            return Err(value);
        };
        entry.value = Some(value);
        Ok(UeKey {
            epoch: self.epoch,
            slot: slot as u32,
            generation: entry.generation,
        })
    }

    pub fn get(&self, key: UeKey) -> Option<&T> {
        if key.epoch != self.epoch {
            return None;
        }
        let entry = self.entries.get(key.slot as usize)?;
        (entry.generation == key.generation)
            .then_some(entry.value.as_ref())
            .flatten()
    }

    pub fn get_mut(&mut self, key: UeKey) -> Option<&mut T> {
        if key.epoch != self.epoch {
            return None;
        }
        let entry = self.entries.get_mut(key.slot as usize)?;
        (entry.generation == key.generation)
            .then_some(entry.value.as_mut())
            .flatten()
    }

    pub fn remove(&mut self, key: UeKey) -> Option<T> {
        self.get(key)?;
        let entry = &mut self.entries[key.slot as usize];
        let value = entry.value.take();
        match entry.generation.checked_add(1) {
            Some(next) => entry.generation = next,
            None => entry.exhausted = true,
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(n: u64) -> RadioEpoch {
        RadioEpoch::new(NonZeroU64::new(n).unwrap())
    }

    #[test]
    fn recycled_slot_cannot_accept_old_work_or_another_cell() {
        let mut registry = UeRegistry::new(epoch(1), 1);
        let old = registry.insert("first").unwrap();
        assert_eq!(registry.remove(old), Some("first"));
        let new = registry.insert("second").unwrap();
        assert_eq!(old.slot, new.slot);
        assert_ne!(old.generation, new.generation);
        assert_eq!(registry.get(old), None);
        assert_eq!(registry.remove(old), None);
        assert_eq!(
            registry.get(UeKey {
                epoch: epoch(2),
                ..new
            }),
            None
        );
        assert_eq!(registry.get(new), Some(&"second"));
    }

    #[test]
    fn exhausted_generation_is_not_reused() {
        let mut registry = UeRegistry::new(epoch(1), 1);
        registry.entries[0].generation = NonZeroU64::new(u64::MAX).unwrap();
        let last = registry.insert(1).unwrap();
        assert_eq!(registry.remove(last), Some(1));
        assert_eq!(registry.insert(2), Err(2));
    }

    #[test]
    fn fallible_constructor_gets_reserved_identity_and_retires_failed_generation() {
        let mut registry = UeRegistry::new(epoch(1), 1);
        let mut failed = None;
        assert_eq!(
            registry.try_insert_with(|key| {
                failed = Some(key);
                Err::<UeKey, _>("full")
            }),
            Err(RegistryInsertError::Construction("full"))
        );
        let failed = failed.unwrap();
        assert!(registry.at_slot(0).is_none());
        let key = registry.try_insert_with(Ok::<_, ()>).unwrap();
        assert_eq!(registry.get(key), Some(&key));
        assert_eq!(key.slot, failed.slot);
        assert_ne!(key.generation, failed.generation);
        assert_eq!(registry.get(failed), None);
        assert_eq!(registry.at_slot(0), Some((key, &key)));
        assert_eq!(registry.capacity(), 1);
        assert_eq!(
            registry.try_insert_with(|_| -> Result<UeKey, ()> {
                panic!("full registry must not construct")
            }),
            Err(RegistryInsertError::Capacity)
        );
    }
}
