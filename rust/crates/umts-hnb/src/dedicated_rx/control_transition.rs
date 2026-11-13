// SPDX-License-Identifier: AGPL-3.0-or-later

use super::ReceiveError;
use std::sync::Arc;
use umts_dsp::{
    dedicated::{DedicatedTimingPlan, DedicatedTimingSearch, UplinkControlFormat, UplinkDpchCode},
    hsdpa::FeedbackChannel,
    pulse_shaping::SamplesPerChip,
};
use umts_types::time::ChipInstant;

const EDCH_ONLY_CONTROL_FORMAT: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlSearchGeometry {
    rate: SamplesPerChip,
    radius_samples: usize,
}

impl ControlSearchGeometry {
    /// Select one complete initial E-DCH TTI from known matched-sample metadata.
    /// This is geometry only. It supplies no pilot or payload acceptance evidence.
    pub(crate) fn initial_suffix_from_samples(
        self,
        frame: ChipInstant,
        frame_sample: umts_types::time::SampleInstant,
        first: umts_types::time::SampleInstant,
        guard: usize,
    ) -> Result<Option<(ChipInstant, u8)>, ReceiveError> {
        use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT};
        if frame.epoch != frame_sample.epoch || frame.epoch != first.epoch {
            return Err(ReceiveError::Configuration);
        }
        let earliest = first
            .index
            .checked_add(self.radius_samples as u64)
            .and_then(|n| n.checked_add(guard as u64))
            .and_then(|n| n.checked_add(1))
            .ok_or(ReceiveError::Time)?;
        let Some(missing) = earliest.checked_sub(frame_sample.index).filter(|n| *n != 0) else {
            return Ok(None);
        };
        let frame_samples = CHIPS_PER_FRAME * self.rate.get() as u64;
        let frames = missing / frame_samples;
        let tti_samples = 3 * CHIPS_PER_SLOT * self.rate.get() as u64;
        let tti = (missing % frame_samples).div_ceil(tti_samples);
        if !(1..5).contains(&tti) {
            return Ok(None);
        }
        frames
            .checked_add(1)
            .and_then(|n| n.checked_mul(frame_samples))
            .and_then(|n| frame_sample.index.checked_add(n))
            .ok_or(ReceiveError::Time)?;
        let index = frames
            .checked_mul(CHIPS_PER_FRAME)
            .and_then(|n| frame.index.checked_add(n))
            .filter(|n| n.checked_add(CHIPS_PER_FRAME).is_some())
            .ok_or(ReceiveError::Time)?;
        Ok(Some((ChipInstant { index, ..frame }, (tti * 3) as u8)))
    }

    pub fn prepare_edch_only(
        self,
        code: Arc<UplinkDpchCode>,
        origin: ChipInstant,
        hsdpa: bool,
    ) -> Result<PreparedControlSearch, ReceiveError> {
        let feedback = hsdpa
            .then(|| FeedbackChannel::new(code.clone(), 0, origin))
            .transpose()?;
        let mut prepared =
            self.prepare(code, UplinkControlFormat::new(EDCH_ONLY_CONTROL_FORMAT)?)?;
        prepared.feedback = feedback;
        Ok(prepared)
    }
    pub fn new(rate: SamplesPerChip, radius_samples: usize) -> Result<Self, ReceiveError> {
        if radius_samples == 0 {
            return Err(ReceiveError::Configuration);
        }
        radius_samples
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .ok_or(ReceiveError::Capacity)?;
        Ok(Self {
            rate,
            radius_samples,
        })
    }

    pub fn prepare(
        self,
        code: Arc<UplinkDpchCode>,
        format: UplinkControlFormat,
    ) -> Result<PreparedControlSearch, ReceiveError> {
        let delays = self
            .radius_samples
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .ok_or(ReceiveError::Capacity)?;
        let plan = Arc::new(DedicatedTimingPlan::new(
            &code,
            format,
            0,
            self.rate.get(),
            delays,
        )?);
        let input_length = plan.input_length();
        let search = DedicatedTimingSearch::new(plan);
        Ok(PreparedControlSearch {
            geometry: self,
            code,
            format,
            search,
            input_length,
            feedback: None,
            startup: None,
        })
    }
}

