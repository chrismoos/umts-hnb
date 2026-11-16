// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(unsafe_code)]

use crate::{Error, Limits, Metadata, Notification, SendResult, UdpEncapsulation, socket::Read};
use std::{
    io,
    mem::size_of,
    net::{IpAddr, SocketAddr},
    num::NonZeroU16,
    ptr::NonNull,
    sync::{Mutex, PoisonError},
};

fn routed_source(peer: SocketAddr) -> Result<SocketAddr, Error> {
    let route = std::net::UdpSocket::bind(if peer.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .map_err(|source| Error::Io {
        operation: "source address socket",
        source,
    })?;
    route.connect(peer).map_err(|source| Error::Io {
        operation: "source address route",
        source,
    })?;
    let mut local = route.local_addr().map_err(|source| Error::Io {
        operation: "source address",
        source,
    })?;
    local.set_port(0);
    Ok(local)
}

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all,
    unsafe_op_in_unsafe_fn
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

// None until usrsctp's one process-wide initialization, then its UDP port, or None for raw SCTP.
static INITIALIZED: Mutex<Option<Option<NonZeroU16>>> = Mutex::new(None);
const RAW_SCTP: u16 = 0;
const IGNORE_UNOWNED_PACKETS: u32 = 2;

pub(crate) struct Socket {
    handle: NonNull<ffi::socket>,
    established: Option<Notification>,
}

// usrsctp ignores a failed UDP bind and then silently drops every packet.
fn free_udp_port(requested: Option<NonZeroU16>) -> Result<NonZeroU16, Error> {
    let io = |source| Error::Io {
        operation: "SCTP UDP encapsulation port",
        source,
    };
    let probe = std::net::UdpSocket::bind((
        std::net::Ipv4Addr::UNSPECIFIED,
        requested.map_or(0, NonZeroU16::get),
    ))
    .map_err(io)?;
    let port = probe.local_addr().map_err(io)?.port();
    NonZeroU16::new(port).ok_or(Error::Configuration)
}

fn initialize(udp: Option<UdpEncapsulation>) -> Result<(), Error> {
    let mut initialized = INITIALIZED.lock().unwrap_or_else(PoisonError::into_inner);
    let local_udp_port = match (*initialized, udp) {
        (Some(None), None) => return Ok(()),
        (Some(Some(current)), Some(udp)) if udp.local_port.is_none_or(|port| port == current) => {
            return Ok(());
        }
        (Some(_), _) => return Err(Error::Configuration),
        (None, None) => None,
        (None, Some(udp)) => Some(free_udp_port(udp.local_port)?),
    };
    // usrsctp owns process-wide timer/raw-reader threads. Initialization must run once, never per association.
    unsafe {
        ffi::usrsctp_init(local_udp_port.map_or(RAW_SCTP, NonZeroU16::get), None, None);
        // Raw readers also see other SCTP owners' packets. Do not abort associations owned by the kernel.
        ffi::usrsctp_sysctl_set_sctp_blackhole(IGNORE_UNOWNED_PACKETS);
    }
    *initialized = Some(local_udp_port);
    Ok(())
}

impl Socket {
    pub(crate) fn connect(
        local: Option<IpAddr>,
        peer: SocketAddr,
        limits: Limits,
        udp: Option<UdpEncapsulation>,
    ) -> Result<Self, Error> {
        let domain = if peer.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        };
        if udp.is_none() {
            // Fail before process-wide initialization if raw sockets are unavailable or permission is missing.
            let probe = socket2::Socket::new(
                domain,
                socket2::Type::from(libc::SOCK_RAW),
                Some(socket2::Protocol::from(libc::IPPROTO_SCTP)),
            )
            .map_err(|source| Error::Io {
                operation: "raw SCTP socket (requires raw-socket privileges)",
                source,
            })?;
            drop(probe);
        }
        initialize(udp)?;
        let family = if peer.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        // Null callbacks select recvv ownership, so no callback can outlive the Rust socket owner.
        let handle = NonNull::new(unsafe {
            ffi::usrsctp_socket(
                family,
                libc::SOCK_STREAM,
                libc::IPPROTO_SCTP,
                None,
                None,
                0,
                std::ptr::null_mut(),
            )
        })
        .ok_or_else(|| error("usrsctp socket"))?;
        let socket = Self {
            handle,
            established: None,
        };
        // This handle is exclusively owned and closed on every subsequent error path.
        if unsafe { ffi::usrsctp_set_non_blocking(handle.as_ptr(), 1) } < 0 {
            return Err(error("usrsctp nonblocking"));
        }
        socket.option(ffi::SCTP_NODELAY, &1i32)?;
        socket.option(ffi::SCTP_FRAGMENT_INTERLEAVE, &0i32)?;
        socket.option(ffi::SCTP_RECVRCVINFO, &1i32)?;
        socket.option(
            ffi::SCTP_INITMSG,
            &ffi::sctp_initmsg {
                sinit_num_ostreams: limits.outbound_streams.get(),
                sinit_max_instreams: limits.inbound_streams.get(),
                ..Default::default()
            },
        )?;
        for kind in [
            ffi::SCTP_ASSOC_CHANGE,
            ffi::SCTP_SHUTDOWN_EVENT,
            ffi::SCTP_PARTIAL_DELIVERY_EVENT,
            ffi::SCTP_SEND_FAILED_EVENT,
        ] {
            socket.option(
                ffi::SCTP_EVENT,
                &ffi::sctp_event {
                    se_assoc_id: ffi::SCTP_FUTURE_ASSOC,
                    se_type: kind as u16,
                    se_on: 1,
                },
            )?;
        }
        if let Some(udp) = udp {
            socket.option(
                ffi::SCTP_REMOTE_UDP_ENCAPS_PORT,
                &ffi::sctp_udpencaps {
                    sue_assoc_id: ffi::SCTP_FUTURE_ASSOC,
                    sue_port: udp.remote_port.get().to_be(),
                    ..Default::default()
                },
            )?;
        }
        let local = match local {
            Some(local) => SocketAddr::new(local, 0),
            None => routed_source(peer)?,
        };
        socket.bind_address(local)?;
        let address = socket2::SockAddr::from(peer);
        // usrsctp copies the borrowed host sockaddr, including Darwin length and IPv6 scope fields.
        let result = unsafe {
            ffi::usrsctp_connect(
                handle.as_ptr(),
                address.as_ptr().cast_mut().cast(),
                address.len(),
            )
        };
        if result < 0
            && !matches!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::EINPROGRESS | libc::EALREADY | libc::EINTR)
            )
        {
            return Err(error("usrsctp connect"));
        }
        Ok(socket)
    }

    fn bind_address(&self, address: SocketAddr) -> Result<(), Error> {
        let address = socket2::SockAddr::from(address);
        // Binding copies this initialized address and does not retain the pointer.
        let result = unsafe {
            ffi::usrsctp_bind(
                self.handle.as_ptr(),
                address.as_ptr().cast_mut().cast(),
                address.len(),
            )
        };
        if result < 0 {
            return Err(error("usrsctp bind"));
        }
        Ok(())
    }

    fn option<T>(&self, name: u32, value: &T) -> Result<(), Error> {
        // Only initialized usrsctp ABI option structures are passed by private callers.
        if unsafe {
            ffi::usrsctp_setsockopt(
                self.handle.as_ptr(),
                libc::IPPROTO_SCTP,
                name as _,
                std::ptr::from_ref(value).cast(),
                size_of::<T>() as _,
            )
        } < 0
        {
            return Err(error("usrsctp setsockopt"));
        }
        Ok(())
    }

    pub(crate) fn connect_complete(&mut self) -> Result<bool, Error> {
        if self.established.is_some() {
            return Ok(true);
        }
        let mut bytes = [0u8; 512];
        match self.receive(&mut bytes)? {
            Read::Idle => Ok(false),
            Read::Notification(notification @ Notification::Established { .. }) => {
                self.established = Some(notification);
                Ok(true)
            }
            Read::Notification(notification) => Err(Error::Connection(notification)),
            _ => Err(Error::State),
        }
    }

    pub(crate) fn send(
        &mut self,
        stream: u16,
        ppid: u32,
        bytes: &[u8],
    ) -> Result<SendResult, Error> {
        let mut info = ffi::sctp_sndinfo {
            snd_sid: stream,
            snd_ppid: ppid.to_be(),
            ..Default::default()
        };
        // RFC 6458 PPID is network-order. Input storage remains borrowed until sendv returns.
        let result = unsafe {
            ffi::usrsctp_sendv(
                self.handle.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                std::ptr::null_mut(),
                0,
                std::ptr::from_mut(&mut info).cast(),
                size_of::<ffi::sctp_sndinfo>() as _,
                ffi::SCTP_SENDV_SNDINFO,
                0,
            )
        };
        if result < 0 {
            return if retry() {
                Ok(SendResult::Retry)
            } else {
                Err(error("usrsctp send"))
            };
        }
        if result as usize != bytes.len() {
            return Err(Error::PartialSend);
        }
        Ok(SendResult::Sent)
    }

    pub(crate) fn receive(&mut self, bytes: &mut [u8]) -> Result<Read, Error> {
        if let Some(notification) = self.established.take() {
            return Ok(Read::Notification(notification));
        }
        let mut info = ffi::sctp_rcvinfo::default();
        let mut info_length = size_of::<ffi::sctp_rcvinfo>() as libc::socklen_t;
        let mut info_type = 0;
        let mut flags = 0;
        // Socket is nonblocking. Output buffers are exclusive and sized for the requested recvv metadata.
        let result = unsafe {
            ffi::usrsctp_recvv(
                self.handle.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::from_mut(&mut info).cast(),
                &mut info_length,
                &mut info_type,
                &mut flags,
            )
        };
        if result < 0 {
            return if retry() {
                Ok(Read::Idle)
            } else {
                Err(error("usrsctp receive"))
            };
        }
        if result == 0 {
            return Ok(Read::Closed);
        }
        let length = usize::try_from(result).map_err(|_| Error::Truncated)?;
        if length > bytes.len() || flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(Error::Truncated);
        }
        let end = flags & libc::MSG_EOR != 0;
        if flags & ffi::MSG_NOTIFICATION as i32 != 0 {
            if !end {
                return Err(Error::Truncated);
            }
            return notification(&bytes[..length]).map(Read::Notification);
        }
        if info_type != ffi::SCTP_RECVV_RCVINFO
            || info_length as usize != size_of::<ffi::sctp_rcvinfo>()
            || info.rcv_assoc_id == 0
        {
            return Err(Error::MissingMetadata);
        }
        if info.rcv_flags & ffi::SCTP_UNORDERED as u16 != 0 {
            return Err(Error::Unordered);
        }
        Ok(Read::Data {
            metadata: Metadata {
                stream: info.rcv_sid,
                sequence: info.rcv_ssn,
                ppid: u32::from_be(info.rcv_ppid),
                association: info.rcv_assoc_id as i32,
            },
            length,
            end,
        })
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // No callback retains this handle, and close transfers outstanding protocol state back to usrsctp.
        unsafe { ffi::usrsctp_close(self.handle.as_ptr()) };
    }
}

