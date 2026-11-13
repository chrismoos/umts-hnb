// SPDX-License-Identifier: AGPL-3.0-or-later

use super::feedback::PilotPowerOwner;
use super::sample_runtime::SampleHistory;
use super::{
    ChipProgress, DedicatedChipReceiver, DedicatedReceiveSession, PilotObservation,
    PilotTpcController, PilotTpcSettings, PilotTpcStatus, ReceiveError, ReceiveIdentity,
    ReceiveOwner, ReceiveResources,
};
use num_complex::Complex32;
use std::{num::NonZeroUsize, sync::Arc};
use umts_dsp::dedicated_transport::UplinkTfcs;
use umts_dsp::{
    carrier::CarrierOffsetHz,
    dedicated::{
        CarrierTrackingSettings, DedicatedTimingPlan, DedicatedTimingSearch, UplinkControlFormat,
        UplinkDpchCode, UplinkDpchReceiver,
    },
    fractional_sampling::{FractionalSampler, SamplePosition},
    pulse_shaping::SamplesPerChip,
};
use umts_types::time::{
    CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant, SLOTS_PER_FRAME, SampleInstant,
};

// Union estimate across searched delays for independent complex Gaussian pilot noise.
const TRACKING_PILOT_NOISE_PROBABILITY: f64 = 1e-6;
const TIMING_TRACKING_FRACTION: f64 = 1.0 / 8.0;

mod quality;
mod radio_link;
pub use radio_link::{
    RadioLinkEvent, RadioLinkSyncSettings, RadioLinkSyncState, RadioLinkSyncStatus,
};

#[derive(Clone, Copy)]
struct ReacquisitionCandidate {
    frame: ChipInstant,
    sample: u64,
}

#[derive(Clone, Copy)]
struct PendingTimingPeak {
    frame: ChipInstant,
    continuity: u64,
    begin: SampleInstant,
    length: usize,
    carrier_bits: u64,
    peak: umts_dsp::dedicated::PilotTiming,
}

impl ReacquisitionCandidate {
    fn confirms(self, next: Self, rate: usize) -> bool {
        self.frame.epoch == next.frame.epoch
            && self.frame.index.checked_add(CHIPS_PER_FRAME) == Some(next.frame.index)
            && self
                .sample
                .checked_add(CHIPS_PER_FRAME * rate as u64)
                .is_some_and(|expected| expected.abs_diff(next.sample) <= rate as u64)
    }
}

#[cfg(test)]
mod reacquisition_tests {
    use super::*;
    use crate::test_support::at;

    #[test]
    fn confirmation_requires_adjacent_frames_and_consistent_chip_timing() {
        let first = ReacquisitionCandidate {
            frame: at(0),
            sample: 1000,
        };
        let second = ReacquisitionCandidate {
            frame: at(CHIPS_PER_FRAME),
            sample: 1000 + 2 * CHIPS_PER_FRAME,
        };
        assert!(first.confirms(second, 2));
        assert!(first.confirms(
            ReacquisitionCandidate {
                sample: second.sample + 2,
                ..second
            },
            2
        ));
        assert!(!first.confirms(
            ReacquisitionCandidate {
                sample: second.sample + 3,
                ..second
            },
            2
        ));
        assert!(!first.confirms(
            ReacquisitionCandidate {
                frame: at(2 * CHIPS_PER_FRAME),
                ..second
            },
            2
        ));
        assert!(!second.confirms(first, 2));
        assert!(
            !ReacquisitionCandidate {
                sample: u64::MAX,
                ..first
            }
            .confirms(second, 2)
        );
    }
}

#[derive(Clone, Copy)]
pub struct SampleReceiveSettings {
    pub rate: SamplesPerChip,
    pub first_receive: SamplePosition,
    pub search_radius_samples: usize,
    pub minimum_coherent_fraction: f64,
    pub radio_link: RadioLinkSyncSettings,
}

pub(crate) fn storage_requirements(
    tfcs: &UplinkTfcs,
    code: Arc<UplinkDpchCode>,
    control: UplinkControlFormat,
    carrier: CarrierTrackingSettings,
    rate: SamplesPerChip,
    search_radius_samples: usize,
    power_control: bool,
) -> Result<usize, ReceiveError> {
    let physical_bits = super::chips::maximum_physical_bits(tfcs).ok_or(ReceiveError::Capacity)?;
    let tti_bytes = if control.tfci_bits() != 0 {
        super::tti::maximum_storage_bytes(tfcs).ok_or(ReceiveError::Capacity)?
    } else {
        0
    };
    let receiver = UplinkDpchReceiver::with_tracking(code.clone(), control, carrier)?;
    let delays = search_radius_samples
        .checked_mul(2)
        .and_then(|v| v.checked_add(1))
        .ok_or(ReceiveError::Capacity)?;
    let plan = DedicatedTimingPlan::new(&code, control, 0, rate.get(), delays)?;
    let search = DedicatedTimingSearch::new(Arc::new(plan));
    let base = physical_bits
        .checked_mul(std::mem::size_of::<f64>())
        .and_then(|bytes| bytes.checked_add(receiver.sample_storage_bytes()))
        .and_then(|bytes| bytes.checked_add(tti_bytes))
        .and_then(|bytes| {
            bytes.checked_add(
                search.storage_bytes() + CHIPS_PER_SLOT as usize * std::mem::size_of::<Complex32>(),
            )
        })
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Option<PendingTimingPeak>>()))
        .ok_or(ReceiveError::Capacity)?;
    if power_control {
        base.checked_add(std::mem::size_of::<PilotPowerOwner>())
            .ok_or(ReceiveError::Capacity)
    } else {
        Ok(base)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleReceiveCounters {
    #[cfg(test)]
    pub startup_timing_search_calls: u64,
    #[cfg(test)]
    pub startup_completed_searches: u64,
    #[cfg(test)]
    pub startup_confirmation_slot_calls: u64,
    #[cfg(test)]
    pub timing_kernel_calls: u64,
    #[cfg(test)]
    pub timing_peak_reuses: u64,
    pub searches: u64,
    pub radio_link_failures: u64,
    pub radio_link_restores: u64,
    pub acquired_frames: u64,
    pub rejected_frames: u64,
    pub history_lost_frames: u64,
    pub sampled_slots: u64,
    pub pilot_confirmation_slots: u64,
    pub pilot_confirmed_frames: u64,
    pub predicted_timing_frames: u64,
    pub tracking_losses: u64,
    pub downlink_feedback_dropped: u64,
}
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct SampleTrackingStatus {
    pub predicted: SamplePosition,
    pub acquired: Option<SamplePosition>,
    pub carrier_seed: CarrierOffsetHz,
    pub contiguous: bool,
    pub confirmation: Option<(ChipInstant, u64)>,
    pub radio_link: RadioLinkSyncStatus,
}
#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct AcquisitionObservation {
    pub frame: ChipInstant,
    pub peak: umts_dsp::dedicated::PilotTiming,
    pub measured_integer_coherence: f64,
    pub measured_significant: bool,
    pub predicted: SamplePosition,
    pub filtered: SamplePosition,
    pub carrier_seed: CarrierOffsetHz,
    pub contiguous: bool,
    pub significant: bool,
    pub confirmed: bool,
    pub accepted: bool,
    pub uses_prediction: bool,
    pub stable_tracking: bool,
}
#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub enum SampleProgress {
    AwaitSamples,
    AwaitFeedback,
    Skipped,
    Slot(ChipProgress),
    Boundary(ChipInstant),
}
pub struct DedicatedSampleReceiver {
    acquisition_trace: Option<(u64, Vec<AcquisitionObservation>)>,
    #[cfg(test)]
    startup_minimum_restore: Option<f64>,
    #[cfg(test)]
    startup_slot_confirmation: bool,
    #[cfg(test)]
    slot_preflight_disabled: bool,
    #[cfg(test)]
    timing_peak_reuse_disabled: bool,
    chips: DedicatedChipReceiver,
    settings: SampleReceiveSettings,
    search: DedicatedTimingSearch,
    search_length: usize,
    pending_timing_peak: Option<PendingTimingPeak>,
    sampler_output: Vec<Complex32>,
    predicted: SamplePosition,
    acquired: Option<SamplePosition>,
    frame: ChipInstant,
    slot: usize,
    generation: Option<u64>,
    carrier_seed: CarrierOffsetHz,
    confirmed_carrier: CarrierOffsetHz,
    counters: SampleReceiveCounters,
    sample_bytes: usize,
    pub(super) power: Option<PilotPowerOwner>,
    pub(super) downlink_control: Option<crate::dedicated_tx::DownlinkControl>,
    pub(super) outer_loop_settings: Option<super::OuterLoopSettings>,
    hsupa_first: bool,
    contiguous_frame: bool,
    tracking_quality: quality::TrackingQuality,
    radio_link: radio_link::RadioLinkMonitor,
    reacquisition_candidate: Option<ReacquisitionCandidate>,
    tracking_candidate: Option<ReacquisitionCandidate>,
    control_transition: Option<super::control_transition::ControlTransition>,
    coexistence_restoration: Option<ChipInstant>,
}
impl DedicatedSampleReceiver {
    pub(super) fn downlink_feedback_dropped(&mut self) {
        self.counters.downlink_feedback_dropped =
            self.counters.downlink_feedback_dropped.saturating_add(1);
    }
    pub fn control_search_geometry(&self) -> super::ControlSearchGeometry {
        super::ControlSearchGeometry::new(self.settings.rate, self.settings.search_radius_samples)
            .expect("validated receive search geometry")
    }

