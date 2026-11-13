// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    DedicatedReceiveSession, ReceiveError, ReceiveIdentity, ReceiveOwner, ReceiveResources,
};
use num_complex::Complex32;
use std::{num::NonZeroUsize, sync::Arc};
use umts_dsp::{
    DspError,
    dedicated::{
        CarrierTrackingSettings, LikelihoodSettings, UplinkControl, UplinkControlFormat,
        UplinkDpchCode, UplinkDpchReceiver, UplinkGains, UplinkLayout,
    },
    dedicated_transport::UplinkTfcs,
    tfci::Tfci,
};
use umts_types::time::{
    CHIPS_PER_FRAME, CHIPS_PER_SLOT, CONNECTION_FRAME_MODULUS, ChipInstant, SLOTS_PER_FRAME,
};

#[derive(Clone, Copy, Debug)]
pub struct TfciAcceptance {
    pub minimum_score: f64,
    pub minimum_margin: f64,
}
pub struct ChipReceiveSettings {
    pub code: Arc<UplinkDpchCode>,
    pub control: UplinkControlFormat,
    pub carrier: CarrierTrackingSettings,
    pub likelihood: LikelihoodSettings,
    pub tfci: TfciAcceptance,
    pub maximum_physical_bits: NonZeroUsize,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChipReceiveCounters {
    pub slots: u64,
    pub ignored_slots: u64,
    pub interrupted_frames: u64,
    pub complete_frames: u64,
    pub accepted_frames: u64,
    pub rejected_tfci: u64,
    pub inconsistent_tfci: u64,
}
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub enum ChipProgress {
    AwaitFrame,
    Slot(UplinkControl),
    Frame {
        control: UplinkControl,
        tfci: Option<Tfci>,
    },
}
pub struct DedicatedChipReceiver {
    session: DedicatedReceiveSession,
    receiver: UplinkDpchReceiver,
    likelihood: LikelihoodSettings,
    acceptance: TfciAcceptance,
    gains: Vec<(Tfci, UplinkGains)>,
    soft: Vec<Vec<f64>>,
    tti: Option<Box<super::tti::TtiReceiver>>,
    next: Option<ChipInstant>,
    last: Option<ChipInstant>,
    collecting: bool,
    counters: ChipReceiveCounters,
    sample_bytes: usize,
    carrier_initial: umts_dsp::carrier::CarrierOffsetHz,
    pilot_count: usize,
    hsdpa: Option<Box<crate::hsdpa_rx::FeedbackReceiver>>,
    hsupa: Option<(ChipInstant, Box<crate::hsupa_rx::TrackedReceiver>)>,
    edch_only_control: Option<UplinkControlFormat>,
    dch_control: UplinkControlFormat,
    hsupa_end: Option<ChipInstant>,
    pub(super) coexisting_selection: Option<Box<super::PreparedDchSelection>>,
}

pub(crate) fn maximum_physical_widths(
    tfcs: &UplinkTfcs,
) -> Option<[usize; UplinkLayout::MAX_CHANNELS]> {
    let mut widths = [0; UplinkLayout::MAX_CHANNELS];
    for combination in tfcs.configured().combinations() {
        let layout = tfcs.allocation(combination.tfci)?.layout();
        for (index, width) in widths.iter_mut().enumerate().take(layout.len()) {
            let bits = layout
                .channel(index)?
                .bits_per_slot()
                .checked_mul(SLOTS_PER_FRAME as usize)?;
            *width = (*width).max(bits);
        }
    }
    Some(widths)
}

pub(crate) fn maximum_physical_bits(tfcs: &UplinkTfcs) -> Option<usize> {
    maximum_physical_widths(tfcs)?
        .into_iter()
        .try_fold(0usize, |sum, width| sum.checked_add(width))
}

impl DedicatedChipReceiver {
    pub(super) fn pilot_count(&self) -> usize {
        self.pilot_count
    }
    pub fn new(
        session: DedicatedReceiveSession,
        settings: ChipReceiveSettings,
        gains: Vec<(Tfci, UplinkGains)>,
    ) -> Result<Self, ReceiveError> {
        let pilot_count = settings.control.pilots().bits();
        let combinations = session.tfcs.configured().combinations();
        if gains.len() != combinations.len()
            || gains
                .iter()
                .zip(combinations)
                .any(|((tfci, gains), combination)| {
                    *tfci != combination.tfci
                        || gains.data() == 0.0
                            && !session
                                .tfcs
                                .allocation(*tfci)
                                .expect("configured TFCI")
                                .layout()
                                .is_empty()
                })
            || settings.control.tfci_bits() == 0 && combinations.len() != 1
            || !settings.tfci.minimum_score.is_finite()
            || !(0.0..=1.0).contains(&settings.tfci.minimum_score)
            || !settings.tfci.minimum_margin.is_finite()
            || !(0.0..=2.0).contains(&settings.tfci.minimum_margin)
        {
            return Err(ReceiveError::Configuration);
        }
        let widths = maximum_physical_widths(&session.tfcs).ok_or(ReceiveError::Capacity)?;
        let total = widths
            .into_iter()
            .try_fold(0usize, |sum, width| sum.checked_add(width))
            .ok_or(ReceiveError::Capacity)?;
        if total > settings.maximum_physical_bits.get() {
            return Err(ReceiveError::Capacity);
        }
        let receiver =
            UplinkDpchReceiver::with_tracking(settings.code, settings.control, settings.carrier)?;
        let tti = (settings.control.tfci_bits() != 0)
            .then(|| super::tti::TtiReceiver::new(&session.tfcs, &gains))
            .flatten()
            .map(Box::new);
        let sample_bytes = total
            .checked_mul(std::mem::size_of::<f64>())
            .and_then(|bytes| bytes.checked_add(receiver.sample_storage_bytes()))
            .and_then(|bytes| bytes.checked_add(tti.as_ref().map_or(0, |tti| tti.storage_bytes())))
            .ok_or(ReceiveError::Capacity)?;
        Ok(Self {
            session,
            receiver,
            likelihood: settings.likelihood,
            acceptance: settings.tfci,
            gains,
            tti,
            soft: widths
                .into_iter()
                .take_while(|width| *width != 0)
                .map(|width| vec![0.0; width])
                .collect(),
            next: None,
            last: None,
            collecting: false,
            counters: ChipReceiveCounters::default(),
            sample_bytes,
            carrier_initial: settings.carrier.initial,
            pilot_count,
            hsdpa: None,
            hsupa: None,
            edch_only_control: None,
            dch_control: settings.control,
            hsupa_end: None,
            coexisting_selection: None,
        })
    }

