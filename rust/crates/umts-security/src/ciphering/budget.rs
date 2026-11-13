// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Error, Scope};
use crate::contexts::KeyOrigin;
use umts_types::{CnDomain, UeKey};

const MAX_SIGNALLING_CONFIGURATIONS: usize = 3;
const MAX_USER_CONFIGURATIONS: usize = 2;
const CN_DOMAINS: usize = 2;

#[derive(Clone, Copy)]
struct Configurations<const N: usize> {
    values: [Option<KeyOrigin>; N],
    used: usize,
}

impl<const N: usize> Configurations<N> {
    fn new() -> Self {
        Self {
            values: [None; N],
            used: 0,
        }
    }

    fn admit(&mut self, origin: Option<KeyOrigin>) -> Result<(), Error> {
        if self.values[..self.used].contains(&origin) {
            return Ok(());
        }
        let target = self.values.get_mut(self.used).ok_or(Error::Capacity)?;
        *target = origin;
        self.used += 1;
        Ok(())
    }
}

// TS 25.331 8.1.12.2 bounds configurations over all bearers, not independently per direction.
pub struct CipherBudget {
    ue: UeKey,
    signalling: Configurations<MAX_SIGNALLING_CONFIGURATIONS>,
    users: [Configurations<MAX_USER_CONFIGURATIONS>; CN_DOMAINS],
}

impl CipherBudget {
    pub fn new(ue: UeKey) -> Self {
        Self {
            ue,
            signalling: Configurations::new(),
            users: [Configurations::new(); CN_DOMAINS],
        }
    }

    pub fn admit(&mut self, scope: Scope, origin: Option<KeyOrigin>) -> Result<(), Error> {
        if origin.is_none() {
            return Ok(());
        }
        if origin.is_some_and(|origin| {
            origin.ue != self.ue || matches!(scope, Scope::User(domain) if domain != origin.domain)
        }) {
            return Err(Error::Owner);
        }
        match scope {
            Scope::Signalling => self.signalling.admit(origin),
            Scope::User(domain) => self.users[match domain {
                CnDomain::Cs => 0,
                CnDomain::Ps => 1,
            }]
            .admit(origin),
        }
    }
}
