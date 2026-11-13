// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::{num::NonZeroU64, os::unix::fs::MetadataExt};
use umts_config::Config;
use umts_types::RadioEpoch;

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("umts-si-{:016x}", getrandom::u64().unwrap()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self) -> PathBuf {
        self.0.join("revision.pb")
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn unchanged_content_survives_reopening_without_replacing_state() {
    let dir = Directory::new();
    let path = dir.file();
    let (first, changed) = select(&path, &[1, 2, 3]).unwrap();
    assert!(changed);
    assert_eq!(first, BroadcastTags::initial());
    let inode = fs::metadata(&path).unwrap().ino();
    let before = fs::read(&path).unwrap();
    let (second, changed) = select(&path, &[1, 2, 3]).unwrap();
    assert!(!changed);
    assert_eq!(second, first);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    let (third, changed) = select(&path, &[1, 3, 2]).unwrap();
    assert!(changed);
    assert_eq!(third, first.next());
    assert_eq!(load(&path).unwrap().unwrap().content, [1, 3, 2]);
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
}

#[test]
fn protobuf_state_has_the_declared_wire_layout() {
    let stored = Stored {
        tags: BroadcastTags::new(4, 2, 2, 4).unwrap(),
        content: vec![1, 2, 3],
    };
    // version=1, nested MIB/SIB tags, then a three-byte broadcast transcript.
    let expected = [
        0x08, 0x01, 0x12, 0x08, 0x08, 0x04, 0x10, 0x02, 0x18, 0x02, 0x20, 0x04, 0x1a, 0x03, 1, 2, 3,
    ];
    assert_eq!(stored.encode().unwrap(), expected);
    let decoded = Stored::decode(&expected).unwrap();
    assert_eq!(decoded.tags, stored.tags);
    assert_eq!(decoded.content, stored.content);
}

fn valid_message() -> proto::SystemInformationState {
    proto::SystemInformationState {
        format_version: FORMAT_VERSION,
        tags: Some(proto::BroadcastValueTags {
            mib: 8,
            sib1: 256,
            sib3: 4,
            sib5: 4,
        }),
        broadcast_transcript: vec![1, 2, 3],
    }
}

#[test]
fn malformed_state_is_not_reinitialized_or_overwritten() {
    let dir = Directory::new();
    let path = dir.file();
    let valid = valid_message().encode_to_vec();
    let mut invalid = vec![
        Vec::new(),
        vec![0xff],
        valid[..valid.len() - 1].to_vec(),
        vec![0; MAX_STATE_BYTES + 1],
    ];
    for version in [0, FORMAT_VERSION + 1, u32::MAX] {
        invalid.push(
            proto::SystemInformationState {
                format_version: version,
                ..valid_message()
            }
            .encode_to_vec(),
        );
    }
    invalid.push(
        proto::SystemInformationState {
            tags: None,
            ..valid_message()
        }
        .encode_to_vec(),
    );
    for content in [Vec::new(), vec![0; MAX_CONTENT_BYTES + 1]] {
        invalid.push(
            proto::SystemInformationState {
                broadcast_transcript: content,
                ..valid_message()
            }
            .encode_to_vec(),
        );
    }
    for bytes in invalid {
        fs::write(&path, &bytes).unwrap();
        assert!(select(&path, &[9]).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    assert!(select(Path::new("relative.pb"), &[1]).is_err());
    assert!(select(&path, &[]).is_err());
    assert!(select(&path, &vec![0; MAX_CONTENT_BYTES + 1]).is_err());
}

#[test]
fn protobuf_value_tags_reject_zero_out_of_range_and_truncating_values() {
    for (mib, sib1, sib3, sib5) in [
        (0, 1, 1, 1),
        (9, 1, 1, 1),
        (256, 1, 1, 1),
        (u32::MAX, 1, 1, 1),
        (1, 0, 1, 1),
        (1, 257, 1, 1),
        (1, 65536, 1, 1),
        (1, 1, 0, 1),
        (1, 1, 5, 1),
        (1, 1, 256, 1),
        (1, 1, 1, 0),
        (1, 1, 1, 5),
        (1, 1, 1, 256),
    ] {
        let bytes = proto::SystemInformationState {
            tags: Some(proto::BroadcastValueTags {
                mib,
                sib1,
                sib3,
                sib5,
            }),
            ..valid_message()
        }
        .encode_to_vec();
        assert!(
            Stored::decode(&bytes).is_err(),
            "{mib}/{sib1}/{sib3}/{sib5}"
        );
    }
    let valid = Stored::decode(&valid_message().encode_to_vec()).unwrap();
    assert_eq!(valid.tags, BroadcastTags::new(8, 256, 4, 4).unwrap());
}

#[test]
fn maximum_broadcast_transcript_survives_persistence() {
    let dir = Directory::new();
    let path = dir.file();
    let content = vec![0xa5; MAX_CONTENT_BYTES];
    let (tags, changed) = select(&path, &content).unwrap();
    assert!(changed);
    let bytes = fs::read(&path).unwrap();
    assert!(bytes.len() <= MAX_STATE_BYTES);
    let message = proto::SystemInformationState::decode(bytes.as_slice()).unwrap();
    assert_eq!(message.format_version, FORMAT_VERSION);
    assert_eq!(message.broadcast_transcript, content);
    assert_eq!(select(&path, &content).unwrap(), (tags, false));
}

#[test]
fn concurrent_update_is_rejected_without_mutating_committed_content() {
    let dir = Directory::new();
    let path = dir.file();
    select(&path, &[1]).unwrap();
    let before = fs::read(&path).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.0.join("revision.pb.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(select(&path, &[2]).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(lock);
    assert!(select(&path, &[2]).unwrap().1);
}

#[test]
fn failed_exclusive_creation_never_owns_or_removes_an_existing_file() {
    let dir = Directory::new();
    let existing = dir.0.join("existing.pending");
    fs::write(&existing, b"preserve existing contents").unwrap();
    assert!(PendingFile::create(existing.clone()).is_err());
    assert_eq!(fs::read(&existing).unwrap(), b"preserve existing contents");
}

#[test]
fn failed_atomic_commit_removes_only_its_owned_pending_file() {
    let dir = Directory::new();
    let path = dir.file();
    fs::create_dir(&path).unwrap();
    assert!(
        persist(
            &path,
            &Stored {
                tags: BroadcastTags::initial(),
                content: vec![1]
            }
        )
        .is_err()
    );
    assert!(path.is_dir());
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
}

fn plan(config: &Config, id: u64) -> PreparedServing {
    PreparedServing::new(config, RadioEpoch::new(NonZeroU64::new(id).unwrap())).unwrap()
}

#[test]
fn actual_broadcast_content_not_epoch_or_unrelated_gain_controls_revision() {
    let dir = Directory::new();
    let mut config: Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99-two-slot.yaml"
    )))
    .unwrap();
    config.cell.broadcast_state_file = dir.file();
    let mut first = plan(&config, 1);
    assert!(!dir.file().exists());
    install(&mut first).unwrap();
    let stored = load(&dir.file()).unwrap().unwrap();
    let original = stored.tags;
    let bch = umts_rrc_codec::BchMessage::decode(
        first
            .cell
            .schedule()
            .message_at(ChipInstant {
                epoch: first.epoch,
                index: 0,
            })
            .unwrap(),
    )
    .unwrap();
    let umts_rrc_codec::BchPayload::CompleteShort { data, .. } = bch.payload else {
        panic!("expected short MIB")
    };
    let mib: umts_rrc_schema::information_elements::MasterInformationBlock =
        rasn::uper::decode(data.bytes()).unwrap();
    assert!(mib.sib_sb_reference_list.0.iter().any(|reference| {
        matches!(
            &reference.sib_sb_type,
            umts_rrc_schema::information_elements::SIBSbTypeAndTag::sysInfoType11(tag)
                if u16::from(tag.0) == original.sib11()
        )
    }));
    assert_eq!(stored.content.len(), MAX_CONTENT_BYTES);
    assert_eq!(
        first.cell.information().sib3().cell_identity,
        config.cell.cell_id
    );
    let time = |epoch| ChipInstant { epoch, index: 0 };
    assert_ne!(
        first.cell.schedule().message_at(time(first.epoch)).unwrap(),
        plan(&config, 1)
            .cell
            .schedule()
            .message_at(time(first.epoch))
            .unwrap()
    );
    let mut next = plan(&config, 2);
    install(&mut next).unwrap();
    assert_eq!(load(&dir.file()).unwrap().unwrap().tags, original);
    config.radio.tx_gain_db -= 1.0;
    install(&mut plan(&config, 3)).unwrap();
    assert_eq!(load(&dir.file()).unwrap().unwrap().tags, original);
    config.cell.cell_id += 1;
    install(&mut plan(&config, 4)).unwrap();
    assert_eq!(load(&dir.file()).unwrap().unwrap().tags, original.next());
    config.cell.advertised_power.cpich_tx_dbm = Some(11);
    install(&mut plan(&config, 5)).unwrap();
    assert_eq!(
        load(&dir.file()).unwrap().unwrap().tags,
        original.next().next()
    );
    config.rrc.periodic_lu_timer_value += 1;
    install(&mut plan(&config, 6)).unwrap();
    assert_eq!(
        load(&dir.file()).unwrap().unwrap().tags,
        original.next().next().next()
    );
}
