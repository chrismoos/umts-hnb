// SPDX-License-Identifier: AGPL-3.0-or-later

use super::CommonController;
use crate::{
    dedicated_rx::ReceiveIdentity,
    packet_rabs::PacketRabs,
    rel99_admission::{
        AdditionalMediaError, AdmissionPhase, CircuitPlayoutTime, CircuitSettings,
        InitialCircuitError, InitialPacketError, RetainedCircuitServices,
    },
    rel99_planner::RabPlan,
    voice_bearers::VoiceTraffic,
};
use std::num::NonZeroUsize;
use umts_core::Transport;
use umts_iuh::{Domain, ranap::rab::SetupOrModify, session::ControlTime};
use umts_user_plane::packet::BoundDatagrams;

#[derive(Debug)]
pub enum MediaServiceError {
    AdditionalService(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        super::AdditionalServiceError,
    ),
    Packet(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        InitialPacketError,
    ),
    Circuit(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        InitialCircuitError,
    ),
    Additional(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        AdditionalMediaError,
    ),
}

#[derive(Debug)]
pub struct MediaServiceFailure {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )
    )]
    pub identity: ReceiveIdentity,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )
    )]
    pub error: MediaServiceError,
}

#[derive(Default, Debug)]
pub struct MediaServiceStep {
    pub visited: usize,
    pub failures: usize,
    pub first_failure: Option<MediaServiceFailure>,
}

impl<T: Transport> CommonController<T> {
    pub fn service_initial_media<D: BoundDatagrams>(
        &mut self,
        packets: &mut PacketRabs<D>,
        voices: &mut VoiceTraffic,
        now: ControlTime,
        visits: NonZeroUsize,
        mut circuit_time: impl FnMut(ReceiveIdentity) -> Result<CircuitPlayoutTime, InitialCircuitError>,
        mut circuit_settings: impl FnMut(
            &RabPlan,
            &SetupOrModify,
        ) -> Result<CircuitSettings, InitialCircuitError>,
    ) -> Result<MediaServiceStep, super::ControllerError> {
        let (Some(admission), Some(receive)) = (&mut self.admission, &mut self.dedicated) else {
            return Err(super::ControllerError::Configuration);
        };
        let core = &mut self.core;
        let mut report = MediaServiceStep::default();
        report.visited = admission.visit_rab_assignments(visits, |admission, identity| {
            let pending = admission
                .rab_assignment(identity)
                .expect("visited RAB assignment");
            let domain = pending.request().domain();
            let phase = admission.phase(identity).expect("visited admission");
            let result = match domain {
                Domain::Ps => {
                    let progress = admission
                        .packet_progress(identity)
                        .expect("PS assignment state");
                    if !matches!(
                        phase,
                        AdmissionPhase::Active | AdmissionPhase::Stopping | AdmissionPhase::Retired
                    ) && progress.prepared < progress.items
                    {
                        admission
                            .prepare_packets(identity, core.gateway(), packets)
                            .map(|_| ())
                    } else if phase == AdmissionPhase::Active && progress.recorded < progress.items
                    {
                        admission
                            .complete_packets(identity, core, packets, receive, now)
                            .map(|_| ())
                    } else {
                        Ok(())
                    }
                    .map_err(MediaServiceError::Packet)
                }
                Domain::Cs => (|| {
                    if matches!(phase, AdmissionPhase::Stopping | AdmissionPhase::Retired) {
                        return Ok(());
                    }
                    let progress = admission
                        .circuit_progress(identity, voices)
                        .expect("CS assignment state");
                    if phase != AdmissionPhase::Active && progress.prepared < progress.items {
                        admission.prepare_circuits(
                            identity,
                            core.gateway(),
                            voices,
                            &mut circuit_settings,
                        )?;
                    }
                    let time = circuit_time(identity)?;
                    // Iu-UP initialization runs before RRC completion, while media activation remains radio-gated.
                    let progress = admission.service_circuits_at(
                        identity,
                        core.gateway(),
                        voices,
                        receive,
                        time,
                        NonZeroUsize::MIN,
                    )?;
                    if phase == AdmissionPhase::Active && progress.recorded < progress.items {
                        admission.complete_circuits(identity, core, voices, receive, now)?;
                    }
                    Ok(())
                })()
                .map_err(MediaServiceError::Circuit),
            };
            if let Err(error) = result {
                report.failures += 1;
                if report.first_failure.is_none() {
                    report.first_failure = Some(MediaServiceFailure { identity, error });
                }
            }
            if phase == AdmissionPhase::Active
                && admission
                    .retained_additional_assignments(identity)
                    .is_some_and(|owners| !owners.is_empty())
            {
                let retained = circuit_time(identity)
                    .and_then(|time| {
                        admission.service_retained_additional_circuits(
                            identity,
                            RetainedCircuitServices {
                                gateway: core.gateway(),
                                voices,
                                receive,
                                time,
                                budget: NonZeroUsize::MIN,
                            },
                        )
                    })
                    .map_err(|error| {
                        MediaServiceError::Additional(AdditionalMediaError::Circuit(error))
                    });
                if let Err(error) = retained {
                    report.failures += 1;
                    if report.first_failure.is_none() {
                        report.first_failure = Some(MediaServiceFailure { identity, error });
                    }
                }
            }
        });
        Ok(report)
    }
}
