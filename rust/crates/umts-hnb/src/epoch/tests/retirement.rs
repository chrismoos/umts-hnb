// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::radio_io::{HalStopOutcome, TerminalDrainError, TerminalTransmitDrain};

fn submitted() -> (CommonEpoch<Mock>, Arc<State>) {
    let (epoch, state) = spawn(parts(), settings());
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    state.tick.store(8_700_000, Ordering::Release);
    until(|| state.writes.load(Ordering::Acquire) > 0);
    (epoch, state)
}

fn retained(epoch: &CommonEpoch<Mock>, state: &State) {
    assert!(epoch.controller().is_none());
    assert!(epoch.controller.is_some());
    assert_eq!(state.tx_drops.load(Ordering::Acquire), 0);
    assert_eq!(state.rx_drops.load(Ordering::Acquire), 0);
}

#[test]
fn successful_hal_stop_does_not_retire_a_driver_submitted_tail_before_device_time() {
    let (mut epoch, state) = submitted();
    let report = epoch.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.tx.shutdown, HalStopOutcome::Succeeded);
    assert_eq!(report.rx.shutdown, HalStopOutcome::Succeeded);
    assert_eq!(report.tx.submission, WorkerExit::Stopped);
    assert!(!state.tx_active.load(Ordering::Acquire));
    assert!(!state.rx_active.load(Ordering::Acquire));
    retained(&epoch, &state);
    let TerminalTransmitDrain::Waiting {
        tail,
        observed_tick,
    } = epoch.poll_retirement().unwrap()
    else {
        panic!("submitted future IQ must retain its actual terminal tail");
    };
    assert_eq!(observed_tick, state.tick.load(Ordering::Acquire));
    assert!(tail.tick() > observed_tick);
    assert!(tail.end().index > 0);
    assert_eq!(tail.end().epoch, epoch.epoch());
    assert!(matches!(
        epoch.finish_retirement(),
        Err(EpochRetirementError::TailPending(_))
    ));
    retained(&epoch, &state);
    state.tick.store(tail.tick() - 1, Ordering::Release);
    assert_eq!(
        epoch.poll_retirement().unwrap(),
        TerminalTransmitDrain::Waiting {
            tail,
            observed_tick: tail.tick() - 1,
        }
    );
    assert!(matches!(
        epoch.finish_retirement(),
        Err(EpochRetirementError::TailPending(_))
    ));
    retained(&epoch, &state);
    state.tick.store(tail.tick(), Ordering::Release);
    assert_eq!(
        epoch.poll_retirement().unwrap(),
        TerminalTransmitDrain::Elapsed {
            tail,
            observed_tick: tail.tick(),
        }
    );
    let finished = epoch.finish_retirement().unwrap();
    assert!(epoch.controller.is_none());
    assert_eq!(state.tx_drops.load(Ordering::Acquire), 1);
    assert_eq!(state.rx_drops.load(Ordering::Acquire), 1);
    assert_eq!(epoch.finish_retirement().unwrap().tx, finished.tx);
    assert_eq!(state.tx_drops.load(Ordering::Acquire), 1);
    assert_eq!(state.rx_drops.load(Ordering::Acquire), 1);
}

#[test]
fn failed_or_reversed_retirement_clock_preserves_all_joined_owners() {
    let (mut epoch, state) = submitted();
    epoch.join(Duration::from_secs(1)).unwrap();
    let TerminalTransmitDrain::Waiting {
        tail,
        observed_tick,
    } = epoch.poll_retirement().unwrap()
    else {
        panic!("submitted future IQ must wait for device time");
    };
    state.bad_clock.store(true, Ordering::Release);
    assert!(matches!(
        epoch.poll_retirement(),
        Err(EpochRetirementError::Drain(TerminalDrainError::Driver(_)))
    ));
    assert!(matches!(
        epoch.finish_retirement(),
        Err(EpochRetirementError::Drain(TerminalDrainError::Driver(_)))
    ));
    retained(&epoch, &state);
    state.bad_clock.store(false, Ordering::Release);
    state.tick.store(observed_tick - 1, Ordering::Release);
    assert!(
        matches!(epoch.poll_retirement(), Err(EpochRetirementError::Drain(
        TerminalDrainError::ClockReversed { previous, observed }
    )) if previous == observed_tick && observed == observed_tick - 1)
    );
    retained(&epoch, &state);
    state.tick.store(observed_tick, Ordering::Release);
    assert!(matches!(
        epoch.poll_retirement().unwrap(),
        TerminalTransmitDrain::Waiting { .. }
    ));
    retained(&epoch, &state);
    state.tick.store(tail.tick(), Ordering::Release);
    epoch.finish_retirement().unwrap();
    assert!(epoch.controller.is_none());
    assert_eq!(state.tx_drops.load(Ordering::Acquire), 1);
    assert_eq!(state.rx_drops.load(Ordering::Acquire), 1);
}

