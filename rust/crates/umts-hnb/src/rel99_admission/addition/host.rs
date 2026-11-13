// SPDX-License-Identifier: AGPL-3.0-or-later

use super::super::setup::SetupSource;
use super::{
    assignment::{AdditionalAssignment, RequestMedia},
    *,
};
use crate::{
    rab_handoff::PendingRabAssignment,
    rel99_admission::RabAdmissionError,
    rel99_planner::{ConfiguredPolicy, InitialOwnerSettings, OwnerBuildError, PreparedExisting},
};
use umts_core::{Gateway, Transport};
use umts_iuh::Domain;
use umts_rrc::dedicated::ScheduledActivation;
use umts_types::CnDomain;

const MAX_ADDITIONAL_ASSIGNMENTS: usize = 16;

#[derive(Clone, Copy)]
enum Launch {
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    Explicit(ScheduledActivation),
    Budgeted(AdditionalLaunch),
}

#[must_use = "Retain the request and any staged radio change until rollback completes"]
pub struct RejectedAdditionalAssignment {
    pub error: RabAdmissionError,
    pub pending: PendingRabAssignment,
    pub change: Option<PreparedRadioChange>,
    pub rollback_error: Option<crate::radio_resources::Error>,
}

impl std::fmt::Debug for RejectedAdditionalAssignment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedAdditionalAssignment")
            .field("error", &self.error)
            .field("change", &self.change)
            .field("rollback_error", &self.rollback_error)
            .finish_non_exhaustive()
    }
}

pub struct AdditionalRabHost<'a, C: Transport> {
    pub gateway: &'a Gateway<C>,
    pub policy: &'a ConfiguredPolicy,
    pub radio: &'a mut CommonSignalling,
    pub resources: &'a mut RadioResources,
    pub receive: &'a mut DedicatedController,
    pub admission: &'a mut Rel99Admission,
}

