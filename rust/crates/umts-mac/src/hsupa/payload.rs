// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;
use umts_types::bits::Bits;

const FIELD_BITS: usize = 6;
const HEADER_BITS: usize = 2 * FIELD_BITS;
const SI_BITS: usize = 18;
const SI_MARKER: usize = 63;
const MAX_GROUPS: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct Mapping {
    pub logical_channel: u8,
    pub flow: u8,
    pub pdu_bits: NonZeroUsize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_types::bits::write_u32;

    fn mapping(channel: u8) -> Mapping {
        Mapping {
            logical_channel: channel,
            flow: 0,
            pdu_bits: NonZeroUsize::new(8).unwrap(),
        }
    }

    fn packet(fields: &[(u32, usize)]) -> (Vec<u8>, usize) {
        let length = fields.iter().map(|(_, width)| width).sum::<usize>();
        let mut bytes = vec![0; length.div_ceil(8)];
        let mut position = 0;
        for &(value, width) in fields {
            write_u32(&mut bytes, position, width, value).unwrap();
            position += width;
        }
        (bytes, length)
    }

    #[test]
    fn headers_precede_all_payloads_and_scheduling_information_follows_them() {
        let mappings = Mappings::new(&[(1, mapping(1)), (2, mapping(2))]).unwrap();
        let (bytes, length) = packet(&[
            (1, 6),
            (2, 6),
            (2, 6),
            (1, 6),
            (63, 6),
            (5, 6),
            (0xabcd, 16),
            (9, 6),
            (0x12, 8),
            (0x23456, 18),
            (0, 3),
        ]);
        let result = Payload::parse(Bits::new(&bytes, length).unwrap(), &mappings, false).unwrap();
        let first = result.groups[0].unwrap();
        let second = result.groups[1].unwrap();
        assert_eq!((first.ddi, first.sequence, first.count), (1, 5, 2));
        assert_eq!(first.data.read_u32(0, 16).unwrap(), 0xabcd);
        assert_eq!((second.ddi, second.sequence, second.count), (2, 9, 1));
        assert_eq!(second.data.read_u32(0, 8).unwrap(), 0x12);
        assert_eq!(
            result
                .scheduling_information
                .unwrap()
                .read_u32(0, 18)
                .unwrap(),
            0x23456
        );
        assert!(result.groups[2..].iter().all(Option::is_none));
    }

    #[test]
    fn implicit_si_and_standalone_si_do_not_become_data() {
        let mappings = Mappings::new(&[(1, mapping(1))]).unwrap();
        for spare in [0, 17, 18, 23] {
            let (bytes, length) = packet(&[(1, 6), (1, 6), (63, 6), (0x55, 8), (0, spare)]);
            let result =
                Payload::parse(Bits::new(&bytes, length).unwrap(), &mappings, false).unwrap();
            assert_eq!(result.scheduling_information.is_some(), spare >= 18);
            assert_eq!(result.groups[0].unwrap().data.read_u32(0, 8).unwrap(), 0x55);
        }
        let (bytes, length) = packet(&[(0x12345, 18)]);
        let result = Payload::parse(Bits::new(&bytes, length).unwrap(), &mappings, true).unwrap();
        assert!(result.groups.iter().all(Option::is_none));
        assert!(result.scheduling_information.is_some());
    }

    #[test]
    fn duplicate_logical_channel_and_truncated_payload_are_rejected() {
        let mappings = Mappings::new(&[(1, mapping(1)), (2, mapping(1))]).unwrap();
        let (bytes, length) = packet(&[
            (1, 6),
            (1, 6),
            (2, 6),
            (1, 6),
            (0, 6),
            (0, 8),
            (0, 6),
            (0, 8),
        ]);
        assert!(matches!(
            Payload::parse(Bits::new(&bytes, length).unwrap(), &mappings, false),
            Err(Error::DuplicateChannel)
        ));
        let (bytes, length) = packet(&[(1, 6), (2, 6), (0, 6), (0, 8)]);
        assert!(matches!(
            Payload::parse(Bits::new(&bytes, length).unwrap(), &mappings, false),
            Err(Error::Length)
        ));
    }
}

pub struct Mappings([Option<Mapping>; SI_MARKER]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Mapping,
    Length,
    DuplicateChannel,
    Capacity,
}

