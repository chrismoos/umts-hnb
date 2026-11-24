// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    rel99_planner::tests::{limits, policy},
    test_support::{at, count, rel99 as fixture},
};
use std::num::NonZeroU8;
use umts_rrc::establishment::TransactionId;
use umts_rrc_codec::decode_downlink_dedicated;
use umts_rrc_schema::class_definitions::DLDCCHMessageType;

fn ids(values: &[u8]) -> Vec<BearerId> {
    values
        .iter()
        .map(|id| BearerId::new(*id).unwrap())
        .collect()
}
fn compile(configuration: Arc<Rel99Configuration>) -> Arc<CompiledRel99> {
    let physical = [fixture::physical()];
    let policy = policy(&physical);
    Arc::new(
        CompiledRel99::new(
            configuration,
            policy.maximum_total_coded_bits,
            policy.transport,
        )
        .unwrap(),
    )
}

#[test]
fn voice_first_release_restores_original_packet_capacity_without_replacing_bearers() {
    use crate::rel99_planner::tests::{cs_request, ps_request, reference_transport_limits};
    let config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99-384.yaml"
    )))
    .unwrap();
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let limits = reference_transport_limits();
    let cs = cs_request();
    let ps = ps_request(384_000);
    let cs_selection = configured
        .services()
        .select(&[(CnDomain::Cs, &cs)])
        .unwrap();
    let ps_selection = configured
        .services()
        .select(&[(CnDomain::Ps, &ps)])
        .unwrap();
    let mut retained = fixture::retained();
    for rb in &mut retained {
        if rb.id != 1 {
            rb.uplink_pdu_bits = 232;
        }
    }
    let voice = plan_initial(
        &retained,
        cs_selection.rabs(),
        &limits,
        &configured.policy(),
    )
    .unwrap();
    let combined = plan_existing(
        voice.compiled().clone(),
        voice.rabs(),
        ps_selection.rabs(),
        &limits,
        &configured.policy(),
    )
    .unwrap();
    let metadata: Vec<_> = combined
        .retained_rabs()
        .iter()
        .chain(combined.rabs())
        .cloned()
        .collect();
    let packet = &combined.rabs()[0];
    assert_eq!(
        packet.packet_capacity.as_ref().unwrap().maximum_blocks,
        [12, 12]
    );
    let packet_rb = combined
        .compiled()
        .configuration()
        .bearer(packet.bearers[0])
        .unwrap();
    let ul_id = packet_rb.mapping.uplink[0].transport;
    let peak = |plan: &CompiledRel99| {
        plan.configuration()
            .uplink()
            .transport(ul_id)
            .unwrap()
            .formats()
            .map(|format| format.blocks)
            .max()
            .unwrap()
    };
    assert_eq!(peak(combined.compiled()), 11);
    let released = ids(&voice.rabs()[0].bearers);
    let restored = plan_release_restoring(
        combined.compiled().clone(),
        &metadata,
        &released,
        &limits,
        &configured.policy(),
    )
    .unwrap();
    assert_eq!(peak(restored.compiled()), 12);
    assert_eq!(
        restored.compiled().configuration().bearer(packet_rb.id),
        Some(packet_rb)
    );
    assert_eq!(
        restored.compiled().configuration().physical(),
        combined.compiled().configuration().physical()
    );
    let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(8).unwrap()).unwrap();
    ExistingDchRelease::new(
        combined.compiled().configuration().clone(),
        restored.compiled().configuration().clone(),
        activation,
    )
    .unwrap()
    .message(TransactionId::new(0).unwrap())
    .unwrap();
    let mut invalid = metadata.clone();
    invalid
        .iter_mut()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .packet_capacity
        .as_mut()
        .unwrap()
        .maximum_blocks[0] = u16::MAX;
    assert!(
        plan_release_restoring(
            combined.compiled().clone(),
            &invalid,
            &released,
            &limits,
            &configured.policy(),
        )
        .is_err()
    );
}
#[test]
fn voice_release_keeps_the_channel_even_under_a_borrowed_code() {
    use crate::rel99_planner::tests::{cs_request, reference_transport_limits};
    let config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/hspa-two-ue.yaml"
    )))
    .unwrap();
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let limits = reference_transport_limits();
    let cs = cs_request();
    let selection = configured
        .services()
        .select(&[(CnDomain::Cs, &cs)])
        .unwrap();
    let combined = plan_initial(
        &fixture::retained(),
        selection.rabs(),
        &limits,
        &configured.policy(),
    )
    .unwrap();
    let sf = |plan: &CompiledRel99| plan.configuration().physical().downlink_sf;
    let smallest = configured
        .policy()
        .physical
        .iter()
        .map(|physical| physical.downlink_sf)
        .max()
        .unwrap();
    assert!(sf(combined.compiled()) < smallest);
    let release = || {
        plan_release_restoring(
            combined.compiled().clone(),
            combined.rabs(),
            &ids(&combined.rabs()[0].bearers),
            &limits,
            &configured.policy(),
        )
        .unwrap()
    };
    crate::radio_resources::set_lent_data_codes(0);
    assert_eq!(sf(release().compiled()), sf(combined.compiled()));
    crate::radio_resources::set_lent_data_codes(u32::MAX);
    let restored = release();
    crate::radio_resources::set_lent_data_codes(0);
    assert_eq!(sf(restored.compiled()), sf(combined.compiled()));
    assert_eq!(
        restored
            .compiled()
            .configuration()
            .physical()
            .downlink_codes,
        combined
            .compiled()
            .configuration()
            .physical()
            .downlink_codes
    );
    assert_eq!(
        restored
            .compiled()
            .configuration()
            .physical()
            .uplink_scrambling,
        combined
            .compiled()
            .configuration()
            .physical()
            .uplink_scrambling
    );
    let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(8).unwrap()).unwrap();
    ExistingDchRelease::new(
        combined.compiled().configuration().clone(),
        restored.compiled().configuration().clone(),
        activation,
    )
    .unwrap()
    .message(TransactionId::new(0).unwrap())
    .unwrap();
}

