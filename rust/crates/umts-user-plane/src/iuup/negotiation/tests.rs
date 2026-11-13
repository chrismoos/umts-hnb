// SPDX-License-Identifier: AGPL-3.0-or-later

use super::super::initialization::{Combination, DataPduType, Rfci};
use super::*;

fn settings(repetitions: u8) -> Settings {
    Settings::new(
        Duration::from_millis(100),
        repetitions,
        Versions::new(3).unwrap(),
    )
    .unwrap()
}

fn frame(rfci: u8, chained: bool) -> Initialization {
    Initialization::new(
        &[Combination::new(Rfci::new(rfci).unwrap(), &[81, 103, 60], None).unwrap()],
        Versions::new(3).unwrap(),
        DataPduType::Crc,
        chained,
    )
    .unwrap()
}

fn request<'a>(frame: &Initialization, number: u8, mode: u8, output: &'a mut [u8]) -> Frame<'a> {
    let bytes = frame.encode(output).unwrap();
    Frame::Control {
        number,
        acknowledgment: Acknowledgment::Procedure,
        mode,
        procedure: INITIALIZATION,
        payload_crc: PayloadCheck::Valid,
        payload: &output[..bytes],
    }
}

fn ack(number: u8) -> Frame<'static> {
    Frame::Control {
        number,
        acknowledgment: Acknowledgment::Positive,
        mode: 1,
        procedure: INITIALIZATION,
        payload_crc: PayloadCheck::Absent,
        payload: &[],
    }
}

fn send(sender: &mut Sender, at: Instant) -> Vec<u8> {
    let mut result = Vec::new();
    assert!(
        sender
            .publish(at, &mut [0; 256], |bytes| {
                result.extend_from_slice(bytes);
                Ok::<_, ()>(())
            })
            .unwrap()
    );
    result
}

fn reply(receiver: &mut Receiver) -> Vec<u8> {
    let mut result = Vec::new();
    assert!(
        receiver
            .publish(&mut [0; 256], |bytes| {
                result.extend_from_slice(bytes);
                Ok::<_, ()>(())
            })
            .unwrap()
    );
    result
}

#[test]
fn seeded_receiver_preserves_original_sender_session_across_rejected_or_blocked_reinitialization() {
    let now = Instant::now();
    let mut sender = Sender::new(
        &[frame(0, false)],
        ModeVersion::new(1).unwrap(),
        settings(3),
    )
    .unwrap();
    send(&mut sender, now);
    assert_eq!(sender.acknowledgment(ack(0), now), Ok(AckEffect::Complete));
    let original = sender.negotiated().unwrap().clone();
    assert!(matches!(
        Receiver::from_negotiated(Versions::new(1).unwrap(), original.clone()),
        Err(Error::Configuration)
    ));
    let mut receiver =
        Receiver::from_negotiated(Versions::new(3).unwrap(), original.clone()).unwrap();
    assert!(receiver.negotiated().unwrap().same_session(&original));
    assert_eq!(receiver.request(ack(0)), Err(Error::NotRequest));
    assert!(receiver.negotiated().unwrap().same_session(&original));
    let mut bytes = [0; 128];
    receiver
        .request_with(request(&frame(7, false), 0, 0, &mut bytes), |_| {
            Err(Cause::Rfci)
        })
        .unwrap();
    assert!(!receiver.pending_accepted());
    assert!(receiver.negotiated().is_none());
    assert!(receiver.publish(&mut [0; 128], |_| Err("blocked")).is_err());
    reply(&mut receiver);
    assert!(receiver.negotiated().unwrap().same_session(&original));
    receiver
        .request(request(&frame(7, false), 0, 0, &mut bytes))
        .unwrap();
    let candidate = receiver.pending_negotiated().unwrap().clone();
    assert!(!candidate.same_session(&original));
    assert!(receiver.publish(&mut [0; 128], |_| Err("blocked")).is_err());
    assert!(receiver.negotiated().is_none());
    reply(&mut receiver);
    assert!(receiver.negotiated().unwrap().same_session(&candidate));
}

