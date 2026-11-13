// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AdmissionPhase, Rel99Admission};
use crate::{
    dedicated_rx::{ReceiveError, ReceiveIdentity},
    hsdpa_rx::{FeedbackPort, FeedbackReceiver},
};
use std::num::NonZeroUsize;
use std::sync::Arc;

pub(crate) struct PreparedCatalog {
    pub secondary: Option<Box<crate::hsdpa_session::SchedulingResources>>,
    pub configuration: Arc<umts_rrc::hsdpa::InitialConfiguration>,
    pub resources: Box<crate::hsdpa_session::SchedulingResources>,
}

mod activation;
pub use activation::ActivationRequest;

const RECEIVER_GENERATIONS_PER_UE: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackAvailability {
    NotConfigured,
    Missing,
    Disconnected,
    Available,
}

pub(super) struct FeedbackRegistry {
    capacity: NonZeroUsize,
    budget: NonZeroUsize,
    bindings: Box<[Option<Binding>]>,
}

struct Binding {
    identity: ReceiveIdentity,
    port: FeedbackPort,
}

#[must_use]
pub(crate) struct PreparedFeedback {
    slot: usize,
    binding: Binding,
}

impl FeedbackRegistry {
    fn transfer<T, E>(
        &mut self,
        identity: ReceiveIdentity,
        build: impl FnOnce(FeedbackPort) -> Result<T, (E, FeedbackPort)>,
    ) -> Result<Option<T>, E> {
        let Some(slot) = self.bindings.iter_mut().find(|binding| {
            binding
                .as_ref()
                .is_some_and(|binding| binding.identity == identity && binding.port.available())
        }) else {
            return Ok(None);
        };
        let binding = slot.take().expect("matched feedback owner");
        match build(binding.port) {
            Ok(value) => Ok(Some(value)),
            Err((error, port)) => {
                assert_eq!(
                    port.ue(),
                    identity.ue,
                    "rejected feedback keeps its original owner"
                );
                *slot = Some(Binding { identity, port });
                Err(error)
            }
        }
    }

    fn new(ues: usize, capacity: NonZeroUsize, budget: NonZeroUsize) -> Result<Self, ReceiveError> {
        FeedbackReceiver::delivery_bytes(capacity)?;
        FeedbackReceiver::delivery_bytes(budget)?;
        let slots = ues
            .checked_mul(RECEIVER_GENERATIONS_PER_UE)
            .ok_or(ReceiveError::Capacity)?;
        Ok(Self {
            capacity,
            budget,
            bindings: (0..slots).map(|_| None).collect(),
        })
    }

    fn vacant(&mut self, identity: ReceiveIdentity) -> Result<usize, ReceiveError> {
        for binding in &mut self.bindings {
            if binding
                .as_ref()
                .is_some_and(|binding| !binding.port.available() && binding.port.outstanding() == 0)
            {
                *binding = None;
            }
        }
        if self
            .bindings
            .iter()
            .flatten()
            .any(|binding| binding.identity == identity)
        {
            return Err(ReceiveError::Occupied);
        }
        self.bindings
            .iter()
            .position(Option::is_none)
            .ok_or(ReceiveError::Capacity)
    }
}

impl Rel99Admission {
    pub(crate) fn attach_hsdpa_catalog(
        &mut self,
        identity: ReceiveIdentity,
        catalog: PreparedCatalog,
    ) {
        let entry = self
            .entry_mut(identity)
            .expect("newly admitted initial radio");
        assert!(
            entry.prepared_hsdpa.is_none(),
            "single initial catalog owner"
        );
        assert!(Arc::ptr_eq(
            entry.procedure.configuration(),
            catalog.configuration.dedicated()
        ));
        entry.prepared_hsdpa = Some(catalog);
    }

