// SPDX-License-Identifier: AGPL-3.0-or-later

mod gateway;
mod queue;

pub use gateway::{Counters, Gateway, LossReason, Phase, RabCommitError, Step};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    os::fd::{AsFd, BorrowedFd},
};
use umts_sctp::{Association, Receive, SendResult};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub outbound_entries: NonZeroUsize,
    pub maximum_packet_bytes: NonZeroUsize,
    pub outbound_byte_budget: NonZeroUsize,
    pub connect_timeout_ms: NonZeroU64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Capacity,
    Association,
    Protocol,
    State,
    Time,
    Session(umts_iuh::Error),
}
#[derive(Debug)]
pub enum DriveError<T, U> {
    Owner(Error),
    Transport(T),
    Upper(U),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity => f.write_str("Core signaling resource budget exceeded"),
            Self::Association => {
                f.write_str("Core signaling association generation does not match")
            }
            Self::Protocol => f.write_str("Unsupported core signaling payload protocol"),
            Self::State => {
                f.write_str("Core signaling command is incompatible with the owner phase")
            }
            Self::Time => f.write_str("Core signaling control time is invalid or out of order"),
            Self::Session(error) => write!(f, "Core signaling session: {error}"),
        }
    }
}
impl std::error::Error for Error {}
impl<T: std::fmt::Display, U: std::fmt::Display> std::fmt::Display for DriveError<T, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owner(error) => error.fmt(f),
            Self::Transport(error) => write!(f, "Core signaling transport: {error}"),
            Self::Upper(error) => write!(f, "Core signaling upper admission: {error}"),
        }
    }
}

pub trait Transport {
    type Error;
    fn complete_connect(&mut self) -> Result<bool, Self::Error>;
    fn receive_once(&mut self) -> Result<Receive<'_>, Self::Error>;
    fn acknowledge_receive(&mut self) -> Result<(), Self::Error>;
    fn send(&mut self, stream: u16, ppid: u32, bytes: &[u8]) -> Result<SendResult, Self::Error>;
}
impl Transport for Association {
    type Error = umts_sctp::Error;
    fn complete_connect(&mut self) -> Result<bool, Self::Error> {
        self.complete_connect()
    }
    fn receive_once(&mut self) -> Result<Receive<'_>, Self::Error> {
        self.receive_once()
    }
    fn acknowledge_receive(&mut self) -> Result<(), Self::Error> {
        self.acknowledge_receive()
    }
    fn send(&mut self, stream: u16, ppid: u32, bytes: &[u8]) -> Result<SendResult, Self::Error> {
        self.send(stream, ppid, bytes)
    }
}

impl<T: Transport + AsFd> Gateway<T> {
    pub fn transport_fd(&self) -> Option<BorrowedFd<'_>> {
        self.transport().map(AsFd::as_fd)
    }
}

#[cfg(test)]
mod tests;
