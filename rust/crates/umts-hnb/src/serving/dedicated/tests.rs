// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn config() -> Config {
    serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap()
}

fn required_shared(config: &Config) -> usize {
    let samples =
        CHIPS_PER_FRAME as usize * 2 * usize::from(config.receiver.dedicated.history_frames);
    samples * 2 * std::mem::size_of::<num_complex::Complex32>()
        + config.runtime.maximum_ues as usize
            * 2
            * (std::mem::size_of::<crate::dedicated_rx::ReceiveIdentity>()
                + std::mem::size_of::<(
                    crate::dedicated_rx::ReceiveIdentity,
                    Result<crate::dedicated_rx::SampleProgress, crate::dedicated_rx::ReceiveError>,
                )>())
        + umts_dsp::fractional_sampling::FractionalSampler::new().storage_bytes()
        + umts_runtime::mailbox::retained_storage_bound::<DownlinkControl>(
            NonZeroUsize::new(usize::from(config.receiver.dedicated.command_capacity)).unwrap(),
        )
        .unwrap()
}

#[test]
fn default_plan_preserves_configured_capacity_and_consistent_fixed_budgets() {
    let mut config = config();
    config.receiver.dedicated = Default::default();
    let plan = DedicatedPlan::new(&config).unwrap();
    let capacity = config.runtime.maximum_ues as usize;
    assert_eq!(plan.capacity.get(), capacity);
    assert_eq!(plan.channels.get(), capacity * 2 * 8);
    assert_eq!(plan.receive.shard.maximum_ues.get(), capacity);
    assert_eq!(plan.receive.shard.resources.channels, capacity * 2 * 32);
    assert_eq!(
        plan.receive.history_samples.get(),
        CHIPS_PER_FRAME as usize * 2 * 2
    );
    assert_eq!(plan.receive.maximum_shared_bytes, 4 * 1024 * 1024);
    assert!(required_shared(&config) <= plan.receive.maximum_shared_bytes);
    assert_eq!(
        plan.receive.shard.resources.physical_sample_bytes,
        512 * 1024 * 1024
    );
    assert_eq!(plan.receive.shard.resources.coded_bits, 8 * 1024 * 1024);
    assert_eq!(plan.receive.shard.resources.output_bytes, 8 * 1024 * 1024);
    assert_eq!(
        plan.receive.shard.resources.demultiplexed_bits,
        8 * 1024 * 1024
    );
    assert_eq!(plan.receive.command_capacity.get(), 64);
    assert_eq!(plan.transmit.command_capacity.get(), 64);
    assert_eq!(plan.transmit.feedback_capacity.get(), 64);
    assert_eq!(plan.receive.output_slots_per_ue.get(), 8);
    assert_eq!(plan.receive.maximum_output_bits.get(), 262_144);
    assert_eq!(
        plan.transmit.maximum_frame_payload_bits,
        plan.receive.maximum_output_bits
    );
    assert_eq!(plan.retained_bits.get(), 16 * 1024 * 1024);
    assert_eq!(
        plan.transmit.maximum_inflight_retained_bits,
        plan.retained_bits
    );
    assert_eq!(plan.control.maximum_bearer_bytes, 64 * 1024 * 1024);
    for work in [
        plan.receive.shard.maximum_sessions_per_step,
        plan.receive.commands_per_step,
        plan.transmit.commands_per_step,
        plan.transmit.feedback_per_step,
        plan.control.receipt_visits,
        plan.control.service_visits,
        plan.admission.visits,
        plan.admission.command_results,
    ] {
        assert_eq!(work.get(), 16);
    }
    assert_eq!(plan.admission.prefill_frames.get(), 8);
    assert_eq!(plan.admission.maximum_lead_frames.get(), 16);
    assert!(plan.admission.maximum_lead_frames > plan.admission.prefill_frames);
    assert_eq!(plan.admission.health_interval_chips.get(), CHIPS_PER_FRAME);
}

#[test]
fn explicit_overrides_reach_every_distinct_owner_without_coupling_work_to_queue_capacity() {
    let mut config = config();
    config.runtime.maximum_ues = 7;
    let limits = &mut config.receiver.dedicated;
    limits.maximum_shared_bytes = 8 * 1024 * 1024;
    limits.maximum_sample_bytes = 3 * 1024 * 1024;
    limits.maximum_coded_bits = 5 * 1024 * 1024;
    limits.maximum_output_bytes = 256 * 1024;
    limits.maximum_demultiplexed_bits = 7 * 1024 * 1024;
    limits.maximum_bearer_bytes = 9 * 1024 * 1024;
    limits.maximum_transmit_bits = 65_536;
    limits.maximum_frame_bits = 8192;
    limits.history_frames = 4;
    limits.work_per_cycle = 3;
    limits.command_capacity = 5;
    limits.output_slots = 3;
    let plan = DedicatedPlan::new(&config).unwrap();
    assert_eq!(plan.capacity.get(), 7);
    assert_eq!(plan.channels.get(), 112);
    assert_eq!(
        plan.receive.shard.resources,
        ReceiveResources {
            physical_sample_bytes: 3 * 1024 * 1024,
            coded_bits: 5 * 1024 * 1024,
            output_bytes: 256 * 1024,
            demultiplexed_bits: 7 * 1024 * 1024,
            channels: 448,
        }
    );
    assert_eq!(
        plan.receive.history_samples.get(),
        CHIPS_PER_FRAME as usize * 2 * 4
    );
    assert_eq!(plan.receive.maximum_shared_bytes, 8 * 1024 * 1024);
    assert_eq!(plan.receive.maximum_output_bytes, 256 * 1024);
    assert_eq!(plan.receive.output_slots_per_ue.get(), 3);
    assert_eq!(plan.receive.maximum_output_bits.get(), 8192);
    assert_eq!(plan.transmit.maximum_frame_payload_bits.get(), 8192);
    assert_eq!(plan.retained_bits.get(), 65_536);
    assert_eq!(plan.transmit.maximum_inflight_retained_bits.get(), 65_536);
    assert_eq!(plan.control.maximum_bearer_bytes, 9 * 1024 * 1024);
    for queue in [
        plan.receive.command_capacity,
        plan.transmit.command_capacity,
        plan.transmit.feedback_capacity,
    ] {
        assert_eq!(queue.get(), 5);
    }
    for work in [
        plan.receive.shard.maximum_sessions_per_step,
        plan.receive.commands_per_step,
        plan.transmit.commands_per_step,
        plan.transmit.feedback_per_step,
        plan.control.receipt_visits,
        plan.control.service_visits,
        plan.admission.visits,
        plan.admission.command_results,
    ] {
        assert_eq!(work.get(), 3);
    }
}

