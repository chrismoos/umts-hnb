// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

const IPV4_MINIMUM_BYTES: usize = 20;
const TCP_MINIMUM_BYTES: usize = 20;
const HEADER_WORD_BYTES: usize = 4;
const HEADER_LENGTH_MASK: u8 = 0x0f;
const IPV4_TOTAL_LENGTH: usize = 2;
const IPV4_ID: usize = 4;
const IPV4_FRAGMENT: usize = 6;
const IPV4_PROTOCOL: usize = 9;
const IPV4_ADDRESSES: usize = 12;
const TCP_PROTOCOL: u8 = 6;
const FRAGMENT_MASK: u16 = 0x3fff;
const TCP_SEQUENCE: usize = 4;
const TCP_HEADER_LENGTH: usize = 12;
const TCP_FLAGS: usize = 13;
const TCP_CONTROL_FLAGS: u8 = 0x07;
const HALF_SEQUENCE_SPACE: u32 = 1 << 31;

struct Header {
    addresses: [u8; 8],
    ports: [u8; 4],
    ip_id: u16,
    sequence: u32,
    payload_bytes: u16,
}

fn word(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}

fn packet(data: &[u8]) -> Option<Header> {
    if data.len() < IPV4_MINIMUM_BYTES
        || data[0] >> PACKET_VERSION_SHIFT != IPV4_VERSION
        || data[IPV4_PROTOCOL] != TCP_PROTOCOL
        || word(data, IPV4_FRAGMENT) & FRAGMENT_MASK != 0
        || usize::from(word(data, IPV4_TOTAL_LENGTH)) != data.len()
    {
        return None;
    }
    let header = usize::from(data[0] & HEADER_LENGTH_MASK) * HEADER_WORD_BYTES;
    if header < IPV4_MINIMUM_BYTES {
        return None;
    }
    let tcp = data.get(header..)?;
    if tcp.len() < TCP_MINIMUM_BYTES || tcp[TCP_FLAGS] & TCP_CONTROL_FLAGS != 0 {
        return None;
    }
    let tcp_header =
        usize::from(tcp[TCP_HEADER_LENGTH] >> PACKET_VERSION_SHIFT) * HEADER_WORD_BYTES;
    if tcp_header < TCP_MINIMUM_BYTES || tcp_header >= tcp.len() {
        return None;
    }
    Some(Header {
        addresses: data[IPV4_ADDRESSES..IPV4_ADDRESSES + 8].try_into().ok()?,
        ports: tcp[..4].try_into().ok()?,
        ip_id: word(data, IPV4_ID),
        sequence: u32::from_be_bytes(tcp[TCP_SEQUENCE..TCP_SEQUENCE + 4].try_into().ok()?),
        payload_bytes: u16::try_from(tcp.len() - tcp_header).ok()?,
    })
}

pub(super) fn observe(
    counters: &mut BearerCounters,
    identity: ReceiveIdentity,
    bearer: BearerId,
    resets: u64,
    data: &[u8],
    coverage: umts_rlc::acknowledged::ReceiveCoverage,
) {
    if counters.first_tcp_gap.is_some() {
        return;
    }
    let Some(header) = packet(data) else {
        counters.last_tcp_packet = None;
        return;
    };
    let current = TcpPacketEvidence {
        identity,
        bearer,
        resets,
        addresses: header.addresses,
        ports: header.ports,
        ip_id: header.ip_id,
        sequence: header.sequence,
        payload_bytes: header.payload_bytes,
        first_position: coverage.first_position(),
        last_position: coverage.last_position(),
    };
    record(counters, current);
}

fn record(counters: &mut BearerCounters, current: TcpPacketEvidence) {
    if counters.first_tcp_gap.is_some() {
        return;
    }
    if let Some(previous) = counters.last_tcp_packet
        && previous.identity == current.identity
        && previous.bearer == current.bearer
        && previous.resets == current.resets
        && previous.addresses == current.addresses
        && previous.ports == current.ports
    {
        let expected = previous
            .sequence
            .wrapping_add(u32::from(previous.payload_bytes));
        let sequence_gap = current.sequence.wrapping_sub(expected);
        if sequence_gap >= HALF_SEQUENCE_SPACE {
            return;
        }
        if sequence_gap != 0 {
            counters.first_tcp_gap = Some(TcpGapEvidence {
                previous,
                current,
                sequence_gap,
            });
        }
    }
    counters.last_tcp_packet = Some(current);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_requires_complete_unfragmented_tcp_data() {
        let mut bytes = [0_u8; 41];
        bytes[0] = 0x45;
        bytes[3] = bytes.len() as u8;
        bytes[IPV4_PROTOCOL] = TCP_PROTOCOL;
        bytes[IPV4_MINIMUM_BYTES + TCP_HEADER_LENGTH] = 0x50;
        assert_eq!(packet(&bytes).unwrap().payload_bytes, 1);
        for end in 0..bytes.len() {
            assert!(packet(&bytes[..end]).is_none());
        }
        bytes[IPV4_FRAGMENT] = 0x20;
        assert!(packet(&bytes).is_none());
        bytes[IPV4_FRAGMENT] = 0;
        bytes[IPV4_MINIMUM_BYTES + TCP_FLAGS] = 2;
        assert!(packet(&bytes).is_none());
    }

    #[test]
    fn first_gap_is_bounded_and_retransmissions_do_not_move_the_frontier() {
        let first = TcpPacketEvidence {
            identity: ReceiveIdentity {
                ue: UeKey {
                    epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
                    slot: 0,
                    generation: std::num::NonZeroU64::MIN,
                },
                configuration: std::num::NonZeroU64::MIN,
            },
            bearer: BearerId::new(5).unwrap(),
            resets: 0,
            addresses: [0; 8],
            ports: [0; 4],
            ip_id: 1,
            sequence: u32::MAX - 9,
            payload_bytes: 10,
            first_position: 10,
            last_position: 11,
        };
        let mut counts = BearerCounters::default();
        record(&mut counts, first);
        let second = TcpPacketEvidence {
            sequence: 0,
            ip_id: 2,
            first_position: 11,
            last_position: 12,
            ..first
        };
        record(&mut counts, second);
        record(&mut counts, first);
        assert_eq!(counts.last_tcp_packet, Some(second));
        assert_eq!(counts.first_tcp_gap, None);
        let third = TcpPacketEvidence {
            sequence: 20,
            ip_id: 4,
            first_position: 12,
            last_position: 13,
            ..first
        };
        record(&mut counts, third);
        let frozen = counts;
        record(
            &mut counts,
            TcpPacketEvidence {
                sequence: 100,
                ..third
            },
        );
        assert_eq!(counts, frozen);
        assert_eq!(
            counts.first_tcp_gap,
            Some(TcpGapEvidence {
                previous: second,
                current: third,
                sequence_gap: 10
            })
        );
        let mut isolated = BearerCounters::default();
        record(&mut isolated, second);
        record(&mut isolated, TcpPacketEvidence { resets: 1, ..third });
        assert_eq!(isolated.first_tcp_gap, None);
    }
}