fn assert_projection(old: &DchTfcs, new: &DchTfcs) {
    let columns: Vec<_> = new
        .transports()
        .iter()
        .map(|retained| {
            let index = old
                .transports()
                .iter()
                .position(|transport| transport.id == retained.id)
                .unwrap();
            let original = &old.transports()[index];
            assert_eq!(retained.interval, original.interval);
            assert_eq!(retained.coding, original.coding);
            assert_eq!(retained.crc, original.crc);
            assert_eq!(
                retained.rate_matching_attribute,
                original.rate_matching_attribute
            );
            assert_eq!(
                retained.formats().collect::<Vec<_>>(),
                original.formats().collect::<Vec<_>>()
            );
            index
        })
        .collect();
    let mut expected = Vec::new();
    for row in old.combinations() {
        let formats: Vec<_> = columns.iter().map(|column| row.formats[*column]).collect();
        if !expected.contains(&formats) {
            expected.push(formats)
        }
    }
    assert_eq!(
        new.combinations()
            .iter()
            .map(|row| row.formats.clone())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn removes_cs_preserving_exact_packet_srb_transport_and_asymmetric_ids() {
    let old = compile(fixture::configuration(true, true, 10));
    let physical = [fixture::physical()];
    let policy = policy(&physical);
    let released = ids(&[10, 11, 12]);
    let planned = plan_release(old.clone(), &released, &limits(), &policy).unwrap();
    assert!(Arc::ptr_eq(planned.previous(), &old));
    assert_eq!(planned.released(), released);
    let target = planned.compiled().configuration();
    assert_eq!(target.physical(), old.configuration().physical());
    assert_eq!(
        target
            .uplink()
            .transports()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [4, 5]
    );
    assert_eq!(
        target
            .downlink()
            .transports()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [9, 10]
    );
    assert_eq!(target.bearers().len(), 4);
    for id in [1, 2, 3, 20] {
        assert_eq!(target.bearer(id), old.configuration().bearer(id))
    }
    assert_projection(old.configuration().uplink(), target.uplink());
    assert_projection(old.configuration().downlink(), target.downlink());
    assert_eq!(target.uplink().combinations().len(), 4);
}

#[test]
fn removes_packet_without_inventing_independent_amr_subflow_combinations() {
    let old = compile(fixture::configuration(true, true, 10));
    let physical = [fixture::physical()];
    let policy = policy(&physical);
    let planned = plan_release(old.clone(), &ids(&[20]), &limits(), &policy).unwrap();
    let target = planned.compiled().configuration();
    for id in [1, 2, 3, 10, 11, 12] {
        assert_eq!(target.bearer(id), old.configuration().bearer(id))
    }
    assert_eq!(
        target
            .uplink()
            .transports()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [1, 2, 3, 5]
    );
    assert_eq!(
        target
            .downlink()
            .transports()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [6, 7, 8, 10]
    );
    assert_projection(old.configuration().uplink(), target.uplink());
    assert_projection(old.configuration().downlink(), target.downlink());
    assert_eq!(target.uplink().combinations().len(), 12);
}

#[test]
fn last_user_release_preserves_cell_dch_srbs_and_encodes_actual_release() {
    let old = compile(fixture::configuration(true, true, 10));
    let physical = [fixture::physical()];
    let policy = policy(&physical);
    let planned = plan_release(old.clone(), &ids(&[10, 11, 12, 20]), &limits(), &policy).unwrap();
    let target = planned.compiled().configuration();
    assert_eq!(
        target.bearers().iter().map(|rb| rb.id).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        target
            .uplink()
            .transports()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [5]
    );
    assert_eq!(
        target
            .downlink()
            .transports()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [10]
    );
    assert_projection(old.configuration().uplink(), target.uplink());
    assert_projection(old.configuration().downlink(), target.downlink());
    assert_eq!(target.uplink().combinations().len(), 2);
    let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(8).unwrap()).unwrap();
    let release =
        ExistingDchRelease::new(old.configuration().clone(), target.clone(), activation).unwrap();
    assert_eq!(release.released(), [10, 11, 12, 20]);
    let encoded = release.message(TransactionId::new(1).unwrap()).unwrap();
    assert!(matches!(
        decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
            .unwrap()
            .value
            .message,
        DLDCCHMessageType::radioBearerRelease(_)
    ));
}

#[test]
fn collapsed_rows_use_new_explicit_policy_not_arbitrary_old_gain() {
    let (ul, dl, bearers, physical) = fixture::components(true, true, 10);
    let mut rows = ul.combinations().to_vec();
    for (index, row) in rows.iter_mut().enumerate() {
        row.gain = Some(GainFactors {
            control: 15,
            data: if index % 2 == 0 { 6 } else { 13 },
        });
    }
    let ul = DchTfcs::new(DchDirection::Uplink, ul.transports().to_vec(), rows).unwrap();
    let old = compile(Arc::new(
        Rel99Configuration::new(ul, dl, bearers, physical).unwrap(),
    ));
    let physical = [fixture::physical()];
    let mut policy = policy(&physical);
    policy.uplink_gains = GainFactors {
        control: 15,
        data: 11,
    };
    let planned = plan_release(old.clone(), &ids(&[10, 11, 12]), &limits(), &policy).unwrap();
    let target = planned.compiled().configuration();
    assert_projection(old.configuration().uplink(), target.uplink());
    assert!(
        target
            .uplink()
            .combinations()
            .iter()
            .all(|row| row.gain == Some(policy.uplink_gains))
    );
    assert!(
        target
            .downlink()
            .combinations()
            .iter()
            .all(|row| row.gain.is_none())
    );
    let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(8).unwrap()).unwrap();
    let release =
        ExistingDchRelease::new(old.configuration().clone(), target.clone(), activation).unwrap();
    release.message(TransactionId::new(2).unwrap()).unwrap();
}

