// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_rx::ReceiveOwner,
    rel99_planner::tests::admission as fixtures,
    test_support::{at, count},
};
use umts_dsp::fractional_sampling::SamplePosition;
use umts_types::CnDomain;
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, SampleInstant},
};

#[test]
fn sequential_cs_then_ps_preserves_combined_configuration_and_ownership() {
    let (initial, addition) = fixtures::sequential_cs_then_ps_plans();
    assert_eq!(initial.rabs().len(), 1);
    assert_eq!(initial.rabs()[0].domain, CnDomain::Cs);
    assert_eq!(addition.retained_rabs().len(), 1);
    assert_eq!(addition.retained_rabs()[0].domain, CnDomain::Cs);
    assert_eq!(addition.rabs().len(), 1);
    assert_eq!(addition.rabs()[0].domain, CnDomain::Ps);
    let target = addition.compiled().configuration();
    assert!(
        target
            .bearers()
            .iter()
            .any(|bearer| bearer.domain == Some(CnDomain::Cs))
    );
    assert!(
        target
            .bearers()
            .iter()
            .any(|bearer| bearer.domain == Some(CnDomain::Ps))
    );
}

#[test]
fn shared_radio_constructor_preserves_exact_compiled_plans_and_selected_anchors() {
    let (_source, _radio, mut resources, prepared) = fixtures::prepared_multi_packet();
    let identity = prepared.allocation.receive_identity();
    let plan = prepared.allocation.plan();
    let mut settings = fixtures::settings(identity.ue.epoch);
    let frame = 4;
    settings.timing.uplink.time = at(frame * CHIPS_PER_FRAME);
    settings.timing.uplink.cfn = frame as u8;
    settings.timing.uplink_frame = frame;
    settings.timing.downlink = settings.timing.uplink;
    settings.timing.downlink_frame = frame;
    settings.samples.first_receive = SamplePosition::new(
        SampleInstant {
            epoch: identity.ue.epoch,
            index: frame * CHIPS_PER_FRAME * settings.samples.rate.get() as u64 + 32,
        },
        0.0,
    )
    .unwrap();
    let owners = build_radio_owners(identity, plan, &settings).unwrap();
    assert_eq!(owners.receiver.session().identity(), identity);
    assert_eq!(
        owners.receiver.session().frame_anchor().time,
        settings.timing.uplink.time
    );
    assert_eq!(
        owners.receiver.session().frame_anchor().cfn,
        settings.timing.uplink.cfn
    );
    assert!(Arc::ptr_eq(
        owners.receiver.session().configured(),
        plan.uplink_configured()
    ));
    assert!(Arc::ptr_eq(owners.transmit.plan(), plan));
    assert_eq!(
        owners.transmit.frame_anchor().time,
        settings.timing.downlink.time
    );
    assert_eq!(owners.transmit.queued_frames(), 0);
    assert!(matches!(
        owners.transmit.drain(),
        crate::dedicated_tx::TransmitDrain::LocalUnsubmitted
    ));
    resources.cancel(prepared.allocation).unwrap();
}

#[test]
fn invalid_receiver_epoch_and_radio_budgets_fail_before_any_frame_pool_reservation() {
    let (_source, radio, mut resources, prepared) = fixtures::prepared_multi_packet();
    let identity = prepared.allocation.receive_identity();
    let before = radio.resources();
    let mut settings = fixtures::settings(identity.ue.epoch);
    settings.samples.first_receive = SamplePosition::new(
        SampleInstant {
            epoch: RadioEpoch::new(NonZeroU64::new(identity.ue.epoch.get() + 1).unwrap()),
            index: 32,
        },
        0.0,
    )
    .unwrap();
    assert!(matches!(
        build_radio_owners(identity, prepared.allocation.plan(), &settings),
        Err(OwnerBuildError::Receive(_))
    ));
    let mut settings = fixtures::settings(identity.ue.epoch);
    settings.receive.maximum_output_bytes = 1;
    assert!(matches!(
        build_radio_owners(identity, prepared.allocation.plan(), &settings),
        Err(OwnerBuildError::Receive(_))
    ));
    let mut settings = fixtures::settings(identity.ue.epoch);
    settings.transmit.maximum_frame_bits = count(1);
    assert!(matches!(
        build_radio_owners(identity, prepared.allocation.plan(), &settings),
        Err(OwnerBuildError::Transmit(_))
    ));
    assert_eq!(radio.resources(), before);
    assert_eq!(resources.outstanding(), 1);
    resources.cancel(prepared.allocation).unwrap();
}