    /// Configure before RX resource admission so the symbol history is included in its budget.
    pub fn configure_hsdpa_feedback(
        &mut self,
        capacity: std::num::NonZeroUsize,
        budget: std::num::NonZeroUsize,
    ) -> Result<crate::hsdpa_rx::FeedbackPort, ReceiveError> {
        if self.hsdpa.is_some() || self.last.is_some() {
            return Err(ReceiveError::Configuration);
        }
        let maximum = self
            .session
            .tfcs
            .configured()
            .combinations()
            .iter()
            .try_fold(0, |maximum, combination| {
                self.session
                    .tfcs
                    .allocation(combination.tfci)
                    .map(|allocation| maximum.max(allocation.layout().len()))
            })
            .ok_or(ReceiveError::Configuration)?;
        let maximum = u8::try_from(maximum).map_err(|_| ReceiveError::Configuration)?;
        let delivery_bytes = crate::hsdpa_rx::FeedbackReceiver::delivery_bytes(capacity)?;
        let bytes = self
            .sample_bytes
            .checked_add(std::mem::size_of::<crate::hsdpa_rx::FeedbackReceiver>())
            .and_then(|bytes| bytes.checked_add(delivery_bytes))
            .ok_or(ReceiveError::Capacity)?;
        let mut receiver = crate::hsdpa_rx::FeedbackReceiver::new(
            self.session.identity.ue,
            self.receiver.code().clone(),
            maximum,
            self.session.anchor.time,
        )?;
        let port = receiver.connect(capacity, budget)?;
        self.hsdpa = Some(Box::new(receiver));
        self.sample_bytes = bytes;
        Ok(port)
    }

    pub(super) fn service_hsdpa_feedback(&mut self) {
        if let Some(receiver) = &mut self.hsdpa {
            receiver.service_requests();
        }
    }

