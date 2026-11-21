// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CommonSignalling, UeKey};
use std::fmt;
use umts_types::CnDomain;
use umts_types::identity::Imsi;

const SUFFIX_DIGITS: usize = 4;

#[derive(Clone, Copy, Debug)]
pub struct UeOverview {
    pub reports: super::Reports,
    pub key: UeKey,
    pub imsi: Option<Imsi>,
    pub establishment: umts_rrc::establishment::EstablishmentPhase,
    pub dedicated: Option<crate::dedicated_rx::ReceiveIdentity>,
    pub parked: Option<umts_rrc::mobility::ParkedState>,
    pub cs_connected: bool,
    pub ps_connected: bool,
    pub bearer_change_pending: bool,
    pub releasing: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SubscriberHint([u8; SUFFIX_DIGITS]);

impl SubscriberHint {
    pub fn from_imsi(imsi: Imsi) -> Self {
        let first = imsi.digit_count() - SUFFIX_DIGITS;
        Self(std::array::from_fn(|index| {
            imsi.digit(first + index).expect("validated IMSI suffix")
        }))
    }
}

impl fmt::Display for SubscriberHint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "****{}{}{}{}",
            self.0[0], self.0[1], self.0[2], self.0[3]
        )
    }
}

impl fmt::Debug for SubscriberHint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl CommonSignalling {
    pub fn visit_ue_overviews(&self, mut visit: impl FnMut(UeOverview)) {
        for slot in 0..self.registry.capacity() {
            let Some((key, context)) = self.registry.at_slot(slot) else {
                continue;
            };
            visit(UeOverview {
                reports: context.reports,
                key,
                imsi: context.registration_imsi.or(context.core_imsi),
                establishment: context.setup.phase(),
                dedicated: context.dedicated_route,
                parked: context.dormancy.parked,
                cs_connected: context
                    .router
                    .as_ref()
                    .is_some_and(|router| router.connection(CnDomain::Cs).is_some()),
                ps_connected: context
                    .router
                    .as_ref()
                    .is_some_and(|router| router.connection(CnDomain::Ps).is_some()),
                bearer_change_pending: context.bearer_change.is_some(),
                releasing: context.idle_release.is_some(),
            });
        }
    }
    pub fn subscriber_hint(&self, key: UeKey) -> Option<SubscriberHint> {
        self.registry
            .get(key)?
            .registration_imsi
            .map(SubscriberHint::from_imsi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_preserves_leading_zeroes_and_minimum_length_identity() {
        for (imsi, expected) in [
            ("123456789000001", "****0001"),
            ("90000", "****0000"),
            ("123450123", "****0123"),
        ] {
            let hint = SubscriberHint::from_imsi(Imsi::from_digits(imsi).unwrap());
            assert_eq!(hint.to_string(), expected);
            assert_eq!(format!("{hint:?}"), expected);
        }
    }

    #[test]
    fn neither_debug_nor_display_includes_the_full_identity() {
        let imsi = "123456789011471";
        let hint = SubscriberHint::from_imsi(Imsi::from_digits(imsi).unwrap());
        for rendered in [hint.to_string(), format!("{hint:?}"), format!("{hint:#?}")] {
            assert_eq!(rendered, "****1471");
            assert!(!rendered.contains(imsi));
            assert!(!rendered.contains("12345678901"));
        }
    }

    #[test]
    fn only_suffix_digits_are_retained_not_a_unique_subscriber_identity() {
        let original = SubscriberHint::from_imsi(Imsi::from_digits("123456789011471").unwrap());
        let copied = original;
        let other = SubscriberHint::from_imsi(Imsi::from_digits("987654321011471").unwrap());
        assert_eq!(original, copied);
        assert_eq!(original, other);
        assert_eq!(std::mem::size_of::<SubscriberHint>(), SUFFIX_DIGITS);
    }
}
