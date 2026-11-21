// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa_rx::{EvidenceLimits, FeedbackPolicy, FeedbackReceiver},
    hsdpa_session::{
        CellSchedulingBudget, DownlinkPower, Error, Phase, QueueBudget, SchedulingBudget,
        SchedulingPreparation, SchedulingSettings, Session, SessionSettings,
    },
    radio_resources::RadioResources,
    test_support::{count, source},
};
use umts_dsp::{dedicated::UplinkDpchCode, uplink_scrambling::UplinkScramblingCode};
use umts_types::hsdpa::{CarrierId, DualCellCategory};

#[test]
fn dual_admission_cannot_replace_an_unsignalled_single_cell_link() {
    let c = connected();
    let source = source(true);
    let mut resources =
        RadioResources::new(c.key.epoch, source.information(), count(2), 0..=1).unwrap();
    let lease = resources
        .reserve_hsdpa(NonZeroU8::new(2).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap());
    let mut receiver = FeedbackReceiver::new(c.key, code, 1, at(0)).unwrap();
    let port = receiver.connect(count(8), count(8)).unwrap();
    let mut session = Session::new(
        c.old,
        Box::new(activation(&c)),
        port,
        SessionSettings {
            incarnation: NonZeroU64::MIN,
            downlink_origin: at(0),
            queues: QueueBudget {
                pdus_per_queue: count(8),
                total_bytes: count(65536),
            },
            codes: *lease.codes(),
            scheduling: None,
        },
    )
    .unwrap_or_else(|rejected| panic!("{:?}", rejected.error));
    let cell = CellSchedulingBudget::new(count(32_000_000));
    let prepared = SchedulingPreparation::new(
        session.procedure().construction().configuration().clone(),
        *lease.codes(),
        SchedulingSettings {
            power: DownlinkPower::new(0.1, 0.1).unwrap(),
            maximum_cqi_age: NonZeroU64::new(CHIPS_PER_FRAME).unwrap(),
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
    .unwrap_or_else(|error| panic!("{error:?}"));
    assert!(cell.available_bytes() < 32_000_000);
    let limits = EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
    assert_eq!(
        session.configure_dual_cell(
            DualCellCategory::new(21).unwrap(),
            NonZeroU64::new(2).unwrap(),
            *lease.codes(),
            prepared,
            FeedbackPolicy {
                acknowledgement: limits,
                cqi: limits
            },
        ),
        Err(Error::Identity)
    );
    assert_eq!(session.phase(), Phase::Prepared);
    assert_eq!(session.carriers().collect::<Vec<_>>(), [CarrierId::Primary]);
    assert!(!session.data_publication_allowed(CarrierId::Secondary));
    // Shared HSDPA plan tables stay with the cell.
    assert_eq!(
        cell.available_bytes() + cell.tables().retained_bytes(),
        32_000_000
    );
    session.begin_retirement();
    assert_eq!(session.phase(), Phase::Retiring);
    assert!(!session.data_publication_allowed(CarrierId::Primary));
    assert!(Box::new(session).try_retire().is_err());
}
