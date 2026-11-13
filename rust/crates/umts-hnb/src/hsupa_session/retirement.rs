// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_rx::{DedicatedSampleReceiver, ReceiveIdentity},
    dedicated_tx::{TransmitCommandId, TransmitCommandOutcome, TransmitEvent},
    hsupa_tx::{Command, CommandOutcome, Control},
    radio_resources::HsupaUserLease,
};
use umts_types::time::ChipInstant;

#[cfg(test)]
mod tests;

enum Stage {
    Start,
    Starting(TransmitCommandId),
    Draining,
    Finishing(TransmitCommandId),
    Retired(HsupaUserLease),
    Released,
    Failed(Box<TransmitEvent>),
}

pub(super) struct Retirement {
    identity: ReceiveIdentity,
    e_rnti: u16,
    receiver: Option<Box<DedicatedSampleReceiver>>,
    component: Option<crate::dedicated_rx::HsupaAttachment>,
    stage: Stage,
}

impl Retirement {
    pub(super) fn new(
        identity: ReceiveIdentity,
        e_rnti: u16,
        receiver: Box<DedicatedSampleReceiver>,
    ) -> Self {
        Self {
            identity,
            e_rnti,
            receiver: Some(receiver),
            component: None,
            stage: Stage::Start,
        }
    }

    /// The sample receiver is already gone, so only the transmit side still holds state.
    pub(super) fn orphaned(identity: ReceiveIdentity, e_rnti: u16) -> Self {
        Self {
            identity,
            e_rnti,
            receiver: None,
            component: None,
            stage: Stage::Start,
        }
    }

    pub(super) fn restored(
        identity: ReceiveIdentity,
        e_rnti: u16,
        receiver: crate::dedicated_rx::HsupaAttachment,
    ) -> Self {
        Self {
            identity,
            e_rnti,
            receiver: None,
            component: Some(receiver),
            stage: Stage::Start,
        }
    }

    pub(super) fn phase(&self) -> super::Phase {
        match self.stage {
            Stage::Retired(_) | Stage::Released => super::Phase::Retired,
            Stage::Failed(_) => super::Phase::RecoveryRequired,
            _ => super::Phase::Retiring,
        }
    }

    pub(super) fn retain_component(&mut self, component: crate::dedicated_rx::HsupaAttachment) {
        assert!(
            self.component.is_none(),
            "retirement already owns an E-DCH component"
        );
        self.component = Some(component);
    }

    pub(super) fn failure(&self) -> Option<&TransmitEvent> {
        match &self.stage {
            Stage::Failed(event) => Some(event),
            _ => None,
        }
    }

    pub(super) fn service(
        &mut self,
        reports_empty: bool,
        controls_empty: bool,
        now: ChipInstant,
        transmit: &mut impl Control,
    ) {
        if now.epoch != self.identity.ue.epoch
            || transmit.epoch() != now.epoch
            || !transmit.available()
        {
            return;
        }
        if reports_empty {
            self.receiver.take();
            self.component.take();
        }
        if self.receiver.is_some() || self.component.is_some() {
            return;
        }
        match self.stage {
            Stage::Start => {
                if let Ok(id) = transmit.queue_hsupa(Command::BeginRetirement(self.identity.ue)) {
                    self.stage = Stage::Starting(id);
                }
            }
            Stage::Draining if controls_empty => {
                if let Ok(id) = transmit.queue_hsupa(Command::FinishRetirement {
                    ue: self.identity.ue,
                    air_now: now,
                }) {
                    self.stage = Stage::Finishing(id);
                }
            }
            _ => (),
        }
    }

    pub(super) fn route(&mut self, event: TransmitEvent) -> Option<TransmitEvent> {
        let (id, finishing) = match self.stage {
            Stage::Starting(id) => (id, false),
            Stage::Finishing(id) => (id, true),
            _ => return Some(event),
        };
        if event.id != id {
            return Some(event);
        }
        let valid = match &event.outcome {
            TransmitCommandOutcome::Hsupa(CommandOutcome::RetirementStarted { ue, result }) => {
                !finishing && *ue == self.identity.ue && result.is_ok()
            }
            TransmitCommandOutcome::Hsupa(CommandOutcome::RetirementFinished { ue, result }) => {
                finishing
                    && *ue == self.identity.ue
                    && result.as_ref().is_ok_and(|lease| {
                        lease.as_ref().is_none_or(|lease| {
                            lease.key() == self.identity.ue && lease.e_rnti() == self.e_rnti
                        })
                    })
            }
            _ => false,
        };
        if !valid {
            self.stage = Stage::Failed(Box::new(event));
            return None;
        }
        self.stage = match event.outcome {
            TransmitCommandOutcome::Hsupa(CommandOutcome::RetirementFinished {
                result: Ok(Some(lease)),
                ..
            }) => Stage::Retired(lease),
            _ => Stage::Draining,
        };
        None
    }

    #[cfg(test)]
    pub(super) fn into_lease(self) -> Result<HsupaUserLease, Box<Self>> {
        match self.stage {
            Stage::Retired(lease) => Ok(lease),
            _ => Err(Box::new(self)),
        }
    }

    pub(super) fn release_resources(
        &mut self,
        resources: &mut crate::radio_resources::RadioResources,
    ) -> Result<bool, crate::radio_resources::Error> {
        let stage = std::mem::replace(&mut self.stage, Stage::Released);
        match stage {
            Stage::Retired(lease) => match resources.release_hsupa_user(lease) {
                Ok(()) => Ok(true),
                Err(failed) => {
                    self.stage = Stage::Retired(failed.lease);
                    Err(failed.error)
                }
            },
            Stage::Released => Ok(true),
            other => {
                self.stage = other;
                Ok(false)
            }
        }
    }
}
