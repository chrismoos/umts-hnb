// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_bearers::MappingActivation, dedicated_rx::FrameAnchor, rel99_plan::CompiledRel99,
};
use umts_dsp::{fractional_sampling::SamplePosition, pulse_shaping::SamplesPerChip};
use umts_types::time::{
    CHIPS_PER_FRAME, CONNECTION_FRAME_MODULUS, ChipInstant, SYSTEM_FRAME_MODULUS, SampleInstant,
};

const T0_CHIPS: u64 = 1_024;
const FRAME_OFFSET_QUANTUM_CHIPS: u64 = 256;
const DEFAULT_OFFSET_QUANTUM_CHIPS: u64 = 512;
const MAX_FRAME_SEARCH: usize = CONNECTION_FRAME_MODULUS as usize;

pub(super) fn associated_common_frame(
    uplink: ChipInstant,
    dpch: umts_dsp::dedicated::DpchTiming,
) -> Result<ChipInstant, InitialTimingError> {
    let index = uplink
        .index
        .checked_sub(T0_CHIPS)
        .and_then(|index| index.checked_sub(dpch.chips()))
        .ok_or(InitialTimingError::Time)?;
    if !index.is_multiple_of(CHIPS_PER_FRAME) {
        return Err(InitialTimingError::Time);
    }
    Ok(ChipInstant { index, ..uplink })
}