#[test]
fn shared_budget_includes_history_sampler_feedback_and_deferred_service_results() {
    let mut config = config();
    config.runtime.maximum_ues = 64;
    let bytes = u32::try_from(required_shared(&config)).unwrap();
    config.receiver.dedicated.maximum_shared_bytes = bytes;
    assert!(DedicatedPlan::new(&config).is_ok());
    config.receiver.dedicated.maximum_shared_bytes = bytes - 1;
    assert!(DedicatedPlan::new(&config).is_err());
}

#[test]
fn output_and_transmit_budgets_reject_even_one_unreservable_mapping_or_frame() {
    {
        let mut config = config();
        let limits = &mut config.receiver.dedicated;
        let slots = NonZeroUsize::new(usize::from(limits.output_slots)).unwrap();
        let payload_bits = limits.maximum_frame_bits as usize;
        let internal =
            crate::dedicated_rx::DedicatedReceiveSession::output_storage_bytes(slots, payload_bits)
                .unwrap();
        let external = slots.get() * payload_bits;
        limits.maximum_output_bytes =
            u32::try_from(internal + external + payload_bits + std::mem::size_of::<usize>())
                .unwrap();
        assert!(DedicatedPlan::new(&config).is_ok());
        config.receiver.dedicated.maximum_output_bytes -= 1;
        assert!(DedicatedPlan::new(&config).is_err());
        config.receiver.dedicated.maximum_output_bytes = u32::try_from(external).unwrap();
        assert!(DedicatedPlan::new(&config).is_err());
    }
    let mut config = config();
    config.receiver.dedicated.maximum_transmit_bits = config.receiver.dedicated.maximum_frame_bits;
    assert!(DedicatedPlan::new(&config).is_ok());
    config.receiver.dedicated.maximum_transmit_bits -= 1;
    assert!(DedicatedPlan::new(&config).is_err());
}

#[test]
fn maximum_supported_ue_capacity_and_smallest_work_queues_remain_bounded() {
    let mut config = config();
    config.runtime.maximum_ues = 64;
    config.receiver.dedicated.work_per_cycle = 1;
    config.receiver.dedicated.command_capacity = 1;
    config.receiver.dedicated.history_frames = 8;
    config.receiver.dedicated.maximum_shared_bytes = 16 * 1024 * 1024;
    let plan = DedicatedPlan::new(&config).unwrap();
    assert_eq!(plan.capacity.get(), 64);
    assert_eq!(plan.channels.get(), 1024);
    assert_eq!(plan.receive.shard.resources.channels, 4096);
    assert_eq!(plan.receive.history_samples.get(), 614_400);
    assert_eq!(plan.receive.commands_per_step.get(), 1);
    assert_eq!(plan.transmit.feedback_capacity.get(), 1);
    assert_eq!(plan.transmit.feedback_per_step.get(), 1);
    assert_eq!(plan.admission.visits.get(), 1);
    assert!(required_shared(&config) <= plan.receive.maximum_shared_bytes);
}

#[test]
fn invalid_schema_and_zero_owner_capacity_fail_before_any_attachment() {
    let changes: &[fn(&mut umts_config::ReceiverDedicatedSettings)] = &[
        |limits| limits.maximum_shared_bytes = 0,
        |limits| limits.maximum_sample_bytes = 0,
        |limits| limits.maximum_coded_bits = 0,
        |limits| limits.maximum_output_bytes = 0,
        |limits| limits.maximum_demultiplexed_bits = 0,
        |limits| limits.maximum_bearer_bytes = 0,
        |limits| limits.maximum_transmit_bits = 0,
        |limits| limits.maximum_frame_bits = 0,
        |limits| limits.maximum_frame_bits = 1_048_577,
        |limits| limits.history_frames = 0,
        |limits| limits.history_frames = 9,
        |limits| limits.work_per_cycle = 0,
        |limits| limits.work_per_cycle = 257,
        |limits| limits.command_capacity = 0,
        |limits| limits.command_capacity = 257,
        |limits| limits.output_slots = 0,
        |limits| limits.output_slots = 257,
    ];
    for (index, change) in changes.iter().enumerate() {
        let mut config = config();
        change(&mut config.receiver.dedicated);
        assert!(DedicatedPlan::new(&config).is_err(), "invalid case {index}");
    }
    let mut config = config();
    config.runtime.maximum_ues = 0;
    assert!(DedicatedPlan::new(&config).is_err());
}
