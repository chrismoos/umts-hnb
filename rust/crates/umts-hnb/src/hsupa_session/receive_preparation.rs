// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ReceiveInputs, ReceiveResources, ReceiveSettings};
use crate::dedicated_rx::ReceiveIdentity;
use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use umts_rrc::hsupa::UplinkActivation;

pub struct Request {
    pub configuration: Arc<UplinkActivation>,
    pub inputs: ReceiveInputs,
    pub settings: ReceiveSettings,
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct Prepared {
    pub identity: ReceiveIdentity,
    pub configuration: Arc<UplinkActivation>,
    pub resources: anyhow::Result<ReceiveResources>,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Busy,
    Disconnected,
    Identity,
}

pub struct Rejected {
    pub error: Error,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub request: Request,
}

pub struct PreparationWorker {
    requests: Option<SyncSender<Request>>,
    results: Option<Receiver<Prepared>>,
    thread: Option<JoinHandle<()>>,
    pending: Option<(ReceiveIdentity, Arc<UplinkActivation>)>,
    ready: Option<Prepared>,
    failure: Option<Error>,
}

impl PreparationWorker {
    pub fn new() -> std::io::Result<Self> {
        let (requests, jobs) = mpsc::sync_channel::<Request>(1);
        let (completed, results) = mpsc::sync_channel(1);
        let thread = umts_runtime::scheduling::spawn_background("edch-catalog", move || {
            while let Ok(request) = jobs.recv() {
                let started = Instant::now();
                let identity = request.inputs.identity;
                let configuration = request.configuration;
                let resources = ReceiveResources::build(
                    Arc::clone(&configuration),
                    request.inputs,
                    request.settings,
                );
                if completed
                    .send(Prepared {
                        identity,
                        configuration,
                        resources,
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
            failure: None,
        })
    }

    pub fn submit(&mut self, request: Request) -> Result<(), Box<Rejected>> {
        if let Some(error) = self.failure {
            return Err(Box::new(Rejected { error, request }));
        }
        if self.pending.is_some() {
            return Err(Box::new(Rejected {
                error: Error::Busy,
                request,
            }));
        }
        let identity = request.inputs.identity;
        let configuration = Arc::clone(&request.configuration);
        if identity.ue.epoch != configuration.activation().time().epoch {
            return Err(Box::new(Rejected {
                error: Error::Identity,
                request,
            }));
        }
        match self
            .requests
            .as_ref()
            .expect("live catalog sender")
            .try_send(request)
        {
            Ok(()) => {
                self.pending = Some((identity, configuration));
                Ok(())
            }
            Err(mpsc::TrySendError::Full(request)) => Err(Box::new(Rejected {
                error: Error::Busy,
                request,
            })),
            Err(mpsc::TrySendError::Disconnected(request)) => {
                self.failure = Some(Error::Disconnected);
                Err(Box::new(Rejected {
                    error: Error::Disconnected,
                    request,
                }))
            }
        }
    }

    pub fn poll(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let Some((identity, configuration)) = &self.pending else {
            return Ok(());
        };
        if self.ready.is_some() {
            return Ok(());
        }
        match self
            .results
            .as_ref()
            .expect("live catalog receiver")
            .try_recv()
        {
            Ok(result) => {
                let matches = result.identity == *identity
                    && Arc::ptr_eq(configuration, &result.configuration);
                self.ready = Some(result);
                if matches {
                    Ok(())
                } else {
                    self.failure = Some(Error::Identity);
                    Err(Error::Identity)
                }
            }
            Err(TryRecvError::Empty) => Ok(()),
            Err(TryRecvError::Disconnected) => {
                self.failure = Some(Error::Disconnected);
                Err(Error::Disconnected)
            }
        }
    }

    #[cfg(test)]
    pub fn identity(&self) -> Option<ReceiveIdentity> {
        self.pending.as_ref().map(|(identity, _)| *identity)
    }

    pub fn take(&mut self) -> Option<Prepared> {
        if self.failure.is_some() {
            return None;
        }
        let result = self.ready.take()?;
        self.pending = None;
        Some(result)
    }
}

impl Drop for PreparationWorker {
    fn drop(&mut self) {
        // Disconnect results before joining so a completed job cannot block shutdown.
        self.results.take();
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
