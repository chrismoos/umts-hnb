// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Where a thread role runs: a logical CPU number, or `performance`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuPlacement {
    Index(u32),
    Performance,
}

const PERFORMANCE: &str = "performance";

impl Serialize for CpuPlacement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Index(cpu) => serializer.serialize_u32(*cpu),
            Self::Performance => serializer.serialize_str(PERFORMANCE),
        }
    }
}

impl<'de> Deserialize<'de> for CpuPlacement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = CpuPlacement;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a logical CPU number or `performance`")
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                u32::try_from(value)
                    .map(CpuPlacement::Index)
                    .map_err(|_| E::custom("CPU number out of range"))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                u32::try_from(value)
                    .map(CpuPlacement::Index)
                    .map_err(|_| E::custom("CPU number out of range"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == PERFORMANCE {
                    Ok(CpuPlacement::Performance)
                } else {
                    Err(E::custom("expected a logical CPU number or `performance`"))
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_performance_roundtrip_and_other_values_are_rejected() {
        for (text, value) in [
            ("3", CpuPlacement::Index(3)),
            ("performance", CpuPlacement::Performance),
        ] {
            let parsed: CpuPlacement = serde_yaml::from_str(text).unwrap();
            assert_eq!(parsed, value);
            assert_eq!(serde_yaml::to_string(&parsed).unwrap().trim(), text);
        }
        for text in ["efficiency", "-1", "4294967296", "1.5", "true"] {
            assert!(
                serde_yaml::from_str::<CpuPlacement>(text).is_err(),
                "{text}"
            );
        }
        let unset: Option<CpuPlacement> = serde_yaml::from_str("null").unwrap();
        assert_eq!(unset, None);
    }
}
