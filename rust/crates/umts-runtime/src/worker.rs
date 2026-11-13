// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const START_GATE_WAIT: Duration = Duration::from_millis(1);

#[derive(Clone, Default)]
pub struct StartGate(Arc<AtomicBool>);
impl StartGate {
    pub fn release(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct StopState {
    stopping: AtomicBool,
    wait_lock: Mutex<()>,
    wake: Condvar,
}

#[derive(Clone, Default)]
pub struct StopToken(Arc<StopState>);

impl StopToken {
    pub fn is_stopping(&self) -> bool {
        self.0.stopping.load(Ordering::Acquire)
    }

    pub fn request_stop(&self) {
        let _guard = self
            .0
            .wait_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.0.stopping.store(true, Ordering::Release);
        self.0.wake.notify_all();
    }

    pub fn wait_or_stop(&self, timeout: Duration) -> bool {
        let guard = self
            .0
            .wait_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _result = self
            .0
            .wake
            .wait_timeout_while(guard, timeout, |_| !self.is_stopping())
            .unwrap_or_else(|error| error.into_inner());
        self.is_stopping()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerExit {
    Stopped,
    Failed(String),
    Panicked,
}

#[derive(Debug, PartialEq, Eq)]
pub enum JoinError {
    Deadline,
    AlreadyJoined,
}

#[must_use]
pub struct Worker<T = ()> {
    stop: StopToken,
    completion: mpsc::Receiver<()>,
    handle: Option<JoinHandle<(WorkerExit, Box<T>)>>,
}

pub struct SpawnError<T> {
    pub error: std::io::Error,
    pub state: Box<T>,
}

impl<T> std::fmt::Debug for SpawnError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

#[cfg(test)]
impl Worker {
    pub fn spawn_gated(
        name: impl Into<String>,
        stop: StopToken,
        gate: StartGate,
        run: impl FnOnce(StopToken) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Self> {
        Self::spawn_gated_with_role(name, stop, gate, crate::scheduling::Role::Background, run)
    }

    pub fn spawn_gated_with_role(
        name: impl Into<String>,
        stop: StopToken,
        gate: StartGate,
        role: crate::scheduling::Role,
        run: impl FnOnce(StopToken) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Self> {
        Self::spawn_owned_gated_with_role(
            name,
            stop,
            gate,
            role,
            Box::new(()),
            move |_, stop| run(stop),
            |_| {},
        )
        .map_err(|failure| failure.error)
    }
    /// The worker must use finite I/O waits and observe its stop token.
    pub fn spawn(
        name: impl Into<String>,
        stop: StopToken,
        run: impl FnOnce(StopToken) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Self> {
        Self::spawn_with_role(name, stop, crate::scheduling::Role::Background, run)
    }

    pub fn spawn_with_role(
        name: impl Into<String>,
        stop: StopToken,
        role: crate::scheduling::Role,
        run: impl FnOnce(StopToken) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Self> {
        let gate = StartGate::default();
        gate.release();
        Self::spawn_gated_with_role(name, stop, gate, role, run)
    }

    /// A deadline error retains the join handle so shutdown can be retried.
    pub fn join(&mut self, timeout: Duration) -> Result<WorkerExit, JoinError> {
        self.join_owned(timeout).map(|(exit, _)| exit)
    }
}

impl<T: Send + 'static> Worker<T> {
    /// State is borrowed during execution and returned even when execution panics.
    /// Execution and finalization must use finite I/O waits and observe cancellation.
    pub fn spawn_owned_gated_with_role(
        name: impl Into<String>,
        stop: StopToken,
        gate: StartGate,
        role: crate::scheduling::Role,
        state: Box<T>,
        run: impl FnOnce(&mut T, StopToken) -> Result<(), String> + Send + 'static,
        finish: fn(&mut T),
    ) -> Result<Self, SpawnError<T>> {
        // The startup slot preserves ownership if OS thread creation fails.
        let startup = Arc::new(Mutex::new(Some(state)));
        let thread_startup = startup.clone();
        let (tx, completion) = mpsc::sync_channel(1);
        let (ready, started) = mpsc::sync_channel(1);
        let thread_stop = stop.clone();
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut state = thread_startup.lock().unwrap().take().unwrap();
                let configured = crate::scheduling::configure_current(role);
                let success = configured.is_ok();
                let _ = ready.send(configured);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if !success {
                        return Err("thread scheduling failed".into());
                    }
                    while !gate.0.load(Ordering::Acquire) {
                        if thread_stop.wait_or_stop(START_GATE_WAIT) {
                            return Ok(());
                        }
                    }
                    if thread_stop.is_stopping() {
                        return Ok(());
                    }
                    run(&mut state, thread_stop.clone())
                }));
                let mut exit = match result {
                    Ok(Ok(())) => WorkerExit::Stopped,
                    Ok(Err(error)) => WorkerExit::Failed(error),
                    Err(_) => WorkerExit::Panicked,
                };
                if exit != WorkerExit::Stopped {
                    thread_stop.request_stop();
                }
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| finish(&mut state)))
                    .is_err()
                {
                    exit = WorkerExit::Panicked;
                    thread_stop.request_stop();
                }
                let _ = tx.send(());
                (exit, state)
            })
            .map_err(|error| {
                let mut state = startup.lock().unwrap().take().unwrap();
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| finish(&mut state)));
                SpawnError { error, state }
            })?;
        match started.recv() {
            Ok(Ok(())) => {}
            result => {
                let (_, state) = handle.join().expect("worker startup retains its state");
                return Err(SpawnError {
                    state,
                    error: match result {
                        Ok(Err(error)) => error,
                        _ => std::io::Error::other("thread scheduling startup disconnected"),
                    },
                });
            }
        }
        Ok(Self {
            stop,
            completion,
            handle: Some(handle),
        })
    }
}

