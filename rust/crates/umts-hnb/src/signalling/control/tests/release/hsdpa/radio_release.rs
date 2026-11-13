// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{BearerLimits, DedicatedBearers, MappingActivation},
    dedicated_rx::{FrameAnchor, ReceiveIdentity},
    hsdpa_rx::FeedbackReceiver,
    hsdpa_session::{Error, Phase, QueueBudget, Session, SessionSettings},
    radio_resources::HsdpaCodeSet,
    rel99_plan::CompiledRel99,
    test_support::count,
};
use umts_dsp::{
    coding::DecodeBudget, dedicated::UplinkDpchCode, dedicated_transport::TransportPlanLimits,
    uplink_scrambling::UplinkScramblingCode,
};
use umts_security::counters::Start;

fn compiled(configuration: Arc<Rel99Configuration>) -> Arc<CompiledRel99> {
    Arc::new(
        CompiledRel99::new(
            configuration,
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    )
}

fn mapping(frame: u64) -> MappingActivation {
    let anchor = FrameAnchor {
        time: at(frame * CHIPS_PER_FRAME),
        cfn: frame as u8,
    };
    MappingActivation {
        uplink: anchor,
        uplink_frame: frame,
        downlink: anchor,
        downlink_frame: frame,
    }
}

pub(super) fn bank(
    c: &Connected,
    identity: ReceiveIdentity,
    configuration: Arc<Rel99Configuration>,
) -> DedicatedBearers {
    bank_at(c, identity, configuration, RELEASE_ACTIVATION_FRAME)
}

pub(super) fn bank_at(
    c: &Connected,
    identity: ReceiveIdentity,
    configuration: Arc<Rel99Configuration>,
    frame: u64,
) -> DedicatedBearers {
    let plan = compiled(configuration);
    let users = super::super::super::frames::user_profiles(plan.clone());
    let mut endpoints = c.radio.signalling_endpoints(c.key).unwrap();
    endpoints.extend(
        users
            .instantiate(c.key, [Some(Start::new(0).unwrap()); 2], at(0))
            .unwrap(),
    );
    let mut bank = DedicatedBearers::from_compiled(
        identity,
        mapping(frame),
        plan,
        endpoints,
        BearerLimits {
            maximum_transport_bits: count(10_000),
            output_slots: count(8),
            maximum_sdu_bytes: c.radio.limits.maximum_sdu_bytes,
            maximum_buffer_bytes: 4_000_000,
        },
    )
    .unwrap();
    bank.activate(identity).unwrap();
    bank
}

fn active_session(
    c: &mut Connected,
    bank: &mut DedicatedBearers,
    codes: HsdpaCodeSet,
) -> (Session, FeedbackReceiver) {
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap());
    let mut receiver = FeedbackReceiver::new(c.key, code, 1, at(0)).unwrap();
    let port = receiver.connect(count(8), count(8)).unwrap();
    let mut session = Session::new(
        c.old,
        Box::new(activation(c)),
        port,
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
    )
    .unwrap_or_else(|rejected| panic!("{:?}", rejected.error));
    session
        .publish(
            &mut c.radio,
            bank,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    let receipt = session.procedure().receipt().unwrap();
    publish_release(c, receipt);
    let provenance = ReceiveProvenance::Path(c.old.configuration);
    receive(c, complete(session.procedure().transaction()), provenance);
    let now = RELEASE_ACTIVATION_FRAME * CHIPS_PER_FRAME;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(now + 20),
            security_time(),
            |message, mut security| {
                session
                    .response(&message, &mut security, |_, _| Ok(()))
                    .map(|_| ())
            },
        ),
        Some(Ok(c.key)),
    );
    session
        .commit(&mut c.radio, |_| Ok::<_, SignallingError>(()))
        .unwrap();
    acknowledge(&mut c.radio, c.key, &mut c.sender, receipt, now + 100);
    session.finish_command(&mut c.radio).unwrap();
    assert_eq!(session.phase(), Phase::Active);
    (session, receiver)
}