    pub fn new(
        chips: DedicatedChipReceiver,
        settings: SampleReceiveSettings,
    ) -> Result<Self, ReceiveError> {
        let anchor = chips.session().anchor;
        if settings.first_receive.sample().epoch != anchor.time.epoch
            || settings.search_radius_samples == 0
            || !settings.minimum_coherent_fraction.is_finite()
            || !(0.0..=1.0).contains(&settings.minimum_coherent_fraction)
            || settings.minimum_coherent_fraction == 0.0
            || settings.first_receive.sample().index < settings.search_radius_samples as u64
        {
            return Err(ReceiveError::Configuration);
        }
        let delays = settings
            .search_radius_samples
            .checked_mul(2)
            .and_then(|v| v.checked_add(1))
            .ok_or(ReceiveError::Capacity)?;
        let plan = chips.timing_plan(settings.rate.get(), delays)?;
        let search_length = plan.input_length();
        let carrier_initial = chips.carrier_initial();
        let search = DedicatedTimingSearch::new(plan);
        let sample_bytes = chips
            .resources()
            .physical_sample_bytes
            .checked_add(search.storage_bytes())
            .and_then(|v| v.checked_add(CHIPS_PER_SLOT as usize * std::mem::size_of::<Complex32>()))
            .and_then(|v| v.checked_add(std::mem::size_of::<Option<PendingTimingPeak>>()))
            .ok_or(ReceiveError::Capacity)?;
        Ok(Self {
            acquisition_trace: None,
            #[cfg(test)]
            startup_minimum_restore: None,
            #[cfg(test)]
            startup_slot_confirmation: false,
            #[cfg(test)]
            slot_preflight_disabled: false,
            #[cfg(test)]
            timing_peak_reuse_disabled: false,
            frame: anchor.time,
            predicted: settings.first_receive,
            acquired: None,
            carrier_seed: carrier_initial,
            confirmed_carrier: carrier_initial,
            settings,
            search,
            search_length,
            pending_timing_peak: None,
            sampler_output: vec![Complex32::default(); CHIPS_PER_SLOT as usize],
            chips,
            slot: 0,
            generation: None,
            counters: SampleReceiveCounters::default(),
            sample_bytes,
            power: None,
            downlink_control: None,
            outer_loop_settings: None,
            hsupa_first: false,
            control_transition: None,
            coexistence_restoration: None,
            contiguous_frame: false,
            tracking_quality: quality::TrackingQuality::default(),
            radio_link: radio_link::RadioLinkMonitor::new(settings.radio_link),
            reacquisition_candidate: None,
            tracking_candidate: None,
        })
    }
    pub fn configure_power_control(
        &mut self,
        settings: PilotTpcSettings,
    ) -> Result<(), ReceiveError> {
        if self.power.is_some()
            || self.counters.sampled_slots != 0
            || settings.rate.get() != self.settings.rate.get()
            || settings.uplink_anchor != self.session().frame_anchor().time
        {
            return Err(ReceiveError::Configuration);
        }
        let controller = PilotTpcController::new(self.session().identity(), settings)
            .map_err(ReceiveError::PowerControl)?;
        self.sample_bytes = self
            .sample_bytes
            .checked_add(std::mem::size_of::<PilotPowerOwner>())
            .ok_or(ReceiveError::Capacity)?;
        self.power = Some(PilotPowerOwner::new(controller, settings.lead_chips.get()));
        Ok(())
    }
    pub fn power_control_status(&self) -> Option<PilotTpcStatus> {
        self.power.as_ref().map(PilotPowerOwner::status)
    }

    pub fn configure_outer_loop(
        &mut self,
        settings: super::OuterLoopSettings,
    ) -> Result<(), ReceiveError> {
        if self.power.is_none()
            || self.outer_loop_settings.is_some()
            || self.counters.sampled_slots != 0
        {
            return Err(ReceiveError::Configuration);
        }
        self.outer_loop_settings = Some(settings);
        Ok(())
    }

    pub fn configure_hsdpa_feedback(
        &mut self,
        capacity: std::num::NonZeroUsize,
        budget: std::num::NonZeroUsize,
    ) -> Result<crate::hsdpa_rx::FeedbackPort, ReceiveError> {
        let delivery_bytes = crate::hsdpa_rx::FeedbackReceiver::delivery_bytes(capacity)?;
        let bytes = self
            .sample_bytes
            .checked_add(std::mem::size_of::<crate::hsdpa_rx::FeedbackReceiver>())
            .and_then(|bytes| bytes.checked_add(delivery_bytes))
            .ok_or(ReceiveError::Capacity)?;
        let port = self.chips.configure_hsdpa_feedback(capacity, budget)?;
        self.sample_bytes = bytes;
        Ok(port)
    }

