// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Serialize};

/// Paging coordination between circuit and packet domains.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", try_from = "Representation")]
pub enum NetworkOperationMode {
    /// Network operation mode I.
    CombinedPaging,
    /// Network operation mode II.
    #[default]
    SeparatePaging,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Representation {
    Name(String),
    Number(u8),
}

impl TryFrom<Representation> for NetworkOperationMode {
    type Error = &'static str;

    fn try_from(value: Representation) -> Result<Self, Self::Error> {
        match value {
            Representation::Name(name) => match name.as_str() {
                "combined-paging" => Ok(Self::CombinedPaging),
                "separate-paging" => Ok(Self::SeparatePaging),
                _ => Err("network operation mode must be combined-paging or separate-paging"),
            },
            Representation::Number(1) => Ok(Self::CombinedPaging),
            Representation::Number(2) => Ok(Self::SeparatePaging),
            Representation::Number(_) => Err("numeric network operation mode must be 1 or 2"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_and_numeric_modes_resolve_and_serialize_identically() {
        for (name, number, expected) in [
            ("combined-paging", 1, NetworkOperationMode::CombinedPaging),
            ("separate-paging", 2, NetworkOperationMode::SeparatePaging),
        ] {
            let named: NetworkOperationMode = serde_yaml::from_str(name).unwrap();
            let numeric: NetworkOperationMode = serde_yaml::from_str(&number.to_string()).unwrap();
            assert_eq!(named, expected);
            assert_eq!(numeric, expected);
            assert_eq!(serde_json::to_value(named).unwrap(), name);
            assert_eq!(
                serde_yaml::to_value(named).unwrap(),
                serde_yaml::to_value(numeric).unwrap()
            );
        }
    }

    #[test]
    fn unsupported_modes_and_wrong_scalar_types_are_rejected() {
        for value in [
            "0", "3", "255", "256", "-1", "1.0", "true", "null", "[]", "{}", "mode-iii", "'2'",
        ] {
            assert!(
                serde_yaml::from_str::<NetworkOperationMode>(value).is_err(),
                "{value}"
            );
        }
    }
}
