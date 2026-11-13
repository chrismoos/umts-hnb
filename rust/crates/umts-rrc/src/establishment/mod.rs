// SPDX-License-Identifier: AGPL-3.0-or-later

mod messages;
mod procedure;

pub use messages::{
    CellFachSetup, RejectionReason, SignallingProfile, UNUSED_DCH_ID, build_cell_fach_setup,
    build_rejection,
};
pub use procedure::{
    CloseReason, Completion, Establishment, EstablishmentPhase, ProcedureError, PublishError,
    RequestMatch,
};

use rasn::types::FixedBitString;
use umts_rrc_schema::information_elements as ie;
use umts_types::CnDomain;

const TRANSACTION_COUNT: u8 = 4;
const RNC_ID_BITS: usize = 12;
const SRNTI_BITS: usize = 20;
const CRNTI_BITS: usize = 16;
const START_BITS: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EstablishmentError {
    Transaction,
    Identity,
    Drx,
    WaitTime,
    Starts,
    Codec(umts_rrc_codec::CodecError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionId(u8);
impl TransactionId {
    pub fn new(value: u8) -> Result<Self, EstablishmentError> {
        if value >= TRANSACTION_COUNT {
            Err(EstablishmentError::Transaction)
        } else {
            Ok(Self(value))
        }
    }
    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssignedRntis {
    rnc: u16,
    srnti: u32,
    crnti: u16,
}
impl AssignedRntis {
    pub fn new(rnc: u16, srnti: u32, crnti: u16) -> Result<Self, EstablishmentError> {
        if u32::from(rnc) >= 1 << RNC_ID_BITS || srnti >= 1 << SRNTI_BITS {
            return Err(EstablishmentError::Identity);
        }
        Ok(Self { rnc, srnti, crnti })
    }
    pub fn rnc(self) -> u16 {
        self.rnc
    }
    pub fn srnti(self) -> u32 {
        self.srnti
    }
    pub fn crnti(self) -> u16 {
        self.crnti
    }
    pub fn urnti(self) -> u32 {
        (u32::from(self.rnc) << SRNTI_BITS) | self.srnti
    }
    fn wire(self) -> ie::URNTI {
        ie::URNTI::new(
            ie::SRNCIdentity(fixed(u32::from(self.rnc))),
            ie::SRNTI(fixed(self.srnti)),
        )
    }
    fn cell_wire(self) -> ie::CRNTI {
        ie::CRNTI(fixed::<CRNTI_BITS>(u32::from(self.crnti)))
    }
}

fn fixed<const N: usize>(value: u32) -> FixedBitString<N> {
    let mut bits = FixedBitString::<N>::default();
    for bit in 0..N {
        bits.set(bit, value & (1 << (N - bit - 1)) != 0);
    }
    bits
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DomainStarts {
    pub cs: Option<u32>,
    pub ps: Option<u32>,
}
impl DomainStarts {
    pub fn get(self, domain: CnDomain) -> Option<u32> {
        match domain {
            CnDomain::Cs => self.cs,
            CnDomain::Ps => self.ps,
        }
    }
    pub(crate) fn from_wire(list: &ie::STARTList) -> Result<Self, EstablishmentError> {
        if list.0.is_empty() || list.0.len() > 2 {
            return Err(EstablishmentError::Starts);
        }
        let mut result = Self::default();
        for value in &list.0 {
            let slot = match value.cn_domain_identity {
                ie::CNDomainIdentity::cs_domain => &mut result.cs,
                ie::CNDomainIdentity::ps_domain => &mut result.ps,
            };
            if slot.is_some() {
                return Err(EstablishmentError::Starts);
            }
            // rasn FixedBitString<N> has N backing bytes but only N wire bits.
            let value = value
                .start_value
                .0
                .iter()
                .take(START_BITS)
                .fold(0, |word, bit| (word << 1) | u32::from(*bit));
            if value >= 1 << START_BITS {
                return Err(EstablishmentError::Starts);
            }
            *slot = Some(value);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