#[derive(Clone, Copy, Debug)]
pub struct InitialTiming {
    pub mapping: MappingActivation,
    pub first_receive: SamplePosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialTimingError {
    Epoch,
    ThresholdBeforeNow,
    Configuration,
    Time,
    RxDelay,
    Sample(umts_dsp::DspError),
}

/// TS 25.331 8.5.15.1 and 8.6.6.14 distinguish DOFF's logical frame from its physical offset.
/// The receive position includes TS 25.211 7.6.3 T0 and the radio receive delay, not software filter delay.
pub fn initial_timing(
    plan: &CompiledRel99,
    now: ChipInstant,
    threshold: ChipInstant,
    rx_delay_samples: f64,
    rate: SamplesPerChip,
) -> Result<InitialTiming, InitialTimingError> {
    if now.epoch != threshold.epoch {
        return Err(InitialTimingError::Epoch);
    }
    if threshold.index < now.index {
        return Err(InitialTimingError::ThresholdBeforeNow);
    }
    if !rx_delay_samples.is_finite() {
        return Err(InitialTimingError::RxDelay);
    }

    let physical = plan.configuration().physical();
    physical
        .validate()
        .map_err(|_| InitialTimingError::Configuration)?;
    let uplink_fmax = fmax(plan.uplink_configured())?;
    let downlink_fmax = fmax(plan.downlink_configured())?;
    let common_fmax = lcm(uplink_fmax, downlink_fmax).ok_or(InitialTimingError::Time)?;

    let offset = u64::from(physical.frame_offset_256_chips)
        .checked_mul(FRAME_OFFSET_QUANTUM_CHIPS)
        .ok_or(InitialTimingError::Time)?;
    let threshold_frame = threshold.index / CHIPS_PER_FRAME;
    let threshold_remainder = threshold.index % CHIPS_PER_FRAME;
    let mut cell_frame = threshold_frame
        .checked_add(u64::from(threshold_remainder > offset))
        .ok_or(InitialTimingError::Time)?;
    let doff_frames = u64::from(physical.default_offset_512_chips)
        .checked_mul(DEFAULT_OFFSET_QUANTUM_CHIPS)
        .ok_or(InitialTimingError::Time)?
        / CHIPS_PER_FRAME;
    cell_frame = cell_frame.max(doff_frames);

    let mut selected = None;
    for _ in 0..MAX_FRAME_SEARCH {
        let logical_frame = cell_frame
            .checked_sub(doff_frames)
            .ok_or(InitialTimingError::Time)?;
        let frame_index = cell_frame
            .checked_mul(CHIPS_PER_FRAME)
            .and_then(|index| index.checked_add(offset))
            .ok_or(InitialTimingError::Time)?;
        if logical_frame.is_multiple_of(common_fmax) && frame_index >= threshold.index {
            selected = Some((cell_frame, logical_frame, frame_index));
            break;
        }
        cell_frame = cell_frame.checked_add(1).ok_or(InitialTimingError::Time)?;
    }
    let (cell_frame, logical_frame, downlink_index) = selected.ok_or(InitialTimingError::Time)?;

    let system_frame = (cell_frame % SYSTEM_FRAME_MODULUS) as u16;
    let cfn = physical
        .cfn_for_sfn(system_frame)
        .map_err(|_| InitialTimingError::Configuration)?;
    if logical_frame % CONNECTION_FRAME_MODULUS != u64::from(cfn)
        || !logical_frame.is_multiple_of(uplink_fmax)
        || !logical_frame.is_multiple_of(downlink_fmax)
    {
        return Err(InitialTimingError::Configuration);
    }

    let downlink = FrameAnchor {
        time: ChipInstant {
            epoch: now.epoch,
            index: downlink_index,
        },
        cfn,
    };
    let uplink_index = downlink_index
        .checked_add(T0_CHIPS)
        .ok_or(InitialTimingError::Time)?;
    let uplink = FrameAnchor {
        time: ChipInstant {
            epoch: now.epoch,
            index: uplink_index,
        },
        cfn,
    };
    let sample_index = uplink_index
        .checked_mul(rate.get() as u64)
        .ok_or(InitialTimingError::Time)?;
    let first_receive = sample_position(
        SampleInstant {
            epoch: now.epoch,
            index: sample_index,
        },
        rx_delay_samples,
    )?;

    Ok(InitialTiming {
        mapping: MappingActivation {
            uplink,
            uplink_frame: logical_frame,
            downlink,
            downlink_frame: logical_frame,
        },
        first_receive,
    })
}

fn fmax(tfcs: &umts_dsp::dedicated_transport::ConfiguredTfcs) -> Result<u64, InitialTimingError> {
    let mut result = None;
    for id in tfcs.transport_ids() {
        let frames = tfcs
            .definition(id)
            .ok_or(InitialTimingError::Configuration)?
            .interval
            .frames() as u64;
        result = Some(result.map_or(frames, |old: u64| old.max(frames)));
    }
    result.ok_or(InitialTimingError::Configuration)
}

fn lcm(a: u64, b: u64) -> Option<u64> {
    let gcd = gcd(a, b);
    a.checked_div(gcd)?.checked_mul(b)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn sample_position(
    base: SampleInstant,
    residual: f64,
) -> Result<SamplePosition, InitialTimingError> {
    let integer = residual.floor();
    if !integer.is_finite() || integer < i128::MIN as f64 || integer > i128::MAX as f64 {
        return Err(InitialTimingError::RxDelay);
    }
    let integer = integer as i128;
    let fraction = residual - integer as f64;
    if !fraction.is_finite() || !(0.0..1.0).contains(&fraction) {
        return Err(InitialTimingError::RxDelay);
    }
    let index = (base.index as i128)
        .checked_add(integer)
        .filter(|index| *index >= 0)
        .and_then(|index| u64::try_from(index).ok())
        .ok_or(InitialTimingError::Time)?;
    SamplePosition::new(SampleInstant { index, ..base }, fraction)
        .map_err(InitialTimingError::Sample)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{at, count};
    use std::sync::Arc;
    use umts_dsp::{
        coding::DecodeBudget, dedicated_transport::TransportPlanLimits,
        pulse_shaping::SamplesPerChip,
    };
    use umts_types::{RadioEpoch, time::SampleInstant};

    #[test]
    fn feedback_common_frame_tracks_the_ue_anchor_not_cell_startup() {
        for frame in [1_u64, 257, 12_345] {
            for offset in [0, 1, 149] {
                let dpch = umts_dsp::dedicated::DpchTiming::new(offset).unwrap();
                let common = at(frame * CHIPS_PER_FRAME);
                let uplink = at(common.index + dpch.chips() + T0_CHIPS);
                assert_eq!(associated_common_frame(uplink, dpch).unwrap(), common);
                assert!(associated_common_frame(at(uplink.index + 1), dpch).is_err());
            }
        }
    }

    fn plan(default_offset: u16, frame_offset: u8) -> Arc<CompiledRel99> {
        let configuration = crate::test_support::rel99::configuration(false, false, 10)
            .with_physical({
                let mut physical = crate::test_support::rel99::physical();
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
    fn selects_physical_offset_and_respects_fmax() {
        let plan = plan(2, 4);
        let result = initial_timing(
            &plan,
            at(0),
            at(CHIPS_PER_FRAME + 1),
            0.0,
            SamplesPerChip::Two,
        )
        .unwrap();
        assert_eq!(
            result.mapping.downlink.time.index,
            4 * CHIPS_PER_FRAME + 1_024
        );
        assert_eq!(result.mapping.downlink.cfn, 4);
        assert_eq!(result.mapping.downlink_frame, 4);
        assert_eq!(
            result.mapping.uplink.time.index,
            result.mapping.downlink.time.index + 1_024
        );
    }

    #[test]
    fn cfn_uses_sfn_modulus_at_wrap() {
        let plan = plan(75, 0);
        let frame = SYSTEM_FRAME_MODULUS * CHIPS_PER_FRAME;
        let result = initial_timing(&plan, at(frame), at(frame), 0.0, SamplesPerChip::Two).unwrap();
        assert_eq!(result.mapping.downlink.cfn, 0);
        assert_eq!(result.mapping.downlink_frame, SYSTEM_FRAME_MODULUS - 1 + 1);
    }

    #[test]
    fn signed_fractional_residual_is_preserved_without_filter_delay() {
        let plan = plan(0, 0);
        let result = initial_timing(&plan, at(0), at(0), -0.25, SamplesPerChip::Two).unwrap();
        assert_eq!(
            result.first_receive.sample(),
            SampleInstant {
                epoch: RadioEpoch::new(std::num::NonZeroU64::MIN),
                index: 2_047,
            }
        );
        assert!((result.first_receive.fraction() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn whole_frame_doff_does_not_shift_the_physical_frame_offset() {
        let plan = plan(77, 4);
        let result = initial_timing(
            &plan,
            at(CHIPS_PER_FRAME),
            at(2 * CHIPS_PER_FRAME + 1_024),
            0.0,
            SamplesPerChip::Two,
        )
        .unwrap();
        assert_eq!(
            result.mapping.downlink.time.index,
            5 * CHIPS_PER_FRAME + 1_024
        );
        assert_eq!(result.mapping.downlink.cfn, 4);
        assert_eq!(result.mapping.downlink_frame, 4);
        assert_eq!(result.mapping.uplink.cfn, result.mapping.downlink.cfn);
        assert_eq!(result.mapping.uplink_frame, result.mapping.downlink_frame);
    }

    #[test]
    fn timing_retains_large_integer_sample_coordinates_and_rejects_invalid_residuals() {
        let plan = plan(0, 0);
        let threshold = at((1_u64 << 45) * 4);
        let result =
            initial_timing(&plan, threshold, threshold, 0.25, SamplesPerChip::Two).unwrap();
        assert_eq!(
            result.first_receive.sample().index,
            result.mapping.uplink.time.index * 2
        );
        assert_eq!(result.first_receive.fraction(), 0.25);
        for residual in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            -f64::MAX,
        ] {
            assert!(initial_timing(&plan, at(0), at(0), residual, SamplesPerChip::Two).is_err());
        }
        assert!(matches!(
            initial_timing(&plan, at(1), at(0), 0.0, SamplesPerChip::Two),
            Err(InitialTimingError::ThresholdBeforeNow)
        ));
    }

    #[test]
    fn rejects_epoch_mismatch_and_sample_overflow() {
        let plan = plan(0, 0);
        let foreign = ChipInstant {
            epoch: RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap()),
            index: 0,
        };
        assert!(matches!(
            initial_timing(&plan, at(0), foreign, 0.0, SamplesPerChip::Two),
            Err(InitialTimingError::Epoch)
        ));
        assert!(matches!(
            initial_timing(
                &plan,
                ChipInstant {
                    epoch: at(0).epoch,
                    index: u64::MAX - 1_000,
                },
                ChipInstant {
                    epoch: at(0).epoch,
                    index: u64::MAX - 1_000,
                },
                0.0,
                SamplesPerChip::Eight,
            ),
            Err(InitialTimingError::Time)
        ));
    }
}
