// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    radio_resources::RadioResources,
    test_support::{at, count, rel99, source},
};
use std::num::NonZeroU8;
use umts_dsp::{dedicated::UplinkDpchCode, uplink_scrambling::UplinkScramblingCode};
use umts_rrc::{dedicated::ScheduledActivation, establishment::TransactionId, hsdpa::*};
use umts_types::{
    UeKey,
    hsdpa::{CarrierId, DualCellCategory},
};

pub(super) fn prepared_dual() -> (
    Session,
    crate::hsdpa_rx::FeedbackReceiver,
    CellSchedulingBudget,
) {
    prepared_dual_with_bearers(false)
}

pub(super) fn prepared_dual_with_bearers(
    two: bool,
) -> (
    Session,
    crate::hsdpa_rx::FeedbackReceiver,
    CellSchedulingBudget,
) {
    let source = source(true);
    let mut radio =
        RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
    let lease = radio
        .reserve_hsdpa(NonZeroU8::new(2).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let codes = *lease.codes();
    let ue = UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    };
    let identity = ReceiveIdentity {
        ue,
        configuration: NonZeroU64::MIN,
    };
    let category = DualCellCategory::new(21).unwrap();
    let mut sizes = [None; 8];
    sizes[0] = Some(count(320));
    let configuration = InitialConfiguration::new_mac_ehs(
        if two {
            two_packet_bearers()
        } else {
            rel99::configuration(false, true, 10)
        },
        ActivationParameters {
            h_rnti: 0x8421,
            harq: HarqConfiguration::implicit(
                category.per_cell_category(),
                NonZeroU8::new(6).unwrap(),
            )
            .unwrap(),
            queues: QueueSet::new(
                (0..if two { 2 } else { 1 })
                    .map(|id| QueueConfiguration::new(id, id, sizes, 100, 32).unwrap())
                    .collect(),
            )
            .unwrap(),
            control: ControlChannels::new(&codes.control_codes().collect::<Vec<_>>()).unwrap(),
            measurement: MeasurementFeedback::new(6, FeedbackCycle::Ms2, NonZeroU8::MIN, 8)
                .unwrap(),
            acknowledgement: AckFeedback::new(8, 8, NonZeroU8::MIN).unwrap(),
        },
        (0..if two { 2 } else { 1 })
            .map(|id| MappedBearer {
                bearer: 20 + id,
                flow: id,
                logical: Some(1 + id),
            })
            .collect(),
    )
    .unwrap()
    .with_secondary_cell(
        SecondaryCellConfiguration::new(
            0x8421,
            10662,
            470,
            ControlChannels::new(&codes.control_codes().collect::<Vec<_>>()).unwrap(),
            6,
            false,
        )
        .unwrap(),
    )
    .unwrap();
    let configuration = Arc::new(configuration);
    let construction = Arc::new(
        InitialActivation::schedule(
            configuration.clone(),
            ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
        )
        .unwrap(),
    );
    let procedure = Box::new(
        ActivationProcedure::new(
            ue,
            TransactionId::new(0).unwrap(),
            construction,
            at(0),
            at(1),
            NonZeroU64::new(8 * 38_400).unwrap(),
        )
        .unwrap(),
    );
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(12345).unwrap()).unwrap());
    let mut receiver = crate::hsdpa_rx::FeedbackReceiver::new(ue, code, 1, at(0)).unwrap();
    let feedback = receiver.connect(count(8), count(8)).unwrap();
    let cell = CellSchedulingBudget::new(count(32_000_000));
    let limits = crate::hsdpa_rx::EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
    let policy = crate::hsdpa_rx::FeedbackPolicy {
        acknowledgement: limits,
        cqi: limits,
    };
    let build = || {
        SchedulingPreparation::new(
            configuration.clone(),
            codes,
            SchedulingSettings {
                power: DownlinkPower::new(0.1, 0.1).unwrap(),
                maximum_cqi_age: NonZeroU64::new(38_400).unwrap(),
                selection_budget: count(1024),
                cqi_offset_limit: 0,
                cqi_target_bler_percent: 10,
            },
            SchedulingBudget {
                maximum_formats: count(1024),
                maximum_coded_bits: count(100_000),
                maximum_plan_bytes: count(16_000_000),
                maximum_buffer_bytes: count(2_000_000),
                maximum_encoder_bytes: count(1_000_000),
                maximum_total_bytes: count(16_000_000),
            },
            &cell,
        )
        .unwrap_or_else(|error| panic!("{error:?}"))
        .build()
        .unwrap_or_else(|error| panic!("{error:?}"))
    };
    let session = Session::new_prepared_dual(
        identity,
        procedure,
        feedback,
        SessionSettings {
            incarnation: NonZeroU64::MIN,
            downlink_origin: at(0),
            queues: QueueBudget {
                pdus_per_queue: count(8),
                total_bytes: count(65536),
            },
            codes,
            scheduling: None,
        },
        Some((build(), policy)),
        DualCellSettings {
            category,
            incarnation: NonZeroU64::new(2).unwrap(),
            codes,
            resources: build(),
            feedback: policy,
        },
    )
    .unwrap_or_else(|rejected| panic!("{:?}", rejected.error));
    (session, receiver, cell)
}