impl<C: Transport> AdditionalRabHost<'_, C> {
    #[cfg(test)]
    pub fn begin_with_settings(
        &mut self,
        previous: ReceiveIdentity,
        pending: PendingRabAssignment,
        activation: ScheduledActivation,
        make_settings: impl FnOnce(&PreparedExisting) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ReceiveIdentity, Box<RejectedAdditionalAssignment>> {
        self.begin(
            previous,
            SetupSource::Fresh(pending),
            Launch::Explicit(activation),
            |prepared, _| make_settings(prepared),
        )
    }

    pub fn begin_with_delivery(
        &mut self,
        previous: ReceiveIdentity,
        pending: PendingRabAssignment,
        launch: AdditionalLaunch,
        make_settings: impl FnOnce(
            &PreparedExisting,
            &AdditionalDelivery,
        ) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ReceiveIdentity, Box<RejectedAdditionalAssignment>> {
        self.begin(
            previous,
            SetupSource::Fresh(pending),
            Launch::Budgeted(launch),
            |prepared, delivery| {
                make_settings(prepared, delivery.expect("budgeted additional launch"))
            },
        )
    }

    pub fn begin_after_release_with_delivery(
        &mut self,
        previous: ReceiveIdentity,
        released: crate::rel99_admission::SetupAfterRelease,
        launch: AdditionalLaunch,
        make_settings: impl FnOnce(
            &PreparedExisting,
            &AdditionalDelivery,
        ) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ReceiveIdentity, Box<RejectedAdditionalAssignment>> {
        self.begin(
            previous,
            SetupSource::AfterRelease(released),
            Launch::Budgeted(launch),
            |prepared, delivery| make_settings(prepared, delivery.expect("budgeted setup launch")),
        )
    }

    fn begin(
        &mut self,
        previous: ReceiveIdentity,
        source: SetupSource,
        launch: Launch,
        make_settings: impl FnOnce(
            &PreparedExisting,
            Option<&AdditionalDelivery>,
        ) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ReceiveIdentity, Box<RejectedAdditionalAssignment>> {
        let mut change = None;
        let mut inventory = None;
        let mut media = None;
        let mut delivery = None;
        let result = (|| {
            let pending = source.pending();
            if pending.request().key() != previous.ue || source.selection().is_none() {
                return Err(RabAdmissionError::Construction);
            }
            let _authority = source
                .selection()
                .expect("checked setup source")
                .authorize_items(self.gateway)
                .map_err(RabAdmissionError::Authority)?;
            let domain = match pending.request().domain() {
                Domain::Cs => CnDomain::Cs,
                Domain::Ps => CnDomain::Ps,
            };
            self.policy
                .validate_rel99_packet_setup(domain)
                .map_err(|error| RabAdmissionError::Hsdpa(error.to_string()))?;
            let entry = self
                .admission
                .entry_mut(previous)
                .ok_or(RabAdmissionError::Admission(AdmissionError::Identity))?;
            if entry
                .rab_assignment
                .as_ref()
                .is_none_or(|initial| !self.gateway.owns_rab_request(initial.request()))
            {
                return Err(RabAdmissionError::Construction);
            }
            if entry.additional_assignments.len() >= MAX_ADDITIONAL_ASSIGNMENTS {
                return Err(RabAdmissionError::Admission(AdmissionError::Capacity));
            }
            entry
                .additional_assignments
                .try_reserve_exact(1)
                .map_err(|_| RabAdmissionError::Admission(AdmissionError::Capacity))?;
            let retained = self
                .admission
                .active_rabs(previous)
                .ok_or(RabAdmissionError::Admission(AdmissionError::Phase))?;
            let requested = pending
                .assignment()
                .setup
                .iter()
                .map(|item| (domain, item))
                .collect::<Vec<_>>();
            let selected = self
                .policy
                .services()
                .select(&requested)
                .map_err(RabAdmissionError::Selection)?;
            let active = self
                .admission
                .active_radio(previous)
                .map_err(RabAdmissionError::Admission)?;
            let planned = self
                .policy
                .plan_existing(&active, self.radio, &retained, selected.rabs())
                .map_err(RabAdmissionError::Planning)?;
            inventory = Some(
                self.admission
                    .entry(previous)
                    .and_then(|entry| entry.inventory.as_ref())
                    .ok_or(RabAdmissionError::Admission(AdmissionError::Phase))?
                    .prepare_addition_with_current_connection(
                        pending.request(),
                        planned.rabs(),
                        |request| self.gateway.check_rab_connection(request).is_ok(),
                    )
                    .map_err(RabAdmissionError::Inventory)?,
            );
            media = Some(RequestMedia::new(
                source.selection().expect("checked setup source"),
                planned.rabs(),
            )?);
            let activation = match launch {
                Launch::Explicit(activation) => activation,
                Launch::Budgeted(launch) => self
                    .admission
                    .provisional_addition(previous, planned.compiled(), self.receive, launch)
                    .map_err(RabAdmissionError::Admission)?,
            };
            let mut prepared =
                match planned.allocate(active, self.radio, self.resources, activation) {
                    Ok(prepared) => prepared,
                    Err(rejected) => {
                        change = rejected.change;
                        return Err(RabAdmissionError::Planning(rejected.error));
                    }
                };
            if let Launch::Budgeted(launch) = launch {
                match self.admission.plan_additional_delivery(
                    previous,
                    &mut prepared,
                    self.radio,
                    self.receive,
                    launch,
                ) {
                    Ok(budget) => delivery = Some(budget),
                    Err(error) => {
                        change = Some(prepared.change);
                        return Err(RabAdmissionError::Admission(error));
                    }
                }
            }
            let settings = match make_settings(&prepared, delivery.as_ref()) {
                Ok(settings) => settings,
                Err(error) => {
                    change = Some(prepared.change);
                    return Err(RabAdmissionError::OwnerSettings(error));
                }
            };
            let mut built = match self
                .admission
                .build_existing_owners(previous, &prepared, self.radio, settings)
            {
                Ok(built) => built,
                Err(error) => {
                    change = Some(prepared.change);
                    return Err(RabAdmissionError::Owners(error));
                }
            };
            if delivery
                .as_ref()
                .is_some_and(|budget| !budget.matches(built.encoded_setup()))
            {
                drop(built);
                change = Some(prepared.change);
                return Err(RabAdmissionError::Construction);
            }
            let feedback = match self
                .admission
                .prepare_hsdpa_feedback(built.identity(), |capacity, budget| {
                    built.configure_hsdpa_feedback(capacity, budget)
                }) {
                Ok(feedback) => feedback,
                Err(error) => {
                    change = Some(prepared.change);
                    return Err(RabAdmissionError::Owners(
                        crate::rel99_planner::OwnerBuildError::Receive(error),
                    ));
                }
            };
            match built.begin(prepared.change, self.admission, self.receive) {
                Ok(identity) => {
                    self.admission.commit_hsdpa_feedback(feedback);
                    Ok(identity)
                }
                Err(rejected) => {
                    change = Some(rejected.change);
                    Err(RabAdmissionError::Admission(rejected.error))
                }
            }
        })();
        match result {
            Ok(identity) => {
                let addition = self
                    .admission
                    .entry_mut(previous)
                    .expect("accepted additional radio owner")
                    .lifecycle
                    .addition_mut()
                    .expect("scheduled addition");
                addition.delivery = delivery;
                addition.assignment = Some(AdditionalAssignment::new(
                    source,
                    inventory.expect("prepared additional inventory"),
                    media.expect("prepared request media"),
                ));
                Ok(identity)
            }
            Err(error) => {
                let mut rollback_error = None;
                if let Some(candidate) = change.take()
                    && let Err(rejected) = self.resources.cancel_reconfiguration(candidate)
                {
                    rollback_error = Some(rejected.error);
                    change = Some(rejected.change);
                }
                Err(Box::new(RejectedAdditionalAssignment {
                    error,
                    pending: source.into_pending(),
                    change,
                    rollback_error,
                }))
            }
        }
    }
}
