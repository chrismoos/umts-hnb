// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    Context, SignallingError, am_admission_error,
    protection::{Protection, connection_error},
};
use crate::connection_security::{Preparation, RadioPlan, SecurityTime};
use umts_iuh::Domain;
use umts_rrc::{
    establishment::TransactionId,
    security::{
        ActivationSequences, BearerLayout, Capabilities, Command, CommandParameters,
        IntegrityActivation, IntegrityChange, SignallingSet,
    },
};
use umts_security::{ciphering::Position, counters::CipherMode};
use umts_types::{Direction, UeKey};

pub(super) fn prepare(
    context: &mut Context,
    key: UeKey,
    domain: Domain,
    now: SecurityTime,
    transaction: TransactionId,
    capabilities: &Capabilities,
) -> Result<Preparation, SignallingError> {
    Protection::retire_delivery(context, key)?;
    if context.bearer_change.is_some()
        || context.protection.blocks_security_reconfiguration()
        || context.control.pending_new_data()
        || context.control.pending_receive_data()
        || context
            .control
            .status_deferral(key)
            .map_err(SignallingError::Am)?
            .is_some()
        || context.capabilities.phase()
            == umts_rrc::capabilities::ExchangePhase::AwaitingInformation
    {
        return Err(SignallingError::Capacity);
    }
    Protection::collect_keys(context, key)?;
    let layout =
        BearerLayout::new(SignallingSet::Base, &[]).map_err(SignallingError::SecurityProcedure)?;
    let active = [Domain::Cs, Domain::Ps]
        .into_iter()
        .any(|domain| context.security.active_origin(domain).is_some());
    let integrity = if active {
        let mut sequences = [0; 4];
        for (bearer, sequence) in sequences.iter_mut().enumerate() {
            *sequence = context
                .protection
                .integrity
                .next_downlink_activation(key, bearer as u8)
                .map_err(SignallingError::Integrity)?;
        }
        IntegrityChange::Modify(
            IntegrityActivation::new(SignallingSet::Base, &sequences)
                .map_err(SignallingError::SecurityProcedure)?,
        )
    } else {
        IntegrityChange::Start {
            fresh: context.security.fresh(),
        }
    };
    let transmit = [
        context.um.transmit_position(),
        context.control.window_positions().next_transmit,
        context.nas_bearer.window_positions().next_transmit,
    ];
    let bank = &context.protection.ciphers;
    let control = &context.control;
    context
        .security
        .prepare_using(domain, now, capabilities, |origin, selection| {
            let cipher = if selection.chosen_encryption().is_some() {
                let mut entries = [(1, 0), (2, 0), (3, 0)];
                for ((bearer, sequence), next) in entries.iter_mut().zip(transmit) {
                    let mode = layout
                        .mode(*bearer)
                        .map_err(SignallingError::SecurityProcedure)?;
                    let pending = bank
                        .pending_sequence_activation(
                            key,
                            *bearer,
                            Direction::Downlink,
                            Position::Sequence(next),
                        )
                        .map_err(SignallingError::Cipher)?;
                    if *bearer == 2 && pending.is_some() {
                        return Err(SignallingError::Capacity);
                    }
                    let at = match pending {
                        Some(Position::Sequence(at)) => at,
                        None => next,
                        _ => return Err(SignallingError::Mapping),
                    };
                    *sequence = (at & ((1 << mode.sequence_bits()) - 1)) as u16;
                }
                let provisional = ActivationSequences::new(layout, &entries)
                    .map_err(SignallingError::SecurityProcedure)?;
                let make = |cipher| {
                    Command::new(
                        CommandParameters {
                            origin,
                            transaction,
                            layout,
                            selection,
                            integrity,
                            cipher: Some(cipher),
                            dpch_cfn: None,
                        },
                        capabilities,
                    )
                    .map_err(SignallingError::SecurityProcedure)
                };
                let bytes = make(provisional)?
                    .protected_octets()
                    .map_err(SignallingError::SecurityProcedure)?;
                let span = control
                    .isolated_sdu_range(key, bytes)
                    .map_err(am_admission_error)?;
                entries[1].1 =
                    (span.next & ((1 << CipherMode::Acknowledged.sequence_bits()) - 1)) as u16;
                let exact = ActivationSequences::new(layout, &entries)
                    .map_err(SignallingError::SecurityProcedure)?;
                if make(exact)?
                    .protected_octets()
                    .map_err(SignallingError::SecurityProcedure)?
                    != bytes
                {
                    return Err(SignallingError::ConfigurationFailure);
                }
                Some(exact)
            } else {
                None
            };
            Ok(RadioPlan {
                transaction,
                layout,
                integrity,
                cipher,
                dpch_cfn: None,
            })
        })
        .map_err(|error| connection_error(error, |error| error))
}
