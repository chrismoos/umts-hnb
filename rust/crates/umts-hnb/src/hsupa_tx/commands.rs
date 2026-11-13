// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_types::RadioEpoch;

pub enum Command {
    Register {
        lease: HsupaUserLease,
        identity: ReceiveIdentity,
        interval: Tti,
    },
    Grant {
        identity: ReceiveIdentity,
        start: ChipInstant,
        grant: AbsoluteGrant,
        amplitude: f32,
    },
    Feedback {
        request: FeedbackRequest,
        amplitude: f32,
    },
    TakeCompletion,
    BeginRetirement(UeKey),
    FinishRetirement {
        ue: UeKey,
        air_now: ChipInstant,
    },
}

pub enum CommandOutcome {
    Registered {
        identity: ReceiveIdentity,
        result: Result<(), (Error, HsupaUserLease)>,
    },
    Queued {
        identity: ReceiveIdentity,
        result: Result<ControlId, Error>,
    },
    Completion(Result<Option<Completion>, Error>),
    RetirementStarted {
        ue: UeKey,
        result: Result<(), Error>,
    },
    RetirementFinished {
        ue: UeKey,
        result: Result<Option<HsupaUserLease>, Error>,
    },
}

impl Command {
    pub(crate) fn matches_epoch(&self, epoch: RadioEpoch) -> bool {
        match self {
            Self::Register {
                lease, identity, ..
            } => lease.key().epoch == epoch && identity.ue.epoch == epoch,
            Self::Grant {
                identity, start, ..
            } => identity.ue.epoch == epoch && start.epoch == epoch,
            Self::Feedback { request, .. } => {
                request.identity.ue.epoch == epoch && request.timing.start.epoch == epoch
            }
            Self::TakeCompletion => true,
            Self::BeginRetirement(ue) => ue.epoch == epoch,
            Self::FinishRetirement { ue, air_now } => ue.epoch == epoch && air_now.epoch == epoch,
        }
    }

    pub(crate) fn retained_bits(&self) -> usize {
        // Reserve the returned lease as well as outbound registration ownership.
        match self {
            Self::Register { .. } | Self::FinishRetirement { .. } => {
                size_of::<HsupaUserLease>() * u8::BITS as usize
            }
            _ => 0,
        }
    }

    pub(crate) fn execute(self, bank: Option<&mut TransmitBank>) -> CommandOutcome {
        match self {
            Self::Register {
                lease,
                identity,
                interval,
            } => {
                let result = match bank {
                    Some(bank) => bank.register(lease, identity, interval),
                    None => Err((Error::Unavailable, lease)),
                };
                CommandOutcome::Registered { identity, result }
            }
            Self::Grant {
                identity,
                start,
                grant,
                amplitude,
            } => CommandOutcome::Queued {
                identity,
                result: bank
                    .ok_or(Error::Unavailable)
                    .and_then(|bank| bank.queue_grant(identity, start, grant, amplitude)),
            },
            Self::Feedback { request, amplitude } => CommandOutcome::Queued {
                identity: request.identity,
                result: bank
                    .ok_or(Error::Unavailable)
                    .and_then(|bank| bank.queue_feedback(request, amplitude)),
            },
            Self::TakeCompletion => CommandOutcome::Completion(
                bank.ok_or(Error::Unavailable)
                    .map(TransmitBank::take_completed),
            ),
            Self::BeginRetirement(ue) => CommandOutcome::RetirementStarted {
                ue,
                result: bank
                    .ok_or(Error::Unavailable)
                    .and_then(|bank| bank.begin_retirement(ue)),
            },
            Self::FinishRetirement { ue, air_now } => CommandOutcome::RetirementFinished {
                ue,
                result: bank
                    .ok_or(Error::Unavailable)
                    .and_then(|bank| bank.take_retired(ue, air_now)),
            },
        }
    }
}
