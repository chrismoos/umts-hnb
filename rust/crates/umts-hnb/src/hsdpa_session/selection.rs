// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa_packet::{PlanBank, PlanFormat},
    hsdpa_tx::{AllocationCalendar, Error as AllocationError},
};

const REDUNDANCY_VERSIONS: u64 = 8;
const QAM64_TRANSMISSION_VERSIONS: [u8; REDUNDANCY_VERSIONS as usize] = [0, 5, 2, 3, 4, 1, 6, 7];

fn transmission_version(
    evolved: bool,
    qam64: bool,
    transmitted: u64,
) -> umts_dsp::hsdpa::TransmissionVersion {
    let value = if evolved {
        let index = (transmitted % REDUNDANCY_VERSIONS) as usize;
        if qam64 {
            QAM64_TRANSMISSION_VERSIONS[index]
        } else {
            index as u8
        }
    } else {
        0
    };
    umts_dsp::hsdpa::TransmissionVersion::new(value).expect("bounded redundancy version")
}

pub struct SelectionRequest<'a> {
    pub now: ChipInstant,
    pub not_before: ChipInstant,
    pub maximum_cqi_age: NonZeroU64,
    pub plans: &'a PlanBank,
    pub calendar: &'a AllocationCalendar,
    pub budget: NonZeroUsize,
}

#[derive(Clone, Copy, Debug)]
pub struct SelectedPacket {
    pub candidate: PacketCandidate,
    pub format: PlanFormat,
    pub control_code: u16,
    pub rate: Rate,
}

#[derive(Default)]
pub struct SelectionStep {
    pub visited: usize,
    pub budget_exhausted: bool,
    pub selected: Option<SelectedPacket>,
}

