// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(unsafe_code)]

use super::{Plan, Role};
use std::{ffi::CStr, io, mem::size_of};

// One UMTS slot: 2560 chips at 3.84 Mcps.
pub(super) const PERIOD_NS: u64 = 666_667;
pub(super) const COMPUTATION_NS: u64 = PERIOD_NS / 2;
pub(super) const CONSTRAINT_NS: u64 = PERIOD_NS;
// The kernel may store a value one Mach tick from the request.
const READBACK_TOLERANCE_TICKS: u32 = 1;
const KERN_SUCCESS: libc::kern_return_t = 0;
const MAXIMUM_NATIVE_THREADS: usize = 256;

#[repr(C)]
pub(super) struct MachTimebaseInfo {
    pub(super) numer: u32,
    pub(super) denom: u32,
}

unsafe extern "C" {
    static mach_task_self_: libc::mach_port_t;
    fn mach_thread_self() -> libc::mach_port_t;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::kern_return_t;
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

fn task() -> libc::mach_port_t {
    // SAFETY: libSystem initializes mach_task_self_ before any Rust code runs and never changes it.
    unsafe { mach_task_self_ }
}

fn kernel(operation: &str, result: libc::kern_return_t) -> io::Result<()> {
    if result == KERN_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{operation} failed: kern_return={result}"
        )))
    }
}

/// Send rights to task threads, released on drop. A thread keeps one port name
/// within the task, so two inventories compare by name.
pub(super) struct Threads(Vec<libc::thread_act_t>);

impl Threads {
    pub(super) fn current() -> io::Result<Self> {
        let mut list: libc::thread_act_array_t = std::ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        // SAFETY: Both outputs refer to live locals. task_threads fills them for this task.
        kernel("task_threads", unsafe {
            libc::task_threads(task(), &mut list, &mut count)
        })?;
        let ports = if list.is_null() {
            Vec::new()
        } else {
            // SAFETY: task_threads returned `count` initialized port names at `list`.
            let ports = unsafe { std::slice::from_raw_parts(list, count as usize) }.to_vec();
            // SAFETY: The array was allocated in this task by task_threads and is not used again.
            unsafe {
                libc::vm_deallocate(
                    task(),
                    list as libc::vm_address_t,
                    count as libc::vm_size_t * size_of::<libc::thread_act_t>(),
                );
            }
            ports
        };
        let threads = Self(ports);
        if threads.0.len() > MAXIMUM_NATIVE_THREADS {
            return Err(io::Error::other(
                "native thread inventory exceeds startup bound",
            ));
        }
        Ok(threads)
    }

    pub(super) fn ports(&self) -> &[libc::thread_act_t] {
        &self.0
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        for &port in &self.0 {
            // SAFETY: Each name holds one send right acquired by task_threads.
            unsafe {
                mach_port_deallocate(task(), port);
            }
        }
    }
}

pub(super) fn name(thread: libc::thread_act_t) -> io::Result<String> {
    let mut info = libc::thread_extended_info::default();
    let mut count = libc::THREAD_EXTENDED_INFO_COUNT;
    // SAFETY: `info` is a live thread_extended_info and `count` is its size in integers.
    kernel("thread_info", unsafe {
        libc::thread_info(
            thread,
            libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
            std::ptr::from_mut(&mut info).cast(),
            &mut count,
        )
    })?;
    // SAFETY: The kernel NUL-terminates pth_name within its fixed buffer.
    let name = unsafe { CStr::from_ptr(info.pth_name.as_ptr()) };
    Ok(name.to_string_lossy().into_owned())
}

pub(super) fn apply_current(plan: &Plan, role: Role, name: &str) -> io::Result<()> {
    // SAFETY: mach_thread_self has no preconditions and returns a send right for this thread.
    let thread = unsafe { mach_thread_self() };
    let result = apply(plan, role, thread, name);
    // SAFETY: Releases the send right acquired by mach_thread_self above.
    unsafe {
        mach_port_deallocate(task(), thread);
    }
    result
}

pub(super) fn apply(
    plan: &Plan,
    role: Role,
    thread: libc::thread_act_t,
    name: &str,
) -> io::Result<()> {
    if plan.assignment(role).fifo_priority.is_none() {
        tracing::info!("Thread scheduling OK: thread={name} role={role:?} policy=default");
        return Ok(());
    }
    let requested = time_constraint()?;
    // SAFETY: The policy is a live thread_time_constraint_policy of the declared count.
    kernel("thread_policy_set", unsafe {
        libc::thread_policy_set(
            thread,
            libc::THREAD_TIME_CONSTRAINT_POLICY as libc::thread_policy_flavor_t,
            std::ptr::from_ref(&requested).cast_mut().cast(),
            libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    })?;
    let mut actual = libc::thread_time_constraint_policy {
        period: 0,
        computation: 0,
        constraint: 0,
        preemptible: 0,
    };
    let mut count = libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT;
    let mut default: libc::boolean_t = 0;
    // SAFETY: All outputs refer to live locals sized for this policy flavor.
    kernel("thread_policy_get", unsafe {
        libc::thread_policy_get(
            thread,
            libc::THREAD_TIME_CONSTRAINT_POLICY as libc::thread_policy_flavor_t,
            std::ptr::from_mut(&mut actual).cast(),
            &mut count,
            &mut default,
        )
    })?;
    if default != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel kept the default policy after accepting time-constraint",
        ));
    }
    if !readback_matches(&requested, &actual) {
        return Err(io::Error::other(format!(
            "time-constraint readback mismatch: default={default} period={} computation={} constraint={}",
            actual.period, actual.computation, actual.constraint
        )));
    }
    tracing::info!(
        "Thread scheduling OK: thread={name} role={role:?} policy=time-constraint period_ns={PERIOD_NS} computation_ns={COMPUTATION_NS} constraint_ns={CONSTRAINT_NS}"
    );
    Ok(())
}

fn time_constraint() -> io::Result<libc::thread_time_constraint_policy> {
    let mut timebase = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: The output refers to a live mach_timebase_info.
    kernel("mach_timebase_info", unsafe {
        mach_timebase_info(&mut timebase)
    })?;
    policy(&timebase)
}

pub(super) fn policy(
    timebase: &MachTimebaseInfo,
) -> io::Result<libc::thread_time_constraint_policy> {
    if timebase.numer == 0 {
        return Err(io::Error::other("invalid Mach timebase"));
    }
    let absolute = |nanoseconds: u64| {
        let numer = u128::from(timebase.numer);
        u32::try_from((u128::from(nanoseconds) * u128::from(timebase.denom) + numer / 2) / numer)
            .map_err(|_| io::Error::other("time constraint exceeds Mach absolute range"))
    };
    let period = absolute(PERIOD_NS)?;
    Ok(libc::thread_time_constraint_policy {
        period,
        computation: period / 2,
        constraint: absolute(CONSTRAINT_NS)?,
        preemptible: 1,
    })
}

pub(super) fn readback_matches(
    requested: &libc::thread_time_constraint_policy,
    actual: &libc::thread_time_constraint_policy,
) -> bool {
    [
        (requested.period, actual.period),
        (requested.computation, actual.computation),
        (requested.constraint, actual.constraint),
    ]
    .into_iter()
    .all(|(requested, actual)| requested.abs_diff(actual) <= READBACK_TOLERANCE_TICKS)
}
