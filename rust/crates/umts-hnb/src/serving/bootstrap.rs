// SPDX-License-Identifier: AGPL-3.0-or-later

use super::settings::MAXIMUM_ACCESS_DELAY_CHIPS;
use super::*;
use crate::signalling::Resources;
use crate::{
    access_message::MessageLimits,
    common_control::ControlIngress,
    common_rx::{AccessRxSettings, RachQueueLimits, prepare_common_rx},
    common_tx::{CommonAmplitudes, CommonTx},
    controller::{CommonController, ControllerSettings},
    core_link::{CoreLink, Reconnect},
    radio_io::{RxSettings, TimedRx, TxBatch, TxSettings, TxTiming, prepare_tx_batched},
    signalling::{CommonSignalling, SignallingLimits},
};
use anyhow::bail;
use std::num::{NonZeroU16, NonZeroU32, NonZeroUsize};
use umts_core::Gateway;
use umts_dsp::{common_downlink::CommonChannelAmplitudes, pulse_shaping::SamplesPerChip};
use umts_iuh::{
    hnbap::{AccessMode, HnbParameters},
    session::{AssociationId, ControlTime, Session, SessionLimits},
    wire::Limits as WireLimits,
};
use umts_radio::pacing::LeadDurations;
use umts_rlc::transparent::TmLimits;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SECOND, CHIPS_PER_SLOT, ChipInstant};

const CONTROL_COMMANDS: usize = 32;
const COMMANDS_PER_CYCLE: usize = 4;
const ACCESS_COMMANDS: usize = 16;
const TRANSPORT_BITS: usize = 512;
const CONTROL_PDU_BITS: usize = 254;
const MAXIMUM_FORMATS: usize = 2;
const ACCESS_BUFFER_BYTES: usize = 8 * 1024 * 1024;
const PREPARED_SLOTS: usize = 64;
const MAXIMUM_TX_ATTEMPTS: usize = 8;
const MAXIMUM_CALLS: usize = 1;
const MAXIMUM_FUTURE_FRAMES: usize = 8;
const PREPARATION_GUARD_CHIPS: u64 = CHIPS_PER_FRAME * 2;
const INGRESS_AGE_CHIPS: u64 = CHIPS_PER_SECOND;
const CORE_PACKET_BYTES: usize = 64 * 1024;
const CORE_REQUEST_TIMEOUT_MS: u64 = 5_000;
const CORE_CONNECT_TIMEOUT_MS: u64 = 5_000;
const RAB_REQUESTS_PER_DOMAIN: usize = 8;
const SCTP_STREAMS: u16 = 8;
const CPU_RESERVATION_NS_PER_FRAME: u64 = 100_000;
const MICROSECONDS_PER_SECOND: u64 = 1_000_000;
/// RRC identities use this RNC identity until the HNB-GW accepts the registration.
const UNREGISTERED_RNC: u16 = 0;

fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero serving limit")
}
fn ticks(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("nonzero serving duration")
}

pub(super) fn sctp_limits() -> umts_sctp::Limits {
    umts_sctp::Limits {
        maximum_record_bytes: count(CORE_PACKET_BYTES),
        receive_chunk_bytes: count(CORE_PACKET_BYTES),
        outbound_streams: NonZeroU16::new(SCTP_STREAMS).expect("nonzero SCTP streams"),
        inbound_streams: NonZeroU16::new(SCTP_STREAMS).expect("nonzero SCTP streams"),
    }
}

/// Generation 0 is the startup association. Each reconnect uses a distinct association identity.
pub(super) fn gateway<T: Transport>(
    config: &Config,
    epoch: RadioEpoch,
    generation: u64,
    transport: T,
    now: ControlTime,
) -> Result<Gateway<T>> {
    let association =
        NonZeroU64::new(epoch.get().wrapping_add(generation)).unwrap_or(NonZeroU64::MIN);
    let mut session = Session::new(
        epoch,
        AssociationId(association),
        HnbParameters {
            identity: config.cell.hnb_identity.as_bytes().to_vec(),
            plmn: umts_config::parse_plmn(&config.cell.plmn)?,
            lac: config.cell.lac,
            rac: config.cell.rac,
            sac: config.cell.sac,
            cell: config.cell.cell_id,
            access: AccessMode::Open,
            csg: None,
        },
        SessionLimits {
            maximum_ues: count(config.runtime.maximum_ues as usize),
            maximum_rab_requests_per_domain: count(RAB_REQUESTS_PER_DOMAIN),
            maximum_rab_release_requests_per_domain: count(RAB_REQUESTS_PER_DOMAIN),
            request_timeout_ms: ticks(CORE_REQUEST_TIMEOUT_MS),
            codec: WireLimits::default(),
        },
        now,
    )
    .context("create Iuh session")?;
    session.allow_tmsi_registration(config.core.hnbap_tmsi_registration);
    let entries = count(config.runtime.maximum_ues as usize * 4);
    let gateway = Gateway::new(
        session,
        transport,
        umts_core::Limits {
            outbound_entries: entries,
            maximum_packet_bytes: count(CORE_PACKET_BYTES),
            outbound_byte_budget: count(entries.get() * CORE_PACKET_BYTES),
            connect_timeout_ms: ticks(CORE_CONNECT_TIMEOUT_MS),
        },
        now,
    )
    .context("create core gateway")?;
    Ok(gateway)
}

