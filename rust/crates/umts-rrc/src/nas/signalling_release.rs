// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{NasError, SignallingBearer, domain};
use umts_rrc_codec::DecodedUplinkDedicated;
use umts_rrc_schema::{
    class_definitions::ULDCCHMessageType,
    pdu_definitions::SignallingConnectionReleaseIndicationV860extSignallingConnectionReleaseIndicationCause as ReleaseCause,
};
use umts_types::CnDomain;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignallingRelease {
    UpperLayer(CnDomain),
    PacketSessionEnd,
}

pub fn inspect_signalling_release(
    bearer: SignallingBearer,
    message: &DecodedUplinkDedicated,
) -> Result<Option<SignallingRelease>, NasError> {
    let ULDCCHMessageType::signallingConnectionReleaseIndication(body) = &message.value.message
    else {
        return Ok(None);
    };
    if bearer != SignallingBearer::ControlAm {
        return Err(NasError::Bearer);
    }
    let domain = domain(body.cn_domain_identity);
    let cause = body
        .later_non_critical_extensions
        .as_ref()
        .and_then(|later| later.v860non_critical_extentions.as_ref())
        .map(|later| {
            later
                .signalling_connection_release_indication_v860ext
                .signalling_connection_release_indication_cause
        });
    if cause == Some(ReleaseCause::uERequestedPSDataSessionEnd) {
        if domain != CnDomain::Ps {
            return Err(NasError::Domain);
        }
        return Ok(Some(SignallingRelease::PacketSessionEnd));
    }
    Ok(Some(SignallingRelease::UpperLayer(domain)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_rrc_codec::decode_uplink_dedicated;

    #[test]
    fn independent_uper_vectors_distinguish_domain_and_release8_session_end() {
        for (bytes, bits, expected) in [
            (
                &[0x58][..],
                8,
                Ok(SignallingRelease::UpperLayer(CnDomain::Cs)),
            ),
            (
                &[0x59][..],
                8,
                Ok(SignallingRelease::UpperLayer(CnDomain::Ps)),
            ),
            (
                &[0x5b, 0x40][..],
                12,
                Ok(SignallingRelease::PacketSessionEnd),
            ),
            (
                &[0x5b, 0x50][..],
                12,
                Ok(SignallingRelease::UpperLayer(CnDomain::Ps)),
            ),
            (&[0x5a, 0x40][..], 12, Err(NasError::Domain)),
        ] {
            let message = decode_uplink_dedicated(bytes, bits).unwrap();
            assert_eq!(
                inspect_signalling_release(SignallingBearer::ControlAm, &message),
                expected.map(Some)
            );
            assert_eq!(
                inspect_signalling_release(SignallingBearer::NasAm, &message),
                Err(NasError::Bearer)
            );
        }
    }
}
