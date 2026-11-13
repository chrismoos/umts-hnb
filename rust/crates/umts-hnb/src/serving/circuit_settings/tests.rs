// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rrc::transport::TransportInterval;
use umts_types::CnDomain;

fn config() -> Config {
    serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap()
}

fn metadata(request: &SetupOrModify) -> RabPlan {
    let qos = request.first.parameters.as_ref().unwrap();
    RabPlan {
        domain: CnDomain::Cs,
        rab: request.first.id.get(),
        bearers: vec![10, 11, 12],
        maximum: qos.effective_maximum().unwrap(),
        guaranteed: qos.effective_guaranteed().unwrap(),
        packet_payload_budget_bits: None,
        packet_capacity: None,
        maximum_sdu_bits: qos.maximum_sdu_bits.get(),
        circuit_interval: Some(TransportInterval::Ms20),
        circuit_versions: Some(3),
        combinations: (0..qos.combination_count())
            .map(|i| qos.combination(i).unwrap())
            .collect(),
    }
}

#[test]
fn canonical_addresses_roles_and_per_attempt_sources_are_independent() {
    let mut config = config();
    config.runtime.media.circuit.initialization = RustCircuitInitialization::Receive;
    let mut plan = CircuitPlan::new(&config).unwrap();
    let request = crate::rel99_planner::tests::cs_request();
    let rab = metadata(&request);
    let first = plan.settings(&rab, &request).unwrap();
    let next = plan.settings(&rab, &request).unwrap();
    assert_eq!(first.local.port(), 0);
    assert_eq!(
        first.local.ip(),
        config.core.gtpu.bind_ip.parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        first.advertised,
        Some(config.gtpu_advertise_ip().parse().unwrap())
    );
    assert_eq!(first.ingress.ssrc, None);
    assert_ne!(first.source.ssrc, next.source.ssrc);
    assert_eq!(next.source.sequence, first.source.sequence.wrapping_add(1));
    assert!(
        matches!(first.initialization, CircuitInitialization::Receive(versions) if versions.mask() == 3)
    );
    assert_eq!(
        plan.playout().lead_chips.get(),
        40 * CHIPS_PER_SECOND / MILLISECONDS_PER_SECOND
    );
    assert_eq!(
        plan.playout().horizon_ticks.get(),
        60_000 * RTP_TICKS_PER_MILLISECOND
    );
}

#[test]
fn invalid_protocol_and_address_policy_reject_before_binding_any_endpoint() {
    for invalid in ["0.0.0.0", "224.0.0.1", "255.255.255.255", "::1"] {
        let mut config = config();
        config.runtime.media.circuit.advertise_ip = Some(invalid.into());
        assert!(CircuitPlan::new(&config).is_err());
    }
    let mut config = config();
    config.runtime.media.circuit.versions = 4;
    assert!(CircuitPlan::new(&config).is_err());
    config.runtime.media.circuit.versions = 3;
    config.runtime.media.circuit.timeout_ms = 0;
    assert!(CircuitPlan::new(&config).is_err());
}

#[test]
fn mismatched_rab_does_not_consume_source_and_exhaustion_is_checked() {
    let mut plan = CircuitPlan::new(&config()).unwrap();
    let request = crate::rel99_planner::tests::cs_request();
    let mut rab = metadata(&request);
    let seed = plan.next_source;
    rab.rab += 1;
    assert!(plan.settings(&rab, &request).is_err());
    assert_eq!(plan.next_source, seed);
    rab.rab -= 1;
    plan.next_source = NonZeroU32::new(u32::MAX);
    assert_eq!(plan.settings(&rab, &request).unwrap().source.ssrc, u32::MAX);
    assert!(matches!(
        plan.settings(&rab, &request),
        Err(InitialCircuitError::Capacity)
    ));
}
