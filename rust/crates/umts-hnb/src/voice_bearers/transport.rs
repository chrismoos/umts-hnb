// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    ControlPublishError, ControlRtpError, Error, Negotiation, UplinkPublishError, VoiceHandle,
    VoiceTraffic,
};
use std::{
    io,
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    time::Instant,
};
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant};
use umts_user_plane::{
    iuup::{self, Acknowledgment, Frame, initialization::Rfci, negotiation::AckEffect},
    media::RtpAnchor,
    packet::BoundDatagrams,
    rtp,
    uplink_media::Progress,
};

const MAX_DATAGRAM_BYTES: usize = 65_507;
const MAX_RECEIVE_BUDGET: usize = 64;
const MAX_PAYLOAD_TYPE: u8 = 127;
const RTP_HALF_RANGE: u32 = 1 << 31;

#[derive(Clone, Copy, Debug)]
pub struct PlayoutSettings {
    pub lead_chips: NonZeroU64,
    pub horizon_ticks: NonZeroU32,
}

#[derive(Clone, Copy, Debug)]
pub struct PlayoutWindow {
    pub observed: ChipInstant,
    pub earliest: ChipInstant,
}

#[derive(Clone, Copy)]
enum Playout {
    Explicit(RtpAnchor),
    Route {
        window: PlayoutWindow,
        settings: PlayoutSettings,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct IngressSettings {
    pub datagram_bytes: NonZeroUsize,
    pub payload_type: u8,
    pub ssrc: Option<u32>,
}

#[derive(Debug)]
pub enum TransportError {
    Configuration,
    Io(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        io::Error,
    ),
    Route(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        Error,
    ),
    Control(ControlPublishError<ControlRtpError<io::Error>>),
    Uplink(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        UplinkPublishError<io::Error>,
    ),
}

impl TransportError {
    pub fn backpressure(&self) -> bool {
        matches!(
            self,
            Self::Control(ControlPublishError::Publication(
                umts_user_plane::iuup::negotiation::PublishError::Send(
                    ControlRtpError::Backpressure
                )
            ))
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceIngress {
    pub received: usize,
    pub accepted: usize,
    pub wrong_peer: usize,
    pub wrong_payload_type: usize,
    pub wrong_source: usize,
    pub malformed: usize,
    pub rejected: usize,
    pub awaiting_publication: bool,
}

#[must_use = "Retire the endpoint and its exact voice route together"]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct VoiceTransport<D> {
    endpoint: Option<D>,
    handle: VoiceHandle,
    peer: SocketAddr,
    local: SocketAddr,
    settings: IngressSettings,
    ssrc: Option<u32>,
    anchor: Option<RtpAnchor>,
    last_radio: Option<ChipInstant>,
    receive: Box<[u8]>,
    control: Box<[u8]>,
    transmit: Box<[u8]>,
}

impl VoiceTraffic {
    pub fn attach_transport<D: BoundDatagrams>(
        &mut self,
        handle: VoiceHandle,
        endpoint: D,
        peer: SocketAddr,
        settings: IngressSettings,
    ) -> Result<VoiceTransport<D>, TransportError> {
        let bytes = settings.datagram_bytes.get();
        let local = endpoint.local_address().map_err(TransportError::Io)?;
        if settings.payload_type > MAX_PAYLOAD_TYPE
            || bytes <= rtp::HEADER_BYTES
            || bytes > MAX_DATAGRAM_BYTES
            || peer.port() == 0
            || !peer.port().is_multiple_of(2)
            || peer.ip().is_unspecified()
            || peer.ip().is_multicast()
            || local.port() == 0
            || !local.port().is_multiple_of(2)
            || peer.is_ipv4() != local.is_ipv4()
        {
            return Err(TransportError::Configuration);
        }
        let control_bytes = bytes - rtp::HEADER_BYTES;
        let storage = bytes
            .checked_mul(2)
            .and_then(|value| value.checked_add(control_bytes))
            .ok_or(TransportError::Configuration)?;
        let reserved = self
            .reserved
            .checked_add(storage)
            .filter(|value| *value <= self.limits.maximum_storage_bytes)
            .ok_or(TransportError::Route(Error::Capacity))?;
        let binding = self.binding_mut(handle).map_err(TransportError::Route)?;
        if binding.transport_capacity.is_some() {
            return Err(TransportError::Route(Error::Attached));
        }
        if matches!(&binding.negotiation, Negotiation::Sender(sender) | Negotiation::Reinitializing { sender, .. }
            if sender.maximum_frame_bytes() > control_bytes
                || sender.proposed_formats().maximum_frame_bytes() >= control_bytes)
        {
            return Err(TransportError::Route(Error::Capacity));
        }
        let formats = match &binding.negotiation {
            Negotiation::Sender(sender) => Some(sender.proposed_formats()),
            Negotiation::Receiver(receiver) | Negotiation::Reinitializing { receiver, .. } => {
                receiver
                    .pending_negotiated()
                    .or_else(|| receiver.negotiated())
                    .map(|value| value.formats())
            }
        };
        if formats.is_some_and(|formats| formats.maximum_frame_bytes() >= control_bytes) {
            return Err(TransportError::Route(Error::Capacity));
        }
        binding.transport_capacity = NonZeroUsize::new(control_bytes);
        self.reserved = reserved;
        Ok(VoiceTransport {
            endpoint: Some(endpoint),
            handle,
            peer,
            local,
            settings,
            ssrc: settings.ssrc,
            anchor: None,
            last_radio: None,
            receive: vec![0; bytes].into_boxed_slice(),
            control: vec![0; control_bytes].into_boxed_slice(),
            transmit: vec![0; bytes].into_boxed_slice(),
        })
    }
}

impl<D: BoundDatagrams> VoiceTransport<D> {
    pub(super) fn radio_retirement_storage(&self, traffic: &VoiceTraffic) -> Result<usize, Error> {
        let binding = traffic
            .bindings
            .get(&self.handle.key)
            .filter(|binding| binding.handle == self.handle)
            .ok_or(Error::Mapping)?;
        if self.endpoint.is_none()
            || binding.transport_capacity.map(NonZeroUsize::get) != Some(self.control.len())
        {
            return Err(Error::Attached);
        }
        self.receive
            .len()
            .checked_add(self.control.len())
            .and_then(|bytes| bytes.checked_add(self.transmit.len()))
            .ok_or(Error::Capacity)
    }

    pub(super) fn commit_radio_retirement(&mut self) {
        self.endpoint = None;
        self.receive = Box::default();
        self.control = Box::default();
        self.transmit = Box::default();
    }

    pub fn handle(&self) -> VoiceHandle {
        self.handle
    }
    #[cfg(test)]
    pub fn local_address(&self) -> SocketAddr {
        self.local
    }
    #[cfg(test)]
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }
    #[cfg(test)]
    pub fn source(&self) -> Option<u32> {
        self.ssrc
    }

    pub fn receive(
        &mut self,
        traffic: &mut VoiceTraffic,
        now: Instant,
        anchor: RtpAnchor,
        budget: NonZeroUsize,
    ) -> Result<VoiceIngress, TransportError> {
        self.receive_with(traffic, now, Playout::Explicit(anchor), budget)
    }

    #[cfg(test)]
    pub fn receive_at(
        &mut self,
        traffic: &mut VoiceTraffic,
        now: Instant,
        radio: ChipInstant,
        settings: PlayoutSettings,
        budget: NonZeroUsize,
    ) -> Result<VoiceIngress, TransportError> {
        self.receive_in(
            traffic,
            now,
            PlayoutWindow {
                observed: radio,
                earliest: radio,
            },
            settings,
            budget,
        )
    }

    pub fn receive_in(
        &mut self,
        traffic: &mut VoiceTraffic,
        now: Instant,
        window: PlayoutWindow,
        settings: PlayoutSettings,
        budget: NonZeroUsize,
    ) -> Result<VoiceIngress, TransportError> {
        if settings.horizon_ticks.get() >= RTP_HALF_RANGE || budget.get() > MAX_RECEIVE_BUDGET {
            return Err(TransportError::Configuration);
        }
        if window.observed.epoch != window.earliest.epoch
            || window.observed.epoch != self.handle.key.connection.ue.epoch
        {
            return Err(TransportError::Route(Error::Radio));
        }
        if self.last_radio.is_some_and(|previous| {
            previous.epoch != window.observed.epoch || previous.index > window.observed.index
        }) {
            return Err(TransportError::Route(Error::Radio));
        }
        if traffic.queued(self.handle).is_some() {
            traffic
                .retire_before(self.handle, window.earliest)
                .map_err(TransportError::Route)?;
        }
        let progress =
            self.receive_with(traffic, now, Playout::Route { window, settings }, budget)?;
        self.last_radio = Some(window.observed);
        Ok(progress)
    }

    fn receive_with(
        &mut self,
        traffic: &mut VoiceTraffic,
        now: Instant,
        playout: Playout,
        budget: NonZeroUsize,
    ) -> Result<VoiceIngress, TransportError> {
        if budget.get() > MAX_RECEIVE_BUDGET {
            return Err(TransportError::Configuration);
        }
        traffic
            .tick_initialization(self.handle, now)
            .map_err(TransportError::Route)?;
        let endpoint = self
            .endpoint
            .as_mut()
            .ok_or(TransportError::Route(Error::Mapping))?;
        let mut progress = VoiceIngress::default();
        for _ in 0..budget.get() {
            let binding = traffic
                .binding_mut(self.handle)
                .map_err(TransportError::Route)?;
            if binding.transport_capacity.is_none() {
                return Err(TransportError::Route(Error::Mapping));
            }
            if matches!(&binding.negotiation, Negotiation::Receiver(receiver) | Negotiation::Reinitializing { receiver, .. } if receiver.response_pending())
            {
                progress.awaiting_publication = true;
                break;
            }
            let Some((length, from)) = endpoint
                .receive(&mut self.receive)
                .map_err(TransportError::Io)?
            else {
                break;
            };
            progress.received += 1;
            if from != self.peer {
                progress.wrong_peer += 1;
                continue;
            }
            // A full UDP buffer cannot distinguish an exact fit from a truncated datagram.
            if length >= self.receive.len() {
                progress.malformed += 1;
                continue;
            }
            let Some(bytes) = self.receive.get(..length) else {
                progress.malformed += 1;
                continue;
            };
            let Ok(packet) = rtp::decode(bytes) else {
                progress.malformed += 1;
                continue;
            };
            // TS 25.414 §5.1.3.3.1.6 ignores received PT. §5.1.3.3.1.9 permits ignoring SSRC without RTCP.
            if self.settings.ssrc.is_some_and(|ssrc| ssrc != packet.ssrc) {
                progress.wrong_source += 1;
                continue;
            }
            let Ok(frame) = iuup::decode(packet.payload) else {
                progress.malformed += 1;
                continue;
            };
            let accepted = match frame {
                Frame::Control {
                    acknowledgment: Acknowledgment::Procedure,
                    ..
                } => match traffic.request(self.handle, frame) {
                    Ok(()) => {
                        matches!(&traffic.binding_mut(self.handle).map_err(TransportError::Route)?.negotiation,
                            Negotiation::Receiver(receiver) | Negotiation::Reinitializing { receiver, .. } if receiver.pending_accepted())
                    }
                    Err(Error::Negotiation(_)) => false,
                    Err(error) => return Err(TransportError::Route(error)),
                },
                Frame::Control { .. } => match traffic.acknowledgment(self.handle, frame, now) {
                    Ok(AckEffect::Complete | AckEffect::Next) => true,
                    Ok(_) | Err(Error::Negotiation(_)) => false,
                    Err(error) => return Err(TransportError::Route(error)),
                },
                Frame::Data { rfci, .. } => {
                    let anchor = match playout {
                        Playout::Explicit(anchor) => Some(anchor),
                        Playout::Route { window, settings } => self.anchor.or_else(|| {
                            let binding = traffic.binding_mut(self.handle).ok()?;
                            let super::RadioBinding::Active {
                                origin,
                                first_frame,
                                ..
                            } = binding.radio
                            else {
                                return None;
                            };
                            if origin.epoch != window.observed.epoch {
                                return None;
                            }
                            binding.layout.rows.get(usize::from(rfci))?.as_ref()?;
                            // Rel99 TTIs are power-of-two radio-frame periods, so their maximum is a common alignment.
                            let period = binding
                                .layout
                                .rows
                                .iter()
                                .flatten()
                                .flat_map(|row| row[..binding.layout.subflows.len()].iter())
                                .map(|geometry| geometry.period)
                                .max()
                                .unwrap_or(1);
                            let earliest = window
                                .observed
                                .index
                                .checked_add(settings.lead_chips.get())?
                                .max(window.earliest.index)
                                .max(origin.index);
                            let relative = earliest.checked_sub(origin.index)?;
                            let frames = relative.div_ceil(CHIPS_PER_FRAME);
                            let frame = first_frame
                                .checked_add(frames)?
                                .div_ceil(period)
                                .checked_mul(period)?;
                            let index = origin.index.checked_add(
                                frame
                                    .checked_sub(first_frame)?
                                    .checked_mul(CHIPS_PER_FRAME)?,
                            )?;
                            RtpAnchor::new(
                                packet.timestamp,
                                ChipInstant {
                                    epoch: window.observed.epoch,
                                    index,
                                },
                                settings.horizon_ticks,
                            )
                            .ok()
                        }),
                    };
                    match anchor {
                        Some(anchor) => {
                            let due = match playout {
                                Playout::Explicit(_) => anchor.map(packet.timestamp),
                                Playout::Route { .. } => anchor.project(packet.timestamp),
                            };
                            let Ok(due) = due else {
                                progress.rejected += 1;
                                continue;
                            };
                            let due = if matches!(playout, Playout::Route { .. }) {
                                let aligned = Rfci::new(rfci).ok().and_then(|rfci| {
                                    traffic
                                        .binding_mut(self.handle)
                                        .ok()?
                                        .align_playout(due, rfci)
                                });
                                let Some(aligned) = aligned else {
                                    progress.rejected += 1;
                                    continue;
                                };
                                aligned
                            } else {
                                due
                            };
                            if matches!(playout, Playout::Route { window, .. } if due.epoch != window.observed.epoch || due.index < window.observed.index || due.index < window.earliest.index)
                            {
                                progress.rejected += 1;
                                continue;
                            }
                            let delivery =
                                RtpAnchor::new(packet.timestamp, due, NonZeroU32::MIN)
                                    .map_err(|error| TransportError::Route(Error::Media(error)))?;
                            match traffic.downlink(self.handle, bytes, delivery) {
                                Ok(()) => {
                                    if matches!(playout, Playout::Route { .. }) {
                                        self.anchor = Some(
                                            anchor.advance(packet.timestamp).unwrap_or(anchor),
                                        );
                                    }
                                    true
                                }
                                Err(_) => false,
                            }
                        }
                        _ => false,
                    }
                }
            };
            if accepted {
                self.ssrc = Some(packet.ssrc);
                progress.accepted += 1;
            } else {
                progress.rejected += 1;
            }
        }
        Ok(progress)
    }

    pub fn publish_initialization(
        &mut self,
        traffic: &mut VoiceTraffic,
        now: Instant,
        timestamp: u32,
    ) -> Result<bool, TransportError> {
        let endpoint = self
            .endpoint
            .as_mut()
            .ok_or(TransportError::Route(Error::Mapping))?;
        traffic
            .publish_initialization_rtp_at(
                self.handle,
                now,
                timestamp,
                &mut self.control,
                &mut self.transmit,
                |bytes| endpoint.send(bytes, self.peer),
            )
            .map_err(TransportError::Control)
    }

    pub fn publish_uplink(
        &mut self,
        traffic: &mut VoiceTraffic,
        now: ChipInstant,
    ) -> Result<Progress, TransportError> {
        let endpoint = self
            .endpoint
            .as_mut()
            .ok_or(TransportError::Route(Error::Mapping))?;
        traffic
            .publish_uplink(self.handle, now, |bytes| endpoint.send(bytes, self.peer))
            .map_err(TransportError::Uplink)
    }

    pub fn retire(&mut self, traffic: &mut VoiceTraffic) -> Result<bool, TransportError> {
        if self.endpoint.is_none() {
            return Ok(false);
        }
        let binding = traffic
            .binding_mut(self.handle)
            .map_err(TransportError::Route)?;
        if binding.transport_capacity.is_none() {
            return Err(TransportError::Route(Error::Mapping));
        }
        binding.transport_capacity = None;
        traffic
            .release(self.handle)
            .map_err(TransportError::Route)?;
        traffic.reserved -= self.receive.len() + self.control.len() + self.transmit.len();
        self.endpoint = None;
        Ok(true)
    }
}
