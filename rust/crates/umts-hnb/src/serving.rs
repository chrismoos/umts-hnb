// SPDX-License-Identifier: AGPL-3.0-or-later

mod bearers;
mod bootstrap;
mod circuit_settings;
mod cleanup;
mod dedicated;
mod downlink_power;
mod hsdpa;
mod hsupa;
mod inactivity;
mod initial_timing;
mod launch;
mod management;
mod media;
mod owner_settings;
mod radio;
mod recovery;
mod retirement;
mod settings;
mod shutdown;
mod system_information;

#[cfg(test)]
use crate::rab_handoff::{HandoffError, PendingRabAssignment};
use crate::{
    cell::{BroadcastTags, CellPlan, IndicatorPowers, ReferencePowers},
    epoch::{CommonEpoch, EpochFailure, EpochJoinError, EpochPhase, EpochReport},
};
use anyhow::{Context, Result, anyhow, ensure};
use sdr_hal::{RadioRx, RadioTx};
use std::{
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU64},
    time::{Duration, Instant},
};
use umts_config::{Config, HnbgwTransport};
use umts_core::Transport;
use umts_types::RadioEpoch;
use umts_user_plane::packet::{BoundDatagrams, UdpEndpoint};

pub use crate::controller::diagnostics::{AccessDiagnostics, ControlFailureStage};
pub use bearers::{BearerFailure, DeferredCore};
#[cfg(test)]
pub(crate) use cleanup::tests::assert_quarantine_recovery;
pub use management::receive_metrics;
pub use recovery::{CoreRetry, RecoveryBudget};
pub use retirement::RetiredServing;
pub use shutdown::ShutdownSignals;

/// Marks a failure to open the HNB-GW association at startup, which the service retries.
#[derive(Debug)]
pub struct CoreUnavailable;

const MILLISECONDS_PER_SECOND: u64 = 1_000;

impl std::fmt::Display for CoreUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HNB-GW unavailable")
    }
}

pub fn core_unavailable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CoreUnavailable>().is_some()
}

fn reference_powers(config: &Config) -> Result<ReferencePowers> {
    Ok(ReferencePowers {
        cpich_tx_dbm: config
            .cell
            .advertised_power
            .cpich_tx_dbm
            .context("missing CPICH power declaration")?,
        uplink_interference_dbm: config
            .cell
            .advertised_power
            .uplink_interference_dbm
            .context("missing uplink interference declaration")?,
    })
}

fn indicator_powers(config: &Config, budget: Option<&downlink_power::Plan>) -> IndicatorPowers {
    IndicatorPowers {
        pich_offset_db: budget.map_or(
            config.power_control.downlink.pich_power_offset_db,
            |power| power.pich_offset_db,
        ),
        aich_offset_db: budget.map_or(config.cell.common_channels.aich_power_offset_db, |power| {
            power.aich_offset_db
        }),
    }
}

fn connect_core(config: &Config, peer: SocketAddr) -> Result<umts_sctp::Association> {
    let backend = sctp_backend(config)?;
    umts_sctp::Association::connect_with(backend, peer, bootstrap::sctp_limits()).map_err(|error| {
        let retry = !matches!(
            error,
            umts_sctp::Error::BackendUnavailable(_) | umts_sctp::Error::Configuration
        );
        let error = anyhow::Error::new(error).context("open Iuh SCTP association");
        if retry {
            error.context(CoreUnavailable)
        } else {
            error
        }
    })
}

fn sctp_backend(config: &Config) -> Result<umts_sctp::Backend> {
    match config.core.hnbgw.transport {
        HnbgwTransport::Sctp => Ok(umts_sctp::Backend::Native),
        HnbgwTransport::Usrsctp => Ok(umts_sctp::Backend::Usrsctp {
            udp: config
                .core
                .hnbgw
                .udp_encapsulation_port
                .and_then(NonZeroU16::new)
                .map(|port| umts_sctp::UdpEncapsulation {
                    local_port: config
                        .core
                        .hnbgw
                        .udp_encapsulation_local_port
                        .and_then(NonZeroU16::new),
                    remote_port: port,
                }),
        }),
    }
}

