// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::{DedicatedBearers, SduDelivery, TransparentTraffic};

pub trait FramePolicy {
    fn select(
        &mut self,
        opportunity: FrameOpportunity,
        now: ChipInstant,
        bank: &mut DedicatedBearers,
        radio: &mut CommonSignalling,
    ) -> Result<Tfci, FrameBuildError>;
    fn fill(
        &mut self,
        draft: &mut DownlinkFrame<'_>,
        now: ChipInstant,
        radio: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError>;
}

pub trait DuplexFramePolicy: FramePolicy {
    type UplinkError;

    fn uplink(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Self::UplinkError>;
}

pub struct TrafficFramePolicy<S> {
    source: S,
    maximum_combinations: NonZeroUsize,
}
impl<S: TransparentTraffic> TrafficFramePolicy<S> {
    pub fn new(source: S, maximum_combinations: NonZeroUsize) -> Self {
        Self {
            source,
            maximum_combinations,
        }
    }
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }
    #[cfg(test)]
    pub fn source(&self) -> &S {
        &self.source
    }
    #[cfg(test)]
    pub fn into_source(self) -> S {
        self.source
    }
}
impl<S: TransparentTraffic> FramePolicy for TrafficFramePolicy<S> {
    fn select(
        &mut self,
        opportunity: FrameOpportunity,
        now: ChipInstant,
        bank: &mut DedicatedBearers,
        radio: &mut CommonSignalling,
    ) -> Result<Tfci, FrameBuildError> {
        bank.select_traffic_on(
            crate::dedicated_bearers::TrafficFrame {
                identity: opportunity.identity,
                start: opportunity.start,
            },
            now,
            radio,
            &self.source,
            self.maximum_combinations,
        )
    }
    fn fill(
        &mut self,
        draft: &mut DownlinkFrame<'_>,
        now: ChipInstant,
        radio: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError> {
        draft.fill_traffic(now, radio, &mut self.source)
    }
}

impl DuplexFramePolicy for TrafficFramePolicy<crate::voice_bearers::VoiceTraffic> {
    type UplinkError = crate::voice_bearers::Error;

    fn uplink(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Self::UplinkError> {
        self.source.uplink(delivery)
    }
}

#[cfg(test)]
pub(crate) struct CallbackDuplexPolicy<'a, P, U> {
    pub policy: &'a mut P,
    pub upper: U,
}

#[cfg(test)]
impl<P: FramePolicy, U> FramePolicy for CallbackDuplexPolicy<'_, P, U> {
    fn select(
        &mut self,
        opportunity: FrameOpportunity,
        now: ChipInstant,
        bank: &mut DedicatedBearers,
        radio: &mut CommonSignalling,
    ) -> Result<Tfci, FrameBuildError> {
        self.policy.select(opportunity, now, bank, radio)
    }

    fn fill(
        &mut self,
        draft: &mut DownlinkFrame<'_>,
        now: ChipInstant,
        radio: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError> {
        self.policy.fill(draft, now, radio)
    }
}

#[cfg(test)]
impl<P, U> DuplexFramePolicy for CallbackDuplexPolicy<'_, P, U>
where
    P: FramePolicy,
    U: FnMut(SduDelivery<'_>) -> bool,
{
    type UplinkError = std::convert::Infallible;

    fn uplink(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Self::UplinkError> {
        Ok((self.upper)(delivery))
    }
}

#[cfg(test)]
pub(crate) struct ClosurePolicy<S, F> {
    pub select: S,
    pub fill: F,
}
#[cfg(test)]
impl<S, F> FramePolicy for ClosurePolicy<S, F>
where
    S: FnMut(FrameOpportunity) -> Tfci,
    F: FnMut(&mut DownlinkFrame<'_>, &mut CommonSignalling) -> Result<(), FrameBuildError>,
{
    fn select(
        &mut self,
        opportunity: FrameOpportunity,
        _: ChipInstant,
        _: &mut DedicatedBearers,
        _: &mut CommonSignalling,
    ) -> Result<Tfci, FrameBuildError> {
        Ok((self.select)(opportunity))
    }
    fn fill(
        &mut self,
        draft: &mut DownlinkFrame<'_>,
        _: ChipInstant,
        radio: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError> {
        (self.fill)(draft, radio)
    }
}
