// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{Backend, Error, Limits, Metadata, Notification, SendResult};
use std::net::{IpAddr, SocketAddr};

pub(crate) const MIN_NOTIFICATION_BYTES: usize = 32;

pub(crate) enum Read {
    Idle,
    Closed,
    Notification(Notification),
    Data {
        metadata: Metadata,
        length: usize,
        end: bool,
    },
}

pub(crate) enum Socket {
    #[cfg(target_os = "linux")]
    Native(std::os::fd::OwnedFd),
    #[cfg(feature = "usrsctp")]
    Userspace(crate::userspace::Socket),
}

impl Socket {
    pub(crate) fn connect(
        backend: Backend,
        local: Option<IpAddr>,
        peer: SocketAddr,
        limits: Limits,
    ) -> Result<(Self, bool), Error> {
        match backend {
            #[cfg(target_os = "linux")]
            Backend::Native => crate::native::connect(local, peer, limits)
                .map(|(socket, open)| (Self::Native(socket), open)),
            #[cfg(feature = "usrsctp")]
            Backend::Usrsctp { udp } => crate::userspace::Socket::connect(local, peer, limits, udp)
                .map(|socket| (Self::Userspace(socket), false)),
            #[allow(unreachable_patterns)]
            unavailable => {
                let _ = (local, peer, limits);
                Err(Error::BackendUnavailable(unavailable))
            }
        }
    }

    pub(crate) fn connect_complete(&mut self) -> Result<bool, Error> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Native(socket) => crate::native::connect_complete(socket),
            #[cfg(feature = "usrsctp")]
            Self::Userspace(socket) => socket.connect_complete(),
        }
    }

    pub(crate) fn send(
        &mut self,
        stream: u16,
        ppid: u32,
        bytes: &[u8],
    ) -> Result<SendResult, Error> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Native(socket) => crate::native::send(socket, stream, ppid, bytes),
            #[cfg(feature = "usrsctp")]
            Self::Userspace(socket) => socket.send(stream, ppid, bytes),
        }
    }

    pub(crate) fn receive(&mut self, bytes: &mut [u8]) -> Result<Read, Error> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Native(socket) => crate::native::receive(socket, bytes),
            #[cfg(feature = "usrsctp")]
            Self::Userspace(socket) => socket.receive(bytes),
        }
    }
}
