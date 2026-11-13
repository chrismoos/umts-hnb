// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{num::NonZeroU64, sync::Arc};

use crate::{
    Error, Key,
    algorithms::{Uea1Key, Uia1Key},
    negotiation::IntegrityAlgorithm,
    snow_algorithms::{Uea2Key, Uia2Key},
};
use umts_types::{CnDomain, Direction, UeKey};

const CIPHER_BEARER_BITS: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyOrigin {
    pub ue: UeKey,
    pub domain: CnDomain,
    pub generation: NonZeroU64,
}

pub enum CipherMaterial {
    Clear,
    Uea1(Key),
    Uea2(Key),
}

// One allocation per shared key context, including the largest supported schedule.
#[allow(clippy::large_enum_variant)]
enum CipherKey {
    Uea1(Uea1Key),
    Uea2(Uea2Key),
}

#[allow(clippy::large_enum_variant)]
pub enum IntegrityKey {
    Uia1(Uia1Key),
    Uia2(Uia2Key),
}

pub const CIPHER_KEY_BYTES: usize = std::mem::size_of::<CipherKey>();
pub const INTEGRITY_KEY_BYTES: usize = std::mem::size_of::<IntegrityKey>();

impl IntegrityKey {
    pub fn kind(&self) -> IntegrityAlgorithm {
        match self {
            Self::Uia1(_) => IntegrityAlgorithm::Uia1,
            Self::Uia2(_) => IntegrityAlgorithm::Uia2,
        }
    }
}

pub trait IntegrityMac {
    fn authenticate(
        &self,
        count: u32,
        fresh: u32,
        direction: Direction,
        bytes: &[u8],
        bits: usize,
    ) -> Result<[u8; 4], Error>;
}

impl IntegrityMac for Uia1Key {
    fn authenticate(
        &self,
        count: u32,
        fresh: u32,
        direction: Direction,
        bytes: &[u8],
        bits: usize,
    ) -> Result<[u8; 4], Error> {
        self.authenticate(count, fresh, direction, bytes, bits)
    }
}

impl IntegrityMac for Uia2Key {
    fn authenticate(
        &self,
        count: u32,
        fresh: u32,
        direction: Direction,
        bytes: &[u8],
        bits: usize,
    ) -> Result<[u8; 4], Error> {
        self.authenticate(count, fresh, direction, bytes, bits)
    }
}

impl IntegrityKey {
    pub fn authenticate(
        &self,
        count: u32,
        fresh: u32,
        direction: Direction,
        bytes: &[u8],
        bits: usize,
    ) -> Result<[u8; 4], Error> {
        match self {
            Self::Uia1(key) => key.authenticate(count, fresh, direction, bytes, bits),
            Self::Uia2(key) => key.authenticate(count, fresh, direction, bytes, bits),
        }
    }
}

impl IntegrityMac for IntegrityKey {
    fn authenticate(
        &self,
        count: u32,
        fresh: u32,
        direction: Direction,
        bytes: &[u8],
        bits: usize,
    ) -> Result<[u8; 4], Error> {
        self.authenticate(count, fresh, direction, bytes, bits)
    }
}

pub struct KeySet {
    cipher: CipherContext,
    integrity: IntegrityContext,
}

impl KeySet {
    pub fn uia1(origin: KeyOrigin, cipher: CipherMaterial, integrity: Key) -> Self {
        Self::new(origin, cipher, integrity, IntegrityAlgorithm::Uia1)
    }

    pub fn new(
        origin: KeyOrigin,
        cipher: CipherMaterial,
        integrity: Key,
        algorithm: IntegrityAlgorithm,
    ) -> Self {
        let cipher = match cipher {
            CipherMaterial::Clear => None,
            CipherMaterial::Uea1(key) => Some(Arc::new(CipherKey::Uea1(Uea1Key::new(&key)))),
            CipherMaterial::Uea2(key) => Some(Arc::new(CipherKey::Uea2(Uea2Key::new(&key)))),
        };
        Self {
            cipher: CipherContext {
                origin,
                algorithm: cipher,
            },
            integrity: IntegrityContext {
                origin,
                algorithm: Arc::new(match algorithm {
                    IntegrityAlgorithm::Uia1 => IntegrityKey::Uia1(Uia1Key::new(&integrity)),
                    IntegrityAlgorithm::Uia2 => IntegrityKey::Uia2(Uia2Key::new(&integrity)),
                }),
            },
        }
    }

    pub fn split(self) -> (CipherContext, IntegrityContext) {
        (self.cipher, self.integrity)
    }
}

// TS 33.102 6.6 separates CS/PS user keys from the domain selected for shared signaling.
#[derive(Clone)]
pub struct CipherContext {
    origin: KeyOrigin,
    algorithm: Option<Arc<CipherKey>>,
}

impl CipherContext {
    pub fn origin(&self) -> KeyOrigin {
        self.origin
    }

    pub fn is_clear(&self) -> bool {
        self.algorithm.is_none()
    }

    pub fn apply(
        &self,
        origin: KeyOrigin,
        count: u32,
        bearer: u8,
        direction: Direction,
        bytes: &mut [u8],
        bits: usize,
    ) -> Result<(), Error> {
        if origin != self.origin {
            return Err(Error::Origin);
        }
        match self.algorithm.as_deref() {
            None => {
                if bits == 0
                    || bits > crate::algorithms::MAX_F8_BITS
                    || bytes.len() != bits.div_ceil(u8::BITS as usize)
                {
                    return Err(Error::Length);
                }
                if u32::from(bearer) >= 1 << CIPHER_BEARER_BITS {
                    return Err(Error::Bearer);
                }
                Ok(())
            }
            Some(CipherKey::Uea1(key)) => key.apply(count, bearer, direction, bytes, bits),
            Some(CipherKey::Uea2(key)) => key.apply(count, bearer, direction, bytes, bits),
        }
    }
}

#[derive(Clone)]
pub struct IntegrityContext {
    origin: KeyOrigin,
    algorithm: Arc<IntegrityKey>,
}

impl IntegrityContext {
    pub fn origin(&self) -> KeyOrigin {
        self.origin
    }

    pub fn kind(&self) -> IntegrityAlgorithm {
        self.algorithm.kind()
    }

    pub fn algorithm(&self, origin: KeyOrigin) -> Result<&IntegrityKey, Error> {
        if origin != self.origin {
            return Err(Error::Origin);
        }
        Ok(&self.algorithm)
    }
}

impl std::fmt::Debug for CipherContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CipherContext")
            .field("origin", &self.origin)
            .field("clear", &self.is_clear())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for IntegrityContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntegrityContext")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
