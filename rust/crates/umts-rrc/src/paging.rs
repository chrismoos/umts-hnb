// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::establishment::TransactionId;
use rasn::types::FixedBitString;
use umts_rrc_codec::{CodecError, EncodedRrc, encode_paging};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{CnDomain, identity::Imsi};

pub use ie::PagingCause as Cause;
pub use ie::ReleaseCause;
const DRX_INDEX_DIVISOR: u64 = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    Imsi(Imsi),
    Tmsi([u8; 4]),
    Ptmsi([u8; 4]),
}

pub const MAX_RECORDS: usize = 8;

#[derive(Clone, Copy)]
pub struct Record {
    pub identity: Identity,
    pub domain: CnDomain,
    pub cause: Cause,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CnOriginatedPage {
    pub domain: CnDomain,
    pub cause: Cause,
    /// Selects the TMSI or P-TMSI record type the core paged with, otherwise IMSI.
    pub temporary: bool,
}

/// TS 25.331 10.3.3.23 "UTRAN single UE identity", acted on in CELL_PCH and URA_PCH only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UtranPage {
    pub urnti: u32,
    pub cn: Option<CnOriginatedPage>,
    /// TS 25.331 8.1.2.3: the UE enters idle mode and sends no answer.
    pub release: Option<ReleaseCause>,
}

fn record_type(temporary: bool) -> ie::PagingRecordTypeID {
    if temporary {
        ie::PagingRecordTypeID::tmsi_GSM_MAP_P_TMSI
    } else {
        ie::PagingRecordTypeID::imsi_GSM_MAP
    }
}

fn domain(value: CnDomain) -> ie::CNDomainIdentity {
    match value {
        CnDomain::Cs => ie::CNDomainIdentity::cs_domain,
        CnDomain::Ps => ie::CNDomainIdentity::ps_domain,
    }
}

fn temporary_bits(value: [u8; 4]) -> FixedBitString<32> {
    let mut bits = FixedBitString::<32>::default();
    for index in 0..32 {
        bits.set(index, value[index / 8] & (1 << (7 - index % 8)) != 0);
    }
    bits
}

pub fn idle(identity: Identity, cn: CnDomain, cause: Cause) -> Result<EncodedRrc, CodecError> {
    idle_records(&[Record {
        identity,
        domain: cn,
        cause,
    }])
}

fn record(value: Record) -> Result<ie::PagingRecord, CodecError> {
    let Record {
        identity,
        domain: cn,
        cause,
    } = value;
    if matches!(
        (identity, cn),
        (Identity::Tmsi(_), CnDomain::Ps) | (Identity::Ptmsi(_), CnDomain::Cs)
    ) {
        return Err(CodecError::InvalidRrcValue);
    }
    let identity = match identity {
        Identity::Imsi(imsi) => ie::CNPagedUEIdentity::imsi_GSM_MAP(ie::IMSIGSMMAP(
            (0..imsi.digit_count())
                .map(|index| ie::Digit(imsi.digit(index).expect("validated IMSI")))
                .collect(),
        )),
        Identity::Tmsi(value) => {
            ie::CNPagedUEIdentity::tmsi_GSM_MAP(ie::TMSIGSMMAP(temporary_bits(value)))
        }
        Identity::Ptmsi(value) => {
            ie::CNPagedUEIdentity::p_TMSI_GSM_MAP(ie::PTMSIGSMMAP(temporary_bits(value)))
        }
    };
    Ok(ie::PagingRecord::cn_Identity(
        ie::PagingRecordCnIdentity::new(cause, domain(cn), identity),
    ))
}

pub fn idle_records(records: &[Record]) -> Result<EncodedRrc, CodecError> {
    paging_type1(records, &[])
}

