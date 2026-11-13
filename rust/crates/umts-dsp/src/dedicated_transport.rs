// SPDX-License-Identifier: AGPL-3.0-or-later

mod allocation;
mod composite;
mod fixed_downlink;
mod flexible_downlink;
mod tfcs;
mod uplink;
pub use allocation::{
    MatchedTransport, PuncturingLimit, TransportId, UplinkAllocation, UplinkRateInput,
};
pub use composite::{
    DownlinkComposite, DownlinkTransportFrame, UplinkComposite, UplinkDemultiplexer,
    UplinkTransportFrame,
};
pub use fixed_downlink::{FixedDownlink, FixedDownlinkPlan, FixedDownlinkSettings};
pub use flexible_downlink::{FlexibleDownlink, FlexibleDownlinkPlan};
pub use tfcs::{
    CombinationDefinition, ConfiguredTfcs, FixedDownlinkTfcs, FlexibleDownlinkTfcs,
    TransportDefinition, UplinkTfcs,
};
pub use uplink::{CompletedUplink, DedicatedUplink, TransportPlanLimits, UplinkTransportPlan};

#[cfg(test)]
mod tests;
