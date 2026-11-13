// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{dedicated_rx::FrameAnchor, hsdpa_rx::FeedbackMode, test_support::at};
use umts_dsp::{DspError, hsdpa::SecondaryCellOrder};
use umts_types::time::CHIPS_PER_FRAME;

// Recorded CS-add mapping: frame 6484, CFN 84, UL offset 1024 chips.
const REBIND_FRAME: u64 = 6484;
const REBIND_CHIPS: u64 = REBIND_FRAME * CHIPS_PER_FRAME;

fn mapping_timing(frame: u64) -> FeedbackTiming {
    FeedbackTiming::new(
        FrameAnchor {
            time: at(frame * CHIPS_PER_FRAME),
            cfn: (frame % 256) as u8,
        },
        FrameAnchor {
            time: at(frame * CHIPS_PER_FRAME + 1024),
            cfn: (frame % 256) as u8,
        },
        at(crate::hsdpa_tx::CONTROL_LEAD_CHIPS),
    )
    .unwrap()
}

#[test]
fn secondary_initialization_uses_current_mapping_even_before_its_anchor() {
    let (mut session, _receiver, _cell) = super::super::dual_tests::prepared_dual();
    let timing = mapping_timing(REBIND_FRAME);
    let activation = session.procedure.construction().activation().time();
    assert!(activation.index < REBIND_CHIPS);
    assert_eq!(
        timing.acknowledgement(timing.next_pdsch(activation).unwrap()),
        Err(DspError::DiscontinuousTime)
    );
    assert_eq!(
        timing.first_acknowledgement(activation),
        Ok(at(REBIND_CHIPS + 32_000))
    );
    assert_eq!(
        timing.first_acknowledgement(at(REBIND_CHIPS - 1)),
        Ok(at(REBIND_CHIPS + 32_000))
    );
    session.ensure_secondary_feedback(timing).unwrap();
    assert_eq!(session.secondary_state(), Some(SecondaryState::Active));
    assert_eq!(
        session
            .feedback_mode
            .unwrap()
            .mode_at(at(REBIND_CHIPS + 32_000)),
        Ok(FeedbackMode::Dual)
    );
}

#[test]
fn feedback_tick_preserves_rebased_active_and_deactivated_secondary() {
    for deactivate in [false, true] {
        let (mut session, _receiver, _cell) = super::super::dual_tests::prepared_dual();
        let origin = at(REBIND_CHIPS + 32_000);
        session.configure_secondary_control(origin).unwrap();
        if deactivate {
            session
                .request_secondary_order(SecondaryCellOrder::Deactivate)
                .unwrap();
            let tx = session
                .prepare_secondary_order(at(REBIND_CHIPS), origin)
                .unwrap();
            session.accept_secondary_order(tx).unwrap();
            let SecondaryControl::Dual { procedure, .. } = &mut session.secondary_control else {
                panic!("configured secondary");
            };
            assert!(procedure.acknowledge(tx, tx.feedback_start));
            session
                .advance_secondary_order(tx.codebook_effective)
                .unwrap();
        }
        let timing = mapping_timing(REBIND_FRAME + 10);
        let rebased = timing.first_acknowledgement(at(0)).unwrap();
        let mode = if deactivate {
            FeedbackMode::Single
        } else {
            FeedbackMode::Dual
        };
        // Same committed rebase as finish_radio_change: only settled procedure.
        let SecondaryControl::Dual {
            origin, procedure, ..
        } = &mut session.secondary_control
        else {
            panic!("configured secondary");
        };
        procedure.rebase_feedback_origin(rebased).unwrap();
        *origin = rebased;
        session.feedback_mode = Some(crate::hsdpa_rx::FeedbackModeSchedule::fixed(rebased, mode));
        for _ in 0..2 {
            session.ensure_secondary_feedback(timing).unwrap();
            assert_eq!(
                session.secondary_state(),
                Some(if deactivate {
                    SecondaryState::Configured
                } else {
                    SecondaryState::Active
                })
            );
            assert_eq!(session.order_progress(), OrderProgress::Idle);
            assert_eq!(session.feedback_mode.unwrap().mode_at(rebased), Ok(mode));
            let SecondaryControl::Dual { origin, .. } = &session.secondary_control else {
                panic!("configured secondary");
            };
            assert_eq!(*origin, rebased);
        }
    }
}

#[test]
fn feedback_tick_preserves_pending_order_identity_and_codebook_boundary() {
    let (mut session, _receiver, _cell) = super::super::dual_tests::prepared_dual();
    let origin = at(REBIND_CHIPS + 32_000);
    session.configure_secondary_control(origin).unwrap();
    session
        .request_secondary_order(SecondaryCellOrder::Deactivate)
        .unwrap();
    let tx = session
        .prepare_secondary_order(at(REBIND_CHIPS), origin)
        .unwrap();
    session.accept_secondary_order(tx).unwrap();
    let request = session.request_secondary_order_feedback(tx).unwrap();
    session
        .ensure_secondary_feedback(mapping_timing(REBIND_FRAME))
        .unwrap();
    assert_eq!(session.order_progress(), OrderProgress::Waiting);
    assert_eq!(
        session.secondary_state(),
        Some(SecondaryState::Deactivating)
    );
    let SecondaryControl::Dual {
        origin: retained,
        feedback,
        ..
    } = &session.secondary_control
    else {
        panic!("configured secondary");
    };
    assert_eq!(*retained, origin);
    assert_eq!(*feedback, Some((request, tx)));
    let mode = session.feedback_mode.unwrap();
    assert_eq!(mode.mode_at(origin), Ok(FeedbackMode::Dual));
    assert_eq!(
        mode.mode_at(tx.codebook_effective),
        Ok(FeedbackMode::Ambiguous)
    );
}
