// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use records::{Assembler, Pending, notification};
use std::num::NonZeroU16;

fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn metadata() -> Metadata {
    Metadata {
        stream: 1,
        sequence: u16::MAX,
        ppid: 19,
        association: 42,
    }
}
fn limits() -> Limits {
    Limits {
        maximum_record_bytes: count(4096),
        receive_chunk_bytes: count(128),
        outbound_streams: NonZeroU16::new(2).unwrap(),
        inbound_streams: NonZeroU16::new(2).unwrap(),
    }
}

#[test]
fn backend_selection_is_explicit_without_fallback() {
    assert_eq!(Backend::default(), Backend::Native);
    assert_eq!(
        Backend::Native.validate().is_ok(),
        cfg!(target_os = "linux")
    );
    assert_eq!(
        Backend::Usrsctp { udp: None }.validate().is_ok(),
        cfg!(feature = "usrsctp")
    );
}
#[test]
fn local_address_must_be_specific_and_match_the_peer_family() {
    let peer = "192.0.2.1:29169".parse().unwrap();
    for local in ["0.0.0.0", "2001:db8::1"] {
        assert!(matches!(
            Association::connect_from(
                Backend::Native,
                Some(local.parse().unwrap()),
                peer,
                limits()
            ),
            Err(Error::Configuration)
        ));
    }
}
#[test]
fn reassembly_keeps_full_record_until_upper_admission() {
    let mut assembler = Assembler::new(count(8)).unwrap();
    assembler.append(metadata(), &[1, 2], false).unwrap();
    assert_eq!(assembler.pending(), None);
    assembler.append(metadata(), &[3, 4], true).unwrap();
    assert_eq!(assembler.pending(), Some(Pending::Record(metadata())));
    assert_eq!(assembler.bytes(), &[1, 2, 3, 4]);
    assert!(matches!(
        assembler.append(metadata(), &[5], true),
        Err(Error::State)
    ));
    assert_eq!(assembler.bytes(), &[1, 2, 3, 4]);
    assembler.acknowledge().unwrap();
    assert_eq!(assembler.pending(), None);
    assert!(assembler.bytes().is_empty());
    let next = Metadata {
        sequence: 0,
        ..metadata()
    };
    assembler.append(next, &[5], true).unwrap();
    assert_eq!(assembler.pending(), Some(Pending::Record(next)));
}
#[test]
fn partial_records_cannot_cross_stream_sequence_ppid_or_association() {
    for changed in [
        Metadata {
            stream: 0,
            ..metadata()
        },
        Metadata {
            sequence: 0,
            ..metadata()
        },
        Metadata {
            ppid: 20,
            ..metadata()
        },
        Metadata {
            association: 43,
            ..metadata()
        },
    ] {
        let mut assembler = Assembler::new(count(8)).unwrap();
        assembler.append(metadata(), &[1], false).unwrap();
        assert!(matches!(
            assembler.append(changed, &[2], true),
            Err(Error::RecordBoundary)
        ));
        assert_eq!(assembler.bytes(), &[1]);
    }
}
#[test]
fn reassembly_resource_limits_precede_mutation() {
    let mut assembler = Assembler::new(count(3)).unwrap();
    assembler.append(metadata(), &[1, 2], false).unwrap();
    assert!(matches!(
        assembler.append(metadata(), &[3, 4], true),
        Err(Error::Capacity)
    ));
    assert_eq!(assembler.bytes(), &[1, 2]);
    assert_eq!(assembler.pending(), None);
    assert!(matches!(
        assembler.append(metadata(), &[], true),
        Err(Error::RecordBoundary)
    ));
    assert!(matches!(assembler.acknowledge(), Err(Error::State)));
}
fn native_notification(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut bytes = kind.to_ne_bytes().to_vec();
    bytes.extend_from_slice(&0u16.to_ne_bytes());
    bytes.extend_from_slice(&(u32::try_from(body.len()).unwrap() + 8).to_ne_bytes());
    bytes.extend_from_slice(body);
    bytes
}
fn association_change(state: u16) -> Vec<u8> {
    let mut body = state.to_ne_bytes().to_vec();
    body.extend_from_slice(&0u16.to_ne_bytes());
    body.extend_from_slice(&2u16.to_ne_bytes());
    body.extend_from_slice(&3u16.to_ne_bytes());
    body.extend_from_slice(&42i32.to_ne_bytes());
    native_notification(0x8001, &body)
}
#[test]
fn native_notifications_preserve_stream_counts_and_require_new_owner_after_restart() {
    assert_eq!(
        notification(&association_change(0)).unwrap(),
        Notification::Established {
            outbound_streams: 2,
            inbound_streams: 3,
            association: 42,
        }
    );
    for state in [1, 2, 3, 4] {
        let notification = notification(&association_change(state)).unwrap();
        assert_eq!(
            notification,
            Notification::Lost {
                state,
                error: 0,
                association: 42
            }
        );
        assert!(notification.invalidates_association());
    }
    assert!(notification(&association_change(5)).is_err());
    assert_eq!(
        notification(&native_notification(0x8005, &42i32.to_ne_bytes())).unwrap(),
        Notification::Shutdown { association: 42 }
    );
    let mut partial = 0u32.to_ne_bytes().to_vec();
    partial.extend_from_slice(&42i32.to_ne_bytes());
    partial.extend_from_slice(&1u32.to_ne_bytes());
    partial.extend_from_slice(&3u32.to_ne_bytes());
    assert_eq!(
        notification(&native_notification(0x8006, &partial)).unwrap(),
        Notification::PartialDeliveryAborted { association: 42 }
    );
}
#[test]
fn notification_lengths_and_unknown_kinds_remain_distinct() {
    let bytes = association_change(0);
    for prefix in 0..bytes.len() {
        assert!(notification(&bytes[..prefix]).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(notification(&trailing).is_err());
    let unknown = notification(&native_notification(0xfffe, &[])).unwrap();
    assert_eq!(unknown, Notification::Unknown { kind: 0xfffe });
    assert!(!unknown.invalidates_association());
    assert!(notification(&native_notification(0x8006, &[0; 4])).is_err());
}
#[test]
fn informational_notifications_do_not_discard_incomplete_records() {
    let mut assembler = Assembler::new(count(8)).unwrap();
    assembler.append(metadata(), &[1], false).unwrap();
    assembler.set_pending(Pending::Notification(Notification::Unknown {
        kind: 0xfffe,
    }));
    assembler.acknowledge().unwrap();
    assembler.append(metadata(), &[2], true).unwrap();
    assert_eq!(assembler.bytes(), &[1, 2]);
    assembler.acknowledge().unwrap();
    assembler.append(metadata(), &[3], false).unwrap();
    assembler.set_pending(Pending::Notification(
        Notification::PartialDeliveryAborted { association: 42 },
    ));
    assembler.acknowledge().unwrap();
    assert!(assembler.bytes().is_empty());
}
#[test]
fn invalid_limits_fail_before_any_socket_is_opened() {
    let peer = "127.0.0.1:1".parse().unwrap();
    for invalid in [
        Limits {
            receive_chunk_bytes: count(1),
            ..limits()
        },
        Limits {
            maximum_record_bytes: count(64),
            ..limits()
        },
    ] {
        assert!(matches!(
            Association::connect(peer, invalid),
            Err(Error::Configuration)
        ));
    }
}