    pub fn hsdpa_feedback(&self) -> Option<&crate::hsdpa_rx::FeedbackReceiver> {
        self.chips.hsdpa_feedback()
    }

    pub(super) fn shared_uplink_code(&self) -> Arc<UplinkDpchCode> {
        self.chips.shared_uplink_code()
    }

    pub(super) fn attach_prepared_hsupa_at(
        &mut self,
        candidate: super::HsupaAttachment,
        activation: ChipInstant,
        maximum_bytes: NonZeroUsize,
    ) -> Result<(), (ReceiveError, super::HsupaAttachment)> {
        let admission = (|| {
            if self.control_transition.is_some()
                || self.chips.coexisting_selection.is_some()
                || candidate.selection.as_ref().is_some_and(|selection| {
                    candidate.control.is_some() || !selection.matches(&self.session().tfcs)
                })
                || (candidate.receiver.maximum_dpdch() == 0) != candidate.control.is_some()
                || candidate.control.as_ref().is_some_and(|control| {
                    !control.is_edch_only()
                        || !control
                            .matches(self.control_search_geometry(), &self.shared_uplink_code())
                        || control.feedback.is_some() != self.hsdpa_feedback().is_some()
                        || control.startup.as_ref().is_some_and(|suffix| {
                            suffix.frame.epoch != activation.epoch
                                || (!suffix.automatic
                                    && (suffix.frame.index < activation.index
                                        || suffix
                                            .frame
                                            .index
                                            .checked_sub(self.frame.index)
                                            .and_then(|delta| delta.checked_mul(self.rate() as u64))
                                            .and_then(|delta| {
                                                self.predicted.sample().index.checked_add(delta)
                                            })
                                            .and_then(|base| {
                                                base.checked_add(
                                                    u64::from(suffix.slot)
                                                        * CHIPS_PER_SLOT
                                                        * self.rate() as u64,
                                                )
                                            })
                                            .is_none()))
                        })
                })
            {
                return Err(ReceiveError::Configuration);
            }
            let added = candidate.retained_bytes()?;
            if candidate.selection.is_some() {
                self.chips.validate_selection_boundary(activation)?;
            }
            if added > maximum_bytes.get() {
                return Err(ReceiveError::Capacity);
            }
            self.sample_bytes
                .checked_add(added)
                .ok_or(ReceiveError::Capacity)
        })();
        let bytes = match admission {
            Ok(bytes) => bytes,
            Err(error) => return Err((error, candidate)),
        };
        let super::HsupaAttachment {
            receiver,
            control,
            selection,
        } = candidate;
        if let Err((error, receiver)) =
            self.chips
                .attach_hsupa_inner(receiver, activation, maximum_bytes, control.is_some())
        {
            return Err((
                error,
                super::HsupaAttachment {
                    receiver,
                    control,
                    selection,
                },
            ));
        }
        self.chips.coexisting_selection = selection;
        self.control_transition =
            control.map(|prepared| super::control_transition::ControlTransition {
                activation,
                prepared,
                phase: super::control_transition::ControlTransitionPhase::AwaitEdch,
            });
        self.sample_bytes = bytes;
        Ok(())
    }

    pub fn hsupa_pending(&self) -> bool {
        self.chips.hsupa_pending()
    }

    pub(super) fn restored_dch_at(&self) -> Option<ChipInstant> {
        self.chips.restored_dch_at()
    }

    pub(super) fn detachable_hsupa_bytes(
        &self,
        restored: ChipInstant,
    ) -> Result<usize, ReceiveError> {
        if self.control_transition.as_ref().is_some_and(|transition| {
            transition.phase != super::control_transition::ControlTransitionPhase::Dch
        }) {
            return Err(ReceiveError::Time);
        }
        self.chips
            .detachable_hsupa_bytes(restored)?
            .checked_add(
                self.control_transition
                    .as_ref()
                    .map_or(0, |transition| transition.prepared.retained_bytes()),
            )
            .and_then(|bytes| {
                bytes.checked_add(
                    self.chips
                        .coexisting_selection
                        .as_ref()
                        .map_or(0, |selection| selection.retained_bytes()),
                )
            })
            .ok_or(ReceiveError::Capacity)
    }

    pub fn detach_restored_hsupa(
        &mut self,
        restored: ChipInstant,
    ) -> Result<super::HsupaAttachment, ReceiveError> {
        let bytes = self.detachable_hsupa_bytes(restored)?;
        let receiver = self.chips.detach_restored_hsupa(restored)?;
        self.sample_bytes -= bytes;
        Ok(super::HsupaAttachment {
            receiver,
            selection: self.chips.coexisting_selection.take(),
            control: self
                .control_transition
                .take()
                .map(|transition| transition.prepared),
        })
    }

    pub(crate) fn hsupa_attachment(&self) -> Option<(ReceiveIdentity, ChipInstant)> {
        self.chips.hsupa_attachment()
    }

    pub(super) fn cancellable_hsupa_bytes(
        &self,
        activation: ChipInstant,
    ) -> Result<usize, ReceiveError> {
        if self.control_transition.as_ref().is_some_and(|transition| {
            transition.phase != super::control_transition::ControlTransitionPhase::AwaitEdch
        }) {
            return Err(ReceiveError::Time);
        }
        self.chips
            .cancellable_hsupa_bytes(activation)?
            .checked_add(
                self.control_transition
                    .as_ref()
                    .map_or(0, |transition| transition.prepared.retained_bytes()),
            )
            .and_then(|bytes| {
                bytes.checked_add(
                    self.chips
                        .coexisting_selection
                        .as_ref()
                        .map_or(0, |selection| selection.retained_bytes()),
                )
            })
            .ok_or(ReceiveError::Capacity)
    }

    pub fn cancel_hsupa(
        &mut self,
        activation: ChipInstant,
    ) -> Result<super::HsupaAttachment, ReceiveError> {
        let bytes = self.cancellable_hsupa_bytes(activation)?;
        let receiver = self.chips.cancel_hsupa(activation)?;
        self.sample_bytes -= bytes;
        Ok(super::HsupaAttachment {
            receiver,
            selection: self.chips.coexisting_selection.take(),
            control: self
                .control_transition
                .take()
                .map(|transition| transition.prepared),
        })
    }
    pub fn hsupa_dropped_intervals(&self) -> u64 {
        self.chips.hsupa_dropped_intervals()
    }