#[test]
fn active_successor_retains_matching_hsdpa_or_retires_absent_route_without_freeing_tx_early() {
    for release_packet in [false, true] {
        let mut c = connected();
        let source = crate::test_support::source(true);
        let mut resources = crate::radio_resources::RadioResources::new(
            c.key.epoch,
            source.information(),
            count(2),
            0..=1,
        )
        .unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(2).unwrap(), NonZeroU8::MIN)
            .unwrap();
        let mut previous = bank(&c, c.old, c.procedure.previous().clone());
        let (mut session, _receiver) = active_session(&mut c, &mut previous, *lease.codes());
        let retained = c.radio.retained_signalling(c.key).unwrap();
        let configuration = if release_packet {
            super::super::super::configuration(&retained)
        } else {
            c.procedure.previous().clone()
        };
        let mut bank = bank(&c, c.new, configuration);
        if !release_packet {
            bank.reserve_hsdpa_route(c.new, session.procedure().construction().clone())
                .unwrap();
        }
        session.begin_radio_change(c.new).unwrap();
        assert_eq!(session.phase(), Phase::ChangingRadio);
        assert_eq!(session.retire_released_route(&bank), Ok(release_packet));
        assert_eq!(session.detached_route(), release_packet);
        if release_packet {
            assert_eq!(session.phase(), Phase::Retiring);
            assert!(matches!(session.active_packets(), Err(Error::Phase)));
            assert!(session.retiring_packets().is_ok());
            let session = match Box::new(session).try_retire() {
                Err(session) => session,
                Ok(_) => panic!("missing TX retirement receipt accepted"),
            };
            assert_eq!(session.phase(), Phase::Retiring);
            assert_eq!(session.identity(), c.old);
        } else {
            assert_eq!(session.phase(), Phase::ChangingRadio);
            assert_eq!(session.identity(), c.old);
            assert!(Arc::ptr_eq(
                bank.hsdpa_route(c.new).unwrap().unwrap(),
                session.procedure().construction(),
            ));
            assert!(matches!(session.retiring_packets(), Err(Error::Phase)));
        }
    }
}

#[test]
fn partial_ps_release_snapshot_never_resurrects_readded_dch_bearer() {
    use crate::dedicated_bearers::BearerId;
    use umts_rrc::dedicated::ScheduledActivation;

    let c = connected();
    let (session, _receiver, _budget) = Session::fixture_two_bearer_session();
    let fixture = session.procedure().construction().configuration();
    // Keep the connected UE's exact SRB modes and PDU geometry. The generic
    // two-bearer Session fixture intentionally has no signaling connection.
    let original = Arc::new(
        umts_rrc::hsdpa::InitialConfiguration::new_mac_ehs(
            super::dual_lifecycle::with_second_packet_bearer(c.procedure.previous()),
            fixture.parameters().clone(),
            fixture.mappings().to_vec(),
        )
        .unwrap()
        .with_secondary_cell(fixture.secondary_cell().unwrap().clone())
        .unwrap(),
    );
    let activation = Arc::new(
        InitialActivation::schedule(
            original.clone(),
            ScheduledActivation::new(
                at((RELEASE_ACTIVATION_FRAME - 4) * CHIPS_PER_FRAME),
                (RELEASE_ACTIVATION_FRAME - 4) as u8,
                NonZeroU8::new(4).unwrap(),
            )
            .unwrap(),
        )
        .unwrap(),
    );
    let mut previous = bank(&c, c.old, original.dedicated().clone());
    previous
        .reserve_hsdpa_route(c.old, activation.clone())
        .unwrap();
    previous.fixture_publish_hsdpa_route();
    let before = previous
        .retained_hsdpa_configuration(c.old)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&before, &original));
    assert_eq!(before.mappings().len(), 2);

    let mut reduced = bank(&c, c.new, c.procedure.previous().clone());
    reduced.fixture_inherit_hsdpa_route(&previous);
    assert!(Arc::ptr_eq(
        reduced.hsdpa_route(c.new).unwrap().unwrap(),
        &activation
    ));
    assert!(
        reduced
            .has_active_hsdpa_bearer(c.new, BearerId::new(20).unwrap())
            .unwrap()
    );
    assert!(
        !reduced
            .has_active_hsdpa_bearer(c.new, BearerId::new(21).unwrap())
            .unwrap()
    );
    assert!(
        reduced
            .has_active_hsdpa_bearer(c.old, BearerId::new(20).unwrap())
            .is_err()
    );
    let snapshot = reduced
        .retained_hsdpa_configuration(c.new)
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot
            .mappings()
            .iter()
            .map(|mapping| mapping.bearer)
            .collect::<Vec<_>>(),
        [20]
    );
    assert_eq!(
        snapshot
            .parameters()
            .queues
            .queues()
            .iter()
            .map(|queue| queue.id())
            .collect::<Vec<_>>(),
        [0]
    );
    assert!(snapshot.uses_mac_ehs());
    assert_eq!(
        snapshot.parameters().harq.category(),
        original.parameters().harq.category()
    );
    assert_eq!(
        snapshot.secondary_cell().unwrap().primary_scrambling_code(),
        original.secondary_cell().unwrap().primary_scrambling_code()
    );
    assert!(reduced.committed_hsdpa(c.new, &activation).is_ok());

    let readded_identity = ReceiveIdentity {
        configuration: NonZeroU64::new(c.new.configuration.get() + 1).unwrap(),
        ..c.new
    };
    let mut readded = bank(&c, readded_identity, original.dedicated().clone());
    readded.fixture_inherit_hsdpa_route(&reduced);
    assert!(
        !readded
            .has_active_hsdpa_bearer(readded_identity, BearerId::new(21).unwrap())
            .unwrap()
    );
    let snapshot = readded
        .retained_hsdpa_configuration(readded_identity)
        .unwrap()
        .unwrap();
    assert!(snapshot.dedicated().bearer(21).is_some());
    assert_eq!(snapshot.mappings().len(), 1);
    assert_eq!(snapshot.mappings()[0].bearer, 20);
    assert_eq!(snapshot.parameters().queues.queues().len(), 1);
    let committed = readded
        .committed_hsdpa(readded_identity, &activation)
        .unwrap();
    assert!(committed.mapping(20).is_some());
    assert!(committed.mapping(21).is_none());
    assert!(Arc::ptr_eq(committed.activation(), &activation));

    let later_identity = ReceiveIdentity {
        configuration: NonZeroU64::new(readded_identity.configuration.get() + 1).unwrap(),
        ..readded_identity
    };
    let mut later = bank(&c, later_identity, original.dedicated().clone());
    later.fixture_inherit_hsdpa_route(&readded);
    let snapshot = later
        .retained_hsdpa_configuration(later_identity)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.mappings().len(), 1);
    assert_eq!(snapshot.parameters().queues.queues().len(), 1);
    assert!(
        !later
            .has_active_hsdpa_bearer(later_identity, BearerId::new(21).unwrap())
            .unwrap()
    );
    assert!(Arc::ptr_eq(
        later.hsdpa_route(later_identity).unwrap().unwrap(),
        &activation
    ));

    let final_identity = ReceiveIdentity {
        configuration: NonZeroU64::new(later_identity.configuration.get() + 1).unwrap(),
        ..later_identity
    };
    let mut released = bank(
        &c,
        final_identity,
        super::super::super::configuration(&c.radio.retained_signalling(c.key).unwrap()),
    );
    released.fixture_inherit_hsdpa_route(&later);
    assert!(released.hsdpa_route(final_identity).unwrap().is_none());
    assert!(
        released
            .retained_hsdpa_configuration(final_identity)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        previous
            .retained_hsdpa_configuration(c.old)
            .unwrap()
            .unwrap()
            .mappings()
            .len(),
        2
    );
}

