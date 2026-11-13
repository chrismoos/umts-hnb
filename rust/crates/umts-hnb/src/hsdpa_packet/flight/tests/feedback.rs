// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_rx::{
    Evidence, FeedbackPort, FeedbackReceiver, Measurement, Observation, Report, Request,
};
use std::sync::Arc;
use umts_dsp::{
    dedicated::UplinkDpchCode,
    hsdpa::{FeedbackEvidence, HarqAck},
    uplink_scrambling::UplinkScramblingCode,
};

fn receiver(ue: UeKey) -> (FeedbackReceiver, FeedbackPort) {
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0).unwrap()).unwrap());
    let mut receiver = FeedbackReceiver::new(ue, code, 1, at(0)).unwrap();
    let port = receiver.connect(count(2), count(1)).unwrap();
    (receiver, port)
}

#[test]
fn lost_transmission_waits_for_requested_feedback_without_accepting_false_delivery() {
    let (mut harq, mut flight, transmission) = fixture(1);
    let (_receiver, mut port) = receiver(transmission.key.ue());
    let id = flight.request_feedback(&mut port).unwrap();
    flight.record_transmit(transmission.key, false).unwrap();
    assert_eq!(flight.resolve(&mut harq).unwrap(), None);
    let report = Report {
        id,
        request: Request::Ack {
            transmission,
            repetitions: 1,
        },
        result: Ok(Measurement::Ack(Evidence::Missing)),
        processed_until: Some(at(50_000)),
    };
    flight
        .admit_feedback_report(&report, |_| panic!("missing evidence is not classified"))
        .unwrap();
    assert_eq!(
        flight.resolve(&mut harq).unwrap(),
        Some(Resolution::NotTransmitted)
    );
    assert_eq!(
        harq.payload(transmission.key.process()).unwrap(),
        Some(&123)
    );
}

#[test]
fn feedback_report_requires_owned_request_exact_window_and_explicit_phy_policy() {
    let (mut harq, mut flight, transmission) = fixture(2);
    let (_receiver, mut port) = receiver(transmission.key.ue());
    let id = flight.request_feedback(&mut port).unwrap();
    assert_eq!(port.outstanding(), 1);
    assert_eq!(
        flight.request_feedback(&mut port),
        Err(FeedbackError::Request)
    );
    let mut report = Report {
        id,
        request: Request::Ack {
            transmission,
            repetitions: 1,
        },
        result: Ok(Measurement::Ack(Evidence::Measured(Observation {
            evidence: FeedbackEvidence {
                candidate: Some(HarqAck::Ack),
                normalized_correlation: 0.9,
                normalized_margin: 0.1,
                mean_soft_power: 0.01,
            },
            minimum_pilot_fraction: 0.5,
            mean_relative_pilot_disturbance: 0.5,
        }))),
        processed_until: Some(at(50_000)),
    };
    assert_eq!(
        flight.admit_feedback_report(&report, |_| panic!("wrong repetition")),
        Err(FeedbackError::Request)
    );
    report.request = Request::Ack {
        transmission,
        repetitions: 2,
    };
    flight
        .admit_feedback_report(&report, |observation| {
            assert_eq!(observation.evidence.candidate, Some(HarqAck::Ack));
            Feedback::Dtx
        })
        .unwrap();
    assert_eq!(flight.resolve(&mut harq).unwrap(), None);
    flight.record_transmit(transmission.key, true).unwrap();
    assert_eq!(flight.resolve(&mut harq).unwrap(), Some(Resolution::Retry));
    assert_eq!(
        harq.payload(transmission.key.process()).unwrap(),
        Some(&123)
    );
    assert!(
        flight
            .admit_feedback_report(&report, |_| panic!("duplicate"))
            .is_err()
    );
}

#[test]
fn missing_report_needs_receive_progress_through_all_repetitions_and_decode_error_is_retained() {
    let (mut harq, mut flight, transmission) = fixture(4);
    let (_receiver, mut port) = receiver(transmission.key.ue());
    let id = flight.request_feedback(&mut port).unwrap();
    let mut report = Report {
        id,
        request: Request::Ack {
            transmission,
            repetitions: 4,
        },
        result: Err(umts_dsp::DspError::DiscontinuousTime),
        processed_until: None,
    };
    assert_eq!(
        flight.admit_feedback_report(&report, |_| panic!("no measurement")),
        Err(FeedbackError::Decode(umts_dsp::DspError::DiscontinuousTime))
    );
    report.result = Ok(Measurement::Ack(Evidence::Missing));
    assert_eq!(
        flight.admit_feedback_report(&report, |_| panic!("no measurement")),
        Err(FeedbackError::Watermark)
    );
    report.processed_until = Some(at(flight.feedback_end.index - 1));
    assert_eq!(
        flight.admit_feedback_report(&report, |_| panic!("no measurement")),
        Err(FeedbackError::Flight(FlightError::Time))
    );
    report.processed_until = Some(flight.feedback_end);
    flight
        .admit_feedback_report(&report, |_| panic!("no measurement"))
        .unwrap();
    flight.record_transmit(transmission.key, true).unwrap();
    assert_eq!(flight.resolve(&mut harq).unwrap(), Some(Resolution::Retry));
}
