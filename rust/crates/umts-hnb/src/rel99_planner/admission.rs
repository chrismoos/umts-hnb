// SPDX-License-Identifier: AGPL-3.0-or-later

use super::PreparedInitial;
use crate::{
    dedicated_bearers::{
        BearerError, BearerLimits, DedicatedBearers, FramePoolLimits, MappingActivation,
    },
    dedicated_controller::{DedicatedController, PreparedDedicated},
    dedicated_rx::{
        ChipReceiveSettings, DedicatedChipReceiver, DedicatedReceiveSession,
        DedicatedSampleReceiver, PilotTpcSettings, ReceiveError, ReceiveIdentity, ReceiveLimits,
        SampleReceiveSettings, TfciAcceptance,
    },
    dedicated_tx::{
        DedicatedTransmitPort, DedicatedTransmitSession, TpcInitialization, TransmitError,
        TransmitLimits,
    },
    radio_resources::AllocatedRadio,
    rel99_admission::{InitialAdmission, RejectedAllocatedAdmission, Rel99Admission},
    signalling::{CommonSignalling, SignallingError},
};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
};
use umts_dsp::{
    dedicated::{CarrierTrackingSettings, DpchAmplitudes, LikelihoodSettings, UplinkDpchCode},
    uplink_scrambling::UplinkScramblingCode,
};
use umts_rrc::dedicated::{DedicatedError, InitialDchSetup, RadioBearerSetupProcedure};
use umts_types::time::ChipInstant;

mod existing;
pub use existing::BuiltExisting;

pub struct InitialOwnerSettings {
    pub timing: MappingActivation,
    // Matched-output center coordinates already remove software RX-filter delay.
    // Include only residual RF, clock and propagation offsets, not that filter delay again.
    pub samples: SampleReceiveSettings,
    pub carrier: CarrierTrackingSettings,
    pub likelihood: LikelihoodSettings,
    pub tfci: TfciAcceptance,
    pub maximum_physical_bits: NonZeroUsize,
    pub receive: ReceiveLimits,
    pub bearers: BearerLimits,
    pub transmit: TransmitLimits,
    pub amplitudes: DpchAmplitudes,
    pub downlink_power: Option<crate::dedicated_tx::DownlinkPowerSettings>,
    pub pool: FramePoolLimits,
    pub now: ChipInstant,
    pub publication_deadline: ChipInstant,
    pub response_timeout: NonZeroU64,
    pub power_control: Option<InitialPowerControl>,
}

#[derive(Clone, Copy)]
pub struct InitialPowerControl {
    pub pilot: PilotTpcSettings,
    pub outer_loop: Option<crate::dedicated_rx::OuterLoopSettings>,
    pub initialization: TpcInitialization,
    pub algorithm2_hold_groups: u8,
}

#[derive(Debug)]
pub enum OwnerBuildError {
    Configuration(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        String,
    ),
    Identity,
    Phase,
    Signalling(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        SignallingError,
    ),
    Bearer(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        BearerError,
    ),
    Receive(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        ReceiveError,
    ),
    Transmit(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        TransmitError,
    ),
    Procedure(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        DedicatedError,
    ),
}

#[must_use]
pub struct BuiltInitial<'a> {
    radio: &'a mut CommonSignalling,
    request: Option<Box<InitialAdmission>>,
}

impl BuiltInitial<'_> {
    pub fn configure_hsdpa_feedback(
        &mut self,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
    ) -> Result<crate::hsdpa_rx::FeedbackPort, ReceiveError> {
        self.request
            .as_mut()
            .expect("unsubmitted initial owners")
            .receive
            .receiver
            .configure_hsdpa_feedback(capacity, budget)
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.request
            .as_ref()
            .expect("unsubmitted initial owners")
            .pool
            .identity()
    }

    pub fn begin(
        mut self,
        allocation: AllocatedRadio,
        admission: &mut Rel99Admission,
        receive: &DedicatedController,
    ) -> Result<ReceiveIdentity, RejectedInitialOwners> {
        let request = self.request.take().expect("unsubmitted initial owners");
        let identity = request.pool.identity();
        match admission.begin_allocated(request, allocation, self.radio, receive) {
            Ok(accepted) => {
                debug_assert_eq!(identity, accepted);
                Ok(accepted)
            }
            Err(RejectedAllocatedAdmission {
                admission,
                allocation,
            }) => {
                self.request = Some(admission.request);
                Err(RejectedInitialOwners {
                    error: admission.error,
                    allocation,
                })
            }
        }
    }
}

#[derive(Debug)]
#[must_use = "Cancel the returned allocation through its original RadioResources owner"]
pub struct RejectedInitialOwners {
    pub error: crate::rel99_admission::AdmissionError,
    pub allocation: AllocatedRadio,
}

impl Drop for BuiltInitial<'_> {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            // No frame writer or worker command exists before admission accepts these owners.
            self.radio
                .retire_frame_pool(request.pool)
                .expect("unsubmitted frame pool has no outstanding users");
        }
    }
}

