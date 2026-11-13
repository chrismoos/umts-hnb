// SPDX-License-Identifier: AGPL-3.0-or-later

mod bearers;
mod budget;
mod count;
mod epochs;
mod timeline;
mod transparent;
mod unit;

pub use bearers::{
    AddedBearerCipherConfiguration, BearerCipherConfiguration, BearerCipherError,
    BearerCipherUpdateError, BearerCiphers, CipherActivation, CipherReset, CipherRetirement,
    ExistingTransparentBearer, TransparentCounterActivation,
};
pub use budget::CipherBudget;
pub use count::{CountAnchor, Position, next_occurrence};
pub use timeline::{
    Binding, CipherTimeline, Configuration, ResetConfiguration, Scope, UpdateError,
};
pub use transparent::TransparentDomain;
pub use unit::{CipherUse, Header, header};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Owner,
    Bearer,
    Mode,
    Position,
    Sequence,
    Length,
    Exhausted,
    Capacity,
    PendingActivation,
    ReusedGeneration,
    Algorithm(crate::Error),
}
