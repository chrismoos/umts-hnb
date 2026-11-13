// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(dead_code)]

use sctp_transport::{Association, Backend, Limits, Receive, SendResult, UdpEncapsulation};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    num::{NonZeroU16, NonZeroUsize},
    time::{Duration, Instant},
};

pub const KERNEL_UDP_PORT: NonZeroU16 = NonZeroU16::new(9899).unwrap();
pub const UNUSED_SCTP_PORT: u16 = 29203;
const PPID: u32 = 19;
const TIMEOUT: Duration = Duration::from_secs(5);

pub fn limits() -> Limits {
    Limits {
        maximum_record_bytes: NonZeroUsize::new(4096).unwrap(),
        receive_chunk_bytes: NonZeroUsize::new(128).unwrap(),
        outbound_streams: NonZeroU16::new(2).unwrap(),
        inbound_streams: NonZeroU16::new(2).unwrap(),
    }
}

pub fn backend(local_port: Option<NonZeroU16>) -> Backend {
    Backend::Usrsctp {
        udp: Some(UdpEncapsulation {
            local_port,
            remote_port: KERNEL_UDP_PORT,
        }),
    }
}

pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

fn before(deadline: Instant) {
    assert!(
        Instant::now() < deadline,
        "SCTP over UDP operation timed out"
    );
    std::thread::sleep(Duration::from_millis(1));
}

/// Requires `net.sctp.udp_port` set to `KERNEL_UDP_PORT` and no raw-socket privilege.
pub fn roundtrip_to_kernel(backend: Backend, sctp_port: u16) {
    let listener = Socket::new(
        Domain::IPV4,
        Type::STREAM,
        Some(Protocol::from(libc::IPPROTO_SCTP)),
    )
    .unwrap();
    listener.set_reuse_address(true).unwrap();
    listener.bind(&loopback(sctp_port).into()).unwrap();
    listener.listen(1).unwrap();
    listener.set_nonblocking(true).unwrap();

    let mut client = Association::connect_with(backend, loopback(sctp_port), limits()).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let mut server = loop {
        client.complete_connect().unwrap();
        match listener.accept() {
            Ok((server, _)) => break server,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => before(deadline),
            Err(error) => panic!("accept: {error}"),
        }
    };
    while !client.complete_connect().unwrap() {
        before(deadline);
    }
    server.set_nonblocking(true).unwrap();

    let to_client = [4u8, 5, 6];
    server.write_all(&to_client).unwrap();
    loop {
        match client.receive_once().unwrap() {
            Receive::Notification(_) => client.acknowledge_receive().unwrap(),
            Receive::Record { bytes, .. } => {
                assert_eq!(bytes, to_client);
                client.acknowledge_receive().unwrap();
                break;
            }
            Receive::Idle | Receive::Fragment => before(deadline),
            Receive::Closed => panic!("kernel peer closed"),
        }
    }

    let to_server = [1u8, 2, 3];
    while client.send(0, PPID, &to_server).unwrap() == SendResult::Retry {
        before(deadline);
    }
    let mut buffer = [0u8; 16];
    let length = loop {
        match server.read(&mut buffer) {
            Ok(length) => break length,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => before(deadline),
            Err(error) => panic!("read: {error}"),
        }
    };
    assert_eq!(&buffer[..length], to_server);
}
