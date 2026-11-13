// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_codec::DecodedUplinkCommon;
use umts_rrc_schema::{
    class_definitions::ULCCCHMessageType, information_elements::AccessStratumReleaseIndicator,
};

pub mod hsdpa;
pub mod hsupa;
pub mod rel99;
mod transfer;
pub use transfer::{
    CapabilityExchange, CapabilityReport, ExchangeError, ExchangePhase, ReceiveResult,
    setup_security_capabilities,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessRelease {
    R99,
    R4,
    R5,
    R6,
    R7,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
    R16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Message,
    UnsupportedRelease,
    Transaction,
    MissingRadioCapability,
    InterRat,
    Expired,
}

#[cfg(test)]
mod tests;

pub fn access_release(message: &DecodedUplinkCommon) -> Result<AccessRelease, Error> {
    let ULCCCHMessageType::rrcConnectionRequest(request) = &message.value.message else {
        return Err(Error::Message);
    };
    let indicator = request
        .v3d0_non_critical_extensions
        .as_ref()
        .and_then(|extensions| extensions.v4b0_non_critical_extensions.as_ref())
        .map(|extensions| {
            extensions
                .rrc_connection_request_v4b0ext
                .access_stratum_release_indicator
        });
    // TS 25.331 10.2.39 specifies R99 when the access-stratum release indicator is absent.
    Ok(match indicator {
        None => AccessRelease::R99,
        Some(AccessStratumReleaseIndicator::rel_4) => AccessRelease::R4,
        Some(AccessStratumReleaseIndicator::rel_5) => AccessRelease::R5,
        Some(AccessStratumReleaseIndicator::rel_6) => AccessRelease::R6,
        Some(AccessStratumReleaseIndicator::rel_7) => AccessRelease::R7,
        Some(AccessStratumReleaseIndicator::rel_8) => AccessRelease::R8,
        Some(AccessStratumReleaseIndicator::rel_9) => AccessRelease::R9,
        Some(AccessStratumReleaseIndicator::rel_10) => AccessRelease::R10,
        Some(AccessStratumReleaseIndicator::rel_11) => AccessRelease::R11,
        Some(AccessStratumReleaseIndicator::rel_12) => AccessRelease::R12,
        Some(AccessStratumReleaseIndicator::rel_13) => AccessRelease::R13,
        Some(AccessStratumReleaseIndicator::rel_14) => AccessRelease::R14,
        Some(AccessStratumReleaseIndicator::rel_15) => AccessRelease::R15,
        Some(AccessStratumReleaseIndicator::rel_16) => AccessRelease::R16,
        _ => return Err(Error::UnsupportedRelease),
    })
}