/// One PAGING TYPE 1 for idle and parked UEs sharing a paging occasion.
///
/// The ASN.1 holds the release information only in the Release 5 "pagingRecord2List"
/// extension, so a page with a release is encoded there and every other record in the
/// Release 99 list.
pub fn paging_type1(cn: &[Record], utran: &[UtranPage]) -> Result<EncodedRrc, CodecError> {
    let total = cn.len() + utran.len();
    if total == 0 || total > MAX_RECORDS {
        return Err(CodecError::InvalidRrcValue);
    }
    let mut records = cn
        .iter()
        .copied()
        .map(record)
        .collect::<Result<Vec<_>, _>>()?;
    let mut releases = Vec::new();
    for page in utran {
        let identity = crate::mobility::urnti_wire(page.urnti);
        match page.release {
            None => records.push(ie::PagingRecord::utran_Identity(
                ie::PagingRecordUtranIdentity::new(
                    identity,
                    page.cn.map(|cn| {
                        ie::PagingRecordUtranIdentityCnOriginatedPageConnectedModeUE::new(
                            cn.cause,
                            domain(cn.domain),
                            record_type(cn.temporary),
                        )
                    }),
                ),
            )),
            Some(cause) => releases.push(ie::PagingRecord2R5::utran_SingleUE_Identity(
                ie::PagingRecord2R5UtranSingleUEIdentity::new(
                    identity,
                    page.cn.map(|cn| {
                        ie::PagingRecord2R5UtranSingleUEIdentityCnOriginatedPageConnectedModeUE::new(
                            cn.cause,
                            domain(cn.domain),
                            record_type(cn.temporary),
                        )
                    }),
                    ie::RRCConnectionReleaseInformation::release(
                        ie::RRCConnectionReleaseInformationRelease::new(cause),
                    ),
                ),
            )),
        }
    }
    let extensions = (!releases.is_empty()).then(|| {
        pdu::PagingType1LaterNonCriticalExtensions::new(
            None,
            Some(
                pdu::PagingType1LaterNonCriticalExtensionsV590NonCriticalExtensions::new(
                    pdu::PagingType1V590extIEs::new(Some(ie::PagingRecord2ListR5(releases))),
                    None,
                ),
            ),
        )
    });
    encode_paging(&class::PCCHMessage::new(
        class::PCCHMessageType::pagingType1(pdu::PagingType1::new(
            (!records.is_empty()).then_some(ie::PagingRecordList(records)),
            None,
            extensions,
        )),
    ))
}

/// TS 25.304 8.3: in connected mode the DRX cycle is the shortest of the UTRAN cycle and the
/// cycles of the CN domains the UE is attached to without a signalling connection.
pub fn connected_drx_coefficient(utran: u8, cn_without_connection: impl Iterator<Item = u8>) -> u8 {
    cn_without_connection.fold(utran, u8::min)
}

pub fn dedicated(
    transaction: TransactionId,
    cn: CnDomain,
    cause: Cause,
    temporary: bool,
) -> class::DLDCCHMessage {
    class::DLDCCHMessage {
        integrity_check_info: None,
        message: class::DLDCCHMessageType::pagingType2(pdu::PagingType2::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            cause,
            domain(cn),
            record_type(temporary),
            None,
        )),
    }
}

// TS 25.304 8.3, FDD PBP=1. The returned frame is the PICH occasion, not the PCH frame.
pub fn next_occasion(
    imsi: Imsi,
    cycle_frames: u64,
    channels: usize,
    earliest_frame: u64,
) -> Option<(usize, u64)> {
    if channels == 0 || cycle_frames == 0 || !cycle_frames.is_power_of_two() {
        return None;
    }
    let value = (0..imsi.digit_count()).fold(0u64, |value, index| {
        value * 10 + u64::from(imsi.digit(index).expect("validated IMSI"))
    });
    let channel = (value % channels as u64) as usize;
    let residue = (value / channels as u64) % cycle_frames;
    let delta = (residue + cycle_frames - earliest_frame % cycle_frames) % cycle_frames;
    Some((channel, earliest_frame.checked_add(delta)?))
}