impl Session {
    pub fn select_packet_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        request: SelectionRequest<'_>,
    ) -> Result<SelectionStep, Error> {
        if !self.has_carrier(carrier) {
            return Err(Error::Identity);
        }

        let mut step = SelectionStep::default();
        self.links[carrier.index()]
            .feedback_snapshot
            .selection
            .calls = self.links[carrier.index()]
            .feedback_snapshot
            .selection
            .calls
            .saturating_add(1);
        if self.links[carrier.index()].feedback_policy.is_none() {
            self.links[carrier.index()]
                .feedback_snapshot
                .selection
                .no_policy = self.links[carrier.index()]
                .feedback_snapshot
                .selection
                .no_policy
                .saturating_add(1);
            return Ok(step);
        }
        let Some(cqi) = self.latest_cqi_on(carrier, request.now, request.maximum_cqi_age) else {
            self.links[carrier.index()]
                .feedback_snapshot
                .selection
                .no_fresh_cqi = self.links[carrier.index()]
                .feedback_snapshot
                .selection
                .no_fresh_cqi
                .saturating_add(1);
            return Ok(step);
        };
        let Some(mut candidate) = self.packet_candidate_on(carrier, request.not_before)? else {
            self.links[carrier.index()]
                .feedback_snapshot
                .selection
                .no_candidate = self.links[carrier.index()]
                .feedback_snapshot
                .selection
                .no_candidate
                .saturating_add(1);
            return Ok(step);
        };
        if self.procedure.construction().configuration().uses_mac_ehs() {
            self.sdu_budget
                .observe_time(request.now)
                .map_err(Error::Timing)?;
            let sdus = match candidate.work {
                PacketWork::New { queue } => self
                    .queues
                    .staged_packet(queue)
                    .map_or(sdu_budget::MAX_REORDERING_SDUS, |packet| packet.pdus),
                PacketWork::Retained { .. } => {
                    self.links[carrier.index()]
                        .packets
                        .payload(candidate.process)
                        .map_err(Error::Packet)?
                        .ok_or(Error::Packet(OwnerError::Process))?
                        .packet()
                        .pdus
                }
            };
            candidate.downlink = match self.sdu_budget.earliest(carrier, candidate.downlink, sdus) {
                Ok(downlink) => downlink,
                Err(umts_dsp::DspError::Capacity) => return Ok(step),
                Err(error) => return Err(Error::Timing(error)),
            };
        }
        let excess_steps = self.excess_rate_steps_on(carrier, request.now, request.maximum_cqi_age);
        let soft_bits = self.links[carrier.index()]
            .packets
            .soft_bits(candidate.process)
            .map_err(Error::Packet)?;
        let parameters = self.procedure.construction().parameters();
        let transmitted = self.links[carrier.index()]
            .packets
            .transmitted_count(candidate.process)
            .map_err(Error::Packet)?;
        if request.plans.h_rnti() != parameters.h_rnti {
            return Err(Error::Identity);
        }
        let maximum_bits = match candidate.work {
            PacketWork::New { queue } => self.links[carrier.index()]
                .rates
                .demand_rate_within(
                    cqi,
                    excess_steps,
                    self.queues.pending_packet_bits(queue),
                    self.shared_tti_transport_bits(queue),
                )
                .map_or(0, |rate| rate.reference.transport_bits.get()),
            PacketWork::Retained { transport_bits } => self.links[carrier.index()]
                .rates
                .demand_rate(cqi, excess_steps, transport_bits.get())
                .map_or(0, |rate| rate.reference.transport_bits.get()),
        };
        let staged_bits = match candidate.work {
            PacketWork::New { queue } => self
                .queues
                .staged_packet(queue)
                .map(|packet| packet.transport_bits),
            PacketWork::Retained { .. } => None,
        };
        let link = &mut self.links[carrier.index()];
        let rates = &link.rates;
        let candidates = |preferred| {
            rates
                .adapted_candidates(cqi, excess_steps)
                .filter(move |rate| {
                    rate.reference.transport_bits.get() <= maximum_bits
                        && (staged_bits == Some(rate.reference.transport_bits.get())) == preferred
                })
        };
        for rate in candidates(true).chain(candidates(false)) {
            let version = transmission_version(
                self.procedure.construction().configuration().uses_mac_ehs(),
                rate.reference.modulation == umts_dsp::hsdpa::Modulation::Qam64,
                transmitted,
            );
            let transport_bits = match candidate.work {
                PacketWork::New { .. } => rate.reference.transport_bits,
                PacketWork::Retained { transport_bits } => transport_bits,
            };
            for codes in rate.allocation_codes(link.rates.codes().data_count().get()) {
                for format in request
                    .plans
                    .matching_formats(transport_bits, soft_bits, codes, rate.reference.modulation)
                    .filter(|format| format.version == version)
                {
                    if step.visited == request.budget.get() {
                        step.budget_exhausted = true;
                        link.feedback_snapshot.selection.budget_exhausted = link
                            .feedback_snapshot
                            .selection
                            .budget_exhausted
                            .saturating_add(1);
                        return Ok(step);
                    }
                    step.visited += 1;
                    let pool = link.rates.codes();
                    for first_code in pool.first_data()
                        ..=pool.first_data() + pool.data_count().get() - format.codes
                    {
                        let format = PlanFormat {
                            first_code,
                            ..format
                        };
                        for (control_index, code) in pool.control_codes().enumerate() {
                            let control_list_number = control_index as u8 + 1;
                            if format.table == umts_dsp::hsdpa::TransportTable::OctetAligned
                                && format.modulation != umts_dsp::hsdpa::Modulation::Qpsk
                            {
                                let offset =
                                    (i16::from(first_code) - 1 - i16::from(format.codes / 8) * 15)
                                        .unsigned_abs();
                                if offset % 2 != u16::from(control_list_number % 2) {
                                    continue;
                                }
                            }
                            let format = PlanFormat {
                                control_list_number,
                                ..format
                            };
                            match request.calendar.available(
                                self.identity.ue,
                                candidate.downlink,
                                code.index(),
                                format.first_code,
                                format.codes,
                            ) {
                                Ok(_) => {
                                    link.feedback_snapshot.selection.selected =
                                        link.feedback_snapshot.selection.selected.saturating_add(1);
                                    step.selected = Some(SelectedPacket {
                                        candidate,
                                        format,
                                        control_code: code.index(),
                                        rate,
                                    });
                                    return Ok(step);
                                }
                                Err(AllocationError::Conflict | AllocationError::Codes) => (),
                                Err(AllocationError::Capacity) => {
                                    link.feedback_snapshot.selection.calendar_capacity = link
                                        .feedback_snapshot
                                        .selection
                                        .calendar_capacity
                                        .saturating_add(1);
                                    return Ok(step);
                                }
                                Err(error) => return Err(Error::Allocation(error)),
                            }
                        }
                    }
                }
            }
        }
        link.feedback_snapshot.selection.no_format =
            link.feedback_snapshot.selection.no_format.saturating_add(1);
        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evolved_retries_cycle_all_precompiled_versions_and_legacy_keeps_chase_combining() {
        for count in 0..2 * REDUNDANCY_VERSIONS {
            assert_eq!(
                transmission_version(true, false, count),
                umts_dsp::hsdpa::TransmissionVersion::new((count % REDUNDANCY_VERSIONS) as u8)
                    .unwrap()
            );
            assert_eq!(
                transmission_version(false, false, count),
                transmission_version(true, false, 0)
            );
        }
        let (redundancy, _) =
            transmission_version(true, false, 1).parameters(umts_dsp::hsdpa::Modulation::Qam64);
        assert!(!redundancy.prioritize_systematic);
    }

    #[test]
    fn qam64_diversifies_the_first_retry_without_changing_new_data_or_legacy() {
        let expected = [0, 5, 2, 3, 4, 1, 6, 7];
        for count in 0..2 * REDUNDANCY_VERSIONS {
            assert_eq!(
                transmission_version(true, true, count),
                umts_dsp::hsdpa::TransmissionVersion::new(
                    expected[(count % REDUNDANCY_VERSIONS) as usize]
                )
                .unwrap()
            );
            assert_eq!(
                transmission_version(false, true, count),
                transmission_version(false, false, count)
            );
        }
        let mut versions = expected;
        versions.sort_unstable();
        assert_eq!(versions, [0, 1, 2, 3, 4, 5, 6, 7]);
    }
}
