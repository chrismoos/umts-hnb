// SPDX-License-Identifier: AGPL-3.0-or-later

use super::Modulation;
use crate::DspError;
use std::num::NonZeroUsize;

// TS 25.321 Table 9.2.3.1 and normative Annex A, bit-aligned MAC-hs.
const QPSK_OFFSETS: [usize; 15] = [
    1, 40, 63, 79, 92, 102, 111, 118, 125, 131, 136, 141, 145, 150, 153,
];
const QAM16_OFFSETS: [usize; 15] = [
    40, 79, 102, 118, 131, 141, 150, 157, 164, 169, 175, 180, 184, 188, 192,
];
const OCTET_QPSK_OFFSETS: [usize; 15] = [
    1, 58, 81, 97, 109, 119, 128, 136, 142, 148, 153, 158, 163, 167, 171,
];
const OCTET_QAM16_OFFSETS: [usize; 15] = [
    58, 97, 119, 136, 148, 158, 167, 174, 181, 187, 192, 197, 201, 206, 209,
];
const OCTET_QAM64_OFFSETS: [usize; 15] = [
    81, 119, 142, 158, 171, 181, 190, 197, 204, 209, 215, 220, 224, 228, 233,
];

// TS 25.321 Annex A is normative. The informative formula differs at the final entry.
const OCTET_TRANSPORT_BITS: [u16; 295] = [
    120, 128, 136, 144, 152, 160, 168, 176, 184, 192, 200, 208, 216, 224, 232, 240, 248, 256, 264,
    272, 280, 288, 296, 304, 312, 320, 328, 336, 344, 352, 360, 368, 376, 384, 392, 400, 408, 416,
    424, 440, 448, 456, 464, 472, 480, 488, 496, 504, 512, 528, 536, 544, 552, 560, 576, 584, 592,
    608, 616, 624, 640, 648, 664, 672, 688, 696, 712, 728, 736, 752, 768, 776, 792, 808, 824, 840,
    848, 864, 880, 896, 912, 928, 952, 968, 984, 1000, 1016, 1040, 1056, 1072, 1096, 1112, 1136,
    1152, 1176, 1200, 1216, 1240, 1264, 1288, 1312, 1336, 1360, 1384, 1408, 1432, 1456, 1488, 1512,
    1536, 1568, 1600, 1624, 1656, 1688, 1712, 1744, 1776, 1808, 1840, 1872, 1912, 1944, 1976, 2016,
    2048, 2088, 2128, 2168, 2200, 2240, 2288, 2328, 2368, 2408, 2456, 2496, 2544, 2592, 2632, 2680,
    2736, 2784, 2832, 2880, 2936, 2984, 3040, 3096, 3152, 3208, 3264, 3328, 3384, 3448, 3512, 3576,
    3640, 3704, 3768, 3840, 3912, 3976, 4048, 4120, 4200, 4272, 4352, 4432, 4512, 4592, 4672, 4760,
    4848, 4936, 5024, 5112, 5208, 5296, 5392, 5488, 5592, 5688, 5792, 5896, 6008, 6112, 6224, 6336,
    6448, 6568, 6688, 6808, 6928, 7056, 7184, 7312, 7440, 7576, 7712, 7856, 7992, 8136, 8288, 8440,
    8592, 8744, 8904, 9064, 9224, 9392, 9560, 9736, 9912, 10088, 10272, 10456, 10648, 10840, 11032,
    11232, 11432, 11640, 11848, 12064, 12280, 12504, 12728, 12960, 13192, 13432, 13672, 13920,
    14168, 14424, 14688, 14952, 15224, 15496, 15776, 16064, 16352, 16648, 16944, 17256, 17568,
    17880, 18200, 18536, 18864, 19208, 19552, 19904, 20264, 20632, 21000, 21384, 21768, 22160,
    22560, 22968, 23384, 23808, 24232, 24672, 25120, 25568, 26032, 26504, 26976, 27464, 27960,
    28464, 28976, 29504, 30032, 30576, 31128, 31688, 32264, 32848, 33440, 34040, 34656, 35280,
    35920, 36568, 37224, 37896, 38576, 39272, 39984, 40704, 41440, 42192,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportTable {
    BitAligned,
    OctetAligned,
}

impl TransportTable {
    pub fn transport_bits(
        self,
        modulation: Modulation,
        codes: u8,
        index: u8,
    ) -> Result<NonZeroUsize, DspError> {
        if !(1..=15).contains(&codes) || index >= 64 {
            return Err(DspError::InvalidCode);
        }
        let offsets = match (self, modulation) {
            (Self::BitAligned, Modulation::Qpsk) => &QPSK_OFFSETS,
            (Self::BitAligned, Modulation::Qam16) => &QAM16_OFFSETS,
            (Self::BitAligned, Modulation::Qam64) => return Err(DspError::InvalidCode),
            (Self::OctetAligned, Modulation::Qpsk) => &OCTET_QPSK_OFFSETS,
            (Self::OctetAligned, Modulation::Qam16) => &OCTET_QAM16_OFFSETS,
            (Self::OctetAligned, Modulation::Qam64) => &OCTET_QAM64_OFFSETS,
        };
        let table: &[u16] = match self {
            Self::BitAligned => &TRANSPORT_BITS,
            Self::OctetAligned => &OCTET_TRANSPORT_BITS,
        };
        let position = offsets[usize::from(codes) - 1] + usize::from(index);
        table
            .get(position - 1)
            .and_then(|&bits| NonZeroUsize::new(usize::from(bits)))
            .ok_or(DspError::InvalidCode)
    }
}
const TRANSPORT_BITS: [u16; 254] = [
    137, 149, 161, 173, 185, 197, 209, 221, 233, 245, 257, 269, 281, 293, 305, 317, 329, 341, 353,
    365, 377, 389, 401, 413, 425, 437, 449, 461, 473, 485, 497, 509, 521, 533, 545, 557, 569, 581,
    593, 605, 616, 627, 639, 650, 662, 674, 686, 699, 711, 724, 737, 751, 764, 778, 792, 806, 821,
    836, 851, 866, 882, 898, 914, 931, 947, 964, 982, 1000, 1018, 1036, 1055, 1074, 1093, 1113,
    1133, 1154, 1175, 1196, 1217, 1239, 1262, 1285, 1308, 1331, 1356, 1380, 1405, 1430, 1456, 1483,
    1509, 1537, 1564, 1593, 1621, 1651, 1681, 1711, 1742, 1773, 1805, 1838, 1871, 1905, 1939, 1974,
    2010, 2046, 2083, 2121, 2159, 2198, 2238, 2279, 2320, 2362, 2404, 2448, 2492, 2537, 2583, 2630,
    2677, 2726, 2775, 2825, 2876, 2928, 2981, 3035, 3090, 3145, 3202, 3260, 3319, 3379, 3440, 3502,
    3565, 3630, 3695, 3762, 3830, 3899, 3970, 4042, 4115, 4189, 4265, 4342, 4420, 4500, 4581, 4664,
    4748, 4834, 4921, 5010, 5101, 5193, 5287, 5382, 5480, 5579, 5680, 5782, 5887, 5993, 6101, 6211,
    6324, 6438, 6554, 6673, 6793, 6916, 7041, 7168, 7298, 7430, 7564, 7700, 7840, 7981, 8125, 8272,
    8422, 8574, 8729, 8886, 9047, 9210, 9377, 9546, 9719, 9894, 10073, 10255, 10440, 10629, 10821,
    11017, 11216, 11418, 11625, 11835, 12048, 12266, 12488, 12713, 12943, 13177, 13415, 13657,
    13904, 14155, 14411, 14671, 14936, 15206, 15481, 15761, 16045, 16335, 16630, 16931, 17237,
    17548, 17865, 18188, 18517, 18851, 19192, 19538, 19891, 20251, 20617, 20989, 21368, 21754,
    22147, 22548, 22955, 23370, 23792, 24222, 24659, 25105, 25558, 26020, 26490, 26969, 27456,
    27952,
];

pub fn transport_bits(
    modulation: Modulation,
    codes: u8,
    index: u8,
) -> Result<NonZeroUsize, DspError> {
    TransportTable::BitAligned.transport_bits(modulation, codes, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octet_table_endpoints_and_all_valid_sizes_are_exact() {
        assert_eq!(
            TransportTable::OctetAligned
                .transport_bits(Modulation::Qpsk, 1, 0)
                .unwrap()
                .get(),
            120
        );
        assert_eq!(
            TransportTable::OctetAligned
                .transport_bits(Modulation::Qam64, 15, 52)
                .unwrap()
                .get(),
            35280
        );
        assert_eq!(
            TransportTable::OctetAligned
                .transport_bits(Modulation::Qam64, 15, 62)
                .unwrap()
                .get(),
            42192
        );
        assert!(
            TransportTable::OctetAligned
                .transport_bits(Modulation::Qam64, 15, 63)
                .is_err()
        );
        assert!(
            OCTET_TRANSPORT_BITS
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(
            OCTET_TRANSPORT_BITS
                .iter()
                .all(|bits| bits.is_multiple_of(8))
        );
    }

    #[test]
    fn normative_endpoints_and_every_formula_entry_agree() {
        assert_eq!(transport_bits(Modulation::Qpsk, 1, 0).unwrap().get(), 137);
        assert_eq!(transport_bits(Modulation::Qam16, 1, 0).unwrap().get(), 605);
        assert_eq!(
            transport_bits(Modulation::Qam16, 15, 62).unwrap().get(),
            27952
        );
        assert!(transport_bits(Modulation::Qam16, 15, 63).is_err());
        for (offset, &value) in TRANSPORT_BITS.iter().enumerate() {
            let index = offset + 1;
            let expected = if index < 40 {
                125 + 12 * index
            } else {
                (296.0 * (2085.0_f64 / 2048.0).powi(index as i32)).floor() as usize
            };
            assert_eq!(usize::from(value), expected);
        }
    }
}
