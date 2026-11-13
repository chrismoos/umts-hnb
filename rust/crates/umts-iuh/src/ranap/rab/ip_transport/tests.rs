// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::wire::{decode_value, encode_value};

const RTP_PORT: u16 = 30_000;
const TEID: u32 = 0x1234_5678;
const IPV4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 17);
const IPV6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 17);

fn raw(bytes: &[u8]) -> Address {
    Address::new(BitString::from_vec(bytes.to_vec())).unwrap()
}

fn transport(address: Address, domain: Domain) -> Transport {
    Transport::new(
        address,
        match domain {
            Domain::Cs => {
                let [high, low] = RTP_PORT.to_be_bytes();
                Association::binding([high, low, 0, 0])
            }
            Domain::Ps => Association::gtp(TEID),
        },
    )
}

#[test]
fn ps_uses_exact_native_ipv4_or_ipv6_octets_without_nsap() {
    for address in [IpAddr::V4(IPV4), IpAddr::V6(IPV6)] {
        let encoded = Address::from_ip(Domain::Ps, address).unwrap();
        match address {
            IpAddr::V4(address) => {
                assert_eq!(octets::<IPV4_BYTES>(&encoded).unwrap(), address.octets())
            }
            IpAddr::V6(address) => {
                assert_eq!(octets::<IPV6_BYTES>(&encoded).unwrap(), address.octets())
            }
        }
        let transport = transport(encoded, Domain::Ps);
        assert_eq!(
            transport.ip_peer(Domain::Ps),
            Ok(Peer::Ps {
                address,
                teid: TEID
            })
        );
        let decoded = decode_value::<Transport>(&encode_value(&transport).unwrap()).unwrap();
        assert_eq!(decoded.ip_peer(Domain::Ps), transport.ip_peer(Domain::Ps));
    }
}