    pub(super) fn discard_hsdpa_before(
        &mut self,
        boundary: ChipInstant,
    ) -> Result<(), ReceiveError> {
        if let Some(receiver) = &mut self.hsdpa
            && receiver
                .processed_until()
                .is_none_or(|through| through.index < boundary.index)
        {
            receiver.discard_before(boundary)?;
        }
        Ok(())
    }

    pub fn hsdpa_feedback(&self) -> Option<&crate::hsdpa_rx::FeedbackReceiver> {
        self.hsdpa.as_deref()
    }

    pub(super) fn shared_uplink_code(&self) -> Arc<UplinkDpchCode> {
        Arc::clone(self.receiver.code())
    }

    pub(super) fn attach_hsupa_inner(
        &mut self,
        receiver: Box<crate::hsupa_rx::TrackedReceiver>,
        activation: ChipInstant,
        maximum_bytes: NonZeroUsize,
        edch_only: bool,
    ) -> Result<(), (ReceiveError, Box<crate::hsupa_rx::TrackedReceiver>)> {
        let admission = (|| {
            if self.hsupa.is_some()
                || receiver.is_started()
                || !receiver.has_output()
                || receiver.identity() != self.session.identity()
                || receiver.anchor().time != self.session.anchor.time
                || receiver.anchor().cfn != self.session.anchor.cfn
                || !receiver.uses_code(self.receiver.code())
            {
                return Err(ReceiveError::Configuration);
            }
            if activation.epoch != self.session.identity().ue.epoch
                || self.last.is_some_and(|last| last.index >= activation.index)
                || activation
                    .index
                    .checked_sub(self.session.anchor.time.index)
                    .is_none_or(|delta| !delta.is_multiple_of(CHIPS_PER_FRAME))
            {
                return Err(ReceiveError::Time);
            }
            let maximum = self
                .session
                .tfcs
                .configured()
                .combinations()
                .iter()
                .try_fold(0usize, |maximum, combination| {
                    self.session
                        .tfcs
                        .allocation(combination.tfci)
                        .map(|allocation| maximum.max(allocation.layout().len()))
                })
                .ok_or(ReceiveError::Configuration)?;
            if if edch_only {
                receiver.maximum_dpdch() != 0
            } else {
                maximum != usize::from(receiver.maximum_dpdch())
            } {
                return Err(ReceiveError::Configuration);
            }
            let bytes = receiver.retained_bytes().map_err(ReceiveError::Hsupa)?;
            if bytes > maximum_bytes.get() {
                return Err(ReceiveError::Capacity);
            }
            self.sample_bytes
                .checked_add(bytes)
                .ok_or(ReceiveError::Capacity)
        })();
        let bytes = match admission {
            Ok(bytes) => bytes,
            Err(error) => return Err((error, receiver)),
        };
        self.hsupa = Some((activation, receiver));
        self.sample_bytes = bytes;
        Ok(())
    }

    pub(super) fn activate_edch_only_control(
        &mut self,
        time: ChipInstant,
        control: &mut super::PreparedControlSearch,
    ) -> Result<(), ReceiveError> {
        let Some((activation, receiver)) = &self.hsupa else {
            return Err(ReceiveError::Configuration);
        };
        if self.edch_only_control.is_some()
            || receiver.maximum_dpdch() != 0
            || time.epoch != activation.epoch
            || time.index < activation.index
            || !((time.index - activation.index).is_multiple_of(CHIPS_PER_FRAME))
            || self.last.is_some_and(|last| last.index >= time.index)
            || !control.is_edch_only()
            || self.hsdpa.is_some() != control.feedback.is_some()
        {
            return Err(ReceiveError::Configuration);
        }
        if let (Some(receiver), Some(replacement)) = (&mut self.hsdpa, &mut control.feedback) {
            receiver.replace_channel_at(replacement, time)?;
        }
        self.edch_only_control = Some(control.format());
        self.pilot_count = control.format().pilots().bits();
        Ok(())
    }

    pub fn hsupa_pending(&self) -> bool {
        self.hsupa
            .as_ref()
            .is_some_and(|(_, receiver)| receiver.pending() || receiver.outstanding_reports() != 0)
    }

