// SPDX-License-Identifier: AGPL-3.0-or-later

use std::io;
use umts_config::ThreadSchedulingConfig;
use umts_runtime::scheduling::{Assignment, Assignments, Placement, Plan, Role, configure_current};

pub fn install(settings: &ThreadSchedulingConfig) -> io::Result<()> {
    if !settings.enabled {
        tracing::info!("Thread scheduling disabled by runtime.scheduling.enabled");
        return Ok(());
    }
    let place = |cpu: Option<umts_config::CpuPlacement>| {
        cpu.map(|cpu| match cpu {
            umts_config::CpuPlacement::Index(index) => Placement::Cpu(index),
            umts_config::CpuPlacement::Performance => Placement::Performance,
        })
    };
    let normal = |cpu| Assignment {
        fifo_priority: None,
        cpu: place(cpu),
    };
    let fifo = |cpu, priority| Assignment {
        fifo_priority: Some(priority),
        cpu: place(cpu),
    };
    Plan::new(Assignments {
        main: normal(settings.main_cpu),
        background: normal(settings.control_cpu),
        usb: fifo(settings.usb_cpu, settings.usb_priority),
        driver_io: fifo(settings.control_cpu, settings.usb_priority),
        tx_submit: fifo(settings.submit_cpu, settings.submit_priority),
        rx_reader: fifo(settings.reader_cpu, settings.reader_priority),
        tx_prepare: fifo(settings.prepare_cpu, settings.prepare_priority),
        rx_process: fifo(settings.receive_cpu, settings.receive_priority),
        rx_decode: fifo(settings.decode_cpu, settings.decode_priority),
    })?
    .install()?;
    configure_current(Role::Main)
}