#[test]
fn cs_ipv4_matches_the_literal_rfc4548_nsap_encoding() {
    let literal = [
        0x35, 0x00, 0x01, 192, 0, 2, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    let address = Address::from_ip(Domain::Cs, IPV4.into()).unwrap();
    assert_eq!(octets::<NSAP_BYTES>(&address).unwrap(), literal);
    assert_eq!(
        transport(raw(&literal), Domain::Cs).ip_peer(Domain::Cs),
        Ok(Peer::Cs(SocketAddr::new(IPV4.into(), RTP_PORT)))
    );
    let mut nonzero_tail = literal;
    nonzero_tail[NSAP_IP_OFFSET + IPV4_BYTES..].fill(0xa5);
    assert_eq!(
        transport(raw(&nonzero_tail), Domain::Cs).ip_peer(Domain::Cs),
        Ok(Peer::Cs(SocketAddr::new(IPV4.into(), RTP_PORT)))
    );
}

#[test]
fn cs_ipv6_matches_the_literal_nsap_encoding_and_ignores_selector() {
    let literal = [
        0x35, 0x00, 0x00, 0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 17, 0,
    ];
    assert_eq!(
        octets::<NSAP_BYTES>(&Address::from_ip(Domain::Cs, IPV6.into()).unwrap()).unwrap(),
        literal
    );
    let mut nonzero_selector = literal;
    nonzero_selector[NSAP_BYTES - 1] = 0xff;
    assert_eq!(
        transport(raw(&nonzero_selector), Domain::Cs).ip_peer(Domain::Cs),
        Ok(Peer::Cs(SocketAddr::new(IPV6.into(), RTP_PORT)))
    );
}

#[test]
fn both_domains_accept_native_ip_and_nsap_encodings() {
    for address in [IpAddr::V4(IPV4), IpAddr::V6(IPV6)] {
        assert_eq!(
            transport(Address::from_ip(Domain::Cs, address).unwrap(), Domain::Ps)
                .ip_peer(Domain::Ps),
            Ok(Peer::Ps {
                address,
                teid: TEID
            })
        );
        assert_eq!(
            transport(Address::from_ip(Domain::Ps, address).unwrap(), Domain::Cs)
                .ip_peer(Domain::Cs),
            Ok(Peer::Cs(SocketAddr::new(address, RTP_PORT)))
        );
    }
    let mut wrong = transport(
        Address::from_ip(Domain::Cs, IPV4.into()).unwrap(),
        Domain::Cs,
    );
    wrong.association = Association::gtp(TEID);
    assert_eq!(wrong.ip_peer(Domain::Cs), Err(Error::Identity));
}

#[test]
fn nsap_afi_and_icp_are_not_guessed_from_payload_bytes() {
    let mut bytes =
        octets::<NSAP_BYTES>(&Address::from_ip(Domain::Cs, IPV4.into()).unwrap()).unwrap();
    for (index, value) in [(0, 0x39), (1, 1), (2, 2), (2, 0xfa)] {
        let original = bytes[index];
        bytes[index] = value;
        for domain in [Domain::Cs, Domain::Ps] {
            assert_eq!(
                transport(raw(&bytes), domain).ip_peer(domain),
                Err(Error::Unsupported)
            );
        }
        bytes[index] = original;
    }
}

#[test]
fn non_octet_and_truncated_addresses_do_not_consume_padding_bits() {
    for domain in [Domain::Cs, Domain::Ps] {
        let address = Address::from_ip(domain, IPV4.into()).unwrap();
        let mut short = address.bits().clone();
        short.pop();
        assert_eq!(
            transport(Address::new(short).unwrap(), domain).ip_peer(domain),
            Err(Error::Length)
        );
    }
    for length in [1, 3, 5, 15, 17, 19] {
        let bytes = vec![0; length];
        assert_eq!(
            transport(raw(&bytes), Domain::Ps).ip_peer(Domain::Ps),
            Err(Error::Length)
        );
        assert_eq!(
            transport(raw(&bytes), Domain::Cs).ip_peer(Domain::Cs),
            Err(Error::Length)
        );
    }
}

#[test]
fn cs_binding_port_is_network_order_even_and_nonzero() {
    let address = Address::from_ip(Domain::Cs, IPV4.into()).unwrap();
    for port in [0_u16, 1, RTP_PORT + 1, u16::MAX] {
        let [high, low] = port.to_be_bytes();
        let peer = Transport::new(address.clone(), Association::binding([high, low, 0, 0]));
        assert_eq!(peer.ip_peer(Domain::Cs), Err(Error::Value(ASSOCIATION_IE)));
    }
    let [high, low] = RTP_PORT.to_be_bytes();
    let peer = Transport::new(address, Association::binding([high, low, 0xaa, 0xbb]));
    assert_eq!(
        peer.ip_peer(Domain::Cs),
        Ok(Peer::Cs(SocketAddr::new(IPV4.into(), RTP_PORT)))
    );
}

#[test]
fn peer_teid_zero_is_preserved_for_gtpu_backward_compatibility() {
    let peer = Transport::new(
        Address::from_ip(Domain::Ps, IPV4.into()).unwrap(),
        Association::gtp(0),
    );
    assert_eq!(
        peer.ip_peer(Domain::Ps),
        Ok(Peer::Ps {
            address: IPV4.into(),
            teid: 0
        })
    );
}

#[test]
fn mapped_ipv6_addresses_are_not_silently_rewritten_as_ipv4() {
    let mapped = IPV4.to_ipv6_mapped();
    for domain in [Domain::Cs, Domain::Ps] {
        let address = Address::from_ip(domain, mapped.into()).unwrap();
        let peer = transport(address, domain).ip_peer(domain).unwrap();
        match peer {
            Peer::Cs(address) => assert_eq!(address.ip(), IpAddr::V6(mapped)),
            Peer::Ps { address, .. } => assert_eq!(address, IpAddr::V6(mapped)),
        }
    }
}
