// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

impl Composition {
    pub(in crate::rel99_planner) fn restore_packets(
        configuration: &Rel99Configuration,
        rabs: &[RabPlan],
    ) -> Result<Self, Error> {
        let mut result = Self::from_existing(configuration)?;
        let mut packet_groups = Vec::new();
        let mut assigned = [BTreeSet::new(), BTreeSet::new()];
        for rab in rabs {
            if rab.domain != CnDomain::Ps {
                continue;
            }
            let capacity = rab
                .packet_capacity
                .as_ref()
                .ok_or_else(|| crate::rel99_planner::trace_identity("restoration-1"))?;
            let [id] = rab.bearers.as_slice() else {
                return Err(Error::Unsupported);
            };
            let bearer = configuration
                .bearer(*id)
                .ok_or_else(|| crate::rel99_planner::trace_identity("restoration-2"))?;
            let mut channels = Vec::with_capacity(2);
            for (index, direction) in [DchDirection::Uplink, DchDirection::Downlink]
                .into_iter()
                .enumerate()
            {
                let mut mappings = bearer.mappings(direction);
                let mapping = mappings
                    .next()
                    .ok_or_else(|| crate::rel99_planner::trace_identity("restoration-3"))?;
                if mappings.next().is_some()
                    || !assigned[index].insert(mapping.transport)
                    || configuration.bearers().iter().any(|other| {
                        other.id != *id
                            && other
                                .mappings(direction)
                                .any(|other_mapping| other_mapping.transport == mapping.transport)
                    })
                {
                    return Err(Error::Unsupported);
                }
                let tfcs = match direction {
                    DchDirection::Uplink => configuration.uplink(),
                    DchDirection::Downlink => configuration.downlink(),
                };
                let mut channel = tfcs
                    .transport(mapping.transport)
                    .ok_or_else(|| crate::rel99_planner::trace_identity("restoration-4"))?
                    .clone();
                let [group] = channel.groups.as_mut_slice() else {
                    return Err(Error::Unsupported);
                };
                let maximum = capacity.maximum_blocks[index];
                // A zero downlink maximum identifies an HS-DSCH-only packet bearer with no DCH data blocks.
                if maximum > service::MAX_PACKET_BLOCKS
                    || capacity.minimum_blocks[index] > maximum
                    || group.blocks.iter().any(|count| *count > maximum)
                {
                    return Err(crate::rel99_planner::trace_identity("restoration-5"));
                }
                group.blocks = (0..=maximum).collect();
                channels.push(channel);
            }
            let rows = |maximum| {
                (0..=usize::from(maximum))
                    .map(|index| Combination {
                        formats: vec![index],
                        gain: None,
                    })
                    .collect()
            };
            let downlink = channels
                .pop()
                .ok_or_else(|| crate::rel99_planner::trace_identity("restoration-6"))?;
            let uplink = channels
                .pop()
                .ok_or_else(|| crate::rel99_planner::trace_identity("restoration-7"))?;
            packet_groups.push(Group {
                uplink: vec![uplink],
                downlink: vec![downlink],
                ul_rows: rows(capacity.maximum_blocks[0]),
                dl_rows: rows(capacity.maximum_blocks[1]),
                policy: GroupPolicy::Packet {
                    minimum: capacity.minimum_blocks,
                },
            });
        }
        let retained = result
            .groups
            .first_mut()
            .ok_or_else(|| crate::rel99_planner::trace_identity("restoration-8"))?;
        remove_columns(&mut retained.uplink, &mut retained.ul_rows, &assigned[0]);
        remove_columns(&mut retained.downlink, &mut retained.dl_rows, &assigned[1]);
        result.groups.extend(packet_groups);
        Ok(result)
    }
}

fn remove_columns(
    channels: &mut Vec<DchTransport>,
    rows: &mut Vec<Combination>,
    removed: &BTreeSet<u8>,
) {
    let columns: Vec<_> = channels
        .iter()
        .enumerate()
        .filter_map(|(index, channel)| (!removed.contains(&channel.id)).then_some(index))
        .collect();
    let mut projected: Vec<Combination> = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let formats = columns.iter().map(|index| row.formats[*index]).collect();
        if !projected.iter().any(|row| row.formats == formats) {
            projected.push(Combination {
                formats,
                gain: row.gain,
            });
        }
    }
    channels.retain(|channel| !removed.contains(&channel.id));
    *rows = projected;
}
