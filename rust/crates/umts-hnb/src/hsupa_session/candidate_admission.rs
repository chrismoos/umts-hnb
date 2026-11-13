// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    ReceiveResources,
    receive_preparation::{Error as PreparationError, PreparationWorker, Request},
};
use crate::{
    controller::{CommonController, HsupaCandidate, PreparedHsupaAdmission},
    dedicated_rx::{HsupaReceiveSource, ReceiveCommandResult, ReceiveIdentity},
    radio_resources::{HsupaUserLease, RadioResources},
    rel99_admission::AdditionalLaunch,
};
use anyhow::{Result, anyhow};
use std::{num::NonZeroU64, sync::Arc};
use umts_core::Transport;

enum State {
    Idle,
    Source(Box<HsupaCandidate>),
    Building {
        identity: ReceiveIdentity,
        lease: HsupaUserLease,
        cancelled: bool,
    },
    Ready {
        resources: Box<ReceiveResources>,
        lease: HsupaUserLease,
    },
    Rollback(HsupaUserLease),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    SourceReply,
    Preparation(PreparationError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Idle,
    Waiting,
    Building,
    Prepared,
    Admitted,
    RolledBack,
}

pub struct AdmissionSettings {
    pub launch: AdditionalLaunch,
    pub response_guard: NonZeroU64,
    pub grant_value: u8,
    pub grant_ceiling: u8,
    pub grant_amplitude: f32,
    pub grant_refresh: Option<NonZeroU64>,
}

#[must_use = "Retain pending candidates until admission or resource rollback completes"]
pub struct CandidateAdmission {
    worker: PreparationWorker,
    state: State,
    failure: Option<Failure>,
    unexpected: Option<Box<ReceiveCommandResult>>,
}

impl CandidateAdmission {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            worker: PreparationWorker::new()?,
            state: State::Idle,
            failure: None,
            unexpected: None,
        })
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    pub fn identity(&self) -> Option<ReceiveIdentity> {
        match &self.state {
            State::Idle | State::Rollback(_) => None,
            State::Source(candidate) => Some(candidate.identity),
            State::Building { identity, .. } => Some(*identity),
            State::Ready { resources, .. } => Some(resources.identity()),
        }
    }

    pub fn select<T: Transport>(
        &mut self,
        controller: &mut CommonController<T>,
        resources: &mut RadioResources,
        pool: crate::radio_resources::HsupaPoolIdentity,
    ) -> Result<bool> {
        if !self.is_idle() || self.failure.is_some() {
            return Ok(false);
        }
        let Some(candidate) = controller.next_hsupa_candidate(resources, pool)? else {
            return Ok(false);
        };
        self.state = State::Source(Box::new(candidate));
        Ok(true)
    }

    pub fn cancel(&mut self) {
        self.state = match std::mem::replace(&mut self.state, State::Idle) {
            State::Source(candidate) => State::Rollback(candidate.lease),
            State::Building {
                identity, lease, ..
            } => State::Building {
                identity,
                lease,
                cancelled: true,
            },
            State::Ready { lease, .. } => State::Rollback(lease),
            other => other,
        };
    }

    pub fn step<T: Transport>(
        &mut self,
        controller: &mut CommonController<T>,
        resources: &mut RadioResources,
        settings: AdmissionSettings,
        construct: impl FnOnce(&HsupaCandidate, HsupaReceiveSource) -> Result<Request>,
    ) -> Result<Progress> {
        if self
            .identity()
            .is_some_and(|identity| !controller.hsupa_candidate_current(identity))
        {
            self.cancel();
        }
        if let Some(failure) = self.failure
            && !matches!(self.state, State::Rollback(_))
        {
            return Err(anyhow!("HSUPA candidate admission stopped: {failure:?}"));
        }
        match &mut self.state {
            State::Idle => Ok(Progress::Idle),
            State::Source(candidate) => {
                let Some(result) = candidate.source.try_result() else {
                    return Ok(Progress::Waiting);
                };
                let source = match result {
                    ReceiveCommandResult::HsupaSource {
                        identity,
                        source,
                        transient,
                    } if identity == candidate.identity => {
                        if source.is_none() && !transient {
                            // The receiver cannot host E-DCH for this configuration, and a new
                            // configuration is a new identity.
                            controller.reject_hsupa_candidate(candidate.identity);
                            tracing::warn!(
                                "HSUPA candidate rejected: receiver cannot host E-DCH identity={:?}",
                                candidate.identity
                            );
                        }
                        source
                    }
                    result => {
                        let State::Source(candidate) =
                            std::mem::replace(&mut self.state, State::Idle)
                        else {
                            unreachable!()
                        };
                        self.unexpected = Some(Box::new(result));
                        self.failure = Some(Failure::SourceReply);
                        self.state = State::Rollback(candidate.lease);
                        return Err(anyhow!("HSUPA source reply identity or command mismatch"));
                    }
                };
                let has_source = source.is_some();
                let request = source
                    .ok_or_else(|| anyhow!("HSUPA source receiver is no longer eligible"))
                    .and_then(|source| construct(candidate, source));
                let State::Source(candidate) = std::mem::replace(&mut self.state, State::Idle)
                else {
                    unreachable!()
                };
                let request = match request {
                    Ok(request)
                        if request.inputs.identity == candidate.identity
                            && Arc::ptr_eq(
                                request.configuration.dedicated(),
                                &candidate.dedicated,
                            )
                            && request.configuration.parameters().transport.category()
                                == candidate.category
                            && request.configuration.parameters().e_rnti
                                == candidate.lease.e_rnti() =>
                    {
                        request
                    }
                    result => {
                        if has_source {
                            controller.reject_hsupa_candidate(candidate.identity);
                        }
                        self.state = State::Rollback(candidate.lease);
                        return Err(result.err().unwrap_or_else(|| {
                            anyhow!("HSUPA receiver request changed candidate identity")
                        }));
                    }
                };
                match self.worker.submit(request) {
                    Ok(()) => {
                        self.state = State::Building {
                            identity: candidate.identity,
                            lease: candidate.lease,
                            cancelled: false,
                        };
                        Ok(Progress::Building)
                    }
                    Err(rejected) => {
                        self.state = State::Rollback(candidate.lease);
                        Err(anyhow!(
                            "HSUPA receiver preparation submission: {:?}",
                            rejected.error
                        ))
                    }
                }
            }
            State::Building { .. } => {
                if let Err(error) = self.worker.poll() {
                    let State::Building { lease, .. } =
                        std::mem::replace(&mut self.state, State::Idle)
                    else {
                        unreachable!()
                    };
                    // Catalog jobs never own radio reservations or publish receiver commands.
                    self.state = State::Rollback(lease);
                    self.failure = Some(Failure::Preparation(error));
                    return Err(anyhow!("HSUPA receiver preparation: {error:?}"));
                }
                let Some(prepared) = self.worker.take() else {
                    return Ok(Progress::Waiting);
                };
                let State::Building {
                    identity,
                    lease,
                    cancelled,
                } = std::mem::replace(&mut self.state, State::Idle)
                else {
                    unreachable!()
                };
                if cancelled || prepared.identity != identity {
                    self.state = State::Rollback(lease);
                    return if cancelled {
                        Ok(Progress::Waiting)
                    } else {
                        Err(anyhow!("HSUPA prepared receiver changed generation"))
                    };
                }
                match prepared.resources {
                    Ok(resources) => {
                        self.state = State::Ready {
                            resources: Box::new(resources),
                            lease,
                        };
                        Ok(Progress::Prepared)
                    }
                    Err(error) => {
                        controller.reject_hsupa_candidate(identity);
                        self.state = State::Rollback(lease);
                        Err(error)
                    }
                }
            }
            State::Ready { resources, .. } => {
                if !controller.hsupa_candidate_ready(resources.identity()) {
                    return Ok(Progress::Waiting);
                }
                let State::Ready { resources, lease } =
                    std::mem::replace(&mut self.state, State::Idle)
                else {
                    unreachable!()
                };
                match controller.admit_prepared_hsupa(PreparedHsupaAdmission {
                    resources: *resources,
                    lease,
                    launch: settings.launch,
                    response_guard: settings.response_guard,
                    grant_value: settings.grant_value,
                    grant_ceiling: settings.grant_ceiling,
                    grant_amplitude: settings.grant_amplitude,
                    grant_refresh: settings.grant_refresh,
                }) {
                    Ok(()) => Ok(Progress::Admitted),
                    Err(rejected) => {
                        self.state = State::Ready {
                            resources: Box::new(rejected.request.resources),
                            lease: rejected.request.lease,
                        };
                        Err(rejected.error)
                    }
                }
            }
            State::Rollback(_) => {
                let State::Rollback(lease) = std::mem::replace(&mut self.state, State::Idle) else {
                    unreachable!()
                };
                match resources.release_hsupa_user(lease) {
                    Ok(()) => Ok(Progress::RolledBack),
                    Err(rejected) => {
                        self.state = State::Rollback(rejected.lease);
                        Err(anyhow!("HSUPA candidate rollback: {:?}", rejected.error))
                    }
                }
            }
        }
    }
}
