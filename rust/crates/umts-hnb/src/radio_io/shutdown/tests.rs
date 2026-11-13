// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    radio_io::{PumpError, PumpProgress, TxPreparation, TxSettings, prepare_tx},
    test_support::{count, source},
};
use num_complex::Complex32;
use sdr_hal::{
    RadioTx, Schedule, TxMetadata,
    error::{Error, ErrorKind},
};
use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

const SAMPLE_RATE: u64 = 7_680_000;
const TICK_RATE: u64 = 1_000_000;
const ORIGIN_TICK: u64 = 1000;

enum Write {
    Complete,
    Partial(usize),
    Unknown,
    Panic,
}
#[derive(Default)]
struct State {
    now: AtomicU64,
    queries: AtomicU64,
    stopped: AtomicBool,
    fail_stop: AtomicBool,
    writes: Mutex<Vec<(u64, usize)>>,
}
struct ScriptedTx {
    state: Arc<State>,
    writes: VecDeque<Write>,
}
impl RadioTx for ScriptedTx {
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        SAMPLE_RATE as f64
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.state.queries.fetch_add(1, Ordering::AcqRel);
        Ok(self.state.now.load(Ordering::Acquire))
    }
    fn transmit(&mut self, _: &[Complex32]) -> Result<(), Error> {
        panic!("untimed transmission")
    }
    fn enable_transmit(&mut self, active: bool) -> Result<(), Error> {
        if !active {
            if self.state.fail_stop.load(Ordering::Acquire) {
                return Err(Error::new(ErrorKind::Backend, "injected stop failure"));
            }
            self.state.stopped.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
        let Schedule::At(time) = metadata.schedule else {
            panic!("untimed write")
        };
        self.state
            .writes
            .lock()
            .unwrap()
            .push((time.ticks(), samples.len()));
        match self.writes.pop_front().unwrap_or(Write::Complete) {
            Write::Complete => Ok(samples.len()),
            Write::Partial(accepted) => {
                assert!(accepted < samples.len());
                Ok(accepted)
            }
            Write::Unknown => Err(Error::new(ErrorKind::Backend, "unknown accepted prefix")),
            Write::Panic => panic!("injected write panic"),
        }
    }
}
fn pair(writes: Vec<Write>) -> (TxPreparation, TxPump, Arc<State>) {
    let state = Arc::new(State::default());
    let tx = Box::new(ScriptedTx {
        state: state.clone(),
        writes: writes.into(),
    });
    let (preparation, pump) = prepare_tx(
        source(false),
        tx,
        TxSettings::offline(ORIGIN_TICK, count(2), count(1), count(3)),
    )
    .unwrap();
    (preparation, pump, state)
}
fn expected_tick(end: SampleInstant) -> u64 {
    ORIGIN_TICK
        + u64::try_from(
            (u128::from(end.index) * u128::from(TICK_RATE) + u128::from(SAMPLE_RATE / 2))
                / u128::from(SAMPLE_RATE),
        )
        .unwrap()
        + 1
}

#[test]
fn partial_then_unknown_progress_retains_the_entire_attempted_suffix_horizon() {
    let (mut preparation, mut pump, state) = pair(vec![Write::Partial(17), Write::Unknown]);
    pump.start().unwrap();
    let window = preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(
        pump.cycle(),
        Ok(PumpProgress::Partial { accepted: 17, .. })
    ));
    let full = pump.terminal_tail().unwrap();
    assert_eq!(full.end(), window.next_sample);
    assert_eq!(full.tick(), expected_tick(window.next_sample));
    assert!(
        matches!(pump.cycle(),Err(PumpError::Submission(failure)) if failure.accepted_samples.is_none())
    );
    assert_eq!(pump.terminal_tail(), Some(full));
    let writes = state.writes.lock().unwrap();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[1].1, writes[0].1 - 17);
    drop(writes);
    assert!(matches!(
        pump.observe_terminal_drain(),
        Err(TerminalDrainError::NotStopped)
    ));
    pump.stop().unwrap();
    assert_eq!(
        pump.observe_terminal_drain().unwrap(),
        TerminalTransmitDrain::Waiting {
            tail: full,
            observed_tick: 0
        }
    );
}