    pub(crate) fn prepared_hsdpa_configuration(
        &mut self,
        identity: ReceiveIdentity,
        category: umts_types::hsdpa::Category,
        h_rnti: u16,
        secondary: Option<crate::controller::SecondaryHsdpaAdmission>,
    ) -> Option<Arc<umts_rrc::hsdpa::InitialConfiguration>> {
        let active = self
            .active_radio(identity)
            .ok()?
            .plan()
            .configuration()
            .clone();
        let entry = self.entry_mut(identity)?;
        if entry.prepared_hsdpa.as_ref().is_some_and(|catalog| {
            !Arc::ptr_eq(catalog.configuration.dedicated(), &active)
                || catalog.configuration.parameters().harq.category() != category
                || catalog.configuration.parameters().h_rnti != h_rnti
                || match (
                    catalog.configuration.secondary_cell(),
                    secondary,
                    catalog.secondary.is_some(),
                ) {
                    (None, None, false) => false,
                    (Some(actual), Some(expected), true) => {
                        actual.h_rnti() != h_rnti
                            || actual.uarfcn_dl() != expected.uarfcn_dl
                            || actual.primary_scrambling_code() != expected.primary.index()
                            || f64::from(actual.measurement_power_offset_half_db())
                                != expected.measurement_power_offset_db * 2.0
                            || actual.qam64() != category.limits().qam64
                            || !actual
                                .control()
                                .codes()
                                .iter()
                                .copied()
                                .eq(expected.codes.control_codes().take(4))
                    }
                    _ => true,
                }
        }) {
            entry.prepared_hsdpa = None;
        }
        entry
            .prepared_hsdpa
            .as_ref()
            .map(|catalog| catalog.configuration.clone())
    }

    pub(crate) fn take_hsdpa_catalog(
        &mut self,
        identity: ReceiveIdentity,
        expected: &Arc<umts_rrc::hsdpa::InitialConfiguration>,
    ) -> Option<PreparedCatalog> {
        let entry = self.entry_mut(identity)?;
        if !entry
            .prepared_hsdpa
            .as_ref()
            .is_some_and(|catalog| Arc::ptr_eq(&catalog.configuration, expected))
        {
            return None;
        }
        entry.prepared_hsdpa.take()
    }

    pub(crate) fn hsdpa_feedback_availability(
        &self,
        identity: ReceiveIdentity,
    ) -> FeedbackAvailability {
        let Some(registry) = &self.hsdpa_feedback else {
            return FeedbackAvailability::NotConfigured;
        };
        match registry
            .bindings
            .iter()
            .flatten()
            .find(|binding| binding.identity == identity)
        {
            None => FeedbackAvailability::Missing,
            Some(binding) if binding.port.available() => FeedbackAvailability::Available,
            Some(_) => FeedbackAvailability::Disconnected,
        }
    }

    pub fn configure_hsdpa_feedback(
        &mut self,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
    ) -> Result<(), ReceiveError> {
        if self.hsdpa_feedback.is_some() || self.entries.iter().any(Option::is_some) {
            return Err(ReceiveError::Occupied);
        }
        self.hsdpa_feedback = Some(FeedbackRegistry::new(self.entries.len(), capacity, budget)?);
        Ok(())
    }

    pub(crate) fn prepare_hsdpa_feedback(
        &mut self,
        identity: ReceiveIdentity,
        connect: impl FnOnce(NonZeroUsize, NonZeroUsize) -> Result<FeedbackPort, ReceiveError>,
    ) -> Result<Option<PreparedFeedback>, ReceiveError> {
        let Some(registry) = self.hsdpa_feedback.as_mut() else {
            return Ok(None);
        };
        if identity.ue.epoch != self.epoch || identity.ue.slot as usize >= self.entries.len() {
            return Err(ReceiveError::Identity);
        }
        let slot = registry.vacant(identity)?;
        let port = connect(registry.capacity, registry.budget)?;
        assert_eq!(port.ue(), identity.ue, "feedback receiver identity");
        Ok(Some(PreparedFeedback {
            slot,
            binding: Binding { identity, port },
        }))
    }

    // Host admission must commit immediately after accepting the same receiver, without another reservation.
    pub(crate) fn commit_hsdpa_feedback(&mut self, prepared: Option<PreparedFeedback>) {
        if let Some(prepared) = prepared {
            let registry = self.hsdpa_feedback.as_mut().expect("configured feedback");
            let slot = &mut registry.bindings[prepared.slot];
            assert!(slot.is_none(), "feedback reservation remains vacant");
            *slot = Some(prepared.binding);
        }
    }

    pub(crate) fn transfer_hsdpa_feedback<T, E>(
        &mut self,
        identity: ReceiveIdentity,
        build: impl FnOnce(FeedbackPort) -> Result<T, (E, FeedbackPort)>,
    ) -> Result<Option<T>, E> {
        if self.active_radio(identity).is_err() {
            return Ok(None);
        }
        match self.hsdpa_feedback.as_mut() {
            Some(registry) => registry.transfer(identity, build),
            None => Ok(None),
        }
    }

    pub(crate) fn active_identity_at(&self, slot: usize) -> Option<ReceiveIdentity> {
        let identity = self.entries.get(slot)?.as_ref()?.identity;
        self.active_radio(identity).ok().map(|_| identity)
    }

