// SPDX-License-Identifier: AGPL-3.0-or-later

#[path = "../../../test-support/rel99_dedicated.rs"]
mod fixture;
use std::{
    env,
    fmt::Write,
    fs,
    num::{NonZeroU8, NonZeroU64},
    path::PathBuf,
    sync::Arc,
};
use umts_rrc::{
    dedicated::{
        ExistingDchRelease, ExistingDchSetup, InitialDchSetup, Rel99Configuration,
        ScheduledActivation,
    },
    establishment::TransactionId,
};
use umts_types::{RadioEpoch, time::ChipInstant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let path = PathBuf::from(arguments.next().ok_or("Usage: rel99_wire <output.hex>")?);
    if arguments.next().is_some() {
        return Err("Usage: rel99_wire <output.hex>".into());
    }
    let build = || -> Result<_, umts_rrc::dedicated::DedicatedError> {
        let transaction = TransactionId::new(2).expect("wire transaction");
        let mut packets = Vec::new();
        for (voice, packet) in [(true, false), (false, true), (true, true)] {
            packets.push(
                InitialDchSetup::new(
                    fixture::configuration(voice, packet, 10),
                    fixture::retained(),
                    fixture::rabs(voice, packet),
                )?
                .message(transaction)?,
            );
        }
        let activation = ScheduledActivation::new(
            ChipInstant {
                epoch: RadioEpoch::new(NonZeroU64::MIN),
                index: 0,
            },
            0,
            NonZeroU8::new(4).expect("activation lead"),
        )?;
        packets.push(
            ExistingDchSetup::new(
                fixture::configuration(true, false, 10),
                fixture::configuration(true, true, 10),
                fixture::rabs(false, true),
                activation,
            )?
            .message(transaction)?,
        );
        packets.push(
            ExistingDchSetup::new(
                fixture::configuration(false, true, 10),
                fixture::configuration(true, true, 10),
                fixture::rabs(true, false),
                activation,
            )?
            .message(transaction)?,
        );
        packets.push(
            ExistingDchSetup::new(
                fixture::configuration(true, false, 10),
                fixture::configuration(true, true, 11),
                fixture::rabs(false, true),
                activation,
            )?
            .message(transaction)?,
        );
        let (ul, dl, bearers, mut physical) = fixture::components(true, true, 10);
        physical.downlink_codes = vec![4];
        let changed = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical)?);
        packets.push(
            ExistingDchSetup::new(
                fixture::configuration(true, false, 10),
                changed,
                fixture::rabs(false, true),
                activation,
            )?
            .message(transaction)?,
        );
        for (voice, packet) in [(false, true), (true, false), (false, false)] {
            packets.push(
                ExistingDchRelease::new(
                    fixture::configuration(true, true, 10),
                    fixture::configuration(voice, packet, 10),
                    activation,
                )?
                .message(transaction)?,
            );
        }
        let (ul, dl, bearers, mut physical) = fixture::components(false, true, 11);
        physical.downlink_codes = vec![4];
        packets.push(
            ExistingDchRelease::new(
                fixture::configuration(true, true, 10),
                Arc::new(Rel99Configuration::new(ul, dl, bearers, physical)?),
                activation,
            )?
            .message(transaction)?,
        );
        Ok(packets)
    };
    let packets = build().map_err(|error| format!("Rel99 construction failed: {error:?}"))?;
    let mut output = String::new();
    for (index, encoded) in packets.iter().enumerate() {
        let mut bytes = vec![2, 4, 0x0c, 0, 0, 0, 0, 0, 0, 0, 0, index as u8, 0, 0, 0, 0];
        bytes.extend_from_slice(&encoded.bytes);
        for (chunk, bytes) in bytes.chunks(16).enumerate() {
            write!(output, "{:04x}", chunk * 16)?;
            for byte in bytes {
                write!(output, " {byte:02x}")?;
            }
            output.push('\n');
        }
        output.push('\n');
    }
    fs::write(&path, output)?;
    println!(
        "Wrote {} bounded Rel99 wire probes to {}",
        packets.len(),
        path.display()
    );
    Ok(())
}
