// SPDX-License-Identifier: AGPL-3.0-or-later

const MAX_ALGORITHMS: usize = 16;
const UIA1_RANAP: u8 = 0;
const UIA2_RANAP: u8 = 1;
const UEA0_RANAP: u8 = 0;
const UEA1_RANAP: u8 = 1;
const UEA2_RANAP: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityAlgorithm {
    Uia1,
    Uia2,
}

impl IntegrityAlgorithm {
    // TS 25.413 uses zero for UIA1, unlike the algorithm number in its name.
    pub fn ranap(self) -> u8 {
        match self {
            Self::Uia1 => UIA1_RANAP,
            Self::Uia2 => UIA2_RANAP,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CipherAlternative {
    Clear,
    Uea1,
    Uea2,
}

impl CipherAlternative {
    pub fn ranap(self) -> u8 {
        match self {
            Self::Clear => UEA0_RANAP,
            Self::Uea1 => UEA1_RANAP,
            Self::Uea2 => UEA2_RANAP,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Algorithms {
    pub integrity: IntegrityAlgorithm,
    pub cipher: CipherAlternative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub algorithms: Algorithms,
    encryption_information_present: bool,
}

impl Selection {
    // TS 25.413 8.18.2 makes this IE present if and only if the command included encryption information.
    pub fn chosen_encryption(self) -> Option<CipherAlternative> {
        self.encryption_information_present
            .then_some(self.algorithms.cipher)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Support {
    pub uia1: bool,
    pub uia2: bool,
    pub uea0: bool,
    pub uea1: bool,
    pub uea2: bool,
}

impl Support {
    pub const IMPLEMENTED: Self = Self {
        uia1: true,
        uia2: true,
        uea0: true,
        uea1: true,
        uea2: true,
    };

    pub const KASUMI: Self = Self {
        uia1: true,
        uia2: false,
        uea0: true,
        uea1: true,
        uea2: false,
    };

    fn integrity(self, algorithm: IntegrityAlgorithm) -> bool {
        match algorithm {
            IntegrityAlgorithm::Uia1 => self.uia1,
            IntegrityAlgorithm::Uia2 => self.uia2,
        }
    }

    fn cipher(self, algorithm: CipherAlternative) -> bool {
        match algorithm {
            CipherAlternative::Clear => self.uea0,
            CipherAlternative::Uea1 => self.uea1,
            CipherAlternative::Uea2 => self.uea2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyStatus {
    Old,
    New,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Constraints {
    pub same_domain: Option<Algorithms>,
    pub other_domain: Option<Algorithms>,
}

pub struct Request<'a> {
    pub integrity: &'a [u8],
    pub cipher: Option<&'a [u8]>,
    pub keys: KeyStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidList,
    MissingCapabilities,
    UnsupportedIntegrity,
    UnsupportedCipher,
    Conflict,
}

pub fn select(
    request: Request<'_>,
    ue: Option<Support>,
    implemented: Support,
    constraints: Constraints,
) -> Result<Selection, Error> {
    validate(request.integrity)?;
    if let Some(cipher) = request.cipher {
        validate(cipher)?
    }
    let ue = ue.ok_or(Error::MissingCapabilities)?;
    // TS 25.413 8.18.2/8.18.4 forbids independent alternatives across active or pending CN domains.
    if constraints.same_domain.is_some() && request.keys == KeyStatus::Old {
        return Err(Error::Conflict);
    }
    let required = match (constraints.same_domain, constraints.other_domain) {
        (Some(same), Some(other)) if same != other => return Err(Error::Conflict),
        (Some(same), _) => Some(same),
        (_, other) => other,
    };
    let algorithms = if let Some(required) = required {
        if !request.integrity.contains(&required.integrity.ranap())
            || !ue.integrity(required.integrity)
            || !implemented.integrity(required.integrity)
        {
            return Err(Error::Conflict);
        }
        let permitted = match request.cipher {
            Some(allowed) => allowed.contains(&required.cipher.ranap()),
            None => required.cipher == CipherAlternative::Clear,
        };
        if !permitted || !ue.cipher(required.cipher) || !implemented.cipher(required.cipher) {
            return Err(Error::Conflict);
        }
        required
    } else {
        let integrity = request
            .integrity
            .iter()
            .filter_map(|&value| match value {
                UIA1_RANAP => Some(IntegrityAlgorithm::Uia1),
                UIA2_RANAP => Some(IntegrityAlgorithm::Uia2),
                _ => None,
            })
            .find(|algorithm| ue.integrity(*algorithm) && implemented.integrity(*algorithm))
            .ok_or(Error::UnsupportedIntegrity)?;
        let cipher = match request.cipher {
            None if implemented.uea0 && ue.uea0 => CipherAlternative::Clear,
            None => return Err(Error::UnsupportedCipher),
            Some(allowed) => allowed
                .iter()
                .filter_map(|&value| match value {
                    UEA0_RANAP => Some(CipherAlternative::Clear),
                    UEA1_RANAP => Some(CipherAlternative::Uea1),
                    UEA2_RANAP => Some(CipherAlternative::Uea2),
                    _ => None,
                })
                .find(|algorithm| ue.cipher(*algorithm) && implemented.cipher(*algorithm))
                .ok_or(Error::UnsupportedCipher)?,
        };
        Algorithms { integrity, cipher }
    };
    Ok(Selection {
        algorithms,
        encryption_information_present: request.cipher.is_some(),
    })
}

fn validate(list: &[u8]) -> Result<(), Error> {
    if list.is_empty()
        || list.len() > MAX_ALGORITHMS
        || list
            .iter()
            .any(|value| usize::from(*value) >= MAX_ALGORITHMS)
    {
        return Err(Error::InvalidList);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