fn startup_tick(now: u64, tick_rate: u64, lead_ms: u32) -> Result<u64> {
    ensure!(tick_rate != 0, "SDR device tick rate is zero");
    let lead = u128::from(tick_rate)
        .checked_mul(u128::from(lead_ms))
        .context("startup lead overflow")?
        .div_ceil(1000);
    now.checked_add(u64::try_from(lead)?)
        .context("device startup timestamp overflow")
}

pub(super) fn spawn<T: Transport, D: BoundDatagrams>(
    mut plan: PreparedServing,
    tx: Box<dyn RadioTx>,
    rx: Box<dyn RadioRx>,
    transport: T,
    datagrams: D,
    reconnect: Option<Reconnect<T>>,
    cancelled: impl Fn() -> bool,
) -> Result<ServingRuntime<T, D>>
where
    T::Error: std::fmt::Debug,
{
    ensure!(!cancelled(), "serving startup canceled");
    let traffic = super::media::MediaPlan::new(&plan.config)?.construct(datagrams)?;
    let mut epoch_settings = settings::epoch_settings(&plan.config)?;
    let tx_samples_per_chip = plan.radio.tx_samples_per_chip;
    let tx_rate = umts_types::time::SampleRate::from_hz(tx.tx_sample_rate_hz())
        .map_err(|error| anyhow!("invalid SDR TX rate: {error:?}"))?;
    let rx_rate = umts_types::time::SampleRate::from_hz(rx.rx_sample_rate_hz())
        .map_err(|error| anyhow!("invalid SDR RX rate: {error:?}"))?;
    let tx_chip_rate = tx_rate
        .divided_by(ticks(tx_samples_per_chip.get() as u64))
        .map_err(|error| anyhow!("invalid SDR TX chip rate: {error:?}"))?;
    let rx_chip_rate = rx_rate
        .divided_by(ticks(SamplesPerChip::Two.get() as u64))
        .map_err(|error| anyhow!("invalid SDR RX chip rate: {error:?}"))?;
    ensure!(tx.supports_timed_tx(), "SDR must support timed TX");
    ensure!(
        tx_rate.hz().round() == CHIPS_PER_SECOND as f64 * tx_samples_per_chip.get() as f64
            && rx_rate.hz().round() == CHIPS_PER_SECOND as f64 * SamplesPerChip::Two.get() as f64
            && tx_chip_rate == rx_chip_rate,
        "SDR TX/RX rates must match configured sampling and share the same chip rate, got TX={} RX={}",
        tx_rate.hz(),
        rx_rate.hz()
    );
    ensure!(
        tx.tick_rate() != 0 && tx.tick_rate() == rx.tick_rate(),
        "SDR TX and RX must share a nonzero device tick rate"
    );
    let capture_geometry = crate::radio_io::capture::live::Geometry {
        direction: crate::radio_io::capture::live::Direction::Receive,
        epoch: plan.epoch.get(),
        sample_rate: rx_rate,
        tick_rate: NonZeroU64::new(tx.tick_rate()).expect("validated tick rate"),
    };
    let core_started = Instant::now();
    let gateway = gateway(&plan.config, plan.epoch, 0, transport, ControlTime(0))?;
    let core = match reconnect {
        Some(reconnect) => CoreLink::with_reconnect(gateway, reconnect),
        None => CoreLink::new(gateway),
    };

    let maximum_ues = count(plan.config.runtime.maximum_ues as usize);
    let origin = ChipInstant {
        epoch: plan.epoch,
        index: 0,
    };
    let amplitude = plan.config.power_control.downlink.cpich_amplitude;
    let common_amplitude = amplitude
        * 10.0_f32.powf(plan.config.power_control.downlink.common_channel_offset_db / 20.0);
    let amplitudes = plan.downlink_power.map_or(
        CommonAmplitudes {
            primary: CommonChannelAmplitudes {
                cpich: amplitude,
                pccpch: common_amplitude,
                primary_sch: common_amplitude,
                secondary_sch: common_amplitude,
            },
            fach: common_amplitude,
            pch: common_amplitude,
        },
        |power| power.common,
    );
    if let Some(power) = &plan.downlink_power {
        power.report();
    }
    let mut source = CommonTx::new(
        plan.cell,
        origin,
        tx_samples_per_chip,
        usize::from(plan.config.receiver.filter_span_chips),
        amplitudes,
        count(CONTROL_COMMANDS),
        MAXIMUM_ACCESS_DELAY_CHIPS,
    )?;
    if plan.config.cell.secondary_carrier.enabled {
        let secondary = &plan.config.cell.secondary_carrier;
        let assignment = crate::radio_assignment::dual_downlink_assignment(
            plan.config.radio.uarfcn_dl,
            plan.config.radio.uarfcn_ul,
            secondary
                .uarfcn_dl
                .context("secondary downlink UARFCN missing")?,
        )?;
        let code = umts_dsp::scrambling::PrimaryScramblingCode::new(
            secondary
                .resolved_scrambling_code(plan.config.cell.physical.downlink_scrambling_code)?,
        )
        .map_err(|error| anyhow!("secondary scrambling code: {error:?}"))?;
        source.enable_secondary_carrier(
            usize::from(plan.config.receiver.filter_span_chips),
            code,
            amplitudes.primary,
            assignment.offsets_hz,
            secondary.power_percent / 100.0,
        )?;
    }
    if let Some(power) = &plan.downlink_power {
        source
            .set_dynamic_power_limit(power.dynamic_power_limit)
            .map_err(|error| anyhow!("configure dynamic downlink power budget: {error:?}"))?;
        source
            .set_component_peak_limit(power.component_peak_limit)
            .map_err(|error| anyhow!("configure final TX component limit: {error:?}"))?;
        if plan
            .config
            .power_control
            .downlink
            .allocation
            .as_ref()
            .is_some_and(|power| power.crest_reduction_enabled)
        {
            source
                .enable_crest_reduction(tx_samples_per_chip)
                .map_err(|error| anyhow!("configure bounded crest reduction: {error:?}"))?;
            tracing::info!(
                "TX bounded crest reduction enabled: total_group_delay_samples={}",
                source.group_delay_samples()
            );
        }
    }
    let origin_tick = startup_tick(
        tx.get_hardware_time().context("read SDR startup time")?,
        tx.tick_rate(),
        plan.config.runtime.startup_lead_ms,
    )?;
    let batch = match plan.config.runtime.tx_batch_slots {
        1 => TxBatch::OneSlot,
        2 => TxBatch::TwoSlots,
        _ => bail!("runtime.tx_batch_slots must be 1 or 2"),
    };
    let hsdpa = plan.bearers.prepare_hsdpa_transmit(&source)?;
    let secondary_hsdpa = plan.bearers.prepare_secondary_hsdpa_transmit(&source)?;
    let hsupa = plan.bearers.prepare_hsupa_transmit(&source)?;
    let (mut preparation, mut pump) = prepare_tx_batched(
        source,
        tx,
        TxSettings {
            origin_tick,
            prepared_slots: count(PREPARED_SLOTS),
            maximum_calls: count(MAXIMUM_CALLS),
            maximum_attempts: count(MAXIMUM_TX_ATTEMPTS),
            timing: TxTiming::DeviceClock(LeadDurations {
                minimum_submission: Duration::from_micros(u64::from(
                    plan.config.runtime.minimum_submission_us,
                )),
                maximum_submission: Duration::from_micros(u64::from(
                    plan.config.runtime.maximum_submission_us,
                )),
                maximum_preparation: Duration::from_micros(u64::from(
                    plan.config.runtime.maximum_preparation_us,
                )),
            }),
        },
        batch,
    )
    .map_err(|error| anyhow!("prepare timed transmit pipeline: {error:?}"))?;
    let management_transmit = plan.config.management.grpc.enabled.then(|| {
        let (port, recorder) =
            crate::radio_io::capture::live::channel(crate::radio_io::capture::live::Geometry {
                direction: crate::radio_io::capture::live::Direction::Transmit,
                sample_rate: tx_rate,
                ..capture_geometry
            });
        pump.attach_live_capture(recorder);
        port
    });
    preparation.spin_wait = plan.config.runtime.spin_wait;
    let ports = preparation
        .control_ports(
            count(CONTROL_COMMANDS),
            count(COMMANDS_PER_CYCLE),
            count(ACCESS_COMMANDS),
            count(COMMANDS_PER_CYCLE),
        )
        .map_err(|error| anyhow!("prepare common transmit control: {error:?}"))?;
    let mut timed = TimedRx::new(
        rx,
        preparation.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: usize::from(plan.config.receiver.filter_span_chips),
            phase: plan.radio.rx_phase,
            maximum_samples: count(CHIPS_PER_SLOT as usize * 2),
            timeout_us: i64::from(plan.config.runtime.rx_timeout_us),
        },
    )
    .map_err(|error| anyhow!("prepare timed receive pipeline: {error:?}"))?;
    let management_receive = plan.config.management.grpc.enabled.then(|| {
        let (port, recorder) = crate::radio_io::capture::live::channel(capture_geometry);
        timed.attach_live_capture(recorder);
        port
    });
    let clock_settings = settings::clock_sampling(&plan.config)?;
    #[cfg(test)]
    let clock_result = match plan.host_clock {
        Some(host) => pump.use_sampled_clock_pair_with_host(
            &mut preparation,
            &mut timed,
            clock_settings,
            host,
        ),
        None => pump.use_sampled_clock_pair(&mut preparation, &mut timed, clock_settings),
    };
    #[cfg(not(test))]
    let clock_result = pump.use_sampled_clock_pair(&mut preparation, &mut timed, clock_settings);
    clock_result.map_err(|error| anyhow!("prepare sampled device clock: {error:?}"))?;
    timed
        .enable_reader(
            preparation.stop_token(),
            count(usize::from(plan.config.runtime.rx_queue_blocks)),
        )
        .map_err(|error| anyhow!("prepare bounded RX reader: {error:?}"))?;
    let (mut receive, rach) = prepare_common_rx(
        timed,
        ports.acquisition,
        SamplesPerChip::Two,
        AccessRxSettings {
            maximum_channels: count(1),
            minimum_coherent_fraction: plan.config.receiver.prach.minimum_coherent_fraction,
            minimum_tx_lead_chips: preparation
                .air_origin()
                .chip_rate
                .ticks_to_samples_ceil(
                    u64::from(plan.config.runtime.minimum_submission_us),
                    ticks(MICROSECONDS_PER_SECOND),
                )
                .map_err(|error| anyhow!("convert access response lead: {error:?}"))?,
            messages: MessageLimits {
                capacity: count(ACCESS_COMMANDS),
                maximum_payload_bits: count(TRANSPORT_BITS),
                maximum_formats: count(MAXIMUM_FORMATS),
                maximum_buffer_bytes: ACCESS_BUFFER_BYTES,
            },
        },
        RachQueueLimits {
            capacity: count(ACCESS_COMMANDS),
            maximum_transport_bits: count(TRANSPORT_BITS),
            maximum_buffer_bytes: TRANSPORT_BITS * ACCESS_COMMANDS,
        },
    )
    .map_err(|error| anyhow!("prepare common receive: {error:?}"))?;
    let security = &plan.config.security;
    let mut signalling = CommonSignalling::with_security_algorithms(
        &ports.transport,
        1,
        UNREGISTERED_RNC,
        SignallingLimits {
            maximum_ues: NonZeroU32::new(plan.config.runtime.maximum_ues)
                .context("nonzero UE capacity")?,
            maximum_sdu_bytes: count(plan.config.runtime.maximum_sdu_bytes as usize),
            queued_sdus_per_bearer: count(plan.config.runtime.queued_sdus_per_bearer as usize),
            ingress_entries: maximum_ues,
            core_entries: maximum_ues,
            response_guard_chips: ticks(CHIPS_PER_SECOND * 5),
            drx_coefficient: plan.config.rrc.cn_drx_cycle_length_coefficient,
            per_ue_cpu_ns_per_frame: CPU_RESERVATION_NS_PER_FRAME,
            tmsi_registration: plan
                .config
                .core
                .hnbap_tmsi_registration
                .then(|| umts_config::parse_plmn(&plan.config.cell.plmn))
                .transpose()?,
            cell_resources: Resources {
                queue_bytes: u64::from(plan.config.runtime.maximum_queue_bytes),
                cpu_ns_per_frame: CPU_RESERVATION_NS_PER_FRAME
                    * u64::from(plan.config.runtime.maximum_ues),
                ..Resources::default()
            },
        },
        umts_security::negotiation::Support {
            uea0: security.allow_uea0,
            uea1: security.allow_uea1,
            uea2: security.allow_uea2,
            uia1: security.allow_uia1,
            uia2: security.allow_uia2,
        },
    )
    .map_err(|error| anyhow!("create common signaling owner: {error:?}"))?;
    signalling.request_cpich_measurements =
        plan.config.cell.measurements.request_cpich_measurements;
    signalling.secondary_cpich_target = plan.bearers.secondary_hsdpa().and_then(|secondary| {
        umts_rrc::measurement::SecondaryCpichTarget::new(
            secondary.uarfcn_dl,
            secondary.primary.index(),
        )
    });
    signalling.measurement_reporting = umts_rrc::measurement::PeriodicalReporting::new(
        plan.config.cell.measurements.ue_measurement_report_count,
        plan.config.cell.measurements.ue_measurement_interval_ms,
    )
    .map_err(|error| anyhow!("UE measurement reporting configuration: {error:?}"))?;
    let common = ControlIngress::new(
        plan.epoch,
        TmLimits {
            maximum_pdus: count(1),
            maximum_pdu_bits: count(CONTROL_PDU_BITS),
            maximum_sdu_bits: count(CONTROL_PDU_BITS),
        },
        count(ACCESS_COMMANDS),
    )
    .map_err(|error| anyhow!("create common control ingress: {error:?}"))?;
    let mut controller = CommonController::new(
        signalling,
        ports.transport,
        rach,
        common,
        core,
        ControllerSettings {
            maximum_ingress_age_chips: ticks(INGRESS_AGE_CHIPS),
            preparation_guard_chips: ticks(PREPARATION_GUARD_CHIPS),
            maximum_future_frames: count(MAXIMUM_FUTURE_FRAMES),
            fach_scan_budget: count(maximum_ues.get() * 3 + 1),
            retirement_scan_budget: maximum_ues,
        },
    )
    .map_err(|error| anyhow!("create common controller: {error:?}"))?;
    let feedback_lead_chips =
        plan.dedicated
            .attach(&mut preparation, &mut receive, &mut controller)?;
    if let Some(bank) = hsdpa {
        preparation
            .attach_hsdpa(bank)
            .map_err(|_| anyhow!("attach shared HSDPA bank to the paired preparation owner"))?;
    }
    if let Some(bank) = secondary_hsdpa {
        preparation
            .attach_hsdpa(bank)
            .map_err(|_| anyhow!("attach secondary HSDPA bank to preparation owner"))?;
    }
    if let Some(bank) = hsupa {
        let settings = plan
            .config
            .hspa
            .uplink
            .resources
            .as_ref()
            .context("HSUPA budgets missing")?;
        controller
            .configure_hsupa_sessions(
                count(usize::from(settings.service_visits)),
                count(usize::from(settings.command_capacity)),
            )
            .map_err(|error| anyhow!("configure HSUPA controller: {error:?}"))?;
        preparation
            .attach_hsupa(bank)
            .map_err(|_| anyhow!("attach HSUPA bank to the paired preparation owner"))?;
    }
    epoch_settings.control_origin = ControlTime(u64::try_from(core_started.elapsed().as_millis())?);
    ensure!(!cancelled(), "serving startup canceled");
    let epoch = CommonEpoch::spawn(preparation, pump, receive, controller, epoch_settings)
        .map_err(|error| anyhow!("start radio epoch: {error:?}"))?;
    Ok(ServingRuntime {
        epoch: Box::new(epoch),
        idle_wait: super::settings::controller_idle_wait(&plan.config),
        join_timeout: Duration::from_millis(u64::from(plan.config.runtime.join_timeout_ms)),
        control: crate::controller::status::ControlStatus::default(),
        feedback_lead_chips,
        traffic,
        bearers: plan.bearers,
        media_error: None,
        bearer_error: None,
        management_capture_ports: management_receive
            .zip(management_transmit)
            .map(|(receive, transmit)| [receive, transmit]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_clock_conversion_rounds_up_and_rejects_overflow() {
        assert_eq!(startup_tick(100, 30_720_000, 250).unwrap(), 7_680_100);
        assert_eq!(startup_tick(0, 3, 1).unwrap(), 1);
        assert!(startup_tick(u64::MAX, 30_720_000, 250).is_err());
        assert!(startup_tick(0, 0, 250).is_err());
    }
}
