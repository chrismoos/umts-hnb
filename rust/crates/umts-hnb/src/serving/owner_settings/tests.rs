// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveOwner;
use crate::rel99_planner::tests::admission;
use crate::test_support::at;
use umts_config::Config;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT};

fn config() -> Config {
    serde_yaml::from_str(include_str!(
        "../../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .expect("canonical serving profile")
}

fn build_real_owner_cs() {
    let config = config();
    let owners = OwnerPlan::new(&config, None).expect("canonical owner policy");
    let (source, mut radio, mut resources, prepared) = admission::prepared_for(true, false);
    let (_runtime, ports) = admission::transmit(&source);
    let settings = owners
        .settings(
            &prepared,
            at(0),
            at(CHIPS_PER_FRAME * 4),
            CHIPS_PER_SLOT * 2,
        )
        .expect("canonical owner settings");
    assert_eq!(
        settings.bearers.maximum_transport_bits,
        owners.maximum_frame_bits
    );
    assert!(
        settings.bearers.maximum_transport_bits.get() > settings.receive.maximum_output_bits.get()
    );
    let built = prepared
        .build_owners(&mut radio, &ports.control, settings)
        .expect("production owner construction");
    assert_eq!(built.identity(), prepared.allocation.receive_identity());
    drop(built);
    resources
        .cancel(prepared.allocation)
        .expect("release prepared allocation");
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn production_owner_builds_cs_only_plan() {
    build_real_owner_cs();
}

#[test]
fn initial_setup_can_publish_after_first_frame_acceptance_with_short_radio_lead() {
    use crate::signalling::tests as signalling_fixture;
    use umts_dsp::waveform::WaveformWindow;
    use umts_rrc::dedicated::{RadioBearerSetupProcedure, SetupPhase};
    use umts_rrc::nas::SignallingBearer;
    use umts_types::time::SampleInstant;

    for circuit in [false, true] {
        let owners = OwnerPlan::new(&config(), None).unwrap();
        let (source, mut radio, mut resources, prepared) =
            admission::prepared_for(circuit, !circuit);
        let key = prepared.allocation.receive_identity().ue;
        let mut nas_peer = signalling_fixture::peer(&source);
        signalling_fixture::send(
            &mut radio,
            key,
            SignallingBearer::NasAm,
            &mut nas_peer,
            &signalling_fixture::INITIAL,
        );
        assert_eq!(
            radio.process_one(),
            Some(Err(crate::signalling::SignallingError::RegistrationPending))
        );
        let (mut core, peer) = crate::core_link::tests::ready(&mut radio);
        assert!(
            core.drive(umts_iuh::session::ControlTime(3), &mut radio, |_, _| Ok::<
                _,
                (),
            >(
                ()
            ))
            .unwrap()
            .uplink_published
        );
        let umts_iuh::hnbap::Message::UeRegister { identity, .. } = peer.last_hnbap() else {
            panic!("missing UE registration");
        };
        peer.hnbap(umts_iuh::hnbap::Message::UeAccepted {
            identity,
            context: umts_iuh::ContextId::new(100).unwrap(),
            membership: None,
        });
        core.drive(umts_iuh::session::ControlTime(4), &mut radio, |_, _| {
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(radio.process_one(), Some(Ok(key)));
        core.drive(umts_iuh::session::ControlTime(5), &mut radio, |_, _| {
            Ok::<_, ()>(())
        })
        .unwrap();
        let settings = owners
            .settings(
                &prepared,
                at(0),
                at(CHIPS_PER_FRAME * 4),
                CHIPS_PER_SLOT * 2,
            )
            .unwrap();
        let identity = prepared.allocation.receive_identity();
        let start = settings.timing.downlink.time;
        let sample = |index| SampleInstant {
            epoch: start.epoch,
            index,
        };
        let rate = 2;
        let tail_samples = 16;
        let (reader, mut writer) = crate::tx_evidence::channel(sample(start.index * rate)).unwrap();
        let proof = reader
            .transport_receipt(start, rate as usize, tail_samples)
            .unwrap();
        let end = (start.index + CHIPS_PER_FRAME) * rate + (tail_samples as u64 * 2);
        let accepted_at = at(proof.air_end().index - CHIPS_PER_SECOND / 1000);
        assert!(accepted_at.index > start.index);
        assert!(accepted_at.index < settings.publication_deadline.index);
        assert!(!proof.submitted());
        assert!(writer.accepted(WaveformWindow {
            first_sample: sample(start.index * rate),
            next_sample: sample(end),
            chip_origin_sample: sample(start.index * rate),
        }));
        assert!(proof.submitted());
        let mut procedure = RadioBearerSetupProcedure::initial(
            identity.ue,
            radio.next_bearer_setup_transaction(identity.ue).unwrap(),
            prepared.setup,
            settings.now,
            settings.publication_deadline,
            settings.response_timeout,
        )
        .unwrap();
        assert!(!procedure.expire(accepted_at).unwrap());
        radio
            .publish_bearer_setup_on(&mut procedure, identity, accepted_at)
            .unwrap();
        assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
        resources.cancel(prepared.allocation).unwrap();
    }
}

#[test]
fn production_owner_builds_ps_only_plan() {
    let config = config();
    let owners = OwnerPlan::new(&config, None).expect("canonical owner policy");
    let (source, mut radio, mut resources, prepared) = admission::prepared_for(false, true);
    let (_runtime, ports) = admission::transmit(&source);
    let settings = owners
        .settings(
            &prepared,
            at(0),
            at(CHIPS_PER_FRAME * 4),
            CHIPS_PER_SLOT * 2,
        )
        .expect("canonical owner settings");
    let built = prepared
        .build_owners(&mut radio, &ports.control, settings)
        .expect("production owner construction");
    assert_eq!(built.identity(), prepared.allocation.receive_identity());
    drop(built);
    resources
        .cancel(prepared.allocation)
        .expect("release prepared allocation");
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn configured_packet_ceilings_fit_production_owners_for_negotiated_sdu_size() {
    for unacknowledged in [false, true] {
        let mut config = config();
        if unacknowledged {
            let packet = &mut config.rel99.physical.planner.services.packet[0];
            let buffers = packet.am.take().unwrap().buffers;
            packet.um = Some(umts_config::R99UmPolicy {
                uplink: buffers.clone(),
                downlink: buffers,
            });
        }
        let owners = OwnerPlan::new(&config, None).unwrap();
        let (source, mut radio, mut resources, prepared) =
            admission::prepared_packet_catalog(&config.rel99.physical.planner.services);
        let (_runtime, ports) = admission::transmit(&source);
        let settings = owners
            .settings(
                &prepared,
                at(0),
                at(CHIPS_PER_FRAME * 4),
                CHIPS_PER_SLOT * 2,
            )
            .unwrap();
        assert_eq!(settings.bearers.maximum_sdu_bytes.get(), 8192);
        let built = prepared
            .build_owners(&mut radio, &ports.control, settings)
            .expect("configured packet profile must fit actual serving owner");
        assert_eq!(built.identity(), prepared.allocation.receive_identity());
        drop(built);
        resources.cancel(prepared.allocation).unwrap();
        assert_eq!(resources.outstanding(), 0);
    }
}

#[test]
fn production_owner_builds_sequential_combined_plan() {
    let config = config();
    let owners = OwnerPlan::new(&config, None).expect("canonical owner policy");
    let (_source, _radio, mut resources, prepared) = admission::prepared_for(true, false);
    let identity = prepared.allocation.receive_identity();
    let (_initial, addition) = admission::sequential_cs_then_ps_plans();
    let settings = owners
        .settings_for_plan(
            addition.compiled(),
            at(0),
            at(CHIPS_PER_FRAME * 4),
            CHIPS_PER_SLOT * 2,
        )
        .expect("canonical combined owner settings");
    let requirements = addition
        .compiled()
        .requirements()
        .expect("combined plan requirements");
    assert!(settings.receive.maximum_coded_bits.get() >= requirements.uplink_coded_bits);
    assert!(settings.transmit.maximum_frame_bits.get() >= requirements.downlink_frame_payload_bits);
    let built = crate::rel99_planner::build_radio_owners(identity, addition.compiled(), &settings)
        .expect("production combined owner construction");
    assert!(Arc::ptr_eq(
        built.receiver.session().configured(),
        addition.compiled().uplink_configured()
    ));
    assert!(Arc::ptr_eq(built.transmit.plan(), addition.compiled()));
    drop(built);
    resources
        .cancel(prepared.allocation)
        .expect("release initial fixture allocation");
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn zero_cpich_amplitude_is_rejected_before_owner_construction() {
    let mut config = config();
    config.power_control.downlink.cpich_amplitude = 0.0;
    assert!(OwnerPlan::new(&config, None).is_err());
}

#[test]
fn tpc_offset_changes_only_the_tpc_field_and_is_compiled_before_admission() {
    let mut config = config();
    let base = OwnerPlan::new(&config, None).unwrap().amplitudes;
    assert_eq!(base.data, base.tpc);
    config.power_control.downlink.dedicated.downlink_offset_db = -6.0;
    let reduced = OwnerPlan::new(&config, None).unwrap().amplitudes;
    config.power_control.downlink.dedicated.tpc_offset_db = 6.0;
    let adjusted = OwnerPlan::new(&config, None).unwrap().amplitudes;
    assert_eq!(adjusted.data, reduced.data);
    assert_eq!(adjusted.tfci, reduced.tfci);
    assert_eq!(adjusted.pilot, reduced.pilot);
    assert!((adjusted.tpc - base.tpc).abs() < 1e-7);
    assert!((20.0 * (adjusted.tpc / adjusted.data).log10() - 6.0).abs() < 1e-5);
}

#[test]
fn invalid_tpc_offset_or_derived_amplitude_is_rejected_before_radio_open() {
    for offset in [f32::NAN, -1.0, 13.0] {
        let mut config = config();
        config.power_control.downlink.dedicated.tpc_offset_db = offset;
        assert!(OwnerPlan::new(&config, None).is_err());
    }
    let mut config = config();
    config.power_control.downlink.cpich_amplitude = 0.25;
    config.power_control.downlink.dedicated.downlink_offset_db = 6.0;
    config.power_control.downlink.dedicated.tpc_offset_db = 12.0;
    assert!(OwnerPlan::new(&config, None).is_err());
}

#[test]
fn zero_mapping_count_is_rejected_without_division() {
    assert!(per_mapping(1, 0, "test").is_err());
}

#[test]
fn nonfinite_coherence_is_rejected_by_schema_validation() {
    let mut config = config();
    config.receiver.dedicated.minimum_coherent_fraction = f64::NAN;
    assert!(OwnerPlan::new(&config, None).is_err());
}

#[test]
fn invalid_tfci_and_carrier_settings_are_rejected() {
    let mut tfci = config();
    tfci.receiver.dedicated.tfci_minimum_score = f64::NAN;
    assert!(OwnerPlan::new(&tfci, None).is_err());

    let mut carrier = config();
    carrier.receiver.dedicated.carrier_initial_hz = 10_000.0;
    assert!(OwnerPlan::new(&carrier, None).is_err());
}
