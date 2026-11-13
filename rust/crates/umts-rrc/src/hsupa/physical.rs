// SPDX-License-Identifier: AGPL-3.0-or-later

use super::ConfigurationError;
use umts_rrc_schema::information_elements as ie;
use umts_types::channel_codes::OvsfCode;

const INDICATOR_SF: u16 = 128;
const GRANT_SF: u16 = 256;
const SIGNATURES: u8 = 40;
const COMBINATIONS: u8 = 6;

#[derive(Clone, Copy, Debug)]
pub struct RelativeGrant {
    signature: u8,
    combination: u8,
}

impl RelativeGrant {
    pub fn new(signature: u8, combination: u8) -> Result<Self, ConfigurationError> {
        if signature >= SIGNATURES || combination >= COMBINATIONS {
            return Err(ConfigurationError::Code);
        }
        Ok(Self {
            signature,
            combination,
        })
    }

    pub fn signature(self) -> u8 {
        self.signature
    }
    pub fn combination(self) -> u8 {
        self.combination
    }

    pub fn wire(self) -> ie::ERGCHInformation {
        ie::ERGCHInformation::new(
            ie::EHICHRGCHSignatureSequence(self.signature),
            ie::ERGCHCombinationIndex(self.combination),
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DownlinkControl {
    absolute: OvsfCode,
    indicator: OvsfCode,
    acknowledgement_signature: u8,
    relative: Option<RelativeGrant>,
}

impl DownlinkControl {
    // E-RGCH shares E-HICH's SF128 code and requires a distinct signature.
    pub fn new(
        absolute: OvsfCode,
        indicator: OvsfCode,
        acknowledgement_signature: u8,
        relative: Option<RelativeGrant>,
        occupied: &[OvsfCode],
    ) -> Result<Self, ConfigurationError> {
        if absolute.spreading_factor() != GRANT_SF
            || indicator.spreading_factor() != INDICATOR_SF
            || acknowledgement_signature >= SIGNATURES
            || absolute.overlaps(indicator)
            || relative.is_some_and(|grant| grant.signature == acknowledgement_signature)
            || occupied
                .iter()
                .any(|code| absolute.overlaps(*code) || indicator.overlaps(*code))
        {
            return Err(ConfigurationError::Code);
        }
        Ok(Self {
            absolute,
            indicator,
            acknowledgement_signature,
            relative,
        })
    }

    pub fn absolute_code(self) -> OvsfCode {
        self.absolute
    }
    pub fn indicator_code(self) -> OvsfCode {
        self.indicator
    }
    pub fn acknowledgement_signature(self) -> u8 {
        self.acknowledgement_signature
    }
    pub fn relative(self) -> Option<RelativeGrant> {
        self.relative
    }

    pub fn absolute_wire(self) -> ie::EAGCHInformation {
        ie::EAGCHInformation::new(ie::EAGCHChannelisationCode(self.absolute.index() as u8))
    }

    pub fn acknowledgement_wire(self) -> ie::EHICHInformation {
        ie::EHICHInformation::new(
            ie::EHICHChannelisationCode(self.indicator.index() as u8),
            ie::EHICHRGCHSignatureSequence(self.acknowledgement_signature),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator_signatures_and_ovsf_branches_do_not_collide() {
        let grant = OvsfCode::new(256, 10).unwrap();
        let indicator = OvsfCode::new(128, 6).unwrap();
        let relative = RelativeGrant::new(3, 0).unwrap();
        let controls = DownlinkControl::new(grant, indicator, 2, Some(relative), &[]).unwrap();
        assert_eq!(controls.absolute_wire().e_agch_channelisation_code.0, 10);
        assert_eq!(controls.acknowledgement_wire().channelisation_code.0, 6);
        assert_eq!(controls.acknowledgement_wire().signature_sequence.0, 2);
        assert!(DownlinkControl::new(grant, indicator, 3, Some(relative), &[]).is_err());
        assert!(
            DownlinkControl::new(grant, indicator, 2, None, &[OvsfCode::new(64, 3).unwrap()])
                .is_err()
        );
        assert!(
            DownlinkControl::new(OvsfCode::new(256, 12).unwrap(), indicator, 2, None, &[]).is_err()
        );
        assert!(RelativeGrant::new(40, 0).is_err());
        assert!(RelativeGrant::new(3, 6).is_err());
    }
}
