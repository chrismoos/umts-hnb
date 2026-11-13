// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
const CFN_MODULUS: u64 = 256;

impl DedicatedTransmitBank {
    pub fn stage_replacement(
        &mut self,
        previous: ReceiveIdentity,
        session: Box<DedicatedTransmitSession>,
    ) -> Result<(), (TransmitError, Box<DedicatedTransmitSession>)> {
        self.insert_prepared(Some(previous), session)
    }
    pub(super) fn validate_replacement(
        &self,
        previous: ReceiveIdentity,
        next: &DedicatedTransmitSession,
    ) -> Result<(), TransmitError> {
        let old = self.session(previous).ok_or(TransmitError::Identity)?;
        if previous.ue != next.identity.ue || next.identity.configuration <= previous.configuration
        {
            return Err(TransmitError::Identity);
        }
        if !matches!(
            old.phase,
            TransmitPhase::Armed | TransmitPhase::Scheduled { .. }
        ) {
            return Err(TransmitError::Phase);
        }
        let before = old.plan.configuration().physical();
        let after = next.plan.configuration().physical();
        // TS 25.331 8.2.2 distinguishes timing adjustment and timing-reinitialized handover from a TFCS change.
        if before.frame_offset_256_chips != after.frame_offset_256_chips
            || before.default_offset_512_chips != after.default_offset_512_chips
        {
            return Err(TransmitError::TimingChange);
        }
        let span = next
            .anchor
            .time
            .index
            .checked_sub(old.anchor.time.index)
            .ok_or(TransmitError::Time)?;
        let longest = old
            .transports
            .iter()
            .chain(&next.transports)
            .map(|transport| transport.frames)
            .max()
            .ok_or(TransmitError::Mapping)?;
        // TS 25.212 4.2.14 includes removed transports in Fmax at the first changed frame.
        if span == 0
            || !span.is_multiple_of(CHIPS_PER_FRAME)
            || old
                .anchor
                .cfn
                .wrapping_add((span / CHIPS_PER_FRAME % CFN_MODULUS) as u8)
                != next.anchor.cfn
            || !usize::from(next.anchor.cfn).is_multiple_of(longest)
            || old.next_prepare.index > next.anchor.time.index
            || next.anchor.time.index < self.next.index
        {
            return Err(TransmitError::Time);
        }
        Ok(())
    }
    pub fn commit_replacement(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<FrameAnchor, TransmitError> {
        let next = self.session(identity).ok_or(TransmitError::Identity)?;
        if next.phase != (TransmitPhase::Staged { previous }) {
            return Err(TransmitError::Phase);
        }
        self.validate_replacement(previous, next)?;
        if next.queued == 0 {
            return Err(TransmitError::Underrun);
        }
        let boundary = next.anchor;
        let previous_power = self
            .session(previous)
            .expect("validated predecessor")
            .downlink_power;
        if let Some(power) = self
            .session_mut(identity)
            .expect("validated successor")
            .downlink_power
            .as_mut()
            && let Some(previous_power) = previous_power
        {
            power.inherit(previous_power);
        }
        let complete = self.next.index == boundary.time.index;
        self.session_mut(previous)
            .expect("validated predecessor")
            .phase = if complete {
            TransmitPhase::Stopped {
                next: identity,
                boundary: boundary.time,
            }
        } else {
            TransmitPhase::Draining {
                next: identity,
                boundary: boundary.time,
            }
        };
        self.session_mut(identity)
            .expect("validated successor")
            .phase = TransmitPhase::Scheduled { previous };
        Ok(boundary)
    }
    /// These coordinates prove synthesis only. Normal retirement also requires the paired submission interval.
    pub fn synthesized_retirement(
        &self,
        identity: ReceiveIdentity,
    ) -> Result<(ChipInstant, ChipInstant), TransmitError> {
        let old = self.session(identity).ok_or(TransmitError::Identity)?;
        let TransmitPhase::Stopped { boundary, .. } = old.phase else {
            return Err(TransmitError::Phase);
        };
        if self.next.index < boundary.index || old.queued != 0 {
            return Err(TransmitError::Phase);
        }
        Ok((old.anchor.time, boundary))
    }
}
