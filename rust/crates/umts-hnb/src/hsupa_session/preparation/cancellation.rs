// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

impl Preparation {
    pub(in crate::hsupa_session) fn release_cancelled_resources(
        &mut self,
        resources: &mut crate::radio_resources::RadioResources,
    ) -> Result<bool, crate::radio_resources::Error> {
        if self.phase() != PreparationPhase::Cancelled {
            return Ok(false);
        }
        let Transmit::Local(lease) = std::mem::replace(&mut self.transmit, Transmit::Closed) else {
            unreachable!("canceled preparation owns its TX lease")
        };
        match resources.release_hsupa_user(lease) {
            Ok(()) => Ok(true),
            Err(failed) => {
                self.transmit = Transmit::Local(failed.lease);
                Err(failed.error)
            }
        }
    }

    pub(in crate::hsupa_session) fn begin_cancellation(&mut self) {
        self.cancelling = true;
        if matches!(self.receive, Receive::Rejected { .. }) {
            let Receive::Rejected { receiver, .. } =
                std::mem::replace(&mut self.receive, Receive::Closed)
            else {
                unreachable!()
            };
            self.receive = Receive::Local(receiver);
        }
        if matches!(self.transmit, Transmit::Rejected { .. }) {
            let Transmit::Rejected { lease, .. } =
                std::mem::replace(&mut self.transmit, Transmit::Closed)
            else {
                unreachable!()
            };
            self.transmit = Transmit::Local(lease);
        }
    }

    pub(in crate::hsupa_session) fn service_cancellation(
        &mut self,
        receive: &mut DedicatedReceivePort,
        transmit: &mut impl crate::hsupa_tx::Control,
        air_now: ChipInstant,
    ) {
        if !self.cancelling {
            return;
        }
        self.poll_receive();
        self.begin_cancellation();
        self.check_ports(receive, transmit);
        if self.phase() != PreparationPhase::Cancelling || air_now.epoch != self.identity.ue.epoch {
            return;
        }
        if matches!(self.receive, Receive::Attached)
            && let Ok(ticket) = receive.cancel_hsupa(self.identity, self.receive_activation)
        {
            self.receive = Receive::Cancelling(ticket);
        }
        // TX signatures remain reserved until RX ownership has actually returned.
        if !matches!(self.receive, Receive::Local(_)) {
            return;
        }
        let command = match self.transmit {
            Transmit::Registered => Command::BeginRetirement(self.identity.ue),
            Transmit::Retiring => Command::FinishRetirement {
                ue: self.identity.ue,
                air_now,
            },
            _ => return,
        };
        if let Ok(id) = transmit.queue_hsupa(command) {
            self.transmit = if matches!(self.transmit, Transmit::Registered) {
                Transmit::BeginRetirement(id)
            } else {
                Transmit::FinishRetirement(id)
            };
        }
    }

    #[cfg(test)]
    pub(in crate::hsupa_session) fn into_cancelled(
        self,
    ) -> Result<(HsupaAttachment, HsupaUserLease), Box<Self>> {
        if self.phase() != PreparationPhase::Cancelled {
            return Err(Box::new(self));
        }
        let Receive::Local(receiver) = self.receive else {
            unreachable!()
        };
        let Transmit::Local(lease) = self.transmit else {
            unreachable!()
        };
        Ok((receiver, lease))
    }
}
