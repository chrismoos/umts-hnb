// SPDX-License-Identifier: AGPL-3.0-or-later

mod exchange;
mod messages;
mod resources;

pub use exchange::{AbortReason, CompletionResult, ExchangeError, ExchangePhase, SecurityExchange};
pub use messages::{Capabilities, Command, CommandParameters, Completion, IntegrityChange};
pub use resources::{ActivationSequences, BearerLayout, IntegrityActivation, SignallingSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Capabilities,
    Algorithm,
    Origin,
    Bearer,
    Activation,
    Transaction,
    Message,
    Codec(umts_rrc_codec::CodecError),
}

#[cfg(test)]
mod tests;
