// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_rx::ReceiveIdentity,
    hsdpa_session::{Error, SchedulingPreparation, SchedulingResources},
};
use std::{
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread::JoinHandle,
    time::{Duration, Instant},
};

struct Job {
    identity: ReceiveIdentity,
    preparation: SchedulingPreparation,
    secondary: Option<SchedulingPreparation>,
}

pub(super) struct Prepared {
    pub identity: ReceiveIdentity,
    pub resources: Result<Box<SchedulingResources>, Error>,
    pub secondary: Option<Result<Box<SchedulingResources>, Error>>,
    pub elapsed: Duration,
}

pub(super) struct PreparationWorker {
    requests: Option<SyncSender<Job>>,
    results: Option<Receiver<Prepared>>,
    thread: Option<JoinHandle<()>>,
    pending: Option<ReceiveIdentity>,
    ready: Option<Prepared>,
}

pub(super) fn capacity_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Capacity | Error::Rate(umts_dsp::DspError::Capacity)
    )
}

impl Prepared {
    pub(super) fn non_capacity_error(&self) -> Option<&Error> {
        self.resources
            .as_ref()
            .err()
            .into_iter()
            .chain(
                self.secondary
                    .as_ref()
                    .and_then(|result| result.as_ref().err()),
            )
            .find(|error| !capacity_error(error))
    }

    /// Mixed failures must retain their non-capacity error instead of silently downgrading.
    pub(super) fn capacity_only_failure(&self) -> bool {
        let errors = self.resources.as_ref().err().into_iter().chain(
            self.secondary
                .as_ref()
                .and_then(|result| result.as_ref().err()),
        );
        let mut capacity = false;
        for error in errors {
            if !capacity_error(error) {
                return false;
            }
            capacity = true;
        }
        capacity
    }
}

pub(super) fn reserve_carriers(
    configuration: std::sync::Arc<umts_rrc::hsdpa::InitialConfiguration>,
    codes: crate::radio_resources::HsdpaCodeSet,
    secondary: Option<super::SecondaryHsdpaAdmission>,
    scheduling: &crate::hsdpa_session::SchedulingAdmission,
) -> Result<(SchedulingPreparation, Option<SchedulingPreparation>), Error> {
    if configuration.secondary_cell().is_some() != secondary.is_some() {
        return Err(Error::Identity);
    }
    let secondary_settings = secondary
        .map(|secondary| {
            let mut power = crate::hsdpa_session::DownlinkPower::new(
                secondary.cpich_amplitude,
                secondary.control_amplitude,
            )
            .map_err(Error::Rate)?;
            if let (Some(maximum), Some(gamma)) =
                (secondary.power_limit, secondary.transmit_gamma_db)
            {
                power = power
                    .with_allocation(maximum, gamma as f32)
                    .map_err(Error::Rate)?;
            }
            Ok::<_, Error>(crate::hsdpa_session::SchedulingSettings {
                power,
                ..scheduling.settings
            })
        })
        .transpose()?;
    let primary = SchedulingPreparation::new(
        configuration.clone(),
        codes,
        scheduling.settings,
        scheduling.budget,
        &scheduling.cell,
    )?;
    let secondary = secondary
        .zip(secondary_settings)
        .map(|(secondary, settings)| {
            SchedulingPreparation::new(
                configuration,
                secondary.codes,
                settings,
                scheduling.budget,
                &scheduling.cell,
            )
        })
        .transpose()?;
    Ok((primary, secondary))
}

