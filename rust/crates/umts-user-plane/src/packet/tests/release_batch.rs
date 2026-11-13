// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn prepared_batch_drop_preserves_routes_queues_and_sequences() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let route = plane
        .prepare_with_state(
            key(0),
            peer(),
            PeerTeid::new(9),
            SequenceState {
                next_uplink: 17,
                next_downlink: Some(23),
            },
        )
        .unwrap();
    plane.activate(route).unwrap();
    plane.enqueue_uplink(route, b"retained packet").unwrap();
    let before = plane.sequence_state(route).unwrap();
    let counters = plane.counters();
    let guard = plane.prepare_release_batch(vec![route]).unwrap();
    assert_eq!(guard.handles(), [route]);
    drop(guard);
    assert_eq!(plane.phase(route).unwrap(), Phase::Active);
    assert_eq!(plane.sequence_state(route).unwrap(), before);
    assert_eq!(plane.counters(), counters);
    assert_eq!(
        plane
            .step(|_, _| DownlinkDisposition::Accepted)
            .unwrap()
            .sent,
        1
    );
}

#[test]
fn foreign_and_duplicate_batch_handles_cannot_partially_release_an_owner() {
    let mut first = PacketPlane::new(Memory::default(), limits()).unwrap();
    let mut second = PacketPlane::new(Memory::default(), limits()).unwrap();
    let route = first.prepare(key(0), peer(), PeerTeid::new(9)).unwrap();
    let foreign = second.prepare(key(0), peer(), PeerTeid::new(9)).unwrap();
    for handles in [vec![route, foreign], vec![route, route]] {
        assert!(matches!(
            first.prepare_release_batch(handles),
            Err(Error::Identity)
        ));
        assert_eq!(first.phase(route).unwrap(), Phase::Prepared);
    }
    assert_eq!(second.phase(foreign).unwrap(), Phase::Prepared);
}

#[test]
fn committed_batch_releases_only_reserved_routes_and_counts_queued_packets() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let routes: Vec<_> = (0..3)
        .map(|slot| plane.prepare(key(slot), peer(), PeerTeid::new(9)).unwrap())
        .collect();
    for route in &routes {
        plane.activate(*route).unwrap();
        plane.enqueue_uplink(*route, b"queued").unwrap();
    }
    let remaining = plane.sequence_state(routes[2]).unwrap();
    plane
        .prepare_release_batch(routes[..2].to_vec())
        .unwrap()
        .commit();
    assert!(matches!(plane.phase(routes[0]), Err(Error::Identity)));
    assert!(matches!(plane.phase(routes[1]), Err(Error::Identity)));
    assert_eq!(plane.phase(routes[2]).unwrap(), Phase::Active);
    assert_eq!(plane.sequence_state(routes[2]).unwrap(), remaining);
    assert_eq!(plane.counters().retired_packets, 2);
    assert_eq!(
        plane
            .step(|_, _| DownlinkDisposition::Accepted)
            .unwrap()
            .sent,
        1
    );
}
