// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{io, sync::OnceLock, thread};

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "linux")]
mod linux;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Main,
    Background,
    Usb,
    DriverIo,
    TxSubmit,
    RxReader,
    TxPrepare,
    RxProcess,
}

const ROLE_COUNT: usize = 8;
const ROLES: [Role; ROLE_COUNT] = [
    Role::Main,
    Role::Background,
    Role::Usb,
    Role::DriverIo,
    Role::TxSubmit,
    Role::RxReader,
    Role::TxPrepare,
    Role::RxProcess,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub fifo_priority: Option<u8>,
    pub cpu: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    assignments: Assignments,
    #[cfg(target_os = "linux")]
    allowed: rustix::thread::CpuSet,
}

static PLAN: OnceLock<Plan> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
pub struct Assignments {
    pub main: Assignment,
    pub background: Assignment,
    pub usb: Assignment,
    pub driver_io: Assignment,
    pub tx_submit: Assignment,
    pub rx_reader: Assignment,
    pub tx_prepare: Assignment,
    pub rx_process: Assignment,
}

impl Assignments {
    fn get(self, role: Role) -> Assignment {
        match role {
            Role::Main => self.main,
            Role::Background => self.background,
            Role::Usb => self.usb,
            Role::DriverIo => self.driver_io,
            Role::TxSubmit => self.tx_submit,
            Role::RxReader => self.rx_reader,
            Role::TxPrepare => self.tx_prepare,
            Role::RxProcess => self.rx_process,
        }
    }
}

impl Plan {
    pub fn new(assignments: Assignments) -> io::Result<Self> {
        for role in ROLES {
            let assignment = assignments.get(role);
            if assignment
                .fifo_priority
                .is_some_and(|value| !(1..=99).contains(&value))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "FIFO priority must be 1..=99",
                ));
            }
        }
        #[cfg(target_os = "linux")]
        {
            let allowed = rustix::thread::sched_getaffinity(None)?;
            for role in ROLES {
                let assignment = assignments.get(role);
                if let Some(cpu) = assignment.cpu {
                    let cpu = cpu as usize;
                    if cpu >= rustix::thread::CpuSet::MAX_CPU || !allowed.is_set(cpu) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("CPU {cpu} is outside the startup affinity mask"),
                        ));
                    }
                }
            }
            Ok(Self {
                assignments,
                allowed,
            })
        }
        #[cfg(target_os = "macos")]
        {
            if ROLES
                .iter()
                .any(|role| assignments.get(*role).cpu.is_some())
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "CPU placement is not available on macOS",
                ));
            }
            Ok(Self { assignments })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = assignments;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "real-time scheduling is supported on Linux and macOS only",
            ))
        }
    }

    pub fn install(self) -> io::Result<()> {
        for role in ROLES {
            tracing::info!(
                "Thread scheduling configured: role={role:?} assignment={:?}",
                self.assignment(role)
            );
        }
        PLAN.set(self).map_err(|_| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "thread scheduling already installed",
            )
        })
    }

    pub fn assignment(&self, role: Role) -> Assignment {
        self.assignments.get(role)
    }
}

pub fn configure_current(role: Role) -> io::Result<()> {
    let Some(plan) = PLAN.get() else {
        return Ok(());
    };
    let name = thread::current().name().unwrap_or("main").to_owned();
    #[cfg(target_os = "linux")]
    let result = linux::apply(plan, role, rustix::thread::gettid(), &name);
    #[cfg(target_os = "macos")]
    let result = darwin::apply_current(plan, role, &name);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "thread scheduling unavailable",
    ));
    if let Err(error) = &result {
        tracing::warn!(
            "Thread scheduling FAILED: thread={name} role={role:?} requested={:?} error={error}",
            plan.assignment(role)
        );
    }
    result
}

