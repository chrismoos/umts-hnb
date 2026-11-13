// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::acknowledged::{AMD_HEADER_BYTES, status::ACK_BITS};

pub(super) struct StatusConfirmation {
    required_next: u64,
    publication: Option<(u64, StatusCoverage)>,
}

impl AmEntity {
    pub fn request_status_confirmation(
        &mut self,
        received: ReceiveCoverage,
    ) -> Result<(), AmError> {
        if received.owner != self.receipt_owner
            || received.generation != self.receive_generation
            || received.last >= self.rx_next
            || self.state() != AmState::Ready
        {
            return Err(AmError::State);
        }
        if self
            .acknowledged_status
            .is_some_and(|status| status.covers(received))
        {
            return Ok(());
        }
        let required_next = received.last + 1;
        match self.status_confirmation.as_mut() {
            Some(pending) if pending.required_next >= required_next => (),
            Some(pending) => {
                pending.required_next = required_next;
                if pending
                    .publication
                    .is_some_and(|(_, status)| status.next < required_next)
                {
                    pending.publication = None;
                }
            }
            None => {
                self.status_confirmation = Some(StatusConfirmation {
                    required_next,
                    publication: None,
                });
            }
        }
        Ok(())
    }

    /// A peer ACK of the containing AMD PDU proves receipt of its piggybacked STATUS.
    pub fn acknowledged_status(&self) -> Option<StatusCoverage> {
        (self.state() == AmState::Ready)
            .then_some(self.acknowledged_status)
            .flatten()
    }

    pub(super) fn confirmation_prohibited(&self, now: ChipInstant) -> bool {
        // TS 25.322 9.7.2 also prohibits ACK SUFIs in a newly piggybacked report.
        self.status_confirmation
            .as_ref()
            .is_some_and(|pending| pending.publication.is_none())
            && self
                .status_prohibit
                .is_some_and(|deadline| now.index < deadline)
    }

    pub(super) fn piggyback_status_confirmation(
        &mut self,
        absolute: u64,
        new: bool,
    ) -> Result<Option<StatusCoverage>, AmError> {
        let Some(pending) = &self.status_confirmation else {
            return Ok(None);
        };
        if let Some((position, coverage)) = pending.publication {
            return Ok((!new && position == absolute).then_some(coverage));
        }
        if !new || self.status_deferral().is_some() || self.rx_next < pending.required_next {
            return Ok(None);
        }
        let width = LiWidth::for_fixed_size(self.scratch.len());
        let pdu = parse_amd(&self.scratch, width, self.config.accept_special_he)?;
        let indicators = pdu.indicators();
        if indicators.last() != Some(&Indicator::Padding) {
            return Ok(None);
        }
        let padding = AMD_HEADER_BYTES + indicators.len() * width.octets() + pdu.data.len();
        let indicator = AMD_HEADER_BYTES + (indicators.len() - 1) * width.octets();
        if (self.scratch.len() - padding) * 8 < CONTROL_HEADER_BITS + ACK_BITS {
            return Ok(None);
        }
        // TS 25.322 9.2.2.8 replaces only the final padding LI, preserving every SDU boundary.
        let word = Indicator::Status.word(width) << 1;
        match width {
            LiWidth::Seven => self.scratch[indicator] = word as u8,
            LiWidth::Fifteen => {
                self.scratch[indicator..indicator + 2].copy_from_slice(&word.to_be_bytes())
            }
        }
        self.scratch[padding..].fill(0);
        Writer::new(&mut self.scratch[padding..]).ack(sn(self.rx_next))?;
        Ok(Some(StatusCoverage {
            owner: self.receipt_owner,
            generation: self.receive_generation,
            next: self.rx_next,
        }))
    }

    pub(super) fn record_status_confirmation(&mut self, position: u64, coverage: StatusCoverage) {
        let pending = self
            .status_confirmation
            .as_mut()
            .expect("requested status confirmation");
        pending.publication.get_or_insert((position, coverage));
        self.published_status = Some(coverage);
    }

    pub(super) fn observe_status_confirmation(&mut self) {
        let Some((position, coverage)) = self
            .status_confirmation
            .as_ref()
            .and_then(|pending| pending.publication)
        else {
            return;
        };
        let slot = &self.tx[(position % self.tx.len() as u64) as usize];
        if slot.sequence == Some(position) && slot.acknowledged {
            self.acknowledged_status = Some(coverage);
            self.status_confirmation = None;
        }
    }
}