#[test]
fn partial_shared_flow_release_is_rejected_before_rrc_publication() {
    use crate::test_support::rel99;
    use umts_rrc::{
        dedicated::{ExistingDchRelease, ScheduledActivation},
        hsdpa::InitialConfiguration,
    };

    let (session, _receiver, _budget) = Session::fixture_two_bearer_session();
    let original = session.procedure().construction().configuration();
    for mode in 0..3 {
        let mut parameters = original.parameters().clone();
        let mut sizes = [None; 8];
        sizes[0] = NonZeroUsize::new(if mode == 0 { 324 } else { 320 });
        parameters.queues =
            QueueSet::new(vec![QueueConfiguration::new(0, 0, sizes, 100, 32).unwrap()]).unwrap();
        let mut mappings = original.mappings().to_vec();
        mappings[1].flow = mappings[0].flow;
        let shared = if mode == 0 {
            InitialConfiguration::new(original.dedicated().clone(), parameters, mappings)
        } else {
            InitialConfiguration::new_mac_ehs(original.dedicated().clone(), parameters, mappings)
        }
        .unwrap();
        let shared = if mode == 2 {
            shared
                .with_secondary_cell(original.secondary_cell().unwrap().clone())
                .unwrap()
        } else {
            shared
        };
        let shared = Arc::new(shared);
        let release = ExistingDchRelease::new(
            original.dedicated().clone(),
            rel99::configuration(false, true, 10),
            ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
        )
        .unwrap()
        .with_retained_hsdpa(Some(shared.clone()));
        assert!(Arc::ptr_eq(release.retained_hsdpa().unwrap(), &shared));
        assert!(matches!(
            release.message(TransactionId::new(0).unwrap()),
            Err(DedicatedError::Mapping)
        ));
        assert_eq!(shared.mappings().len(), 2);
    }
    assert_eq!(original.mappings().len(), 2);
    assert_eq!(original.parameters().queues.queues().len(), 2);
}
