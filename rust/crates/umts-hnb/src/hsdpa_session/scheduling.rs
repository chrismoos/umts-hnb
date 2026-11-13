// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_mac::hsdpa::{ProcessId, ProcessState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketWork {
    New { queue: u8 },
    Retained { transport_bits: NonZeroUsize },
}

#[derive(Clone, Copy, Debug)]
pub struct PacketCandidate {
    pub carrier: umts_types::hsdpa::CarrierId,
    pub process: ProcessId,
    pub downlink: ChipInstant,
    pub work: PacketWork,
}

impl Session {
    pub fn encode_new_packet_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        candidate: PacketCandidate,
        encoder: &mut crate::hsdpa_packet::PacketEncoder,
        buffer: crate::hsdpa_packet::BlockBuffer,
    ) -> Result<Option<crate::hsdpa_packet::BlockBuffer>, crate::hsdpa_packet::RejectedBlock> {
        use crate::hsdpa_packet::{Error as PacketError, RejectedBlock};
        let ready = self.has_carrier(carrier)
            && candidate.carrier == carrier
            && self.phase() == Phase::Active
            && self.links[carrier.index()].pending.is_none()
            && self.links[carrier.index()]
                .recycled_blocks
                .get(usize::from(candidate.process.value()))
                .is_some_and(Option::is_none)
            && self.links[carrier.index()]
                .queue_commands
                .get(usize::from(candidate.process.value()))
                .is_some_and(Option::is_none)
            && self.links[carrier.index()]
                .packets
                .next_downlink(candidate.downlink)
                == Ok(candidate.downlink);
        if !ready {
            return Err(RejectedBlock {
                error: PacketError::Phase,
                buffer,
            });
        }
        match candidate.work {
            PacketWork::Retained { .. } => Err(RejectedBlock {
                error: PacketError::Phase,
                buffer,
            }),
            PacketWork::New { queue } => self.links[carrier.index()].packets.encode(
                &mut self.queues,
                encoder,
                queue,
                candidate.process,
                buffer,
            ),
        }
    }

    pub fn packet_candidate_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        not_before: ChipInstant,
    ) -> Result<Option<PacketCandidate>, Error> {
        if !self.has_carrier(carrier) {
            return Err(Error::Identity);
        }

        if self.phase() != Phase::Active || self.links[carrier.index()].pending.is_some() {
            return Ok(None);
        }
        let activation = self.procedure.construction().activation().time();
        if not_before.epoch != activation.epoch {
            return Err(Error::Bearer(BearerError::Time));
        }
        let downlink = self.links[carrier.index()]
            .packets
            .next_downlink(ChipInstant {
                epoch: not_before.epoch,
                index: not_before.index.max(activation.index),
            })
            .map_err(Error::Packet)?;
        let processes = self.links[carrier.index()].queue_commands.len();
        for desired in [ProcessState::Ready, ProcessState::Empty] {
            for offset in 0..processes {
                let index = (self.links[carrier.index()].process_cursor + offset) % processes;
                if self.links[carrier.index()].queue_commands[index].is_some() {
                    continue;
                }
                let process = ProcessId::new(index as u8).expect("admitted HARQ process");
                if self.links[carrier.index()]
                    .packets
                    .process_state(process)
                    .map_err(Error::Packet)?
                    != desired
                {
                    continue;
                }
                let work = if desired == ProcessState::Ready {
                    let block = self.links[carrier.index()]
                        .packets
                        .payload(process)
                        .map_err(Error::Packet)?
                        .ok_or(Error::Packet(OwnerError::Process))?;
                    PacketWork::Retained {
                        transport_bits: block.transport_bits(),
                    }
                } else {
                    let queues = self.procedure.construction().parameters().queues.queues();
                    let selected = (0..queues.len())
                        .map(|offset| {
                            (self.links[carrier.index()].packet_queue_cursor + offset)
                                % queues.len()
                        })
                        .find(|&index| {
                            let queue = queues[index].id();
                            self.queues.staged_packet(queue).is_some()
                                || self.queues.queued(queue) != 0
                        });
                    let Some(index) = selected else {
                        self.links[carrier.index()]
                            .feedback_snapshot
                            .selection
                            .empty_packet_queues = self.links[carrier.index()]
                            .feedback_snapshot
                            .selection
                            .empty_packet_queues
                            .saturating_add(1);
                        return Ok(None);
                    };
                    self.links[carrier.index()].packet_queue_cursor = (index + 1) % queues.len();
                    PacketWork::New {
                        queue: queues[index].id(),
                    }
                };
                self.links[carrier.index()].process_cursor = (index + 1) % processes;
                return Ok(Some(PacketCandidate {
                    carrier,
                    process,
                    downlink,
                    work,
                }));
            }
        }
        self.links[carrier.index()]
            .feedback_snapshot
            .selection
            .no_available_process = self.links[carrier.index()]
            .feedback_snapshot
            .selection
            .no_available_process
            .saturating_add(1);
        Ok(None)
    }
}