pub struct PreparedServing {
    #[cfg(test)]
    host_clock: Option<crate::radio_io::sampled_clock::HostClock>,
    config: Config,
    epoch: RadioEpoch,
    cell: CellPlan,
    radio: radio::RadioPlan,
    peer: SocketAddr,
    dedicated: dedicated::DedicatedPlan,
    bearers: bearers::BearerCoordinator,
    downlink_power: Option<downlink_power::Plan>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Description {
    pub epoch: RadioEpoch,
    pub downlink_hz: u64,
    pub uplink_hz: u64,
    pub core: SocketAddr,
}

#[derive(Debug)]
#[cfg(test)]
pub enum AssignmentReadinessError {
    Epoch(
        #[cfg_attr(
            test,
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        crate::epoch::EpochMutationError,
    ),
    Handoff(
        #[cfg_attr(
            test,
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        HandoffError,
    ),
    Capabilities {
        #[cfg_attr(
            test,
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        request: umts_iuh::session::RabRequest,
        #[cfg_attr(
            test,
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        error: crate::signalling::SignallingError,
    },
}

pub use crate::controller::status::Counters as ServingCounters;

#[must_use]
pub struct ServingRuntime<T: Transport, D: BoundDatagrams = UdpEndpoint> {
    epoch: Box<CommonEpoch<T>>,
    idle_wait: Duration,
    join_timeout: Duration,
    control: crate::controller::status::ControlStatus,
    feedback_lead_chips: u64,
    traffic: crate::rel99_traffic::Rel99Traffic<D>,
    bearers: bearers::BearerCoordinator,
    media_error: Option<crate::controller::MediaServiceFailure>,
    bearer_error: Option<BearerFailure>,
    management_capture_ports: Option<[crate::radio_io::capture::live::Port; 2]>,
}

impl PreparedServing {
    #[cfg(test)]
    fn with_test_clock(mut self, read: impl Fn() -> Duration + Send + Sync + 'static) -> Self {
        self.host_clock = Some(crate::radio_io::sampled_clock::HostClock::Controlled(
            std::sync::Arc::new(read),
        ));
        self
    }
    pub fn new(config: &Config, epoch: RadioEpoch) -> Result<Self> {
        config.validate()?;
        let downlink_power = downlink_power::Plan::resolve(config)?;
        settings::validate(config)?;
        RecoveryBudget::new(config)?;
        let dedicated = dedicated::DedicatedPlan::new(config)?;
        media::MediaPlan::new(config)?;
        ensure!(
            !config.hspa.downlink.enabled || config.hspa.downlink.resources.scheduling.is_some(),
            "Rust HSDPA requires explicit scheduler budgets and feedback evidence limits"
        );
        sctp_backend(config)?
            .validate()
            .context("configured SCTP backend is unavailable in this build")?;
        let peer = SocketAddr::new(
            config
                .core
                .hnbgw
                .address
                .parse()
                .context("hnbgw.address must be a numeric IP address for Rust serving")?,
            config.core.hnbgw.port,
        );
        ensure!(
            peer.port() != 0 && !peer.ip().is_unspecified() && !peer.ip().is_multicast(),
            "core peer must have a nonzero port and a unicast address"
        );
        let radio = radio::radio_plan(config)?;
        ensure!(
            radio.rx_phase == 0,
            "Rust common receive currently requires sample phase zero"
        );
        let cell = CellPlan::compile_tagged(
            config,
            epoch,
            BroadcastTags::reference(CellPlan::CURRENT_MIB_VALUE_TAG)?,
            reference_powers(config)?,
            indicator_powers(config, downlink_power.as_ref()),
        )?;
        settings::validate_cell(config, cell.information())?;
        let bearers = bearers::BearerCoordinator::new(
            config,
            epoch,
            cell.information(),
            downlink_power.as_ref(),
        )?;
        Ok(Self {
            #[cfg(test)]
            host_clock: None,
            config: config.clone(),
            epoch,
            cell,
            radio,
            peer,
            dedicated,
            bearers,
            downlink_power,
        })
    }

    pub fn description(&self) -> Description {
        Description {
            epoch: self.epoch,
            downlink_hz: self.radio.downlink_hz,
            uplink_hz: self.radio.uplink_hz,
            core: self.peer,
        }
    }

