// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{RegistrationRequest, SignallingError};
use std::num::NonZeroUsize;
use umts_iuh::{
    ranap::SecurityResponse,
    session::{IuReleaseRequest, SecurityRequest},
};
use umts_rrc::nas::{
    CoreConnection, CoreContextId, CoreNas, CoreNasKind, InitialNasMetadata, MAX_NAS_BYTES,
};
use umts_types::UeKey;

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub enum CoreEventKind<'a> {
    RrcRelease,
    SignallingRelease {
        connection: CoreConnection,
    },
    InactivityRelease {
        connection: CoreConnection,
    },
    RadioLostRelease {
        connection: CoreConnection,
    },
    SecurityResponse {
        request: SecurityRequest,
        response: SecurityResponse,
    },
    Register {
        request: RegistrationRequest,
    },
    Nas {
        kind: CoreNasKind,
        emergency: bool,
        connection: CoreConnection,
        initial: Option<&'a InitialNasMetadata>,
        nas: &'a [u8],
    },
    RadioFailure {
        context: Option<CoreContextId>,
        registration_pending: bool,
        connections: [Option<CoreConnection>; 2],
    },
}
pub struct CoreEvent<'a> {
    pub key: UeKey,
    pub kind: CoreEventKind<'a>,
}
enum OwnedCoreKind {
    RrcRelease,
    SignallingRelease {
        connection: CoreConnection,
    },
    InactivityRelease {
        connection: CoreConnection,
    },
    RadioLostRelease {
        connection: CoreConnection,
    },
    SecurityResponse {
        request: SecurityRequest,
        response: SecurityResponse,
    },
    Register {
        request: RegistrationRequest,
    },
    Nas {
        kind: CoreNasKind,
        emergency: bool,
        connection: CoreConnection,
        initial: Option<InitialNasMetadata>,
    },
    RadioFailure {
        context: Option<CoreContextId>,
        registration_pending: bool,
        connections: [Option<CoreConnection>; 2],
    },
}
struct CoreSlot {
    key: Option<UeKey>,
    kind: Option<OwnedCoreKind>,
    bytes: Vec<u8>,
    length: usize,
    cancelled: bool,
}
pub struct CoreOutbox {
    slots: Vec<CoreSlot>,
    releases: Vec<Option<ConnectionRelease>>,
    head: usize,
    length: usize,
}
#[derive(Clone, Copy)]
struct ConnectionRelease {
    request: IuReleaseRequest,
    notified: bool,
}
impl CoreOutbox {
    pub fn new(capacity: NonZeroUsize) -> Result<Self, SignallingError> {
        capacity
            .get()
            .checked_mul(MAX_NAS_BYTES)
            .ok_or(SignallingError::Capacity)?;
        Ok(Self {
            releases: vec![None; capacity.get()],
            slots: (0..capacity.get())
                .map(|_| CoreSlot {
                    key: None,
                    kind: None,
                    bytes: vec![0; MAX_NAS_BYTES],
                    length: 0,
                    cancelled: false,
                })
                .collect(),
            head: 0,
            length: 0,
        })
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.length
    }
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub(super) fn pending_for(&self, key: UeKey) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.key == Some(key) && !slot.cancelled)
    }
    pub(super) fn begin_connection_release(
        &mut self,
        request: IuReleaseRequest,
    ) -> Result<(), SignallingError> {
        if self
            .releases
            .iter()
            .flatten()
            .any(|entry| entry.request == request)
        {
            return Ok(());
        }
        if self.releases.iter().flatten().any(|entry| {
            entry.request.key() == request.key() && entry.request.domain() == request.domain()
        }) {
            return Err(SignallingError::Capacity);
        }
        let index = self
            .releases
            .iter()
            .position(Option::is_none)
            .ok_or(SignallingError::Capacity)?;
        self.cancel_connection(request.key(), request.domain().into());
        self.releases[index] = Some(ConnectionRelease {
            request,
            notified: false,
        });
        Ok(())
    }
    pub(super) fn connection_release_notified(&self, request: IuReleaseRequest) -> Option<bool> {
        self.releases
            .iter()
            .flatten()
            .find(|entry| entry.request == request)
            .map(|entry| entry.notified)
    }
    pub(super) fn connection_releasing(&self, key: UeKey, domain: umts_iuh::Domain) -> bool {
        self.releases
            .iter()
            .flatten()
            .any(|entry| entry.request.key() == key && entry.request.domain() == domain)
    }
    pub(super) fn mark_connection_release_notified(&mut self, request: IuReleaseRequest) {
        if let Some(entry) = self
            .releases
            .iter_mut()
            .flatten()
            .find(|entry| entry.request == request)
        {
            entry.notified = true;
        }
    }
    pub(super) fn finish_connection_release(&mut self, request: IuReleaseRequest) {
        for entry in &mut self.releases {
            if entry.as_ref().is_some_and(|entry| entry.request == request) {
                *entry = None;
            }
        }
    }
    pub(super) fn push_rrc_release(&mut self, key: UeKey) -> Result<(), SignallingError> {
        if self.length == self.slots.len() {
            return Err(SignallingError::Capacity);
        }
        self.cancel_for(key);
        let index = (self.head + self.length) % self.slots.len();
        self.slots[index].key = Some(key);
        self.slots[index].kind = Some(OwnedCoreKind::RrcRelease);
        self.slots[index].length = 0;
        self.length += 1;
        Ok(())
    }
    pub fn push_security_response(
        &mut self,
        request: SecurityRequest,
        response: SecurityResponse,
    ) -> Result<(), SignallingError> {
        if self.length == self.slots.len() {
            return Err(SignallingError::Capacity);
        }
        let count = self.slots.len();
        let slot = &mut self.slots[(self.head + self.length) % count];
        slot.key = Some(request.key());
        slot.length = 0;
        slot.kind = Some(OwnedCoreKind::SecurityResponse { request, response });
        self.length += 1;
        Ok(())
    }
    pub(crate) fn cancel_for(&mut self, key: UeKey) {
        for entry in &mut self.releases {
            if entry
                .as_ref()
                .is_some_and(|entry| entry.request.key() == key)
            {
                *entry = None;
            }
        }
        for slot in &mut self.slots {
            if slot.key == Some(key) {
                slot.cancelled = true;
            }
        }
    }
    pub(crate) fn cancel_connection(&mut self, key: UeKey, domain: umts_types::CnDomain) {
        for slot in &mut self.slots {
            if slot.key != Some(key) {
                continue;
            }
            if match &slot.kind {
                Some(
                    OwnedCoreKind::Nas { connection, .. }
                    | OwnedCoreKind::SignallingRelease { connection }
                    | OwnedCoreKind::InactivityRelease { connection }
                    | OwnedCoreKind::RadioLostRelease { connection },
                ) => connection.key.domain == domain,
                Some(OwnedCoreKind::SecurityResponse { request, .. }) => {
                    umts_types::CnDomain::from(request.domain()) == domain
                }
                _ => false,
            } {
                slot.cancelled = true;
            }
        }
    }
    pub(crate) fn clear(&mut self) {
        self.releases.fill(None);
        for slot in &mut self.slots {
            slot.bytes[..slot.length].fill(0);
            slot.key = None;
            slot.kind = None;
            slot.length = 0;
            slot.cancelled = false;
        }
        self.head = 0;
        self.length = 0;
    }
    pub(crate) fn push_registration(
        &mut self,
        key: UeKey,
        request: RegistrationRequest,
    ) -> Result<(), SignallingError> {
        if self.length == self.slots.len() {
            return Err(SignallingError::Capacity);
        }
        let count = self.slots.len();
        let slot = &mut self.slots[(self.head + self.length) % count];
        slot.key = Some(key);
        slot.length = 0;
        slot.kind = Some(OwnedCoreKind::Register { request });
        self.length += 1;
        Ok(())
    }
    pub(super) fn push_signalling_release(
        &mut self,
        connection: CoreConnection,
    ) -> Result<(), SignallingError> {
        if self.slots.iter().any(|slot| !slot.cancelled && matches!(slot.kind, Some(OwnedCoreKind::SignallingRelease { connection: queued }) if queued == connection)) {
            return Ok(());
        }
        if self.length == self.slots.len() {
            return Err(SignallingError::Capacity);
        }
        let index = (self.head + self.length) % self.slots.len();
        let slot = &mut self.slots[index];
        slot.key = Some(connection.key.ue);
        slot.length = 0;
        slot.kind = Some(OwnedCoreKind::SignallingRelease { connection });
        self.length += 1;
        Ok(())
    }
    pub(super) fn push_inactivity_release(
        &mut self,
        connection: CoreConnection,
    ) -> Result<(), SignallingError> {
        if self.slots.iter().any(|slot| !slot.cancelled && matches!(slot.kind, Some(OwnedCoreKind::InactivityRelease { connection: queued }) if queued == connection)) {
            return Ok(());
        }
        if self.length == self.slots.len() {
            return Err(SignallingError::Capacity);
        }
        let index = (self.head + self.length) % self.slots.len();
        let slot = &mut self.slots[index];
        slot.key = Some(connection.key.ue);
        slot.kind = Some(OwnedCoreKind::InactivityRelease { connection });
        slot.length = 0;
        self.length += 1;
        Ok(())
    }
    // TS 25.413 8.4.2: one Iu Release Request per lost connection, never repeated.
    pub(super) fn push_radio_lost_release(
        &mut self,
        connection: CoreConnection,
    ) -> Result<(), SignallingError> {
        if self.slots.iter().any(|slot| !slot.cancelled && matches!(slot.kind, Some(OwnedCoreKind::RadioLostRelease { connection: queued }) if queued == connection)) {
            return Ok(());
        }
        if self.length == self.slots.len() {
            return Err(SignallingError::Capacity);
        }
        let index = (self.head + self.length) % self.slots.len();
        let slot = &mut self.slots[index];
        slot.key = Some(connection.key.ue);
        slot.kind = Some(OwnedCoreKind::RadioLostRelease { connection });
        slot.length = 0;
        self.length += 1;
        Ok(())
    }
    pub(crate) fn push_nas(
        &mut self,
        event: CoreNas<'_>,
        emergency: bool,
    ) -> Result<(), SignallingError> {
        if self.releases.iter().flatten().any(|entry| {
            entry.request.key() == event.connection.key.ue
                && umts_types::CnDomain::from(entry.request.domain()) == event.connection.key.domain
        }) {
            return Err(SignallingError::Capacity);
        }
        if self.length == self.slots.len() {
            return Err(SignallingError::Capacity);
        }
        if event.nas.is_empty() || event.nas.len() > MAX_NAS_BYTES {
            return Err(SignallingError::Message);
        }
        let count = self.slots.len();
        let slot = &mut self.slots[(self.head + self.length) % count];
        slot.bytes[..event.nas.len()].copy_from_slice(event.nas);
        slot.length = event.nas.len();
        slot.key = Some(event.connection.key.ue);
        slot.kind = Some(OwnedCoreKind::Nas {
            kind: event.kind,
            emergency,
            connection: event.connection,
            initial: event.initial,
        });
        self.length += 1;
        Ok(())
    }
    pub(crate) fn push_radio_failure(
        &mut self,
        key: UeKey,
        context: Option<CoreContextId>,
        registration_pending: bool,
        connections: [Option<CoreConnection>; 2],
    ) -> Result<(), SignallingError> {
        self.push_radio_failure_with(key, context, registration_pending, connections, || Ok(()))
    }

    pub(super) fn push_radio_failure_with<T>(
        &mut self,
        key: UeKey,
        context: Option<CoreContextId>,
        registration_pending: bool,
        connections: [Option<CoreConnection>; 2],
        admit: impl FnOnce() -> Result<T, SignallingError>,
    ) -> Result<T, SignallingError> {
        let needed =
            context.is_some() || registration_pending || connections.iter().any(Option::is_some);
        if needed && self.length == self.slots.len() {
            return Err(SignallingError::Capacity);
        }
        let result = admit()?;
        self.cancel_for(key);
        if !needed {
            return Ok(result);
        }
        let count = self.slots.len();
        let slot = &mut self.slots[(self.head + self.length) % count];
        slot.length = 0;
        slot.key = Some(key);
        slot.kind = Some(OwnedCoreKind::RadioFailure {
            context,
            registration_pending,
            connections,
        });
        self.length += 1;
        Ok(result)
    }
    pub fn publish_one<T, E>(
        &mut self,
        publish: impl FnOnce(CoreEvent<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        if self.length == 0 {
            return Ok(None);
        }
        let slot = &mut self.slots[self.head];
        if slot.cancelled {
            slot.key = None;
            slot.kind = None;
            slot.bytes[..slot.length].fill(0);
            slot.length = 0;
            slot.cancelled = false;
            self.head = (self.head + 1) % self.slots.len();
            self.length -= 1;
            return Ok(None);
        }
        let kind = match slot.kind.as_ref().expect("occupied core event") {
            OwnedCoreKind::RrcRelease => CoreEventKind::RrcRelease,
            OwnedCoreKind::InactivityRelease { connection } => CoreEventKind::InactivityRelease {
                connection: *connection,
            },
            OwnedCoreKind::RadioLostRelease { connection } => CoreEventKind::RadioLostRelease {
                connection: *connection,
            },
            OwnedCoreKind::SignallingRelease { connection } => CoreEventKind::SignallingRelease {
                connection: *connection,
            },
            OwnedCoreKind::SecurityResponse { request, response } => {
                CoreEventKind::SecurityResponse {
                    request: *request,
                    response: *response,
                }
            }
            OwnedCoreKind::Register { request } => CoreEventKind::Register { request: *request },
            OwnedCoreKind::Nas {
                kind,
                emergency,
                connection,
                initial,
            } => CoreEventKind::Nas {
                kind: *kind,
                emergency: *emergency,
                connection: *connection,
                initial: initial.as_ref(),
                nas: &slot.bytes[..slot.length],
            },
            OwnedCoreKind::RadioFailure {
                context,
                registration_pending,
                connections,
            } => CoreEventKind::RadioFailure {
                context: *context,
                registration_pending: *registration_pending,
                connections: *connections,
            },
        };
        let result = publish(CoreEvent {
            key: slot.key.expect("occupied core owner"),
            kind,
        })?;
        slot.kind = None;
        slot.key = None;
        slot.bytes[..slot.length].fill(0);
        slot.length = 0;
        self.head = (self.head + 1) % self.slots.len();
        self.length -= 1;
        Ok(Some(result))
    }
}
