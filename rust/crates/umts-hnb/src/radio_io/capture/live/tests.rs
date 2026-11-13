// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn geometry(direction: Direction) -> Geometry {
    Geometry {
        direction,
        epoch: 42,
        sample_rate: SampleRate::new(1000, 1).unwrap(),
        tick_rate: NonZeroU64::new(4000).unwrap(),
    }
}

fn recording(
    directory: &std::path::Path,
    direction: Direction,
    duration_ms: u64,
) -> Box<Recording> {
    Recording::new(
        directory.to_owned(),
        1,
        geometry(direction),
        Duration::from_millis(duration_ms),
    )
    .unwrap()
}

#[test]
fn receive_window_is_sample_timed_and_preserves_gap_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("receive");
    let (mut port, mut recorder) = channel(geometry(Direction::Receive));
    let recording = recording(&path, Direction::Receive, 10);
    let control = recording.control();
    assert!(port.publish(recording).is_ok());
    let values = [Complex32::new(0.25, -0.5); 8];
    recorder.record_receive(100, 400, 10, &values[..4], false);
    recorder.record_receive(108, 432, 11, &values, true);
    let progress = control.progress();
    assert_eq!(progress.phase, Phase::Flushing);
    assert_eq!(progress.samples, 6);
    assert_eq!(progress.discontinuities, 1);
    let (reason, result) = port.take_finished().unwrap().finish();
    assert_eq!(reason, EndReason::Duration);
    let summary = result.unwrap();
    assert_eq!(summary.attempted_samples, 6);
    assert_eq!(summary.records, 2);
    let raw = std::fs::read(path.join("raw-rx-0.bin")).unwrap();
    assert_eq!(&raw[..8], b"UMTSRX01");
    let metadata = std::fs::read(path.join("raw-rx-0.meta")).unwrap();
    let second = super::super::METADATA_HEADER_BYTES + super::super::METADATA_BYTES;
    assert_eq!(
        u64::from_le_bytes(metadata[second..second + 8].try_into().unwrap()),
        108
    );
    assert_eq!(
        u64::from_le_bytes(metadata[second + 8..second + 16].try_into().unwrap()),
        432
    );
    assert_eq!(
        u32::from_le_bytes(metadata[second + 28..second + 32].try_into().unwrap()),
        2
    );
    assert_eq!(
        u32::from_le_bytes(metadata[second + 32..second + 36].try_into().unwrap()),
        RX_DISCONTINUITY
    );
}

#[test]
fn transmit_attempts_preserve_unknown_acceptance_and_stop_without_disk_io() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("transmit");
    let (mut port, mut recorder) = channel(geometry(Direction::Transmit));
    let recording = recording(&path, Direction::Transmit, 100);
    let control = recording.control();
    assert!(port.publish(recording).is_ok());
    recorder.record_transmit(100, 400, 10, &[Complex32::new(0.25, -0.5); 4], None, true);
    assert!(!path.join("raw-tx-0.bin").exists());
    control.stop();
    recorder.poll();
    let (reason, summary) = port.take_finished().unwrap().finish();
    assert_eq!(reason, EndReason::Stopped);
    assert_eq!(summary.unwrap().incomplete_records, 1);
    let metadata = std::fs::read(path.join("raw-tx-0.meta")).unwrap();
    let start = super::super::METADATA_HEADER_BYTES;
    assert_eq!(
        u32::from_le_bytes(metadata[start + 28..start + 32].try_into().unwrap()),
        u32::MAX
    );
    assert_eq!(
        u32::from_le_bytes(metadata[start + 32..start + 36].try_into().unwrap()),
        TX_INCOMPLETE
    );
}

#[test]
fn geometry_single_outstanding_and_closed_epoch_are_enforced() {
    let directory = tempfile::tempdir().unwrap();
    let (mut port, mut recorder) = channel(geometry(Direction::Receive));
    assert!(
        port.publish(recording(
            &directory.path().join("wrong"),
            Direction::Transmit,
            10
        ))
        .is_err()
    );
    assert!(
        port.publish(recording(
            &directory.path().join("first"),
            Direction::Receive,
            10
        ))
        .is_ok()
    );
    let second = recording(&directory.path().join("second"), Direction::Receive, 10);
    let second = port.publish(second).unwrap_err();
    recorder.stop();
    assert_eq!(
        port.take_finished().unwrap().finish().0,
        EndReason::RadioStopped
    );
    assert!(port.publish(second).is_err());
}

#[test]
fn retirement_drains_publication_that_was_already_in_flight() {
    let directory = tempfile::tempdir().unwrap();
    let (mut port, mut recorder) = channel(geometry(Direction::Receive));
    port.publishing.store(true, Ordering::SeqCst);
    let recording = recording(&directory.path().join("late"), Direction::Receive, 10);
    let closed = Arc::clone(&port.closed);
    let worker = std::thread::spawn(move || {
        recorder.stop();
        recorder
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while !closed.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(port.commands.try_send(recording).is_ok());
    port.publishing.store(false, Ordering::SeqCst);
    let _recorder = worker.join().unwrap();
    assert_eq!(
        port.take_finished().unwrap().finish().0,
        EndReason::RadioStopped
    );
}

#[test]
fn invalid_hal_acceptance_is_not_clamped_into_valid_capture_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let (mut port, mut recorder) = channel(geometry(Direction::Transmit));
    assert!(
        port.publish(recording(
            &directory.path().join("invalid"),
            Direction::Transmit,
            1
        ))
        .is_ok()
    );
    recorder.record_transmit(100, 400, 1, &[Complex32::new(0.25, -0.5)], Some(2), true);
    let (reason, result) = port.take_finished().unwrap().finish();
    assert!(matches!(reason, EndReason::Invalid(_)));
    assert_eq!(result.unwrap().attempted_samples, 0);
}