#[test]
fn stop_failures_remain_separate_from_run_exit_and_retry_requires_tail_elapsed() {
    let (mut epoch, state) = submitted();
    state.fail_tx_stop.store(true, Ordering::Release);
    state.fail_rx_stop.store(true, Ordering::Release);
    let report = epoch.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.tx.submission, WorkerExit::Stopped);
    assert_eq!(report.rx.exit, WorkerExit::Stopped);
    assert!(
        matches!(&report.tx.shutdown, HalStopOutcome::Failed(error) if error.contains("TX stop failure"))
    );
    assert!(
        matches!(&report.rx.shutdown, HalStopOutcome::Failed(error) if error.contains("RX stop failure"))
    );
    retained(&epoch, &state);
    let before = state.rx_stop_calls.load(Ordering::Acquire);
    assert!(matches!(
        epoch.poll_retirement(),
        Err(EpochRetirementError::ReceiveStop(_))
    ));
    assert!(state.rx_stop_calls.load(Ordering::Acquire) > before);
    retained(&epoch, &state);
    state.fail_rx_stop.store(false, Ordering::Release);
    let before = state.tx_stop_calls.load(Ordering::Acquire);
    assert!(matches!(
        epoch.poll_retirement(),
        Err(EpochRetirementError::TransmitStop(_))
    ));
    assert!(state.tx_stop_calls.load(Ordering::Acquire) > before);
    assert!(!state.rx_active.load(Ordering::Acquire));
    retained(&epoch, &state);
    state.fail_tx_stop.store(false, Ordering::Release);
    let TerminalTransmitDrain::Waiting { tail, .. } = epoch.poll_retirement().unwrap() else {
        panic!("successful stop retry does not imply elapsed queued IQ");
    };
    assert!(!state.tx_active.load(Ordering::Acquire));
    assert!(matches!(
        epoch.finish_retirement(),
        Err(EpochRetirementError::TailPending(_))
    ));
    retained(&epoch, &state);
    state.tick.store(tail.tick(), Ordering::Release);
    let report = epoch.finish_retirement().unwrap();
    assert_eq!(report.tx.shutdown, HalStopOutcome::Succeeded);
    assert_eq!(report.rx.shutdown, HalStopOutcome::Succeeded);
    assert_eq!(report.tx.submission, WorkerExit::Stopped);
    assert_eq!(report.rx.exit, WorkerExit::Stopped);
    assert_eq!(state.tx_drops.load(Ordering::Acquire), 1);
    assert_eq!(state.rx_drops.load(Ordering::Acquire), 1);
}

#[test]
fn actual_joined_epoch_without_any_write_retires_as_unsubmitted() {
    let (mut epoch, state) = spawn(parts(), settings());
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    assert_eq!(state.writes.load(Ordering::Acquire), 0);
    assert_eq!(state.tick.load(Ordering::Acquire), 0);
    epoch.join(Duration::from_secs(1)).unwrap();
    retained(&epoch, &state);
    assert_eq!(
        epoch.poll_retirement().unwrap(),
        TerminalTransmitDrain::Unsubmitted
    );
    epoch.finish_retirement().unwrap();
    assert!(epoch.controller.is_none());
    assert_eq!(state.tx_drops.load(Ordering::Acquire), 1);
    assert_eq!(state.rx_drops.load(Ordering::Acquire), 1);
}