fn error(operation: &'static str) -> Error {
    Error::Io {
        operation,
        source: io::Error::last_os_error(),
    }
}
fn retry() -> bool {
    matches!(
        io::Error::last_os_error().kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

fn notification(bytes: &[u8]) -> Result<Notification, Error> {
    const KIND: usize = std::mem::offset_of!(ffi::sctp_assoc_change, sac_type);
    const LENGTH: usize = std::mem::offset_of!(ffi::sctp_assoc_change, sac_length);
    let word = |at: usize| -> Result<u16, Error> {
        Ok(u16::from_ne_bytes(
            bytes
                .get(at..at + 2)
                .ok_or(Error::Notification)?
                .try_into()
                .unwrap(),
        ))
    };
    let dword = |at: usize| -> Result<u32, Error> {
        Ok(u32::from_ne_bytes(
            bytes
                .get(at..at + 4)
                .ok_or(Error::Notification)?
                .try_into()
                .unwrap(),
        ))
    };
    if dword(LENGTH)? as usize != bytes.len() {
        return Err(Error::Notification);
    }
    let kind = word(KIND)?;
    Ok(match u32::from(kind) {
        ffi::SCTP_ASSOC_CHANGE => {
            let state = word(std::mem::offset_of!(ffi::sctp_assoc_change, sac_state))?;
            let error = word(std::mem::offset_of!(ffi::sctp_assoc_change, sac_error))?;
            let outbound_streams = word(std::mem::offset_of!(
                ffi::sctp_assoc_change,
                sac_outbound_streams
            ))?;
            let inbound_streams = word(std::mem::offset_of!(
                ffi::sctp_assoc_change,
                sac_inbound_streams
            ))?;
            let association =
                dword(std::mem::offset_of!(ffi::sctp_assoc_change, sac_assoc_id))? as i32;
            match u32::from(state) {
                ffi::SCTP_COMM_UP if inbound_streams > 0 && outbound_streams > 0 => {
                    Notification::Established {
                        inbound_streams,
                        outbound_streams,
                        association,
                    }
                }
                ffi::SCTP_COMM_LOST
                | ffi::SCTP_RESTART
                | ffi::SCTP_SHUTDOWN_COMP
                | ffi::SCTP_CANT_STR_ASSOC => Notification::Lost {
                    state,
                    error,
                    association,
                },
                _ => return Err(Error::Notification),
            }
        }
        ffi::SCTP_SHUTDOWN_EVENT => Notification::Shutdown {
            association: dword(std::mem::offset_of!(ffi::sctp_shutdown_event, sse_assoc_id))?
                as i32,
        },
        ffi::SCTP_PARTIAL_DELIVERY_EVENT => {
            if bytes.len() < size_of::<ffi::sctp_pdapi_event>()
                || dword(std::mem::offset_of!(
                    ffi::sctp_pdapi_event,
                    pdapi_indication
                ))? != ffi::SCTP_PARTIAL_DELIVERY_ABORTED
            {
                return Err(Error::Notification);
            }
            Notification::PartialDeliveryAborted {
                association: dword(std::mem::offset_of!(ffi::sctp_pdapi_event, pdapi_assoc_id))?
                    as i32,
            }
        }
        ffi::SCTP_SEND_FAILED_EVENT => {
            if bytes.len() < size_of::<ffi::sctp_send_failed_event>() {
                return Err(Error::Notification);
            }
            Notification::SendFailed
        }
        _ => Notification::Unknown { kind },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userspace_notifications_are_not_linux_uapi_notifications() {
        let mut bytes = Vec::new();
        for value in [1u16, 0] {
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        bytes.extend_from_slice(&20u32.to_ne_bytes());
        for value in [1u16, 0, 8, 4] {
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        bytes.extend_from_slice(&42u32.to_ne_bytes());
        assert_eq!(
            notification(&bytes).unwrap(),
            Notification::Established {
                inbound_streams: 4,
                outbound_streams: 8,
                association: 42
            }
        );
        assert!(matches!(
            crate::records::notification(&bytes).unwrap(),
            Notification::Unknown { kind: 1 }
        ));
        bytes[8..10].copy_from_slice(&3u16.to_ne_bytes());
        let restart = notification(&bytes).unwrap();
        assert!(matches!(
            restart,
            Notification::Lost {
                state: 3,
                association: 42,
                ..
            }
        ));
        assert!(restart.invalidates_association());
        bytes.truncate(19);
        assert!(matches!(notification(&bytes), Err(Error::Notification)));
    }
}
