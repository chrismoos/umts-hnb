// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::serving::{
    CoreRetry, PreparedServing, RecoveryBudget, ShutdownSignals, core_unavailable, fresh_epoch,
};
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use umts_config::Deployment;

use crate::main_management::Management;

const SHUTDOWN_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const STATUS_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const SHUTDOWN_RETRY_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Parser)]
#[command(version, about = "UMTS Home Node B with Rel99 and HSPA support.")]
struct Args {
    #[arg(
        short,
        long,
        help = "Operator configuration file [default: site.local.yaml]"
    )]
    config: Option<PathBuf>,
    #[arg(
        long,
        help = "System defaults file. Defaults to config/defaults.yaml, or defaults.yaml beside an explicit --config file."
    )]
    defaults: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "serve",
        help = "Validate configuration and serving construction without opening radio or network connections."
    )]
    check: bool,
    #[arg(
        long,
        conflicts_with = "check",
        help = "Start the UMTS Home Node B with the configured Rel99 and HSPA services."
    )]
    serve: bool,
    #[arg(long, requires = "serve", value_parser = clap::value_parser!(u64).range(1..=3600))]
    run_seconds: Option<u64>,
}

pub fn run_cli() -> Result<std::process::ExitCode> {
    let args = Args::parse();
    let logging =
        umts_runtime::logging::Logging::initialize(if args.serve { "info" } else { "error" })
            .map_err(anyhow::Error::from_boxed)?;
    match run(args, &logging) {
        Ok(()) => Ok(std::process::ExitCode::SUCCESS),
        Err(error) => {
            tracing::error!(error = %format_args!("{error:#}"), "HNB stopped");
            Ok(std::process::ExitCode::FAILURE)
        }
    }
}

