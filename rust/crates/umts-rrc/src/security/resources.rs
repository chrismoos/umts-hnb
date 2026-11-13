// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_schema::information_elements as ie;
use umts_security::counters::CipherMode;

use super::Error;

const MAX_RB: usize = 32;
const FIRST_USER_RB: u8 = 5;
const MAX_INTEGRITY_SN: u8 = 15;
const BASE_SIGNALLING_COUNT: usize = 4;
const MAX_SIGNALLING_COUNT: usize = 5;
const DPCH_ACTIVATION_MULTIPLE: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignallingSet {
    Base,
    WithLowPriorityNas,
}

impl SignallingSet {
    pub fn count(self) -> usize {
        match self {
            Self::Base => BASE_SIGNALLING_COUNT,
            Self::WithLowPriorityNas => MAX_SIGNALLING_COUNT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegrityActivation {
    signalling: SignallingSet,
    sequences: [u8; MAX_SIGNALLING_COUNT],
}

impl IntegrityActivation {
    pub fn new(signalling: SignallingSet, sequences: &[u8]) -> Result<Self, Error> {
        if sequences.len() != signalling.count()
            || sequences
                .iter()
                .any(|sequence| *sequence > MAX_INTEGRITY_SN)
        {
            return Err(Error::Activation);
        }
        let mut result = Self {
            signalling,
            sequences: [0; MAX_SIGNALLING_COUNT],
        };
        result.sequences[..sequences.len()].copy_from_slice(sequences);
        Ok(result)
    }

    pub fn signalling(self) -> SignallingSet {
        self.signalling
    }

    pub fn sequence(self, bearer: u8) -> Result<u8, Error> {
        self.sequences
            .get(usize::from(bearer))
            .copied()
            .filter(|_| usize::from(bearer) < self.signalling.count())
            .ok_or(Error::Bearer)
    }

    pub(super) fn wire(self) -> ie::IntegrityProtActivationInfo {
        // TS 25.331 10.3.3.17 orders entries RB0, RB1, RB2, RB3, RB4.
        ie::IntegrityProtActivationInfo::new(ie::RRCMessageSequenceNumberList(
            self.sequences[..self.signalling.count()]
                .iter()
                .copied()
                .map(ie::RRCMessageSequenceNumber)
                .collect(),
        ))
    }

    pub(super) fn from_wire(
        signalling: SignallingSet,
        value: &ie::IntegrityProtActivationInfo,
    ) -> Result<Self, Error> {
        let list = &value.rrc_message_sequence_number_list.0;
        if list.len() != signalling.count() {
            return Err(Error::Activation);
        }
        let mut sequences = [0; MAX_SIGNALLING_COUNT];
        for (target, sequence) in sequences.iter_mut().zip(list) {
            *target = sequence.0
        }
        Self::new(signalling, &sequences[..signalling.count()])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BearerLayout {
    signalling: SignallingSet,
    modes: [Option<CipherMode>; MAX_RB],
}

impl BearerLayout {
    pub fn new(
        signalling: SignallingSet,
        user_bearers: &[(u8, CipherMode)],
    ) -> Result<Self, Error> {
        let mut modes = [None; MAX_RB];
        modes[0] = Some(CipherMode::Unacknowledged);
        modes[1] = Some(CipherMode::Acknowledged);
        modes[2] = Some(CipherMode::Acknowledged);
        if signalling == SignallingSet::WithLowPriorityNas {
            modes[3] = Some(CipherMode::Acknowledged)
        }
        for &(bearer, mode) in user_bearers {
            if bearer < FIRST_USER_RB || usize::from(bearer) > MAX_RB {
                return Err(Error::Bearer);
            }
            let target = &mut modes[usize::from(bearer) - 1];
            if target.replace(mode).is_some() {
                return Err(Error::Bearer);
            }
        }
        Ok(Self { signalling, modes })
    }

    pub fn signalling(self) -> SignallingSet {
        self.signalling
    }

    pub fn mode(self, bearer: u8) -> Result<CipherMode, Error> {
        bearer
            .checked_sub(1)
            .and_then(|index| self.modes.get(usize::from(index)))
            .copied()
            .flatten()
            .ok_or(Error::Bearer)
    }

    pub fn cipher_bearer(self, bearer: u8) -> Result<u8, Error> {
        self.mode(bearer)?;
        // TS 25.331 8.6.3.4 defines f8 BEARER as the radio bearer identity minus one.
        Ok(bearer - 1)
    }

    pub fn has_transparent(self) -> bool {
        self.modes.contains(&Some(CipherMode::Transparent))
    }

    pub(super) fn sequence_bearers(self) -> impl Iterator<Item = (u8, CipherMode)> {
        self.modes
            .into_iter()
            .enumerate()
            .filter_map(|(index, mode)| {
                mode.filter(|mode| *mode != CipherMode::Transparent)
                    .map(|mode| ((index + 1) as u8, mode))
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivationSequences {
    layout: BearerLayout,
    sequences: [Option<u16>; MAX_RB],
}

impl ActivationSequences {
    pub fn new(layout: BearerLayout, entries: &[(u8, u16)]) -> Result<Self, Error> {
        let mut sequences = [None; MAX_RB];
        for &(bearer, sequence) in entries {
            let mode = layout.mode(bearer)?;
            if mode == CipherMode::Transparent || u32::from(sequence) >= 1 << mode.sequence_bits() {
                return Err(Error::Activation);
            }
            let target = &mut sequences[usize::from(bearer) - 1];
            if target.replace(sequence).is_some() {
                return Err(Error::Activation);
            }
        }
        if layout
            .sequence_bearers()
            .any(|(bearer, _)| sequences[usize::from(bearer) - 1].is_none())
        {
            return Err(Error::Activation);
        }
        Ok(Self { layout, sequences })
    }

    pub fn layout(self) -> BearerLayout {
        self.layout
    }

    pub fn sequence(self, bearer: u8) -> Result<u16, Error> {
        self.layout.mode(bearer)?;
        self.sequences[usize::from(bearer) - 1].ok_or(Error::Activation)
    }

    pub(super) fn wire(self) -> ie::RBActivationTimeInfoList {
        ie::RBActivationTimeInfoList(
            self.sequences
                .into_iter()
                .enumerate()
                .filter_map(|(index, sequence)| {
                    sequence.map(|sequence| {
                        ie::RBActivationTimeInfo::new(
                            ie::RBIdentity((index + 1) as u8),
                            ie::RLCSequenceNumber(sequence),
                        )
                    })
                })
                .collect(),
        )
    }

    pub(super) fn from_wire(
        layout: BearerLayout,
        value: &ie::RBActivationTimeInfoList,
    ) -> Result<Self, Error> {
        if value.0.len() > MAX_RB {
            return Err(Error::Activation);
        }
        let mut entries = [(0, 0); MAX_RB];
        for (target, value) in entries.iter_mut().zip(&value.0) {
            *target = (value.rb_identity.0, value.rlc_sequence_number.0)
        }
        Self::new(layout, &entries[..value.0.len()])
    }
}

pub(super) fn validate_dpch(layout: BearerLayout, cfn: Option<u8>) -> Result<(), Error> {
    if layout.has_transparent() != cfn.is_some()
        || cfn.is_some_and(|cfn| !cfn.is_multiple_of(DPCH_ACTIVATION_MULTIPLE))
    {
        return Err(Error::Activation);
    }
    Ok(())
}
