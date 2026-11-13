// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    radio_resources::PreparedRadioChange,
    rel99_admission::{ActiveRadio, AdmissionError, ScheduledAdditionRequest},
    rel99_planner::PreparedExisting,
};
use umts_rrc::dedicated::ExistingDchSetup;
use umts_types::time::CONNECTION_FRAME_MODULUS;

#[must_use]
pub struct BuiltExisting<'a> {
    radio: &'a mut CommonSignalling,
    previous: ReceiveIdentity,
    request: Option<Box<ScheduledAdditionRequest>>,
}

#[derive(Debug)]
#[must_use = "Cancel the returned radio change through its original RadioResources owner"]
pub struct RejectedExistingOwners {
    pub error: AdmissionError,
    pub change: PreparedRadioChange,
}

impl BuiltExisting<'_> {
    pub fn configure_hsdpa_feedback(
        &mut self,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
    ) -> Result<crate::hsdpa_rx::FeedbackPort, ReceiveError> {
        self.request
            .as_mut()
            .expect("unsubmitted additional owners")
            .receiver
            .configure_hsdpa_feedback(capacity, budget)
    }
    pub fn encoded_setup(&self) -> &umts_rrc_codec::EncodedRrc {
        self.request
            .as_ref()
            .expect("unsubmitted additional owners")
            .procedure
            .encoded()
    }

    #[cfg(test)]
    pub fn previous(&self) -> ReceiveIdentity {
        self.previous
    }

    pub fn identity(&self) -> ReceiveIdentity {
        self.request
            .as_ref()
            .expect("unsubmitted additional owners")
            .pool
            .identity()
    }

    #[cfg(test)]
    pub fn plan(&self) -> &Arc<crate::rel99_plan::CompiledRel99> {
        self.request
            .as_ref()
            .expect("unsubmitted additional owners")
            .pool
            .plan()
    }

    pub fn begin(
        mut self,
        change: PreparedRadioChange,
        admission: &mut Rel99Admission,
        receive: &mut DedicatedController,
    ) -> Result<ReceiveIdentity, RejectedExistingOwners> {
        let request = self.request.take().expect("unsubmitted additional owners");
        match admission.begin_addition(self.previous, request, change, self.radio, receive) {
            Ok(identity) => Ok(identity),
            Err(rejected) => {
                self.request = Some(rejected.request);
                Err(RejectedExistingOwners {
                    error: *rejected.error,
                    change: rejected.change,
                })
            }
        }
    }
}

impl Drop for BuiltExisting<'_> {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            // No worker command or frame writer may be retained when scheduled admission rejects ownership.
            self.radio
                .retire_frame_pool(request.pool)
                .expect("unsubmitted additional frame pool has no outstanding users");
        }
    }
}

impl PreparedExisting {
    pub fn build_owners<'a>(
        &self,
        active: &ActiveRadio<'_>,
        radio: &'a mut CommonSignalling,
        port: &DedicatedTransmitPort,
        settings: InitialOwnerSettings,
    ) -> Result<BuiltExisting<'a>, OwnerBuildError> {
        let previous = active.identity();
        let identity = self.change.receive_identity();
        let plan = self.change.plan();
        if identity.ue != previous.ue
            || identity.configuration <= previous.configuration
            || !Arc::ptr_eq(self.setup.previous(), active.plan().configuration())
            || !Arc::ptr_eq(self.setup.configuration(), plan.configuration())
            || !Arc::ptr_eq(self.profiles.plan(), plan)
        {
            return Err(OwnerBuildError::Identity);
        }
        if radio.dedicated_route(previous.ue) != Some(previous) {
            return Err(OwnerBuildError::Phase);
        }
        let activation = self.setup.activation();
        if settings.timing.downlink.time != activation.time()
            || settings.timing.downlink.cfn != activation.cfn()
            || settings.timing.uplink.time.epoch != previous.ue.epoch
            || settings.timing.uplink.cfn != activation.cfn()
            || settings.timing.uplink_frame % CONNECTION_FRAME_MODULUS
                != u64::from(activation.cfn())
            || settings.timing.downlink_frame % CONNECTION_FRAME_MODULUS
                != u64::from(activation.cfn())
        {
            return Err(OwnerBuildError::Identity);
        }
        radio
            .validate_retained_configuration(previous.ue, plan.configuration())
            .map_err(OwnerBuildError::Signalling)?;
        let setup = ExistingDchSetup::new(
            self.setup.previous().clone(),
            plan.configuration().clone(),
            self.setup.rabs().to_vec(),
            activation,
        )
        .map_err(OwnerBuildError::Procedure)?
        .with_retained_hsdpa(self.setup.retained_hsdpa().cloned());
        let procedure = RadioBearerSetupProcedure::existing(
            previous.ue,
            radio
                .next_bearer_setup_transaction(previous.ue)
                .map_err(OwnerBuildError::Signalling)?,
            setup,
            settings.now,
            settings.publication_deadline,
            settings.response_timeout,
        )
        .map_err(OwnerBuildError::Procedure)?;
        let endpoints = self.profiles.placeholders(previous.ue);
        let requested = self
            .setup
            .rabs()
            .iter()
            .map(|rab| rab.bearers.len())
            .sum::<usize>();
        if endpoints.len() != requested
            || endpoints.iter().any(|endpoint| {
                self.setup.previous().bearer(endpoint.id().get()).is_some()
                    || !self
                        .setup
                        .rabs()
                        .iter()
                        .any(|rab| rab.bearers.contains(&endpoint.id().get()))
            })
        {
            return Err(OwnerBuildError::Identity);
        }
        let RadioOwners { receiver, transmit } = build_radio_owners(identity, plan, &settings)?;
        let pool = radio
            .allocate_frame_pool(identity, plan.clone(), port, settings.pool)
            .map_err(OwnerBuildError::Signalling)?;
        Ok(BuiltExisting {
            radio,
            previous,
            request: Some(Box::new(ScheduledAdditionRequest {
                receiver: Box::new(receiver),
                transmit: Box::new(transmit),
                procedure: Box::new(procedure),
                pool,
                activation: settings.timing,
                endpoints,
            })),
        })
    }
}

#[cfg(test)]
mod tests;