    pub(super) fn observe_sample_progress(
        &self,
        counters: super::samples::SampleReceiveCounters,
        search: Option<ChipInstant>,
    ) {
        if let Some((_, receiver)) = &self.hsupa {
            receiver.observe_samples(counters, search);
        }
    }

    pub(super) fn restored_dch_at(&self) -> Option<ChipInstant> {
        self.hsupa_end
    }

    pub(super) fn detachable_hsupa_bytes(
        &self,
        restored: ChipInstant,
    ) -> Result<usize, ReceiveError> {
        if self.hsupa_end != Some(restored)
            || self.edch_only_control.is_some()
            || self.hsupa_pending()
        {
            return Err(ReceiveError::Time);
        }
        self.hsupa
            .as_ref()
            .ok_or(ReceiveError::Configuration)?
            .1
            .retained_bytes()
            .map_err(ReceiveError::Hsupa)
    }

    pub(super) fn detach_restored_hsupa(
        &mut self,
        restored: ChipInstant,
    ) -> Result<Box<crate::hsupa_rx::TrackedReceiver>, ReceiveError> {
        let bytes = self.detachable_hsupa_bytes(restored)?;
        let (_, receiver) = self.hsupa.take().expect("validated restored attachment");
        self.sample_bytes -= bytes;
        self.hsupa_end = None;
        Ok(receiver)
    }

    pub(super) fn validate_selection_boundary(
        &self,
        time: ChipInstant,
    ) -> Result<(), ReceiveError> {
        let anchor = self.session.anchor;
        let delta = time
            .index
            .checked_sub(anchor.time.index)
            .ok_or(ReceiveError::Time)?;
        if time.epoch != anchor.time.epoch || !delta.is_multiple_of(CHIPS_PER_FRAME) {
            return Err(ReceiveError::Time);
        }
        let cfn = anchor.cfn.wrapping_add((delta / CHIPS_PER_FRAME) as u8);
        let configured = self.session.tfcs.configured();
        if configured.transport_ids().any(|id| {
            !usize::from(cfn).is_multiple_of(
                configured
                    .definition(id)
                    .expect("configured transport")
                    .interval
                    .frames(),
            )
        }) {
            return Err(ReceiveError::Time);
        }
        Ok(())
    }

    pub(super) fn validate_coexistence_restoration(
        &self,
        time: ChipInstant,
    ) -> Result<(), ReceiveError> {
        if self.coexisting_selection.is_some() {
            self.validate_selection_boundary(time)?;
        }
        let Some((activation, receiver)) = &self.hsupa else {
            return Err(ReceiveError::Configuration);
        };
        if receiver.maximum_dpdch() == 0
            || self.edch_only_control.is_some()
            || self.hsupa_end.is_some()
            || time.epoch != activation.epoch
            || time.index <= activation.index
            || !(time.index - activation.index).is_multiple_of(CHIPS_PER_FRAME)
            || self.last.is_some_and(|last| last.index >= time.index)
        {
            return Err(ReceiveError::Time);
        }
        Ok(())
    }

    pub(super) fn restore_coexisting_dch(&mut self, time: ChipInstant) -> Result<(), ReceiveError> {
        self.validate_coexistence_restoration(time)?;
        self.hsupa_end = Some(time);
        self.discard_incomplete_hsupa();
        Ok(())
    }

    pub(super) fn restore_dch_control(
        &mut self,
        time: ChipInstant,
        retained: &mut super::PreparedControlSearch,
    ) -> Result<(), ReceiveError> {
        let Some((activation, _)) = &self.hsupa else {
            return Err(ReceiveError::Configuration);
        };
        if self.edch_only_control.is_none()
            || self.hsupa_end.is_some()
            || time.epoch != activation.epoch
            || time.index <= activation.index
            || !(time.index - activation.index).is_multiple_of(CHIPS_PER_FRAME)
            || self.last.is_some_and(|last| last.index >= time.index)
            || self.hsdpa.is_some() != retained.feedback.is_some()
        {
            return Err(ReceiveError::Time);
        }
        if let (Some(receiver), Some(replacement)) = (&mut self.hsdpa, &mut retained.feedback) {
            receiver.replace_channel_at(replacement, time)?;
        }
        self.hsupa_end = Some(time);
        self.edch_only_control = None;
        self.pilot_count = self.dch_control.pilots().bits();
        self.discard_incomplete_hsupa();
        Ok(())
    }

