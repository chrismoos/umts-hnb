// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(super) struct TransitionStep<'a, P> {
    pub now: ChipInstant,
    pub limits: AdmissionLimits,
    pub port: &'a mut DedicatedTransmitPort,
    pub radio: &'a mut CommonSignalling,
    pub receive: &'a mut DedicatedController,
    pub common: &'a CommonTxPort,
    pub policy: &'a mut P,
}

pub(super) struct FrameLane<'a> {
    pub identity: ReceiveIdentity,
    pub pool: &'a mut FramePool,
    pub frame: &'a mut Option<FrameInput>,
    pub next: &'a mut ChipInstant,
    pub idle: Tfci,
    pub signalling: bool,
}
pub(super) fn schedule_frame(
    lane: FrameLane<'_>,
    now: ChipInstant,
    horizon: u64,
    port: &mut DedicatedTransmitPort,
    radio: &mut CommonSignalling,
    receive: &mut DedicatedController,
    policy: &mut impl FramePolicy,
) -> Result<Option<TransmitCommandId>, AdmissionError> {
    if lane.frame.is_none() {
        if lane.next.index >= horizon {
            return Ok(None);
        }
        let built = receive
            .with_active_mapping(lane.identity, |bank| {
                let tfci = if lane.signalling {
                    policy.select(
                        FrameOpportunity {
                            identity: lane.identity,
                            start: *lane.next,
                            dedicated_signalling: true,
                            user_bearers_ready: bank.user_bearers_ready(),
                            idle: lane.idle,
                        },
                        now,
                        bank,
                        radio,
                    )?
                } else {
                    lane.idle
                };
                let mut draft = bank.begin_pooled_frame(lane.pool, port, *lane.next, tfci)?;
                if lane.signalling {
                    policy.fill(&mut draft, now, radio)?;
                }
                if !draft.complete() {
                    return Err(FrameBuildError::Bearer(BearerError::Transport));
                }
                Ok(draft.finish().ok().expect("complete frame"))
            })
            .map_err(AdmissionError::Receive)?;
        match built {
            Ok(input) => *lane.frame = Some(input),
            Err(FrameBuildError::Pool(crate::dedicated_bearers::FramePoolError::Capacity)) => {
                return Ok(None);
            }
            Err(error) => return Err(AdmissionError::Frame(error)),
        }
    }
    let input = lane.frame.take().expect("prepared frame");
    let Some(next) = input.start.index.checked_add(CHIPS_PER_FRAME) else {
        *lane.frame = Some(input);
        return Err(AdmissionError::Time);
    };
    if next > horizon {
        *lane.frame = Some(input);
        return Ok(None);
    }
    match port.frame(input) {
        Ok(id) => {
            lane.next.index = next;
            Ok(Some(id))
        }
        Err(input) => {
            *lane.frame = Some(input);
            Ok(None)
        }
    }
}
