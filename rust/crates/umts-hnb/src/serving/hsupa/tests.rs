// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count as capacity, source};
use umts_config::RustHsupaResources;

pub(in crate::serving) fn config() -> Config {
    let mut config: Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap();
    config.runtime.maximum_ues = 2;
    config.hspa.uplink.resources = Some(RustHsupaResources {
        receive: RustHsupaReceive {
            likelihood_limit: 64.0,
            minimum_control_correlation: 0.5,
            minimum_control_margin: 0.05,
            iterations: 4,
            maximum_format_bits: 100_000,
            maximum_catalog_bytes: 64 * 1024 * 1024,
            retained_sample_bytes: 4 * 1024 * 1024,
            soft_buffer_bytes: 4 * 1024 * 1024,
            scratch_bytes: 4 * 1024 * 1024,
            maximum_receive_bytes: 128 * 1024 * 1024,
            report_capacity: 4,
            report_bytes: 1024 * 1024,
            packet_capacity: 8,
            packet_bytes: 1024 * 1024,
            reorder_hold_ttis: 32,
            feedback_capacity: 4,
            feedback_amplitude: 0.1,
        },
        maximum_cell_receive_bytes: 1024 * 1024 * 1024,
        pending_transmissions: 16,
        maximum_tx_bytes: 4 * 1024 * 1024,
        command_capacity: 16,
        service_visits: 2,
        grant_amplitude: 0.1,
        primary_grant_refresh_ms: None,
    });
    config
}

#[test]
fn typed_receive_settings_preserve_yaml_and_reject_unfunded_limits() {
    let config = config();
    validate(&config).unwrap();
    let encoded = serde_yaml::to_string(&config).unwrap();
    let decoded: Config = serde_yaml::from_str(&encoded).unwrap();
    let settings = decoded.hspa.uplink.resources.as_ref().unwrap();
    let receive = receive_settings(&settings.receive).unwrap();
    let references = reference_powers(&decoded).unwrap();
    let actual: Vec<_> = references
        .as_slice()
        .iter()
        .map(|reference| (reference.format.value(), reference.power_index))
        .collect();
    let expected: Vec<_> = decoded
        .hspa
        .uplink
        .edpdch_references
        .iter()
        .map(|reference| (reference.etfci, reference.power_offset))
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(receive.maximum_catalog_bytes.get(), 64 * 1024 * 1024);
    assert_eq!(receive.iterations.get(), 4);
    assert_eq!(receive.feedback_amplitude, 0.1);
    for variant in 0..5 {
        let mut invalid = config.clone();
        let settings = invalid.hspa.uplink.resources.as_mut().unwrap();
        match variant {
            0 => settings.maximum_cell_receive_bytes = 1,
            1 => settings.maximum_tx_bytes = 1,
            2 => settings.receive.likelihood_limit = 0.0,
            3 => settings.receive.maximum_receive_bytes = 1,
            4 => settings.receive.minimum_control_margin = f64::NAN,
            _ => unreachable!(),
        }
        assert!(validate(&invalid).is_err());
    }
}

#[test]
fn shared_control_codes_are_reserved_once_and_transferred_to_tx() {
    let mut config = config();
    let source = source(true);
    let mut resources = crate::radio_resources::RadioResources::new(
        at(0).epoch,
        source.information(),
        capacity(2),
        0..=1,
    )
    .unwrap();
    assert!(
        SharedChannels::reserve(&config, &mut resources, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(resources.outstanding(), 0);
    config.hspa.uplink.enabled = true;
    let mut shared = SharedChannels::reserve(&config, &mut resources, None)
        .unwrap()
        .unwrap();
    assert!(SharedChannels::reserve(&config, &mut resources, None).is_err());
    let bank = shared.prepare_transmit(&source).unwrap();
    assert!(shared.prepare_transmit(&source).is_err());
    resources
        .release_hsupa(
            bank.release()
                .unwrap_or_else(|_| panic!("empty local bank")),
        )
        .unwrap();
    assert_eq!(resources.outstanding(), 0);
    config
        .hspa
        .uplink
        .resources
        .as_mut()
        .unwrap()
        .maximum_tx_bytes = 1;
    assert!(SharedChannels::reserve(&config, &mut resources, None).is_err());
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn shared_decoder_storage_is_funded_by_the_cell_receive_budget() {
    let mut config = config();
    config.hspa.uplink.enabled = true;
    let limits = config.hspa.uplink.resources.as_ref().unwrap();
    let receive = &limits.receive;
    let per_ue = receive.maximum_receive_bytes
        + receive.maximum_catalog_bytes
        + receive.report_bytes
        + receive.packet_bytes;
    let owners = u64::from(per_ue) * u64::from(config.runtime.maximum_ues);
    config
        .hspa
        .uplink
        .resources
        .as_mut()
        .unwrap()
        .maximum_cell_receive_bytes = owners;
    assert!(validate(&config).is_err());
    let worker =
        crate::hsupa_rx::decode::Worker::required_storage_bytes(config.runtime.maximum_ues)
            .unwrap();
    config
        .hspa
        .uplink
        .resources
        .as_mut()
        .unwrap()
        .maximum_cell_receive_bytes = owners + u64::try_from(worker).unwrap();
    validate(&config).unwrap();
}
