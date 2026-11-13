// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Error, OvsfCode, RadioLease, RadioResources, leaves};
use std::num::NonZeroU64;
use umts_rrc::hsupa::{DownlinkControl, RelativeGrant};
use umts_types::UeKey;

const GRANT_SF: u16 = 256;
const INDICATOR_SF: u16 = 128;
const SIGNATURES: usize = 40;
const COMBINATIONS: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HsupaCodeSet {
    absolute: OvsfCode,
    indicator: OvsfCode,
}
impl HsupaCodeSet {
    pub fn absolute(self) -> OvsfCode {
        self.absolute
    }
    pub fn indicator(self) -> OvsfCode {
        self.indicator
    }
}

#[derive(Debug)]
#[must_use = "Retain the pool until every user and queued waveform has retired"]
pub struct HsupaCodeLease {
    owner: NonZeroU64,
    serial: NonZeroU64,
    codes: HsupaCodeSet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HsupaPoolIdentity {
    owner: NonZeroU64,
    serial: NonZeroU64,
    codes: HsupaCodeSet,
}
impl HsupaCodeLease {
    pub fn identity(&self) -> HsupaPoolIdentity {
        HsupaPoolIdentity {
            owner: self.owner,
            serial: self.serial,
            codes: self.codes,
        }
    }
    pub fn codes(&self) -> HsupaCodeSet {
        self.codes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct User {
    key: UeKey,
    serial: NonZeroU64,
    acknowledgement: u8,
    relative: Option<u8>,
}

pub(super) struct Pool {
    serial: NonZeroU64,
    users: [Option<User>; SIGNATURES],
    occupied: [bool; SIGNATURES],
}
impl Pool {
    pub(super) fn has_user(&self, key: UeKey) -> bool {
        self.users.iter().flatten().any(|user| user.key == key)
    }
}

#[derive(Debug)]
#[must_use = "Retain the UE signatures and E-RNTI until its queued control transmissions retire"]
pub struct HsupaUserLease {
    owner: NonZeroU64,
    pool: NonZeroU64,
    index: usize,
    user: User,
    codes: HsupaCodeSet,
    combination: Option<u8>,
}
impl HsupaUserLease {
    pub(crate) fn belongs_to(&self, pool: &HsupaCodeLease) -> bool {
        self.owner == pool.owner && self.pool == pool.serial && self.codes == pool.codes
    }
    pub fn key(&self) -> UeKey {
        self.user.key
    }
    pub fn e_rnti(&self) -> u16 {
        self.index as u16 + 1
    }
    pub fn controls(&self) -> DownlinkControl {
        let relative = self
            .user
            .relative
            .zip(self.combination)
            .map(|(signature, combination)| {
                RelativeGrant::new(signature, combination).expect("reserved relative grant")
            });
        DownlinkControl::new(
            self.codes.absolute,
            self.codes.indicator,
            self.user.acknowledgement,
            relative,
            &[],
        )
        .expect("reserved nonoverlapping control codes and signatures")
    }
}

#[derive(Debug)]
#[cfg(test)]
pub struct HsupaReleaseError {
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )
    )]
    pub error: Error,
    pub lease: HsupaCodeLease,
}
#[derive(Debug)]
pub struct HsupaUserReleaseError {
    pub error: Error,
    pub lease: HsupaUserLease,
}

impl RadioResources {
    pub fn reserve_hsupa(&mut self) -> Result<HsupaCodeLease, Error> {
        if self.hsupa.is_some() {
            return Err(Error::Capacity);
        }
        let serial = self.next.ok_or(Error::Capacity)?;
        for index in 0..INDICATOR_SF {
            let indicator = OvsfCode::new(INDICATOR_SF, index).expect("bounded indicator code");
            if self.occupied[leaves(indicator)].iter().any(|used| *used) {
                continue;
            }
            let mut occupied = self.occupied;
            occupied[leaves(indicator)].fill(true);
            let absolute = (0..GRANT_SF)
                .map(|index| OvsfCode::new(GRANT_SF, index).expect("bounded grant code"))
                .find(|code| occupied[leaves(*code)].iter().all(|used| !*used));
            let Some(absolute) = absolute else {
                continue;
            };
            occupied[leaves(absolute)].fill(true);
            self.occupied = occupied;
            self.hsupa = Some(Pool {
                serial,
                users: [None; SIGNATURES],
                occupied: [false; SIGNATURES],
            });
            self.next = serial.checked_add(1);
            return Ok(HsupaCodeLease {
                owner: self.owner,
                serial,
                codes: HsupaCodeSet {
                    absolute,
                    indicator,
                },
            });
        }
        Err(Error::Capacity)
    }

