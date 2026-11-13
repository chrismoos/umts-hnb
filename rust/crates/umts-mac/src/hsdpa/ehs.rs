// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::MacError;
use umts_types::bits::write_u32;

const LOGICAL_CHANNELS: usize = 15;
const QUEUES: u8 = 8;
const MAX_QUEUES: usize = 3;
const LENGTH_BITS: usize = 11;
const CHANNEL_BITS: usize = 4;
const SEGMENTATION_BITS: usize = 2;
const FLAG_BITS: usize = 1;
const OCTET_BITS: usize = 8;
const MAX_SDU_OCTETS: usize = (1 << LENGTH_BITS) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EhsTsnWidth {
    Six,
    Fourteen,
}

impl EhsTsnWidth {
    fn bits(self) -> usize {
        match self {
            Self::Six => 6,
            Self::Fourteen => 14,
        }
    }

    fn maximum_sdus(self) -> usize {
        match self {
            Self::Six => 26,
            Self::Fourteen => 44,
        }
    }
}

// TS 25.321 Table 9.2.2-1 describes the first and last SDU boundaries per reordering PDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EhsSegmentation {
    Complete = 0,
    Last = 1,
    First = 2,
    // One SDU is a middle segment. Multiple SDUs start with a last and end with a first segment.
    LastAndFirstOrMiddle = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct EhsSdu<'a> {
    pub logical_channel: u8,
    pub bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct EhsReorderingPdu<'a> {
    pub queue: u8,
    pub sequence: u16,
    pub segmentation: EhsSegmentation,
    pub sdus: &'a [EhsSdu<'a>],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EhsPackedPdu {
    pub transport_octets: usize,
    pub header_octets: usize,
    pub payload_octets: usize,
    pub sdus: usize,
    pub reordering_pdus: usize,
}