impl PreparationWorker {
    pub fn new() -> std::io::Result<Self> {
        let (requests, jobs) = mpsc::sync_channel::<Job>(1);
        let (completed, results) = mpsc::sync_channel(1);
        let thread = umts_runtime::scheduling::spawn_background("hs-catalog", move || {
            while let Ok(job) = jobs.recv() {
                let started = Instant::now();
                let resources = job.preparation.build();
                let secondary = job.secondary.map(SchedulingPreparation::build);
                if completed
                    .send(Prepared {
                        identity: job.identity,
                        resources,
                        secondary,
                        elapsed: started.elapsed(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        })?;
        Ok(Self {
            requests: Some(requests),
            results: Some(results),
            thread: Some(thread),
            pending: None,
            ready: None,
        })
    }

    pub fn poll(&mut self) -> Result<(), &'static str> {
        if self.ready.is_some() || self.pending.is_none() {
            return Ok(());
        }
        match self
            .results
            .as_ref()
            .expect("live catalog receiver")
            .try_recv()
        {
            Ok(result) if self.pending == Some(result.identity) => {
                self.ready = Some(result);
                Ok(())
            }
            Ok(_) => Err("HSDPA catalog result identity mismatch"),
            Err(TryRecvError::Empty) => Ok(()),
            Err(TryRecvError::Disconnected) => Err("HSDPA catalog worker disconnected"),
        }
    }

    pub fn identity(&self) -> Option<ReceiveIdentity> {
        self.pending
    }
    pub fn ready(&self, identity: ReceiveIdentity) -> bool {
        self.ready
            .as_ref()
            .is_some_and(|result| result.identity == identity)
    }
    pub(super) fn needs_single_fallback(&self, identity: ReceiveIdentity) -> bool {
        self.ready.as_ref().is_some_and(|prepared| {
            prepared.identity == identity
                && prepared.secondary.is_some()
                && prepared.capacity_only_failure()
        })
    }
    pub fn take(&mut self) -> Option<Prepared> {
        let result = self.ready.take()?;
        self.pending = None;
        Some(result)
    }
    pub fn submit_carriers(
        &mut self,
        identity: ReceiveIdentity,
        preparation: SchedulingPreparation,
        secondary: Option<SchedulingPreparation>,
    ) -> Result<(), &'static str> {
        if self.pending.is_some() {
            return Err("HSDPA catalog worker occupied");
        }
        self.requests
            .as_ref()
            .expect("live catalog sender")
            .try_send(Job {
                identity,
                preparation,
                secondary,
            })
            .map_err(|_| "HSDPA catalog publication failed")?;
        self.pending = Some(identity);
        Ok(())
    }
}

impl Drop for PreparationWorker {
    fn drop(&mut self) {
        // Disconnect results before joining so shutdown cannot block on publication.
        self.results.take();
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn identity(generation: u64) -> ReceiveIdentity {
        ReceiveIdentity {
            ue: umts_types::UeKey {
                epoch: crate::test_support::at(0).epoch,
                slot: 0,
                generation: NonZeroU64::new(generation).unwrap(),
            },
            configuration: NonZeroU64::MIN,
        }
    }

    fn pending(identity: ReceiveIdentity) -> (PreparationWorker, SyncSender<Prepared>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        (
            PreparationWorker {
                requests: None,
                results: Some(receiver),
                thread: None,
                pending: Some(identity),
                ready: None,
            },
            sender,
        )
    }

    #[test]
    fn result_retains_exact_generation_until_consumed() {
        let owner = identity(1);
        let (mut worker, sender) = pending(owner);
        worker.poll().unwrap();
        assert!(!worker.ready(owner));
        sender
            .send(Prepared {
                identity: owner,
                resources: Err(Error::Capacity),
                secondary: None,
                elapsed: Duration::ZERO,
            })
            .unwrap_or_else(|_| panic!("result queue"));
        worker.poll().unwrap();
        assert!(worker.ready(owner));
        assert!(!worker.ready(identity(2)));
        assert_eq!(worker.identity(), Some(owner));
        assert!(matches!(
            worker.take().unwrap().resources,
            Err(Error::Capacity)
        ));
        assert!(worker.identity().is_none());
    }

    #[test]
    fn mismatched_or_disconnected_result_is_not_admitted() {
        let (mut worker, sender) = pending(identity(1));
        sender
            .send(Prepared {
                identity: identity(2),
                resources: Err(Error::Capacity),
                secondary: None,
                elapsed: Duration::ZERO,
            })
            .unwrap_or_else(|_| panic!("result queue"));
        assert!(worker.poll().is_err());
        assert!(!worker.ready(identity(1)));
        drop(sender);
        assert!(worker.poll().is_err());
    }

    #[test]
    fn dual_capacity_results_allow_one_identity_scoped_fallback() {
        let owner = identity(1);
        let (mut worker, _) = pending(owner);
        for (primary, secondary, eligible) in [
            (Error::Capacity, Some(Error::Capacity), true),
            (
                Error::Rate(umts_dsp::DspError::Capacity),
                Some(Error::Capacity),
                true,
            ),
            (
                Error::Capacity,
                Some(Error::Rate(umts_dsp::DspError::InvalidAmplitude)),
                false,
            ),
            (Error::Capacity, Some(Error::Identity), false),
            (Error::Identity, Some(Error::Capacity), false),
            (Error::Capacity, None, false),
        ] {
            worker.ready = Some(Prepared {
                identity: owner,
                resources: Err(primary),
                secondary: secondary.map(Err),
                elapsed: Duration::ZERO,
            });
            assert_eq!(worker.needs_single_fallback(owner), eligible);
            assert!(!worker.needs_single_fallback(identity(2)));
            drop(worker.take());
            assert!(!worker.needs_single_fallback(owner));
            worker.pending = Some(owner);
        }
    }

    #[test]
    fn idle_worker_shutdown_joins() {
        drop(PreparationWorker::new().unwrap());
    }
}
