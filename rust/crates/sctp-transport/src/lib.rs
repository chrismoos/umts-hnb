// SPDX-License-Identifier: AGPL-3.0-or-later

#![deny(unsafe_code)]

#[cfg(all(not(target_os = "linux"), not(feature = "usrsctp")))]
compile_error!("enable the usrsctp feature on hosts without Linux kernel SCTP");

#[cfg(target_os = "linux")]
mod native;
mod records;
mod socket;
#[cfg(feature = "usrsctp")]
mod userspace;

use records::{Assembler, Pending};
pub use records::{Metadata, Notification};
use socket::Socket;
use std::{
    io,
    net::SocketAddr,
    num::{NonZeroU16, NonZeroUsize},
};
use zeroize::Zeroize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connecting,
    Open,
    Failed,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Native,
    Usrsctp {
        udp: Option<UdpEncapsulation>,
    },
}
/// SCTP over UDP (RFC 6951). An absent local port selects a free one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpEncapsulation {
    pub local_port: Option<NonZeroU16>,
    pub remote_port: NonZeroU16,
}
impl Backend {
    pub fn validate(self) -> Result<(), Error> {
        let available = match self {
            Self::Native => cfg!(target_os = "linux"),
            Self::Usrsctp { .. } => cfg!(feature = "usrsctp"),
        };
        if available {
            Ok(())
        } else {
            Err(Error::BackendUnavailable(self))
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub maximum_record_bytes: NonZeroUsize,
    pub receive_chunk_bytes: NonZeroUsize,
    pub outbound_streams: std::num::NonZeroU16,
    pub inbound_streams: std::num::NonZeroU16,
}
#[derive(Debug)]
pub enum Error {
    BackendUnavailable(Backend),
    Connection(Notification),
    Configuration,
    State,
    Capacity,
    RecordBoundary,
    MissingMetadata,
    Unordered,
    Truncated,
    PartialSend,
    Notification,
    Io {
        operation: &'static str,
        source: io::Error,
    },
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "SCTP {operation}: {source}"),
            error => write!(f, "SCTP {error:?}"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendResult {
    Sent,
    Retry,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Receive<'a> {
    Idle,
    Fragment,
    Record { metadata: Metadata, bytes: &'a [u8] },
    Notification(Notification),
    Closed,
}
impl std::fmt::Debug for Receive<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => f.write_str("Idle"),
            Self::Fragment => f.write_str("Fragment"),
            Self::Closed => f.write_str("Closed"),
            Self::Notification(value) => f.debug_tuple("Notification").field(value).finish(),
            Self::Record { metadata, bytes } => f
                .debug_struct("Record")
                .field("metadata", metadata)
                .field("bytes", &bytes.len())
                .finish(),
        }
    }
}

pub struct Association {
    socket: Socket,
    state: State,
    limits: Limits,
    receive: Vec<u8>,
    assembler: Assembler,
    negotiated_outbound: Option<u16>,
}
impl Association {
    pub fn connect(peer: SocketAddr, limits: Limits) -> Result<Self, Error> {
        Self::connect_with(Backend::Native, peer, limits)
    }
    pub fn connect_with(backend: Backend, peer: SocketAddr, limits: Limits) -> Result<Self, Error> {
        Self::validate_limits(limits)?;
        let mut receive = Vec::new();
        receive
            .try_reserve_exact(limits.receive_chunk_bytes.get())
            .map_err(|_| Error::Capacity)?;
        receive.resize(limits.receive_chunk_bytes.get(), 0);
        let assembler = Assembler::new(limits.maximum_record_bytes)?;
        let (socket, connected) = Socket::connect(backend, peer, limits)?;
        Ok(Self {
            socket,
            state: if connected {
                State::Open
            } else {
                State::Connecting
            },
            limits,
            receive,
            assembler,
            negotiated_outbound: None,
        })
    }
    fn validate_limits(limits: Limits) -> Result<(), Error> {
        if limits.receive_chunk_bytes.get() < socket::MIN_NOTIFICATION_BYTES
            || limits.receive_chunk_bytes > limits.maximum_record_bytes
            || limits.maximum_record_bytes.get() > i32::MAX as usize
        {
            return Err(Error::Configuration);
        }
        Ok(())
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn complete_connect(&mut self) -> Result<bool, Error> {
        match self.state {
            State::Open => Ok(true),
            State::Failed => Err(Error::State),
            State::Connecting => match self.socket.connect_complete() {
                Ok(false) => Ok(false),
                Ok(true) => {
                    self.state = State::Open;
                    Ok(true)
                }
                Err(error) => {
                    self.state = State::Failed;
                    Err(error)
                }
            },
        }
    }
    pub fn send(&mut self, stream: u16, ppid: u32, bytes: &[u8]) -> Result<SendResult, Error> {
        if self.state != State::Open {
            return Err(Error::State);
        }
        let outbound_streams = self.negotiated_outbound.ok_or(Error::State)?;
        if stream >= outbound_streams {
            return Err(Error::Configuration);
        }
        if bytes.is_empty() || bytes.len() > self.limits.maximum_record_bytes.get() {
            return Err(Error::Capacity);
        }
        match self.socket.send(stream, ppid, bytes) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.state = State::Failed;
                Err(error)
            }
        }
    }
    // A completed record or notification remains visible until explicit acknowledgment after upper admission.
    pub fn receive_once(&mut self) -> Result<Receive<'_>, Error> {
        if self.assembler.pending().is_none() {
            if self.state != State::Open {
                return Err(Error::State);
            }
            let received = self.socket.receive(&mut self.receive);
            let result = match received {
                Ok(socket::Read::Idle) => return Ok(Receive::Idle),
                Ok(socket::Read::Closed) => {
                    self.state = State::Failed;
                    self.assembler.set_pending(Pending::Closed);
                    Ok(())
                }
                Ok(socket::Read::Notification(notification)) => {
                    if let Notification::Established {
                        outbound_streams, ..
                    } = notification
                    {
                        self.negotiated_outbound = Some(outbound_streams);
                    }
                    if notification.invalidates_association() {
                        self.state = State::Failed;
                    }
                    self.assembler
                        .set_pending(Pending::Notification(notification));
                    Ok(())
                }
                Ok(socket::Read::Data {
                    metadata,
                    length,
                    end,
                }) => {
                    let result = self
                        .assembler
                        .append(metadata, &self.receive[..length], end);
                    self.receive[..length].zeroize();
                    result
                }
                Err(error) => {
                    self.receive.as_mut_slice().zeroize();
                    Err(error)
                }
            };
            if let Err(error) = result {
                self.state = State::Failed;
                return Err(error);
            }
        }
        Ok(match self.assembler.pending() {
            None => Receive::Fragment,
            Some(Pending::Record(metadata)) => Receive::Record {
                metadata,
                bytes: self.assembler.bytes(),
            },
            Some(Pending::Notification(notification)) => Receive::Notification(notification),
            Some(Pending::Closed) => Receive::Closed,
        })
    }
    pub fn acknowledge_receive(&mut self) -> Result<(), Error> {
        self.assembler.acknowledge()
    }
}

impl Drop for Association {
    fn drop(&mut self) {
        self.receive.zeroize();
    }
}

#[cfg(test)]
mod tests;
