// SPDX-License-Identifier: AGPL-3.0-or-later

#![cfg(feature = "usrsctp")]

use sctp_transport::{Association, Backend, Error, Limits, UdpEncapsulation};
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    num::{NonZeroU16, NonZeroUsize},
};

const PEER_SCTP_PORT: u16 = 29204;

#[test]
fn occupied_local_udp_port_fails_before_initialization() {
    let holder = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = NonZeroU16::new(holder.local_addr().unwrap().port()).unwrap();
    let result = Association::connect_with(
        Backend::Usrsctp {
            udp: Some(UdpEncapsulation {
                local_port: Some(port),
                remote_port: port,
            }),
        },
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, PEER_SCTP_PORT)),
        Limits {
            maximum_record_bytes: NonZeroUsize::new(4096).unwrap(),
            receive_chunk_bytes: NonZeroUsize::new(128).unwrap(),
            outbound_streams: NonZeroU16::new(2).unwrap(),
            inbound_streams: NonZeroU16::new(2).unwrap(),
        },
    );
    assert!(matches!(
        result,
        Err(Error::Io {
            operation: "SCTP UDP encapsulation port",
            ..
        })
    ));
}
