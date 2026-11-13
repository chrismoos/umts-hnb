// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Address, Association, Transport};
use crate::{Domain, Error};
use rasn::types::BitString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const BITS_PER_OCTET: usize = 8;
const IPV4_BYTES: usize = 4;
const IPV6_BYTES: usize = 16;
const NSAP_BYTES: usize = 20;
const NSAP_IP_OFFSET: usize = 3;
const IANA_AFI: u8 = 0x35;
const ICP_IPV4: [u8; 2] = [0, 1];
const ICP_IPV6: [u8; 2] = [0, 0];
const ASSOCIATION_IE: u16 = 13;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer {
    Cs(SocketAddr),
    Ps { address: IpAddr, teid: u32 },
}

impl Address {
    // TS 25.401 6.1.8.2 uses NSAP on Iu-CS and straight IP on Iu-PS.
    pub fn from_ip(domain: Domain, address: IpAddr) -> Result<Self, Error> {
        let bytes = match domain {
            Domain::Ps => match address {
                IpAddr::V4(address) => address.octets().to_vec(),
                IpAddr::V6(address) => address.octets().to_vec(),
            },
            Domain::Cs => {
                let mut nsap = [0; NSAP_BYTES];
                nsap[0] = IANA_AFI;
                match address {
                    IpAddr::V4(address) => {
                        nsap[1..NSAP_IP_OFFSET].copy_from_slice(&ICP_IPV4);
                        nsap[NSAP_IP_OFFSET..NSAP_IP_OFFSET + IPV4_BYTES]
                            .copy_from_slice(&address.octets());
                    }
                    IpAddr::V6(address) => {
                        nsap[1..NSAP_IP_OFFSET].copy_from_slice(&ICP_IPV6);
                        nsap[NSAP_IP_OFFSET..NSAP_IP_OFFSET + IPV6_BYTES]
                            .copy_from_slice(&address.octets());
                    }
                }
                nsap.to_vec()
            }
        };
        Self::new(BitString::from_vec(bytes))
    }

    // Either encoding is accepted in both domains: osmo-sgsn sends NSAP on
    // Iu-PS by default (`rab-assign-addr-enc x213`).
    fn ip_address(&self) -> Result<IpAddr, Error> {
        match self.bits().len() {
            length if length == IPV4_BYTES * BITS_PER_OCTET => {
                Ok(Ipv4Addr::from(octets::<IPV4_BYTES>(self)?).into())
            }
            length if length == IPV6_BYTES * BITS_PER_OCTET => {
                Ok(Ipv6Addr::from(octets::<IPV6_BYTES>(self)?).into())
            }
            _ => self.nsap_address(),
        }
    }

    fn nsap_address(&self) -> Result<IpAddr, Error> {
        let nsap = octets::<NSAP_BYTES>(self)?;
        if nsap[0] != IANA_AFI {
            return Err(Error::Unsupported);
        }
        // RFC 4548 3.1/3.2 leaves the bytes following the embedded IP unspecified.
        match [nsap[1], nsap[2]] {
            ICP_IPV4 => Ok(Ipv4Addr::from([
                nsap[NSAP_IP_OFFSET],
                nsap[NSAP_IP_OFFSET + 1],
                nsap[NSAP_IP_OFFSET + 2],
                nsap[NSAP_IP_OFFSET + 3],
            ])
            .into()),
            ICP_IPV6 => {
                let mut bytes = [0; IPV6_BYTES];
                bytes.copy_from_slice(&nsap[NSAP_IP_OFFSET..NSAP_IP_OFFSET + IPV6_BYTES]);
                Ok(Ipv6Addr::from(bytes).into())
            }
            _ => Err(Error::Unsupported),
        }
    }
}

impl Transport {
    pub fn ip_peer(&self, domain: Domain) -> Result<Peer, Error> {
        self.association.validate_domain(domain)?;
        let address = self.address.ip_address()?;
        match (&self.association, domain) {
            (Association::Binding(binding), Domain::Cs) => {
                let port = u16::from_be_bytes([binding[0], binding[1]]);
                // TS 25.414 5.1.3.2 requires even RTP ports. Port zero is reserved by RFC 6335.
                if port == 0 || !port.is_multiple_of(2) {
                    return Err(Error::Value(ASSOCIATION_IE));
                }
                Ok(Peer::Cs(SocketAddr::new(address, port)))
            }
            (Association::Gtp(teid), Domain::Ps) => {
                // TS 29.281 5.1 requires accepting a control-plane peer TEID of zero.
                let teid = u32::from_be_bytes([teid[0], teid[1], teid[2], teid[3]]);
                Ok(Peer::Ps { address, teid })
            }
            _ => Err(Error::Identity),
        }
    }
}

fn octets<const N: usize>(address: &Address) -> Result<[u8; N], Error> {
    if address.bits().len() != N * BITS_PER_OCTET {
        return Err(Error::Length);
    }
    let mut bytes = [0; N];
    for index in 0..address.bits().len() {
        if address.bits()[index] {
            bytes[index / BITS_PER_OCTET] |= 1 << (BITS_PER_OCTET - 1 - index % BITS_PER_OCTET);
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
