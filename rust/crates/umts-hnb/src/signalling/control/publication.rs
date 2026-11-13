// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ControlProtection, KeyOrigin, SignallingError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicationProtection {
    Clear,
    Integrity(KeyOrigin),
}

impl From<Option<KeyOrigin>> for PublicationProtection {
    fn from(origin: Option<KeyOrigin>) -> Self {
        match origin {
            Some(origin) => Self::Integrity(origin),
            None => Self::Clear,
        }
    }
}

impl PublicationProtection {
    pub(super) fn matches(self, received: ControlProtection) -> bool {
        match (self, received) {
            (Self::Clear, ControlProtection::Unprotected) => true,
            (Self::Integrity(expected), ControlProtection::Integrity { origin, .. }) => {
                expected == origin
            }
            _ => false,
        }
    }

    pub(super) fn wire_bits(self, encoded_bits: usize) -> Result<usize, SignallingError> {
        match self {
            Self::Clear => Ok(encoded_bits),
            Self::Integrity(_) => umts_rrc::integrity::protected_bit_length(encoded_bits)
                .map_err(|_| SignallingError::Message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use umts_types::{CnDomain, UeKey};

    fn origin() -> KeyOrigin {
        KeyOrigin {
            ue: UeKey {
                epoch: crate::test_support::at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            domain: CnDomain::Cs,
            generation: NonZeroU64::MIN,
        }
    }

    #[test]
    fn clear_and_protected_publications_reject_each_others_response_mode() {
        let protected = ControlProtection::Integrity {
            origin: origin(),
            count: 2,
        };
        assert!(PublicationProtection::Clear.matches(ControlProtection::Unprotected));
        assert!(!PublicationProtection::Clear.matches(protected));
        assert!(PublicationProtection::Integrity(origin()).matches(protected));
        assert!(
            !PublicationProtection::Integrity(origin()).matches(ControlProtection::Unprotected)
        );
    }

    #[test]
    fn integrity_response_keeps_exact_ue_domain_and_key_generation() {
        let expected = origin();
        for foreign in [
            KeyOrigin {
                ue: UeKey {
                    generation: NonZeroU64::new(2).unwrap(),
                    ..expected.ue
                },
                ..expected
            },
            KeyOrigin {
                domain: CnDomain::Ps,
                ..expected
            },
            KeyOrigin {
                generation: NonZeroU64::new(2).unwrap(),
                ..expected
            },
        ] {
            assert!(!PublicationProtection::Integrity(expected).matches(
                ControlProtection::Integrity {
                    origin: foreign,
                    count: 2
                }
            ));
        }
    }

    #[test]
    fn forecast_uses_clear_body_bits_or_exact_integrity_envelope() {
        for bits in 1..256 {
            assert_eq!(PublicationProtection::Clear.wire_bits(bits).unwrap(), bits);
            assert_eq!(
                PublicationProtection::Integrity(origin())
                    .wire_bits(bits)
                    .unwrap(),
                umts_rrc::integrity::protected_bit_length(bits).unwrap()
            );
        }
    }
}
