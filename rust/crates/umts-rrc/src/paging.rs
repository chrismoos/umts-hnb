// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::establishment::TransactionId;
use rasn::types::FixedBitString;
use umts_rrc_codec::{CodecError, EncodedRrc, encode_paging};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{CnDomain, identity::Imsi};

pub use ie::PagingCause as Cause;
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
    if records.is_empty() || records.len() > MAX_RECORDS {
        return Err(CodecError::InvalidRrcValue);
    }
    let records = records
        .iter()
        .copied()
        .map(record)
        .collect::<Result<Vec<_>, _>>()?;
    encode_paging(&class::PCCHMessage::new(
        class::PCCHMessageType::pagingType1(pdu::PagingType1::new(
            Some(ie::PagingRecordList(records)),
            None,
            None,
        )),
    ))
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
            if temporary {
                ie::PagingRecordTypeID::tmsi_GSM_MAP_P_TMSI
            } else {
                ie::PagingRecordTypeID::imsi_GSM_MAP
            },
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
}