    #[cfg(test)]
    pub fn reserve_hsupa_user(
        &mut self,
        pool: &HsupaCodeLease,
        radio: &RadioLease,
        relative_combination: Option<u8>,
    ) -> Result<HsupaUserLease, Error> {
        self.reserve_hsupa_user_in_pool(pool.identity(), radio, relative_combination)
    }

    pub fn reserve_hsupa_user_in_pool(
        &mut self,
        pool: HsupaPoolIdentity,
        radio: &RadioLease,
        relative_combination: Option<u8>,
    ) -> Result<HsupaUserLease, Error> {
        self.check(radio)?;
        if relative_combination.is_some_and(|combination| combination >= COMBINATIONS) {
            return Err(Error::Configuration);
        }
        let retained = self
            .hsupa
            .as_mut()
            .filter(|retained| pool.owner == self.owner && retained.serial == pool.serial)
            .ok_or(Error::Identity)?;
        if retained.has_user(radio.key) {
            return Err(Error::Capacity);
        }
        let index = retained
            .users
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Capacity)?;
        let mut free = retained
            .occupied
            .iter()
            .enumerate()
            .filter_map(|(index, used)| (!used).then_some(index as u8));
        let acknowledgement = free.next().ok_or(Error::Capacity)?;
        let relative = if relative_combination.is_some() {
            Some(free.next().ok_or(Error::Capacity)?)
        } else {
            None
        };
        let serial = self.next.ok_or(Error::Capacity)?;
        let user = User {
            key: radio.key,
            serial,
            acknowledgement,
            relative,
        };
        retained.users[index] = Some(user);
        retained.occupied[usize::from(acknowledgement)] = true;
        if let Some(relative) = relative {
            retained.occupied[usize::from(relative)] = true;
        }
        self.next = serial.checked_add(1);
        Ok(HsupaUserLease {
            owner: self.owner,
            pool: pool.serial,
            index,
            user,
            codes: pool.codes,
            combination: relative_combination,
        })
    }

    // The controller must retain this lease through receive, control-queue and radio-tail retirement.
    pub fn release_hsupa_user(
        &mut self,
        lease: HsupaUserLease,
    ) -> Result<(), HsupaUserReleaseError> {
        let Some(pool) = self.hsupa.as_mut().filter(|pool| {
            lease.owner == self.owner
                && pool.serial == lease.pool
                && pool.users.get(lease.index) == Some(&Some(lease.user))
        }) else {
            return Err(HsupaUserReleaseError {
                error: Error::Identity,
                lease,
            });
        };
        pool.users[lease.index] = None;
        pool.occupied[usize::from(lease.user.acknowledgement)] = false;
        if let Some(relative) = lease.user.relative {
            pool.occupied[usize::from(relative)] = false;
        }
        Ok(())
    }

    // Empty membership does not replace the caller's accepted/lost waveform drain evidence.
    #[cfg(test)]
    pub fn release_hsupa(&mut self, lease: HsupaCodeLease) -> Result<(), HsupaReleaseError> {
        let Some(pool) = self
            .hsupa
            .as_ref()
            .filter(|pool| lease.owner == self.owner && pool.serial == lease.serial)
        else {
            return Err(HsupaReleaseError {
                error: Error::Identity,
                lease,
            });
        };
        if pool.users.iter().any(Option::is_some) {
            return Err(HsupaReleaseError {
                error: Error::Capacity,
                lease,
            });
        }
        for code in [lease.codes.absolute, lease.codes.indicator] {
            self.occupied[leaves(code)].fill(false);
        }
        self.hsupa = None;
        Ok(())
    }
}
