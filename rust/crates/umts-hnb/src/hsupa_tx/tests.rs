// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    radio_resources::{RadioLease, RadioResources},
    test_support::{at, count, rel99, source},
};
use std::num::NonZeroU8;
use umts_dsp::{
    coding::DecodeBudget,
    dedicated_transport::TransportPlanLimits,
    hsupa::{GrantScope, HarqFeedbackTiming},
    waveform::WaveformWindow,
};
use umts_types::time::SampleInstant;

mod commands;

pub(crate) fn setup(
    source: &CommonTx,
) -> (
    RadioResources,
    Box<TransmitBank>,
    Vec<RadioLease>,
    Vec<ReceiveIdentity>,
) {
    let ConfiguredUsers {
        resources,
        bank,
        radios,
        identities,
        ..
    } = setup_with_configurations(source);
    (resources, bank, radios, identities)
}

pub(crate) struct ConfiguredUsers {
    pub resources: RadioResources,
    pub bank: Box<TransmitBank>,
    pub radios: Vec<RadioLease>,
    pub identities: Vec<ReceiveIdentity>,
    pub configurations: Vec<std::sync::Arc<umts_rrc::dedicated::Rel99Configuration>>,
}

pub(crate) fn setup_with_configurations(source: &CommonTx) -> ConfiguredUsers {
    let mut resources =
        RadioResources::new(at(0).epoch, source.information(), count(4), 17..=30).unwrap();
    let pool = resources.reserve_hsupa().unwrap();
    let mut users = Vec::new();
    let mut radios = Vec::new();
    let mut identities = Vec::new();
    let mut configurations = Vec::new();
    for slot in 0..2 {
        let key = UeKey {
            epoch: at(0).epoch,
            slot,
            generation: NonZeroU64::MIN,
        };
        let radio = resources
            .prepare(
                key,
                &rel99::configuration(false, true, 10),
                count(200_000),
                TransportPlanLimits {
                    maximum_coded_bits: count(200_000),
                    maximum_mapping_entries: count(300_000),
                    decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
                },
            )
            .unwrap();
        configurations.push(radio.plan().configuration().clone());
        let radio = radio.into_lease();
        identities.push(radio.receive_identity());
        users.push(resources.reserve_hsupa_user(&pool, &radio, None).unwrap());
        radios.push(radio);
    }
    assert_ne!(users[0].e_rnti(), users[1].e_rnti());
    assert_ne!(
        users[0].controls().acknowledgement_signature(),
        users[1].controls().acknowledgement_signature()
    );
    let mut bank = TransmitBank::new(source, pool, count(8), count(2_000_000))
        .unwrap_or_else(|_| panic!("control bank"));
    for (lease, identity) in users.into_iter().zip(&identities) {
        bank.register(lease, *identity, Tti::TwoMilliseconds)
            .unwrap();
    }
    ConfiguredUsers {
        resources,
        bank,
        radios,
        identities,
        configurations,
    }
}

fn feedback(identity: ReceiveIdentity) -> FeedbackRequest {
    FeedbackRequest {
        identity,
        absolute_tti: 0,
        timing: HarqFeedbackTiming {
            start: at(0),
            active_chips: 3 * CHIPS_PER_SLOT,
        },
        indicator: Indicator::Positive,
    }
}

#[test]
fn overlapping_feedback_is_per_user_while_shared_grants_are_serialized() {
    let source = source(true);
    let (_, mut bank, _, identities) = setup(&source);
    bank.queue_feedback(feedback(identities[0]), 0.1).unwrap();
    bank.queue_feedback(feedback(identities[1]), 0.1).unwrap();
    assert_eq!(
        bank.queue_feedback(feedback(identities[0]), 0.1),
        Err(Error::Time)
    );
    let grant = AbsoluteGrant::new(10, GrantScope::AllProcesses).unwrap();
    let grant_start = umts_dsp::hsupa::GRANT_FRAME_OFFSET_CHIPS;
    assert_eq!(
        bank.queue_grant(identities[0], at(0), grant, 0.1),
        Err(Error::Time)
    );
    bank.queue_grant(identities[0], at(grant_start), grant, 0.1)
        .unwrap();
    assert_eq!(
        bank.queue_grant(identities[1], at(grant_start), grant, 0.1),
        Err(Error::Conflict)
    );
    bank.queue_grant(
        identities[1],
        at(grant_start + 3 * CHIPS_PER_SLOT),
        grant,
        0.1,
    )
    .unwrap();
    let mut samples = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    bank.mix_slot(at(0), &mut samples).unwrap();
    assert!(samples.iter().any(|sample| sample.norm_sqr() > 0.0));
    let previous = samples.clone();
    assert_eq!(bank.mix_slot(at(0), &mut samples), Err(Error::Time));
    assert_eq!(samples, previous);
}

#[test]
fn generated_control_requires_submission_evidence_and_retirement_keeps_leases() {
    for lost in [false, true] {
        let mut source = source(true);
        let mut writer = source.submission_writer().unwrap();
        let (mut resources, mut bank, radios, identities) = setup(&source);
        for identity in &identities {
            bank.queue_feedback(feedback(*identity), 0.1).unwrap();
        }
        for slot in 0..3 {
            bank.mix_slot(
                at(slot * CHIPS_PER_SLOT),
                &mut vec![Complex32::default(); CHIPS_PER_SLOT as usize],
            )
            .unwrap();
        }
        assert!(bank.take_completed().is_none());
        let instant = |index| SampleInstant {
            epoch: at(0).epoch,
            index,
        };
        let window = WaveformWindow {
            first_sample: instant(0),
            next_sample: instant(
                3 * CHIPS_PER_SLOT * bank.rate as u64 + 2 * bank.group_delay as u64,
            ),
            chip_origin_sample: instant(bank.group_delay as u64),
        };
        assert!(if lost {
            writer.lost(window)
        } else {
            writer.accepted(window)
        });
        for _ in 0..2 {
            assert_eq!(bank.take_completed().unwrap().submitted, !lost);
        }
        for (identity, radio) in identities.iter().zip(radios) {
            let rejected = resources.release(radio).unwrap_err();
            assert_eq!(rejected.error, crate::radio_resources::Error::Capacity);
            bank.begin_retirement(identity.ue).unwrap();
            assert!(bank.take_retired(identity.ue, at(0)).unwrap().is_none());
            resources
                .release_hsupa_user(
                    bank.take_retired(identity.ue, at(3 * CHIPS_PER_SLOT))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
            resources.release(rejected.lease).unwrap();
        }
        let pool = bank
            .release()
            .unwrap_or_else(|_| panic!("empty control bank"));
        resources.release_hsupa(pool).unwrap();
        assert_eq!(resources.outstanding(), 0);
    }
}
