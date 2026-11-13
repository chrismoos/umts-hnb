// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_rx::{
    Evidence, FeedbackPort, Measurement, Observation, Report, Request, RequestId,
};
use umts_dsp::{DspError, hsdpa::HarqAck};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackError {
    Flight(FlightError),
    Capacity,
    Request,
    Watermark,
    Decode(DspError),
}

impl Flight {
    pub fn request_feedback(
        &mut self,
        port: &mut FeedbackPort,
    ) -> Result<RequestId, FeedbackError> {
        self.check_receive(self.transmission.key)
            .map_err(FeedbackError::Flight)?;
        if self.feedback_request.is_some() || matches!(self.transmit, Transmit::Lost) {
            return Err(FeedbackError::Request);
        }
        let id = port
            .request(Request::Ack {
                transmission: self.transmission,
                repetitions: self.feedback_repetitions.get(),
            })
            .map_err(|_| FeedbackError::Capacity)?;
        self.feedback_request = Some(id);
        Ok(id)
    }

    /// The policy must reject ambiguous or insufficient PHY evidence rather than accepting its winning word.
    pub fn admit_feedback_report(
        &mut self,
        report: &Report,
        classify: impl FnOnce(&Observation<HarqAck>) -> Feedback,
    ) -> Result<(), FeedbackError> {
        let Request::Ack {
            transmission,
            repetitions,
        } = report.request
        else {
            return Err(FeedbackError::Request);
        };
        if self.feedback_request != Some(report.id)
            || transmission != self.transmission
            || repetitions != self.feedback_repetitions.get()
        {
            return Err(FeedbackError::Request);
        }
        self.check_receive(transmission.key)
            .map_err(FeedbackError::Flight)?;
        let result = report.result.map_err(FeedbackError::Decode)?;
        match result {
            Measurement::Ack(Evidence::Measured(observation)) => self.record_feedback(
                transmission.key,
                transmission.expected_ack,
                classify(&observation),
            ),
            Measurement::Ack(Evidence::Missing) => self.record_missing(
                transmission.key,
                report.processed_until.ok_or(FeedbackError::Watermark)?,
            ),
            Measurement::Cqi(_) | Measurement::AckCells(_) | Measurement::CqiCells(_) => {
                return Err(FeedbackError::Request);
            }
        }
        .map_err(FeedbackError::Flight)
    }
}

impl Flight {
    pub fn request_feedback_cells(
        &mut self,
        port: &mut FeedbackPort,
        schedule: crate::hsdpa_rx::FeedbackModeSchedule,
    ) -> Result<RequestId, FeedbackError> {
        self.check_receive(self.transmission.key)
            .map_err(FeedbackError::Flight)?;
        if self.feedback_request.is_some() || matches!(self.transmit, Transmit::Lost) {
            return Err(FeedbackError::Request);
        }
        let id = port
            .request(Request::AckCells {
                start: self.transmission.expected_ack,
                repetitions: self.feedback_repetitions.get(),
                schedule,
            })
            .map_err(|_| FeedbackError::Capacity)?;
        self.feedback_request = Some(id);
        Ok(id)
    }

    pub(crate) fn matches_feedback_report(&self, report: &Report) -> bool {
        self.feedback_request == Some(report.id)
    }

    pub fn admit_feedback_cells(
        &mut self,
        report: &Report,
        carrier: umts_types::hsdpa::CarrierId,
        policy: &crate::hsdpa_rx::FeedbackPolicy,
    ) -> Result<Option<Feedback>, FeedbackError> {
        let Request::AckCells {
            start, repetitions, ..
        } = report.request
        else {
            return Err(FeedbackError::Request);
        };
        if !self.matches_feedback_report(report)
            || start != self.transmission.expected_ack
            || repetitions != self.feedback_repetitions.get()
        {
            return Err(FeedbackError::Request);
        }
        let classification = match report.result.map_err(FeedbackError::Decode)? {
            Measurement::AckCells(Evidence::Measured(observation)) => {
                let feedback = policy.classify_ack_cells(&observation)[carrier.index()];
                self.record_feedback(self.transmission.key, start, feedback)
                    .map_err(FeedbackError::Flight)?;
                Some(feedback)
            }
            Measurement::AckCells(Evidence::Missing) => {
                self.record_missing(
                    self.transmission.key,
                    report.processed_until.ok_or(FeedbackError::Watermark)?,
                )
                .map_err(FeedbackError::Flight)?;
                None
            }
            _ => return Err(FeedbackError::Request),
        };
        Ok(classification)
    }
}
