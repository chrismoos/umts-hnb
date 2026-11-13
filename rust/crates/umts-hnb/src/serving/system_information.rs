// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{PreparedServing, reference_powers};
use crate::cell::{BroadcastTags, CellPlan};
use anyhow::{Context, Result, ensure};
use prost::Message;
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant, SYSTEM_FRAME_MODULUS};

mod proto {
    include!(concat!(env!("OUT_DIR"), "/umts.hnb.state.v1.rs"));
}

const FORMAT_VERSION: u32 = 1;
const BCH_FRAMES: usize = 2;
const MAX_CONTENT_BYTES: usize =
    SYSTEM_FRAME_MODULUS as usize / BCH_FRAMES * umts_rrc_codec::BCH_BYTES;
// Bounded space for protobuf tags, lengths and revision metadata.
const MAX_STATE_BYTES: usize = MAX_CONTENT_BYTES + 128;

struct Stored {
    tags: BroadcastTags,
    content: Vec<u8>,
}

impl Stored {
    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_STATE_BYTES,
            "system-information state length invalid"
        );
        let state = proto::SystemInformationState::decode(bytes)
            .context("decode system-information protobuf state")?;
        ensure!(
            state.format_version == FORMAT_VERSION,
            "system-information state version invalid"
        );
        let tags = state
            .tags
            .context("system-information value tags missing")?;
        let tags = BroadcastTags::new(
            tags.mib.try_into().context("MIB value tag outside range")?,
            tags.sib1
                .try_into()
                .context("SIB1 value tag outside range")?,
            tags.sib3
                .try_into()
                .context("SIB3 value tag outside range")?,
            tags.sib5
                .try_into()
                .context("SIB5 value tag outside range")?,
        )?;
        ensure!(
            !state.broadcast_transcript.is_empty()
                && state.broadcast_transcript.len() <= MAX_CONTENT_BYTES,
            "system-information content outside bound"
        );
        Ok(Self {
            tags,
            content: state.broadcast_transcript,
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.content.is_empty() && self.content.len() <= MAX_CONTENT_BYTES,
            "system-information content outside bound"
        );
        let state = proto::SystemInformationState {
            format_version: FORMAT_VERSION,
            tags: Some(proto::BroadcastValueTags {
                mib: u32::from(self.tags.mib()),
                sib1: u32::from(self.tags.sib1()),
                sib3: u32::from(self.tags.sib3()),
                sib5: u32::from(self.tags.sib5()),
            }),
            broadcast_transcript: self.content.clone(),
        };
        let bytes = state.encode_to_vec();
        ensure!(
            bytes.len() <= MAX_STATE_BYTES,
            "system-information state outside bound"
        );
        Ok(bytes)
    }
}

struct PendingFile(Option<PathBuf>);

impl PendingFile {
    fn create(path: PathBuf) -> Result<(Self, File)> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| {
                format!("create pending system-information state {}", path.display())
            })?;
        Ok((Self(Some(path)), file))
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn load(path: &Path) -> Result<Option<Stored>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open system-information state"),
    };
    ensure!(
        file.metadata()?.is_file(),
        "system-information state must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take((MAX_STATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Stored::decode(&bytes)
        .map(Some)
        .context("read system-information state")
}

fn persist(path: &Path, state: &Stored) -> Result<()> {
    let bytes = state.encode()?;
    let parent = path
        .parent()
        .context("system-information state parent missing")?;
    let mut name = path
        .file_name()
        .context("system-information state filename missing")?
        .to_os_string();
    name.push(format!(".{:016x}.pending", getrandom::u64()?));
    let pending_path = parent.join(name);
    let (mut pending, mut file) = PendingFile::create(pending_path.clone())?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&pending_path, path).context("commit system-information state")?;
    pending.0 = None;
    File::open(parent)?
        .sync_all()
        .context("sync system-information state directory")
}

fn select(path: &Path, content: &[u8]) -> Result<(BroadcastTags, bool)> {
    ensure!(
        path.is_absolute() && path.file_name().is_some(),
        "system-information state requires an absolute file path"
    );
    ensure!(
        !content.is_empty() && content.len() <= MAX_CONTENT_BYTES,
        "system-information content outside bound"
    );
    let parent = path
        .parent()
        .context("system-information state parent missing")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "create system-information state directory {}",
            parent.display()
        )
    })?;
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(PathBuf::from(lock_name))
        .context("open system-information state lock")?;
    lock.try_lock()
        .context("system-information state is already being updated")?;
    let stored = load(path)?;
    if let Some(stored) = &stored
        && stored.content == content
    {
        return Ok((stored.tags, false));
    }
    let tags = stored.map_or_else(BroadcastTags::initial, |stored| stored.tags.next());
    persist(
        path,
        &Stored {
            tags,
            content: content.to_vec(),
        },
    )?;
    Ok((tags, true))
}

pub(super) fn install(plan: &mut PreparedServing) -> Result<()> {
    let mut content = Vec::with_capacity(MAX_CONTENT_BYTES);
    for frame in (0..SYSTEM_FRAME_MODULUS).step_by(BCH_FRAMES) {
        let time = ChipInstant {
            epoch: plan.epoch,
            index: frame * CHIPS_PER_FRAME,
        };
        content.extend_from_slice(
            plan.cell
                .schedule()
                .message_at(time)
                .map_err(|error| anyhow::anyhow!("system-information transcript: {error:?}"))?,
        );
    }
    let (tags, changed) = select(&plan.config.cell.broadcast_state_file, &content)?;
    plan.cell = CellPlan::compile_tagged(
        &plan.config,
        plan.epoch,
        tags,
        reference_powers(&plan.config)?,
        super::indicator_powers(&plan.config, plan.downlink_power.as_ref()),
    )?;
    tracing::info!(
        "System-information tags: mib={} sib1={} sib3={} sib5={} content_changed={} state={}",
        tags.mib(),
        tags.sib1(),
        tags.sib3(),
        tags.sib5(),
        changed,
        plan.config.cell.broadcast_state_file.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
