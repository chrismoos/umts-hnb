// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::packet::{BoundDatagrams, Datagrams, UdpEndpoint};
use std::{io, net::SocketAddr};

const PAIR_ATTEMPTS: usize = 32;

pub struct RtpEndpoint {
    rtp: UdpEndpoint,
    rtcp: UdpEndpoint,
}

impl RtpEndpoint {
    // TS 25.414 5.1.3.2 reserves the following RTCP port even when RTCP is unused.
    pub fn bind(local: SocketAddr) -> io::Result<Self> {
        if local.port() != 0 && (!local.port().is_multiple_of(2) || local.port() == u16::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Iu-CS requires an even RTP port",
            ));
        }
        for _ in 0..PAIR_ATTEMPTS {
            let rtp = UdpEndpoint::bind(local)?;
            let address = rtp.local_address()?;
            if !address.port().is_multiple_of(2) {
                continue;
            }
            let rtcp = SocketAddr::new(
                address.ip(),
                address.port().checked_add(1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "RTCP port overflow")
                })?,
            );
            match UdpEndpoint::bind(rtcp) {
                Ok(rtcp) => return Ok(Self { rtp, rtcp }),
                Err(error) if local.port() == 0 && error.kind() == io::ErrorKind::AddrInUse => (),
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no RTP/RTCP port pair available within allocation budget",
        ))
    }

    pub fn local_address(&self) -> io::Result<SocketAddr> {
        self.rtp.local_address()
    }
    pub fn reserved_rtcp_address(&self) -> io::Result<SocketAddr> {
        self.rtcp.local_address()
    }
}

impl Datagrams for RtpEndpoint {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.rtp.receive(output)
    }
    fn send(&mut self, bytes: &[u8], peer: SocketAddr) -> io::Result<bool> {
        self.rtp.send(bytes, peer)
    }
}

impl BoundDatagrams for RtpEndpoint {
    fn local_address(&self) -> io::Result<SocketAddr> {
        self.rtp.local_address()
    }
}

#[cfg(test)]
mod tests;
