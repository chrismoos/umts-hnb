// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(super) fn rows(
    composition: &Composition,
    mut selected: Selection<'_>,
    gain: GainFactors,
) -> Result<Vec<ProductRow>, Error> {
    // TS 25.331 8.6.5.12 replaces TFCI numbering, not retained RLC or TrCH identities.
    let old = &selected.views[0];
    let circuit: BTreeSet<_> = composition
        .bearers
        .iter()
        .filter(|bearer| bearer.domain == Some(CnDomain::Cs))
        .flat_map(|bearer| bearer.mappings(selected.direction))
        .map(|mapping| mapping.transport)
        .collect();
    let circuit_columns: Vec<_> = old
        .channels
        .iter()
        .enumerate()
        .filter_map(|(index, channel)| circuit.contains(&channel.id).then_some(index))
        .collect();
    let signatures: BTreeSet<Vec<usize>> = old
        .choices
        .iter()
        .map(|choice| {
            circuit_columns
                .iter()
                .map(|column| choice.formats[*column])
                .collect()
        })
        .collect();
    let flexible_columns: Vec<_> = old
        .channels
        .iter()
        .enumerate()
        .filter_map(|(index, channel)| (!circuit.contains(&channel.id)).then_some(index))
        .collect();
    let lengths: Vec<_> = selected
        .views
        .iter()
        .enumerate()
        .map(|(index, view)| {
            if index != 0 && view.policy.retain_all() {
                view.choices.len()
            } else {
                1
            }
        })
        .collect();
    let required = lengths
        .iter()
        .try_fold(signatures.len(), |count, length| count.checked_mul(*length))
        .ok_or(Error::Capacity)?;
    if required > selected.limit {
        return Err(Error::CombinationBudget {
            direction: selected.direction,
            required,
            maximum: selected.limit,
        });
    }
    let mut base = vec![0; selected.views.len()];
    loop {
        for signature in &signatures {
            let candidates: Vec<_> = selected.views[0]
                .choices
                .iter()
                .enumerate()
                .filter_map(|(index, choice)| {
                    circuit_columns
                        .iter()
                        .zip(signature)
                        .all(|(column, format)| choice.formats[*column] == *format)
                        .then_some(index)
                })
                .collect();
            let mut row = base.clone();
            let least =
                select(&selected, &row, &candidates, None, false)?.ok_or(Error::Capacity)?;
            row[0] = least;
            selected.require(row.clone())?;
            for column in &flexible_columns {
                for peak in [false, true] {
                    if let Some(choice) =
                        select(&selected, &base, &candidates, Some(*column), peak)?
                    {
                        let mut active = base.clone();
                        active[0] = choice;
                        selected.require(active)?;
                    }
                }
            }
            for index in 1..selected.views.len() {
                if selected.views[index].policy.retain_all() {
                    continue;
                }
                let mut active = row.clone();
                if let GroupPolicy::Packet { minimum } = selected.views[index].policy {
                    active[index] = usize::from(
                        minimum[usize::from(selected.direction == DchDirection::Downlink)],
                    );
                    selected.require(active.clone())?;
                }
                if selected.maximize(&mut active, index)? {
                    selected.require(active)?;
                }
            }
        }
        if !advance(&mut base, &lengths, true) {
            break;
        }
    }
    let lengths: Vec<_> = selected
        .views
        .iter()
        .map(|view| view.choices.len())
        .collect();
    let mut row = vec![0; lengths.len()];
    for _ in 0..MAX_OPTIONAL_CANDIDATES {
        if selected.ordered.len() == selected.limit {
            break;
        }
        selected.optional(row.clone())?;
        if !advance(&mut row, &lengths, true) {
            break;
        }
    }
    Ok(finish_rows(composition, selected, gain))
}

fn select(
    selected: &Selection<'_>,
    base: &[usize],
    candidates: &[usize],
    column: Option<usize>,
    peak: bool,
) -> Result<Option<usize>, Error> {
    let mut row = base.to_vec();
    let mut best: Option<(usize, usize, usize)> = None;
    for choice in candidates {
        row[0] = *choice;
        let bits = if let Some(column) = column {
            let view = &selected.views[0];
            let format = view.channels[column]
                .formats()
                .nth(view.choices[*choice].formats[column])
                .ok_or(Error::Identity)?;
            let bits = usize::from(format.rlc_bits) * usize::from(format.blocks);
            if bits == 0 {
                continue;
            }
            bits
        } else {
            0
        };
        match selected
            .validate_row(&row)
            .and_then(|()| selected.validate_downlink(std::iter::once(&row)))
        {
            Ok(()) => (),
            Err(Error::Plan(Rel99PlanError::Uplink(umts_dsp::DspError::Capacity)))
            | Err(Error::Plan(Rel99PlanError::Downlink(
                umts_dsp::DspError::Capacity | umts_dsp::DspError::InvalidRateMatching,
            )))
            | Err(Error::Capability(umts_rrc::capabilities::rel99::Error::Exceeded { .. })) => {
                continue;
            }
            Err(error) => return Err(error),
        }
        let cost = selected.views[0].costs[*choice].bits;
        if best.is_none_or(|(_, old_bits, old_cost)| {
            (bits != old_bits
                && if peak {
                    bits > old_bits
                } else {
                    bits < old_bits
                })
                || (bits == old_bits && cost < old_cost)
        }) {
            best = Some((*choice, bits, cost));
        }
    }
    Ok(best.map(|(choice, _, _)| choice))
}