#[test]
fn seeded_receiver_chained_duplicates_preserve_the_same_committed_format_identity() {
    let now = Instant::now();
    let mut sender = Sender::new(
        &[frame(0, false)],
        ModeVersion::new(1).unwrap(),
        settings(3),
    )
    .unwrap();
    send(&mut sender, now);
    sender.acknowledgment(ack(0), now).unwrap();
    let mut receiver = Receiver::from_negotiated(
        Versions::new(3).unwrap(),
        sender.negotiated().unwrap().clone(),
    )
    .unwrap();
    let mut bytes = [0; 128];
    receiver
        .request(request(&frame(7, true), 0, 0, &mut bytes))
        .unwrap();
    reply(&mut receiver);
    assert!(receiver.negotiated().is_none());
    receiver
        .request(request(&frame(7, true), 0, 0, &mut bytes))
        .unwrap();
    reply(&mut receiver);
    receiver
        .request(request(&frame(8, false), 1, 0, &mut bytes))
        .unwrap();
    reply(&mut receiver);
    let committed = receiver.negotiated().unwrap().clone();
    receiver
        .request(request(&frame(8, false), 1, 0, &mut bytes))
        .unwrap();
    reply(&mut receiver);
    assert!(receiver.negotiated().unwrap().same_session(&committed));
    assert!(
        Receiver::storage_ceiling()
            >= std::mem::size_of::<Receiver>()
                + MAX_CHAIN_FRAMES * std::mem::size_of::<Initialization>()
                + 2 * MAX_RFCIS * std::mem::size_of::<Combination>()
    );
}

#[test]
fn no_timer_or_ack_credit_before_successful_transport_publication() {
    let now = Instant::now();
    let mut sender = Sender::new(
        &[frame(0, false)],
        ModeVersion::new(1).unwrap(),
        settings(3),
    )
    .unwrap();
    assert_eq!(sender.acknowledgment(ack(0), now), Ok(AckEffect::Ignored));
    assert!(matches!(
        sender.publish(now, &mut [0; 256], |_| Err("blocked")),
        Err(PublishError::Send("blocked"))
    ));
    assert_eq!(sender.deadline(), None);
    assert!(matches!(
        sender.publish(now, &mut [], |_| Ok::<_, ()>(())),
        Err(PublishError::Wire(WireError::Capacity))
    ));
    assert_eq!(sender.deadline(), None);
    let later = now + Duration::from_secs(10);
    send(&mut sender, later);
    assert_eq!(sender.deadline(), Some(later + Duration::from_millis(100)));
    assert!(
        !sender
            .publish(later, &mut [0; 256], |_| -> Result<(), ()> {
                panic!("already waiting")
            })
            .unwrap()
    );
    assert_eq!(
        sender.acknowledgment(ack(0), later),
        Ok(AckEffect::Complete)
    );
    assert_eq!(sender.negotiated().unwrap().mode().number(), 2);
}

#[test]
fn timeout_repeats_identical_frame_only_up_to_the_operator_limit() {
    let now = Instant::now();
    let mut sender = Sender::new(
        &[frame(0, false)],
        ModeVersion::new(1).unwrap(),
        settings(2),
    )
    .unwrap();
    let first = send(&mut sender, now);
    for iteration in 1..=2 {
        let deadline = now + Duration::from_millis(iteration * 100);
        sender.tick(deadline - Duration::from_nanos(1)).unwrap();
        assert!(
            !sender
                .publish(
                    deadline - Duration::from_nanos(1),
                    &mut [0; 256],
                    |_| -> Result<(), ()> { panic!("early retry") }
                )
                .unwrap()
        );
        assert_eq!(send(&mut sender, deadline), first);
    }
    sender.tick(now + Duration::from_millis(300)).unwrap();
    assert_eq!(
        sender.failure(),
        Some(Failure {
            frame: 0,
            reason: RetryReason::Timeout
        })
    );
    assert!(
        !sender
            .publish(
                now + Duration::from_secs(10),
                &mut [0; 256],
                |_| -> Result<(), ()> { panic!("exhausted") }
            )
            .unwrap()
    );
    assert!(sender.negotiated().is_none());
}

#[test]
fn four_frame_exchange_waits_for_each_ack_and_preserves_receiver_replay() {
    let now = Instant::now();
    let frames = [
        frame(0, true),
        frame(1, true),
        frame(2, true),
        frame(3, false),
    ];
    let mut sender = Sender::new(&frames, ModeVersion::new(1).unwrap(), settings(1)).unwrap();
    let mut receiver = Receiver::new(Versions::new(3).unwrap()).unwrap();
    for index in 0..MAX_CHAIN_FRAMES {
        let wire = send(&mut sender, now);
        let decoded = super::super::decode(&wire).unwrap();
        receiver.request(decoded).unwrap();
        assert!(receiver.negotiated().is_none());
        assert!(matches!(
            receiver.publish(&mut [0; 256], |_| Err("blocked")),
            Err(PublishError::Send("blocked"))
        ));
        assert_eq!(receiver.request(decoded), Err(Error::Busy));
        let ack_wire = reply(&mut receiver);
        let Frame::Control {
            number,
            acknowledgment,
            mode,
            payload_crc,
            ..
        } = super::super::decode(&ack_wire).unwrap()
        else {
            panic!("control");
        };
        assert_eq!(
            (usize::from(number), acknowledgment, mode, payload_crc),
            (index, Acknowledgment::Positive, 1, PayloadCheck::Absent)
        );
        receiver.request(decoded).unwrap();
        assert_eq!(reply(&mut receiver), ack_wire);
        let effect = sender
            .acknowledgment(super::super::decode(&ack_wire).unwrap(), now)
            .unwrap();
        assert_eq!(
            effect,
            if index + 1 == MAX_CHAIN_FRAMES {
                AckEffect::Complete
            } else {
                AckEffect::Next
            }
        );
        assert_eq!(
            receiver.negotiated().is_some(),
            index + 1 == MAX_CHAIN_FRAMES
        );
    }
    assert_eq!(sender.negotiated().unwrap().formats().rows().count(), 4);
    assert_eq!(receiver.negotiated().unwrap().formats().rows().count(), 4);
    assert!(
        !sender
            .negotiated()
            .unwrap()
            .same_session(receiver.negotiated().unwrap())
    );
}