#[test]
fn panic_inside_first_write_preserves_its_full_attempted_horizon_in_the_original_pump() {
    let (mut preparation, mut pump, state) = pair(vec![Write::Panic]);
    pump.start().unwrap();
    let window = preparation.prepare_one().unwrap().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| pump.cycle())).is_err());
    let tail = pump.terminal_tail().unwrap();
    assert_eq!(tail.end(), window.next_sample);
    assert_eq!(tail.tick(), expected_tick(window.next_sample));
    assert_eq!(state.writes.lock().unwrap().len(), 1);
    assert!(matches!(
        pump.observe_terminal_drain(),
        Err(TerminalDrainError::NotStopped)
    ));
    pump.stop().unwrap();
    assert_eq!(
        pump.observe_terminal_drain().unwrap(),
        TerminalTransmitDrain::Waiting {
            tail,
            observed_tick: 0
        }
    );
}

#[test]
fn successful_hal_stop_does_not_advance_time_or_prove_the_transmit_tail_elapsed() {
    let (mut preparation, mut pump, state) = pair(vec![Write::Complete]);
    pump.start().unwrap();
    let window = preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    let tail = pump.terminal_tail().unwrap();
    assert_eq!(tail.tick(), expected_tick(window.next_sample));
    state.now.store(tail.tick() - 1, Ordering::Release);
    pump.stop().unwrap();
    assert!(state.stopped.load(Ordering::Acquire));
    assert_eq!(state.now.load(Ordering::Acquire), tail.tick() - 1);
    assert_eq!(state.queries.load(Ordering::Acquire), 0);
    assert_eq!(
        pump.observe_terminal_drain().unwrap(),
        TerminalTransmitDrain::Waiting {
            tail,
            observed_tick: tail.tick() - 1
        }
    );
    state.now.store(tail.tick(), Ordering::Release);
    assert_eq!(
        pump.observe_terminal_drain().unwrap(),
        TerminalTransmitDrain::Elapsed {
            tail,
            observed_tick: tail.tick()
        }
    );
    state.now.store(tail.tick() - 1, Ordering::Release);
    assert!(
        matches!(pump.observe_terminal_drain(),Err(TerminalDrainError::ClockReversed {previous,observed}) if previous==tail.tick()&&observed==tail.tick()-1)
    );
    assert_eq!(pump.terminal_tail(), Some(tail));
}

#[test]
fn no_writes_are_unsubmitted_only_after_a_successful_actual_stop() {
    let (_preparation, mut pump, state) = pair(vec![]);
    assert_eq!(pump.terminal_tail(), None);
    assert!(matches!(
        pump.observe_terminal_drain(),
        Err(TerminalDrainError::NotStopped)
    ));
    state.fail_stop.store(true, Ordering::Release);
    assert!(pump.stop().is_err());
    assert!(matches!(
        pump.observe_terminal_drain(),
        Err(TerminalDrainError::NotStopped)
    ));
    state.fail_stop.store(false, Ordering::Release);
    pump.stop().unwrap();
    assert_eq!(
        pump.observe_terminal_drain().unwrap(),
        TerminalTransmitDrain::Unsubmitted
    );
    assert_eq!(state.queries.load(Ordering::Acquire), 0);
    assert!(state.writes.lock().unwrap().is_empty());
}

#[test]
fn successive_attempts_extend_but_never_shrink_the_terminal_horizon() {
    let (mut preparation, mut pump, _state) = pair(vec![Write::Complete, Write::Partial(9)]);
    pump.start().unwrap();
    preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    let previous = pump.terminal_tail().unwrap();
    let next = preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(
        pump.cycle(),
        Ok(PumpProgress::Partial { accepted: 9, .. })
    ));
    let tail = pump.terminal_tail().unwrap();
    assert_eq!(tail.end(), next.next_sample);
    assert_eq!(tail.tick(), expected_tick(next.next_sample));
    assert!(tail.tick() > previous.tick());
}
