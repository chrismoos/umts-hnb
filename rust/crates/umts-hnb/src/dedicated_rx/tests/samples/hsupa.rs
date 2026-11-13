// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsupa_rx::{
    ControlAdmission, ReceivePort, TrackedFormat, TrackedReceiver, TrackedSettings,
};
use std::num::NonZeroU8;
use umts_dsp::{
    dedicated::{LikelihoodSettings, UplinkGains},
    hsupa::TransportPlan,
};
use umts_types::hsupa::{CodeLayout, Etfci, Rel6Category, Tti};
use umts_types::time::{CHIPS_PER_SLOT, SLOTS_PER_FRAME};

#[test]
fn unpublished_session_returns_rx_and_tx_owners_after_worker_acknowledgments() {
    unpublished_cleanup(false, false);
}

#[test]
fn expired_unpublished_session_returns_rx_and_tx_owners_after_worker_acknowledgments() {
    unpublished_cleanup(true, false);
}

#[test]
fn automatic_suffix_preparation_retries_capacity_once_and_returns_both_owners() {
    unpublished_cleanup(false, true);
}

fn unpublished_cleanup(expired: bool, suffix_retry: bool) {
    use crate::{
        dedicated_tx::{DedicatedTransmitRuntime, TransmitRuntimeLimits},
        hsupa_session::{Phase, Preparation, PreparationInputs, Session},
    };
    use umts_rrc::{
        dedicated::ScheduledActivation,
        establishment::TransactionId,
        hsupa::{
            ActivationParameters, ActivationProcedure, ControlConfiguration, ReportingPeriod,
            ScheduledFlow, SchedulingInformation, TransportConfiguration,
        },
    };
    use umts_types::hsupa::{PuncturingLimit as EdchPuncturing, TransportBlockTable};
    let source = crate::test_support::source(true);
    let (mut resources, mut bank, radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let allocated = identities[0];
    bank.begin_retirement(identities[0].ue).unwrap();
    let lease = bank.take_retired(identities[0].ue, at(0)).unwrap().unwrap();
    let (receiver, hsupa, _reports) = if suffix_retry {
        paired_for_layout(coexistence_configuration(), allocated, Some(0))
    } else {
        paired_for(coexistence_configuration(), allocated)
    };
    let (attachment, initial_bytes) = if suffix_retry {
        let control = receiver
            .control_search_geometry()
            .prepare_edch_only(receiver.shared_uplink_code(), at(0), false)
            .unwrap();
        let mut attachment =
            HsupaAttachment::edch_only(hsupa, Box::new(control)).unwrap_or_else(|_| panic!("edch"));
        let initial_bytes = attachment.retained_bytes().unwrap();
        attachment.prepare_initial_suffixes().unwrap();
        assert!(attachment.retained_bytes().unwrap() > initial_bytes);
        (attachment, initial_bytes)
    } else {
        let bytes = hsupa.retained_bytes().unwrap();
        (hsupa.into(), bytes)
    };
    let mut settings = runtime_limits();
    settings.shard.resources.physical_sample_bytes =
        receiver.resources().physical_sample_bytes + initial_bytes;
    let (mut rx, mut rx_port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, settings).unwrap();
    let mut installed = install(&mut rx_port, receiver);
    run(&mut rx);
    assert!(matches!(
        installed.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    let (mut tx, mut tx_ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(8),
        count(1_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(4),
            commands_per_step: count(2),
            feedback_capacity: count(4),
            feedback_per_step: count(2),
            maximum_inflight_retained_bits: count(8_000_000),
            maximum_frame_payload_bits: count(100_000),
        },
    )
    .unwrap();
    assert!(tx.attach_hsupa(bank).is_ok());
    let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
    let construction = crate::hsupa_session::build_coexistence_activation(
        crate::test_support::rel99::configuration(false, true, 10),
        ActivationParameters {
            e_rnti: lease.e_rnti(),
            transport: TransportConfiguration::new(
                Tti::TwoMilliseconds,
                Rel6Category::new(6).unwrap(),
                TransportBlockTable::Zero,
                CodeLayout::new(&[16]).unwrap(),
                EdchPuncturing::new(44).unwrap(),
            )
            .unwrap(),
            control: ControlConfiguration::new(8, 20).unwrap(),
            downlink: lease.controls(),
            references: umts_rrc::hsupa::ReferencePowers::single(Etfci::new(10).unwrap(), 20)
                .unwrap(),
            minimum: Etfci::new(0).unwrap(),
            scheduling: SchedulingInformation {
                without_grant: Some(ReportingPeriod::EveryTti),
                with_grant: None,
                power_offset: 0,
            },
            rv_zero_only: true,
            flows: vec![ScheduledFlow::new(0, 0, 3).unwrap()],
        },
        None,
        activation,
    )
    .unwrap();
    let procedure = Box::new(
        ActivationProcedure::new(
            allocated.ue,
            TransactionId::new(0).unwrap(),
            construction.clone(),
            at(0),
            at(1),
            std::num::NonZeroU64::new(10 * CHIPS_PER_FRAME).unwrap(),
        )
        .unwrap(),
    );
    let preparation = Preparation::new(PreparationInputs {
        receiver: attachment,
        lease,
        configuration: construction,
        maximum_receive_bytes: count(2_000_000),
    })
    .unwrap_or_else(|_| panic!("preparation"));
    let mut session = Session::new(preparation, procedure).unwrap_or_else(|_| panic!("session"));
    assert_eq!(session.phase(), Phase::Preparing);
    session
        .preparation()
        .service(&mut rx_port, &mut tx_ports.control);
    assert_eq!(session.phase(), Phase::Preparing);
    tx.service().unwrap();
    assert!(
        session
            .preparation()
            .route_command(tx_ports.control.try_event().unwrap())
            .is_none()
    );
    session
        .preparation()
        .service(&mut rx_port, &mut tx_ports.control);
    assert_eq!(session.phase(), Phase::Preparing);
    run(&mut rx);
    session
        .preparation()
        .service(&mut rx_port, &mut tx_ports.control);
    if suffix_retry {
        assert_eq!(session.phase(), Phase::Preparing);
        assert_eq!(tx_ports.control.outstanding(), 0);
        run(&mut rx);
        session
            .preparation()
            .service(&mut rx_port, &mut tx_ports.control);
    }
    assert_eq!(session.phase(), Phase::Prepared);
    let cleanup_time = if expired {
        let mut signalling = crate::signalling::tests::controller(&source);
        assert!(session.expire(&mut signalling, at(1)).unwrap());
        assert!(!session.expire(&mut signalling, at(1)).unwrap());
        at(1)
    } else {
        session.abort_unpublished(at(0)).unwrap();
        at(0)
    };
    assert_eq!(session.phase(), Phase::Cancelling);
    for _ in 0..8 {
        session.service_cancellation(&mut rx_port, &mut tx_ports.control, cleanup_time);
        run(&mut rx);
        tx.service().unwrap();
        while let Some(event) = tx_ports.control.try_event() {
            assert!(session.preparation().route_command(event).is_none());
        }
    }
    assert_eq!(session.phase(), Phase::Cancelled);
    let (returned, lease) = session
        .into_cancelled()
        .unwrap_or_else(|_| panic!("cleanup complete"));
    assert!(!returned.receiver().is_started());
    assert_eq!(returned.identity(), allocated);
    if suffix_retry {
        assert_eq!(returned.retained_bytes().unwrap(), initial_bytes);
    }
    resources.release_hsupa_user(lease).unwrap();
    let mut removed = rx_port.remove(allocated).unwrap();
    run(&mut rx);
    assert!(matches!(
        removed.try_result(),
        Some(ReceiveCommandResult::Removed {
            receiver: Some(_),
            ..
        })
    ));
    let mut radios = radios.into_iter();
    resources.release(radios.next().unwrap()).unwrap();
    assert_eq!(tx_ports.control.outstanding(), 0);
}

fn coexistence_configuration() -> Arc<UplinkTfcs> {
    let reference = configuration();
    let original = reference.configured();
    let definitions = original
        .transport_ids()
        .map(|id| {
            let definition = original.definition(id).unwrap();
            TransportDefinition {
                id,
                interval: definition.interval,
                attribute: definition.attribute,
                formats: definition.formats.clone(),
            }
        })
        .collect();
    let combinations = original
        .combinations()
        .iter()
        .filter(|combination| combination.formats.iter().all(|index| *index <= 1))
        .map(|combination| CombinationDefinition {
            tfci: combination.tfci,
            formats: combination.formats.clone(),
        })
        .collect();
    let configured =
        Arc::new(ConfiguredTfcs::new(definitions, combinations, count(200_000)).unwrap());
    let layouts: Vec<_> = [256, 128, 64, 32, 16, 8, 4]
        .into_iter()
        .map(|sf| UplinkLayout::single(sf).unwrap())
        .collect();
    Arc::new(
        UplinkTfcs::new(
            configured,
            &layouts,
            PuncturingLimit::new(100).unwrap(),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(5_000_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    )
}

fn paired(
    config: Arc<UplinkTfcs>,
) -> (
    Box<DedicatedSampleReceiver>,
    Box<TrackedReceiver>,
    ReceivePort,
) {
    paired_for(config, identity(0, 1))
}

fn paired_for(
    config: Arc<UplinkTfcs>,
    allocated: ReceiveIdentity,
) -> (
    Box<DedicatedSampleReceiver>,
    Box<TrackedReceiver>,
    ReceivePort,
) {
    paired_for_layout(config, allocated, None)
}

fn paired_for_layout(
    config: Arc<UplinkTfcs>,
    allocated: ReceiveIdentity,
    maximum_override: Option<u8>,
) -> (
    Box<DedicatedSampleReceiver>,
    Box<TrackedReceiver>,
    ReceivePort,
) {
    let settings = chips::settings(0x123456);
    let code = settings.code.clone();
    let maximum = config
        .configured()
        .combinations()
        .iter()
        .map(|combination| config.allocation(combination.tfci).unwrap().layout().len())
        .max()
        .unwrap();
    let gains = config
        .configured()
        .combinations()
        .iter()
        .map(|combination| (combination.tfci, UplinkGains::new(8, 15).unwrap()))
        .collect();
    let session = DedicatedReceiveSession::new(
        allocated,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        config,
        limits(8),
    )
    .unwrap();
    let receiver = Box::new(
        DedicatedSampleReceiver::new(
            DedicatedChipReceiver::new(session, settings, gains).unwrap(),
            SampleReceiveSettings {
                radio_link: Default::default(),
                rate: SamplesPerChip::Two,
                first_receive: SamplePosition::new(sample(20_098), 0.0).unwrap(),
                search_radius_samples: 8,
                minimum_coherent_fraction: 0.05,
            },
        )
        .unwrap(),
    );
    let mut hsupa = Box::new(
        TrackedReceiver::new(
            code,
            TrackedSettings {
                identity: allocated,
                anchor: FrameAnchor {
                    time: at(0),
                    cfn: 0,
                },
                interval: Tti::TwoMilliseconds,
                maximum_dpdch: maximum_override.unwrap_or(maximum as u8),
                hsdpa_configured: false,
                control_gain_ratio: 0.7,
                likelihood: LikelihoodSettings::new(100.0).unwrap(),
                admission: ControlAdmission::new(0.99, 0.5).unwrap(),
                rv_zero_only: true,
                iterations: NonZeroU8::new(6).unwrap(),
                retained_sample_bytes: count(1_000_000),
                soft_buffer_bytes: count(100_000),
                scratch_bytes: count(100_000),
            },
            vec![TrackedFormat {
                identity: Etfci::new(10).unwrap(),
                plan: Arc::new(
                    TransportPlan::new(
                        count(120),
                        Tti::TwoMilliseconds,
                        Rel6Category::new(6).unwrap(),
                        &[16],
                        count(100_000),
                    )
                    .unwrap(),
                ),
                layout: CodeLayout::new(&[16]).unwrap(),
                gain_ratios: vec![0.5],
            }],
        )
        .unwrap(),
    );
    let output = hsupa.connect(count(2), count(100_000)).unwrap();
    (receiver, hsupa, output)
}

#[test]
fn source_query_returns_the_actual_receiver_code_and_frame_anchor() {
    let (receiver, hsupa, _reports) = paired(coexistence_configuration());
    let identity = receiver.session().identity();
    let anchor = receiver.session().frame_anchor();
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut missing = port.hsupa_source(identity).unwrap();
    run(&mut runtime);
    assert!(matches!(
        missing.try_result(),
        Some(ReceiveCommandResult::HsupaSource { source: None, .. })
    ));
    let mut installed = install(&mut port, receiver);
    run(&mut runtime);
    assert!(matches!(
        installed.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    let mut ticket = port.hsupa_source(identity).unwrap();
    run(&mut runtime);
    let Some(ReceiveCommandResult::HsupaSource {
        identity: actual,
        source: Some(source),
        ..
    }) = ticket.try_result()
    else {
        panic!("active receiver source");
    };
    assert_eq!(actual, identity);
    assert_eq!(source.anchor.time, anchor.time);
    assert_eq!(source.anchor.cfn, anchor.cfn);
    assert!(hsupa.uses_code(&source.code));
}

#[test]
fn admitted_startup_suffix_requires_real_pilots_and_restores_full_frame_fallback() {
    startup_suffix_fallback(false);
}

#[test]
fn automatic_startup_suffix_uses_history_and_preserves_fallback() {
    startup_suffix_fallback(true);
}

#[test]
fn automatic_suffix_capacity_rejection_returns_ownership_for_initial_retry() {
    let (receiver, hsupa, _reports) =
        paired_for_layout(coexistence_configuration(), identity(0, 1), Some(0));
    let control = receiver
        .control_search_geometry()
        .prepare_edch_only(receiver.shared_uplink_code(), at(0), false)
        .unwrap();
    let mut candidate =
        HsupaAttachment::edch_only(hsupa, Box::new(control)).unwrap_or_else(|_| panic!("edch"));
    let initial_bytes = candidate.retained_bytes().unwrap();
    candidate.prepare_initial_suffixes().unwrap();
    let pooled_bytes = candidate.retained_bytes().unwrap();
    assert!(pooled_bytes > initial_bytes);
    let mut settings = runtime_limits();
    settings.shard.resources.physical_sample_bytes =
        receiver.resources().physical_sample_bytes + initial_bytes;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, settings).unwrap();
    let mut ticket = install(&mut port, receiver);
    run(&mut runtime);
    assert!(matches!(
        ticket.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    let mut ticket = port
        .attach_hsupa_at(candidate, at(0), count(pooled_bytes))
        .unwrap_or_else(|_| panic!("attach"));
    run(&mut runtime);
    let Some(ReceiveCommandResult::HsupaRejected {
        error,
        receiver: mut candidate,
    }) = ticket.try_result()
    else {
        panic!("capacity rejection")
    };
    assert!(matches!(
        error,
        ReceiveError::Capacity
            | ReceiveError::ResourceBudget {
                resource: super::super::super::ReceiveResource::PhysicalSampleBytes,
                ..
            }
    ));
    assert_eq!(candidate.retained_bytes().unwrap(), pooled_bytes);
    assert!(candidate.discard_initial_suffixes());
    assert!(!candidate.discard_initial_suffixes());
    assert_eq!(candidate.retained_bytes().unwrap(), initial_bytes);
    let mut ticket = port
        .attach_hsupa_at(candidate, at(0), count(initial_bytes))
        .unwrap_or_else(|_| panic!("initial retry"));
    run(&mut runtime);
    assert!(matches!(
        ticket.try_result(),
        Some(ReceiveCommandResult::HsupaAttached { .. })
    ));
}

fn startup_suffix_fallback(automatic: bool) {
    use umts_dsp::{
        dedicated::UplinkControlFormat,
        uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
    };
    for (start_slot, damage, gap, tiny, full_first) in [3_u8, 6, 9, 12]
        .into_iter()
        .flat_map(|slot| {
            [
                (false, false, false),
                (true, false, false),
                (false, true, false),
                (false, false, true),
            ]
            .map(|(damage, gap, tiny)| (slot, damage, gap, tiny, false))
        })
        .chain(automatic.then_some((3, false, true, false, true)))
    {
        let (mut receiver, hsupa, mut reports) =
            paired_for_layout(coexistence_configuration(), identity(0, 1), Some(0));
        let control = receiver
            .control_search_geometry()
            .prepare_edch_only(receiver.shared_uplink_code(), at(0), false)
            .unwrap();
        let mut candidate =
            HsupaAttachment::edch_only(hsupa, Box::new(control)).unwrap_or_else(|_| panic!("edch"));
        let before = candidate.retained_bytes().unwrap();
        assert!(candidate.prepare_startup_suffix(at(0), 1).is_err());
        assert!(
            candidate
                .prepare_startup_suffix(
                    ChipInstant {
                        index: at(0).index + 1,
                        ..at(0)
                    },
                    12
                )
                .is_err()
        );
        assert_eq!(candidate.retained_bytes().unwrap(), before);
        for frame in [
            ChipInstant {
                epoch: umts_types::RadioEpoch::new(NonZeroU64::new(2).unwrap()),
                ..at(0)
            },
            ChipInstant { index: 0, ..at(0) },
            ChipInstant {
                index: u64::MAX - (u64::MAX - at(0).index) % CHIPS_PER_FRAME,
                ..at(0)
            },
        ] {
            assert!(candidate.prepare_startup_suffix(frame, 12).is_err());
            assert_eq!(candidate.retained_bytes().unwrap(), before);
        }
        if automatic {
            candidate.prepare_initial_suffixes().unwrap();
            assert!(candidate.prepare_initial_suffixes().is_err());
        } else {
            candidate.prepare_startup_suffix(at(0), start_slot).unwrap();
        }
        let bytes = candidate.retained_bytes().unwrap();
        assert!(bytes > before);
        assert!(candidate.prepare_startup_suffix(at(0), 9).is_err());
        assert_eq!(candidate.retained_bytes().unwrap(), bytes);
        for (activation, maximum, expected) in [
            (at(1), bytes, ReceiveError::Configuration),
            (at(0), bytes - 1, ReceiveError::Capacity),
        ] {
            if automatic && activation.index > at(0).index {
                continue;
            }
            let Err((error, returned)) =
                receiver.attach_prepared_hsupa_at(candidate, activation, count(maximum))
            else {
                panic!("invalid suffix admission accepted")
            };
            assert_eq!(error, expected);
            candidate = returned;
            assert_eq!(candidate.retained_bytes().unwrap(), bytes);
            assert_eq!(receiver.timing_slot_for_test(), 0);
        }
        let mut settings = runtime_limits();
        settings.shard.resources.physical_sample_bytes =
            receiver.resources().physical_sample_bytes + bytes;
        if tiny {
            settings.history_samples = count(10_000);
        }
        let (mut runtime, mut port) =
            DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, settings).unwrap();
        let mut install = install(&mut port, receiver);
        run(&mut runtime);
        assert!(matches!(
            install.try_result(),
            Some(ReceiveCommandResult::Installed(_))
        ));
        let mut attachment = port
            .attach_hsupa_at(candidate, at(0), count(bytes))
            .unwrap_or_else(|_| panic!("attach"));
        run(&mut runtime);
        assert!(matches!(
            attachment.try_result(),
            Some(ReceiveCommandResult::HsupaAttached { .. })
        ));
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        UplinkLongSequence::new(UplinkScramblingCode::new(0x123456).unwrap(), 0)
            .unwrap()
            .write(&mut scrambling);
        let format = UplinkControlFormat::new(1).unwrap();
        let frame: Vec<_> = scrambling
            .iter()
            .enumerate()
            .map(|(chip, code)| {
                let slot = chip / CHIPS_PER_SLOT as usize;
                let symbol = chip % CHIPS_PER_SLOT as usize / 256;
                let negative = symbol < 8
                    && format.pilots().word(slot as u8).unwrap() & (1 << (7 - symbol)) != 0;
                *code * Complex32::new(0.0, if negative { -1.0 } else { 1.0 })
            })
            .collect();
        let mut samples: Vec<_> = (0..3)
            .flat_map(|_| frame.iter().flat_map(|chip| [*chip, *chip]))
            .collect();
        samples.resize(samples.len() + 64, Complex32::default());
        if damage {
            samples[(usize::from(start_slot) + 1) * 5120..(usize::from(start_slot) + 2) * 5120]
                .fill(Complex32::default());
        }
        let prefix = if full_first { 64 } else { 0 };
        if full_first {
            samples.splice(..0, [Complex32::default(); 64]);
        }
        let offset = if full_first {
            0
        } else {
            (usize::from(start_slot) - 1) * 5120 + 64
        };
        let mut checked_partial = false;
        let mut gap_sent = false;
        for (i, input) in samples[offset..].chunks(127).enumerate() {
            let first = 20_098 + (offset + i * 127) as u64 - prefix;
            let discontinuity = gap
                && !gap_sent
                && if full_first {
                    first >= 20_098 + 76800 + 64
                } else {
                    i == 20
                };
            gap_sent |= discontinuity;
            runtime
                .feed(matched(first, input, 1, discontinuity))
                .unwrap();
            for _ in 0..8 {
                run(&mut runtime);
                while reports.peek().is_some() {
                    assert!(reports.release());
                }
            }
            if !checked_partial && first + input.len() as u64 >= 20_098 + 76800 + 32 {
                let mut observed = port.observe(identity(0, 1)).unwrap();
                run(&mut runtime);
                let Some(ReceiveCommandResult::Observed {
                    status: Some(status),
                    ..
                }) = observed.try_result()
                else {
                    panic!("status")
                };
                assert_eq!(status.chips.complete_frames, u64::from(full_first));
                if !full_first {
                    assert_eq!(
                        status.tracking.radio_link.state,
                        super::super::super::RadioLinkSyncState::Initial
                    );
                }
                checked_partial = true;
            }
        }
        for _ in 0..32 {
            run(&mut runtime);
            while reports.peek().is_some() {
                assert!(reports.release());
            }
        }
        let mut removed = port.remove(identity(0, 1)).unwrap();
        run(&mut runtime);
        let Some(ReceiveCommandResult::Removed {
            receiver: Some(receiver),
            ..
        }) = removed.try_result()
        else {
            panic!("removed")
        };
        let recovered = !damage && !gap && !tiny;
        assert_eq!(
            receiver.counters().sampled_slots,
            if recovered {
                30 + u64::from(15 - start_slot)
            } else {
                30
            },
            "start_slot={start_slot} damage={damage} gap={gap} tiny={tiny}"
        );
        assert_eq!(receiver.chip_counters().complete_frames, 2);
        assert_eq!(receiver.timing_slot_for_test(), 0);
        assert!(checked_partial);
        if damage {
            assert!(receiver.counters().rejected_frames > 0);
        }
    }
}

#[test]
fn edch_only_chip_suffix_preserves_ttis_without_full_frames_or_same_frame_restarts() {
    use umts_dsp::{
        bits::Bit,
        channel_codes::OvsfCode,
        hsupa::{ControlInformation, TransportEncoder},
        uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
    };
    use umts_types::hsupa::Rsn;
    for (first, gap) in (0..15_u8).map(|first| (first, false)).chain([(12, true)]) {
        let config = coexistence_configuration();
        let (sample_receiver, hsupa, mut port) =
            paired_for_layout(config.clone(), identity(0, 1), Some(0));
        let mut settings = chips::settings(0x123456);
        settings.code = sample_receiver.shared_uplink_code();
        let gains = config
            .configured()
            .combinations()
            .iter()
            .map(|row| (row.tfci, UplinkGains::new(8, 15).unwrap()))
            .collect();
        let session = DedicatedReceiveSession::new(
            identity(0, 1),
            FrameAnchor {
                time: at(0),
                cfn: 0,
            },
            config,
            limits(8),
        )
        .unwrap();
        let mut receiver = DedicatedChipReceiver::new(session, settings, gains).unwrap();
        let mut control = sample_receiver
            .control_search_geometry()
            .prepare_edch_only(sample_receiver.shared_uplink_code(), at(0), false)
            .unwrap();
        let bytes = hsupa.retained_bytes().unwrap();
        receiver
            .attach_hsupa_inner(hsupa, at(0), count(bytes), true)
            .unwrap_or_else(|_| panic!("suffix attachment"));
        receiver
            .activate_edch_only_control(at(0), &mut control)
            .unwrap();
        let plan = Arc::new(
            TransportPlan::new(
                count(120),
                Tti::TwoMilliseconds,
                Rel6Category::new(6).unwrap(),
                &[16],
                count(100_000),
            )
            .unwrap(),
        );
        let payload: Vec<_> = (0..120).map(|n| Bit::from(n % 3 == 0)).collect();
        let mut encoded = vec![Bit::Zero; plan.physical_bits()];
        TransportEncoder::new(plan)
            .unwrap()
            .encode(&payload, 0, &mut encoded)
            .unwrap();
        let information = ControlInformation {
            format: Etfci::new(10).unwrap(),
            sequence: Rsn::new(0).unwrap(),
            happy: true,
        }
        .encode();
        let format = umts_dsp::dedicated::UplinkControlFormat::new(1).unwrap();
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        UplinkLongSequence::new(UplinkScramblingCode::new(0x123456).unwrap(), 0)
            .unwrap()
            .write(&mut scrambling);
        let mut data_code = [0_f32; 16];
        OvsfCode::new(16, 4).unwrap().write(&mut data_code).unwrap();
        let sign = |bit| if bit == Bit::Zero { 1.0 } else { -1.0 };
        let waveform: Vec<_> = (0..CHIPS_PER_FRAME as usize)
            .map(|chip| {
                let slot = chip / CHIPS_PER_SLOT as usize;
                let symbol = chip % CHIPS_PER_SLOT as usize / 256;
                let pilot = if symbol < 8
                    && format.pilots().word(slot as u8).unwrap() & (1 << (7 - symbol)) != 0
                {
                    -1.0
                } else {
                    1.0
                };
                let control_code = if chip % 256 < 128 { 1.0 } else { -1.0 };
                Complex32::new(
                    0.7 * sign(information[(slot % 3) * 10 + symbol]) * control_code
                        + 0.5 * sign(encoded[(chip / 16) % encoded.len()]) * data_code[chip % 16],
                    pilot,
                ) * scrambling[chip]
                    * 0.03
            })
            .collect();
        let mut decoded = Vec::new();
        for frame in 0..2 {
            for slot in if frame == 0 { usize::from(first) } else { 0 }..15 {
                if gap && frame == 0 && slot == 13 {
                    continue;
                }
                let start = slot * CHIPS_PER_SLOT as usize;
                let progress = receiver
                    .receive_slot(
                        identity(0, 1),
                        ChipInstant {
                            index: at(frame).index + slot as u64 * CHIPS_PER_SLOT,
                            ..at(frame)
                        },
                        &waveform[start..start + CHIPS_PER_SLOT as usize],
                    )
                    .unwrap();
                if frame == 0 && first != 0 {
                    assert!(!matches!(progress, ChipProgress::Frame { .. }));
                }
                for _ in 0..1000 {
                    receiver
                        .service_hsupa(
                            &mut umts_dsp::coding::DecodeWork::new(count(100_000)),
                            |_| {},
                        )
                        .unwrap();
                    if let Some(result) = port.peek() {
                        assert_eq!(result.payload, payload.as_slice());
                        assert!(matches!(
                            result.report.feedback,
                            umts_mac::hsupa::Reception::Ack { deliver: true }
                        ));
                        decoded.push(result.report.absolute_tti);
                        assert!(port.release());
                    }
                    if !receiver.hsupa_pending() {
                        break;
                    }
                }
            }
        }
        let expected: Vec<_> = if gap {
            5..10
        } else {
            u64::from(first).div_ceil(3)..10
        }
        .collect();
        assert_eq!(decoded, expected, "first={first} gap={gap}");
        assert_eq!(
            receiver.counters().complete_frames,
            if first == 0 { 2 } else { 1 }
        );
        assert_eq!(receiver.counters().interrupted_frames, u64::from(gap));
        assert_eq!(receiver.counters().ignored_slots, u64::from(gap));
        assert_eq!(
            receiver.counters().slots,
            if gap { 16 } else { 30 - u64::from(first) }
        );
        assert_eq!(receiver.counters().accepted_frames, 0);
    }
}

#[test]
fn edch_only_attachment_returns_search_and_restores_dch_payloads_at_boundary() {
    use umts_dsp::{
        dedicated::UplinkControlFormat,
        uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
    };

    let config = coexistence_configuration();
    let (receiver, hsupa, mut reports) = paired_for_layout(config.clone(), identity(0, 1), Some(0));
    let control = Box::new(
        receiver
            .control_search_geometry()
            .prepare_edch_only(receiver.shared_uplink_code(), at(0), false)
            .unwrap(),
    );
    let control_address = std::ptr::from_ref(control.as_ref());
    let candidate =
        HsupaAttachment::edch_only(hsupa, control).unwrap_or_else(|_| panic!("control candidate"));
    let added_bytes = candidate.retained_bytes().unwrap();
    let mut settings = runtime_limits();
    settings.shard.resources.physical_sample_bytes =
        receiver.resources().physical_sample_bytes + added_bytes;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, settings).unwrap();
    let mut installed = install(&mut port, receiver);
    run(&mut runtime);
    assert!(matches!(
        installed.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    let mut attached = port
        .attach_hsupa_at(candidate, at(0), count(added_bytes))
        .unwrap_or_else(|_| panic!("attach"));
    run(&mut runtime);
    assert!(matches!(
        attached.try_result(),
        Some(ReceiveCommandResult::HsupaAttached { .. })
    ));
    let mut cancelled = port.cancel_hsupa(identity(0, 1), at(0)).unwrap();
    run(&mut runtime);
    let Some(ReceiveCommandResult::HsupaCancelled {
        receiver: candidate,
        ..
    }) = cancelled.try_result()
    else {
        panic!("canceled resources");
    };
    assert_eq!(candidate.retained_bytes().unwrap(), added_bytes);
    assert_eq!(
        std::ptr::from_ref(candidate.control.as_deref().unwrap()),
        control_address
    );
    let mut attached = port
        .attach_hsupa_at(candidate, at(0), count(added_bytes))
        .unwrap_or_else(|_| panic!("reattach"));
    run(&mut runtime);
    assert!(matches!(
        attached.try_result(),
        Some(ReceiveCommandResult::HsupaAttached { .. })
    ));

    let format = UplinkControlFormat::new(1).unwrap();
    let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    UplinkLongSequence::new(UplinkScramblingCode::new(0x123456).unwrap(), 0)
        .unwrap()
        .write(&mut scrambling);
    let mut chips = vec![Complex32::default(); 32];
    for _ in 0..4 {
        chips.extend(scrambling.iter().enumerate().map(|(chip, scramble)| {
            let slot = chip / CHIPS_PER_SLOT as usize;
            let symbol = chip % CHIPS_PER_SLOT as usize / 256;
            let word = format.pilots().word(slot as u8).unwrap();
            let negative = symbol < format.pilots().bits()
                && word & (1 << (format.pilots().bits() - 1 - symbol)) != 0;
            *scramble * Complex32::new(0.0, if negative { -1.0 } else { 1.0 })
        }));
    }
    for frame in 4..8 {
        chips.extend(chips::waveform(
            &config,
            chips::WaveformSettings {
                frame,
                voice: 1,
                packet: 1,
                seed: 7,
                gain: 1.0,
                cfo: 0.0,
                code: 0x123456,
            },
        ));
    }
    chips.resize(chips.len() + 64, Complex32::default());
    let mut shaper = PulseShaper::new(SamplesPerChip::Two, 32).unwrap();
    let mut raw = vec![Complex32::default(); shaper.output_length(chips.len()).unwrap()];
    shaper.process(&chips, &mut raw).unwrap();
    let (first, samples) = filtered(&raw);
    let mut results = Vec::new();
    for (index, input) in samples.chunks(5120).enumerate() {
        runtime
            .feed(matched(first + (index * 5120) as u64, input, 1, false))
            .unwrap();
        run(&mut runtime);
        drain(&mut port, &mut results);
        while reports.peek().is_some() {
            assert!(reports.release());
        }
        if index == 20 {
            let mut restoration = port.restore_dch_at(identity(0, 1), at(4)).unwrap();
            run(&mut runtime);
            assert!(
                matches!(restoration.try_result(), Some(ReceiveCommandResult::DchRestorationScheduled { activation, result: Ok(()), .. }) if activation == at(4))
            );
            let mut duplicate = port.restore_dch_at(identity(0, 1), at(5)).unwrap();
            run(&mut runtime);
            assert!(matches!(
                duplicate.try_result(),
                Some(ReceiveCommandResult::DchRestorationScheduled {
                    result: Err(ReceiveError::Time),
                    ..
                })
            ));
        }
        if index < 60 {
            assert!(results.is_empty(), "no Rel99 payload before restoration");
        }
    }
    assert!(runtime.first_failure().is_none());
    assert!(runtime.counters().sampled_slots >= 3 * SLOTS_PER_FRAME);
    assert!(
        !results.is_empty(),
        "restored DPDCH provides decoded payloads"
    );
    let mut observed = port.observe(identity(0, 1)).unwrap();
    run(&mut runtime);
    assert!(
        matches!(observed.try_result(), Some(ReceiveCommandResult::Observed { status: Some(status), .. }) if status.restored_dch_at == Some(at(4)))
    );
    let mut cancelled = port.cancel_hsupa(identity(0, 1), at(0)).unwrap();
    run(&mut runtime);
    assert!(matches!(
        cancelled.try_result(),
        Some(ReceiveCommandResult::HsupaCancellationRejected {
            error: ReceiveError::Time,
            ..
        })
    ));
    let mut detached = port.detach_restored_hsupa(identity(0, 1), at(4)).unwrap();
    run(&mut runtime);
    let Some(ReceiveCommandResult::RestoredHsupaDetached { receiver, .. }) = detached.try_result()
    else {
        panic!("drained E-DCH resources returned without removing the DCH owner");
    };
    assert_eq!(receiver.retained_bytes().unwrap(), added_bytes);
    assert_eq!(
        std::ptr::from_ref(receiver.control.as_deref().unwrap()),
        control_address
    );
    let mut observed = port.observe(identity(0, 1)).unwrap();
    run(&mut runtime);
    assert!(
        matches!(observed.try_result(), Some(ReceiveCommandResult::Observed { status: Some(status), .. }) if status.phase == ReceivePlanPhase::Active && !status.hsupa_pending)
    );
}

#[test]
fn active_receiver_attachment_preserves_rel99_payloads_and_rejects_late_cutover() {
    let config = coexistence_configuration();
    let raw = waveform(&config, 6, 1, 1.0);
    let (first, samples) = filtered(&raw);
    let mut baseline = None;
    for (attached, restore) in [(false, false), (true, false), (true, true)] {
        let (receiver, hsupa, mut reports) = paired(config.clone());
        let (mut runtime, mut port) =
            DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
        let mut installed = install(&mut port, receiver);
        run(&mut runtime);
        assert!(matches!(
            installed.try_result(),
            Some(ReceiveCommandResult::Installed(_))
        ));
        let mut candidate = Some(hsupa);
        let mut results = Vec::new();
        for (index, input) in samples.chunks(5120).enumerate() {
            runtime
                .feed(matched(first + (index * 5120) as u64, input, 1, false))
                .unwrap();
            run(&mut runtime);
            drain(&mut port, &mut results);
            while reports.peek().is_some() {
                assert!(reports.release());
            }
            if attached && index == 20 {
                let mut late = port
                    .attach_hsupa_at(candidate.take().unwrap(), at(0), count(2_000_000))
                    .unwrap_or_else(|_| panic!("command capacity"));
                run(&mut runtime);
                let Some(ReceiveCommandResult::HsupaRejected {
                    error: ReceiveError::Time,
                    receiver,
                }) = late.try_result()
                else {
                    panic!("late activation rejected")
                };
                assert!(!receiver.receiver().is_started());
                let mut accepted = port
                    .attach_hsupa_at(receiver, at(3), count(2_000_000))
                    .unwrap_or_else(|_| panic!("command capacity"));
                run(&mut runtime);
                assert!(
                    matches!(accepted.try_result(), Some(ReceiveCommandResult::HsupaAttached { identity: actual, activation })
                    if actual == identity(0, 1) && activation == at(3))
                );
                if restore {
                    let mut request = port.restore_dch_at(identity(0, 1), at(4)).unwrap();
                    run(&mut runtime);
                    assert!(
                        matches!(request.try_result(), Some(ReceiveCommandResult::DchRestorationScheduled { activation, result: Ok(()), .. }) if activation == at(4))
                    );
                    let mut duplicate = port.restore_dch_at(identity(0, 1), at(5)).unwrap();
                    run(&mut runtime);
                    assert!(matches!(
                        duplicate.try_result(),
                        Some(ReceiveCommandResult::DchRestorationScheduled {
                            result: Err(ReceiveError::Time),
                            ..
                        })
                    ));
                }
            }
        }
        for _ in 0..48 {
            run(&mut runtime);
            drain(&mut port, &mut results);
        }
        if restore {
            let mut observed = port.observe(identity(0, 1)).unwrap();
            run(&mut runtime);
            assert!(
                matches!(observed.try_result(), Some(ReceiveCommandResult::Observed { status: Some(status), .. }) if status.restored_dch_at == Some(at(4)) && !status.hsupa_pending)
            );
            let mut detached = port.detach_restored_hsupa(identity(0, 1), at(4)).unwrap();
            run(&mut runtime);
            let Some(ReceiveCommandResult::RestoredHsupaDetached { receiver, .. }) =
                detached.try_result()
            else {
                panic!("coexistence owner returned");
            };
            assert!(receiver.control.is_none());
            assert_eq!(receiver.identity(), identity(0, 1));
        }
        assert!(!results.is_empty());
        if let Some(baseline) = &baseline {
            assert_eq!(&results, baseline);
        } else {
            baseline = Some(results);
        }
    }
}

#[test]
fn shard_rejection_returns_hsupa_owner_without_charging_memory() {
    let (receiver, hsupa, _reports) = paired(coexistence_configuration());
    let mut settings = runtime_limits();
    settings.shard.resources.physical_sample_bytes = receiver.resources().physical_sample_bytes;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, settings).unwrap();
    let mut installed = install(&mut port, receiver);
    run(&mut runtime);
    assert!(matches!(
        installed.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    let mut ticket = port
        .attach_hsupa_at(hsupa, at(3), count(2_000_000))
        .unwrap_or_else(|_| panic!("command capacity"));
    run(&mut runtime);
    let Some(ReceiveCommandResult::HsupaRejected {
        error: ReceiveError::Capacity,
        receiver,
    }) = ticket.try_result()
    else {
        panic!("shard memory admission")
    };
    assert!(!receiver.receiver().is_started());
    assert_eq!(receiver.identity(), identity(0, 1));
    let mut removal = port.remove(identity(0, 1)).unwrap();
    run(&mut runtime);
    assert!(matches!(
        removal.try_result(),
        Some(ReceiveCommandResult::Removed {
            receiver: Some(_),
            ..
        })
    ));
}

#[test]
fn pending_attachment_cancels_exactly_and_reuses_its_memory_but_started_receiver_is_retained() {
    let config = coexistence_configuration();
    let (receiver, hsupa, mut reports) = paired(config.clone());
    let mut settings = runtime_limits();
    settings.shard.resources.physical_sample_bytes =
        receiver.resources().physical_sample_bytes + hsupa.retained_bytes().unwrap();
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, settings).unwrap();
    let mut installed = install(&mut port, receiver);
    run(&mut runtime);
    assert!(matches!(
        installed.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    let mut attached = port
        .attach_hsupa_at(hsupa, at(3), count(2_000_000))
        .unwrap_or_else(|_| panic!("attachment capacity"));
    run(&mut runtime);
    assert!(matches!(
        attached.try_result(),
        Some(ReceiveCommandResult::HsupaAttached { .. })
    ));
    let mut wrong = port.cancel_hsupa(identity(0, 1), at(4)).unwrap();
    run(&mut runtime);
    assert!(matches!(
        wrong.try_result(),
        Some(ReceiveCommandResult::HsupaCancellationRejected {
            error: ReceiveError::Time,
            ..
        })
    ));
    let mut cancellation = port.cancel_hsupa(identity(0, 1), at(3)).unwrap();
    run(&mut runtime);
    let Some(ReceiveCommandResult::HsupaCancelled {
        identity: actual,
        activation,
        receiver,
    }) = cancellation.try_result()
    else {
        panic!("returned pending receiver")
    };
    assert_eq!(actual, identity(0, 1));
    assert_eq!(activation, at(3));
    assert!(!receiver.receiver().is_started());
    let mut attached = port
        .attach_hsupa_at(receiver, at(3), count(2_000_000))
        .unwrap_or_else(|_| panic!("reattachment capacity"));
    run(&mut runtime);
    assert!(matches!(
        attached.try_result(),
        Some(ReceiveCommandResult::HsupaAttached { .. })
    ));
    let raw = waveform(&config, 6, 1, 1.0);
    let (first, samples) = filtered(&raw);
    let mut results = Vec::new();
    for (index, input) in samples.chunks(5120).enumerate() {
        runtime
            .feed(matched(first + (index * 5120) as u64, input, 1, false))
            .unwrap();
        run(&mut runtime);
        drain(&mut port, &mut results);
        while reports.peek().is_some() {
            assert!(reports.release());
        }
    }
    assert!(!results.is_empty());
    let mut late = port.cancel_hsupa(identity(0, 1), at(3)).unwrap();
    run(&mut runtime);
    assert!(matches!(
        late.try_result(),
        Some(ReceiveCommandResult::HsupaCancellationRejected {
            error: ReceiveError::Time,
            ..
        })
    ));
}