    pub(crate) fn committed_identity_at(&self, slot: usize) -> Option<ReceiveIdentity> {
        let entry = self.entries.get(slot)?.as_ref()?;
        (entry.lifecycle.phase() == AdmissionPhase::Active
            && entry.pool.as_ref()?.identity() == entry.identity
            && entry.resources.as_ref()?.receive_identity() == entry.identity)
            .then_some(entry.identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hsdpa_rx::Request, test_support::at};
    use std::{num::NonZeroU64, sync::Arc};
    use umts_dsp::{dedicated::UplinkDpchCode, uplink_scrambling::UplinkScramblingCode};
    use umts_types::UeKey;

    fn binding(configuration: u64) -> (Binding, FeedbackReceiver) {
        let identity = ReceiveIdentity {
            ue: UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::new(configuration).unwrap(),
        };
        let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0).unwrap()).unwrap());
        let mut receiver = FeedbackReceiver::new(identity.ue, code, 1, at(0)).unwrap();
        let port = receiver
            .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
            .unwrap();
        (Binding { identity, port }, receiver)
    }

    #[test]
    fn generations_coexist_and_live_receivers_cannot_be_reaped() {
        let mut registry = FeedbackRegistry::new(1, NonZeroUsize::MIN, NonZeroUsize::MIN).unwrap();
        let (first, first_rx) = binding(1);
        let (second, second_rx) = binding(2);
        let identity = first.identity;
        let successor = second.identity;
        let slot = registry.vacant(identity).unwrap();
        registry.bindings[slot] = Some(first);
        assert_eq!(registry.vacant(identity), Err(ReceiveError::Occupied));
        let slot = registry.vacant(successor).unwrap();
        registry.bindings[slot] = Some(second);
        let third = ReceiveIdentity {
            configuration: NonZeroU64::new(3).unwrap(),
            ..identity
        };
        assert_eq!(registry.vacant(third), Err(ReceiveError::Capacity));
        drop(first_rx);
        assert_eq!(registry.vacant(third), Ok(0));
        assert_eq!(registry.bindings[1].as_ref().unwrap().identity, successor);
        assert!(registry.bindings[1].as_ref().unwrap().port.available());
        drop(second_rx);
    }

    #[test]
    fn closed_receiver_does_not_discard_unresolved_requests() {
        let mut registry = FeedbackRegistry::new(1, NonZeroUsize::MIN, NonZeroUsize::MIN).unwrap();
        let (mut first, receiver) = binding(1);
        first
            .port
            .request(Request::Cqi {
                start: at(0),
                repetitions: 1,
            })
            .unwrap();
        let identity = first.identity;
        registry.bindings[0] = Some(first);
        drop(receiver);
        assert_eq!(registry.vacant(identity), Err(ReceiveError::Occupied));
        assert_eq!(registry.bindings[0].as_ref().unwrap().port.outstanding(), 1);
    }

    #[test]
    fn invalid_feedback_budgets_are_rejected_before_receiver_admission() {
        let too_large = NonZeroUsize::new(usize::MAX).unwrap();
        assert!(FeedbackRegistry::new(1, too_large, NonZeroUsize::MIN).is_err());
        assert!(FeedbackRegistry::new(1, NonZeroUsize::MIN, too_large).is_err());
        assert!(FeedbackRegistry::new(usize::MAX, NonZeroUsize::MIN, NonZeroUsize::MIN).is_err());
    }

    #[test]
    fn rejected_transfer_restores_original_port_and_success_moves_it_once() {
        let mut registry = FeedbackRegistry::new(1, NonZeroUsize::MIN, NonZeroUsize::MIN).unwrap();
        let (first, receiver) = binding(1);
        let identity = first.identity;
        registry.bindings[0] = Some(first);
        assert_eq!(
            registry.transfer(identity, |port| Err::<(), _>(("retry", port))),
            Err("retry")
        );
        assert!(registry.bindings[0].as_ref().unwrap().port.available());
        let port = registry
            .transfer(identity, Ok::<_, ((), FeedbackPort)>)
            .unwrap()
            .unwrap();
        assert!(port.available());
        assert!(registry.bindings[0].is_none());
        assert!(
            registry
                .transfer(identity, |_| -> Result<(), ((), FeedbackPort)> {
                    panic!("feedback can only transfer once")
                })
                .unwrap()
                .is_none()
        );
        drop(receiver);
        assert!(!port.available());
    }
}