pub struct PreparedControlSearch {
    geometry: ControlSearchGeometry,
    code: Arc<UplinkDpchCode>,
    format: UplinkControlFormat,
    search: DedicatedTimingSearch,
    input_length: usize,
    pub(super) feedback: Option<FeedbackChannel>,
    pub(super) startup: Option<Box<StartupSuffix>>,
}

pub(super) struct StartupSuffix {
    pub frame: ChipInstant,
    pub slot: u8,
    pub plan: Arc<DedicatedTimingPlan>,
    pub automatic: bool,
    pub attempted: bool,
    alternatives: Option<[Arc<DedicatedTimingPlan>; 3]>,
}

impl StartupSuffix {
    pub(super) fn select_slot(&mut self, slot: u8) -> Result<(), ReceiveError> {
        if self.plan.slot() != slot {
            let plan = self
                .alternatives
                .as_mut()
                .and_then(|plans| plans.iter_mut().find(|plan| plan.slot() == slot))
                .ok_or(ReceiveError::Configuration)?;
            std::mem::swap(&mut self.plan, plan);
        }
        self.slot = slot;
        Ok(())
    }
}

impl PreparedControlSearch {
    pub(super) fn is_edch_only(&self) -> bool {
        self.format.number() == EDCH_ONLY_CONTROL_FORMAT
    }
    pub fn matches(&self, geometry: ControlSearchGeometry, code: &Arc<UplinkDpchCode>) -> bool {
        self.geometry == geometry && Arc::ptr_eq(&self.code, code)
    }

    pub fn format(&self) -> UplinkControlFormat {
        self.format
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.search.storage_bytes()
            + self.startup.as_ref().map_or(0, |suffix| {
                std::mem::size_of::<StartupSuffix>()
                    + suffix.plan.storage_bytes()
                    + suffix
                        .alternatives
                        .as_ref()
                        .map_or(0, |plans| plans.iter().map(|p| p.storage_bytes()).sum())
            })
    }

    pub(super) fn prepare_initial_suffixes(
        &mut self,
        origin: ChipInstant,
    ) -> Result<(), ReceiveError> {
        if !self.is_edch_only() || self.startup.is_some() {
            return Err(ReceiveError::Configuration);
        }
        let delays = self
            .geometry
            .radius_samples
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(ReceiveError::Capacity)?;
        let make = |slot| {
            DedicatedTimingPlan::new(
                &self.code,
                self.format,
                slot,
                self.geometry.rate.get(),
                delays,
            )
            .map(Arc::new)
            .map_err(ReceiveError::from)
        };
        let plan = make(3)?;
        let alternatives = Some([make(6)?, make(9)?, make(12)?]);
        self.startup = Some(Box::new(StartupSuffix {
            frame: origin,
            slot: 3,
            plan,
            automatic: true,
            attempted: false,
            alternatives,
        }));
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn prepare_startup_suffix(
        &mut self,
        frame: ChipInstant,
        slot: u8,
    ) -> Result<(), ReceiveError> {
        if !self.is_edch_only() || self.startup.is_some() || ![3, 6, 9, 12].contains(&slot) {
            return Err(ReceiveError::Configuration);
        }
        let delays = self
            .geometry
            .radius_samples
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(ReceiveError::Capacity)?;
        let plan = Arc::new(DedicatedTimingPlan::new(
            &self.code,
            self.format,
            slot,
            self.geometry.rate.get(),
            delays,
        )?);
        self.startup = Some(Box::new(StartupSuffix {
            frame,
            slot,
            plan,
            automatic: false,
            attempted: false,
            alternatives: None,
        }));
        Ok(())
    }

    pub(super) fn exchange_search(
        &mut self,
        search: &mut DedicatedTimingSearch,
        length: &mut usize,
    ) {
        std::mem::swap(&mut self.search, search);
        std::mem::swap(&mut self.input_length, length);
    }
}

pub(super) struct ControlTransition {
    pub activation: ChipInstant,
    pub prepared: Box<PreparedControlSearch>,
    pub phase: ControlTransitionPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ControlTransitionPhase {
    AwaitEdch,
    Edch,
    AwaitDch(ChipInstant),
    Dch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use umts_types::{
        RadioEpoch,
        time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, SampleInstant},
    };