pub fn indicator(imsi: Imsi, count: usize) -> Option<usize> {
    if ![18, 36, 72, 144].contains(&count) {
        return None;
    }
    let value = (0..imsi.digit_count()).fold(0u64, |value, index| {
        value * 10 + u64::from(imsi.digit(index).expect("validated IMSI"))
    });
    Some(((value / DRX_INDEX_DIVISOR) % count as u64) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_idle_and_dedicated_vectors_have_exact_payloads_and_bit_lengths() {
        let imsi = Imsi::from_digits("001010123456789").unwrap();
        let first = idle(
            Identity::Imsi(imsi),
            CnDomain::Ps,
            Cause::terminatingHighPrioritySignalling,
        )
        .unwrap();
        assert_eq!(
            first.bytes,
            [0x40, 0x91, 0x20, 0x02, 0x02, 0x02, 0x46, 0x8a, 0xcf, 0x12]
        );
        assert_eq!(first.bit_length, 79);
        let temporary = idle(
            Identity::Ptmsi([0x12, 0x34, 0x56, 0x78]),
            CnDomain::Ps,
            Cause::terminatingHighPrioritySignalling,
        )
        .unwrap();
        assert_eq!(temporary.bytes, [0x40, 0x94, 0x24, 0x68, 0xac, 0xf0]);
        assert_eq!(temporary.bit_length, 47);
        let second = umts_rrc_codec::encode_downlink_dedicated(&dedicated(
            TransactionId::new(2).unwrap(),
            CnDomain::Ps,
            Cause::terminatingHighPrioritySignalling,
            true,
        ))
        .unwrap();
        assert_eq!(second.bytes, [0x25, 0x4a]);
        assert_eq!(second.bit_length, 15);
        assert!(
            idle(
                Identity::Ptmsi([0; 4]),
                CnDomain::Cs,
                Cause::terminatingCauseUnknown
            )
            .is_err()
        );
        assert!(idle_records(&[]).is_err());
    }

    #[test]
    fn fdd_occasion_and_indicator_keep_absolute_frame_grid_across_sfn_wrap_and_multiple_channels() {
        let imsi = Imsi::from_digits("001010123456789").unwrap();
        assert_eq!(next_occasion(imsi, 64, 1, 0), Some((0, 21)));
        assert_eq!(next_occasion(imsi, 64, 1, 22), Some((0, 85)));
        assert_eq!(next_occasion(imsi, 64, 1, 4095), Some((0, 4117)));
        assert_eq!(indicator(imsi, 18), Some(2));
        assert_eq!(next_occasion(imsi, 64, 2, 0), Some((1, 10)));
        assert_eq!(next_occasion(imsi, 0, 1, 0), None);
        assert_eq!(next_occasion(imsi, 64, 0, 0), None);
    }

    fn decoded(encoded: &EncodedRrc) -> pdu::PagingType1 {
        let message: class::PCCHMessage = rasn::uper::decode(&encoded.bytes).unwrap();
        let class::PCCHMessageType::pagingType1(value) = message.message else {
            panic!("PAGING TYPE 1 required")
        };
        value
    }

    fn urnti(value: &ie::URNTI) -> u32 {
        crate::mobility::urnti_word(value)
    }

    #[test]
    fn utran_pages_share_one_message_with_idle_records_and_keep_every_field() {
        let imsi = Imsi::from_digits("001010123456789").unwrap();
        let idle_record = Record {
            identity: Identity::Imsi(imsi),
            domain: CnDomain::Ps,
            cause: Cause::terminatingBackgroundCall,
        };
        let plain = UtranPage {
            urnti: 0x1234_5678,
            cn: None,
            release: None,
        };
        let call = UtranPage {
            urnti: 0xfff0_0001,
            cn: Some(CnOriginatedPage {
                domain: CnDomain::Cs,
                cause: Cause::terminatingConversationalCall,
                temporary: true,
            }),
            release: None,
        };
        let value = decoded(&paging_type1(&[idle_record], &[plain, call]).unwrap());
        assert!(value.bcch_modification_info.is_none());
        assert!(value.later_non_critical_extensions.is_none());
        let records = value.paging_record_list.unwrap().0;
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[0],
            decoded(&idle_records(&[idle_record]).unwrap())
                .paging_record_list
                .unwrap()
                .0[0]
        );
        let ie::PagingRecord::utran_Identity(first) = &records[1] else {
            panic!("UTRAN identity required")
        };
        assert_eq!(urnti(&first.u_rnti), plain.urnti);
        assert!(first.cn_originated_page_connected_mode_ue.is_none());
        let ie::PagingRecord::utran_Identity(second) = &records[2] else {
            panic!("UTRAN identity required")
        };
        assert_eq!(urnti(&second.u_rnti), call.urnti);
        let page = second
            .cn_originated_page_connected_mode_ue
            .as_ref()
            .unwrap();
        assert_eq!(page.paging_cause, Cause::terminatingConversationalCall);
        assert_eq!(page.cn_domain_identity, ie::CNDomainIdentity::cs_domain);
        assert_eq!(
            page.paging_record_type_id,
            ie::PagingRecordTypeID::tmsi_GSM_MAP_P_TMSI
        );
    }

    #[test]
    fn literal_utran_page_matches_the_independent_bit_layout() {
        let encoded = paging_type1(
            &[],
            &[UtranPage {
                urnti: 0x1234_5678,
                cn: None,
                release: None,
            }],
        )
        .unwrap();
        // PCCH choice, three option bits with the record list present, one record of 1..8,
        // UTRAN identity choice, no CN page information, then the 32-bit U-RNTI.
        assert_eq!(encoded.bit_length, 41);
        assert_eq!(encoded.bytes, [0x41, 0x09, 0x1a, 0x2b, 0x3c, 0x00]);
    }

    #[test]
    fn release_pages_use_the_release_5_record_list_with_the_cause() {
        let release = UtranPage {
            urnti: 0x0abc_def1,
            cn: None,
            release: Some(ReleaseCause::userInactivity),
        };
        let wake = UtranPage {
            urnti: 0x0abc_def2,
            cn: Some(CnOriginatedPage {
                domain: CnDomain::Ps,
                cause: Cause::terminatingInteractiveCall,
                temporary: false,
            }),
            release: None,
        };
        let value = decoded(&paging_type1(&[], &[release, wake]).unwrap());
        let records = value.paging_record_list.unwrap().0;
        assert_eq!(records.len(), 1);
        let ie::PagingRecord::utran_Identity(record) = &records[0] else {
            panic!("UTRAN identity required")
        };
        assert_eq!(urnti(&record.u_rnti), wake.urnti);
        let page = record
            .cn_originated_page_connected_mode_ue
            .as_ref()
            .unwrap();
        assert_eq!(page.paging_cause, Cause::terminatingInteractiveCall);
        assert_eq!(page.cn_domain_identity, ie::CNDomainIdentity::ps_domain);
        assert_eq!(
            page.paging_record_type_id,
            ie::PagingRecordTypeID::imsi_GSM_MAP
        );
        let extension = value.later_non_critical_extensions.unwrap();
        assert!(extension.paging_type1_r3_add_ext.is_none());
        let later = extension.v590_non_critical_extensions.unwrap();
        assert!(later.v860_non_critical_extensions.is_none());
        let list = later.paging_type1_v590ext.paging_record2_list.unwrap().0;
        assert_eq!(list.len(), 1);
        let ie::PagingRecord2R5::utran_SingleUE_Identity(record) = &list[0] else {
            panic!("single UE identity required")
        };
        assert_eq!(urnti(&record.u_rnti), release.urnti);
        assert!(record.cn_originated_page_connected_mode_ue.is_none());
        assert_eq!(
            record.rrc_connection_release_information,
            ie::RRCConnectionReleaseInformation::release(
                ie::RRCConnectionReleaseInformationRelease::new(ReleaseCause::userInactivity)
            )
        );
        let only_release = decoded(&paging_type1(&[], &[release]).unwrap());
        assert!(only_release.paging_record_list.is_none());
        assert!(only_release.later_non_critical_extensions.is_some());
    }

    #[test]
    fn record_count_is_limited_across_both_identity_kinds() {
        let imsi = Imsi::from_digits("001010123456789").unwrap();
        let idle_record = Record {
            identity: Identity::Imsi(imsi),
            domain: CnDomain::Cs,
            cause: Cause::terminatingCauseUnknown,
        };
        let page = |release| UtranPage {
            urnti: 1,
            cn: None,
            release,
        };
        assert!(paging_type1(&[], &[]).is_err());
        assert!(paging_type1(&[idle_record; 4], &[page(None); 4]).is_ok());
        assert!(
            paging_type1(
                &[idle_record; 3],
                &[
                    page(None),
                    page(Some(ReleaseCause::normalEvent)),
                    page(None),
                    page(None),
                    page(None)
                ]
            )
            .is_ok()
        );
        assert!(paging_type1(&[idle_record; 5], &[page(None); 4]).is_err());
        assert!(paging_type1(&[idle_record; 8], &[page(Some(ReleaseCause::normalEvent))]).is_err());
        assert!(paging_type1(&[], &[page(None); 9]).is_err());
        assert!(
            paging_type1(
                &[Record {
                    identity: Identity::Ptmsi([0; 4]),
                    domain: CnDomain::Cs,
                    cause: Cause::terminatingCauseUnknown,
                }],
                &[page(None)]
            )
            .is_err()
        );
    }

    #[test]
    fn connected_drx_is_the_shortest_of_the_utran_and_unconnected_domain_cycles() {
        assert_eq!(connected_drx_coefficient(6, std::iter::empty()), 6);
        assert_eq!(connected_drx_coefficient(8, [7].into_iter()), 7);
        assert_eq!(connected_drx_coefficient(5, [7, 6].into_iter()), 5);
        assert_eq!(connected_drx_coefficient(9, [9, 6].into_iter()), 6);
    }
}