#[test]
fn rejects_empty_duplicate_missing_and_srb_release_without_changing_original() {
    let old = compile(fixture::configuration(true, true, 10));
    let original = old.configuration().clone();
    let physical = [fixture::physical()];
    let policy = policy(&physical);
    assert!(matches!(
        plan_release(old.clone(), &[], &limits(), &policy),
        Err(Error::Capacity)
    ));
    for invalid in [&[20, 20][..], &[31][..], &[2][..]] {
        assert!(matches!(
            plan_release(old.clone(), &ids(invalid), &limits(), &policy),
            Err(Error::Identity)
        ));
    }
    assert!(Arc::ptr_eq(old.configuration(), &original));
    assert_eq!(old.configuration().bearers().len(), 7);
}

#[test]
fn projected_product_is_bounded_and_zero_power_active_policy_is_rejected() {
    let old = compile(fixture::configuration(true, true, 10));
    let physical = [fixture::physical()];
    let mut policy = policy(&physical);
    policy.maximum_combinations = count(1);
    assert!(matches!(
        plan_release(old.clone(), &ids(&[20]), &limits(), &policy),
        Err(Error::Capacity)
    ));
    policy.maximum_combinations = count(1024);
    policy.uplink_gains = GainFactors {
        control: 15,
        data: 0,
    };
    assert!(matches!(
        plan_release(old, &ids(&[20]), &limits(), &policy),
        Err(Error::Configuration(DedicatedError::Gains))
    ));
}

