// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct ShutdownSignals {
    stopping: Arc<AtomicBool>,
    registrations: Vec<signal_hook::SigId>,
}

impl ShutdownSignals {
    pub fn install() -> std::io::Result<Self> {
        let mut signals = Self {
            stopping: Arc::new(AtomicBool::new(false)),
            registrations: Vec::new(),
        };
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            signals.registrations.push(signal_hook::flag::register(
                signal,
                signals.stopping.clone(),
            )?);
        }
        Ok(signals)
    }

    pub fn requested(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::Command,
        time::{Duration, Instant},
    };

    const CHILD_MARKER: &str = "HNB_SHUTDOWN_SIGNAL_TEST_CHILD";
    const TEST_NAME: &str = "serving::shutdown::tests::signal_handlers_set_only_the_owned_flag_and_release_their_registrations";

    #[test]
    fn signal_handlers_set_only_the_owned_flag_and_release_their_registrations() {
        if std::env::var_os(CHILD_MARKER).is_some() {
            let signals = ShutdownSignals::install().unwrap();
            let owned = signals.stopping.clone();
            assert!(!signals.requested());
            assert!(!signals.requested());
            signal_hook::low_level::raise(signal_hook::consts::SIGTERM).unwrap();
            assert!(signals.requested());
            drop(signals);
            assert_eq!(Arc::strong_count(&owned), 1);
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_MARKER, "1")
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if started.elapsed() > Duration::from_secs(3) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("isolated signal test exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