// TS 25.321 9.1.4 and 9.2.2. The caller assigns queue mappings, TSNs, and boundary segments.
// The output prefix is one octet-aligned transport block, excluding its PHY CRC.
pub fn pack_ehs(
    transport_octets: usize,
    tsn_width: EhsTsnWidth,
    pdus: &[EhsReorderingPdu<'_>],
    output: &mut [u8],
) -> Result<EhsPackedPdu, MacError> {
    if pdus.is_empty() {
        return Err(MacError::EmptyPayload);
    }
    if pdus.len() > MAX_QUEUES || transport_octets > output.len() {
        return Err(MacError::Capacity);
    }
    let mut queues = 0_u8;
    let mut channel_queues = [None; LOGICAL_CHANNELS];
    let mut sdus = 0_usize;
    let mut payload_octets = 0_usize;
    for pdu in pdus {
        if pdu.queue >= QUEUES
            || queues & (1 << pdu.queue) != 0
            || usize::from(pdu.sequence) >= 1 << tsn_width.bits()
        {
            return Err(MacError::Mapping);
        }
        queues |= 1 << pdu.queue;
        if pdu.sdus.is_empty() {
            return Err(MacError::EmptyPayload);
        }
        sdus = sdus.checked_add(pdu.sdus.len()).ok_or(MacError::Capacity)?;
        if sdus > tsn_width.maximum_sdus() {
            return Err(MacError::Capacity);
        }
        for sdu in pdu.sdus {
            if !(1..=LOGICAL_CHANNELS as u8).contains(&sdu.logical_channel) {
                return Err(MacError::LogicalChannel);
            }
            if sdu.bytes.is_empty() {
                return Err(MacError::EmptyPayload);
            }
            if sdu.bytes.len() > MAX_SDU_OCTETS {
                return Err(MacError::Capacity);
            }
            let mapping = &mut channel_queues[usize::from(sdu.logical_channel - 1)];
            if mapping.is_some_and(|queue| queue != pdu.queue) {
                return Err(MacError::Mapping);
            }
            *mapping = Some(pdu.queue);
            payload_octets = payload_octets
                .checked_add(sdu.bytes.len())
                .ok_or(MacError::Capacity)?;
        }
    }
    let header_bits = sdus * (CHANNEL_BITS + LENGTH_BITS + FLAG_BITS)
        + pdus.len() * (tsn_width.bits() + SEGMENTATION_BITS);
    let header_octets = header_bits / OCTET_BITS;
    if header_octets
        .checked_add(payload_octets)
        .is_none_or(|required| required > transport_octets)
    {
        return Err(MacError::Capacity);
    }
    let output = &mut output[..transport_octets];
    output.fill(0);
    let mut offset = 0;
    let mut written_sdus = 0;
    for pdu in pdus {
        for (index, sdu) in pdu.sdus.iter().enumerate() {
            write_u32(
                output,
                offset,
                CHANNEL_BITS,
                u32::from(sdu.logical_channel - 1),
            )?;
            offset += CHANNEL_BITS;
            write_u32(output, offset, LENGTH_BITS, sdu.bytes.len() as u32)?;
            offset += LENGTH_BITS;
            if index == 0 {
                write_u32(output, offset, tsn_width.bits(), u32::from(pdu.sequence))?;
                offset += tsn_width.bits();
                write_u32(output, offset, SEGMENTATION_BITS, pdu.segmentation as u32)?;
                offset += SEGMENTATION_BITS;
            }
            written_sdus += 1;
            write_u32(output, offset, FLAG_BITS, u32::from(written_sdus == sdus))?;
            offset += FLAG_BITS;
        }
    }
    let mut payload_offset = header_octets;
    for pdu in pdus {
        for sdu in pdu.sdus {
            let end = payload_offset + sdu.bytes.len();
            output[payload_offset..end].copy_from_slice(sdu.bytes);
            payload_offset = end;
        }
    }
    Ok(EhsPackedPdu {
        transport_octets,
        header_octets,
        payload_octets,
        sdus,
        reordering_pdus: pdus.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_types::bits::Bits;

    #[test]
    fn single_sdu_wire_vector_and_padding() {
        let sdus = [EhsSdu {
            logical_channel: 3,
            bytes: &[0xab, 0xcd],
        }];
        let pdus = [EhsReorderingPdu {
            queue: 0,
            sequence: 5,
            segmentation: EhsSegmentation::Complete,
            sdus: &sdus,
        }];
        let mut output = [0xff; 8];
        let packed = pack_ehs(7, EhsTsnWidth::Six, &pdus, &mut output).unwrap();
        assert_eq!(packed.header_octets, 3);
        assert_eq!(output, [0x20, 0x04, 0x29, 0xab, 0xcd, 0, 0, 0xff]);
    }

    #[test]
    fn multiple_channels_and_queues_include_tsn_only_at_queue_boundary() {
        let first = [
            EhsSdu {
                logical_channel: 1,
                bytes: &[0xaa],
            },
            EhsSdu {
                logical_channel: 2,
                bytes: &[0xbb],
            },
        ];
        let second = [EhsSdu {
            logical_channel: 3,
            bytes: &[0xcc],
        }];
        let pdus = [
            EhsReorderingPdu {
                queue: 0,
                sequence: 63,
                segmentation: EhsSegmentation::Last,
                sdus: &first,
            },
            EhsReorderingPdu {
                queue: 1,
                sequence: 7,
                segmentation: EhsSegmentation::First,
                sdus: &second,
            },
        ];
        let mut output = [0; 11];
        let packed = pack_ehs(11, EhsTsnWidth::Six, &pdus, &mut output).unwrap();
        assert_eq!(packed.header_octets, 8);
        let bits = Bits::new(&output, 88).unwrap();
        assert_eq!(bits.read_u32(15, 6).unwrap(), 63);
        assert_eq!(bits.read_u32(21, 2).unwrap(), 1);
        assert_eq!(bits.read_u32(24, 4).unwrap(), 1);
        assert_eq!(bits.read_u32(40, 4).unwrap(), 2);
        assert_eq!(bits.read_u32(55, 6).unwrap(), 7);
        assert_eq!(bits.read_u32(61, 2).unwrap(), 2);
        assert_eq!(bits.read_u32(63, 1).unwrap(), 1);
        assert_eq!(&output[8..], &[0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn all_segmentation_values_and_extended_tsn() {
        let sdus = [EhsSdu {
            logical_channel: 15,
            bytes: &[0xa5],
        }];
        for segmentation in [
            EhsSegmentation::Complete,
            EhsSegmentation::Last,
            EhsSegmentation::First,
            EhsSegmentation::LastAndFirstOrMiddle,
        ] {
            let pdus = [EhsReorderingPdu {
                queue: 7,
                sequence: 16383,
                segmentation,
                sdus: &sdus,
            }];
            let mut output = [0; 5];
            let packed = pack_ehs(5, EhsTsnWidth::Fourteen, &pdus, &mut output).unwrap();
            assert_eq!(packed.header_octets, 4);
            let bits = Bits::new(&output, 40).unwrap();
            assert_eq!(bits.read_u32(15, 14).unwrap(), 16383);
            assert_eq!(bits.read_u32(29, 2).unwrap(), segmentation as u32);
            assert_eq!(output[4], 0xa5);
        }
    }

    #[test]
    fn invalid_inputs_leave_output_unchanged() {
        let sdus = [EhsSdu {
            logical_channel: 1,
            bytes: &[0],
        }];
        let valid = EhsReorderingPdu {
            queue: 0,
            sequence: 0,
            segmentation: EhsSegmentation::Complete,
            sdus: &sdus,
        };
        let mut output = [0x55; 16];
        assert_eq!(
            pack_ehs(3, EhsTsnWidth::Six, &[valid], &mut output),
            Err(MacError::Capacity)
        );
        assert_eq!(
            pack_ehs(16, EhsTsnWidth::Six, &[valid, valid], &mut output),
            Err(MacError::Mapping)
        );
        assert_eq!(
            pack_ehs(
                16,
                EhsTsnWidth::Six,
                &[EhsReorderingPdu {
                    sequence: 64,
                    ..valid
                }],
                &mut output
            ),
            Err(MacError::Mapping)
        );
        assert_eq!(
            pack_ehs(
                16,
                EhsTsnWidth::Six,
                &[valid, EhsReorderingPdu { queue: 1, ..valid }],
                &mut output
            ),
            Err(MacError::Mapping)
        );
        assert_eq!(output, [0x55; 16]);
    }

    #[test]
    fn sdu_count_and_length_bounds() {
        let single = EhsSdu {
            logical_channel: 1,
            bytes: &[0x81],
        };
        let sdus = [single; 44];
        let mut output = [0; 256];
        for (width, maximum) in [(EhsTsnWidth::Six, 26), (EhsTsnWidth::Fourteen, 44)] {
            let pdu = EhsReorderingPdu {
                queue: 0,
                sequence: 0,
                segmentation: EhsSegmentation::Complete,
                sdus: &sdus[..maximum],
            };
            let packed = pack_ehs(256, width, &[pdu], &mut output).unwrap();
            assert_eq!(packed.sdus, maximum);
            if maximum < sdus.len() {
                assert_eq!(
                    pack_ehs(
                        256,
                        width,
                        &[EhsReorderingPdu {
                            sdus: &sdus[..maximum + 1],
                            ..pdu
                        }],
                        &mut output
                    ),
                    Err(MacError::Capacity)
                );
            }
        }
        let bytes = [0x5a; MAX_SDU_OCTETS + 1];
        let sdus = [EhsSdu {
            logical_channel: 1,
            bytes: &bytes,
        }];
        let pdu = EhsReorderingPdu {
            queue: 0,
            sequence: 0,
            segmentation: EhsSegmentation::Complete,
            sdus: &sdus,
        };
        assert_eq!(
            pack_ehs(256, EhsTsnWidth::Six, &[pdu], &mut output),
            Err(MacError::Capacity)
        );
        let sdus = [EhsSdu {
            logical_channel: 0,
            ..single
        }];
        assert_eq!(
            pack_ehs(
                256,
                EhsTsnWidth::Six,
                &[EhsReorderingPdu { sdus: &sdus, ..pdu }],
                &mut output
            ),
            Err(MacError::LogicalChannel)
        );
    }
}
