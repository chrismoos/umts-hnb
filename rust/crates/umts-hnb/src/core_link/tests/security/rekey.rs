// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::radio_io::{PumpProgress, TxPreparation, TxPump, prepare_tx};
use sdr_hal::{
    Radio,
    memory::{MemoryRadio, MemoryRadioHandle},
};
use umts_rlc::acknowledged::{
    AmReceive, AmState, PreparedReceive, ReceivePreparation, ResetCounters,
};
use umts_rrc_schema::class_definitions::DLDCCHMessageType;
use umts_security::{contexts::KeyOrigin, counters::Start};

const AM_SEQUENCE_MASK: u64 = 4095;
const AM_SEQUENCE_BITS: u32 = 12;
const PENDING_CONTROL_ACTIVATION: u16 = 32;
const PENDING_NAS_DOWNLINK: u16 = 10;
const PENDING_NAS_UPLINK: u16 = 20;

#[derive(Clone, Copy)]
enum ResetInitiator {
    Peer,
    Network,
}

struct Mapping {
    first: u64,
    count: u32,
    cipher: CipherContext,
}
#[derive(Default)]
struct PeerCipher {
    downlink: [Vec<Mapping>; 2],
    uplink: [Vec<Mapping>; 2],
}
impl PeerCipher {
    fn index(bearer: SignallingBearer) -> usize {
        match bearer {
            SignallingBearer::ControlAm => 0,
            SignallingBearer::NasAm => 1,
            _ => panic!("AM signaling peer"),
        }
    }
    fn mappings(&self, bearer: SignallingBearer, direction: Direction) -> &[Mapping] {
        let maps = match direction {
            Direction::Downlink => &self.downlink,
            Direction::Uplink => &self.uplink,
        };
        &maps[Self::index(bearer)]
    }
    fn mapping(
        &self,
        bearer: SignallingBearer,
        direction: Direction,
        position: u64,
    ) -> Option<&Mapping> {
        self.mappings(bearer, direction)
            .iter()
            .rev()
            .find(|mapping| mapping.first <= position)
    }
    fn count(&self, bearer: SignallingBearer, direction: Direction, position: u64) -> u32 {
        self.mapping(bearer, direction, position).map_or_else(
            || u32::try_from(position).unwrap(),
            |mapping| {
                mapping
                    .count
                    .checked_add(u32::try_from(position - mapping.first).unwrap())
                    .unwrap()
            },
        )
    }
    fn apply(
        &self,
        bearer: SignallingBearer,
        direction: Direction,
        position: u64,
        bytes: &mut [u8],
    ) {
        if let Some(mapping) = self.mapping(bearer, direction, position) {
            let body = &mut bytes[2..];
            mapping
                .cipher
                .apply(
                    mapping.cipher.origin(),
                    self.count(bearer, direction, position),
                    bearer.identity() - 1,
                    direction,
                    body,
                    body.len() * BITS_PER_BYTE,
                )
                .unwrap();
        }
    }
    fn install(
        &mut self,
        direction: Direction,
        bearer: SignallingBearer,
        first: u64,
        cipher: &CipherContext,
    ) {
        let maps = match direction {
            Direction::Downlink => &mut self.downlink,
            Direction::Uplink => &mut self.uplink,
        };
        maps[Self::index(bearer)].push(Mapping {
            first,
            count: (first & AM_SEQUENCE_MASK) as u32,
            cipher: cipher.clone(),
        });
    }
    fn reset(&mut self, bearer: SignallingBearer, direction: Direction, first: u64, hfn: u32) {
        let cipher = self
            .mappings(bearer, direction)
            .last()
            .unwrap()
            .cipher
            .clone();
        self.install(direction, bearer, first, &cipher);
        let maps = match direction {
            Direction::Downlink => &mut self.downlink,
            Direction::Uplink => &mut self.uplink,
        };
        maps[Self::index(bearer)].last_mut().unwrap().count = hfn << AM_SEQUENCE_BITS;
    }
    fn reset_hfn(
        &self,
        bearer: SignallingBearer,
        direction: Direction,
        next: u64,
        synchronized: u32,
    ) -> u32 {
        self.pending_hfn(bearer, direction, next)
            .unwrap_or(synchronized)
    }
    fn pending_hfn(
        &self,
        bearer: SignallingBearer,
        direction: Direction,
        next: u64,
    ) -> Option<u32> {
        self.mappings(bearer, direction)
            .last()
            .filter(|mapping| mapping.first > next)
            .map(|mapping| mapping.count >> AM_SEQUENCE_BITS)
    }
}

struct Fixture {
    port: crate::common_tx::CommonTxPort,
    radio: CommonSignalling,
    link: CoreLink<Mock>,
    core: Peer,
    key: UeKey,
    context: ContextId,
    preparation: TxPreparation,
    pump: TxPump,
    memory: MemoryRadioHandle,
    control: AmEntity,
    nas: AmEntity,
    cipher: PeerCipher,
    control_rrc: u8,
    nas_rrc: u8,
    nas_count: u32,
    downlink_nas_count: u32,
    current_integrity: Option<IntegrityContext>,
    security_tick: u64,
    last_dl_control: Option<u8>,
    active: [Option<KeyOrigin>; 2],
}

