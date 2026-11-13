// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    signalling::tests::controller,
    test_support::{at, source},
};

pub(crate) fn request(imsi: &str) -> Request {
    Request {
        domain: umts_iuh::Domain::Ps,
        imsi: Imsi::from_digits(imsi).unwrap(),
        temporary: Some(TemporaryIdentity::Ptmsi([0x12, 0x34, 0x56, 0x78])),
        area: None,
        cause: Some(Cause::Background),
        drx_coefficient: Some(6),
        search_other_domain: true,
    }
}

#[test]
fn domain_page_waits_until_its_previous_connection_release_finishes() {
    for domain in [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps] {
        let mut source = source(false);
        let mut radio = controller(&source);
        let (mut core, peer) = crate::core_link::tests::ready(&mut radio);
        let (key, context, _) =
            crate::core_link::tests::register(&mut core, &peer, &mut radio, &source, 0, 3);
        let mut page = request("001010123456789");
        page.domain = domain;
        radio.registry.get_mut(key).unwrap().core_imsi = Some(page.imsi);
        peer.rua(umts_iuh::rua::Message::Disconnect {
            context,
            domain: umts_iuh::Domain::Cs,
            cause: umts_iuh::rua::Cause::Radio(umts_iuh::rua::RadioCause::NetworkRelease),
            ranap: None,
        });
        let mut release = None;
        core.drive(umts_iuh::session::ControlTime(6), &mut radio, |_, event| {
            let umts_iuh::session::Event::Disconnected { request, .. } = event else {
                panic!("release expected")
            };
            release = Some(*request);
            Ok::<_, ()>(())
        })
        .unwrap();
        let release = release.unwrap();
        radio.receive_paging(&page);
        radio.service_paging(&mut source, at(0), 0);
        assert_eq!(
            radio.counters().paging.dedicated_queued,
            u64::from(domain == umts_iuh::Domain::Ps)
        );
        assert_eq!(
            radio.counters().paging.dedicated_blocked,
            u64::from(domain == umts_iuh::Domain::Cs)
        );
        radio.finish_core_connection_release(release);
        for _ in 0..radio.paging.entries.len() {
            radio.service_paging(&mut source, at(0), 0);
        }
        assert_eq!(radio.counters().paging.dedicated_queued, 1);
    }
}

#[test]
fn admitted_dedicated_page_refreshes_only_its_owners_activity_once() {
    let mut source = source(false);
    let mut radio = controller(&source);
    let page = request("001010123456789");
    let key = crate::signalling::tests::establish(&mut radio, &source, 0);
    let peer = crate::signalling::tests::establish(&mut radio, &source, 1);
    radio.registry.get_mut(key).unwrap().core_imsi = Some(page.imsi);
    let before = radio.registry.get(key).unwrap().activity;
    let peer_before = radio.registry.get(peer).unwrap().activity;
    radio.receive_paging(&page);
    assert_eq!(radio.registry.get(key).unwrap().activity, before);
    radio.service_paging(&mut source, at(0), 0);
    assert_eq!(radio.registry.get(key).unwrap().activity, before + 1);
    assert_eq!(radio.registry.get(peer).unwrap().activity, peer_before);
    radio.service_paging(&mut source, at(0), 0);
    assert_eq!(radio.registry.get(key).unwrap().activity, before + 1);
    assert_eq!(radio.counters().paging.dedicated_queued, 1);
}

#[test]
fn releasing_context_does_not_capture_a_successors_page() {
    for successor in [false, true] {
        let mut source = source(false);
        let mut radio = controller(&source);
        let page = request("001010123456789");
        let old = crate::signalling::tests::establish(&mut radio, &source, 0);
        radio.registry.get_mut(old).unwrap().core_imsi = Some(page.imsi);
        radio
            .release_idle_rrc(
                old,
                umts_rrc::mobility::ReleaseCompleteRetries::new(1).unwrap(),
            )
            .unwrap();
        let next = successor.then(|| {
            let key = crate::signalling::tests::establish(&mut radio, &source, 1);
            radio.registry.get_mut(key).unwrap().core_imsi = Some(page.imsi);
            key
        });
        radio.receive_paging(&page);
        radio.service_paging(&mut source, at(0), 0);
        assert!(!radio.registry.get(old).unwrap().control.pending_new_data());
        if let Some(next) = next {
            assert!(radio.registry.get(next).unwrap().control.pending_new_data());
            assert_eq!(radio.counters().paging.dedicated_queued, 1);
            assert_eq!(radio.counters().paging.pch_planned, 0);
        } else {
            assert_eq!(radio.counters().paging.dedicated_queued, 0);
            assert_eq!(radio.counters().paging.pch_planned, 1);
        }
    }
}

#[test]
fn same_occasion_pages_share_one_pch_and_keep_both_owners_until_submission() {
    let mut source = source(false);
    let mut radio = controller(&source);
    radio.receive_paging(&request("001010123456789"));
    let mut second = request("001010123456853");
    second.temporary = Some(TemporaryIdentity::Ptmsi([0x87, 0x65, 0x43, 0x21]));
    radio.receive_paging(&second);
    radio.service_paging(&mut source, at(0), 0);
    assert_eq!(radio.counters().paging.pch_planned, 2);
    assert_eq!(radio.counters().paging.pch_submitted, 0);
    for entry in radio.paging.entries.iter().flatten() {
        let receipt = entry.receipt.as_ref().unwrap();
        assert_eq!(
            receipt.snapshot().first.index,
            (23 * CHIPS_PER_FRAME - PAGING_LEAD_CHIPS) * 2
        );
        assert!(!receipt.submitted());
    }
}