#[test]
fn cs_only_and_ps_only_sources_release_to_original_signalling() {
    let physical = [fixture::physical()];
    let policy = policy(&physical);
    for (voice, packet, released) in [(true, false, ids(&[10, 11, 12])), (false, true, ids(&[20]))]
    {
        let old = compile(fixture::configuration(voice, packet, 10));
        let planned = plan_release(old.clone(), &released, &limits(), &policy).unwrap();
        let target = planned.compiled().configuration();
        assert_eq!(target.bearers().len(), 3);
        assert_projection(old.configuration().uplink(), target.uplink());
        assert_projection(old.configuration().downlink(), target.downlink());
        for rb in target.bearers() {
            assert_eq!(Some(rb), old.configuration().bearer(rb.id))
        }
    }
}

#[test]
fn dc_packet_geometry_preserves_512_octets_through_csps_and_rel99_fallback() {
    use crate::rel99_planner::tests::{cs_request, ps_request, reference_transport_limits};
    for hsdpa in [false, true] {
        let mut config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/rel99-384.yaml"
        )))
        .unwrap();
        config.rel99.physical.planner.services.packet[0]
            .downlink
            .rlc_bits = 4096;
        config.rel99.physical.planner.services.packet[0]
            .downlink
            .maximum_blocks = 3;
        let configured = ConfiguredPolicy::new(&config).unwrap();
        // The reference UE capability: 6400 transport bits and 9600
        // physical DL bits per 10 ms.
        let mut ordinary_limits = reference_transport_limits();
        ordinary_limits.downlink_physical_bits = 9600;
        assert_eq!(ordinary_limits.downlink.transport_bits, 6400);
        let limits = if hsdpa {
            umts_rrc::capabilities::hsdpa::SimultaneousDch::Kbps64.apply(ordinary_limits)
        } else {
            ordinary_limits
        };
        let mut physical = configured.policy().physical.to_vec();
        if hsdpa {
            for candidate in &mut physical {
                candidate.downlink_sf = 32;
                candidate.downlink_pilot_bits = 8;
            }
        }
        let policy = Policy {
            hsdpa,
            physical: &physical,
            ..configured.policy()
        };
        let cs = cs_request();
        let ps = ps_request(384_000);
        let cs_selection = configured
            .services()
            .select(&[(CnDomain::Cs, &cs)])
            .unwrap();
        let ps_selection = configured
            .services()
            .select(&[(CnDomain::Ps, &ps)])
            .unwrap();
        let mut retained = fixture::retained();
        for rb in &mut retained {
            if rb.id != 1 {
                rb.uplink_pdu_bits = 232;
            }
        }
        let packet_only = plan_initial(&retained, ps_selection.rabs(), &limits, &policy).unwrap();
        let voice = plan_initial(&retained, cs_selection.rabs(), &limits, &policy).unwrap();
        let combined = plan_existing(
            voice.compiled().clone(),
            voice.rabs(),
            ps_selection.rabs(),
            &limits,
            &policy,
        )
        .unwrap();
        let metadata: Vec<_> = combined
            .retained_rabs()
            .iter()
            .chain(combined.rabs())
            .cloned()
            .collect();
        let released = ids(&voice.rabs()[0].bearers);
        let restored = plan_release_restoring(
            combined.compiled().clone(),
            &metadata,
            &released,
            &limits,
            &policy,
        )
        .unwrap();
        for plan in [
            packet_only.compiled(),
            combined.compiled(),
            restored.compiled(),
        ] {
            limits.validate_dch(plan.configuration()).unwrap();
            let configuration = plan.configuration();
            if !hsdpa {
                assert_eq!(limits.downlink_physical_bits, 9600);
                assert_eq!(configuration.physical().downlink_sf, 8);
                assert_eq!(configuration.physical().downlink_codes.len(), 1);
            }
            for bearer in configuration
                .bearers()
                .iter()
                .filter(|bearer| bearer.domain == Some(CnDomain::Ps))
            {
                let mut nonzero = false;
                for mapping in &bearer.mapping.downlink {
                    for format in configuration
                        .downlink()
                        .transport(mapping.transport)
                        .unwrap()
                        .formats()
                    {
                        assert_eq!(format.rlc_bits, 4096);
                        assert!(format.blocks <= 3);
                        nonzero |= format.blocks != 0;
                        if hsdpa {
                            assert_eq!(format.blocks, 0);
                        }
                    }
                }
                if !hsdpa {
                    assert!(nonzero, "Rel99 fallback must provide packet data");
                }
            }
        }
        let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(8).unwrap()).unwrap();
        ExistingDchRelease::new(
            combined.compiled().configuration().clone(),
            restored.compiled().configuration().clone(),
            activation,
        )
        .unwrap()
        .message(TransactionId::new(0).unwrap())
        .unwrap();
    }
}