    pub fn open_with_cancellation(
        mut self,
        cancelled: impl Fn() -> bool,
    ) -> Result<ServingRuntime<umts_sctp::Association>> {
        ensure!(!cancelled(), "serving startup canceled");
        system_information::install(&mut self)?;
        ensure!(!cancelled(), "serving startup canceled");
        let transport = connect_core(&self.config, self.peer)?;
        let native_threads = umts_runtime::scheduling::NativeThreads::before_open()?;
        let mut radio = self.radio.hal.open().context("open configured SDR HAL")?;
        ensure!(!cancelled(), "serving startup canceled");
        radio
            .set_tx_lo_offset_hz(self.config.radio.tx_lo_offset_hz)
            .context("configure compensated TX LO offset")?;
        radio
            .set_tx_frequency(usize::try_from(self.radio.downlink_hz)?)
            .context("tune downlink")?;
        let applied_tx_gain_db = radio.tx_gain_db();
        let (tx, rx) = radio.split().context("split configured SDR streams")?;
        let mut rx = rx.context("SDR did not supply configured RX stream")?;
        rx.set_rx_frequency(self.radio.uplink_hz as f64)
            .context("tune uplink")?;
        native_threads
            .configure_after_open()
            .context("configure native SDR thread scheduling")?;
        tracing::info!(
            applied_tx_gain_db = ?applied_tx_gain_db,
            "SDR requests: downlink_hz={} uplink_hz={} tx_gain_db={} rx_gain_db={}. HAL setup readbacks: tx_sample_rate_hz={} rx_sample_rate_hz={} tx_tick_rate={} rx_tick_rate={}. Final RF frequency, bandwidth, RX gain and antenna readbacks are unavailable through the HAL.",
            self.radio.downlink_hz,
            self.radio.uplink_hz,
            self.config.radio.tx_gain_db,
            self.config.radio.rx_gain_db,
            tx.tx_sample_rate_hz(),
            rx.rx_sample_rate_hz(),
            tx.tick_rate(),
            rx.tick_rate()
        );
        ensure!(!cancelled(), "serving startup canceled");
        let reconnect = self.core_reconnect();
        self.spawn_with_cancellation(tx, rx, transport, reconnect, cancelled)
    }

    fn core_reconnect(&self) -> crate::core_link::Reconnect<umts_sctp::Association> {
        let config = self.config.clone();
        let (epoch, peer) = (self.epoch, self.peer);
        crate::core_link::Reconnect::new(
            std::num::NonZeroU32::new(self.config.core.iuh.retry_attempts),
            self.config
                .core
                .iuh
                .retry_backoff_secs
                .saturating_mul(MILLISECONDS_PER_SECOND),
            Box::new(
                move |now: umts_iuh::session::ControlTime,
                      generation: u64|
                      -> Result<umts_core::Gateway<umts_sctp::Association>, String> {
                connect_core(&config, peer)
                    .and_then(|transport| {
                        bootstrap::gateway(&config, epoch, generation, transport, now)
                    })
                    .map_err(|error| format!("{error:#}"))
                },
            ),
        )
    }

    /// Cancellation is checked before transferring handles to RF workers. HNB-GW registration
    /// continues after the radio starts.
    pub fn spawn_with_cancellation<T: Transport>(
        self,
        tx: Box<dyn RadioTx>,
        rx: Box<dyn RadioRx>,
        transport: T,
        reconnect: crate::core_link::Reconnect<T>,
        cancelled: impl Fn() -> bool,
    ) -> Result<ServingRuntime<T>>
    where
        T::Error: std::fmt::Debug,
    {
        ensure!(!cancelled(), "serving startup canceled");
        let media = media::MediaPlan::new(&self.config)?;
        let datagrams =
            UdpEndpoint::bind(media.bind_address()).context("bind configured GTP-U endpoint")?;
        bootstrap::spawn(
            self,
            tx,
            rx,
            transport,
            datagrams,
            Some(reconnect),
            cancelled,
        )
    }

    /// Both HAL handles must share the actual device clock and use finite I/O waits.
    #[cfg(test)]
    pub fn spawn_with_packet_transport<T: Transport, D: BoundDatagrams>(
        self,
        tx: Box<dyn RadioTx>,
        rx: Box<dyn RadioRx>,
        transport: T,
        datagrams: D,
    ) -> Result<ServingRuntime<T, D>>
    where
        T::Error: std::fmt::Debug,
    {
        self.spawn_with_packet_transport_and_cancellation(tx, rx, transport, datagrams, || false)
    }

    /// Cancellation is checked before transferring handles to RF workers. A closed HNB-GW
    /// association is not replaced.
    #[cfg(test)]
    pub fn spawn_with_packet_transport_and_cancellation<T: Transport, D: BoundDatagrams>(
        self,
        tx: Box<dyn RadioTx>,
        rx: Box<dyn RadioRx>,
        transport: T,
        datagrams: D,
        cancelled: impl Fn() -> bool,
    ) -> Result<ServingRuntime<T, D>>
    where
        T::Error: std::fmt::Debug,
    {
        bootstrap::spawn(self, tx, rx, transport, datagrams, None, cancelled)
    }
}

pub fn fresh_epoch() -> Result<RadioEpoch> {
    let value = NonZeroU64::new(getrandom::u64().context("allocate radio epoch")?)
        .ok_or_else(|| anyhow!("entropy returned a zero radio epoch"))?;
    Ok(RadioEpoch::new(value))
}

impl<T: Transport, D: BoundDatagrams> ServingRuntime<T, D> {
    pub fn take_management_capture_ports(
        &mut self,
    ) -> Option<[crate::radio_io::capture::live::Port; 2]> {
        self.management_capture_ports.take()
    }
    pub fn phase(&self) -> EpochPhase {
        self.epoch.phase()
    }