    #[test]
    fn initial_suffix_metadata_selects_only_the_first_complete_nonzero_tti() {
        let epoch = RadioEpoch::new(NonZeroU64::MIN);
        let frame = ChipInstant { epoch, index: 1024 };
        let origin = SampleInstant {
            epoch,
            index: 1_000_000,
        };
        for rate in [
            SamplesPerChip::Two,
            SamplesPerChip::Four,
            SamplesPerChip::Eight,
        ] {
            for radius in [1, 8, 64] {
                let geometry = ControlSearchGeometry::new(rate, radius).unwrap();
                let tti_samples = 3 * CHIPS_PER_SLOT * rate.get() as u64;
                let guard = umts_dsp::fractional_sampling::FractionalSampler::GUARD_SAMPLES;
                for offset in 0..CHIPS_PER_FRAME * rate.get() as u64 * 2 {
                    let first = SampleInstant {
                        index: origin.index + offset,
                        ..origin
                    };
                    let actual = geometry
                        .initial_suffix_from_samples(frame, origin, first, guard)
                        .unwrap();
                    let earliest = offset + (radius + guard + 1) as u64;
                    let absolute_tti = earliest.div_ceil(tti_samples);
                    if absolute_tti.is_multiple_of(5) {
                        assert!(actual.is_none());
                    } else {
                        let (hint, slot) = actual.unwrap();
                        assert_eq!(hint.index, frame.index + absolute_tti / 5 * CHIPS_PER_FRAME);
                        assert_eq!(u64::from(slot), absolute_tti % 5 * 3);
                        let position = (hint.index - frame.index
                            + u64::from(slot) * CHIPS_PER_SLOT)
                            * rate.get() as u64;
                        assert!(position >= earliest);
                        assert!(position - tti_samples < earliest);
                    }
                }
                let early = SampleInstant {
                    index: origin.index - (radius + guard + 1) as u64,
                    ..origin
                };
                assert_eq!(
                    geometry
                        .initial_suffix_from_samples(frame, origin, early, guard)
                        .unwrap(),
                    None
                );
            }
        }
    }

    #[test]
    fn initial_suffix_metadata_rejects_epoch_and_time_overflow() {
        let epoch = RadioEpoch::new(NonZeroU64::MIN);
        let foreign = RadioEpoch::new(NonZeroU64::new(2).unwrap());
        let frame = ChipInstant { epoch, index: 1024 };
        let origin = SampleInstant { epoch, index: 1000 };
        let geometry = ControlSearchGeometry::new(SamplesPerChip::Two, 8).unwrap();
        for (frame, origin, first, guard) in [
            (
                ChipInstant {
                    epoch: foreign,
                    ..frame
                },
                origin,
                origin,
                16,
            ),
            (
                frame,
                SampleInstant {
                    epoch: foreign,
                    ..origin
                },
                origin,
                16,
            ),
            (
                frame,
                origin,
                SampleInstant {
                    epoch: foreign,
                    ..origin
                },
                16,
            ),
            (
                frame,
                origin,
                SampleInstant {
                    index: u64::MAX,
                    ..origin
                },
                16,
            ),
            (frame, origin, origin, usize::MAX),
            (
                ChipInstant {
                    index: u64::MAX - 1,
                    ..frame
                },
                origin,
                origin,
                16,
            ),
            (
                frame,
                SampleInstant {
                    index: u64::MAX - 100,
                    ..origin
                },
                SampleInstant {
                    index: u64::MAX - 100,
                    ..origin
                },
                16,
            ),
        ] {
            assert!(
                geometry
                    .initial_suffix_from_samples(frame, origin, first, guard)
                    .is_err()
            );
        }
    }
}