impl PreparedInitial {
    pub fn build_owners<'a>(
        &self,
        radio: &'a mut CommonSignalling,
        port: &DedicatedTransmitPort,
        settings: InitialOwnerSettings,
    ) -> Result<BuiltInitial<'a>, OwnerBuildError> {
        let key = self.allocation.key();
        let plan = self.allocation.plan();
        if !Arc::ptr_eq(self.setup.configuration(), plan.configuration())
            || !Arc::ptr_eq(self.profiles.plan(), plan)
        {
            return Err(OwnerBuildError::Identity);
        }
        if radio.dedicated_route(key).is_some() {
            return Err(OwnerBuildError::Phase);
        }
        radio
            .validate_retained_configuration(key, plan.configuration())
            .map_err(OwnerBuildError::Signalling)?;
        let identity = self.allocation.receive_identity();
        let setup = InitialDchSetup::new(
            plan.configuration().clone(),
            radio
                .retained_signalling(key)
                .map_err(OwnerBuildError::Signalling)?
                .to_vec(),
            self.setup.rabs().to_vec(),
        )
        .map_err(OwnerBuildError::Procedure)?;
        let transaction = radio
            .next_bearer_setup_transaction(key)
            .map_err(OwnerBuildError::Signalling)?;
        let procedure = if radio.resuming(key) {
            RadioBearerSetupProcedure::resume(
                key,
                transaction,
                setup,
                settings.now,
                settings.publication_deadline,
                settings.response_timeout,
            )
        } else {
            RadioBearerSetupProcedure::initial(
                key,
                transaction,
                setup,
                settings.now,
                settings.publication_deadline,
                settings.response_timeout,
            )
        }
        .map_err(OwnerBuildError::Procedure)?;
        let mut endpoints = radio
            .signalling_endpoints(key)
            .map_err(OwnerBuildError::Signalling)?;
        endpoints.extend(self.profiles.placeholders(key));
        let bearers = DedicatedBearers::from_compiled(
            identity,
            settings.timing,
            plan.clone(),
            endpoints,
            settings.bearers,
        )
        .map_err(|rejected| OwnerBuildError::Bearer(rejected.error))?;
        let RadioOwners { receiver, transmit } = build_radio_owners(identity, plan, &settings)?;
        // Pool allocation charges the signaling ledger and must follow every other fallible construction.
        let pool = radio
            .allocate_frame_pool(identity, plan.clone(), port, settings.pool)
            .map_err(OwnerBuildError::Signalling)?;
        Ok(BuiltInitial {
            radio,
            request: Some(Box::new(InitialAdmission {
                receive: Box::new(PreparedDedicated {
                    receiver: Box::new(receiver),
                    bearers: Box::new(bearers),
                }),
                transmit: Box::new(transmit),
                procedure: Box::new(procedure),
                pool,
            })),
        })
    }
}

pub(crate) struct RadioOwners {
    pub(crate) receiver: DedicatedSampleReceiver,
    pub(crate) transmit: DedicatedTransmitSession,
}

pub(crate) fn build_radio_owners(
    identity: ReceiveIdentity,
    plan: &Arc<crate::rel99_plan::CompiledRel99>,
    settings: &InitialOwnerSettings,
) -> Result<RadioOwners, OwnerBuildError> {
    let session = DedicatedReceiveSession::new(
        identity,
        settings.timing.uplink,
        plan.uplink().clone(),
        settings.receive,
    )
    .map_err(OwnerBuildError::Receive)?;
    let code = UplinkScramblingCode::new(plan.configuration().physical().uplink_scrambling)
        .and_then(UplinkDpchCode::new)
        .map_err(|error| OwnerBuildError::Receive(ReceiveError::Dsp(error)))?;
    let gains = plan
        .uplink_configured()
        .combinations()
        .iter()
        .map(|combination| {
            plan.uplink_gains(combination.tfci)
                .map(|gains| (combination.tfci, gains))
                .ok_or(OwnerBuildError::Identity)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut receiver = DedicatedChipReceiver::new(
        session,
        ChipReceiveSettings {
            code: Arc::new(code),
            control: plan.uplink_control(),
            carrier: settings.carrier,
            likelihood: settings.likelihood,
            tfci: settings.tfci,
            maximum_physical_bits: settings.maximum_physical_bits,
        },
        gains,
    )
    .and_then(|chips| DedicatedSampleReceiver::new(chips, settings.samples))
    .map_err(OwnerBuildError::Receive)?;
    let mut transmit = DedicatedTransmitSession::new(
        identity,
        settings.timing.downlink,
        plan.clone(),
        settings.amplitudes,
        settings.transmit,
    )
    .map_err(OwnerBuildError::Transmit)?;
    if let Some(power) = settings.downlink_power {
        transmit
            .configure_downlink_power(power)
            .map_err(OwnerBuildError::Transmit)?;
    }
    if let Some(power) = settings.power_control {
        if power.pilot.downlink_anchor != settings.timing.downlink.time {
            return Err(OwnerBuildError::Identity);
        }
        receiver
            .configure_power_control(power.pilot)
            .map_err(OwnerBuildError::Receive)?;
        if let Some(settings) = power.outer_loop {
            receiver
                .configure_outer_loop(settings)
                .map_err(OwnerBuildError::Receive)?;
        }
        transmit
            .configure_initial_tpc(power.initialization)
            .map_err(OwnerBuildError::Transmit)?;
        transmit
            .configure_algorithm2_hold_groups(power.algorithm2_hold_groups)
            .map_err(OwnerBuildError::Transmit)?;
    }
    Ok(RadioOwners { receiver, transmit })
}