    pub fn core_phase(&self) -> Option<umts_core::Phase> {
        self.epoch
            .controller()
            .map(|owner| owner.core().gateway().phase())
    }

    pub fn registered_ues(&self) -> usize {
        self.epoch
            .controller()
            .map_or(0, |owner| owner.core().gateway().registered_ues())
    }
    #[cfg(test)]
    pub fn rnc(&self) -> Option<u16> {
        self.epoch.controller().map(|owner| owner.radio().rnc())
    }

    pub fn has_pending_assignment(&self) -> bool {
        self.bearers.assignments.is_pending()
    }

    pub fn counters(&self) -> ServingCounters {
        let mut counters = self.control.counters;
        counters.failed_user_plane = counters
            .failed_user_plane
            .saturating_add(self.bearers.network_failures);
        (
            counters.received_iu_releases,
            counters.completed_iu_releases,
        ) = self.bearers.iu_release_counts();
        counters
    }

    pub fn access_diagnostics(&self) -> &AccessDiagnostics {
        &self.control.access_diagnostics
    }

    pub fn report_failures(&mut self) {
        self.control.report_failures();
        self.bearers.report_failures();
        if let Some(failure) = self.media_error.take() {
            tracing::warn!("Bearer media service failed: {failure:?}");
        }
        if let Some(failure) = self.bearer_error.take() {
            tracing::warn!("Bearer lifecycle service failed: {failure:?}");
        }
    }

    pub fn pending_cleanups(&self) -> usize {
        self.bearers.pending_cleanups()
    }
    pub fn visit_downlink_power(
        &self,
        visit: impl FnMut(
            crate::dedicated_rx::ReceiveIdentity,
            crate::dedicated_tx::DownlinkPowerCounters,
        ),
    ) {
        if let Some(admission) = self
            .epoch
            .controller()
            .and_then(|controller| controller.admission())
        {
            admission.visit_downlink_power(visit);
        }
    }
    pub fn service_receive_diagnostics(
        &mut self,
        visit: impl FnMut(crate::dedicated_rx::ReceiveIdentity, crate::dedicated_rx::ReceivePlanStatus),
    ) -> Result<(), crate::epoch::EpochMutationError> {
        self.epoch.with_controller(|controller, _| {
            if let Some(dedicated) = controller.dedicated_mut() {
                dedicated.service_receive_diagnostics(visit);
            }
        })
    }

    pub fn recovery_required(&self) -> Option<crate::dedicated_rx::ReceiveIdentity> {
        self.bearers.recovery_required()
    }

    pub fn recovery_error(&self) -> Option<&crate::rel99_admission::AdmissionError> {
        self.bearers.recovery_error()
    }

    #[cfg(test)]
    pub fn feedback_lead_chips(&self) -> u64 {
        self.feedback_lead_chips
    }
    pub fn air_origin(&self) -> crate::radio_io::AirClockOrigin {
        self.epoch.air_origin()
    }

    /// True once the HNB-GW reconnect attempts set by `core.iuh.retry_attempts` are spent.
    pub fn core_retry_exhausted(&self) -> bool {
        self.epoch
            .controller()
            .is_some_and(|owner| owner.core().reconnect_exhausted())
    }

    pub fn core_loss(&self) -> Option<umts_core::LossReason> {
        self.epoch
            .controller()
            .and_then(|owner| owner.core().gateway().loss_reason())
    }

    #[cfg(test)]
    pub fn take_assignment(&mut self) -> Result<Option<PendingRabAssignment>, HandoffError> {
        self.bearers.assignments.take()
    }

    pub fn radio_allocations(&self) -> usize {
        self.bearers.outstanding()
    }

    pub fn hsdpa_codes(&self) -> Option<crate::radio_resources::HsdpaCodeSet> {
        self.bearers.hsdpa_codes()
    }

    pub fn visit_hsdpa_diagnostics(
        &self,
        visit: impl FnMut(
            crate::dedicated_rx::ReceiveIdentity,
            crate::hsdpa_session::Phase,
            crate::hsdpa_session::FeedbackSnapshot,
        ),
    ) {
        if let Some(controller) = self.epoch.controller() {
            controller.visit_hsdpa_diagnostics(visit);
        }
    }

