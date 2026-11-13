// SPDX-License-Identifier: AGPL-3.0-or-later

use super::live;
use num_complex::Complex32;
use std::time::Instant;
use umts_types::time::TimeError;

pub(crate) struct Recording {
    live: Option<live::Recorder>,
    origin: Instant,
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            live: None,
            origin: Instant::now(),
        }
    }
}

pub(crate) type TxRecording = Recording;
pub(crate) type RxRecording = Recording;

impl Recording {
    pub(crate) fn attach_live(&mut self, recorder: live::Recorder) {
        self.live = Some(recorder);
    }

    pub(crate) fn poll(&mut self) {
        if let Some(live) = &mut self.live {
            live.poll();
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.live.as_ref().is_some_and(live::Recorder::is_recording)
    }

    pub(crate) fn record_transmit(
        &mut self,
        first_sample: u64,
        tick: Result<u64, TimeError>,
        samples: &[Complex32],
        accepted: Option<usize>,
        failed: bool,
    ) -> Result<(), TimeError> {
        let host_us = u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX);
        if let Some(live) = &mut self.live
            && live.is_recording()
        {
            match tick {
                Ok(tick) => {
                    live.record_transmit(first_sample, tick, host_us, samples, accepted, failed)
                }
                Err(_) => live.invalidate_time(),
            }
        }
        Ok(())
    }

    pub(crate) fn record_receive(
        &mut self,
        first_sample: u64,
        first_tick: u64,
        returned_at: Instant,
        samples: &[Complex32],
        discontinuity: bool,
    ) {
        if !self.enabled() {
            return;
        }
        let host_us = u64::try_from(
            returned_at
                .saturating_duration_since(self.origin)
                .as_micros(),
        )
        .unwrap_or(u64::MAX);
        if let Some(live) = &mut self.live
            && live.is_recording()
        {
            live.record_receive(first_sample, first_tick, host_us, samples, discontinuity);
        }
    }

    pub(crate) fn finish(&mut self) {
        if let Some(live) = &mut self.live {
            live.stop();
        }
    }
}
