// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Context, SignallingError, ingress::DedicatedIngress};
use crate::{
    common_am::{AmPayloadPublishError, CommonAmPreparedError},
    common_tx::CommonTxTarget,
    common_um::{CommonUmPreparedError, CommonUmPreparedReceiveError, UmPayloadPublishError},
    connection_security::ConnectionSecurity,
};
use umts_rlc::acknowledged::{ReceivePreparation, ResetCounters, WindowPositions};
use umts_rrc::nas::SignallingBearer;
use umts_security::{
    ciphering::{
        BearerCipherUpdateError, BearerCiphers, CipherReset, CountAnchor, Position,
        ResetConfiguration,
    },
    counters::CipherMode,
};
use umts_types::{
    Direction, UeKey,
    bits::{BITS_PER_BYTE, Bits},
    time::ChipInstant,
};

pub(super) fn receive(
    context: &mut Context,
    ingress: &mut DedicatedIngress,
    key: UeKey,
    bearer: SignallingBearer,
    time: umts_rlc::acknowledged::ReceiveTime,
    payload: Bits<'_>,
) -> Result<(), SignallingError> {
    let (ciphers, mut receiver) = split_receiver(context, ingress, key);
    receiver.receive(ciphers, bearer, time, payload)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResetBlockers {
    pub bearer_change: bool,
    pub protection: bool,
    pub integrity: bool,
    pub cs_security: bool,
    pub ps_security: bool,
    pub ingress: bool,
    pub admission: bool,
}

impl ResetBlockers {
    fn any(self) -> bool {
        self.bearer_change
            || self.protection
            || self.integrity
            || self.cs_security
            || self.ps_security
            || self.ingress
            || self.admission
    }
}

pub(crate) struct SignallingReceiver<'a> {
    key: UeKey,
    um: &'a mut crate::common_um::CommonUmBearer,
    control: &'a mut crate::common_am::CommonAmBearer,
    nas: &'a mut crate::common_am::CommonAmBearer,
    ingress: &'a mut DedicatedIngress,
    security: &'a ConnectionSecurity,
    pending_cipher: Option<&'a super::protection::cipher::PendingCipher>,
    admission: &'a mut super::protection::ControlAdmission,
    blocked: [ResetBlockers; 2],
}

pub(super) fn split_receiver<'a>(
    context: &'a mut Context,
    ingress: &'a mut DedicatedIngress,
    key: UeKey,
) -> (&'a mut BearerCiphers, SignallingReceiver<'a>) {
    let blocked = [
        reset_blockers(context, SignallingBearer::ControlAm),
        reset_blockers(context, SignallingBearer::NasAm),
    ];
    (
        &mut context.protection.ciphers,
        SignallingReceiver {
            key,
            um: &mut context.um,
            control: &mut context.control,
            nas: &mut context.nas_bearer,
            ingress,
            security: &context.security,
            pending_cipher: context.protection.pending_cipher.as_ref(),
            admission: &mut context.protection.admission,
            blocked,
        },
    )
}