fn run(args: Args, logging: &umts_runtime::logging::Logging) -> Result<()> {
    let (config_path, default_defaults) = match args.config {
        Some(path) => {
            let defaults = path.with_file_name("defaults.yaml");
            (path, defaults)
        }
        None => (
            PathBuf::from("site.local.yaml"),
            PathBuf::from("config/defaults.yaml"),
        ),
    };
    let defaults = args.defaults.unwrap_or(default_defaults);
    let loaded = Deployment::load(&config_path, &defaults)?;
    let config = loaded.config;
    if args.serve {
        logging
            .set_filter(&config.diagnostics.logging.filter)
            .map_err(anyhow::Error::from_boxed)?;
    }
    let effective = loaded.effective;
    let source = serde_yaml::to_string(&effective)?.into_bytes();
    let _rel99 = crate::rel99_planner::ConfiguredPolicy::new(&config)?;
    if args.check || args.serve {
        if args.serve {
            crate::thread_scheduling::install(&config.runtime.scheduling)
                .context("configure HNB thread scheduling")?;
            if config.runtime.lock_memory {
                match umts_runtime::memory::lock_all() {
                    Ok(()) => tracing::info!("Process memory locked"),
                    Err(error) => tracing::warn!("Process memory not locked: {error}"),
                }
            }
        }
        let mut prepared = PreparedServing::new(&config, fresh_epoch()?)?;
        let description = prepared.description();
        if args.check {
            println!("Config is okay.");
            return Ok(());
        }
        tracing::info!(
            "UMTS serving: epoch={} downlink_hz={} uplink_hz={} core={} tx_gain_db={} rx_gain_db={} tx_batch_slots={}",
            description.epoch.get(),
            description.downlink_hz,
            description.uplink_hz,
            description.core,
            config.radio.tx_gain_db,
            config.radio.rx_gain_db,
            config.runtime.tx_batch_slots
        );
        let signals = ShutdownSignals::install().context("install graceful shutdown signals")?;
        if signals.requested() {
            return Ok(());
        }
        let mut management = Management::start(&config, &source, description)?;
        let result = (|| -> Result<()> {
            let began = Instant::now();
            let limit = args.run_seconds.map(Duration::from_secs);
            let mut recovery = RecoveryBudget::new(&config)?;
            let mut core_retry = CoreRetry::new(&config);
            let stopping = |management: &Option<Management>| {
                signals.requested()
                    || limit.is_some_and(|limit| began.elapsed() >= limit)
                    || management
                        .as_ref()
                        .is_some_and(Management::shutdown_requested)
            };
            loop {
                if let Some(management) = &mut management {
                    management.phase(
                        umts_management::proto::Phase::Starting,
                        prepared.description().epoch.get(),
                        "",
                    )?;
                    management.service();
                }
                if stopping(&management) {
                    return Ok(());
                }
                let epoch = prepared.description().epoch.get();
                let mut runtime = match prepared.open_with_cancellation(|| {
                    signals.requested() || limit.is_some_and(|limit| began.elapsed() >= limit)
                }) {
                    Ok(runtime) => runtime,
                    Err(error) if core_unavailable(&error) => {
                        let Some(delay) = core_retry.failed() else {
                            return Err(error.context(format!(
                                "giving up after {} HNB-GW connection attempts",
                                core_retry.failures()
                            )));
                        };
                        tracing::warn!(
                            "HNB-GW connection attempt {} failed, retrying in {}s: {error:#}",
                            core_retry.failures(),
                            delay.as_secs()
                        );
                        if let Some(management) = &mut management {
                            management.phase(
                                umts_management::proto::Phase::Recovering,
                                epoch,
                                &format!("{error:#}"),
                            )?;
                        }
                        let ready_at = Instant::now() + delay;
                        while Instant::now() < ready_at {
                            if let Some(management) = &mut management {
                                management.service();
                            }
                            if stopping(&management) {
                                return Ok(());
                            }
                            std::thread::sleep(
                                SHUTDOWN_RETRY_INTERVAL
                                    .min(ready_at.saturating_duration_since(Instant::now())),
                            );
                        }
                        prepared = PreparedServing::new(&config, fresh_epoch()?)?;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if core_retry.failures() != 0 {
                    tracing::info!(
                        "HNB-GW association opened after {} failed attempts",
                        core_retry.failures()
                    );
                }
                core_retry.connected();
                let air = runtime.air_origin();
                if let Some(management) = &mut management {
                    if let Some(ports) = runtime.take_management_capture_ports() {
                        management.attach_capture_ports(ports);
                    }
                    management.update(runtime.management_snapshot());
                    if let Err(error) = management.publish_received() {
                        tracing::warn!("Management snapshot unavailable: {error:#}");
                    }
                }
                tracing::info!(
                    "Applied radio timebase: chip_rate_hz={} device_tick_rate_hz={}",
                    air.chip_rate.hz(),
                    air.tick_rate
                );
                tracing::info!("UMTS Home Node B runtime started.");
                let mut last_report = Instant::now();
                let mut last_access_revision = 0;
                let mut core_retry_exhausted = false;

                let run = (|| -> Result<()> {
                    while !signals.requested() && limit.is_none_or(|limit| began.elapsed() < limit)
                    {
                        if let Some(management) = &mut management {
                            management.service();
                            if management.shutdown_requested() {
                                break;
                            }
                        }
                        let now = Instant::now();

                        runtime.step(now).map_err(|error| {
                            anyhow!(
                                "radio epoch failed: {error:?}, retained bearer={:?}, recovery error={:?}",
                                runtime.recovery_required(),
                                runtime.recovery_error()
                            )
                        })?;

                        if runtime.core_retry_exhausted() {
                            core_retry_exhausted = true;
                            return Err(anyhow!(
                                "giving up after the configured HNB-GW reconnect attempts, last loss: {:?}",
                                runtime.core_loss()
                            ));
                        }
                        if now.duration_since(last_report) >= STATUS_REPORT_INTERVAL {
                            if let Some(management) = &mut management {
                                management.update(runtime.management_snapshot());
                            }
                            if let Some(management) = &mut management
                                && let Err(error) =
                                    runtime.service_receive_diagnostics(|identity, status| {
                                        management.receive(identity, &status);
                                    })
                            {
                                tracing::warn!("Receive diagnostics unavailable: {error:?}");
                            }
                            let counts = runtime.counters();
                            let access = runtime.access_diagnostics();
                            if config.hspa.uplink.enabled {
                                tracing::debug!(
                                    "HSUPA diagnostics: failures={} visits={} replies={} packet_pdus={} packet_backpressure={}",
                                    counts.failed_hsupa_control,
                                    counts.hsupa_control_visits,
                                    counts.hsupa_control_replies,
                                    counts.hsupa_packet_pdus,
                                    counts.hsupa_packet_backpressure,
                                );
                            }
                            if config.hspa.downlink.enabled {
                                tracing::debug!(
                                    "HSDPA diagnostics: failures={} published={} canceled={} delivered={} retry={} not_transmitted={} feedback_requests={} feedback_reports={} packet_pdus={} stale_canceled={} command_rejected={} last_command_rejection={:?} late_rejections={} late_chips_total={} late_chips_maximum={} last_stale_downlink_floor={:?}",
                                    counts.failed_hsdpa_control,
                                    counts.hsdpa_published,
                                    counts.hsdpa_cancelled,
                                    counts.hsdpa_delivered,
                                    counts.hsdpa_retry,
                                    counts.hsdpa_not_transmitted,
                                    counts.hsdpa_feedback_requests,
                                    counts.hsdpa_feedback_reports,
                                    counts.hsdpa_packet_pdus,
                                    counts.hsdpa_stale_cancelled,
                                    counts.hsdpa_command_rejected,
                                    counts.hsdpa_last_command_rejection,
                                    counts.hsdpa_late_rejections,
                                    counts.hsdpa_late_chips_total,
                                    counts.hsdpa_late_chips_maximum,
                                    counts.hsdpa_last_stale_downlink_floor,
                                );
                            }
                            if access.revision != last_access_revision {
                                tracing::debug!("Common signaling diagnostics: {access:?}");
                                last_access_revision = access.revision;
                            }
                            tracing::debug!(
                                "phase={:?} core={:?} registered_ues={} rach_transports={} connection_requests={} common_rejected={} core_deferred={} core_failed={} control_failed={} dedicated_failed={} admission_failed={} user_plane_failed={} radio_allocations={} pending_rab={}",
                                runtime.phase(),
                                runtime.core_phase(),
                                runtime.registered_ues(),
                                counts.rach_transports,
                                counts.connection_requests,
                                counts.rejected_common,
                                counts.deferred_core,
                                counts.failed_core,
                                counts.failed_control,
                                counts.failed_dedicated,
                                counts.failed_admission,
                                counts.failed_user_plane,
                                runtime.radio_allocations(),
                                runtime.has_pending_assignment()
                            );
                            runtime.report_failures();
                            tracing::debug!(
                                "rab_accepted={} rab_rejected={} cleanup_completed={} cleanup_pending={} bearer_failed={} iu_release_received={} iu_release_completed={}",
                                counts.accepted_assignments,
                                counts.rejected_assignments,
                                counts.completed_cleanups,
                                runtime.pending_cleanups(),
                                counts.failed_bearers,
                                counts.received_iu_releases,
                                counts.completed_iu_releases
                            );
                            if let Some(management) = &mut management {
                                management.publish_received()?;
                            }
                            last_report = now;
                        }

                        std::thread::sleep(runtime.idle_wait());
                    }
                    Ok(())
                })();
                if let Some(management) = &mut management
                    && let Err(error) = management.phase(
                        umts_management::proto::Phase::Stopping,
                        air.chip.epoch.get(),
                        "",
                    )
                {
                    tracing::warn!("Management stopping snapshot unavailable: {error:#}");
                }
                runtime.request_stop();
                if runtime.access_diagnostics().revision != 0 {
                    tracing::debug!(
                        "Final common signaling diagnostics: {:?}",
                        runtime.access_diagnostics()
                    );
                }
                let mut join_reported = None;
                loop {
                    match runtime.join() {
                        Ok(_) => break,
                        Err(error) => {
                            let now = Instant::now();
                            if join_reported.is_none_or(|previous| {
                                now.duration_since(previous) >= SHUTDOWN_REPORT_INTERVAL
                            }) {
                                tracing::warn!(
                                    "Waiting for the same radio workers to stop: {error:?}. No replacement owner will start."
                                );
                                join_reported = Some(now);
                            }
                            std::thread::sleep(SHUTDOWN_RETRY_INTERVAL);
                        }
                    }
                }
                let mut retirement_reported = None;
                let retired = loop {
                    match runtime.try_finish_shutdown() {
                        Ok(report) => break report,
                        Err(retained) => {
                            let now = Instant::now();
                            if retirement_reported.is_none_or(|previous| {
                                now.duration_since(previous) >= SHUTDOWN_REPORT_INTERVAL
                            }) {
                                tracing::warn!(
                                    "Retaining the original radio epoch until shutdown is proven: {:?}",
                                    retained.error
                                );
                                retirement_reported = Some(now);
                            }
                            runtime = *retained.into_runtime();
                            std::thread::sleep(SHUTDOWN_RETRY_INTERVAL);
                        }
                    }
                };
                let report = retired.report();
                tracing::info!(
                    "Radio epoch retired: TX preparation={:?}, TX submission={:?}, RX={:?}, TX stop={:?}, RX stop={:?}",
                    report.tx.preparation,
                    report.tx.submission,
                    report.rx.exit,
                    report.tx.shutdown,
                    report.rx.shutdown
                );
                tracing::debug!(
                    "Final TX health: {:?}. Final RX counters: {:?}",
                    report.tx.health,
                    report.rx.statistics
                );
                tracing::debug!("Final TX recovery: {:?}", report.tx.recovery);
                if let Some(estimates) = report.tx.tpc_estimates {
                    tracing::debug!("Final TPC estimates: {estimates:?}");
                }
                if let Some(failure) = &report.rx.first_dedicated_failure {
                    tracing::warn!("First dedicated RX failure: {failure:?}");
                }
                let worker_failed = [
                    &report.tx.preparation,
                    &report.tx.submission,
                    &report.rx.exit,
                ]
                .iter()
                .any(|exit| **exit != umts_runtime::worker::WorkerExit::Stopped);
                let fault = match run {
                    Err(error) => error,
                    Ok(()) if worker_failed => anyhow!("radio workers did not stop cleanly"),
                    Ok(()) => return Ok(()),
                };
                if stopping(&management) || core_retry_exhausted {
                    return Err(fault);
                }
                if let Some(management) = &mut management {
                    management.phase(
                        umts_management::proto::Phase::Recovering,
                        air.chip.epoch.get(),
                        &format!("{fault:#}"),
                    )?;
                }
                let pending = recovery.schedule(retired, Instant::now()).map_err(|retired| {
                    anyhow!(
                        "cannot schedule another radio restart after {} attempts, epoch={}: {fault:#}",
                        recovery.attempts(),
                        retired.report().epoch.get()
                    )
                })?;
                tracing::info!(
                    "Radio recovery attempt {} after retiring epoch {}: {fault:#}",
                    pending.attempt(),
                    pending.previous_epoch().get()
                );
                while Instant::now() < pending.ready_at() {
                    if let Some(management) = &mut management {
                        management.service();
                    }
                    if stopping(&management) {
                        return Ok(());
                    }
                    std::thread::sleep(
                        SHUTDOWN_RETRY_INTERVAL
                            .min(pending.ready_at().saturating_duration_since(Instant::now())),
                    );
                }
                prepared = pending
                    .prepare(&config, Instant::now())
                    .map_err(|rejected| {
                        anyhow!("prepare fresh serving epoch: {:?}", rejected.error)
                    })?;
            }
        })();
        if let Some(management) = &mut management {
            let management_result = management.finish(&result);
            if result.is_ok() {
                management_result?;
            } else if let Err(error) = management_result {
                tracing::warn!("Management shutdown failed: {error:#}");
            }
        }
        return result;
    }
    bail!("No radio or network connection was opened. Use --check or --serve.")
}
