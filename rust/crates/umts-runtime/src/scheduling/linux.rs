// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(unsafe_code)]

use super::{Plan, Role};
use rustix::{
    process::Pid,
    thread::{CpuSet, sched_getaffinity, sched_setaffinity},
};
use std::{fs, io};

const MAXIMUM_NATIVE_THREADS: usize = 256;

pub(super) fn apply(plan: &Plan, role: Role, tid: Pid, name: &str) -> io::Result<()> {
    let assignment = plan.assignment(role);
    let mut cpus = plan.allowed;
    match assignment.cpu {
        Some(super::Placement::Cpu(cpu)) => {
            cpus = CpuSet::new();
            cpus.set(cpu as usize);
        }
        Some(super::Placement::Performance) => cpus = plan.performance,
        None => (),
    }
    sched_setaffinity(Some(tid), &cpus)?;
    let requested_policy = if assignment.fifo_priority.is_some() {
        libc::SCHED_FIFO
    } else {
        libc::SCHED_OTHER
    };
    let requested = libc::sched_param {
        sched_priority: i32::from(assignment.fifo_priority.unwrap_or(0)),
    };
    // Linux sched_* takes a kernel TID, not an opaque pthread_t.
    let result = unsafe {
        libc::sched_setscheduler(tid.as_raw_nonzero().get(), requested_policy, &requested)
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut actual = libc::sched_param { sched_priority: 0 };
    // The output pointer remains valid for this synchronous syscall.
    let result = unsafe { libc::sched_getparam(tid.as_raw_nonzero().get(), &mut actual) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // This syscall reads scheduling state for the same live kernel TID.
    let policy = unsafe { libc::sched_getscheduler(tid.as_raw_nonzero().get()) };
    if policy < 0 {
        return Err(io::Error::last_os_error());
    }
    let affinity = sched_getaffinity(Some(tid))?;
    if policy != requested_policy
        || actual.sched_priority != requested.sched_priority
        || affinity != cpus
    {
        return Err(io::Error::other(format!(
            "scheduling readback mismatch: policy={policy} priority={} affinity={affinity:?}",
            actual.sched_priority
        )));
    }
    tracing::info!(
        "Thread scheduling OK: tid={tid} thread={name} role={role:?} policy={} priority={} affinity={affinity:?}",
        if policy == libc::SCHED_FIFO {
            "FIFO"
        } else {
            "OTHER"
        },
        actual.sched_priority
    );
    Ok(())
}

pub(super) fn threads() -> io::Result<Vec<(u32, String)>> {
    let mut result = Vec::new();
    for entry in fs::read_dir("/proc/self/task")? {
        if result.len() == MAXIMUM_NATIVE_THREADS {
            return Err(io::Error::other(
                "native thread inventory exceeds startup bound",
            ));
        }
        let entry = entry?;
        let tid = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| io::Error::other("invalid task directory"))?;
        match fs::read_to_string(entry.path().join("comm")) {
            Ok(name) => result.push((tid, name.trim().to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(result)
}

pub(super) fn maximum_frequency(cpu: usize) -> Option<u64> {
    std::fs::read_to_string(format!(
        "/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq"
    ))
    .ok()?
    .trim()
    .parse()
    .ok()
}

/// The allowed CPUs that share the highest maximum frequency. Without frequency information
/// every allowed CPU counts as equally fast.
pub(super) fn fastest(allowed: &CpuSet, frequency: impl Fn(usize) -> Option<u64>) -> CpuSet {
    let rated: Vec<(usize, u64)> = (0..CpuSet::MAX_CPU)
        .filter(|cpu| allowed.is_set(*cpu))
        .filter_map(|cpu| Some((cpu, frequency(cpu)?)))
        .collect();
    let Some(top) = rated.iter().map(|(_, value)| *value).max() else {
        return *allowed;
    };
    let mut fastest = CpuSet::new();
    for (cpu, value) in rated {
        if value == top {
            fastest.set(cpu);
        }
    }
    fastest
}
