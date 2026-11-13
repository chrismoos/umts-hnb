// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod hnbap;
pub mod ranap;
pub mod rua;
pub mod session;
mod values;
pub mod wire;
pub use values::{Domain, NasSelector, ProtocolCause};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Capacity,
    Length,
    Decode,
    Encode,
    Trailing,
    Unsupported,
    Origin,
    State,
    Time,
    Identity,
    MissingIe(u16),
    DuplicateIe(u16),
    IeOrder(u16),
    UnknownRejectIe(u16),
    Value(u16),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity => f.write_str("Iuh resource limit exceeded"),
            Self::Length => f.write_str("Invalid Iuh field length"),
            Self::Decode => f.write_str("Invalid Iuh APER encoding"),
            Self::Encode => f.write_str("Iuh value violates its ASN.1 constraints"),
            Self::Trailing => f.write_str("Unexpected data after an Iuh value"),
            Self::Unsupported => f.write_str("Unsupported Iuh procedure or extension"),
            Self::Origin => f.write_str("Iuh association or UE identity does not match"),
            Self::State => f.write_str("Iuh procedure is incompatible with the current state"),
            Self::Time => f.write_str("Invalid or expired Iuh control time"),
            Self::Identity => f.write_str("Invalid or conflicting Iuh identity"),
            Self::MissingIe(id) => write!(f, "Missing mandatory Iuh IE {id}"),
            Self::DuplicateIe(id) => write!(f, "Duplicate Iuh IE {id}"),
            Self::IeOrder(id) => write!(f, "Iuh IE {id} is not in object-set order"),
            Self::UnknownRejectIe(id) => write!(f, "Unknown Iuh IE {id} requires rejection"),
            Self::Value(id) => write!(f, "Invalid or unsupported value in Iuh IE {id}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<umts_types::identity::IdentityError> for Error {
    fn from(_: umts_types::identity::IdentityError) -> Self {
        Self::Identity
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContextId(u32);
impl ContextId {
    pub const BITS: usize = 24;
    pub fn new(value: u32) -> Result<Self, Error> {
        if value >= 1 << Self::BITS {
            Err(Error::Identity)
        } else {
            Ok(Self(value))
        }
    }
    pub fn get(self) -> u32 {
        self.0
    }
    pub(crate) fn bytes(self) -> [u8; 3] {
        let bytes = self.0.to_be_bytes();
        [bytes[1], bytes[2], bytes[3]]
    }
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let [a, b, c] = *bytes else {
            return Err(Error::Length);
        };
        Self::new(u32::from_be_bytes([0, a, b, c]))
    }
}