#[test]
fn duplicates_coalesce_and_overflow_area_mismatch_and_expiry_are_explicit() {
    let mut source = source(false);
    let mut radio = controller(&source);
    let page = request("001010123456789");
    radio.receive_paging(&page);
    radio.receive_paging(&page);
    assert_eq!(radio.counters().paging.queued, 1);
    assert_eq!(radio.counters().paging.coalesced, 1);
    for index in 0..radio.paging.entries.len() {
        radio.receive_paging(&request(&format!("001010{index:09}")));
    }
    assert_eq!(radio.counters().paging.capacity_dropped, 1);
    assert_eq!(
        radio.paging.entries.iter().flatten().count(),
        radio.paging.entries.len()
    );
    let mut radio = controller(&source);
    let mut outside = page.clone();
    outside.area = Some(umts_iuh::ranap::paging::Area {
        plmn: [0xff; 3],
        lac: 0,
        rac: None,
    });
    radio.receive_paging(&outside);
    radio.service_paging(&mut source, at(0), 0);
    assert_eq!(radio.counters().paging.outside_area, 1);
    radio.receive_paging(&page);
    let slot = radio.paging.entries.iter_mut().flatten().next().unwrap();
    slot.expiry = Some(0);
    for _ in 0..radio.paging.entries.len() {
        radio.service_paging(&mut source, at(0), 0);
    }
    assert_eq!(radio.counters().paging.expired, 1);
}

#[test]
fn lost_pich_cannot_be_reported_as_submitted_when_pch_itself_is_accepted() {
    let mut source = source(false);
    let mut writer = source.submission_writer().unwrap();
    let bits = usize::from(source.information().common_layout().transport_block_bits());
    let receipt = source
        .publish_transport(
            at(2 * CHIPS_PER_FRAME),
            0,
            &vec![Bit::Zero; bits],
            &[true; 18],
            None,
        )
        .unwrap();
    let mut output = vec![num_complex::Complex32::default(); source.samples_per_slot()];
    for slot in 0..46 {
        let window = source.generate_slot(&mut output).unwrap();
        if slot == 12 {
            assert!(writer.lost(window));
        } else {
            assert!(writer.accepted(window));
        }
    }
    assert_eq!(receipt.snapshot().intersects_loss, Some(true));
    assert!(!receipt.viable());
    assert!(!receipt.submitted());
}

#[test]
fn idle_pages_repeat_boundedly_after_submission_or_local_loss() {
    for lose_first in [false, true] {
        let mut source = source(false);
        let mut writer = source.submission_writer().unwrap();
        let mut radio = controller(&source);
        radio.receive_paging(&request("001010123456789"));
        let mut output = vec![num_complex::Complex32::default(); source.samples_per_slot()];
        let frames = (RETENTION_CYCLES + 1) * (1 << 6);
        let first_pich_slot =
            (23 * CHIPS_PER_FRAME - PAGING_LEAD_CHIPS) / umts_types::time::CHIPS_PER_SLOT;
        for slot in 0..frames * umts_types::time::SLOTS_PER_FRAME {
            let now = source.next_chip();
            radio.service_paging(&mut source, now, 0);
            let window = source.generate_slot(&mut output).unwrap();
            if lose_first && slot == first_pich_slot {
                assert!(writer.lost(window));
            } else {
                assert!(writer.accepted(window));
            }
        }
        assert!(radio.paging.entries.iter().all(Option::is_none));
        assert_eq!(radio.counters().paging.pch_planned, RETENTION_CYCLES);
        assert_eq!(radio.counters().paging.pch_lost, u64::from(lose_first));
        assert_eq!(
            radio.counters().paging.pch_submitted,
            RETENTION_CYCLES - u64::from(lose_first)
        );
    }
}

#[test]
fn completed_idle_attempts_allow_late_identity_to_select_dedicated_paging() {
    let mut source = source(false);
    let mut writer = source.submission_writer().unwrap();
    let mut radio = controller(&source);
    let page = request("001010123456789");
    let key = crate::signalling::tests::establish(&mut radio, &source, 0);
    radio.registry.get_mut(key).unwrap().core_imsi = None;
    radio.registry.get_mut(key).unwrap().registration_imsi = None;
    radio.receive_paging(&page);
    let mut output = vec![num_complex::Complex32::default(); source.samples_per_slot()];
    let frames = RETENTION_CYCLES * (1 << 6);
    for _ in 0..frames * umts_types::time::SLOTS_PER_FRAME {
        let now = source.next_chip();
        radio.service_paging(&mut source, now, 0);
        let window = source.generate_slot(&mut output).unwrap();
        assert!(writer.accepted(window));
        if radio.counters().paging.pch_submitted == RETENTION_CYCLES {
            break;
        }
    }
    assert_eq!(radio.counters().paging.pch_submitted, RETENTION_CYCLES);
    assert_eq!(radio.paging.entries.iter().flatten().count(), 1);
    for _ in 0..radio.paging.entries.len() {
        let now = source.next_chip();
        radio.service_paging(&mut source, now, 0);
    }
    assert_eq!(radio.counters().paging.pch_planned, RETENTION_CYCLES);
    radio.registry.get_mut(key).unwrap().core_imsi = Some(page.imsi);
    for _ in 0..radio.paging.entries.len() {
        let now = source.next_chip();
        radio.service_paging(&mut source, now, 0);
    }
    assert_eq!(radio.counters().paging.dedicated_queued, 1);
    assert!(radio.registry.get(key).unwrap().control.pending_new_data());
    assert!(radio.paging.entries.iter().all(Option::is_none));
}
