// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::integrity::{AcceptedUplink, DownlinkIntegrity, StreamError};
use crate::integrity::{IntegritySchedule, ScheduleError};
use umts_rrc_codec::{EncodedRrc, decode_uplink_dedicated};

use super::{
    CoreConnection, CoreNas, DownlinkPriority, ForwardedNas, NasError, NasPublishError, NasRouter,
    SignallingBearer, TRANSACTION_COUNT, TransactionId, downlink_nas_message, index,
    inspect_message,
};

const OCTET_BITS: usize = u8::BITS as usize;

#[derive(Debug, PartialEq, Eq)]
pub enum NasProtectedError<E> {
    Nas(NasError),
    Integrity(StreamError<E>),
}
#[derive(Debug, PartialEq, Eq)]
pub enum NasScheduledError<E> {
    Nas(NasError),
    Schedule(ScheduleError<E>),
}

impl NasRouter {
    pub fn release_scheduled_with<T, E>(
        &mut self,
        connection: CoreConnection,
        schedule: &mut IntegritySchedule,
        publish: impl FnOnce(SignallingBearer, &EncodedRrc) -> Result<T, E>,
    ) -> Result<T, NasScheduledError<E>> {
        if connection.key.ue != self.key
            || self.links[index(connection.key.domain)] != Some(connection)
        {
            return Err(NasScheduledError::Nas(NasError::Context));
        }
        let transaction = TransactionId::new(self.transaction)
            .map_err(|_| NasScheduledError::Nas(NasError::Message))?;
        let message =
            super::signalling_connection_release_message(transaction, connection.key.domain);
        let bearer = SignallingBearer::ControlAm;
        if self.security_required
            && schedule
                .downlink_origin(self.key, bearer.identity())
                .map_err(|error| NasScheduledError::Schedule(ScheduleError::Schedule(error)))?
                .is_none()
        {
            return Err(NasScheduledError::Nas(NasError::SecurityRequired));
        }
        let result = schedule
            .publish_with(self.key, bearer.identity(), &message, |encoded| {
                publish(bearer, encoded)
            })
            .map_err(NasScheduledError::Schedule)?;
        self.links[index(connection.key.domain)] = None;
        self.transaction = (self.transaction + 1) % TRANSACTION_COUNT;
        Ok(result)
    }
    pub fn downlink_scheduled_with<T, E>(
        &mut self,
        connection: CoreConnection,
        priority: DownlinkPriority,
        nas: &[u8],
        schedule: &mut IntegritySchedule,
        publish: impl FnOnce(SignallingBearer, &EncodedRrc) -> Result<T, E>,
    ) -> Result<T, NasScheduledError<E>> {
        let fail = NasScheduledError::Nas;
        if connection.key.ue != self.key {
            return Err(fail(NasError::Origin));
        }
        if self.links[index(connection.key.domain)] != Some(connection) {
            return Err(fail(NasError::Context));
        }
        let bearer = priority.bearer(self.fourth_available);
        if self.security_required
            && schedule
                .downlink_origin(self.key, bearer.identity())
                .map_err(|error| NasScheduledError::Schedule(ScheduleError::Schedule(error)))?
                .is_none()
        {
            return Err(fail(NasError::SecurityRequired));
        }
        let transaction =
            TransactionId::new(self.transaction).map_err(|_| fail(NasError::Message))?;
        let message =
            downlink_nas_message(transaction, connection.key.domain, nas).map_err(fail)?;
        let result = schedule
            .publish_with(self.key, bearer.identity(), &message, |encoded| {
                publish(bearer, encoded)
            })
            .map_err(NasScheduledError::Schedule)?;
        self.transaction = (self.transaction + 1) % TRANSACTION_COUNT;
        Ok(result)
    }
    pub fn forward_authenticated_with<T, E>(
        &mut self,
        accepted: AcceptedUplink<'_>,
        publish: impl FnOnce(CoreNas<'_>) -> Result<T, E>,
    ) -> Result<(ForwardedNas, T), NasPublishError<E>> {
        let fail = NasPublishError::Procedure;
        let origin = accepted.origin();
        if origin.ue != self.key {
            return Err(fail(NasError::Origin));
        }
        let bearer = match accepted.bearer() {
            3 => SignallingBearer::NasAm,
            4 => SignallingBearer::LowPriorityNasAm,
            _ => return Err(fail(NasError::Bearer)),
        };
        let message =
            decode_uplink_dedicated(accepted.bytes(), accepted.bytes().len() * OCTET_BITS)
                .map_err(|error| fail(NasError::Codec(error)))?;
        let inspected = inspect_message(bearer, &message, self.fourth_available).map_err(fail)?;
        // TS 33.102 6.5.5 selects shared signaling keys independently of the NAS message's CN domain.
        self.forward_admitted(
            origin.ue,
            inspected.domain,
            inspected.nas,
            inspected.initial,
            publish,
        )
    }

    pub fn downlink_authenticated_with<T, E>(
        &mut self,
        connection: CoreConnection,
        priority: DownlinkPriority,
        nas: &[u8],
        integrity: &mut DownlinkIntegrity,
        publish: impl FnOnce(SignallingBearer, &EncodedRrc) -> Result<T, E>,
    ) -> Result<T, NasProtectedError<E>> {
        let fail = NasProtectedError::Nas;
        if connection.key.ue != self.key || integrity.origin().ue != self.key {
            return Err(fail(NasError::Origin));
        }
        if self.links[index(connection.key.domain)] != Some(connection) {
            return Err(fail(NasError::Context));
        }
        let bearer = priority.bearer(self.fourth_available);
        if integrity.bearer() != bearer.identity() {
            return Err(fail(NasError::Bearer));
        }
        let transaction =
            TransactionId::new(self.transaction).map_err(|_| fail(NasError::Message))?;
        let message =
            downlink_nas_message(transaction, connection.key.domain, nas).map_err(fail)?;
        let result = integrity
            .publish_with(integrity.origin(), &message, |protected| {
                publish(bearer, protected)
            })
            .map_err(NasProtectedError::Integrity)?;
        self.transaction = (self.transaction + 1) % TRANSACTION_COUNT;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
