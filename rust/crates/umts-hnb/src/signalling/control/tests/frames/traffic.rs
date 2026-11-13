// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::{
    EmptyTransparentTraffic, FrameBuildError, TransparentOffer, TransparentTraffic,
};

struct Speech {
    identity: ReceiveIdentity,
    widths: [usize; 3],
    consumed: u8,
    reject: Option<u8>,
}
impl TransparentTraffic for Speech {
    fn offer(
        &self,
        identity: ReceiveIdentity,
        start: ChipInstant,
        bearer: BearerId,
    ) -> Option<TransparentOffer> {
        let index = bearer.get().checked_sub(10)? as usize;
        (identity == self.identity
            && start == at(0)
            && index < self.widths.len()
            && self.consumed & (1 << index) == 0)
            .then(|| TransparentOffer {
                payload_bits_per_block: self.widths[index],
                blocks: 1,
            })
    }
    fn publish(
        &mut self,
        grant: DownlinkGrant,
        admit: impl FnOnce(&[Bits<'_>]) -> Result<(), FrameBuildError>,
    ) -> Result<(), FrameBuildError> {
        let offer = self
            .offer(grant.identity, grant.start, grant.bearer)
            .ok_or(BearerError::Transport)?;
        if self.reject == Some(grant.bearer.get()) {
            return Err(BearerError::Capacity.into());
        }
        let bytes = [0x5a; 13];
        admit(&[Bits::new(&bytes, offer.payload_bits_per_block).unwrap()])?;
        self.consumed |= 1 << (grant.bearer.get() - 10);
        Ok(())
    }
}

#[test]
fn pending_users_do_not_prevent_ready_control_from_being_selected_and_published() {
    let mut f = fixture_parts(true, true, true, true);
    f.radio
        .registry
        .get_mut(f.key)
        .unwrap()
        .control
        .enqueue(f.key, Bits::new(&[0x42], BITS_PER_BYTE).unwrap())
        .unwrap();
    let selected = f
        .bank
        .select_traffic(
            at(0),
            at(0),
            &mut f.radio,
            &EmptyTransparentTraffic,
            count(1024),
        )
        .unwrap();
    assert_eq!(selected, Tfci::new(12).unwrap());
    let mut draft = f
        .bank
        .begin_frame(f.plan.clone(), &f.ports.control, at(0), selected)
        .unwrap();
    draft
        .fill_traffic(at(0), &mut f.radio, &mut EmptyTransparentTraffic)
        .unwrap();
    assert!(draft.complete());
    drop(draft.finish().ok().unwrap());
    assert!(!f.bank.user_bearers_ready());
}

#[test]
fn traffic_policy_fills_short_packet_tti_with_clear_status_padding_and_retains_tti_format() {
    let mut f = fixture(true);
    let mut source = EmptyTransparentTraffic;
    assert_eq!(
        f.bank
            .select_traffic(at(0), at(0), &mut f.radio, &source, count(1024))
            .unwrap(),
        Tfci::new(12).unwrap()
    );
    assert!(
        f.radio
            .srb_publication_ready(f.key, SignallingBearer::NasAm, at(0))
            .unwrap()
    );
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), b"packet")
        .unwrap();
    for frame in 0..9 {
        let start = at(frame * CHIPS_PER_FRAME);
        let selected = f
            .bank
            .select_traffic(start, at(0), &mut f.radio, &source, count(1024))
            .unwrap();
        assert_eq!(
            selected,
            Tfci::new(match frame {
                0 | 1 => 18,
                2..=7 => 12,
                _ => 0,
            })
            .unwrap()
        );
        let mut draft = f
            .bank
            .begin_frame(f.plan.clone(), &f.ports.control, start, selected)
            .unwrap();
        draft
            .fill_traffic(at(0), &mut f.radio, &mut source)
            .unwrap();
        assert!(draft.complete());
        let input = draft.finish().ok().unwrap();
        if frame == 0 {
            assert!(
                !f.radio
                    .srb_publication_ready(f.key, SignallingBearer::ControlAm, at(0))
                    .unwrap()
            );
            assert!(
                f.radio
                    .srb_publication_ready(f.key, SignallingBearer::NasAm, at(0))
                    .unwrap()
            );
        }
        if frame == 4 {
            assert!(
                !f.radio
                    .srb_publication_ready(f.key, SignallingBearer::NasAm, at(0))
                    .unwrap()
            );
        }
        if frame == 0 {
            let packet = input
                .transports
                .iter()
                .find(|entry| entry.transport.get() == 9)
                .unwrap();
            assert_eq!(packet.bits.len(), 4 * 320);
            assert_eq!(packet.bits[0], Bit::One);
            assert!(packet.bits[320..].iter().all(|bit| *bit == Bit::Zero));
        }
    }
}

