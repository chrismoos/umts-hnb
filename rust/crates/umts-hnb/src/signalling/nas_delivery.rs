// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rlc::acknowledged::{SduReceipt, SduTransmission};

#[derive(Clone, Copy)]
struct Delivery {
    connection: CoreConnection,
    receipt: SduReceipt,
}

pub(super) struct NasDeliveries {
    slots: Vec<Option<Delivery>>,
}

impl NasDeliveries {
    pub(super) fn buffer_bytes(capacity: NonZeroUsize) -> Option<usize> {
        capacity
            .get()
            .checked_mul(std::mem::size_of::<Option<Delivery>>())?
            .checked_add(std::mem::size_of::<Self>())
    }

    pub(super) fn new(capacity: NonZeroUsize) -> Result<Self, SignallingError> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity.get())
            .map_err(|_| SignallingError::Capacity)?;
        slots.resize(capacity.get(), None);
        Ok(Self { slots })
    }

    fn retire_terminal(&mut self, bearer: &mut CommonAmBearer) -> Result<(), SignallingError> {
        for slot in &mut self.slots {
            let Some(delivery) = *slot else { continue };
            match bearer
                .transmission(delivery.connection.key.ue, delivery.receipt)
                .map_err(SignallingError::Am)?
            {
                SduTransmission::Acknowledged | SduTransmission::Discarded => {
                    // A discarded SDU cannot arrive after release, but is not evidence of delivery.
                    bearer
                        .release_receipt(delivery.connection.key.ue, delivery.receipt)
                        .map_err(SignallingError::Am)?;
                    *slot = None;
                }
                SduTransmission::Queued | SduTransmission::InFlight { .. } => (),
            }
        }
        Ok(())
    }

    pub(super) fn enqueue(
        &mut self,
        bearer: &mut CommonAmBearer,
        connection: CoreConnection,
        bits: Bits<'_>,
    ) -> Result<(), SignallingError> {
        self.retire_terminal(bearer)?;
        let vacant =
            self.slots
                .iter_mut()
                .find(|slot| slot.is_none())
                .ok_or(SignallingError::Am(crate::common_am::CommonAmError::Rlc(
                    umts_rlc::acknowledged::AmError::Capacity,
                )))?;
        let receipt = bearer
            .enqueue_tracked(connection.key.ue, bits)
            .map_err(SignallingError::Am)?;
        *vacant = Some(Delivery {
            connection,
            receipt,
        });
        Ok(())
    }

    pub(super) fn pending(
        &mut self,
        bearer: &mut CommonAmBearer,
        connection: CoreConnection,
    ) -> Result<bool, SignallingError> {
        self.retire_terminal(bearer)?;
        Ok(self
            .slots
            .iter()
            .flatten()
            .any(|delivery| delivery.connection == connection))
    }
}