#[test]
fn negative_and_erroneous_acks_share_the_per_frame_retry_budget() {
    let now = Instant::now();
    let mut sender = Sender::new(
        &[frame(0, true), frame(1, false)],
        ModeVersion::new(1).unwrap(),
        settings(1),
    )
    .unwrap();
    let first = send(&mut sender, now);
    let Frame::Control {
        number,
        mode,
        procedure,
        payload_crc,
        ..
    } = ack(0)
    else {
        unreachable!()
    };
    let nack = Frame::Control {
        number,
        mode,
        procedure,
        payload_crc,
        acknowledgment: Acknowledgment::Negative,
        payload: &[4],
    };
    assert_eq!(sender.acknowledgment(nack, now), Ok(AckEffect::Retry));
    assert_eq!(send(&mut sender, now), first);
    assert_eq!(sender.acknowledgment(ack(0), now), Ok(AckEffect::Next));
    send(&mut sender, now);
    assert_eq!(sender.acknowledgment(ack(0), now), Ok(AckEffect::Retry));
    send(&mut sender, now);
    assert_eq!(sender.acknowledgment(nack, now), Ok(AckEffect::Retry));
    assert_eq!(
        sender.failure(),
        Some(Failure {
            frame: 1,
            reason: RetryReason::Negative
        })
    );
}

#[test]
fn unsupported_modes_report_the_highest_common_or_local_version() {
    let mut bytes = [0; 128];
    let mut receiver = Receiver::new(Versions::new(3).unwrap()).unwrap();
    for (offered, expected) in [(2, 1), (4, 1), (1, 0)] {
        let proposal = Initialization::new(
            &[Combination::new(Rfci::new(0).unwrap(), &[1], None).unwrap()],
            Versions::new(offered).unwrap(),
            DataPduType::Crc,
            false,
        )
        .unwrap();
        receiver
            .request(request(&proposal, 0, 2, &mut bytes))
            .unwrap();
        let response = reply(&mut receiver);
        let Frame::Control {
            acknowledgment,
            mode,
            payload,
            ..
        } = super::super::decode(&response).unwrap()
        else {
            panic!("control")
        };
        assert_eq!(acknowledgment, Acknowledgment::Negative);
        assert_eq!(mode, expected);
        assert_eq!(payload, &[(Cause::Version as u8) << ERROR_CAUSE_SHIFT]);
        assert!(receiver.negotiated().is_none());
    }
}

#[test]
fn malformed_or_unadmitted_proposals_never_install_media_formats() {
    let mut bytes = [0; 128];
    let mut receiver = Receiver::new(Versions::new(3).unwrap()).unwrap();
    receiver
        .request_with(request(&frame(0, false), 0, 0, &mut bytes), |_| {
            Err(Cause::Rfci)
        })
        .unwrap();
    let response = reply(&mut receiver);
    assert!(
        matches!(super::super::decode(&response).unwrap(), Frame::Control {acknowledgment: Acknowledgment::Negative, payload, ..} if payload == [(Cause::Rfci as u8) << ERROR_CAUSE_SHIFT])
    );
    assert!(receiver.negotiated().is_none());
    receiver
        .request(request(&frame(0, true), 3, 0, &mut bytes))
        .unwrap();
    let response = reply(&mut receiver);
    assert!(
        matches!(super::super::decode(&response).unwrap(), Frame::Control {acknowledgment: Acknowledgment::Negative, payload, ..} if payload == [(Cause::FrameNumber as u8) << ERROR_CAUSE_SHIFT])
    );
    assert_eq!(receiver.request(ack(0)), Err(Error::NotRequest));
    let Frame::Control {
        number,
        acknowledgment,
        mode,
        procedure,
        payload,
        ..
    } = request(&frame(0, false), 0, 0, &mut bytes)
    else {
        unreachable!()
    };
    receiver
        .request(Frame::Control {
            number,
            acknowledgment,
            mode,
            procedure,
            payload,
            payload_crc: PayloadCheck::Failed,
        })
        .unwrap();
    let response = reply(&mut receiver);
    assert!(
        matches!(super::super::decode(&response).unwrap(), Frame::Control {acknowledgment: Acknowledgment::Negative, payload, ..} if payload == [(Cause::PayloadCrc as u8) << ERROR_CAUSE_SHIFT])
    );
}

