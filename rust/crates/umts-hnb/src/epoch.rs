// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    common_rx::{CommonRx, RxWorker, RxWorkerError, RxWorkerReport},
    controller::{CommonController, ControllerError, ControllerStep, ControllerTime},
    radio_io::{
        PumpError, RadioClock, RadioClockError, RadioTime, TxHealthSettings, TxPreparation, TxPump,
        TxWorkerError, TxWorkerReport, TxWorkers,
    },
    signalling::CommonSignalling,
};
use std::time::{Duration, Instant};
use umts_core::Transport;
use umts_iuh::session::{ControlTime, Event};
use umts_runtime::worker::{JoinError, StartGate, StopToken};
use umts_types::RadioEpoch;

const MAX_IDLE_WAIT: Duration = Duration::from_millis(1);

mod retirement;
use retirement::Retirement;
pub use retirement::{EpochOwner, EpochRetirementError};

#[derive(Clone, Copy, Debug)]
pub struct EpochSettings {
    pub idle_wait: Duration,
    pub startup_timeout: Duration,
    pub clock_stall_timeout: Duration,
    pub control_origin: ControlTime,
    pub rx_activation_tick: Option<u64>,
    pub health: TxHealthSettings,
}

#[derive(Debug)]
pub enum EpochStartError {
    Configuration,
    Unpaced,
    Owners,
    Clock(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        RadioClockError,
    ),
    Health(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        PumpError,
    ),
    Tx(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        TxWorkerError,
    ),
    Rx(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        RxWorkerError,
    ),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EpochFailure {
    StartupDeadline,
    AirOriginDeadline,
    Clock(RadioClockError),
    ClockStalled { tick: u64 },
    Time,
    WorkerStopped,
    UnreconciledBearer(crate::dedicated_rx::ReceiveIdentity),
    Controller(ControllerError),
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochPhase {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EpochMutationError {
    Phase(EpochPhase),
    Failure(EpochFailure),
}

type ControllerProgress<T, U, E> = Result<Option<ControllerStep<T, U, E>>, EpochFailure>;

#[derive(Clone, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct EpochReport {
    pub epoch: RadioEpoch,
    pub failure: Option<EpochFailure>,
    pub tx: TxWorkerReport,
    pub rx: RxWorkerReport,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EpochJoinError {
    Tx(JoinError),
    Rx(JoinError),
    MissingOwner(EpochOwner),
}

#[must_use]
pub struct CommonEpoch<T: Transport> {
    tx: TxWorkers,
    rx: RxWorker,
    clock: RadioClock,
    stop: StopToken,
    settings: EpochSettings,
    phase: EpochPhase,
    failure: Option<EpochFailure>,
    started: Instant,
    previous_host: Instant,
    last_controller_time: Option<ControllerTime>,
    progress_at: Instant,
    tick: u64,
    tx_report: Option<TxWorkerReport>,
    rx_report: Option<RxWorkerReport>,
    retirement: Retirement,
    // Worker handles must join before controller-owned radio pools can be dropped.
    controller: Option<CommonController<T>>,
}

impl<T: Transport> CommonEpoch<T> {
    /// Every HAL activation, read, write, clock query, health query and stop must use finite waits.
    pub fn spawn(
        preparation: TxPreparation,
        mut submission: TxPump,
        receiver: CommonRx,
        controller: CommonController<T>,
        settings: EpochSettings,
    ) -> Result<Self, EpochStartError> {
        if settings.idle_wait.is_zero()
            || settings.idle_wait > MAX_IDLE_WAIT
            || settings.startup_timeout.is_zero()
            || settings.clock_stall_timeout.is_zero()
        {
            return Err(EpochStartError::Configuration);
        }
        let clock = preparation.radio_clock().ok_or(EpochStartError::Unpaced)?;
        if !preparation.paired_with(&submission) || !controller.paired_with(&preparation, &receiver)
        {
            return Err(EpochStartError::Owners);
        }
        let tick = clock.read().map_err(EpochStartError::Clock)?.tick;
        submission
            .monitor_health(settings.health)
            .map_err(EpochStartError::Health)?;
        let started = Instant::now();
        let stop = preparation.stop_token();
        let activation = StartGate::default();
        let tx = TxWorkers::spawn_staged(
            preparation,
            submission,
            settings.idle_wait,
            settings.startup_timeout,
            activation.clone(),
        )
        .map_err(EpochStartError::Tx)?;
        let rx = RxWorker::spawn_staged(
            receiver,
            stop.clone(),
            settings.rx_activation_tick,
            settings.idle_wait,
            activation.clone(),
        )
        .map_err(EpochStartError::Rx)?;
        let owner = Self {
            controller: Some(controller),
            tx,
            rx,
            clock,
            stop,
            settings,
            phase: EpochPhase::Starting,
            failure: None,
            started,
            previous_host: started,
            last_controller_time: None,
            progress_at: started,
            tick,
            tx_report: None,
            rx_report: None,
            retirement: Retirement::default(),
        };
        activation.release();
        Ok(owner)
    }

    pub fn epoch(&self) -> RadioEpoch {
        self.clock.origin().chip.epoch
    }

    pub(crate) fn telemetry(&mut self) -> crate::radio_io::telemetry::Radio {
        let mut telemetry = self.tx.telemetry();
        telemetry.receive = self.rx.telemetry();
        telemetry
    }
    pub fn air_origin(&self) -> crate::radio_io::AirClockOrigin {
        self.clock.origin()
    }
    pub fn phase(&self) -> EpochPhase {
        self.phase
    }
    pub fn controller(&self) -> Option<&CommonController<T>> {
        if matches!(self.phase, EpochPhase::Starting | EpochPhase::Running) {
            self.controller.as_ref()
        } else {
            None
        }
    }

    /// The supplied time is the last successful controller step, not a new device-time observation.
    /// Keep owned requests outside the closure and move them only inside an accepted callback.
    pub fn with_controller<R>(
        &mut self,
        operation: impl FnOnce(&mut CommonController<T>, ControllerTime) -> R,
    ) -> Result<R, EpochMutationError> {
        if let Some(failure) = &self.failure {
            return Err(EpochMutationError::Failure(failure.clone()));
        }
        if self.phase != EpochPhase::Running {
            return Err(EpochMutationError::Phase(self.phase));
        }
        if self.stop.is_stopping() {
            return Err(EpochMutationError::Failure(
                self.fail(EpochFailure::WorkerStopped),
            ));
        }
        let current = self.read_clock().map_err(EpochMutationError::Failure)?;
        let Some(time) = self.last_controller_time else {
            return Err(EpochMutationError::Phase(self.phase));
        };
        if current
            .chip
            .is_none_or(|chip| chip.index < time.radio.index)
        {
            return Err(EpochMutationError::Failure(self.fail(EpochFailure::Time)));
        }
        let controller = self.controller.as_mut().expect("running controller owner");
        Ok(operation(controller, time))
    }

    pub(crate) fn step_serving<D: umts_user_plane::packet::BoundDatagrams>(
        &mut self,
        now: Instant,
        other_core: impl FnMut(
            &mut CommonSignalling,
            &Event,
        ) -> Result<(), crate::serving::DeferredCore>,
        traffic: &mut crate::rel99_traffic::Rel99Traffic<D>,
        status: &mut crate::controller::status::ControlStatus,
    ) -> Result<Option<bool>, EpochFailure> {
        let report = self.step_with_duplex_policy(
            now,
            other_core,
            |received, _| Err(received.unsupported()),
            traffic,
        )?;
        Ok(report.map(|report| {
            status.observe(
                report,
                self.controller
                    .as_ref()
                    .expect("running controller owner")
                    .radio(),
            )
        }))
    }

    pub fn step_with_duplex_policy<U, P: crate::rel99_admission::DuplexFramePolicy>(
        &mut self,
        now: Instant,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        control: impl FnMut(
            crate::signalling::ReceivedControl<'_>,
            crate::signalling::ControlSecurity<'_>,
        ) -> Result<(), crate::signalling::SignallingError>,
        policy: &mut P,
    ) -> ControllerProgress<T::Error, U, P::UplinkError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        if matches!(self.phase, EpochPhase::Stopping | EpochPhase::Stopped) {
            return Err(EpochFailure::Stopped);
        }
        if now < self.previous_host {
            return Err(self.fail(EpochFailure::Time));
        }
        self.previous_host = now;
        if self.stop.is_stopping() {
            return Err(self.fail(EpochFailure::WorkerStopped));
        }
        let elapsed = now.duration_since(self.started);
        let radio = self.read_clock()?;
        if radio.tick != self.tick {
            self.tick = radio.tick;
            self.progress_at = now;
        } else if self.tx.is_ready()
            && now.duration_since(self.progress_at) >= self.settings.clock_stall_timeout
        {
            return Err(self.fail(EpochFailure::ClockStalled { tick: self.tick }));
        }
        if !self.tx.is_ready() || !self.rx.is_ready() {
            if elapsed >= self.settings.startup_timeout {
                return Err(self.fail(EpochFailure::StartupDeadline));
            }
            return Ok(None);
        }
        let Some(chip) = radio.chip else {
            if elapsed >= self.settings.startup_timeout {
                return Err(self.fail(EpochFailure::AirOriginDeadline));
            }
            return Ok(None);
        };
        let control_time = u64::try_from(elapsed.as_millis())
            .ok()
            .and_then(|millis| self.settings.control_origin.0.checked_add(millis));
        let Some(control_time) = control_time else {
            return Err(self.fail(EpochFailure::Time));
        };
        let time = ControllerTime::new(chip, ControlTime(control_time), now);
        let controller = self.controller.as_mut().expect("running controller owner");
        match controller.step_with_duplex_policy(time, other_core, control, policy) {
            Ok(step) => {
                self.phase = EpochPhase::Running;
                self.last_controller_time = Some(time);
                Ok(Some(step))
            }
            Err(error) => Err(self.fail(EpochFailure::Controller(error))),
        }
    }

    fn read_clock(&mut self) -> Result<RadioTime, EpochFailure> {
        self.clock
            .read()
            .map_err(|error| self.fail(EpochFailure::Clock(error)))
    }

    fn fail(&mut self, failure: EpochFailure) -> EpochFailure {
        self.failure = Some(failure.clone());
        self.phase = EpochPhase::Failed;
        self.stop.request_stop();
        if let Some(controller) = self.controller.as_mut() {
            controller.request_stop();
        }
        failure
    }

    pub(crate) fn require_bearer_recovery(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> EpochFailure {
        self.fail(EpochFailure::UnreconciledBearer(identity))
    }

    pub fn request_stop(&mut self) {
        if self.phase == EpochPhase::Stopped {
            return;
        }
        self.stop.request_stop();
        if self.failure.is_none() {
            self.phase = EpochPhase::Stopping;
        }
        if let Some(controller) = self.controller.as_mut() {
            controller.request_stop();
        }
    }

    pub fn join(&mut self, timeout: Duration) -> Result<EpochReport, EpochJoinError> {
        self.request_stop();
        let began = Instant::now();
        if self.tx_report.is_none() {
            self.tx_report = Some(self.tx.join(timeout).map_err(EpochJoinError::Tx)?);
        }
        if self.rx_report.is_none() {
            self.rx_report = Some(
                self.rx
                    .join(timeout.saturating_sub(began.elapsed()))
                    .map_err(EpochJoinError::Rx)?,
            );
        }
        self.collect_retired_owners()?;
        self.phase = EpochPhase::Stopped;
        Ok(EpochReport {
            epoch: self.epoch(),
            failure: self.failure.clone(),
            tx: self.tx_report.as_ref().expect("joined TX").clone(),
            rx: self.rx_report.as_ref().expect("joined RX").clone(),
        })
    }
}

impl<T: Transport> Drop for CommonEpoch<T> {
    fn drop(&mut self) {
        self.request_stop();
    }
}

#[cfg(test)]
mod tests;