pub fn spawn_background(
    name: impl Into<String>,
    run: impl FnOnce() + Send + 'static,
) -> io::Result<thread::JoinHandle<()>> {
    let (ready, started) = std::sync::mpsc::sync_channel(1);
    let handle = thread::Builder::new().name(name.into()).spawn(move || {
        let configured = configure_current(Role::Background);
        let success = configured.is_ok();
        let _ = ready.send(configured);
        if success {
            run();
        }
    })?;
    match started.recv() {
        Ok(Ok(())) => Ok(handle),
        result => {
            let _ = handle.join();
            Err(match result {
                Ok(Err(error)) => error,
                _ => io::Error::other("background thread scheduling disconnected"),
            })
        }
    }
}

pub struct NativeThreads {
    #[cfg(target_os = "linux")]
    before: std::collections::BTreeSet<u32>,
    #[cfg(target_os = "linux")]
    creator_name: String,
    #[cfg(target_os = "macos")]
    before: darwin::Threads,
}

impl NativeThreads {
    pub fn before_open() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                before: linux::threads()?.into_iter().map(|(tid, _)| tid).collect(),
                creator_name: std::fs::read_to_string("/proc/self/comm")?
                    .trim()
                    .to_owned(),
            })
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                before: darwin::Threads::current()?,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Ok(Self {})
        }
    }

    pub fn configure_after_open(self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(plan) = PLAN.get() {
            for (tid, name) in linux::threads()? {
                if self.before.contains(&tid) && !is_driver_service(&name) {
                    continue;
                }
                let role = native_role(&name, &self.creator_name);
                let pid =
                    rustix::process::Pid::from_raw(i32::try_from(tid).map_err(io::Error::other)?)
                        .ok_or_else(|| io::Error::other("invalid native thread ID"))?;
                if let Err(error) = linux::apply(plan, role, pid, &name) {
                    tracing::warn!(
                        "Thread scheduling FAILED: native_tid={tid} thread={name} role={role:?} error={error}"
                    );
                    return Err(error);
                }
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(plan) = PLAN.get() {
            let after = darwin::Threads::current()?;
            for &thread in after.ports() {
                let name = darwin::name(thread)?;
                if self.before.ports().contains(&thread) && !is_driver_service(&name) {
                    continue;
                }
                // Darwin threads start unnamed, unlike Linux threads inheriting the process name.
                let role = native_role(&name, "");
                match darwin::apply(plan, role, thread, &name) {
                    Ok(()) => {}
                    // Unnamed system threads started by the driver open, such as
                    // dispatch workers, can keep the default policy.
                    Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                        tracing::warn!(
                            "Thread scheduling skipped: native_thread={thread:#x} thread={name} role={role:?} error={error}"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            "Thread scheduling FAILED: native_thread={thread:#x} thread={name} role={role:?} error={error}"
                        );
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_driver_service(name: &str) -> bool {
    name == "uhd_log" || name == "libusb_event"
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_role(name: &str, creator: &str) -> Role {
    match name {
        "libusb_event" => Role::Usb,
        "uhd_log" => Role::Background,
        _ if name == creator => Role::DriverIo,
        _ => Role::Background,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn normal_assignments() -> Assignments {
        let normal = Assignment {
            fifo_priority: None,
            cpu: None,
        };
        Assignments {
            main: normal,
            background: normal,
            usb: normal,
            driver_io: normal,
            tx_submit: normal,
            rx_reader: normal,
            tx_prepare: normal,
            rx_process: normal,
        }
    }

    #[test]
    fn invalid_priorities_and_disallowed_cpus_are_rejected_before_install() {
        for priority in [0, 100, u8::MAX] {
            let mut assignments = normal_assignments();
            assignments.tx_prepare.fifo_priority = Some(priority);
            assert!(Plan::new(assignments).is_err());
        }
        let mut assignments = normal_assignments();
        assignments.rx_reader.cpu = Some(u32::MAX);
        assert!(Plan::new(assignments).is_err());
    }

    #[test]
    fn normal_setup_reads_back_exact_policy_and_affinity_without_installing_global_state() {
        thread::spawn(|| {
            let allowed = rustix::thread::sched_getaffinity(None).unwrap();
            let cpu = (0..rustix::thread::CpuSet::MAX_CPU)
                .find(|cpu| allowed.is_set(*cpu))
                .unwrap();
            let mut assignments = normal_assignments();
            assignments.background.cpu = Some(cpu as u32);
            let plan = Plan::new(assignments).unwrap();
            linux::apply(
                &plan,
                Role::Background,
                rustix::thread::gettid(),
                "scheduling-test",
            )
            .unwrap();
            let actual = rustix::thread::sched_getaffinity(None).unwrap();
            assert_eq!(actual.count(), 1);
            assert!(actual.is_set(cpu));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn native_startup_classification_does_not_promote_unrecognized_application_threads() {
        assert_eq!(native_role("uhd_log", "umts-hnb"), Role::Background);
        assert_eq!(native_role("libusb_event", "umts-hnb"), Role::Usb);
        assert_eq!(native_role("umts-hnb", "umts-hnb"), Role::DriverIo);
        assert_eq!(native_role("tokio-runtime-w", "umts-hnb"), Role::Background);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod darwin_tests {
    use super::*;

    fn assignments(fifo_priority: Option<u8>) -> Assignments {
        let assignment = Assignment {
            fifo_priority,
            cpu: None,
        };
        Assignments {
            main: assignment,
            background: assignment,
            usb: assignment,
            driver_io: assignment,
            tx_submit: assignment,
            rx_reader: assignment,
            tx_prepare: assignment,
            rx_process: assignment,
        }
    }

    #[test]
    fn cpu_placement_is_rejected() {
        let mut requested = assignments(None);
        requested.rx_reader.cpu = Some(0);
        assert!(Plan::new(requested).is_err());
    }

    #[test]
    fn time_constraint_policy_rounds_to_whole_ticks_with_half_period_computation() {
        const APPLE_SILICON: darwin::MachTimebaseInfo = darwin::MachTimebaseInfo {
            numer: 125,
            denom: 3,
        };
        const NANOSECONDS: darwin::MachTimebaseInfo =
            darwin::MachTimebaseInfo { numer: 1, denom: 1 };
        let policy = darwin::policy(&APPLE_SILICON).unwrap();
        assert_eq!(
            (policy.period, policy.computation, policy.constraint),
            (16_000, 8_000, 16_000)
        );
        let policy = darwin::policy(&NANOSECONDS).unwrap();
        assert_eq!(
            (policy.period, policy.computation, policy.constraint),
            (666_667, 333_333, 666_667)
        );
    }

    #[test]
    fn time_constraint_readback_allows_one_tick_of_kernel_normalization() {
        let requested = darwin::policy(&darwin::MachTimebaseInfo {
            numer: 125,
            denom: 3,
        })
        .unwrap();
        let mut actual = requested;
        actual.computation += 1;
        assert!(darwin::readback_matches(&requested, &actual));
        actual.computation += 1;
        assert!(!darwin::readback_matches(&requested, &actual));
    }

    #[test]
    fn time_constraint_policy_reads_back_on_the_calling_thread() {
        let plan = Plan::new(assignments(Some(80))).unwrap();
        thread::spawn(move || darwin::apply_current(&plan, Role::TxSubmit, "darwin-test"))
            .join()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn thread_inventory_finds_new_named_threads() {
        const NAME: &str = "darwin-inventory";
        let before = darwin::Threads::current().unwrap();
        let (started, ready) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let worker = thread::Builder::new()
            .name(NAME.into())
            .spawn(move || {
                started.send(()).unwrap();
                let _ = stopped.recv();
            })
            .unwrap();
        ready.recv().unwrap();
        let after = darwin::Threads::current().unwrap();
        let found = after
            .ports()
            .iter()
            .filter(|port| !before.ports().contains(port))
            .any(|port| darwin::name(*port).unwrap() == NAME);
        drop(stop);
        worker.join().unwrap();
        assert!(found);
        assert_eq!(native_role("", ""), Role::DriverIo);
        assert_eq!(native_role("libusb_event", ""), Role::Usb);
    }
}
