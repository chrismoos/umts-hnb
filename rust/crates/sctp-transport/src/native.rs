// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(unsafe_code)]

use crate::socket::Read;
use crate::{Error, Limits, Metadata, SendResult};
use std::{
    io,
    mem::{size_of, size_of_val},
    net::SocketAddr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

const SCTP_INITMSG: i32 = 2;
const SCTP_NODELAY: i32 = 3;
const SCTP_EVENTS: i32 = 11;
const SCTP_FRAGMENT_INTERLEAVE: i32 = 18;
const MSG_NOTIFICATION: i32 = 0x8000;
const SCTP_UNORDERED: u16 = 1;

#[repr(C)]
#[derive(Default)]
struct Info {
    stream: u16,
    sequence: u16,
    flags: u16,
    padding: u16,
    ppid: u32,
    context: u32,
    time_to_live: u32,
    tsn: u32,
    cumulative_tsn: u32,
    association: i32,
}
#[repr(C)]
struct Init {
    outbound: u16,
    inbound: u16,
    attempts: u16,
    timeout: u16,
}

#[link(name = "sctp")]
unsafe extern "C" {
    fn sctp_send(
        fd: libc::c_int,
        bytes: *const libc::c_void,
        length: libc::size_t,
        info: *const Info,
        flags: libc::c_int,
    ) -> libc::c_int;
    fn sctp_recvmsg(
        fd: libc::c_int,
        bytes: *mut libc::c_void,
        length: libc::size_t,
        from: *mut libc::sockaddr,
        from_length: *mut libc::socklen_t,
        info: *mut Info,
        flags: *mut libc::c_int,
    ) -> libc::c_int;
}

fn io_error(operation: &'static str) -> Error {
    Error::Io {
        operation,
        source: io::Error::last_os_error(),
    }
}
fn retry(source: &io::Error) -> bool {
    matches!(
        source.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
fn option<T>(fd: &OwnedFd, name: i32, value: &T) -> Result<(), Error> {
    let length = libc::socklen_t::try_from(size_of_val(value)).map_err(|_| Error::Configuration)?;
    // The private callers pass initialized Linux SCTP UAPI values whose storage remains borrowed for this call.
    let result = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::IPPROTO_SCTP,
            name,
            std::ptr::from_ref(value).cast(),
            length,
        )
    };
    if result < 0 {
        return Err(io_error("setsockopt"));
    }
    Ok(())
}
pub(crate) fn connect(peer: SocketAddr, limits: Limits) -> Result<(OwnedFd, bool), Error> {
    let family = if peer.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    // A successful socket call returns a new descriptor with exclusive ownership.
    let raw = unsafe {
        libc::socket(
            family,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            libc::IPPROTO_SCTP,
        )
    };
    if raw < 0 {
        return Err(io_error("socket"));
    }
    // Ownership transfers exactly once from the successful socket call.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    option(&fd, SCTP_NODELAY, &1i32)?;
    option(&fd, SCTP_FRAGMENT_INTERLEAVE, &0i32)?;
    option(
        &fd,
        SCTP_INITMSG,
        &Init {
            outbound: limits.outbound_streams.get(),
            inbound: limits.inbound_streams.get(),
            attempts: 0,
            timeout: 0,
        },
    )?;
    // Linux sctp_event_subscribe order is data, association, address, send failure, peer error, shutdown, partial delivery.
    option(
        &fd,
        SCTP_EVENTS,
        &[1u8, 1, 0, 1, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    )?;
    let result = match peer {
        SocketAddr::V4(peer) => {
            let address = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: peer.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(peer.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // The address matches AF_INET and remains initialized and borrowed through connect.
            unsafe {
                libc::connect(
                    fd.as_raw_fd(),
                    std::ptr::from_ref(&address).cast(),
                    size_of_val(&address) as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(peer) => {
            let address = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: peer.port().to_be(),
                sin6_flowinfo: peer.flowinfo().to_be(),
                sin6_addr: libc::in6_addr {
                    s6_addr: peer.ip().octets(),
                },
                sin6_scope_id: peer.scope_id(),
            };
            // The address matches AF_INET6 and remains initialized and borrowed through connect.
            unsafe {
                libc::connect(
                    fd.as_raw_fd(),
                    std::ptr::from_ref(&address).cast(),
                    size_of_val(&address) as libc::socklen_t,
                )
            }
        }
    };
    if result == 0 {
        return Ok((fd, true));
    }
    let source = io::Error::last_os_error();
    if matches!(
        source.raw_os_error(),
        Some(libc::EINPROGRESS | libc::EALREADY | libc::EINTR)
    ) {
        Ok((fd, false))
    } else {
        Err(Error::Io {
            operation: "connect",
            source,
        })
    }
}

pub(crate) fn connect_complete(fd: &OwnedFd) -> Result<bool, Error> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // One initialized pollfd is exclusively borrowed and the zero timeout cannot block the control owner.
    let result = unsafe { libc::poll(&mut poll, 1, 0) };
    if result < 0 {
        let source = io::Error::last_os_error();
        return if retry(&source) {
            Ok(false)
        } else {
            Err(Error::Io {
                operation: "poll connect",
                source,
            })
        };
    }
    if result == 0 {
        return Ok(false);
    }
    if poll.revents & libc::POLLNVAL != 0 {
        return Err(Error::State);
    }
    let mut error = 0i32;
    let mut length = size_of_val(&error) as libc::socklen_t;
    // The integer and its length are valid writable storage for SO_ERROR on the owned descriptor.
    if unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            std::ptr::from_mut(&mut error).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io_error("getsockopt SO_ERROR"));
    }
    if length as usize != size_of_val(&error) {
        return Err(Error::State);
    }
    if error != 0 {
        return Err(Error::Io {
            operation: "complete connect",
            source: io::Error::from_raw_os_error(error),
        });
    }
    Ok(poll.revents & libc::POLLOUT != 0)
}

pub(crate) fn send(
    fd: &OwnedFd,
    stream: u16,
    ppid: u32,
    bytes: &[u8],
) -> Result<SendResult, Error> {
    let info = Info {
        stream,
        ppid: ppid.to_be(),
        ..Info::default()
    };
    // RFC 6458 requires network-order PPID. Both buffers remain initialized and borrowed for the native call.
    let result = unsafe {
        sctp_send(
            fd.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            &info,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if result < 0 {
        let source = io::Error::last_os_error();
        return if retry(&source) {
            Ok(SendResult::Retry)
        } else {
            Err(Error::Io {
                operation: "send",
                source,
            })
        };
    }
    if result as usize != bytes.len() {
        return Err(Error::PartialSend);
    }
    Ok(SendResult::Sent)
}

pub(crate) fn receive(fd: &OwnedFd, bytes: &mut [u8]) -> Result<Read, Error> {
    let mut info = Info::default();
    let mut flags = libc::MSG_DONTWAIT;
    // All writable buffers are exclusive for the call. Null source-address arguments are allowed by sctp_recvmsg.
    let result = unsafe {
        sctp_recvmsg(
            fd.as_raw_fd(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut info,
            &mut flags,
        )
    };
    if result < 0 {
        let source = io::Error::last_os_error();
        return if retry(&source) {
            Ok(Read::Idle)
        } else {
            Err(Error::Io {
                operation: "receive",
                source,
            })
        };
    }
    if result == 0 {
        return Ok(Read::Closed);
    }
    if flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(Error::Truncated);
    }
    let length = usize::try_from(result).map_err(|_| Error::Truncated)?;
    if length > bytes.len() {
        return Err(Error::Truncated);
    }
    let end = flags & libc::MSG_EOR != 0;
    if flags & MSG_NOTIFICATION != 0 {
        if !end {
            return Err(Error::Truncated);
        }
        return crate::records::notification(&bytes[..length]).map(Read::Notification);
    }
    if info.association == 0 {
        return Err(Error::MissingMetadata);
    }
    if info.flags & SCTP_UNORDERED != 0 {
        return Err(Error::Unordered);
    }
    Ok(Read::Data {
        metadata: Metadata {
            stream: info.stream,
            sequence: info.sequence,
            ppid: u32::from_be(info.ppid),
            association: info.association,
        },
        length,
        end,
    })
}

const _: () = {
    assert!(size_of::<Info>() == 32);
    assert!(std::mem::offset_of!(Info, ppid) == 8);
    assert!(std::mem::offset_of!(Info, association) == 28);
    assert!(size_of::<Init>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Association, Receive};
    use std::{
        net::{Ipv4Addr, SocketAddrV4},
        num::{NonZeroU16, NonZeroUsize},
        time::{Duration, Instant},
    };

    fn listener(port: u16, ip: Ipv4Addr) -> (OwnedFd, SocketAddr) {
        // Every address is initialized and correctly sized. Each successful socket is immediately owned for panic-safe cleanup.
        unsafe {
            let raw = libc::socket(
                libc::AF_INET,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::IPPROTO_SCTP,
            );
            assert!(raw >= 0, "SCTP socket: {}", io::Error::last_os_error());
            let fd = OwnedFd::from_raw_fd(raw);
            let mut address = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: port.to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(ip.octets()),
                },
                sin_zero: [0; 8],
            };
            assert_eq!(
                libc::bind(
                    raw,
                    std::ptr::from_ref(&address).cast(),
                    size_of_val(&address) as libc::socklen_t
                ),
                0
            );
            assert_eq!(libc::listen(raw, 1), 0);
            let mut length = size_of_val(&address) as libc::socklen_t;
            assert_eq!(
                libc::getsockname(raw, std::ptr::from_mut(&mut address).cast(), &mut length),
                0
            );
            (
                fd,
                SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be(address.sin_port))),
            )
        }
    }
    fn before(deadline: Instant) {
        assert!(
            Instant::now() < deadline,
            "Bounded SCTP loopback operation timed out"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    #[test]
    fn real_kernel_records_preserve_ppid_stream_eor_and_backpressure() {
        record_roundtrip(crate::Backend::Native, 0);
    }

    #[cfg(feature = "usrsctp")]
    #[test]
    #[ignore = "requires root and scoped kernel ABORT suppression on loopback SCTP destination port 29201"]
    fn real_userspace_to_kernel_records_and_shutdown() {
        const INTEROP_PORT: u16 = 29201;
        let (listener, address) = listener(INTEROP_PORT, Ipv4Addr::new(127, 0, 0, 10));
        for _ in 0..3 {
            record_on_listener(crate::Backend::Usrsctp { udp: None }, &listener, address);
        }
    }

    fn record_roundtrip(backend: crate::Backend, port: u16) {
        // Distinct loopback addresses avoid the kernel's same-address checksum-offload shortcut on raw receive.
        let ip = if matches!(backend, crate::Backend::Usrsctp { .. }) {
            Ipv4Addr::new(127, 0, 0, 10)
        } else {
            Ipv4Addr::LOCALHOST
        };
        let (listener, address) = listener(port, ip);
        record_on_listener(backend, &listener, address);
    }

    fn record_on_listener(backend: crate::Backend, listener: &OwnedFd, address: SocketAddr) {
        const PPID_TO_CLIENT: u32 = 20;
        const PPID_TO_SERVER: u32 = 19;
        let limits = Limits {
            maximum_record_bytes: NonZeroUsize::new(4096).unwrap(),
            receive_chunk_bytes: NonZeroUsize::new(128).unwrap(),
            outbound_streams: NonZeroU16::new(2).unwrap(),
            inbound_streams: NonZeroU16::new(2).unwrap(),
        };
        let mut client = Association::connect_with(backend, address, limits).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let server = loop {
            client.complete_connect().unwrap();
            // accept4 returns a new exclusive descriptor. Null address outputs are permitted by the socket API.
            let raw = unsafe {
                libc::accept4(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                )
            };
            if raw >= 0 {
                // This successful accept4 descriptor has not been transferred to another owner.
                break unsafe { OwnedFd::from_raw_fd(raw) };
            }
            assert!(retry(&io::Error::last_os_error()));
            before(deadline);
        };
        while !client.complete_connect().unwrap() {
            before(deadline);
        }
        option(
            &server,
            SCTP_EVENTS,
            &[1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        let payload: Vec<u8> = (0..1500).map(|index| (index % 251) as u8).collect();
        while send(&server, 1, PPID_TO_CLIENT, &payload).unwrap() == SendResult::Retry {
            before(deadline);
        }
        let mut fragments = 0;
        loop {
            match client.receive_once().unwrap() {
                Receive::Notification(notification) => {
                    assert!(!notification.invalidates_association());
                    client.acknowledge_receive().unwrap();
                }
                Receive::Record { metadata, bytes } => {
                    assert_eq!(metadata.stream, 1);
                    assert_eq!(metadata.ppid, PPID_TO_CLIENT);
                    assert_eq!(bytes, payload);
                    break;
                }
                Receive::Fragment => fragments += 1,
                Receive::Idle => before(deadline),
                Receive::Closed => panic!("Peer closed during record reassembly"),
            }
        }
        assert!(fragments > 0);
        assert!(
            matches!(client.receive_once().unwrap(), Receive::Record { bytes, .. } if bytes == payload)
        );
        client.acknowledge_receive().unwrap();
        while client.send(0, PPID_TO_SERVER, &[1, 2, 3]).unwrap() == SendResult::Retry {
            before(deadline);
        }
        let mut buffer = [0; 128];
        loop {
            match receive(&server, &mut buffer).unwrap() {
                Read::Idle => before(deadline),
                Read::Notification(_) => {}
                Read::Data {
                    metadata,
                    length,
                    end,
                } => {
                    assert_eq!(metadata.stream, 0);
                    assert_eq!(metadata.ppid, PPID_TO_SERVER);
                    assert!(end);
                    assert_eq!(&buffer[..length], &[1, 2, 3]);
                    break;
                }
                Read::Closed => panic!("Peer closed before reverse transfer"),
            }
        }
        drop(server);
        loop {
            match client.receive_once().unwrap() {
                Receive::Notification(notification) if notification.invalidates_association() => {
                    break;
                }
                Receive::Closed => break,
                Receive::Notification(_) => client.acknowledge_receive().unwrap(),
                Receive::Idle => before(deadline),
                event => panic!("Unexpected shutdown event {event:?}"),
            }
        }
        assert_eq!(client.state(), crate::State::Failed);
        assert!(matches!(
            client.send(0, PPID_TO_SERVER, &[1]),
            Err(Error::State)
        ));
    }
}
