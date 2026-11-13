// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn format() -> QueueFormat {
    let mut sizes = [None; SIZE_INDICES];
    sizes[2] = NonZeroUsize::new(8);
    sizes[3] = NonZeroUsize::new(5);
    QueueFormat::new(3, sizes).unwrap()
}

#[test]
fn literal_header_and_ordered_payload_do_not_assume_octet_alignment() {
    let input = [0xa5, 0x5a];
    let pdus = [
        MacdPdu {
            size_index: 2,
            bits: Bits::new(&input[..1], 8).unwrap(),
        },
        MacdPdu {
            size_index: 2,
            bits: Bits::new(&input[1..], 8).unwrap(),
        },
    ];
    let mut output = [0xff; 5];
    let packed = format().pack(5, &pdus, 40, &mut output).unwrap();
    assert_eq!((packed.queue, packed.sequence), (3, 5));
    assert_eq!(packed.header_bits, 21);
    let bits = Bits::new(&output, 40).unwrap();
    assert_eq!(bits.read_u32(0, 21).unwrap(), 0x62a05);
    assert_eq!(bits.read_u32(21, 16).unwrap(), 0xa55a);
    assert_eq!(bits.read_u32(37, 3).unwrap(), 0);
    let decoded = format().unpack(bits).unwrap();
    assert_eq!(decoded.sequence, 5);
    assert_eq!(
        decoded
            .pdus()
            .map(|bits| bits.read_u32(0, 8).unwrap())
            .collect::<Vec<_>>(),
        [0xa5, 0x5a]
    );
}

#[test]
fn mixed_sizes_preserve_bits_and_bad_capacity_is_transactional() {
    let first = [0xa5];
    let second = [0xd8];
    let pdus = [
        MacdPdu {
            size_index: 2,
            bits: Bits::new(&first, 8).unwrap(),
        },
        MacdPdu {
            size_index: 3,
            bits: Bits::new(&second, 5).unwrap(),
        },
    ];
    let mut output = [0xff; 6];
    let packed = format().pack(63, &pdus, 48, &mut output).unwrap();
    assert_eq!((packed.queue, packed.sequence), (3, 63));
    assert_eq!(packed.header_bits, 32);
    let decoded = format().unpack(Bits::new(&output, 48).unwrap()).unwrap();
    assert_eq!(
        decoded
            .pdus()
            .map(|bits| (bits.len(), bits.read_u32(0, bits.len()).unwrap()))
            .collect::<Vec<_>>(),
        [(8, 0xa5), (5, 27)]
    );
    output.fill(0xa5);
    assert!(format().pack(63, &pdus, 44, &mut output).is_err());
    assert_eq!(output, [0xa5; 6]);
}