#[test]
fn traffic_policy_matches_the_complete_voice_subflow_combination_with_packet_data() {
    let mut f = fixture(true);
    let mut source = Speech {
        identity: f.bank.identity(),
        widths: [81, 103, 60],
        consumed: 0,
        reject: None,
    };
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), &[0x42; 200])
        .unwrap();
    let selected = f
        .bank
        .select_traffic(at(0), at(0), &mut f.radio, &source, count(1024))
        .unwrap();
    assert_eq!(selected, Tfci::new(23).unwrap());
    let mut draft = f
        .bank
        .begin_frame(f.plan.clone(), &f.ports.control, at(0), selected)
        .unwrap();
    draft
        .fill_traffic(at(0), &mut f.radio, &mut source)
        .unwrap();
    assert!(draft.complete());
    assert_eq!(source.consumed, 7);
    let input = draft.finish().ok().unwrap();
    for (transport, width) in [(6, 81), (7, 103), (8, 60)] {
        assert_eq!(
            input
                .transports
                .iter()
                .find(|entry| entry.transport.get() == transport)
                .unwrap()
                .bits
                .len(),
            width
        );
    }
}

#[test]
fn incompatible_voice_offer_and_excessive_scan_budget_do_not_consume_any_source() {
    let mut f = fixture(true);
    let source = Speech {
        identity: f.bank.identity(),
        widths: [81, 84, 60],
        consumed: 0,
        reject: None,
    };
    assert!(matches!(
        f.bank
            .select_traffic(at(0), at(0), &mut f.radio, &source, count(1024)),
        Err(FrameBuildError::Bearer(BearerError::Transport))
    ));
    assert_eq!(source.consumed, 0);
    assert!(matches!(
        f.bank.select_traffic(
            at(0),
            at(0),
            &mut f.radio,
            &EmptyTransparentTraffic,
            count(1)
        ),
        Err(FrameBuildError::Bearer(BearerError::Capacity))
    ));
    assert!(
        f.bank
            .begin_frame(
                f.plan.clone(),
                &f.ports.control,
                at(0),
                Tfci::new(0).unwrap()
            )
            .unwrap()
            .complete()
    );
}

#[test]
fn frame_policy_rejects_a_foreign_configuration_before_touching_its_queues() {
    use crate::rel99_admission::{FrameOpportunity, FramePolicy, TrafficFramePolicy};
    let mut f = fixture(true);
    let mut identity = f.bank.identity();
    identity.configuration = NonZeroU64::new(identity.configuration.get() + 1).unwrap();
    let opportunity = FrameOpportunity {
        identity,
        start: at(0),
        dedicated_signalling: true,
        user_bearers_ready: true,
        idle: Tfci::new(0).unwrap(),
    };
    let mut policy = TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024));
    assert!(matches!(
        policy.select(opportunity, at(0), &mut f.bank, &mut f.radio),
        Err(FrameBuildError::Bearer(BearerError::Owner))
    ));
}

#[test]
fn partial_voice_publication_requires_recovery_without_consuming_the_failed_subflow() {
    let mut f = fixture(false);
    let mut source = Speech {
        identity: f.bank.identity(),
        widths: [81, 103, 60],
        consumed: 0,
        reject: Some(11),
    };
    let selected = f
        .bank
        .select_traffic(at(0), at(0), &mut f.radio, &source, count(1024))
        .unwrap();
    let mut draft = f
        .bank
        .begin_frame(f.plan.clone(), &f.ports.control, at(0), selected)
        .unwrap();
    assert!(
        draft
            .fill_traffic(at(0), &mut f.radio, &mut source)
            .is_err()
    );
    drop(draft);
    assert_eq!(source.consumed, 1);
    assert!(matches!(
        f.bank
            .select_traffic(at(0), at(0), &mut f.radio, &source, count(1024)),
        Err(FrameBuildError::Bearer(BearerError::RecoveryRequired))
    ));
}