    pub(super) fn hsupa_attachment(&self) -> Option<(ReceiveIdentity, ChipInstant)> {
        self.hsupa
            .as_ref()
            .map(|(activation, receiver)| (receiver.identity(), *activation))
    }

    pub(super) fn cancellable_hsupa_bytes(
        &self,
        activation: ChipInstant,
    ) -> Result<usize, ReceiveError> {
        let (start, receiver) = self.hsupa.as_ref().ok_or(ReceiveError::Configuration)?;
        if *start != activation
            || receiver.is_started()
            || self.last.is_some_and(|last| last.index >= activation.index)
        {
            return Err(ReceiveError::Time);
        }
        receiver.retained_bytes().map_err(ReceiveError::Hsupa)
    }

    pub fn cancel_hsupa(
        &mut self,
        activation: ChipInstant,
    ) -> Result<Box<crate::hsupa_rx::TrackedReceiver>, ReceiveError> {
        let bytes = self.cancellable_hsupa_bytes(activation)?;
        let (_, receiver) = self
            .hsupa
            .take()
            .expect("validated pending HSUPA attachment");
        self.sample_bytes -= bytes;
        Ok(receiver)
    }
    pub fn hsupa_dropped_intervals(&self) -> u64 {
        self.hsupa
            .as_ref()
            .map_or(0, |(_, receiver)| receiver.dropped_intervals())
    }

    pub(super) fn hsupa_decode_work_per_slot(&self) -> usize {
        self.hsupa
            .as_ref()
            .map_or(0, |(_, receiver)| receiver.decode_work_per_slot())
    }

    pub(super) fn observe_hsupa_collection_clock(&mut self, slot: ChipInstant, upper: ChipInstant) {
        if let Some((_, receiver)) = &mut self.hsupa {
            receiver.observe_collection_clock(slot, upper);
        }
    }

    pub(super) fn discard_incomplete_hsupa(&mut self) {
        if let Some((_, receiver)) = &mut self.hsupa {
            receiver.discard_incomplete();
        }
    }

    pub(super) fn service_hsupa(
        &mut self,
        work: &mut umts_dsp::coding::DecodeWork,
        mut observe: impl FnMut(super::UplinkDecodeObservation),
    ) -> Result<usize, ReceiveError> {
        let Some((_, receiver)) = &mut self.hsupa else {
            return Ok(0);
        };
        match receiver
            .service_worker_with_observer(work, |decoded, presence_log_likelihood| {
                observe(super::UplinkDecodeObservation::Edch {
                    identity: decoded.identity,
                    start: decoded.start,
                    control: decoded.control,
                    presence_log_likelihood,
                    feedback: decoded.received.feedback,
                    outcome: decoded.received.outcome,
                });
            })
            .map_err(ReceiveError::Hsupa)?
        {
            crate::hsupa_rx::Publication::Published => Ok(1),
            crate::hsupa_rx::Publication::Closed => Err(ReceiveError::HsupaOutputClosed),
            _ => Ok(0),
        }
    }
    pub fn counters(&self) -> ChipReceiveCounters {
        self.counters
    }
    pub(super) fn timing_plan(
        &self,
        rate: usize,
        delays: usize,
    ) -> Result<Arc<umts_dsp::dedicated::DedicatedTimingPlan>, ReceiveError> {
        Ok(Arc::new(umts_dsp::dedicated::DedicatedTimingPlan::new(
            self.receiver.code(),
            self.receiver.format(),
            0,
            rate,
            delays,
        )?))
    }
    pub(super) fn carrier_initial(&self) -> umts_dsp::carrier::CarrierOffsetHz {
        self.carrier_initial
    }

    pub(super) fn restore_confirmed_carrier(
        &mut self,
        offset: umts_dsp::carrier::CarrierOffsetHz,
    ) -> Result<(), ReceiveError> {
        self.receiver.restore_confirmed_carrier(offset)?;
        Ok(())
    }

