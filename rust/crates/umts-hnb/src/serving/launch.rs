// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;
use umts_types::time::ChipInstant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LaunchError {
    Epoch,
    Time,
}

pub(super) fn threshold(
    cursor: ChipInstant,
    now: ChipInstant,
    preparation_guard_chips: NonZeroU64,
) -> Result<ChipInstant, LaunchError> {
    if cursor.epoch != now.epoch {
        return Err(LaunchError::Epoch);
    }
    let base = cursor.index.max(now.index);
    let index = base
        .checked_add(preparation_guard_chips.get())
        .ok_or(LaunchError::Time)?;
    Ok(ChipInstant {
        epoch: cursor.epoch,
        index,
    })
}

pub(super) fn packet_threshold(
    cursor: ChipInstant,
    now: ChipInstant,
    preparation_guard_chips: NonZeroU64,
) -> Result<ChipInstant, LaunchError> {
    if cursor.epoch != now.epoch {
        return Err(LaunchError::Epoch);
    }
    let prepared = now
        .index
        .checked_add(preparation_guard_chips.get())
        .ok_or(LaunchError::Time)?;
    Ok(ChipInstant {
        epoch: now.epoch,
        index: cursor.index.max(prepared),
    })
}

#[cfg(test)]
mod tests {
    use super::super::initial_timing::{self, InitialTiming, InitialTimingError};
    use super::*;

    #[test]
    fn packet_horizon_respects_both_bounds_without_adding_producer_lead_twice() {
        let guard = NonZeroU64::new(100).unwrap();
        assert_eq!(packet_threshold(at(150), at(100), guard), Ok(at(200)));
        assert_eq!(packet_threshold(at(250), at(100), guard), Ok(at(250)));
        assert_eq!(packet_threshold(at(50), at(100), guard), Ok(at(200)));
        assert_eq!(
            packet_threshold(at(0), at(u64::MAX), guard),
            Err(LaunchError::Time)
        );
    }
    use crate::rel99_plan::CompiledRel99;
    use crate::test_support::{at, count, rel99};
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use umts_dsp::{
        coding::DecodeBudget, dedicated_transport::TransportPlanLimits,
        pulse_shaping::SamplesPerChip,
    };
    use umts_types::time::CHIPS_PER_FRAME;

    #[derive(Clone, Copy, Debug)]
    struct TestLaunchPlan {
        threshold: ChipInstant,
        timing: InitialTiming,
        required_end: ChipInstant,
        horizon: ChipInstant,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TestLaunchError {
        Epoch,
        Time,
        Timing(InitialTimingError),
        Window,
    }

    #[derive(Clone, Copy, Debug)]
    struct TestLaunchSettings {
        cursor: ChipInstant,
        now: ChipInstant,
        preparation_guard_chips: NonZeroU64,
        rx_delay_samples: f64,
        rate: SamplesPerChip,
        prefill_frames: NonZeroUsize,
        maximum_lead_frames: NonZeroUsize,
    }

    fn select(
        plan: &CompiledRel99,
        settings: TestLaunchSettings,
    ) -> Result<TestLaunchPlan, TestLaunchError> {
        if settings.prefill_frames > settings.maximum_lead_frames {
            return Err(TestLaunchError::Window);
        }
        let threshold = threshold(
            settings.cursor,
            settings.now,
            settings.preparation_guard_chips,
        )
        .map_err(|error| match error {
            LaunchError::Epoch => TestLaunchError::Epoch,
            LaunchError::Time => TestLaunchError::Time,
        })?;
        let timing = initial_timing::initial_timing(
            plan,
            settings.now,
            threshold,
            settings.rx_delay_samples,
            settings.rate,
        )
        .map_err(TestLaunchError::Timing)?;
        let base = settings.cursor.index.max(settings.now.index);
        let horizon = base
            .checked_add(
                u64::try_from(settings.maximum_lead_frames.get())
                    .ok()
                    .and_then(|frames| frames.checked_mul(CHIPS_PER_FRAME))
                    .ok_or(TestLaunchError::Time)?,
            )
            .ok_or(TestLaunchError::Time)?;
        let required_end = timing
            .mapping
            .downlink
            .time
            .index
            .checked_add(
                u64::try_from(settings.prefill_frames.get())
                    .ok()
                    .and_then(|frames| frames.checked_mul(CHIPS_PER_FRAME))
                    .ok_or(TestLaunchError::Time)?,
            )
            .ok_or(TestLaunchError::Time)?;
        if required_end > horizon {
            return Err(TestLaunchError::Window);
        }
        Ok(TestLaunchPlan {
            threshold,
            timing,
            required_end: ChipInstant {
                epoch: settings.cursor.epoch,
                index: required_end,
            },
            horizon: ChipInstant {
                epoch: settings.cursor.epoch,
                index: horizon,
            },
        })
    }

