// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

const TEMPORARY_ID: u16 = 64;
const AREA: u16 = 21;
const PAGING_CAUSE: u16 = 22;
const NON_SEARCHING: u16 = 17;
const DRX: u16 = 76;

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum Cause {
    Conversational,
    Streaming,
    Interactive,
    Background,
    LowPrioritySignalling,
    #[rasn(extension_addition)]
    HighPrioritySignalling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemporaryIdentity {
    Tmsi([u8; 4]),
    Ptmsi([u8; 4]),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub plmn: [u8; 3],
    pub lac: u16,
    pub rac: Option<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub domain: Domain,
    pub imsi: Imsi,
    pub temporary: Option<TemporaryIdentity>,
    pub area: Option<Area>,
    pub cause: Option<Cause>,
    pub drx_coefficient: Option<u8>,
    pub search_other_domain: bool,
}

#[derive(AsnType, Decode, Encode, Clone, Debug)]
#[rasn(delegate, size("4"))]
struct Temporary(OctetString);
#[derive(AsnType, Decode, Encode, Clone, Debug)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
enum TemporaryAsn {
    Tmsi(Temporary),
    Ptmsi(Temporary),
}
#[derive(AsnType, Decode, Encode, Clone, Debug)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct Rai {
    lai: Lai,
    rac: Rac,
    extensions: Option<ExtensionList>,
}
#[derive(AsnType, Decode, Encode, Clone, Debug)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
enum AreaAsn {
    Lai(Lai),
    Rai(Rai),
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
enum Searching {
    NonSearching,
    Searching,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug)]
#[rasn(delegate, value("6..=9"))]
struct Drx(u8);

impl Request {
    pub(super) fn from_packet(packet: Packet) -> Result<(Self, Vec<u16>), Error> {
        let notify = packet.check(
            &[
                DOMAIN,
                PERMANENT_ID,
                TEMPORARY_ID,
                AREA,
                PAGING_CAUSE,
                NON_SEARCHING,
                DRX,
            ],
            &[],
        )?;
        let domain = packet.required(DOMAIN)?.decode()?;
        let PermanentIdentity::Imsi(imsi) = packet.required(PERMANENT_ID)?.decode()?;
        let temporary = packet
            .optional(TEMPORARY_ID)
            .map(|ie| {
                let (value, ps) = match ie.decode::<TemporaryAsn>()? {
                    TemporaryAsn::Tmsi(value) => (value, false),
                    TemporaryAsn::Ptmsi(value) => (value, true),
                };
                if ps != (domain == Domain::Ps) {
                    return Err(Error::Value(TEMPORARY_ID));
                }
                let bytes = value
                    .0
                    .as_ref()
                    .try_into()
                    .map_err(|_| Error::Value(TEMPORARY_ID))?;
                Ok(if ps {
                    TemporaryIdentity::Ptmsi(bytes)
                } else {
                    TemporaryIdentity::Tmsi(bytes)
                })
            })
            .transpose()?;
        let area = packet
            .optional(AREA)
            .map(|ie| {
                let (lai, rac) = match ie.decode::<AreaAsn>()? {
                    AreaAsn::Lai(lai) => (lai, None),
                    AreaAsn::Rai(rai) => {
                        if rai.extensions.as_ref().is_some_and(|v| {
                            v.0.iter().any(|ie| ie.criticality != Criticality::Ignore)
                        }) {
                            return Err(Error::Unsupported);
                        }
                        (rai.lai, Some(*rai.rac.0.first().ok_or(Error::Value(AREA))?))
                    }
                };
                if lai
                    .extensions
                    .as_ref()
                    .is_some_and(|v| v.0.iter().any(|ie| ie.criticality != Criticality::Ignore))
                {
                    return Err(Error::Unsupported);
                }
                Ok(Area {
                    plmn: lai
                        .plmn
                        .0
                        .as_ref()
                        .try_into()
                        .map_err(|_| Error::Value(AREA))?,
                    lac: u16::from_be_bytes(
                        lai.lac
                            .0
                            .as_ref()
                            .try_into()
                            .map_err(|_| Error::Value(AREA))?,
                    ),
                    rac,
                })
            })
            .transpose()?;
        Ok((
            Self {
                domain,
                imsi: Imsi::from_bcd(imsi.0.as_ref())?,
                temporary,
                area,
                cause: packet.optional(PAGING_CAUSE).map(Ie::decode).transpose()?,
                drx_coefficient: packet
                    .optional(DRX)
                    .map(|ie| ie.decode::<Drx>().map(|v| v.0))
                    .transpose()?,
                search_other_domain: packet
                    .optional(NON_SEARCHING)
                    .map(Ie::decode::<Searching>)
                    .transpose()?
                    != Some(Searching::NonSearching),
            },
            notify,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(hex: &str) -> Vec<u8> {
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    const FULL: &str = "000e40380000070003400180001740095000010121436587f9004040054012345678001540074000f110003f2d00164001800011400180004c400100";

    #[test]
    fn independent_minimal_and_full_paging_vectors_preserve_domain_identity_area_and_extended_cause()
     {
        let minimal = decode(
            &wire("000e40150000020003400180001740095000010121436587f9"),
            Limits::default(),
        )
        .unwrap();
        let Inbound::Paging(request) = minimal.message else {
            panic!("paging");
        };
        assert_eq!(request.imsi, Imsi::from_digits("001010123456789").unwrap());
        assert_eq!(request.domain, Domain::Ps);
        assert_eq!(request.temporary, None);
        assert_eq!(request.cause, None);
        assert_eq!(request.drx_coefficient, None);
        assert!(request.search_other_domain);
        let decoded = decode(&wire(FULL), Limits::default()).unwrap();
        let Inbound::Paging(request) = decoded.message else {
            panic!("paging");
        };
        assert_eq!(
            request.temporary,
            Some(TemporaryIdentity::Ptmsi([0x12, 0x34, 0x56, 0x78]))
        );
        assert_eq!(
            request.area,
            Some(Area {
                plmn: [0, 0xf1, 0x10],
                lac: 63,
                rac: Some(45)
            })
        );
        assert_eq!(request.cause, Some(Cause::HighPrioritySignalling));
        assert_eq!(request.drx_coefficient, Some(6));
        assert!(request.search_other_domain);
        assert!(decoded.notify_ies.is_empty());
    }

    #[test]
    fn paging_rejects_missing_identity_and_wrong_domain_and_preserves_notify_policy() {
        let mut packet = Packet::decode(Protocol::Ranap, &wire(FULL), Limits::default()).unwrap();
        packet.ies[0] = Ie::new(DOMAIN, Criticality::Ignore, &Domain::Cs).unwrap();
        assert_eq!(
            Request::from_packet(packet).unwrap_err(),
            Error::Value(TEMPORARY_ID)
        );
        let mut packet = Packet::decode(Protocol::Ranap, &wire(FULL), Limits::default()).unwrap();
        packet.ies.retain(|ie| ie.id != PERMANENT_ID);
        assert_eq!(
            Request::from_packet(packet).unwrap_err(),
            Error::MissingIe(PERMANENT_ID)
        );
        let mut packet = Packet::decode(Protocol::Ranap, &wire(FULL), Limits::default()).unwrap();
        packet
            .ies
            .push(Ie::new(123, Criticality::Notify, &true).unwrap());
        assert_eq!(Request::from_packet(packet).unwrap().1, [123]);
    }
}