    pub fn visit_hsupa_candidates(
        &self,
        visit: impl FnMut(
            crate::dedicated_rx::ReceiveIdentity,
            Option<umts_rrc::capabilities::hsupa::FddHsupa>,
            bool,
            bool,
            bool,
            Option<crate::hsupa_session::Phase>,
            Option<crate::hsupa_rx::IngressCounters>,
            Option<crate::hsupa_session::GrantCounters>,
        ),
    ) {
        if let Some(controller) = self.epoch.controller() {
            controller.visit_hsupa_candidates(visit);
        }
    }

    #[cfg(test)]
    pub fn take_ready_assignment(
        &mut self,
    ) -> Result<Option<PendingRabAssignment>, AssignmentReadinessError> {
        let assignments = &mut self.bearers.assignments;
        self.epoch
            .with_controller(|controller, time| {
                let mut failed = None;
                let pending = assignments
                    .take_ready(|request| {
                        match controller
                            .radio_mut()
                            .ensure_capabilities(request.key(), time.radio)
                        {
                            Ok(crate::signalling::CapabilityReadiness::Ready) => true,
                            Ok(_) => false,
                            Err(error) => {
                                if failed.is_none() {
                                    failed = Some((request, error));
                                }
                                false
                            }
                        }
                    })
                    .map_err(AssignmentReadinessError::Handoff)?;
                if pending.is_none()
                    && let Some((request, error)) = failed
                {
                    return Err(AssignmentReadinessError::Capabilities { request, error });
                }
                Ok(pending)
            })
            .map_err(AssignmentReadinessError::Epoch)?
    }

    #[cfg(test)]
    pub fn service_packet_network(
        &mut self,
    ) -> Result<
        Result<crate::packet_bearers::Step, crate::packet_rabs::Error>,
        crate::epoch::EpochMutationError,
    > {
        let traffic = &mut self.traffic;
        self.epoch.with_controller(|controller, _| {
            let receive = controller
                .dedicated_mut()
                .expect("serving attaches dedicated controller");
            traffic.network_step(receive)
        })
    }

    pub fn step(&mut self, now: Instant) -> Result<(), EpochFailure> {
        let bearers = &mut self.bearers;
        let Some(_) = self.epoch.step_serving(
            now,
            |radio, event| bearers.offer_core(radio, event),
            &mut self.traffic,
            &mut self.control,
        )?
        else {
            return Ok(());
        };
        let bearers = &mut self.bearers;
        let traffic = &mut self.traffic;
        let feedback_lead_chips = self.feedback_lead_chips;
        let service = match self.epoch.with_controller(|controller, time| {
            bearers.service(controller, traffic, time, now, feedback_lead_chips)
        }) {
            Ok(result) => result,
            Err(crate::epoch::EpochMutationError::Failure(error)) => return Err(error),
            Err(crate::epoch::EpochMutationError::Phase(_)) => {
                return Err(EpochFailure::Stopped);
            }
        };
        let service = service.map_err(EpochFailure::Controller)?;
        self.control.counters.accepted_assignments = self
            .control
            .counters
            .accepted_assignments
            .saturating_add(service.accepted as u64);
        self.control.counters.rejected_assignments = self
            .control
            .counters
            .rejected_assignments
            .saturating_add(service.rejected as u64);
        self.control.counters.completed_cleanups = self
            .control
            .counters
            .completed_cleanups
            .saturating_add(service.cleanup.completed as u64);
        self.control.counters.failed_bearers = self.control.counters.failed_bearers.saturating_add(
            (service.cleanup.failures + usize::from(service.failure.is_some())) as u64,
        );
        if self.bearer_error.is_none() {
            self.bearer_error = service
                .cleanup
                .first_failure
                .map(BearerFailure::Cleanup)
                .or(service.failure);
        }
        let media = service.media;
        self.control.counters.failed_user_plane = self
            .control
            .counters
            .failed_user_plane
            .saturating_add(media.failures as u64);
        if self.media_error.is_none() {
            self.media_error = media.first_failure;
        }
        if let Some(identity) = self.bearers.recovery_required() {
            return Err(self.epoch.require_bearer_recovery(identity));
        }
        Ok(())
    }

    pub fn idle_wait(&self) -> Duration {
        self.idle_wait
    }

    pub fn request_stop(&mut self) {
        self.epoch.request_stop();
    }

    /// A deadline retains the same worker owners. Retry this instance rather than starting another radio owner.
    pub fn join(&mut self) -> Result<EpochReport, EpochJoinError> {
        self.epoch.join(self.join_timeout)
    }
}

#[cfg(test)]
mod tests;
