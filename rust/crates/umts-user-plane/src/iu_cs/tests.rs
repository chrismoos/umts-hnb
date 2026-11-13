// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::net::{Ipv4Addr, UdpSocket};

const POLL_BUDGET: usize = 1024;

fn ephemeral() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn receive(endpoint: &mut impl Datagrams, output: &mut [u8]) -> (usize, SocketAddr) {
    for _ in 0..POLL_BUDGET {
        if let Some(value) = endpoint.receive(output).unwrap() {
            return value;
        }
        std::thread::yield_now();
    }
    panic!("loopback datagram not delivered within polling budget")
}

#[test]
fn loopback_rtp_is_nonblocking_and_bidirectional() {
    let mut first = RtpEndpoint::bind(ephemeral()).unwrap();
    let mut second = RtpEndpoint::bind(ephemeral()).unwrap();
    let mut bytes = [0; 32];
    assert_eq!(first.receive(&mut bytes).unwrap(), None);
    assert_eq!(second.receive(&mut bytes).unwrap(), None);
    let first_address = first.local_address().unwrap();
    let second_address = second.local_address().unwrap();
    assert!(first.send(&[1, 2, 3], second_address).unwrap());
    assert_eq!(receive(&mut second, &mut bytes), (3, first_address));
    assert_eq!(&bytes[..3], &[1, 2, 3]);
    assert!(second.send(&[4, 5], first_address).unwrap());
    assert_eq!(receive(&mut first, &mut bytes), (2, second_address));
    assert_eq!(&bytes[..2], &[4, 5]);
    assert_eq!(first.receive(&mut bytes).unwrap(), None);
}

#[test]
fn even_rtp_and_consecutive_rtcp_ports_are_reserved_for_endpoint_lifetime() {
    let mut endpoint = RtpEndpoint::bind(ephemeral()).unwrap();
    let rtp = endpoint.local_address().unwrap();
    let rtcp = endpoint.reserved_rtcp_address().unwrap();
    assert!(rtp.port().is_multiple_of(2));
    assert_eq!(rtcp.ip(), rtp.ip());
    assert_eq!(rtcp.port(), rtp.port() + 1);
    assert_eq!(
        UdpSocket::bind(rtp).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    assert_eq!(
        UdpSocket::bind(rtcp).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    let sender = UdpSocket::bind(ephemeral()).unwrap();
    sender.send_to(&[9, 8, 7], rtcp).unwrap();
    assert_eq!(endpoint.receive(&mut [0; 32]).unwrap(), None);
    drop(endpoint);
    let rebound = RtpEndpoint::bind(rtp).unwrap();
    assert_eq!(rebound.local_address().unwrap(), rtp);
    assert_eq!(rebound.reserved_rtcp_address().unwrap(), rtcp);
}

#[test]
fn explicit_odd_port_is_rejected_without_binding_it() {
    let pair = RtpEndpoint::bind(ephemeral()).unwrap();
    let odd = pair.reserved_rtcp_address().unwrap();
    drop(pair);
    let error = RtpEndpoint::bind(odd)
        .err()
        .expect("odd RTP port must fail");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    let socket = UdpSocket::bind(odd).unwrap();
    assert_eq!(socket.local_addr().unwrap(), odd);
}

#[test]
fn occupied_rtcp_port_does_not_leave_a_partial_rtp_binding() {
    let pair = RtpEndpoint::bind(ephemeral()).unwrap();
    let rtp = pair.local_address().unwrap();
    let rtcp = pair.reserved_rtcp_address().unwrap();
    drop(pair);
    let reservation = UdpSocket::bind(rtcp).unwrap();
    let error = RtpEndpoint::bind(rtp)
        .err()
        .expect("reserved RTCP port must fail");
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    let available_rtp = UdpSocket::bind(rtp).unwrap();
    assert_eq!(available_rtp.local_addr().unwrap(), rtp);
    drop(available_rtp);
    drop(reservation);
    assert!(RtpEndpoint::bind(rtp).is_ok());
}