    /// Samples must already be matched-filtered and aligned to this UE's local uplink chip/slot timing.
    pub fn receive_slot(
        &mut self,
        identity: ReceiveIdentity,
        time: ChipInstant,
        input: &[Complex32],
    ) -> Result<ChipProgress, ReceiveError> {
        if identity != self.session.identity {
            return Err(ReceiveError::Identity);
        }
        if self.session.failed {
            return Err(ReceiveError::Failed);
        }
        if self.session.retired {
            return Err(ReceiveError::Retired);
        }
        let anchor = self.session.anchor;
        let delta = time
            .index
            .checked_sub(anchor.time.index)
            .ok_or(ReceiveError::Time)?;
        if time.epoch != anchor.time.epoch
            || !delta.is_multiple_of(CHIPS_PER_SLOT)
            || self.last.is_some_and(|last| time.index <= last.index)
        {
            return Err(ReceiveError::Time);
        }
        let next = time
            .index
            .checked_add(CHIPS_PER_SLOT)
            .ok_or(ReceiveError::Time)?;
        if input.len() != CHIPS_PER_SLOT as usize {
            return Err(ReceiveError::Dsp(DspError::InvalidLength));
        }
        if input
            .iter()
            .any(|value| !value.re.is_finite() || !value.im.is_finite())
        {
            return Err(ReceiveError::Dsp(DspError::NonFiniteEvidence));
        }
        let slot = delta % CHIPS_PER_FRAME / CHIPS_PER_SLOT;
        if self.next.is_some_and(|expected| expected != time) && self.collecting {
            self.collecting = false;
            self.counters.interrupted_frames = self.counters.interrupted_frames.saturating_add(1);
        }
        if slot == 0 {
            self.receiver.begin_frame_with_format(
                time,
                self.edch_only_control.unwrap_or(self.dch_control),
            )?;
            self.collecting = true;
        } else if !self.collecting
            && self.edch_only_control.is_some()
            && self.hsupa.as_ref().is_some_and(|(activation, _)| {
                time.index >= activation.index
                    && self.hsupa_end.is_none_or(|end| time.index < end.index)
            })
        {
            let frame = ChipInstant {
                index: time.index - slot * CHIPS_PER_SLOT,
                ..time
            };
            // Only a first observation in this frame may start a suffix. A gap
            // in an already observed frame still waits for the next full frame.
            if self.last.is_none_or(|last| last.index < frame.index) {
                self.receiver.begin_frame_suffix(
                    frame,
                    self.edch_only_control.expect("checked E-DCH-only control"),
                    slot as u8,
                )?;
                self.collecting = true;
            }
        }
        self.last = Some(time);
        self.next = Some(ChipInstant {
            epoch: time.epoch,
            index: next,
        });
        if !self.collecting {
            self.counters.ignored_slots = self.counters.ignored_slots.saturating_add(1);
            return Ok(ChipProgress::AwaitFrame);
        }
        let control = match self.receiver.receive(time, input) {
            Ok(control) => control,
            Err(error) => {
                self.session.failed = true;
                return Err(error.into());
            }
        };
        if let Some(hsdpa) = &mut self.hsdpa
            && let Err(error) = hsdpa.feed(&self.receiver.last_slot().expect("received slot"))
        {
            self.session.failed = true;
            return Err(error.into());
        }
        self.counters.slots = self.counters.slots.saturating_add(1);
        if let Some((activation, hsupa)) = &mut self.hsupa
            && time.index >= activation.index
            && self.hsupa_end.is_none_or(|end| time.index < end.index)
        {
            hsupa
                .feed_worker(&self.receiver.last_slot().expect("received slot"))
                .map_err(ReceiveError::Hsupa)?;
        }
        if slot + 1 != SLOTS_PER_FRAME {
            return Ok(ChipProgress::Slot(control));
        }
        self.collecting = false;
        if self.receiver.frame().is_none() {
            // Complete E-DCH TTIs have already reached their normal owner. This
            // suffix is not a complete DCH/TFCI frame or frame-completion event.
            return Ok(ChipProgress::Slot(control));
        }
        self.counters.complete_frames = self.counters.complete_frames.saturating_add(1);
        let result = if self.edch_only_control.is_some() {
            Ok(None)
        } else {
            self.finish_frame()
        };
        match result {
            Ok(tfci) => Ok(ChipProgress::Frame { control, tfci }),
            Err(error) => {
                self.session.failed = true;
                Err(error)
            }
        }
    }
    fn finish_frame(&mut self) -> Result<Option<Tfci>, ReceiveError> {
        let frame = self.receiver.frame().ok_or(ReceiveError::Configuration)?;
        let selection = self.coexisting_selection.as_deref().filter(|_| {
            self.hsupa
                .as_ref()
                .is_some_and(|(at, _)| frame.start().index >= at.index)
                && self
                    .hsupa_end
                    .is_none_or(|end| frame.start().index < end.index)
        });
        if let Some(tti) = &mut self.tti {
            return tti.feed(
                &frame,
                &mut self.session,
                self.likelihood,
                self.acceptance,
                &mut self.counters,
                selection,
            );
        }
        let chosen = if self.receiver.format().tfci_bits() == 0 {
            Some(self.gains[0].0)
        } else {
            frame
                .decode_tfci_reliable(
                    selection.map_or(self.session.tfcs.decoder(), |value| &value.decoder),
                    self.likelihood,
                )?
                .filter(|decision| {
                    decision.normalized_score >= self.acceptance.minimum_score
                        && decision
                            .normalized_margin
                            .is_none_or(|margin| margin >= self.acceptance.minimum_margin)
                })
                .map(|decision| decision.tfci)
        };
        let Some(tfci) = chosen else {
            self.counters.rejected_tfci = self.counters.rejected_tfci.saturating_add(1);
            return Ok(None);
        };
        let wire_tfci = tfci;
        let tfci = selection.map_or(Ok(tfci), |selection| selection.source(tfci))?;
        let gains = self
            .gains
            .binary_search_by_key(&tfci.index(), |(tfci, _)| tfci.index())
            .map(|index| self.gains[index].1)
            .map_err(|_| ReceiveError::Configuration)?;
        let allocation = self
            .session
            .tfcs
            .allocation(tfci)
            .ok_or(ReceiveError::Configuration)?;
        let layout = allocation.layout();
        let mut physical: [&[f64]; UplinkLayout::MAX_CHANNELS] = [&[]; UplinkLayout::MAX_CHANNELS];
        for (index, (output, reference)) in self
            .soft
            .iter_mut()
            .zip(&mut physical)
            .enumerate()
            .take(layout.len())
        {
            let bits = layout
                .channel(index)
                .expect("layout channel")
                .bits_per_slot()
                * SLOTS_PER_FRAME as usize;
            frame.despread_llrs(layout, index, gains, self.likelihood, &mut output[..bits])?;
            *reference = &output[..bits];
        }
        let start = frame.start();
        let number = (start.index - self.session.anchor.time.index) / CHIPS_PER_FRAME;
        let cfn = self
            .session
            .anchor
            .cfn
            .wrapping_add((number % CONNECTION_FRAME_MODULUS) as u8);
        match self.session.receive(
            self.session.identity(),
            start,
            cfn,
            tfci,
            &physical[..layout.len()],
        ) {
            Ok(()) => {}
            Err(ReceiveError::FormatChangedWithinTti) => {
                // TS 25.212 4.2 segments one transport block set across its TTI.
                // Contradictory air-decoded formats cannot contribute to that block set.
                self.session.discard_incomplete_ttis();
                self.counters.inconsistent_tfci = self.counters.inconsistent_tfci.saturating_add(1);
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        self.counters.accepted_frames = self.counters.accepted_frames.saturating_add(1);
        Ok(Some(wire_tfci))
    }
}
impl ReceiveOwner for DedicatedChipReceiver {
    fn last_consumed_frame(&self) -> Option<ChipInstant> {
        self.last.map(|time| ChipInstant {
            epoch: time.epoch,
            index: time.index - (time.index - self.session.anchor.time.index) % CHIPS_PER_FRAME,
        })
    }
    fn session(&self) -> &DedicatedReceiveSession {
        &self.session
    }
    fn session_mut(&mut self) -> &mut DedicatedReceiveSession {
        &mut self.session
    }
    fn resources(&self) -> ReceiveResources {
        ReceiveResources {
            physical_sample_bytes: self.sample_bytes,
            ..self.session.resources()
        }
    }
}