    fn plan(default_offset: u16, frame_offset: u8) -> Arc<CompiledRel99> {
        let configuration = rel99::configuration(false, false, 10)
            .with_physical({
                let mut physical = rel99::physical();
                physical.default_offset_512_chips = default_offset;
                physical.frame_offset_256_chips = frame_offset;
                physical
            })
            .unwrap();
        Arc::new(
            CompiledRel99::new(
                Arc::new(configuration),
                count(200_000),
                TransportPlanLimits {
                    maximum_coded_bits: count(200_000),
                    maximum_mapping_entries: count(300_000),
                    decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
                },
            )
            .unwrap(),
        )
    }

    #[test]
    fn threshold_uses_the_later_radio_cursor_and_host_time() {
        let cursor = at(12345);
        let now = at(23456);
        let value = threshold(cursor, now, NonZeroU64::new(5120).unwrap()).unwrap();
        assert_eq!(value.index, 23456 + 5120);
        assert_ne!(value.index, cursor.index);
    }

    #[test]
    fn selected_normal_dpch_has_window_without_exact_cursor_match() {
        for (default_offset, frame_offset) in [(0, 0), (2, 4), (75, 0), (77, 4)] {
            let plan = plan(default_offset, frame_offset);
            let cursor = at(12345);
            let now = at(23456);
            let selected = select(
                &plan,
                TestLaunchSettings {
                    cursor,
                    now,
                    preparation_guard_chips: NonZeroU64::new(5120).unwrap(),
                    rx_delay_samples: 0.0,
                    rate: SamplesPerChip::Two,
                    prefill_frames: count(8),
                    maximum_lead_frames: count(16),
                },
            )
            .unwrap();
            assert!(selected.timing.mapping.downlink.time.index >= selected.threshold.index);
            assert!(selected.required_end.index <= selected.horizon.index);
            assert_ne!(selected.timing.mapping.downlink.time.index, cursor.index);
        }
    }

    #[test]
    fn equal_prefill_and_lead_rejects_before_dispatch() {
        let result = select(
            &plan(0, 0),
            TestLaunchSettings {
                cursor: at(12345),
                now: at(23456),
                preparation_guard_chips: NonZeroU64::new(5120).unwrap(),
                rx_delay_samples: 0.0,
                rate: SamplesPerChip::Two,
                prefill_frames: count(8),
                maximum_lead_frames: count(8),
            },
        );
        assert!(matches!(result, Err(TestLaunchError::Window)));
    }

    #[test]
    fn epoch_and_overflow_are_rejected_without_forged_time() {
        let foreign = ChipInstant {
            epoch: umts_types::RadioEpoch::new(NonZeroU64::new(2).unwrap()),
            index: 0,
        };
        assert_eq!(
            threshold(at(0), foreign, NonZeroU64::new(1).unwrap()),
            Err(LaunchError::Epoch)
        );
        assert_eq!(
            threshold(
                ChipInstant {
                    epoch: at(0).epoch,
                    index: u64::MAX,
                },
                at(u64::MAX),
                NonZeroU64::new(1).unwrap(),
            ),
            Err(LaunchError::Time)
        );
    }
}
