// SPDX-License-Identifier: AGPL-3.0-or-later

mod activation;
pub use activation::{ActivationMessage, ActivationProcedure};
mod configuration;
mod counters;
pub use counters::{ReconfigurationCounters, ReleaseCounters, SetupCounters};
mod physical;
mod procedure;
mod release;
mod release_procedure;
mod setup;
pub(crate) mod transaction;
mod transport;
pub use configuration::{
    BearerMapping, LogicalMapping, Rel99Bearer, Rel99Configuration, Rel99Rlc, UplinkLogicalMapping,
};
pub use physical::{NormalDpch, UplinkPowerControl};
pub use procedure::{
    RadioBearerSetupProcedure, SetupPhase, SetupPublicationError, SetupResponse, SetupResponseError,
};
pub use release::ExistingDchRelease;
pub use release_procedure::RadioBearerReleaseProcedure;
pub use setup::{ExistingDchSetup, InitialDchSetup, NewRab, RetainedBearer, ScheduledActivation};
pub use transaction::{
    PublicationError, ReconfigurationPhase, ReconfigurationResponse, ResponseError,
};
pub use transport::{
    Combination, DchDirection, DchFormat, DchFormatGroup, DchTfcs, DchTransport, GainFactors,
};
use umts_rrc_codec::CodecError;
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DedicatedError {
    Capacity,
    Transport,
    Format,
    Combination,
    Gains,
    Mapping,
    Bearer,
    Physical,
    Activation,
    Phase,
    Time,
    Identity,
    Transaction,
    Codec(CodecError),
}
