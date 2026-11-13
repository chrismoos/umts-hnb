// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AccessCounters, CommonRx};
use crate::radio_io::{HalStopOutcome, RxCounters};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use umts_runtime::worker::{JoinError, StartGate, StopToken, Worker, WorkerExit};

const MAX_IDLE_WAIT: Duration = Duration::from_millis(1);

#[derive(Debug)]
pub enum RxWorkerError {
    InvalidWait,
    Spawn(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        std::io::Error,
    ),
}

#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct RxStatistics {
    pub radio: RxCounters,
    pub access: AccessCounters,
    pub access_search: super::AccessSearchEvidence,
    pub published: u64,
    pub dedicated: Option<crate::dedicated_rx::SampleRuntimeCounters>,
    pub first_retired_feedback: Option<crate::dedicated_rx::RetiredPilotFeedback>,
    pub timing: Option<super::RxTimingSummary>,
}

#[derive(Clone, Debug)]
pub struct RxWorkerReport {
    pub exit: WorkerExit,
    pub shutdown: HalStopOutcome,
    pub statistics: Option<RxStatistics>,
    pub first_dedicated_failure: Option<crate::dedicated_rx::SampleRuntimeFailure>,
}

struct ReceiveState {
    runtime: CommonRx,
    shutdown: HalStopOutcome,
    ready: Arc<AtomicBool>,
}
impl ReceiveState {
    fn runtime(&mut self) -> &mut CommonRx {
        &mut self.runtime
    }

    fn stop(&mut self) {
        if !self.runtime().started {
            return;
        }
        self.shutdown = match self.runtime().stop() {
            Ok(()) => HalStopOutcome::Succeeded,
            Err(error) => HalStopOutcome::Failed(format!("RX shutdown: {error:?}")),
        };
    }
}
impl ReceiveState {
    fn finish(&mut self) {
        self.ready.store(false, Ordering::Release);
    }
}

#[must_use]
pub struct RxWorker {
    telemetry: crate::radio_io::telemetry::Inbox<crate::radio_io::telemetry::Receive>,
    ready: Arc<AtomicBool>,
    stop: StopToken,
    worker: Worker<ReceiveState>,
    retired_runtime: Option<CommonRx>,
    report: Option<RxWorkerReport>,
}

impl RxWorker {
    /// Use the TX epoch's stop token. HAL activation, reads and stop must have finite waits.
    #[cfg(test)]
    pub fn spawn(
        runtime: CommonRx,
        stop: StopToken,
        activation_tick: Option<u64>,
        idle_wait: Duration,
    ) -> Result<Self, RxWorkerError> {
        let gate = StartGate::default();
        let worker = Self::spawn_staged(runtime, stop, activation_tick, idle_wait, gate.clone())?;
        gate.release();
        Ok(worker)
    }

    pub(crate) fn spawn_staged(
        runtime: CommonRx,
        stop: StopToken,
        activation_tick: Option<u64>,
        idle_wait: Duration,
        gate: StartGate,
    ) -> Result<Self, RxWorkerError> {
        if idle_wait.is_zero() || idle_wait > MAX_IDLE_WAIT {
            return Err(RxWorkerError::InvalidWait);
        }
        let (mut metrics, telemetry) = crate::radio_io::telemetry::channel();
        let ready = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();
        let returned = ReceiveState {
            runtime,
            shutdown: HalStopOutcome::NotAttempted,
            ready: ready.clone(),
        };
        let worker = Worker::spawn_owned_gated_with_role(
            "hnb-common-rx",
            stop.clone(),
            gate,
            umts_runtime::scheduling::Role::RxProcess,
            Box::new(returned),
            move |returned, stop| {
                let run = (|| {
                    if stop.is_stopping() {
                        return Ok(());
                    }
                    returned
                        .runtime()
                        .start(activation_tick)
                        .map_err(|error| format!("RX activation: {error:?}"))?;
                    worker_ready.store(true, Ordering::Release);
                    while !stop.is_stopping() {
                        let cycle = returned.runtime().cycle();
                        metrics.publish(std::time::Instant::now(), || {
                            let runtime = &returned.runtime;
                            crate::radio_io::telemetry::Receive {
                                radio: runtime.receiver.counters(),
                                access: runtime.access.counters(),
                                published: runtime.published,
                                cycle: runtime
                                    .timings
                                    .as_ref()
                                    .map(|timings| timings.summary.cycle),
                                read_filter: runtime
                                    .timings
                                    .as_ref()
                                    .map(|timings| timings.summary.read_filter),
                                dedicated_feed: runtime
                                    .timings
                                    .as_ref()
                                    .map(|timings| timings.summary.dedicated_feed),
                                access_search: runtime
                                    .timings
                                    .as_ref()
                                    .map(|timings| timings.summary.access_search),
                                dedicated_service: runtime
                                    .timings
                                    .as_ref()
                                    .map(|timings| timings.summary.dedicated_service),
                                access_decode: runtime
                                    .timings
                                    .as_ref()
                                    .map(|timings| timings.summary.access_decode),
                                clock_sample: runtime
                                    .timings
                                    .as_ref()
                                    .map(|timings| timings.summary.clock_sample),
                                dedicated_cpu_ns: runtime
                                    .timings
                                    .as_ref()
                                    .map(|timings| timings.summary.dedicated_cpu_total_ns),
                            }
                        });
                        match cycle {
                            Ok(progress) if progress.is_idle() => thread::park_timeout(idle_wait),
                            Ok(_) => {}
                            Err(
                                super::CommonRxError::Closed
                                | super::CommonRxError::Access(super::AccessError::Ingress(
                                    super::IngressError::Closed,
                                )),
                            ) if stop.is_stopping() => break,
                            Err(error) => return Err(format!("Common RX: {error:?}")),
                        }
                    }
                    Ok(())
                })();
                worker_ready.store(false, Ordering::Release);
                returned.stop();
                run
            },
            ReceiveState::finish,
        )
        .map_err(|failure| RxWorkerError::Spawn(failure.error))?;
        Ok(Self {
            telemetry,
            worker,
            retired_runtime: None,
            report: None,
            ready,
            stop,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.stop.is_stopping()
    }

    pub(crate) fn telemetry(
        &mut self,
    ) -> Option<crate::radio_io::telemetry::Observation<crate::radio_io::telemetry::Receive>> {
        self.telemetry.latest()
    }

    #[cfg(test)]
    pub fn request_stop(&self) {
        self.worker.request_stop();
    }
    pub fn join(&mut self, timeout: Duration) -> Result<RxWorkerReport, JoinError> {
        if let Some(report) = &self.report {
            return Ok(report.clone());
        }
        let (mut exit, mut returned) = self.worker.join_owned(timeout)?;
        if let Some(reader_exit) = returned.runtime.receiver.reader_exit()
            && *reader_exit != WorkerExit::Stopped
        {
            exit = reader_exit.clone();
        }
        let statistics = Some(returned.runtime.statistics());
        let first_dedicated_failure = returned.runtime.first_dedicated_failure().cloned();
        returned.runtime.receiver.finish_capture();
        self.retired_runtime = Some(returned.runtime);
        let report = RxWorkerReport {
            exit,
            shutdown: returned.shutdown,
            statistics,
            first_dedicated_failure,
        };
        self.report = Some(report.clone());
        Ok(report)
    }

    pub(crate) fn take_retired_runtime(&mut self) -> Option<CommonRx> {
        self.report.as_ref()?;
        self.retired_runtime.take()
    }
}

#[cfg(test)]
mod tests;