#[test]
fn pending_reinitialization_suspends_old_media_and_replaces_only_after_publication() {
    let mut bytes = [0; 128];
    let mut receiver = Receiver::new(Versions::new(3).unwrap()).unwrap();
    receiver
        .request(request(&frame(0, false), 0, 0, &mut bytes))
        .unwrap();
    assert!(receiver.pending_negotiated().is_some());
    assert!(receiver.negotiated().is_none());
    assert!(matches!(
        receiver.publish(&mut [], |_| Ok::<_, ()>(())),
        Err(PublishError::Wire(WireError::Capacity))
    ));
    reply(&mut receiver);
    let old = receiver.negotiated().unwrap().clone();
    receiver
        .request(request(&frame(1, false), 0, 0, &mut bytes))
        .unwrap();
    assert!(receiver.negotiated().is_none());
    assert!(!receiver.pending_negotiated().unwrap().same_session(&old));
    receiver.reject_pending(Cause::Value).unwrap();
    reply(&mut receiver);
    assert!(receiver.negotiated().unwrap().same_session(&old));
    receiver
        .request(request(&frame(1, false), 0, 0, &mut bytes))
        .unwrap();
    reply(&mut receiver);
    assert!(!receiver.negotiated().unwrap().same_session(&old));
    assert!(
        receiver
            .negotiated()
            .unwrap()
            .formats()
            .lookup(Rfci::new(0).unwrap())
            .is_none()
    );
}

#[test]
fn invalid_local_configuration_and_backwards_clocks_are_explicit_errors() {
    assert!(Settings::new(Duration::ZERO, 3, Versions::new(3).unwrap()).is_err());
    assert!(Settings::new(Duration::from_secs(1), 3, Versions::new(4).unwrap()).is_err());
    assert!(Sender::new(&[], ModeVersion::new(1).unwrap(), settings(3)).is_err());
    assert!(Sender::new(&[frame(0, true)], ModeVersion::new(1).unwrap(), settings(3)).is_err());
    assert!(
        Sender::new(
            &[frame(0, true), frame(0, false)],
            ModeVersion::new(1).unwrap(),
            settings(3)
        )
        .is_err()
    );
    let now = Instant::now();
    let mut sender = Sender::new(
        &[frame(0, false)],
        ModeVersion::new(1).unwrap(),
        settings(0),
    )
    .unwrap();
    send(&mut sender, now);
    assert_eq!(sender.tick(now - Duration::from_nanos(1)), Err(Error::Time));
    sender.tick(now + Duration::from_millis(100)).unwrap();
    assert!(sender.failure().is_some());
}

#[test]
fn rate_control_is_acknowledged_and_time_alignment_is_refused_as_unsupported() {
    let control = |procedure: u8, number: u8| Frame::Control {
        number,
        acknowledgment: Acknowledgment::Procedure,
        mode: 1,
        procedure,
        payload_crc: PayloadCheck::Valid,
        payload: &[0x80],
    };
    let mut output = [0; 16];
    let reply = ControlProcedure::classify(control(1, 2))
        .unwrap()
        .reply()
        .unwrap();
    let bytes = reply.encode(&mut output).unwrap();
    assert_eq!(
        super::super::decode(&output[..bytes]).unwrap(),
        Frame::Control {
            number: 2,
            acknowledgment: Acknowledgment::Positive,
            mode: 1,
            procedure: 1,
            payload_crc: PayloadCheck::Absent,
            payload: &[],
        }
    );
    for (procedure, cause) in [(2, 47), (9, 5)] {
        let reply = ControlProcedure::classify(control(procedure, 3))
            .unwrap()
            .reply()
            .unwrap();
        let bytes = reply.encode(&mut output).unwrap();
        let Frame::Control {
            acknowledgment: Acknowledgment::Negative,
            procedure: echoed,
            payload,
            ..
        } = super::super::decode(&output[..bytes]).unwrap()
        else {
            panic!("negative acknowledgement expected")
        };
        assert_eq!(echoed, procedure);
        assert_eq!(payload[0] >> 2, cause);
    }
    assert_eq!(
        ControlProcedure::classify(control(3, 4)),
        Some(ControlProcedure::ErrorEvent)
    );
    assert_eq!(ControlProcedure::classify(control(0, 4)), None);
    assert_eq!(ControlProcedure::classify(ack(4)), None);
}