impl Mappings {
    pub fn new(entries: &[(u8, Mapping)]) -> Result<Self, Error> {
        let mut values = [None; SI_MARKER];
        for &(ddi, mapping) in entries {
            let value = values.get_mut(usize::from(ddi)).ok_or(Error::Mapping)?;
            if value.is_some()
                || usize::from(mapping.logical_channel) >= MAX_GROUPS
                || mapping.flow > 7
            {
                return Err(Error::Mapping);
            }
            *value = Some(mapping);
        }
        Ok(Self(values))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Group<'a> {
    pub ddi: u8,
    pub mapping: Mapping,
    pub sequence: u8,
    pub count: u8,
    pub data: Bits<'a>,
}

pub struct Payload<'a> {
    pub groups: [Option<Group<'a>>; MAX_GROUPS],
    pub scheduling_information: Option<Bits<'a>>,
}

impl<'a> Payload<'a> {
    // TS 25.321 9.1.5 uses E-TFCI 0 for standalone scheduling information.
    pub fn parse(
        bits: Bits<'a>,
        mappings: &Mappings,
        scheduling_only: bool,
    ) -> Result<Self, Error> {
        let mut result = Self {
            groups: [None; MAX_GROUPS],
            scheduling_information: None,
        };
        if scheduling_only {
            if bits.len() != SI_BITS {
                return Err(Error::Length);
            }
            result.scheduling_information = Some(bits);
            return Ok(result);
        }
        let mut headers = [None; MAX_GROUPS];
        let mut seen = [false; MAX_GROUPS];
        let mut header_bits = 0_usize;
        let mut payload_bits = 0_usize;
        let mut groups = 0;
        let has_si = loop {
            if groups == MAX_GROUPS {
                return Err(Error::Capacity);
            }
            let ddi = bits
                .read_u32(header_bits, FIELD_BITS)
                .map_err(|_| Error::Length)? as usize;
            let count = bits
                .read_u32(header_bits + FIELD_BITS, FIELD_BITS)
                .map_err(|_| Error::Length)? as u8;
            let mapping = mappings
                .0
                .get(ddi)
                .copied()
                .flatten()
                .ok_or(Error::Mapping)?;
            if count == 0 {
                return Err(Error::Length);
            }
            let logical = usize::from(mapping.logical_channel);
            if seen[logical] {
                return Err(Error::DuplicateChannel);
            }
            seen[logical] = true;
            let data_bits = mapping
                .pdu_bits
                .get()
                .checked_mul(usize::from(count))
                .ok_or(Error::Length)?;
            payload_bits = payload_bits
                .checked_add(FIELD_BITS)
                .and_then(|n| n.checked_add(data_bits))
                .ok_or(Error::Length)?;
            header_bits += HEADER_BITS;
            let remaining = bits
                .len()
                .checked_sub(header_bits.checked_add(payload_bits).ok_or(Error::Length)?)
                .ok_or(Error::Length)?;
            headers[groups] = Some((ddi as u8, mapping, count, data_bits));
            groups += 1;
            // TS 25.321 9.2.4.2: 18..23 spare bits imply SI without a DDI marker.
            if remaining < SI_BITS + FIELD_BITS {
                break remaining >= SI_BITS;
            }
            if bits
                .read_u32(header_bits, FIELD_BITS)
                .map_err(|_| Error::Length)? as usize
                == SI_MARKER
            {
                header_bits += FIELD_BITS;
                break true;
            }
        };
        let mut position = header_bits;
        for (index, header) in headers[..groups].iter().enumerate() {
            let (ddi, mapping, count, data_bits) = header.expect("populated header");
            let sequence = bits
                .read_u32(position, FIELD_BITS)
                .map_err(|_| Error::Length)? as u8;
            position += FIELD_BITS;
            let end = position.checked_add(data_bits).ok_or(Error::Length)?;
            let data = bits.subspan(position..end).map_err(|_| Error::Length)?;
            result.groups[index] = Some(Group {
                ddi,
                mapping,
                sequence,
                count,
                data,
            });
            position = end;
        }
        if has_si {
            result.scheduling_information = Some(
                bits.subspan(position..position + SI_BITS)
                    .map_err(|_| Error::Length)?,
            );
        }
        Ok(result)
    }
}
