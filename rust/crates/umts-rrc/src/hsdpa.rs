// SPDX-License-Identifier: AGPL-3.0-or-later

mod activation;
pub(crate) mod dual_cell;
mod harq;
mod physical;
mod power;
mod procedure;
mod queues;

pub use activation::{ActivationParameters, InitialActivation, InitialConfiguration, MappedBearer};
pub use dual_cell::{
    SecondaryCellConfiguration, SecondaryCellUpdate, secondary_cell_physical_reconfiguration,
};
pub use harq::HarqConfiguration;
pub use physical::{ControlChannels, FeedbackCycle, MeasurementFeedback};
pub use power::{AckFeedback, HarqPreamble};
pub use procedure::{
    ActivationPhase, ActivationProcedure, ActivationResponse, PublicationError, ResponseError,
};
pub use queues::{QueueConfiguration, QueueSet};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigurationError {
    Capability(umts_types::hsdpa::CapabilityError),
    MemorySize,
    Feedback,
    ControlCodes,
    Queue,
    QueueSize,
    Reordering,
}
