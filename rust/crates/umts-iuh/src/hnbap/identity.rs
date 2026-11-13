// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AreaCode, Plmn};
use crate::{
    Error,
    values::{fixed, octets},
};
use rasn::Decoder;
use rasn::{
    AsnType, Decode, Encode,
    types::{FixedBitString, OctetString},
};

const BCD_MASK: u8 = 0xf;
pub use umts_types::identity::Imsi;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UeIdentity {
    Imsi(Imsi),
    Tmsi {
        tmsi: [u8; 4],
        plmn: [u8; 3],
        lac: u16,
    },
    Ptmsi {
        ptmsi: [u8; 4],
        plmn: [u8; 3],
        lac: u16,
        rac: u8,
    },
    Imei([u8; 8]),
    Esn([u8; 4]),
    ImsiDs41(Vec<u8>),
    ImsiEsn {
        imsi: Vec<u8>,
        esn: [u8; 4],
    },
    TmsiDs41(Vec<u8>),
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("3..=8"))]
pub(super) struct ImsiAsn(OctetString);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("5..=7"))]
pub(super) struct Ds41Asn(OctetString);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("2..=17"))]
pub(super) struct TmsiDs41Asn(OctetString);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub(super) struct Lai {
    plmn: Plmn,
    lac: AreaCode,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub(super) struct TmsiLai {
    tmsi: FixedBitString<32>,
    lai: Lai,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub(super) struct Rai {
    lai: Lai,
    rac: super::Rac,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub(super) struct PtmsiRai {
    ptmsi: FixedBitString<32>,
    rai: Rai,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub(super) struct ImsiEsn {
    imsi: Ds41Asn,
    esn: FixedBitString<32>,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
pub(super) enum IdentityAsn {
    Imsi(ImsiAsn),
    Tmsi(TmsiLai),
    Ptmsi(PtmsiRai),
    Imei(FixedBitString<60>),
    Esn(FixedBitString<32>),
    ImsiDs41(Ds41Asn),
    ImsiEsn(ImsiEsn),
    TmsiDs41(TmsiDs41Asn),
}
fn lai(plmn: [u8; 3], lac: u16) -> Lai {
    Lai {
        plmn: Plmn(plmn.to_vec().into()),
        lac: AreaCode(lac.to_be_bytes().to_vec().into()),
    }
}
fn parse_lai(lai: &Lai) -> Result<([u8; 3], u16), Error> {
    Ok((
        lai.plmn
            .0
            .as_ref()
            .try_into()
            .map_err(|_| Error::Identity)?,
        u16::from_be_bytes(lai.lac.0.as_ref().try_into().map_err(|_| Error::Identity)?),
    ))
}
impl UeIdentity {
    pub(super) fn to_asn(&self) -> Result<IdentityAsn, Error> {
        Ok(match self {
            Self::Imsi(imsi) => IdentityAsn::Imsi(ImsiAsn(imsi.bcd().to_vec().into())),
            Self::Tmsi { tmsi, plmn, lac } => IdentityAsn::Tmsi(TmsiLai {
                tmsi: fixed(tmsi)?,
                lai: lai(*plmn, *lac),
            }),
            Self::Ptmsi {
                ptmsi,
                plmn,
                lac,
                rac,
            } => IdentityAsn::Ptmsi(PtmsiRai {
                ptmsi: fixed(ptmsi)?,
                rai: Rai {
                    lai: lai(*plmn, *lac),
                    rac: super::Rac(vec![*rac].into()),
                },
            }),
            Self::Imei(imei) => {
                if imei[7] & BCD_MASK != 0 {
                    return Err(Error::Identity);
                }
                IdentityAsn::Imei(fixed(imei)?)
            }
            Self::Esn(esn) => IdentityAsn::Esn(fixed(esn)?),
            Self::ImsiDs41(imsi) => IdentityAsn::ImsiDs41(Ds41Asn(imsi.clone().into())),
            Self::ImsiEsn { imsi, esn } => IdentityAsn::ImsiEsn(ImsiEsn {
                imsi: Ds41Asn(imsi.clone().into()),
                esn: fixed(esn)?,
            }),
            Self::TmsiDs41(tmsi) => IdentityAsn::TmsiDs41(TmsiDs41Asn(tmsi.clone().into())),
        })
    }
    pub(super) fn from_asn(identity: IdentityAsn) -> Result<Self, Error> {
        Ok(match identity {
            IdentityAsn::Imsi(imsi) => Self::Imsi(Imsi::from_bcd(imsi.0.as_ref())?),
            IdentityAsn::Tmsi(identity) => {
                let (plmn, lac) = parse_lai(&identity.lai)?;
                Self::Tmsi {
                    tmsi: octets(&identity.tmsi)
                        .try_into()
                        .map_err(|_| Error::Identity)?,
                    plmn,
                    lac,
                }
            }
            IdentityAsn::Ptmsi(identity) => {
                let (plmn, lac) = parse_lai(&identity.rai.lai)?;
                Self::Ptmsi {
                    ptmsi: octets(&identity.ptmsi)
                        .try_into()
                        .map_err(|_| Error::Identity)?,
                    plmn,
                    lac,
                    rac: identity.rai.rac.0[0],
                }
            }
            IdentityAsn::Imei(imei) => {
                Self::Imei(octets(&imei).try_into().map_err(|_| Error::Identity)?)
            }
            IdentityAsn::Esn(esn) => {
                Self::Esn(octets(&esn).try_into().map_err(|_| Error::Identity)?)
            }
            IdentityAsn::ImsiDs41(imsi) => Self::ImsiDs41(imsi.0.to_vec()),
            IdentityAsn::ImsiEsn(identity) => Self::ImsiEsn {
                imsi: identity.imsi.0.to_vec(),
                esn: octets(&identity.esn)
                    .try_into()
                    .map_err(|_| Error::Identity)?,
            },
            IdentityAsn::TmsiDs41(tmsi) => Self::TmsiDs41(tmsi.0.to_vec()),
        })
    }
}