impl<T> Worker<T> {
    pub fn request_stop(&self) {
        self.stop.request_stop();
    }

    /// A deadline error retains the join handle so shutdown can be retried.
    pub fn join_owned(&mut self, timeout: Duration) -> Result<(WorkerExit, Box<T>), JoinError> {
        if self.handle.is_none() {
            return Err(JoinError::AlreadyJoined);
        }
        match self.completion.recv_timeout(timeout) {
            Ok(()) => (),
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(JoinError::Deadline),
            Err(mpsc::RecvTimeoutError::Disconnected) => (),
        }
        let handle = self.handle.take().expect("unjoined worker has a handle");
        Ok(handle
            .join()
            .expect("worker catches execution and finalization panics"))
    }

    pub fn stop_and_join(&mut self) -> Option<(WorkerExit, Box<T>)> {
        self.request_stop();
        Some(
            self.handle
                .take()?
                .join()
                .expect("worker catches execution and finalization panics"),
        )
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        // A worker cannot outlive the state whose leases it may still use.
        let _ = self.stop_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_owned_state_stays_on_heap_across_start_panic_and_join() {
        const STATE_BYTES: usize = 512 * 1024;
        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                for panic in [false, true] {
                    let state: Box<[u8; STATE_BYTES]> =
                        vec![7; STATE_BYTES].into_boxed_slice().try_into().unwrap();
                    let gate = StartGate::default();
                    gate.release();
                    let mut worker = Worker::spawn_owned_gated_with_role(
                        "heap-owned-state",
                        StopToken::default(),
                        gate,
                        crate::scheduling::Role::Background,
                        state,
                        move |state, _| {
                            state[0] = 9;
                            if panic {
                                panic!("injected owned-state panic");
                            }
                            Ok(())
                        },
                        |state| state[STATE_BYTES - 1] = 11,
                    )
                    .unwrap();
                    let (exit, state) = worker.join_owned(Duration::from_secs(1)).unwrap();
                    assert_eq!(
                        exit,
                        if panic {
                            WorkerExit::Panicked
                        } else {
                            WorkerExit::Stopped
                        }
                    );
                    assert_eq!((state[0], state[STATE_BYTES - 1]), (9, 11));
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    struct ReturnedOwner {
        value: Option<u32>,
        sender: mpsc::SyncSender<u32>,
    }
    impl ReturnedOwner {
        fn value(&self) -> u32 {
            self.value.unwrap()
        }
    }
    impl Drop for ReturnedOwner {
        fn drop(&mut self) {
            if let Some(value) = self.value.take() {
                let _ = self.sender.send(value);
            }
        }
    }

    #[test]
    fn closed_start_gate_never_invokes_run_and_cancellation_returns_original_owner_once() {
        let gate = StartGate::default();
        let stop = StopToken::default();
        let invoked = Arc::new(AtomicBool::new(false));
        let called = invoked.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let owner = ReturnedOwner {
            value: Some(47),
            sender,
        };
        let mut worker =
            Worker::spawn_gated("closed-start-gate", stop.clone(), gate.clone(), move |_| {
                assert_eq!(owner.value(), 47);
                called.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap();
        assert_eq!(worker.join(Duration::ZERO), Err(JoinError::Deadline));
        assert!(!invoked.load(Ordering::Acquire));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        stop.request_stop();
        assert_eq!(worker.join(Duration::from_secs(1)), Ok(WorkerExit::Stopped));
        gate.release();
        assert!(!invoked.load(Ordering::Acquire));
        assert_eq!(receiver.try_recv(), Ok(47));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn shared_start_gate_releases_every_constructed_worker() {
        let gate = StartGate::default();
        let stop = StopToken::default();
        let (sender, receiver) = mpsc::sync_channel(2);
        let mut workers = Vec::new();
        for id in 0..2 {
            let sender = sender.clone();
            workers.push(
                Worker::spawn_gated("shared-start-gate", stop.clone(), gate.clone(), move |_| {
                    sender.send(id).unwrap();
                    Ok(())
                })
                .unwrap(),
            );
        }
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        gate.release();
        for worker in &mut workers {
            assert_eq!(worker.join(Duration::from_secs(1)), Ok(WorkerExit::Stopped));
        }
        let mut received = [receiver.try_recv().unwrap(), receiver.try_recv().unwrap()];
        received.sort();
        assert_eq!(received, [0, 1]);
    }

    #[test]
    fn cancellation_before_an_open_gate_never_invokes_the_closure() {
        let gate = StartGate::default();
        gate.release();
        let stop = StopToken::default();
        stop.request_stop();
        let mut worker = Worker::spawn_gated("cancelled-open-gate", stop, gate, |_| {
            panic!("canceled run invoked")
        })
        .unwrap();
        assert_eq!(worker.join(Duration::from_secs(1)), Ok(WorkerExit::Stopped));
    }

    #[test]
    fn stop_wakes_sleeping_worker_and_repeated_start_stop_joins_every_thread() {
        for _ in 0..100 {
            let stop = StopToken::default();
            let mut worker = Worker::spawn("lifecycle-test", stop.clone(), |stop| {
                while !stop.wait_or_stop(Duration::from_secs(60)) {}
                Ok(())
            })
            .unwrap();
            stop.request_stop();
            assert_eq!(worker.join(Duration::from_secs(1)), Ok(WorkerExit::Stopped));
            assert_eq!(worker.join(Duration::ZERO), Err(JoinError::AlreadyJoined));
        }
    }

    #[test]
    fn failure_and_panic_cancel_peer_workers() {
        for panic in [false, true] {
            let stop = StopToken::default();
            let mut peer = Worker::spawn("peer", stop.clone(), |stop| {
                while !stop.wait_or_stop(Duration::from_secs(60)) {}
                Ok(())
            })
            .unwrap();
            let mut failed = Worker::spawn("failure", stop.clone(), move |_| {
                if panic {
                    panic!("injected worker panic");
                }
                Err("injected device loss".to_owned())
            })
            .unwrap();
            let exit = failed.join(Duration::from_secs(1)).unwrap();
            assert!(matches!(exit, WorkerExit::Failed(_) | WorkerExit::Panicked));
            assert!(stop.is_stopping());
            assert_eq!(peer.join(Duration::from_secs(1)), Ok(WorkerExit::Stopped));
        }
    }

    #[test]
    fn timed_out_join_does_not_detach_and_can_be_retried() {
        let stop = StopToken::default();
        let mut worker = Worker::spawn("retry-join", stop.clone(), |stop| {
            while !stop.wait_or_stop(Duration::from_secs(60)) {}
            Ok(())
        })
        .unwrap();
        assert_eq!(worker.join(Duration::ZERO), Err(JoinError::Deadline));
        stop.request_stop();
        assert_eq!(worker.join(Duration::from_secs(1)), Ok(WorkerExit::Stopped));
    }
}