impl SignallingReceiver<'_> {
    pub(crate) fn receive(
        &mut self,
        ciphers: &mut BearerCiphers,
        bearer: SignallingBearer,
        time: umts_rlc::acknowledged::ReceiveTime,
        payload: Bits<'_>,
    ) -> Result<(), SignallingError> {
        let key = self.key;
        let ingress = &mut *self.ingress;
        let security = self.security;
        let mut blockers = match bearer {
            SignallingBearer::ControlAm => self.blocked[0],
            SignallingBearer::NasAm => self.blocked[1],
            _ => ResetBlockers::default(),
        };
        blockers.ingress = bearer == SignallingBearer::ControlAm && ingress.pending(key, bearer);
        blockers.admission = !matches!(self.admission, super::protection::ControlAdmission::Ready);
        if bearer == SignallingBearer::Unacknowledged {
            let received = self
                .um
                .receive_payload_prepared_admitted(
                    key,
                    payload,
                    |position, bytes| {
                        ciphers
                            .apply(
                                key,
                                bearer.identity(),
                                Direction::Uplink,
                                Position::Sequence(position),
                                bytes,
                                bytes.len() * BITS_PER_BYTE,
                            )
                            .map(|_| ())
                            .map_err(SignallingError::Cipher)
                    },
                    |count, bytes| ingress.admit(key, bearer, count, bytes),
                )
                .map_err(|error| match error {
                    CommonUmPreparedReceiveError::Bearer(error) => SignallingError::Um(error),
                    CommonUmPreparedReceiveError::Preparation(error)
                    | CommonUmPreparedReceiveError::Admission(error) => error,
                });
            let received = match received {
                Ok(received) => received,
                // TS 25.322 10.3 discards malformed PDUs without releasing the bearer.
                Err(SignallingError::Um(crate::common_um::CommonUmError::MalformedPdu(_))) => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            for sdu in received.sdus() {
                ingress.push_timed_admitted(
                    key,
                    bearer,
                    time.with_provenance(umts_rlc::acknowledged::ReceiveProvenance::Unspecified),
                    sdu,
                    None,
                );
            }
            return Ok(());
        }
        let am = match bearer {
            SignallingBearer::ControlAm => &mut *self.control,
            SignallingBearer::NasAm => &mut *self.nas,
            _ => return Err(SignallingError::Mapping),
        };
        let positions = am.window_positions();
        let pending_cipher = self.pending_cipher;
        let control_pending = ingress.pending(key, SignallingBearer::ControlAm)
            || (bearer == SignallingBearer::ControlAm
                && am
                    .status_deferral(key)
                    .map_err(SignallingError::Am)?
                    .is_some());
        let result = am.receive_payload_prepared_timed(key, time, payload, |preparation, bytes| {
            match preparation {
                ReceivePreparation::Data { position, .. } => {
                    // SRB2 may send new-cipher traffic without waiting for Complete's RLC acknowledgment.
                    if pending_cipher.is_some()
                        && bearer == SignallingBearer::ControlAm
                        && (control_pending || position != positions.next_receive)
                    {
                        return Err(SignallingError::Capacity);
                    }
                    ciphers
                        .apply(
                            key,
                            bearer.identity(),
                            Direction::Uplink,
                            Position::Sequence(position),
                            bytes,
                            bytes.len() * BITS_PER_BYTE,
                        )
                        .map(|_| None)
                        .map_err(SignallingError::Cipher)
                }
                ReceivePreparation::Control
                | ReceivePreparation::Reset {
                    transition: None, ..
                } => Ok(None),
                ReceivePreparation::Reset {
                    transition: Some(reset),
                    ..
                } => {
                    if blockers.any() {
                        return Err(SignallingError::CipherResetBlocked { bearer, blockers });
                    }
                    let [transmit, receive] =
                        reset_selection(ciphers, security, key, bearer, positions)?;
                    let mode = CipherMode::Acknowledged;
                    let transmit_count = transmit
                        .initial_count
                        .unwrap_or(reset.tx_hfn << mode.sequence_bits());
                    let receive_count = receive
                        .initial_count
                        .unwrap_or(reset.rx_hfn << mode.sequence_bits());
                    let counters = ResetCounters {
                        transmit_hfn: transmit
                            .initial_count
                            .map(|count| count >> mode.sequence_bits()),
                        receive_hfn: receive
                            .initial_count
                            .map(|count| count >> mode.sequence_bits()),
                    };
                    ciphers
                        .reset_with(
                            key,
                            CipherReset {
                                bearer: bearer.identity(),
                                origin: transmit.origin,
                                transmit: CountAnchor::new(
                                    mode,
                                    Position::Sequence(reset.tx_position),
                                    transmit_count,
                                )
                                .map_err(|error| SignallingError::Cipher(error.into()))?,
                                receive: CountAnchor::new(
                                    mode,
                                    Position::Sequence(reset.rx_position),
                                    receive_count,
                                )
                                .map_err(|error| SignallingError::Cipher(error.into()))?,
                            },
                            || Ok::<_, std::convert::Infallible>(Some(counters)),
                        )
                        .map_err(update_error)
                }
            }
        });
        match result {
            Ok(_) => Ok(()),
            Err(CommonAmPreparedError::Bearer(error)) => Err(SignallingError::Am(error)),
            Err(CommonAmPreparedError::Preparation(error)) => {
                if matches!(
                    error,
                    SignallingError::CipherResetRequired
                        | SignallingError::CipherResetBlocked { .. }
                ) && matches!(self.admission, super::protection::ControlAdmission::Ready)
                {
                    *self.admission =
                        super::protection::ControlAdmission::RecoveryRequired { rejected: None };
                }
                Err(error)
            }
        }
    }
}

fn reset_selection(
    ciphers: &BearerCiphers,
    security: &ConnectionSecurity,
    key: UeKey,
    bearer: SignallingBearer,
    positions: WindowPositions,
) -> Result<[ResetConfiguration; 2], SignallingError> {
    let mut selections = [ResetConfiguration {
        origin: None,
        initial_count: None,
    }; 2];
    for (slot, direction, next) in [
        (0, Direction::Downlink, positions.next_transmit),
        (1, Direction::Uplink, positions.next_receive),
    ] {
        let position = Position::Sequence(next);
        let selection = ciphers
            .reset_configuration(key, bearer.identity(), direction, position)
            .map_err(SignallingError::Cipher)?;
        if selection.initial_count.is_some()
            && selection
                .origin
                .is_none_or(|origin| security.active_origin(origin.domain.into()) != Some(origin))
        {
            return Err(SignallingError::CipherResetRequired);
        }
        selections[slot] = selection;
    }
    if selections[0].origin != selections[1].origin {
        return Err(SignallingError::CipherResetRequired);
    }
    Ok(selections)
}

