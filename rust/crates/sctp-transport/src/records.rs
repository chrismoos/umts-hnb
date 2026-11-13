// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::Error;
use std::num::NonZeroUsize;
use zeroize::Zeroize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub stream: u16,
    pub sequence: u16,
    pub ppid: u32,
    pub association: i32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notification {
    Established {
        inbound_streams: u16,
        outbound_streams: u16,
        association: i32,
    },
    Lost {
        state: u16,
        error: u16,
        association: i32,
    },
    Shutdown {
        association: i32,
    },
    PartialDeliveryAborted {
        association: i32,
    },
    SendFailed,
    Unknown {
        kind: u16,
    },
}
impl Notification {
    pub fn invalidates_association(self) -> bool {
        !matches!(self, Self::Established { .. } | Self::Unknown { .. })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pending {
    Record(Metadata),
    Notification(Notification),
    Closed,
}
pub(crate) struct Assembler {
    bytes: Vec<u8>,
    maximum: usize,
    identity: Option<Metadata>,
    pending: Option<Pending>,
}
impl Assembler {
    pub(crate) fn new(maximum: NonZeroUsize) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(maximum.get())
            .map_err(|_| Error::Capacity)?;
        Ok(Self {
            bytes,
            maximum: maximum.get(),
            identity: None,
            pending: None,
        })
    }
    pub(crate) fn pending(&self) -> Option<Pending> {
        self.pending
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(crate) fn set_pending(&mut self, pending: Pending) {
        self.pending = Some(pending);
    }
    pub(crate) fn append(
        &mut self,
        metadata: Metadata,
        bytes: &[u8],
        end: bool,
    ) -> Result<(), Error> {
        if self.pending.is_some() {
            return Err(Error::State);
        }
        if bytes.is_empty() || self.identity.is_some_and(|identity| identity != metadata) {
            return Err(Error::RecordBoundary);
        }
        let length = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(Error::Capacity)?;
        if length > self.maximum {
            return Err(Error::Capacity);
        }
        self.bytes.extend_from_slice(bytes);
        self.identity = Some(metadata);
        if end {
            self.pending = Some(Pending::Record(metadata));
        }
        Ok(())
    }
    pub(crate) fn acknowledge(&mut self) -> Result<(), Error> {
        match self.pending.take().ok_or(Error::State)? {
            Pending::Record(_) | Pending::Closed => {
                self.bytes.as_mut_slice().zeroize();
                self.bytes.clear();
                self.identity = None;
            }
            Pending::Notification(notification) if notification.invalidates_association() => {
                self.bytes.as_mut_slice().zeroize();
                self.bytes.clear();
                self.identity = None;
            }
            Pending::Notification(_) => {}
        }
        Ok(())
    }
}

impl Drop for Assembler {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn notification(bytes: &[u8]) -> Result<Notification, Error> {
    const HEADER_BYTES: usize = 8;
    const ASSOCIATION_CHANGE: u16 = 0x8001;
    const SEND_FAILED: u16 = 0x8003;
    const SHUTDOWN: u16 = 0x8005;
    const PARTIAL_DELIVERY: u16 = 0x8006;
    const SEND_FAILED_EVENT: u16 = 0x800d;
    const COMM_UP: u16 = 0;
    const COMM_LOST: u16 = 1;
    const RESTART: u16 = 2;
    const SHUTDOWN_COMPLETE: u16 = 3;
    const CANNOT_START: u16 = 4;
    const KIND_OFFSET: usize = 0;
    const LENGTH_OFFSET: usize = 4;
    const STATE_OFFSET: usize = 8;
    const ERROR_OFFSET: usize = 10;
    const OUTBOUND_OFFSET: usize = 12;
    const INBOUND_OFFSET: usize = 14;
    const ASSOCIATION_OFFSET: usize = 16;
    const SHUTDOWN_ASSOCIATION_OFFSET: usize = 8;
    const PARTIAL_INDICATION_OFFSET: usize = 8;
    const PARTIAL_ASSOCIATION_OFFSET: usize = 12;
    const PARTIAL_EVENT_BYTES: usize = 24;
    const SEND_FAILED_BYTES: usize = 48;
    const SEND_FAILED_EVENT_BYTES: usize = 32;
    let u16_at = |offset: usize| -> Result<u16, Error> {
        Ok(u16::from_ne_bytes(
            bytes
                .get(offset..offset + 2)
                .ok_or(Error::Notification)?
                .try_into()
                .map_err(|_| Error::Notification)?,
        ))
    };
    let u32_at = |offset: usize| -> Result<u32, Error> {
        Ok(u32::from_ne_bytes(
            bytes
                .get(offset..offset + 4)
                .ok_or(Error::Notification)?
                .try_into()
                .map_err(|_| Error::Notification)?,
        ))
    };
    if bytes.len() < HEADER_BYTES
        || usize::try_from(u32_at(LENGTH_OFFSET)?).map_err(|_| Error::Notification)? != bytes.len()
    {
        return Err(Error::Notification);
    }
    Ok(match u16_at(KIND_OFFSET)? {
        ASSOCIATION_CHANGE => {
            let state = u16_at(STATE_OFFSET)?;
            let error = u16_at(ERROR_OFFSET)?;
            let association = u32_at(ASSOCIATION_OFFSET)? as i32;
            match state {
                COMM_UP => {
                    let outbound_streams = u16_at(OUTBOUND_OFFSET)?;
                    let inbound_streams = u16_at(INBOUND_OFFSET)?;
                    if outbound_streams == 0 || inbound_streams == 0 {
                        return Err(Error::Notification);
                    }
                    Notification::Established {
                        inbound_streams,
                        outbound_streams,
                        association,
                    }
                }
                COMM_LOST | RESTART | SHUTDOWN_COMPLETE | CANNOT_START => Notification::Lost {
                    state,
                    error,
                    association,
                },
                _ => return Err(Error::Notification),
            }
        }
        SHUTDOWN => Notification::Shutdown {
            association: u32_at(SHUTDOWN_ASSOCIATION_OFFSET)? as i32,
        },
        PARTIAL_DELIVERY => {
            const ABORTED: u32 = 0;
            if u32_at(PARTIAL_INDICATION_OFFSET)? != ABORTED || bytes.len() < PARTIAL_EVENT_BYTES {
                return Err(Error::Notification);
            }
            Notification::PartialDeliveryAborted {
                association: u32_at(PARTIAL_ASSOCIATION_OFFSET)? as i32,
            }
        }
        SEND_FAILED | SEND_FAILED_EVENT => {
            let minimum = if u16_at(KIND_OFFSET)? == SEND_FAILED {
                SEND_FAILED_BYTES
            } else {
                SEND_FAILED_EVENT_BYTES
            };
            if bytes.len() < minimum {
                return Err(Error::Notification);
            }
            Notification::SendFailed
        }
        kind => Notification::Unknown { kind },
    })
}