pub(crate) fn two_packet_bearers() -> Arc<umts_rrc::dedicated::Rel99Configuration> {
    use umts_rrc::dedicated::{DchDirection, DchTfcs, Rel99Bearer, Rel99Configuration};
    let (uplink, downlink, mut bearers, physical) = rel99::components(false, true, 10);
    let extend = |source: DchTfcs, direction, transport| {
        let mut transports = source.transports().to_vec();
        let mut extra = transports[0].clone();
        extra.id = transport;
        transports.push(extra);
        let combinations = source
            .combinations()
            .iter()
            .flat_map(|row| {
                (0..2).map(move |format| {
                    let mut row = row.clone();
                    row.formats.push(format);
                    row
                })
            })
            .collect();
        DchTfcs::new(direction, transports, combinations).unwrap()
    };
    bearers.push(Rel99Bearer {
        id: 21,
        domain: Some(umts_types::CnDomain::Ps),
        rlc: rel99::am(),
        mapping: rel99::mapping(6, 11, None),
    });
    Arc::new(
        Rel99Configuration::new(
            extend(uplink, DchDirection::Uplink, 6),
            extend(downlink, DchDirection::Downlink, 11),
            bearers,
            physical,
        )
        .unwrap(),
    )
}

#[test]
fn retirement_with_one_process_budget_visits_both_links() {
    let (mut session, _receiver, cell) = prepared_dual();
    assert_eq!(
        session.carriers().collect::<Vec<_>>(),
        [CarrierId::Primary, CarrierId::Secondary]
    );
    assert!(cell.available_bytes() < 32_000_000);
    session.begin_retirement();
    for completed in 1..=6 {
        for _ in 0..2 {
            let step = session.drain_retirement(count(1)).unwrap();
            assert_eq!(step.processes, 1);
            assert_eq!(step.waiting, 0);
        }
        // The secondary must advance even when the per-call budget is smaller
        // than the primary HARQ process count.
        assert_eq!(session.links[0].resolution_cursor, completed % 6);
        assert_eq!(session.links[1].resolution_cursor, completed % 6);
    }
    assert!(!session.data_publication_allowed(CarrierId::Primary));
    assert!(!session.data_publication_allowed(CarrierId::Secondary));
    let retained = Box::new(session)
        .try_retire()
        .err()
        .expect("TX retirement receipt required");
    assert!(cell.available_bytes() < 32_000_000);
    drop(retained);
    assert_eq!(cell.available_bytes(), 32_000_000);
}

impl Session {
    pub(crate) fn fixture_two_bearer_session() -> (
        Self,
        crate::hsdpa_rx::FeedbackReceiver,
        CellSchedulingBudget,
    ) {
        prepared_dual_with_bearers(true)
    }

    pub(crate) fn fixture_enqueue_pdu(&mut self, bytes: &[u8], bits: usize) {
        self.queues.enqueue_fixture(
            0,
            umts_mac::hsdpa::MacdPdu {
                size_index: 0,
                bits: umts_types::bits::Bits::new(bytes, bits).unwrap(),
            },
        );
    }

    pub(crate) fn fixture_packet(
        &self,
        carrier: CarrierId,
        process: umts_mac::hsdpa::ProcessId,
    ) -> Option<umts_mac::hsdpa::PackedPdu> {
        self.links[carrier.index()]
            .packets
            .payload(process)
            .unwrap()
            .map(|block| block.packet())
    }

    pub(crate) fn fixture_rejects_stale_feedback(
        &mut self,
        report: &crate::hsdpa_rx::Report,
    ) -> bool {
        self.links.iter_mut().all(|link| {
            link.packets
                .admit_feedback_cells(report, link.carrier, &link.feedback_policy.unwrap())
                .is_none()
        })
    }
}

#[test]
fn persistent_backlog_waits_before_secondary_trial_and_keeps_queued_data() {
    use umts_dsp::hsdpa::SecondaryCellOrder;
    use umts_types::time::CHIPS_PER_SLOT;
    let (mut session, _receiver, _cell) = prepared_dual();
    session.configure_secondary_control(at(0)).unwrap();
    session.fixture_enqueue_pdu(&[0; 40], 320);
    session.links[1].unusable_cqi_reports = SECONDARY_UNUSABLE_CQI_REPORTS;
    session
        .request_secondary_order(SecondaryCellOrder::Deactivate)
        .unwrap();
    let start = session.procedure.construction().activation().time();
    let tx = session
        .prepare_secondary_order(start, at(start.index + 12 * CHIPS_PER_SLOT))
        .unwrap();
    session.accept_secondary_order(tx).unwrap();
    let SecondaryControl::Dual { procedure, .. } = &mut session.secondary_control else {
        panic!("dual control");
    };
    assert!(procedure.acknowledge(tx, tx.feedback_start));
    session
        .advance_secondary_order(tx.codebook_effective)
        .unwrap();
    assert_eq!(
        session.secondary_quality_target(tx.codebook_effective),
        None
    );
    let retry = at(tx.codebook_effective.index + 1500 * CHIPS_PER_SLOT);
    assert_eq!(session.secondary_quality_target(at(retry.index - 1)), None);
    assert_eq!(
        session.secondary_quality_target(retry),
        Some(SecondaryCellOrder::Activate)
    );
    session.advance_secondary_order(retry).unwrap();
    session
        .request_secondary_order(SecondaryCellOrder::Activate)
        .unwrap();
    let activation = session
        .prepare_secondary_order(retry, at(retry.index + 12 * CHIPS_PER_SLOT))
        .unwrap();
    // Merely requesting/preparing a trial cannot erase the previous bad history.
    assert_eq!(
        session.links[1].unusable_cqi_reports,
        SECONDARY_UNUSABLE_CQI_REPORTS
    );
    session.accept_secondary_order(activation).unwrap();
    assert_eq!(session.links[1].unusable_cqi_reports, 0);
    assert_eq!(session.queues.queued(0), 1);
    let SecondaryControl::Dual { procedure, .. } = &mut session.secondary_control else {
        panic!("dual control");
    };
    assert!(procedure.acknowledge(activation, activation.feedback_start));
    session
        .advance_secondary_order(activation.codebook_effective)
        .unwrap();
    assert_eq!(
        session.secondary_quality_target(activation.codebook_effective),
        None
    );
}
