// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, anyhow, ensure};
use std::{fs::File, io::Write, num::NonZeroU64, path::PathBuf};
use umts_config::Config;
use umts_hnb::cell::{CellPlan, ReferencePowers};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

const PERIOD_FRAMES: u64 = 32;
const BCH_INTERVAL_FRAMES: usize = 2;
const HEX_LINE_BYTES: usize = 16;

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(arguments.len() == 2, "broadcast_wire CONFIG NEW_HEX_OUTPUT");
    let config = Config::load(&PathBuf::from(&arguments[0]))?;
    let settings = &config.cell.advertised_power;
    let powers = ReferencePowers {
        cpich_tx_dbm: settings
            .cpich_tx_dbm
            .context("explicit CPICH power required")?,
        uplink_interference_dbm: settings
            .uplink_interference_dbm
            .context("explicit uplink interference required")?,
    };
    let epoch = RadioEpoch::new(NonZeroU64::MIN);
    let plan = CellPlan::compile(&config, epoch, CellPlan::CURRENT_MIB_VALUE_TAG, powers)?;
    let mut output = File::options()
        .write(true)
        .create_new(true)
        .open(&arguments[1])?;
    for frame in (0..PERIOD_FRAMES).step_by(BCH_INTERVAL_FRAMES) {
        let value = plan
            .schedule()
            .message_at(ChipInstant {
                epoch,
                index: frame * CHIPS_PER_FRAME,
            })
            .map_err(|error| anyhow!("BCH frame {frame}: {error:?}"))?;
        for (line, bytes) in value.chunks(HEX_LINE_BYTES).enumerate() {
            write!(output, "{:04x}", line * HEX_LINE_BYTES)?;
            for byte in bytes {
                write!(output, " {byte:02x}")?;
            }
            writeln!(output)?;
        }
        writeln!(output)?;
    }
    Ok(())
}
