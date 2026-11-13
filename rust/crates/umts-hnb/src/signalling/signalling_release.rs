// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CoreOutbox, NasRouter, SignallingBearer, SignallingError};
use umts_rrc::nas::{SignallingRelease, inspect_signalling_release};
use umts_rrc_codec::DecodedUplinkDedicated;

pub(super) fn admit(
    bearer: SignallingBearer,
    message: &DecodedUplinkDedicated,
    router: Option<&NasRouter>,
    core: &mut CoreOutbox,
) -> Result<bool, SignallingError> {
    match inspect_signalling_release(bearer, message).map_err(SignallingError::Nas)? {
        None => Ok(false),
        // TS 25.331 8.1.14.3 permits retaining the RRC state for a PS session-end indication.
        Some(SignallingRelease::PacketSessionEnd) => Ok(true),
        Some(SignallingRelease::UpperLayer(domain)) => {
            if let Some(connection) = router.and_then(|router| router.connection(domain)) {
                core.push_signalling_release(connection)?;
            }
            Ok(true)
        }
    }
}