impl Fixture {
    fn new(compact: bool) -> Self {
        let source = source(compact);
        let mut radio = controller(&source);
        let (mut link, core) = ready(&mut radio);
        let (key, context, mut nas_peer) = register(&mut link, &core, &mut radio, &source, 0, 3);
        let mut control = radio_peer(&source);
        control.enqueue(&COMPLETE).unwrap();
        control
            .publish_next(at(0), |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        radio
            .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
            .unwrap();
        send_capability_report(&mut radio, key, &mut control);
        assert_eq!(radio.process_one(), Some(Ok(key)));
        let mut initial = INITIAL;
        initial[1] |= 0x80;
        uplink(
            &mut radio,
            key,
            SignallingBearer::NasAm,
            &mut nas_peer,
            at(0),
            &initial,
            None,
        );
        assert_eq!(radio.process_one(), Some(Ok(key)));
        for _ in 0..4 {
            drive(&mut link, &mut radio, 6);
        }
        assert!(radio.connection(key, CnDomain::Cs).is_some());
        assert!(radio.connection(key, CnDomain::Ps).is_some());
        let (mut memory, peer) = MemoryRadio::new(1);
        memory.set_tx_sample_rate(7_680_000.0).unwrap();
        let (tx, _) = Box::new(memory).split().unwrap();
        let (mut preparation, mut pump) = prepare_tx(
            source,
            tx,
            crate::radio_io::TxSettings::offline(1000, count(1), count(1), count(2)),
        )
        .unwrap();
        let port = preparation.control_port(count(8), count(2)).unwrap();
        pump.start().unwrap();
        let mut fixture = Self {
            port,
            radio,
            link,
            core,
            key,
            context,
            preparation,
            pump,
            memory: peer,
            control,
            nas: nas_peer,
            cipher: PeerCipher::default(),
            control_rrc: 0,
            nas_rrc: 0,
            nas_count: 0,
            downlink_nas_count: 0,
            current_integrity: None,
            security_tick: 6,
            last_dl_control: None,
            active: [None; 2],
        };
        for _ in 0..4 {
            fixture.frame(SignallingBearer::ControlAm);
        }
        fixture
    }
    fn now(&self) -> ChipInstant {
        crate::common_tx::CommonTxTarget::next_chip(&self.port)
    }
    fn peer_mut(&mut self, bearer: SignallingBearer) -> &mut AmEntity {
        match bearer {
            SignallingBearer::ControlAm => &mut self.control,
            SignallingBearer::NasAm => &mut self.nas,
            _ => panic!("AM peer"),
        }
    }
    fn inject(&mut self, bearer: SignallingBearer, bytes: &[u8]) {
        self.radio
            .receive(
                self.key,
                self.now(),
                umts_mac::CommonPdu {
                    header: CommonHeader::Dedicated {
                        identity: CommonIdentity::Cell(
                            self.radio.assigned(self.key).unwrap().crnti(),
                        ),
                        channel: LogicalChannelId::new(bearer.identity()).unwrap(),
                    },
                    payload: Bits::new(bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
                },
            )
            .unwrap();
        for _ in 0..64 {
            if !self
                .radio
                .collect_one(self.key, bearer, self.now())
                .unwrap()
            {
                break;
            }
        }
    }
    fn next_wire(&mut self, bearer: SignallingBearer) -> Option<Vec<u8>> {
        let now = self.now();
        let cipher = &self.cipher;
        let peer = match bearer {
            SignallingBearer::ControlAm => &mut self.control,
            SignallingBearer::NasAm => &mut self.nas,
            _ => panic!("AM peer"),
        };
        peer.publish_next_mapped(
            now,
            |position| Ok::<_, ()>(cipher.count(bearer, Direction::Uplink, position)),
            |publication| {
                let mut bytes = publication.bytes.to_vec();
                if let Some(position) = publication.position {
                    cipher.apply(bearer, Direction::Uplink, position, &mut bytes);
                }
                Ok::<_, ()>(bytes)
            },
        )
        .unwrap()
        .map(|(_, bytes)| bytes)
    }
    fn send(&mut self, bearer: SignallingBearer, bytes: &[u8]) {
        self.peer_mut(bearer).enqueue(bytes).unwrap();
        for _ in 0..64 {
            if !self.peer_mut(bearer).pending_new_data() {
                return;
            }
            let next = self.next_wire(bearer).unwrap();
            self.inject(bearer, &next);
        }
        panic!("bounded peer SDU publication");
    }
    fn flush(&mut self, bearer: SignallingBearer) {
        if let Some(bytes) = self.next_wire(bearer) {
            self.inject(bearer, &bytes);
        }
    }
    fn frame(&mut self, bearer: SignallingBearer) -> Vec<Vec<u8>> {
        let now = self.now();
        let published = self
            .radio
            .publish_bearer(self.key, bearer, &mut self.port, now, now)
            .unwrap();
        let mut received = Vec::new();
        for slot in 0..15 {
            self.preparation.prepare_one().unwrap().unwrap();
            assert!(matches!(
                self.pump.cycle().unwrap(),
                PumpProgress::Submitted(_)
            ));
            self.memory.output.recv().unwrap();
            for _ in 0..8 {
                let Some(event) = self.port.next_event() else {
                    break;
                };
                assert!(!matches!(
                    event.stage,
                    crate::common_tx::PlanStage::Rejected(_)
                        | crate::common_tx::PlanStage::Cancelled
                ));
            }
            if slot == 0 && published {
                let bytes = recover_current_fach(self.preparation.source());
                let mac = decode_common(
                    FddCommonChannel::Fach,
                    Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    mac.header,
                    CommonHeader::Dedicated {
                        identity: CommonIdentity::Cell(
                            self.radio.assigned(self.key).unwrap().crnti()
                        ),
                        channel: LogicalChannelId::new(bearer.identity()).unwrap()
                    }
                );
                let mut payload = vec![0; mac.payload.len() / BITS_PER_BYTE];
                mac.payload.copy_to(&mut payload).unwrap();
                let mut plaintext = payload.clone();
                let cipher = &self.cipher;
                let peer = match bearer {
                    SignallingBearer::ControlAm => &mut self.control,
                    SignallingBearer::NasAm => &mut self.nas,
                    _ => panic!("AM peer"),
                };
                let before = peer.window_positions();
                let result = peer
                    .receive_prepared_with_reset(now, &payload, |preparation| {
                        let reset_counters = if let ReceivePreparation::Reset {
                            transition: Some(_),
                            ..
                        } = preparation
                        {
                            Some(ResetCounters {
                                transmit_hfn: cipher.pending_hfn(
                                    bearer,
                                    Direction::Uplink,
                                    before.next_transmit,
                                ),
                                receive_hfn: cipher.pending_hfn(
                                    bearer,
                                    Direction::Downlink,
                                    before.next_receive,
                                ),
                            })
                        } else {
                            None
                        };
                        if let ReceivePreparation::Data { position, .. } = preparation {
                            cipher.apply(bearer, Direction::Downlink, position, &mut plaintext);
                        }
                        Ok::<_, ()>(PreparedReceive {
                            plaintext: plaintext.as_slice(),
                            reset_counters,
                        })
                    })
                    .unwrap();
                if let AmReceive::ResetApplied { tx_hfn, rx_hfn }
                | AmReceive::ResetAcknowledged { tx_hfn, rx_hfn } = result
                {
                    let positions = peer.window_positions();
                    self.cipher
                        .reset(bearer, Direction::Uplink, positions.next_transmit, tx_hfn);
                    self.cipher
                        .reset(bearer, Direction::Downlink, positions.next_receive, rx_hfn);
                }
                for _ in 0..64 {
                    if self
                        .peer_mut(bearer)
                        .deliver_with(|delivery| {
                            received.extend(delivery.sdus().map(<[u8]>::to_vec));
                            Ok::<_, ()>(())
                        })
                        .unwrap()
                        .is_none()
                    {
                        break;
                    }
                }
            }
        }
        received
    }
    fn exchange(&mut self, cycle: u8) {
        self.exchange_with_reset(cycle, None);
    }
    fn exchange_with_reset(&mut self, cycle: u8, pending_reset: Option<ResetInitiator>) {
        let domain = if cycle.is_multiple_of(2) {
            Domain::Cs
        } else {
            Domain::Ps
        };
        let slot = usize::from(!cycle.is_multiple_of(2));
        let previous_domain_origin = self.active[slot];
        let tick = 20 + u64::from(cycle) * 10;
        let ik = [0x20 + cycle; 16];
        let ck = [0x40 + cycle; 16];
        let command = ranap::SecurityCommand {
            integrity: ranap::Protection {
                algorithms: vec![ranap::Algorithm(0)],
                key: ranap::Key::new(ik),
            },
            encryption: Some(ranap::Protection {
                algorithms: vec![ranap::Algorithm(1)],
                key: ranap::Key::new(ck),
            }),
            key_status: ranap::KeyStatus::New,
        };
        self.core.rua(rua::Message::Direct {
            domain,
            context: self.context,
            ranap: ranap::encode_security(&command, Limits::default()).unwrap(),
        });
        drive(&mut self.link, &mut self.radio, tick);
        let transaction = TransactionId::new(cycle % 4).unwrap();
        if pending_reset.is_some() {
            assert_eq!(cycle, 0);
            let fresh = self.radio.security(self.key).unwrap().fresh();
            self.radio
                .prepare_security(
                    self.key,
                    domain,
                    time(tick + 1),
                    RadioPlan {
                        transaction,
                        layout: BearerLayout::new(SignallingSet::Base, &[]).unwrap(),
                        integrity: IntegrityChange::Start { fresh },
                        cipher: Some(
                            ActivationSequences::new(
                                BearerLayout::new(SignallingSet::Base, &[]).unwrap(),
                                &[
                                    (1, 0),
                                    (2, PENDING_CONTROL_ACTIVATION),
                                    (3, PENDING_NAS_DOWNLINK),
                                ],
                            )
                            .unwrap(),
                        ),
                        dpch_cfn: None,
                    },
                )
                .unwrap();
        } else {
            self.radio
                .prepare_common_security(self.key, domain, time(tick + 1), transaction)
                .unwrap();
        }
        for bearer in [SignallingBearer::ControlAm, SignallingBearer::NasAm] {
            assert_eq!(
                self.radio.initiate_am_reset(self.key, bearer, self.now()),
                Err(SignallingError::Phase)
            );
        }
        let origin = self
            .radio
            .security(self.key)
            .unwrap()
            .pending_origin(domain)
            .unwrap();
        let (cipher, integrity) =
            KeySet::uia1(origin, CipherMaterial::Uea1(Key::new(ck)), Key::new(ik)).split();
        let fresh = self.radio.security(self.key).unwrap().fresh();
        let plan = self
            .radio
            .security(self.key)
            .unwrap()
            .cipher_plan(domain, Some(Start::new(0).unwrap()))
            .unwrap()
            .unwrap();
        self.radio
            .publish_security_command(self.key, domain, time(tick + 2))
            .unwrap();
        let mut delivered = None;
        for _ in 0..64 {
            for bytes in self.frame(SignallingBearer::ControlAm) {
                let decoded =
                    decode_downlink_dedicated(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
                if let DLDCCHMessageType::securityModeCommand(pdu::SecurityModeCommand::r3(body)) =
                    &decoded.value.message
                {
                    let info = &body.security_mode_command_r3;
                    assert_eq!(info.rrc_transaction_identifier.0, transaction.get());
                    assert_eq!(
                        info.cn_domain_identity,
                        if domain == Domain::Cs {
                            ie::CNDomainIdentity::cs_domain
                        } else {
                            ie::CNDomainIdentity::ps_domain
                        }
                    );
                    let entries = &info
                        .ciphering_mode_info
                        .as_ref()
                        .unwrap()
                        .rb_dl_ciph_activation_time_info
                        .as_ref()
                        .unwrap()
                        .0;
                    for entry in entries {
                        assert_eq!(
                            entry.rlc_sequence_number.0,
                            plan.downlink.sequence(entry.rb_identity.0).unwrap()
                        );
                    }
                    let sequence = decoded
                        .value
                        .integrity_check_info
                        .as_ref()
                        .unwrap()
                        .rrc_message_sequence_number
                        .0;
                    let count = u32::from(sequence)
                        + if self
                            .last_dl_control
                            .is_some_and(|previous| sequence < previous)
                        {
                            16
                        } else {
                            0
                        };
                    assert_downlink(&integrity, fresh, 2, count, &bytes);
                    self.last_dl_control = Some(sequence);
                    delivered = Some(bytes);
                }
            }
            if delivered.is_some() {
                break;
            }
        }
        assert!(delivered.is_some(), "command cycle {cycle}");
        let SduTransmission::InFlight {
            first,
            last,
            delimited: true,
        } = self
            .radio
            .security_delivery(self.key)
            .unwrap()
            .unwrap()
            .command
        else {
            panic!("complete command span");
        };
        assert!(last >= first);
        if pending_reset.is_none() {
            assert_eq!(
                u64::from(plan.downlink.sequence(2).unwrap()),
                (last + 1) & AM_SEQUENCE_MASK
            );
        } else {
            assert!(last + 1 < u64::from(PENDING_CONTROL_ACTIVATION));
        }
        for bearer in [SignallingBearer::ControlAm, SignallingBearer::NasAm] {
            let next = if bearer == SignallingBearer::ControlAm {
                if pending_reset.is_some() {
                    u64::from(PENDING_CONTROL_ACTIVATION)
                } else {
                    last + 1
                }
            } else {
                let next = self.nas.window_positions().next_receive;
                let sequence = u64::from(plan.downlink.sequence(bearer.identity()).unwrap());
                next + sequence.wrapping_sub(next & AM_SEQUENCE_MASK) % (AM_SEQUENCE_MASK + 1)
            };
            self.cipher
                .install(Direction::Downlink, bearer, next, &cipher);
        }
        let next_control = (self.control_rrc + 1) & 15;
        let control_count = u32::from(next_control)
            + if next_control < self.control_rrc {
                16
            } else {
                0
            };
        let next_nas = (self.nas_rrc + 1) & 15;
        let sequences = if cycle == 0 {
            [0; 4]
        } else {
            [0, 0, 0, next_nas]
        };
        let complete = |control_activation| {
            protected(
                &integrity,
                fresh,
                2,
                control_count,
                ULDCCHMessageType::securityModeComplete(pdu::SecurityModeComplete::new(
                    ie::RRCTransactionIdentifier(transaction.get()),
                    Some(ie::IntegrityProtActivationInfo::new(
                        ie::RRCMessageSequenceNumberList(
                            sequences
                                .into_iter()
                                .map(ie::RRCMessageSequenceNumber)
                                .collect(),
                        ),
                    )),
                    Some(ie::RBActivationTimeInfoList(vec![
                        ie::RBActivationTimeInfo::new(ie::RBIdentity(1), ie::RLCSequenceNumber(0)),
                        ie::RBActivationTimeInfo::new(
                            ie::RBIdentity(2),
                            ie::RLCSequenceNumber(control_activation),
                        ),
                        ie::RBActivationTimeInfo::new(
                            ie::RBIdentity(3),
                            ie::RLCSequenceNumber(
                                (self.nas.send_sequence()
                                    + if pending_reset.is_some() {
                                        PENDING_NAS_UPLINK
                                    } else {
                                        0
                                    })
                                    & AM_SEQUENCE_MASK as u16,
                            ),
                        ),
                    ])),
                    None,
                )),
            )
        };
        let span = self
            .control
            .isolated_sdu_range(complete(self.control.send_sequence()).len())
            .unwrap();
        let complete = complete((span.next & AM_SEQUENCE_MASK) as u16);
        let nas_activation = self.nas.window_positions().next_transmit
            + if pending_reset.is_some() {
                u64::from(PENDING_NAS_UPLINK)
            } else {
                0
            };
        self.send(SignallingBearer::ControlAm, &complete);
        assert_eq!(
            self.radio.process_one_at(time(tick + 3)),
            Some(Ok(self.key))
        );
        self.control_rrc = next_control;
        assert_eq!(
            self.radio.security(self.key).unwrap().unconfirmed_origin(),
            Some(origin)
        );
        assert_eq!(
            self.radio
                .security(self.key)
                .unwrap()
                .previous_completion_origin(),
            previous_domain_origin
        );
        assert_eq!(
            self.radio
                .initiate_am_reset(self.key, SignallingBearer::ControlAm, self.now()),
            Err(SignallingError::Phase)
        );
        self.cipher.install(
            Direction::Uplink,
            SignallingBearer::ControlAm,
            span.next,
            &cipher,
        );
        self.cipher.install(
            Direction::Uplink,
            SignallingBearer::NasAm,
            nas_activation,
            &cipher,
        );
        if let Some(initiator) = pending_reset {
            match initiator {
                ResetInitiator::Peer => self.peer_reset(SignallingBearer::NasAm, true),
                ResetInitiator::Network => self.network_reset(SignallingBearer::NasAm),
            }
            let positions = self.nas.window_positions();
            assert_eq!(
                self.cipher.count(
                    SignallingBearer::NasAm,
                    Direction::Uplink,
                    positions.next_transmit
                ),
                0
            );
            assert_eq!(
                self.cipher.count(
                    SignallingBearer::NasAm,
                    Direction::Downlink,
                    positions.next_receive
                ),
                0
            );
        }
        self.active[slot] = Some(origin);
        assert_eq!(
            self.radio
                .security(self.key)
                .unwrap()
                .active_origin(Domain::Cs),
            self.active[0]
        );
        assert_eq!(
            self.radio
                .security(self.key)
                .unwrap()
                .active_origin(Domain::Ps),
            self.active[1]
        );
        for _ in 0..64 {
            self.frame(SignallingBearer::ControlAm);
            self.flush(SignallingBearer::ControlAm);
            if self.control.outstanding_pdus() == 0
                && self
                    .radio
                    .security_delivery(self.key)
                    .unwrap()
                    .unwrap()
                    .completion_status_submitted
            {
                break;
            }
        }
        assert_eq!(self.control.outstanding_pdus(), 0);
        assert!(
            self.radio
                .security_delivery(self.key)
                .unwrap()
                .unwrap()
                .completion_status_submitted
        );
        let nas_count = u32::from(next_nas) + if next_nas < self.nas_rrc { 16 } else { 0 };
        self.send(SignallingBearer::NasAm, &nas(&integrity, fresh, nas_count));
        assert_eq!(
            self.radio.process_one_at(time(tick + 4)),
            Some(Ok(self.key))
        );
        self.nas_rrc = next_nas;
        self.nas_count = nas_count;
        assert!(
            self.radio
                .security_delivery(self.key)
                .unwrap()
                .unwrap()
                .peer_progress_observed
        );
        let mut retired = false;
        for _ in 0..16 {
            retired |= self.radio.service_security_one(time(tick + 4)).unwrap()
                == SecurityWork::DeliveryRetired {
                    key: self.key,
                    origin,
                };
        }
        assert!(retired);
        assert_eq!(
            self.radio.security(self.key).unwrap().unconfirmed_origin(),
            None
        );
        if let Some(previous) = previous_domain_origin {
            assert!(
                self.radio
                    .security(self.key)
                    .unwrap()
                    .keys(previous)
                    .is_none()
            );
        }
        assert_eq!(self.radio.security_delivery(self.key).unwrap(), None);
        for _ in 0..4 {
            drive(&mut self.link, &mut self.radio, tick + 4);
        }
        self.radio
            .downlink_nas(
                self.radio.connection(self.key, CnDomain::Cs).unwrap(),
                DownlinkPriority::Sms,
                &[9, cycle],
            )
            .unwrap();
        let mut data = false;
        for _ in 0..64 {
            for bytes in self.frame(SignallingBearer::NasAm) {
                let decoded =
                    decode_downlink_dedicated(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
                let sequence = decoded
                    .value
                    .integrity_check_info
                    .as_ref()
                    .unwrap()
                    .rrc_message_sequence_number
                    .0;
                let count = u32::from(sequence) + if cycle != 0 && sequence == 0 { 16 } else { 0 };
                assert_downlink(&integrity, fresh, 3, count, &bytes);
                self.downlink_nas_count = count;
                data = true;
            }
            self.flush(SignallingBearer::NasAm);
            if data && self.nas.outstanding_pdus() == 0 {
                break;
            }
        }
        assert!(data);
        assert_eq!(self.nas.outstanding_pdus(), 0);
        for _ in 0..4 {
            drive(&mut self.link, &mut self.radio, tick + 4);
        }
        self.current_integrity = Some(integrity);
        self.security_tick = tick + 4;
    }
    fn protected_traffic(&mut self) {
        let integrity = self.current_integrity.as_ref().unwrap().clone();
        let fresh = self.radio.security(self.key).unwrap().fresh();
        self.nas_count += 1;
        self.nas_rrc = (self.nas_count & 15) as u8;
        self.send(
            SignallingBearer::NasAm,
            &nas(&integrity, fresh, self.nas_count),
        );
        self.security_tick += 1;
        assert_eq!(
            self.radio.process_one_at(time(self.security_tick)),
            Some(Ok(self.key))
        );
        for _ in 0..4 {
            drive(&mut self.link, &mut self.radio, self.security_tick);
        }
        self.radio
            .downlink_nas(
                self.radio.connection(self.key, CnDomain::Cs).unwrap(),
                DownlinkPriority::Sms,
                &[0x35, 0x36],
            )
            .unwrap();
        let mut received = 0;
        for _ in 0..64 {
            for bytes in self.frame(SignallingBearer::NasAm) {
                self.downlink_nas_count += 1;
                assert_downlink(&integrity, fresh, 3, self.downlink_nas_count, &bytes);
                received += 1;
            }
            self.flush(SignallingBearer::NasAm);
            if received != 0 && self.nas.outstanding_pdus() == 0 {
                break;
            }
        }
        assert_eq!(received, 1);
        assert_eq!(self.nas.outstanding_pdus(), 0);
    }
    fn peer_reset(&mut self, bearer: SignallingBearer, duplicate: bool) {
        let now = self.now();
        self.peer_mut(bearer).initiate_reset(now).unwrap();
        let reset = self.next_wire(bearer).unwrap();
        let message = umts_rlc::acknowledged::parse_reset(&reset).unwrap();
        assert!(!message.acknowledge);
        let before = self.peer_mut(bearer).window_positions();
        let expected_hfn = self.cipher.reset_hfn(
            bearer,
            Direction::Uplink,
            before.next_transmit,
            message.hfn + 1,
        );
        self.inject(bearer, &reset);
        if duplicate {
            self.inject(bearer, &reset);
        }
        for _ in 0..16 {
            self.frame(bearer);
            if self.peer_mut(bearer).state() == AmState::Ready {
                break;
            }
        }
        assert_eq!(self.peer_mut(bearer).state(), AmState::Ready);
        let after = self.peer_mut(bearer).window_positions();
        assert!(after.next_transmit > before.next_transmit);
        assert!(after.next_receive > before.next_receive);
        assert_eq!(after.next_transmit & AM_SEQUENCE_MASK, 0);
        assert_eq!(after.next_receive & AM_SEQUENCE_MASK, 0);
        assert_eq!(
            self.cipher
                .count(bearer, Direction::Uplink, after.next_transmit),
            expected_hfn << AM_SEQUENCE_BITS
        );
    }
    fn network_reset(&mut self, bearer: SignallingBearer) {
        self.radio
            .initiate_am_reset(self.key, bearer, self.now())
            .unwrap();
        let before = self.peer_mut(bearer).window_positions();
        for _ in 0..16 {
            self.frame(bearer);
            let after = self.peer_mut(bearer).window_positions();
            if after.next_receive > before.next_receive {
                let ack = self.next_wire(bearer).unwrap();
                assert!(
                    umts_rlc::acknowledged::parse_reset(&ack)
                        .unwrap()
                        .acknowledge
                );
                self.inject(bearer, &ack);
                return;
            }
        }
        panic!("network RESET delivery");
    }
    fn simultaneous_reset(&mut self, bearer: SignallingBearer) {
        self.radio
            .initiate_am_reset(self.key, bearer, self.now())
            .unwrap();
        let now = self.now();
        self.peer_mut(bearer).initiate_reset(now).unwrap();
        let peer_reset = self.next_wire(bearer).unwrap();
        assert!(
            !umts_rlc::acknowledged::parse_reset(&peer_reset)
                .unwrap()
                .acknowledge
        );
        self.frame(bearer);
        assert_eq!(self.peer_mut(bearer).state(), AmState::ResetPending);
        self.inject(bearer, &peer_reset);
        for _ in 0..4 {
            self.frame(bearer);
            self.flush(bearer);
        }
        assert_eq!(self.peer_mut(bearer).state(), AmState::Ready);
    }
}

fn assert_downlink(context: &IntegrityContext, fresh: u32, bearer: u8, count: u32, bytes: &[u8]) {
    let decoded = decode_downlink_dedicated(bytes, bytes.len() * BITS_PER_BYTE).unwrap();
    let actual = decoded.value.integrity_check_info.as_ref().unwrap();
    assert_eq!(actual.rrc_message_sequence_number.0, (count & 15) as u8);
    let mut preimage = bytes.to_vec();
    for (offset, bits, value) in [(1, 32, u32::from(bearer)), (33, 4, 0)] {
        for index in 0..bits {
            let at = offset + index;
            let mask = 1 << (7 - at % 8);
            if value & (1 << (bits - 1 - index)) != 0 {
                preimage[at / 8] |= mask;
            } else {
                preimage[at / 8] &= !mask;
            }
        }
    }
    let expected = context
        .algorithm(context.origin())
        .unwrap()
        .authenticate(
            count,
            fresh,
            Direction::Downlink,
            &preimage,
            preimage.len() * BITS_PER_BYTE,
        )
        .unwrap();
    assert_eq!(
        actual.message_authentication_code,
        ie::MessageAuthenticationCode(fixed(u32::from_be_bytes(expected)))
    );
}

#[test]
fn repeated_encrypted_cs_ps_cycles_cross_counter_wrap_without_key_slot_exhaustion() {
    let mut fixture = Fixture::new(false);
    for cycle in 0..24 {
        fixture.exchange(cycle);
    }
}
#[test]
fn compact_common_profile_uses_the_actual_segmented_command_span() {
    let mut fixture = Fixture::new(true);
    for cycle in 0..6 {
        fixture.exchange(cycle);
    }
}

#[test]
fn encrypted_common_am_resets_preserve_integrity_and_allow_subsequent_cs_ps_rekey() {
    for compact in [false, true] {
        let mut fixture = Fixture::new(compact);
        fixture.exchange(0);
        for (cycle, bearer) in [SignallingBearer::NasAm, SignallingBearer::ControlAm]
            .into_iter()
            .enumerate()
        {
            fixture.peer_reset(bearer, true);
            fixture.protected_traffic();
            fixture.exchange(1 + cycle as u8 * 2);
            fixture.network_reset(bearer);
            fixture.protected_traffic();
            fixture.exchange(2 + cycle as u8 * 2);
        }
    }
}

#[test]
fn pending_common_cipher_activation_resets_before_delivery_retirement_and_crosses_another_rekey() {
    for compact in [false, true] {
        for initiator in [ResetInitiator::Peer, ResetInitiator::Network] {
            let mut fixture = Fixture::new(compact);
            fixture.exchange_with_reset(0, Some(initiator));
            match initiator {
                ResetInitiator::Peer => fixture.peer_reset(SignallingBearer::ControlAm, true),
                ResetInitiator::Network => fixture.network_reset(SignallingBearer::ControlAm),
            }
            fixture.protected_traffic();
            fixture.exchange(1);
            fixture.simultaneous_reset(SignallingBearer::NasAm);
            fixture.protected_traffic();
        }
    }
}

#[test]
fn protected_cell_update_uses_prior_rb0_key_and_releases_both_domains_with_radio_backpressure() {
    for user_only in [false, true] {
        protected_cell_update_case(user_only);
    }
}

fn protected_cell_update_case(user_only: bool) {
    // TS 25.321 RACH CCCH has a two-bit MAC header, leaving an octet-sized transport's TM SDU nonaligned.
    const STORAGE_PADDING_BITS: usize = 2;
    fn wire(
        context: &IntegrityContext,
        fresh: u32,
        assigned: umts_rrc::establishment::AssignedRntis,
        user_only: bool,
    ) -> Vec<u8> {
        let mut message = crate::test_support::failed_cell_update(assigned);
        let umts_rrc_schema::class_definitions::ULCCCHMessageType::cellUpdate(body) =
            &mut message.message
        else {
            unreachable!()
        };
        body.am_rlc_error_indication_rb2_3or4 = !user_only;
        body.am_rlc_error_indication_rb5or_above = user_only;
        message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
            ie::MessageAuthenticationCode(fixed(0)),
            ie::RRCMessageSequenceNumber(0),
        ));
        let mut preimage = rasn::uper::encode(&message).unwrap();
        preimage.push(0xa4);
        let mac = context
            .algorithm(context.origin())
            .unwrap()
            .authenticate(
                1,
                fresh,
                Direction::Uplink,
                &preimage,
                preimage.len() * BITS_PER_BYTE - STORAGE_PADDING_BITS,
            )
            .unwrap();
        message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
            ie::MessageAuthenticationCode(fixed(u32::from_be_bytes(mac))),
            ie::RRCMessageSequenceNumber(1),
        ));
        let mut encoded = rasn::uper::encode(&message).unwrap();
        encoded.push(0xa4);
        encoded
    }
    let mut fixture = Fixture::new(false);
    fixture.exchange(0);
    let prior = fixture.current_integrity.as_ref().unwrap().clone();
    let command = ranap::SecurityCommand {
        integrity: ranap::Protection {
            algorithms: vec![ranap::Algorithm(0)],
            key: ranap::Key::new([0x77; 16]),
        },
        encryption: Some(ranap::Protection {
            algorithms: vec![ranap::Algorithm(1)],
            key: ranap::Key::new([0x66; 16]),
        }),
        key_status: ranap::KeyStatus::New,
    };
    fixture.core.rua(rua::Message::Direct {
        domain: Domain::Ps,
        context: fixture.context,
        ranap: ranap::encode_security(&command, Limits::default()).unwrap(),
    });
    drive(&mut fixture.link, &mut fixture.radio, 40);
    fixture
        .radio
        .prepare_common_security(
            fixture.key,
            Domain::Ps,
            time(41),
            TransactionId::new(1).unwrap(),
        )
        .unwrap();
    fixture
        .radio
        .publish_security_command(fixture.key, Domain::Ps, time(42))
        .unwrap();
    let pending = fixture
        .radio
        .security(fixture.key)
        .unwrap()
        .pending_origin(Domain::Ps)
        .unwrap();
    let new = KeySet::uia1(
        pending,
        CipherMaterial::Uea1(Key::new([0x66; 16])),
        Key::new([0x77; 16]),
    )
    .split()
    .1;
    let assigned = fixture.radio.assigned(fixture.key).unwrap();
    let fresh = fixture.radio.security(fixture.key).unwrap().fresh();
    let wrong = wire(&new, fresh, assigned, user_only);
    let now = fixture.now();
    let mut ingress = crate::common_control::tests::ingress(1);
    crate::common_control::tests::enqueue_sdu(
        &mut ingress,
        &wrong,
        wrong.len() * BITS_PER_BYTE - STORAGE_PADDING_BITS,
        now,
    );
    assert!(matches!(
        fixture
            .radio
            .service_common_one(&fixture.port, &mut ingress, TransactionId::new(2).unwrap(),)
            .unwrap()
            .work,
        crate::signalling::CommonIngressWork::Rejected(_)
    ));
    assert_eq!(ingress.pending(), 0);
    assert!(fixture.radio.assigned(fixture.key).is_some());
    let correct = wire(&prior, fresh, assigned, user_only);
    crate::common_control::tests::enqueue_sdu(
        &mut ingress,
        &correct,
        correct.len() * BITS_PER_BYTE - STORAGE_PADDING_BITS,
        now,
    );
    for _ in 0..4 {
        fixture
            .radio
            .core()
            .push_radio_failure(
                UeKey {
                    slot: fixture.key.slot + 1,
                    ..fixture.key
                },
                Some(umts_rrc::nas::CoreContextId::new(999).unwrap()),
                false,
                [None; 2],
            )
            .unwrap();
    }
    for _ in 0..4 {
        let step = fixture
            .radio
            .service_common_one(&fixture.port, &mut ingress, TransactionId::new(2).unwrap())
            .unwrap();
        assert_eq!(step.transport.start, now);
        assert_eq!(step.work, crate::signalling::CommonIngressWork::Deferred);
        assert_eq!(ingress.pending(), 1);
        assert_eq!(ingress.counters().decoded, 2);
    }
    assert!(fixture.radio.assigned(fixture.key).is_some());
    fixture.radio.core().clear();
    let step = fixture
        .radio
        .service_common_one(&fixture.port, &mut ingress, TransactionId::new(2).unwrap())
        .unwrap();
    assert!(
        matches!(step.work, crate::signalling::CommonIngressWork::ConnectionReleased { key, .. } if key == fixture.key)
    );
    assert_eq!(step.transport.start, now);
    assert_eq!(ingress.pending(), 0);
    assert_eq!(fixture.radio.assigned(fixture.key), None);
    for (tick, domain) in [(43, Domain::Cs), (44, Domain::Ps)] {
        drive(&mut fixture.link, &mut fixture.radio, tick);
        assert!(
            matches!(fixture.core.last_rua(), rua::Message::Disconnect { domain: actual, context, .. } if actual == domain && context == fixture.context)
        );
    }
    drive(&mut fixture.link, &mut fixture.radio, 45);
    assert_eq!(fixture.link.gateway().registered_ues(), 0);
    assert!(
        matches!(fixture.core.last_hnbap(), hnbap::Message::UeDeregister { context, .. } if context == fixture.context)
    );
}
