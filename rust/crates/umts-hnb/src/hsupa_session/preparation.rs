// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_rx::{
        DedicatedReceivePort, HsupaAttachment, ReceiveCommandResult, ReceiveCommandTicket,
        ReceiveError, ReceiveIdentity,
    },
    dedicated_tx::{TransmitCommandId, TransmitCommandOutcome, TransmitEvent},
    hsupa_rx::TrackedReceiver,
    hsupa_tx::{Command, CommandOutcome},
    radio_resources::HsupaUserLease,
};
use std::{num::NonZeroUsize, sync::Arc};
use umts_rrc::hsupa::UplinkActivation;
use umts_types::time::ChipInstant;

mod cancellation;

pub struct PreparationInputs {
    pub receiver: HsupaAttachment,
    pub lease: HsupaUserLease,
    pub configuration: Arc<UplinkActivation>,
    pub maximum_receive_bytes: NonZeroUsize,
}

pub struct RejectedPreparation {
    pub inputs: PreparationInputs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationPhase {
    Preparing,
    Ready,
    Cancelling,
    Cancelled,
    RecoveryRequired,
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
enum Receive {
    Local(HsupaAttachment),
    Pending(ReceiveCommandTicket),
    Cancelling(ReceiveCommandTicket),
    Attached,
    Rejected {
        receiver: HsupaAttachment,
        error: ReceiveError,
    },
    Unexpected(Box<ReceiveCommandResult>),
    Closed,
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
enum Transmit {
    Local(HsupaUserLease),
    Pending(TransmitCommandId),
    Registered,
    BeginRetirement(TransmitCommandId),
    Retiring,
    FinishRetirement(TransmitCommandId),
    Rejected {
        lease: HsupaUserLease,
        error: crate::hsupa_tx::Error,
    },
    Unexpected(Box<TransmitEvent>),
    Closed,
}

pub struct Preparation {
    identity: ReceiveIdentity,
    receive_activation: ChipInstant,
    configuration: Arc<UplinkActivation>,
    maximum_receive_bytes: NonZeroUsize,
    receive: Receive,
    transmit: Transmit,
    cancelling: bool,
    unavailable: bool,
}

impl Preparation {
    pub(in crate::hsupa_session) fn ready_for_ue_release(&self) -> bool {
        !self.unavailable
            && matches!(self.receive, Receive::Attached)
            && matches!(self.transmit, Transmit::Registered)
    }

    pub fn new(inputs: PreparationInputs) -> Result<Self, Box<RejectedPreparation>> {
        let identity = inputs.receiver.identity();
        let receive_activation = match super::timing::receive_activation(
            inputs.configuration.activation(),
            inputs.receiver.receiver().anchor(),
        ) {
            Ok(time) => time,
            Err(_) => return Err(Box::new(RejectedPreparation { inputs })),
        };
        let parameters = inputs.configuration.parameters();
        let actual = inputs.lease.controls();
        let expected = parameters.downlink;
        let relative = |control: umts_rrc::hsupa::DownlinkControl| {
            control
                .relative()
                .map(|grant| (grant.signature(), grant.combination()))
        };
        if inputs.lease.key() != identity.ue
            || inputs.lease.e_rnti() != parameters.e_rnti
            || inputs.receiver.receiver().interval() != parameters.transport.interval()
            || inputs.receiver.receiver().is_started()
            || !inputs.receiver.receiver().has_output()
            || inputs.configuration.activation().time().epoch != identity.ue.epoch
            || actual.absolute_code() != expected.absolute_code()
            || actual.indicator_code() != expected.indicator_code()
            || actual.acknowledgement_signature() != expected.acknowledgement_signature()
            || relative(actual) != relative(expected)
        {
            return Err(Box::new(RejectedPreparation { inputs }));
        }
        Ok(Self {
            identity,
            receive_activation,
            configuration: inputs.configuration,
            maximum_receive_bytes: inputs.maximum_receive_bytes,
            receive: Receive::Local(inputs.receiver),
            transmit: Transmit::Local(inputs.lease),
            cancelling: false,
            unavailable: false,
        })
    }

    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    pub(super) fn receive_activation(&self) -> ChipInstant {
        self.receive_activation
    }
    #[cfg(test)]
    pub fn configuration(&self) -> &Arc<UplinkActivation> {
        &self.configuration
    }

    pub fn phase(&self) -> PreparationPhase {
        if self.cancelling
            && matches!(self.receive, Receive::Local(_))
            && matches!(self.transmit, Transmit::Local(_))
        {
            return PreparationPhase::Cancelled;
        }
        if matches!(
            self.receive,
            Receive::Rejected { .. } | Receive::Unexpected(_) | Receive::Closed
        ) || matches!(
            self.transmit,
            Transmit::Rejected { .. } | Transmit::Unexpected(_) | Transmit::Closed
        ) || self.unavailable
        {
            PreparationPhase::RecoveryRequired
        } else if self.cancelling {
            PreparationPhase::Cancelling
        } else if matches!(self.receive, Receive::Attached)
            && matches!(self.transmit, Transmit::Registered)
        {
            PreparationPhase::Ready
        } else {
            PreparationPhase::Preparing
        }
    }

    pub fn service(
        &mut self,
        receive: &mut DedicatedReceivePort,
        transmit: &mut impl crate::hsupa_tx::Control,
    ) {
        self.poll_receive();
        self.check_ports(receive, transmit);
        if self.phase() != PreparationPhase::Preparing {
            return;
        }
        if matches!(self.transmit, Transmit::Local(_)) {
            let Transmit::Local(lease) = std::mem::replace(&mut self.transmit, Transmit::Closed)
            else {
                unreachable!()
            };
            self.transmit = match transmit.queue_hsupa(Command::Register {
                lease,
                identity: self.identity,
                interval: self.configuration.parameters().transport.interval(),
            }) {
                Ok(id) => Transmit::Pending(id),
                Err(Command::Register { lease, .. }) => Transmit::Local(lease),
                Err(_) => unreachable!("registration returns its original lease"),
            };
        }
        // Register TX first so RX cannot produce reports before its indicator owner exists.
        if matches!(self.transmit, Transmit::Registered)
            && matches!(self.receive, Receive::Local(_))
        {
            let Receive::Local(receiver) = std::mem::replace(&mut self.receive, Receive::Closed)
            else {
                unreachable!()
            };
            self.receive = match receive.attach_hsupa_at(
                receiver,
                self.receive_activation,
                self.maximum_receive_bytes,
            ) {
                Ok(ticket) => Receive::Pending(ticket),
                Err(receiver) => Receive::Local(receiver),
            };
        }
    }

    pub(in crate::hsupa_session) fn poll_receive(&mut self) {
        let (ticket, cancelling) = match &mut self.receive {
            Receive::Pending(ticket) => (ticket, false),
            Receive::Cancelling(ticket) => (ticket, true),
            _ => return,
        };
        let Some(result) = ticket.try_result() else {
            return;
        };
        self.receive = match result {
            ReceiveCommandResult::HsupaAttached {
                identity,
                activation,
            } if !cancelling
                && identity == self.identity
                && activation == self.receive_activation =>
            {
                Receive::Attached
            }
            ReceiveCommandResult::HsupaRejected {
                error,
                mut receiver,
            } if !cancelling && receiver.identity() == self.identity => {
                if matches!(
                    &error,
                    ReceiveError::Capacity
                        | ReceiveError::ResourceBudget {
                            resource: crate::dedicated_rx::ReceiveResource::PhysicalSampleBytes,
                            ..
                        }
                ) && receiver.discard_initial_suffixes()
                {
                    Receive::Local(receiver)
                } else {
                    Receive::Rejected { receiver, error }
                }
            }
            ReceiveCommandResult::HsupaCancelled {
                identity,
                activation,
                receiver,
            } if cancelling
                && identity == self.identity
                && receiver.identity() == self.identity
                && activation == self.receive_activation =>
            {
                Receive::Local(receiver)
            }
            ReceiveCommandResult::Closed => Receive::Closed,
            other => Receive::Unexpected(Box::new(other)),
        };
    }

    pub fn route_command(&mut self, event: TransmitEvent) -> Option<TransmitEvent> {
        let id = match self.transmit {
            Transmit::Pending(id)
            | Transmit::BeginRetirement(id)
            | Transmit::FinishRetirement(id) => id,
            _ => return Some(event),
        };
        if id != event.id {
            return Some(event);
        }
        self.transmit = match event.outcome {
            TransmitCommandOutcome::Hsupa(CommandOutcome::Registered { identity, result })
                if matches!(self.transmit, Transmit::Pending(_)) && identity == self.identity =>
            {
                match result {
                    Ok(()) => Transmit::Registered,
                    Err((error, lease)) => Transmit::Rejected { lease, error },
                }
            }
            TransmitCommandOutcome::Hsupa(CommandOutcome::RetirementStarted {
                ue,
                result: Ok(()),
            }) if matches!(self.transmit, Transmit::BeginRetirement(_))
                && ue == self.identity.ue =>
            {
                Transmit::Retiring
            }
            TransmitCommandOutcome::Hsupa(CommandOutcome::RetirementFinished {
                ue,
                result: Ok(None),
            }) if matches!(self.transmit, Transmit::FinishRetirement(_))
                && ue == self.identity.ue =>
            {
                Transmit::Retiring
            }
            TransmitCommandOutcome::Hsupa(CommandOutcome::RetirementFinished {
                ue,
                result: Ok(Some(lease)),
            }) if matches!(self.transmit, Transmit::FinishRetirement(_))
                && ue == self.identity.ue
                && lease.key() == ue
                && lease.e_rnti() == self.configuration.parameters().e_rnti =>
            {
                Transmit::Local(lease)
            }
            outcome => Transmit::Unexpected(Box::new(TransmitEvent {
                id: event.id,
                outcome,
            })),
        };
        None
    }

    fn check_ports(
        &mut self,
        receive: &DedicatedReceivePort,
        transmit: &impl crate::hsupa_tx::Control,
    ) {
        self.unavailable |= !receive.available()
            || !transmit.available()
            || receive.epoch() != self.identity.ue.epoch
            || transmit.epoch() != self.identity.ue.epoch;
    }

    pub fn rejected_receiver(&self) -> Option<(&TrackedReceiver, &ReceiveError)> {
        match &self.receive {
            Receive::Rejected { receiver, error } => Some((receiver.receiver(), error)),
            _ => None,
        }
    }
    pub fn rejected_transmitter(&self) -> Option<(&HsupaUserLease, crate::hsupa_tx::Error)> {
        match &self.transmit {
            Transmit::Rejected { lease, error } => Some((lease, *error)),
            _ => None,
        }
    }
}