    pub fn schedule_dch_restoration(
        &mut self,
        activation: ChipInstant,
    ) -> Result<(), ReceiveError> {
        use super::control_transition::ControlTransitionPhase;
        if activation.epoch != self.frame.epoch
            || activation.index < self.frame.index
            || activation.index == self.frame.index && self.slot != 0
        {
            return Err(ReceiveError::Time);
        }
        if self.control_transition.is_none() {
            if self.coexistence_restoration.is_some() {
                return Err(ReceiveError::Time);
            }
            self.chips.validate_coexistence_restoration(activation)?;
            self.coexistence_restoration = Some(activation);
            return Ok(());
        }
        let transition = self
            .control_transition
            .as_mut()
            .ok_or(ReceiveError::Configuration)?;
        if transition.phase != ControlTransitionPhase::Edch
            || activation.epoch != transition.activation.epoch
            || activation.index <= transition.activation.index
            || !(activation.index - transition.activation.index).is_multiple_of(CHIPS_PER_FRAME)
            || activation.index < self.frame.index
            || activation.index == self.frame.index && self.slot != 0
        {
            return Err(ReceiveError::Time);
        }
        transition.phase = ControlTransitionPhase::AwaitDch(activation);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn confirm_startup_slots_for_test(&mut self) {
        assert_eq!(self.counters.searches, 0);
        assert!(self.startup_minimum_restore.is_none());
        self.startup_slot_confirmation = true;
    }

    #[cfg(test)]
    pub(crate) fn disable_slot_preflight_for_test(&mut self) {
        assert_eq!(self.counters.searches, 0);
        self.slot_preflight_disabled = true;
    }

    #[cfg(test)]
    pub(crate) fn disable_timing_peak_reuse_for_test(&mut self) {
        assert_eq!(self.counters.searches, 0);
        self.timing_peak_reuse_disabled = true;
    }

    #[cfg(test)]
    pub(crate) fn timing_slot_for_test(&self) -> u8 {
        self.search.slot()
    }

    #[cfg(test)]
    pub(crate) fn trace_acquisition_for_test(&mut self, start_chip: u64) {
        assert!(self.acquisition_trace.is_none() && self.counters.searches == 0);
        self.acquisition_trace = Some((start_chip, Vec::with_capacity(16)));
    }

    #[cfg(test)]
    pub(crate) fn acquisition_trace_for_test(&self) -> &[AcquisitionObservation] {
        self.acquisition_trace
            .as_ref()
            .map_or(&[], |(_, records)| records.as_slice())
    }

    pub(super) fn hsupa_decode_work_per_slot(&self) -> usize {
        self.chips.hsupa_decode_work_per_slot()
    }
    pub(super) fn discard_incomplete_hsupa(&mut self) {
        self.chips.discard_incomplete_hsupa();
    }

    pub(super) fn service_decodes(
        &mut self,
        work: &mut umts_dsp::coding::DecodeWork,
        mut observe: impl FnMut(super::UplinkDecodeObservation),
    ) -> Result<usize, ReceiveError> {
        let first = self.hsupa_first;
        self.hsupa_first = !first;
        if first {
            let hsupa = self.chips.service_hsupa(work, &mut observe)?;
            Ok(hsupa
                + self.session_mut().service_with_observer(work, |decoded| {
                    observe(super::UplinkDecodeObservation::Dch(decoded));
                })?)
        } else {
            let rel99 = self.session_mut().service_with_observer(work, |decoded| {
                observe(super::UplinkDecodeObservation::Dch(decoded));
            })?;
            Ok(rel99 + self.chips.service_hsupa(work, observe)?)
        }
    }
    pub fn counters(&self) -> SampleReceiveCounters {
        self.counters
    }
    pub fn tracking_status(&self) -> SampleTrackingStatus {
        SampleTrackingStatus {
            predicted: self.predicted,
            acquired: self.acquired,
            carrier_seed: self.carrier_seed,
            contiguous: self.contiguous_frame,
            confirmation: self
                .reacquisition_candidate
                .map(|candidate| (candidate.frame, candidate.sample)),
            radio_link: self.radio_link.status(),
        }
    }
    pub fn rate(&self) -> usize {
        self.settings.rate.get()
    }
    pub fn first_receive(&self) -> SamplePosition {
        self.settings.first_receive
    }
    pub fn next_frame(&self) -> ChipInstant {
        self.frame
    }
    /// The requested time is the UE-local uplink frame coordinate, not a cell downlink frame.
    pub fn predicted_receive(&self, frame: ChipInstant) -> Result<SamplePosition, ReceiveError> {
        let delta = frame
            .index
            .checked_sub(self.frame.index)
            .ok_or(ReceiveError::Time)?;
        if frame.epoch != self.frame.epoch || !delta.is_multiple_of(CHIPS_PER_FRAME) {
            return Err(ReceiveError::Time);
        }
        let index = delta
            .checked_mul(self.rate() as u64)
            .and_then(|delta| self.predicted.sample().index.checked_add(delta))
            .ok_or(ReceiveError::Time)?;
        Ok(SamplePosition::new(
            SampleInstant {
                index,
                ..self.predicted.sample()
            },
            self.predicted.fraction(),
        )?)
    }
    pub(super) fn search_radius(&self) -> usize {
        self.settings.search_radius_samples
    }
    pub fn required_history(&self, guard: usize) -> usize {
        self.search_length
            .max(CHIPS_PER_SLOT as usize * self.rate() + 2 * guard + 1)
    }
    pub fn chip_counters(&self) -> super::ChipReceiveCounters {
        self.chips.counters()
    }
    fn advance_frames(&mut self, count: u64) -> Result<(), ReceiveError> {
        self.restore_startup_plan()?;
        self.pending_timing_peak = None;
        let chips = count
            .checked_mul(CHIPS_PER_FRAME)
            .ok_or(ReceiveError::Time)?;
        let samples = chips
            .checked_mul(self.rate() as u64)
            .ok_or(ReceiveError::Time)?;
        let receive = self
            .predicted
            .sample()
            .index
            .checked_add(samples)
            .ok_or(ReceiveError::Time)?;
        let frame = self
            .frame
            .index
            .checked_add(chips)
            .ok_or(ReceiveError::Time)?;
        self.predicted = SamplePosition::new(
            SampleInstant {
                index: receive,
                ..self.predicted.sample()
            },
            self.predicted.fraction(),
        )?;
        self.frame.index = frame;
        self.slot = 0;
        self.acquired = None;
        Ok(())
    }

    // Startup recovery is considered only when the initial frame's samples
    // have already been lost. Keep its geometry out of the ordinary slot path.
    #[cold]
    #[inline(never)]
    fn initial_suffix_hint(
        &self,
        first: u64,
        guard: usize,
    ) -> Result<Option<(ChipInstant, u8)>, ReceiveError> {
        let Some(transition) = self.control_transition.as_ref() else {
            return Ok(None);
        };
        if !matches!(
            transition.phase,
            super::control_transition::ControlTransitionPhase::AwaitEdch
                | super::control_transition::ControlTransitionPhase::Edch
        ) {
            return Ok(None);
        }
        let Some(suffix) = transition
            .prepared
            .startup
            .as_ref()
            .filter(|s| !s.attempted)
        else {
            return Ok(None);
        };
        let hint = if suffix.automatic {
            self.control_search_geometry().initial_suffix_from_samples(
                self.frame,
                self.predicted.sample(),
                SampleInstant {
                    epoch: self.frame.epoch,
                    index: first,
                },
                guard,
            )?
        } else {
            Some((suffix.frame, suffix.slot))
        };
        Ok(hint.filter(|(frame, _)| frame.index >= transition.activation.index))
    }

    fn restore_startup_plan(&mut self) -> Result<(), ReceiveError> {
        if self.search.slot() == 0 {
            return Ok(());
        }
        let suffix = self
            .control_transition
            .as_mut()
            .and_then(|t| t.prepared.startup.as_mut())
            .ok_or(ReceiveError::Configuration)?;
        if suffix.plan.slot() != 0 {
            return Err(ReceiveError::Configuration);
        }
        self.search.exchange_plan(&mut suffix.plan)?;
        self.pending_timing_peak = None;
        Ok(())
    }
    fn discard_frames(&mut self, count: u64) -> Result<(), ReceiveError> {
        let elapsed = self.frame;
        self.advance_frames(count)?;
        if count == 0 {
            return Ok(());
        }
        self.tracking_candidate = None;
        // Acquisition can reject a frame before its end has arrived, so do not publish that future end.
        self.chips.discard_hsdpa_before(elapsed)
    }

    fn receive_position(&self, measured: SamplePosition) -> Result<SamplePosition, ReceiveError> {
        if !self.contiguous_frame {
            return Ok(measured);
        }
        let predicted = self.predicted.sample();
        let distance = measured.sample().index.abs_diff(predicted.index);
        if measured.sample().epoch != predicted.epoch
            || distance > self.settings.search_radius_samples as u64 + 1
        {
            return Err(ReceiveError::Time);
        }
        let delta = if measured.sample().index >= predicted.index {
            distance as f64
        } else {
            -(distance as f64)
        } + measured.fraction()
            - self.predicted.fraction();
        // Continuous tracking filters measurement jitter without changing the logical frame clock.
        let fraction = self.predicted.fraction() + TIMING_TRACKING_FRACTION * delta;
        let shift = fraction.floor();
        let index = if shift >= 0.0 {
            predicted.index.checked_add(shift as u64)
        } else {
            predicted.index.checked_sub((-shift) as u64)
        }
        .ok_or(ReceiveError::Time)?;
        Ok(SamplePosition::new(
            SampleInstant { index, ..predicted },
            fraction - shift,
        )?)
    }

    pub(super) fn step_with_feedback(
        &mut self,
        history: &SampleHistory,
        sampler: &FractionalSampler,
        boundary: Option<ChipInstant>,
        clock: Option<&crate::radio_io::RadioClock>,
    ) -> Result<SampleProgress, ReceiveError> {
        if self.session().failed {
            return Err(ReceiveError::Failed);
        }
        if self.session().retired {
            return Err(ReceiveError::Retired);
        }
        self.chips.service_hsdpa_feedback();
        if let Some(boundary) = boundary
            && self.frame.index >= boundary.index
        {
            return Ok(SampleProgress::Boundary(boundary));
        }
        if let Some(power) = &mut self.power
            && !power.ready()?
        {
            return Ok(SampleProgress::AwaitFeedback);
        }
        let Some((first, end, generation)) = history.bounds() else {
            return Ok(SampleProgress::AwaitSamples);
        };
        if self.generation.is_some_and(|old| old != generation) {
            self.contiguous_frame = false;
            self.tracking_quality.reset();
            self.reacquisition_candidate = None;
            self.discard_frames(u64::from(self.slot != 0 || self.acquired.is_some()))?;
        }
        self.generation = Some(generation);
        let guard = sampler.guard_samples();
        let radius = self.settings.search_radius_samples as u64;
        let margin = radius.max(guard as u64 + 1);
        let earliest = first.checked_add(margin).ok_or(ReceiveError::Time)?;
        let suffix_earliest = first
            .checked_add(radius + guard as u64 + 1)
            .ok_or(ReceiveError::Time)?;
        if self.acquired.is_none()
            && self
                .predicted
                .sample()
                .index
                .checked_add(self.slot as u64 * CHIPS_PER_SLOT * self.rate() as u64)
                .ok_or(ReceiveError::Time)?
                < earliest
        {
            self.contiguous_frame = false;
            self.tracking_quality.reset();
            self.reacquisition_candidate = None;
            let span = CHIPS_PER_FRAME * self.rate() as u64;
            let missing = earliest - self.predicted.sample().index;
            let floor = missing / span;
            let suffix_hint = if self.slot == 0 {
                self.initial_suffix_hint(first, guard)?
            } else {
                None
            };
            let suffix_fits = suffix_hint.is_some_and(|(frame, slot)| {
                self.frame.index.checked_add(floor * CHIPS_PER_FRAME) == Some(frame.index)
                    && self
                        .predicted
                        .sample()
                        .index
                        .checked_add(floor * span)
                        .and_then(|base| {
                            base.checked_add(u64::from(slot) * CHIPS_PER_SLOT * self.rate() as u64)
                        })
                        .is_some_and(|at| at >= suffix_earliest)
            });
            let count = if suffix_fits {
                floor
            } else {
                missing.div_ceil(span)
            };
            self.discard_frames(count)?;
            self.counters.history_lost_frames =
                self.counters.history_lost_frames.saturating_add(count);
            self.chips.observe_sample_progress(self.counters, None);
        }
        if let Some(boundary) = boundary
            && self.frame.index >= boundary.index
        {
            return Ok(SampleProgress::Boundary(boundary));
        }
        if self
            .coexistence_restoration
            .is_some_and(|at| self.frame.index >= at.index)
        {
            if self.slot != 0 {
                return Err(ReceiveError::Time);
            }
            self.chips.restore_coexisting_dch(self.frame)?;
            self.pending_timing_peak = None;
            self.coexistence_restoration = None;
        }
        if let Some(transition) = &mut self.control_transition {
            use super::control_transition::ControlTransitionPhase;
            let next = match transition.phase {
                ControlTransitionPhase::AwaitEdch
                    if self.frame.index >= transition.activation.index =>
                {
                    Some(ControlTransitionPhase::Edch)
                }
                ControlTransitionPhase::AwaitDch(at) if self.frame.index >= at.index => {
                    Some(ControlTransitionPhase::Dch)
                }
                _ => None,
            };
            if let Some(next) = next {
                self.reacquisition_candidate = None;
                if self.slot != 0 {
                    return Err(ReceiveError::Time);
                }
                self.tracking_candidate = None;
                match next {
                    ControlTransitionPhase::Edch => self
                        .chips
                        .activate_edch_only_control(self.frame, &mut transition.prepared)?,
                    ControlTransitionPhase::Dch => self
                        .chips
                        .restore_dch_control(self.frame, &mut transition.prepared)?,
                    _ => unreachable!("only active control phases are selected"),
                }
                transition
                    .prepared
                    .exchange_search(&mut self.search, &mut self.search_length);
                self.pending_timing_peak = None;
                transition.phase = next;
            }
        }
        if self.acquired.is_none() && self.slot == 0 && self.predicted.sample().index < earliest {
            let rate = self.rate() as u64;
            if let Some((frame, slot)) = self.initial_suffix_hint(first, guard)?
                && let Some(transition) = &mut self.control_transition
                && transition.phase == super::control_transition::ControlTransitionPhase::Edch
                && let Some(suffix) = &mut transition.prepared.startup
                && frame == self.frame
                && self
                    .predicted
                    .sample()
                    .index
                    .checked_add(u64::from(slot) * CHIPS_PER_SLOT * rate)
                    .is_some_and(|at| at >= suffix_earliest)
            {
                suffix.select_slot(slot)?;
                suffix.frame = frame;
                suffix.attempted = true;
                self.search.exchange_plan(&mut suffix.plan)?;
                self.slot = usize::from(suffix.slot);
                self.pending_timing_peak = None;
            }
        }
        let mut first_slot_sampled = false;
        if self.acquired.is_none() {
            let slot_offset = self.slot as u64 * CHIPS_PER_SLOT * self.rate() as u64;
            let begin = self
                .predicted
                .sample()
                .index
                .checked_add(slot_offset)
                .ok_or(ReceiveError::Time)?
                .checked_sub(radius)
                .ok_or(ReceiveError::Time)?;
            // A valid interpolated peak is no earlier than predicted-radius-1.
            // Even that earliest position needs a full slot and the sampler's
            // right guard. Searching before this necessary bound cannot emit a slot.
            let minimum_slot = (CHIPS_PER_SLOT as usize - 1) * self.rate() + guard + 1;
            let required = self.search_length.max(minimum_slot);
            #[cfg(test)]
            let required = if self.slot_preflight_disabled {
                self.search_length
            } else {
                required
            };
            let stop = begin
                .checked_add(required as u64)
                .ok_or(ReceiveError::Time)?;
            if stop > end {
                return Ok(SampleProgress::AwaitSamples);
            }
            let input = history
                .window(begin, self.search_length)
                .ok_or(ReceiveError::Time)?;
            let search_begin = SampleInstant {
                index: begin,
                ..self.predicted.sample()
            };
            #[cfg(test)]
            let reuse = !self.timing_peak_reuse_disabled;
            #[cfg(not(test))]
            let reuse = true;
            // Retained history is immutable within a continuity generation. Still validate
            // its window above: a cached measurement cannot resurrect evicted samples.
            let cached = self.pending_timing_peak.filter(|cached| {
                reuse
                    && cached.frame == self.frame
                    && cached.continuity == generation
                    && cached.begin == search_begin
                    && cached.length == self.search_length
                    && cached.carrier_bits == self.carrier_seed.hz().to_bits()
            });
            let mut peak = if let Some(cached) = cached {
                #[cfg(test)]
                {
                    self.counters.timing_peak_reuses =
                        self.counters.timing_peak_reuses.saturating_add(1);
                }
                cached.peak
            } else {
                self.pending_timing_peak = None;
                #[cfg(test)]
                {
                    self.counters.timing_kernel_calls =
                        self.counters.timing_kernel_calls.saturating_add(1);
                    if self.counters.acquired_frames == 0 {
                        self.counters.startup_timing_search_calls =
                            self.counters.startup_timing_search_calls.saturating_add(1);
                    }
                }
                let peak = self
                    .search
                    .search(search_begin, input, self.carrier_seed)?
                    .strongest();
                if reuse {
                    self.pending_timing_peak = Some(PendingTimingPeak {
                        frame: self.frame,
                        continuity: generation,
                        begin: search_begin,
                        length: self.search_length,
                        carrier_bits: self.carrier_seed.hz().to_bits(),
                        peak,
                    });
                }
                peak
            };
            let delta = peak.fractional_offset_samples;
            let measured_integer_coherence = peak.despread_coherent_fraction;
            let mut integer = peak.position.sample();
            let fraction = if delta < 0.0 && 1.0 + delta < 1.0 {
                integer.index = integer.index.checked_sub(1).ok_or(ReceiveError::Time)?;
                1.0 + delta
            } else {
                delta.max(0.0)
            };
            let position = self.receive_position(SamplePosition::new(integer, fraction)?)?;
            let integer = position.sample();
            let begin = integer
                .index
                .checked_sub(guard as u64)
                .ok_or(ReceiveError::Time)?;
            let length = (CHIPS_PER_SLOT as usize - 1) * self.rate() + 2 * guard + 2;
            if begin < first {
                self.reacquisition_candidate = None;
                self.counters.history_lost_frames =
                    self.counters.history_lost_frames.saturating_add(1);
                self.discard_frames(1)?;
                return Ok(SampleProgress::Skipped);
            }
            if begin.checked_add(length as u64).ok_or(ReceiveError::Time)? > end {
                return Ok(SampleProgress::AwaitSamples);
            }
            sampler.write_validated_chips(
                SampleInstant {
                    index: begin,
                    ..integer
                },
                history
                    .evidence_window(begin, length)
                    .ok_or(ReceiveError::Time)?,
                position,
                self.settings.rate,
                &mut self.sampler_output,
            )?;
            peak.despread_coherent_fraction = self.search.interpolated_pilot_coherence(
                SampleInstant {
                    epoch: self.frame.epoch,
                    index: self
                        .frame
                        .index
                        .checked_add(self.slot as u64 * CHIPS_PER_SLOT)
                        .ok_or(ReceiveError::Time)?,
                },
                &self.sampler_output,
                self.carrier_seed,
            )?;
            first_slot_sampled = true;
            let pilot_count = self.chips.pilot_count();
            let pilot_noise =
                (1.0 - peak.despread_coherent_fraction).powi((pilot_count - 1) as i32);
            let hypotheses = (self.settings.search_radius_samples * 2 + 1) as f64;
            let probability_budget = TRACKING_PILOT_NOISE_PROBABILITY / 2.0;
            let measured_significant =
                (1.0 - measured_integer_coherence).powi((pilot_count - 1) as i32) * hypotheses
                    <= probability_budget;
            let mut significant = pilot_noise * hypotheses <= probability_budget;
            #[cfg(test)]
            let startup_slot_probe = self.startup_slot_confirmation;
            #[cfg(not(test))]
            let startup_slot_probe = false;
            let startup_slot_probe = startup_slot_probe || self.slot != 0;
            let mut startup_slot_confirmed = false;
            if (peak.coherent_fraction < self.settings.minimum_coherent_fraction || self.slot != 0)
                && (!significant || startup_slot_probe)
                && !self.contiguous_frame
            {
                let mut product = pilot_noise;
                let mut independent_pilots =
                    significant && measured_significant && peak.despread_coherent_fraction >= 0.5;
                for distance in 1..=2 {
                    let following_sample = SampleInstant {
                        index: position
                            .sample()
                            .index
                            .checked_add(distance * CHIPS_PER_SLOT * self.rate() as u64)
                            .ok_or(ReceiveError::Time)?,
                        ..position.sample()
                    };
                    let following = SamplePosition::new(following_sample, position.fraction())?;
                    let following_begin = following_sample
                        .index
                        .checked_sub(guard as u64)
                        .ok_or(ReceiveError::Time)?;
                    if following_begin
                        .checked_add(length as u64)
                        .ok_or(ReceiveError::Time)?
                        > end
                    {
                        return Ok(SampleProgress::AwaitSamples);
                    }
                    sampler.write_validated_prefix_chips(
                        SampleInstant {
                            index: following_begin,
                            ..following_sample
                        },
                        history
                            .evidence_window(following_begin, length)
                            .ok_or(ReceiveError::Time)?,
                        following,
                        self.settings.rate,
                        self.sampler_output.len(),
                        &mut self.sampler_output[..pilot_count * 256],
                    )?;
                    let following_start = SampleInstant {
                        index: self
                            .frame
                            .index
                            .checked_add((self.slot as u64 + distance) * CHIPS_PER_SLOT)
                            .ok_or(ReceiveError::Time)?,
                        epoch: self.frame.epoch,
                    };
                    let coherence = if distance == 1 {
                        self.search.following_pilot_coherence(
                            following_start,
                            &self.sampler_output,
                            self.carrier_seed,
                        )?
                    } else {
                        self.search.third_pilot_coherence(
                            following_start,
                            &self.sampler_output,
                            self.carrier_seed,
                        )?
                    };
                    #[cfg(test)]
                    if self.counters.acquired_frames == 0 {
                        self.counters.startup_confirmation_slot_calls = self
                            .counters
                            .startup_confirmation_slot_calls
                            .saturating_add(1);
                    }
                    first_slot_sampled = false;
                    self.counters.pilot_confirmation_slots =
                        self.counters.pilot_confirmation_slots.saturating_add(1);
                    // Fisher's tail combines adjacent pilot evidence without another timing search.
                    product *= (1.0 - coherence).powi((pilot_count - 1) as i32);
                    let joint_probability = if product == 0.0 {
                        0.0
                    } else {
                        let log = -product.ln();
                        product * (1.0 + log + if distance == 2 { log * log / 2.0 } else { 0.0 })
                    };
                    // Split the remaining false-admission budget between the two confirmation decisions.
                    let joint_significant =
                        joint_probability * hypotheses <= probability_budget / 2.0;
                    // The optional third-slot probe cannot revoke earlier valid evidence.
                    significant |= joint_significant;
                    if startup_slot_probe {
                        independent_pilots &= coherence >= 0.5
                            && (1.0 - coherence).powi((pilot_count - 1) as i32) * hypotheses
                                <= probability_budget;
                        startup_slot_confirmed = distance == 2
                            && independent_pilots
                            && joint_probability * hypotheses <= (probability_budget / 2.0).powi(2);
                    }
                    if significant && (!startup_slot_probe || distance == 2) {
                        break;
                    }
                }
                self.counters.pilot_confirmed_frames = self
                    .counters
                    .pilot_confirmed_frames
                    .saturating_add(u64::from(significant));
            }
            #[cfg(test)]
            if self.counters.acquired_frames == 0 {
                self.counters.startup_completed_searches =
                    self.counters.startup_completed_searches.saturating_add(1);
            }
            self.counters.searches = self.counters.searches.saturating_add(1);
            let candidate = ReacquisitionCandidate {
                frame: self.frame,
                sample: peak
                    .position
                    .sample()
                    .index
                    .checked_sub(slot_offset)
                    .ok_or(ReceiveError::Time)?,
            };
            let confirmed = significant
                && self
                    .reacquisition_candidate
                    .is_some_and(|previous| previous.confirms(candidate, self.rate()));
            let coherent_tracking = self.contiguous_frame || confirmed || startup_slot_confirmed;
            let predicted_peak = self
                .predicted
                .sample()
                .index
                .checked_add(u64::from(self.predicted.fraction() >= 0.5))
                .ok_or(ReceiveError::Time)?;
            // A peak outside the tracked chip's correlation lobe needs its own pilot evidence.
            let supported_peak = candidate.sample.abs_diff(predicted_peak) < self.rate() as u64
                || measured_significant;
            let stable_tracking = self.contiguous_frame
                && supported_peak
                && self.tracking_quality.confirmed()
                && self
                    .tracking_candidate
                    .is_some_and(|previous| previous.confirms(candidate, self.rate()));
            if let Some((start, records)) = &mut self.acquisition_trace
                && self.frame.index >= *start
                && records.len() < records.capacity()
            {
                records.push(AcquisitionObservation {
                    frame: self.frame,
                    peak,
                    measured_integer_coherence,
                    measured_significant,
                    predicted: self.predicted,
                    filtered: position,
                    carrier_seed: self.carrier_seed,
                    contiguous: self.contiguous_frame,
                    significant,
                    confirmed,
                    accepted: (self.slot == 0 || startup_slot_confirmed)
                        && (peak.coherent_fraction >= self.settings.minimum_coherent_fraction
                            || coherent_tracking),
                    uses_prediction: self.contiguous_frame
                        && peak.coherent_fraction < self.settings.minimum_coherent_fraction
                        && !significant
                        && !stable_tracking,
                    stable_tracking,
                });
            }
            if (self.slot != 0 && !startup_slot_confirmed)
                || (peak.coherent_fraction < self.settings.minimum_coherent_fraction
                    && !coherent_tracking)
            {
                self.contiguous_frame = false;
                self.reacquisition_candidate = significant.then_some(candidate);
                self.counters.rejected_frames = self.counters.rejected_frames.saturating_add(1);
                self.chips
                    .observe_sample_progress(self.counters, Some(self.frame));
                self.discard_frames(1)?;
                return Ok(SampleProgress::Skipped);
            }
            #[cfg(test)]
            if let Some(minimum) = self.startup_minimum_restore.take() {
                self.settings.minimum_coherent_fraction = minimum;
            }
            #[cfg(test)]
            {
                self.startup_slot_confirmation = false;
            }
            self.reacquisition_candidate = None;
            self.pending_timing_peak = None;
            self.tracking_candidate = supported_peak.then_some(candidate);
            if self.contiguous_frame
                && peak.coherent_fraction < self.settings.minimum_coherent_fraction
                && !significant
                && !stable_tracking
            {
                first_slot_sampled = false;
                self.acquired = Some(self.predicted);
                self.counters.predicted_timing_frames =
                    self.counters.predicted_timing_frames.saturating_add(1);
            } else {
                let frame_position = SamplePosition::new(
                    SampleInstant {
                        index: position
                            .sample()
                            .index
                            .checked_sub(slot_offset)
                            .ok_or(ReceiveError::Time)?,
                        ..position.sample()
                    },
                    position.fraction(),
                )?;
                self.predicted = frame_position;
                self.acquired = Some(frame_position);
            }
            self.tracking_quality.begin_frame();
            self.counters.acquired_frames = self.counters.acquired_frames.saturating_add(1);
            if self.slot == 0
                && let Some(transition) = &mut self.control_transition
                && transition.phase == super::control_transition::ControlTransitionPhase::Edch
                && let Some(suffix) = &mut transition.prepared.startup
            {
                suffix.attempted = true;
            }
            self.chips
                .observe_sample_progress(self.counters, Some(self.frame));
        }
        let frame = self.acquired.expect("acquired receive frame");
        let mut position = frame.sample();
        position.index = position
            .index
            .checked_add(self.slot as u64 * CHIPS_PER_SLOT * self.rate() as u64)
            .ok_or(ReceiveError::Time)?;
        let begin = position
            .index
            .checked_sub(guard as u64)
            .ok_or(ReceiveError::Time)?;
        let length = (CHIPS_PER_SLOT as usize - 1) * self.rate() + 2 * guard + 2;
        if begin < first {
            self.reacquisition_candidate = None;
            self.counters.history_lost_frames = self.counters.history_lost_frames.saturating_add(1);
            self.discard_frames(1)?;
            return Ok(SampleProgress::Skipped);
        }
        if begin.checked_add(length as u64).ok_or(ReceiveError::Time)? > end {
            return Ok(SampleProgress::AwaitSamples);
        }
        let input = history
            .evidence_window(begin, length)
            .ok_or(ReceiveError::Time)?;
        if !first_slot_sampled {
            sampler.write_validated_chips(
                SampleInstant {
                    index: begin,
                    ..position
                },
                input,
                SamplePosition::new(position, frame.fraction())?,
                self.settings.rate,
                &mut self.sampler_output,
            )?;
        }
        let logical = ChipInstant {
            index: self
                .frame
                .index
                .checked_add(self.slot as u64 * CHIPS_PER_SLOT)
                .ok_or(ReceiveError::Time)?,
            ..self.frame
        };
        let progress = self.chips.receive_slot(
            self.chips.session().identity(),
            logical,
            &self.sampler_output,
        )?;
        let control = match progress {
            ChipProgress::Slot(control) | ChipProgress::Frame { control, .. } => Some(control),
            ChipProgress::AwaitFrame => None,
        };
        if let Some(carrier) = control.and_then(|control| control.carrier) {
            self.carrier_seed = CarrierOffsetHz::new(carrier.next_hz)?;
        }
        if let Some(control) = control {
            if self.tracking_quality.observe(control.pilot)
                && let Some(carrier) = control.carrier
            {
                // These pilots validate the applied correction, not the next proposed update.
                self.confirmed_carrier = CarrierOffsetHz::new(carrier.applied_hz)?;
            }
            use crate::dedicated_tx::{DownlinkCommand, DownlinkControl};
            // TS 25.214 5.2.1.2.2: TPCest is 0 or 1 for every transmitted slot, so the sign of
            // the TPC correlation decides even when the pilot is below the coherence floor. A
            // tie or an unusable correlation estimates 1, which keeps the link rather than
            // starving it. Only a slot with no finite observation at all leaves the power.
            let command = if !control.tpc.is_finite() {
                DownlinkCommand::Hold
            } else if control.tpc <= 0.0 {
                DownlinkCommand::Up
            } else {
                DownlinkCommand::Down
            };
            self.downlink_control = Some(DownlinkControl {
                identity: self.session().identity(),
                measurement_end: ChipInstant {
                    index: logical
                        .index
                        .checked_add(CHIPS_PER_SLOT)
                        .ok_or(ReceiveError::Time)?,
                    ..logical
                },
                command,
            });
            if let Some(power) = &mut self.power {
                power.observed_control(logical, control);
            }
        }
        let identity = self.session().identity();
        if let Some(power) = &mut self.power
            && let Some(control) = control
        {
            let (sample_generation, continuity) = history.provenance().ok_or(ReceiveError::Time)?;
            let latest_input = history.latest_input().ok_or(ReceiveError::Time)?;
            let now = clock
                .ok_or(ReceiveError::FeedbackClosed)?
                .read_upper_bound()
                .map_err(|_| ReceiveError::FeedbackClosed)?
                .chip
                .ok_or(ReceiveError::FeedbackClosed)?;
            let now = ChipInstant {
                index: now.index.checked_add(1).ok_or(ReceiveError::Time)?,
                ..now
            };
            self.chips.observe_hsupa_collection_clock(logical, now);
            let ready_at = ChipInstant {
                index: now
                    .index
                    .max(latest_input.div_ceil(self.settings.rate.get() as u64)),
                ..now
            };
            if power.observing(ready_at)? {
                let observation = PilotObservation {
                    identity,
                    uplink_slot: logical,
                    measurement_end: SampleInstant {
                        index: begin.checked_add(length as u64).ok_or(ReceiveError::Time)?,
                        ..position
                    },
                    ready_at,
                    sample_generation,
                    continuity,
                    pilot: control.pilot,
                    pilot_count: self.chips.pilot_count(),
                };
                match power.controller.observe(observation) {
                    Ok(feedback) => {
                        power.produced(feedback);
                    }
                    Err(
                        error @ (super::PowerControlError::Stale
                        | super::PowerControlError::InvalidPilot
                        | super::PowerControlError::Coherence),
                    ) => power.discard_observation(error, observation),
                    Err(error) => {
                        power.fail_observation(error, observation);
                        let error = ReceiveError::PowerControl(error);
                        return Err(error);
                    }
                }
            }
        }
        self.slot += 1;
        self.counters.sampled_slots = self.counters.sampled_slots.saturating_add(1);
        if self.slot == SLOTS_PER_FRAME as usize {
            let partial = self.search.slot() != 0;
            self.advance_frames(1)?;
            let was_tracking = self.contiguous_frame;
            self.contiguous_frame = self.tracking_quality.finish_frame(was_tracking);
            // TS 25.214 4.3.1.3: one in-sync or out-of-sync indication per radio frame.
            match if partial {
                RadioLinkEvent::None
            } else {
                self.radio_link.observe_frame(self.contiguous_frame)
            } {
                RadioLinkEvent::Failure => {
                    self.counters.radio_link_failures =
                        self.counters.radio_link_failures.saturating_add(1);
                }
                RadioLinkEvent::Restore => {
                    self.counters.radio_link_restores =
                        self.counters.radio_link_restores.saturating_add(1);
                }
                RadioLinkEvent::None => {}
            }
            if was_tracking && !self.contiguous_frame {
                self.counters.tracking_losses = self.counters.tracking_losses.saturating_add(1);
                self.carrier_seed = self.confirmed_carrier;
                self.chips
                    .restore_confirmed_carrier(self.confirmed_carrier)?;
            }
        }
        Ok(SampleProgress::Slot(progress))
    }
}
impl ReceiveOwner for DedicatedSampleReceiver {
    fn session(&self) -> &DedicatedReceiveSession {
        self.chips.session()
    }
    fn session_mut(&mut self) -> &mut DedicatedReceiveSession {
        self.chips.session_mut()
    }
    fn last_consumed_frame(&self) -> Option<ChipInstant> {
        self.chips.last_consumed_frame()
    }
    fn resources(&self) -> ReceiveResources {
        ReceiveResources {
            physical_sample_bytes: self.sample_bytes,
            ..self.chips.resources()
        }
    }
}