fn reset_blockers(context: &Context, bearer: SignallingBearer) -> ResetBlockers {
    ResetBlockers {
        bearer_change: context.bearer_change.is_some(),
        protection: context.protection.blocks_reset(bearer),
        integrity: context.protection.integrity.pending_origin().is_some(),
        cs_security: context
            .security
            .pending_origin(umts_iuh::Domain::Cs)
            .is_some(),
        ps_security: context
            .security
            .pending_origin(umts_iuh::Domain::Ps)
            .is_some(),
        ..ResetBlockers::default()
    }
}

#[cfg(test)]
pub(super) fn initiate_reset(
    context: &mut Context,
    key: UeKey,
    bearer: SignallingBearer,
    now: ChipInstant,
) -> Result<(), SignallingError> {
    if reset_blockers(context, bearer).any() {
        return Err(SignallingError::Phase);
    }
    let am = match bearer {
        SignallingBearer::ControlAm => &mut context.control,
        SignallingBearer::NasAm => &mut context.nas_bearer,
        _ => return Err(SignallingError::Mapping),
    };
    reset_selection(
        &context.protection.ciphers,
        &context.security,
        key,
        bearer,
        am.window_positions(),
    )?;
    am.initiate_reset(key, now).map_err(SignallingError::Am)
}

pub(super) fn publish(
    context: &mut Context,
    key: UeKey,
    bearer: SignallingBearer,
    source: &mut dyn CommonTxTarget,
    now: ChipInstant,
    start: ChipInstant,
) -> Result<bool, SignallingError> {
    let protection = &context.protection;
    if bearer == SignallingBearer::Unacknowledged {
        return context
            .um
            .publish_frame_prepared(source, start, |publication, bytes| {
                protection.prepare_um_payload(key, publication, bytes)
            })
            .map_err(|error| match error {
                CommonUmPreparedError::Bearer(error) => SignallingError::Um(error),
                CommonUmPreparedError::Preparation(error) => error,
            });
    }
    let am = match bearer {
        SignallingBearer::ControlAm => &mut context.control,
        SignallingBearer::NasAm => &mut context.nas_bearer,
        _ => return Err(SignallingError::Mapping),
    };
    am.publish_frame_mapped(
        source,
        now,
        start,
        |position| protection.downlink_count(key, bearer, position),
        |publication, bytes| protection.prepare_am_payload(key, bearer, publication, bytes),
    )
    .map(|published| published.is_some())
    .map_err(|error| match error {
        CommonAmPreparedError::Bearer(error) => SignallingError::Am(error),
        CommonAmPreparedError::Preparation(error) => error,
    })
}

pub(super) fn publish_payload(
    context: &mut Context,
    key: UeKey,
    bearer: SignallingBearer,
    now: ChipInstant,
    pdu_bytes: std::num::NonZeroUsize,
    admit: impl FnOnce(&[u8]) -> Result<crate::tx_evidence::TransportSubmission, SignallingError>,
) -> Result<bool, SignallingError> {
    let protection = &context.protection;
    if bearer == SignallingBearer::Unacknowledged {
        return context
            .um
            .publish_payload_prepared(
                key,
                pdu_bytes,
                |publication, bytes| protection.prepare_um_payload(key, publication, bytes),
                |bytes| admit(bytes).map(|_| ()),
            )
            .map_err(|error| match error {
                UmPayloadPublishError::Bearer(error) => SignallingError::Um(error),
                UmPayloadPublishError::Preparation(error)
                | UmPayloadPublishError::Admission(error) => error,
            });
    }
    let am = match bearer {
        SignallingBearer::ControlAm => &mut context.control,
        SignallingBearer::NasAm => &mut context.nas_bearer,
        _ => return Err(SignallingError::Mapping),
    };
    if pdu_bytes < am.transmit_pdu_bytes() {
        return Err(SignallingError::Capacity);
    }
    let published = am
        .publish_payload_mapped(
            key,
            now,
            |position| protection.downlink_count(key, bearer, position),
            |publication, bytes| protection.prepare_am_payload(key, bearer, publication, bytes),
            admit,
        )
        .map_err(|error| match error {
            AmPayloadPublishError::Bearer(error) => SignallingError::Am(error),
            AmPayloadPublishError::Preparation(error) | AmPayloadPublishError::Admission(error) => {
                error
            }
        })?;
    Ok(published.is_some())
}
pub(super) fn update_error(
    error: BearerCipherUpdateError<std::convert::Infallible>,
) -> SignallingError {
    match error {
        BearerCipherUpdateError::Cipher(error) => SignallingError::Cipher(error),
        BearerCipherUpdateError::Publication(never) => match never {},
    }
}

#[cfg(test)]
mod tests;
