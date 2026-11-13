// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod replacement;

const MAX_OPTIONAL_CANDIDATES: usize = MAX_COMBINATIONS;

struct View<'a> {
    channels: &'a [DchTransport],
    choices: &'a [Combination],
    policy: GroupPolicy,
    costs: Vec<TransportUsage>,
    ranking: Vec<usize>,
    inputs: Vec<Vec<umts_dsp::dedicated_transport::UplinkRateInput>>,
}

struct Selection<'a> {
    composition: &'a Composition,
    downlink: Option<&'a DownlinkBudget>,
    views: Vec<View<'a>>,
    direction: DchDirection,
    limit: usize,
    limits: Option<DirectionLimits>,
    uplink: Option<&'a UplinkBudget>,
    intervals: Vec<TransportInterval>,
    known: BTreeSet<Vec<usize>>,
    coverage: BTreeSet<(u8, Vec<usize>)>,
    used_formats: BTreeSet<(u8, usize)>,
    ordered: Vec<Vec<usize>>,
}

enum Admission {
    Accepted,
    Budget { required: usize },
}

impl<'a> Selection<'a> {
    #[cfg(test)]
    fn new(
        composition: &'a Composition,
        direction: DchDirection,
        limit: usize,
    ) -> Result<Self, Error> {
        Self::with_limits(composition, direction, limit, None)
    }

    #[cfg(test)]
    fn with_limits(
        composition: &'a Composition,
        direction: DchDirection,
        limit: usize,
        limits: Option<DirectionLimits>,
    ) -> Result<Self, Error> {
        Self::with_budgets(composition, direction, limit, limits, None, None)
    }

    fn with_budgets(
        composition: &'a Composition,
        direction: DchDirection,
        limit: usize,
        limits: Option<DirectionLimits>,
        uplink: Option<&'a UplinkBudget>,
        downlink: Option<&'a DownlinkBudget>,
    ) -> Result<Self, Error> {
        let mut views = Vec::with_capacity(composition.groups.len());
        let mut intervals = Vec::new();
        for group in &composition.groups {
            let (channels, choices) = match direction {
                DchDirection::Uplink => (&group.uplink, &group.ul_rows),
                DchDirection::Downlink => (&group.downlink, &group.dl_rows),
            };
            let idle = choices.first().ok_or(Error::Identity)?;
            if idle.formats.len() != channels.len() {
                return Err(Error::Identity);
            }
            for (channel, index) in channels.iter().zip(&idle.formats) {
                let format = channel.formats().nth(*index).ok_or(Error::Identity)?;
                if format.blocks != 0 && format.rlc_bits != 0 {
                    return Err(Error::CombinationClosure {
                        direction,
                        interval: channel.interval,
                    });
                }
                if direction == DchDirection::Uplink && !intervals.contains(&channel.interval) {
                    intervals.push(channel.interval);
                }
            }
            let mut costs = Vec::with_capacity(choices.len());
            let mut inputs = Vec::new();
            for choice in choices {
                if choice.formats.len() != channels.len() {
                    return Err(Error::Identity);
                }
                let usage = channels.iter().zip(&choice.formats).try_fold(
                    TransportUsage::default(),
                    |sum, (channel, index)| {
                        sum.checked_add(composition.channel_usage(direction, channel, *index)?)
                            .map_err(Error::Capability)
                    },
                )?;
                costs.push(usage);
                if let Some(budget) = uplink {
                    inputs.push(
                        channels
                            .iter()
                            .zip(&choice.formats)
                            .map(|(channel, index)| {
                                budget.input(
                                    channel,
                                    *index,
                                    composition.multiplexed(direction, channel.id),
                                )
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
            }
            let mut ranking: Vec<_> = (0..choices.len()).collect();
            ranking.sort_by_key(|index| (std::cmp::Reverse(costs[*index].bits), *index));
            views.push(View {
                channels,
                choices,
                policy: group.policy,
                costs,
                ranking,
                inputs,
            });
        }
        intervals.sort_by_key(|interval| *interval as u8);
        let used_formats: BTreeSet<_> = views
            .iter()
            .filter(|view| view.policy.retain_all())
            .flat_map(|view| {
                view.channels.iter().flat_map(|channel| {
                    (0..channel.formats().count()).map(move |index| (channel.id, index))
                })
            })
            .collect();
        if let Some(limits) = limits {
            let channels = views.iter().map(|view| view.channels.len()).sum();
            check_storage(
                direction,
                umts_rrc::capabilities::rel99::Limit::TransportChannels,
                channels,
                limits.transport_channels,
            )?;
            check_storage(
                direction,
                umts_rrc::capabilities::rel99::Limit::Formats,
                used_formats.len(),
                limits.formats,
            )?;
        }
        Ok(Self {
            composition,
            downlink,
            views,
            direction,
            limit,
            limits,
            uplink,
            intervals,
            known: BTreeSet::new(),
            coverage: BTreeSet::new(),
            used_formats,
            ordered: Vec::with_capacity(limit),
        })
    }

    fn usage(&self, row: &[usize]) -> Result<TransportUsage, Error> {
        if row.len() != self.views.len() {
            return Err(Error::Identity);
        }
        self.views
            .iter()
            .zip(row)
            .try_fold(TransportUsage::default(), |usage, (view, choice)| {
                usage
                    .checked_add(*view.costs.get(*choice).ok_or(Error::Identity)?)
                    .map_err(Error::Capability)
            })
    }

    fn validate_row(&self, row: &[usize]) -> Result<(), Error> {
        let usage = self.usage(row)?;
        if let Some(limits) = self.limits {
            limits
                .validate_usage(self.direction, usage)
                .map_err(Error::Capability)?;
        }
        if let Some(budget) = self.uplink {
            budget.check(
                self.views
                    .iter()
                    .zip(row)
                    .flat_map(|(view, choice)| view.inputs[*choice].iter().copied())
                    .collect(),
            )?;
        }
        Ok(())
    }

    fn record_formats(&self, row: &[usize], formats: &mut BTreeSet<(u8, usize)>) {
        for (view, choice) in self.views.iter().zip(row) {
            formats.extend(
                view.channels
                    .iter()
                    .zip(&view.choices[*choice].formats)
                    .map(|(channel, index)| (channel.id, *index)),
            );
        }
    }

    fn maximize(&self, row: &mut [usize], index: usize) -> Result<bool, Error> {
        let original = row[index];
        for choice in &self.views[index].ranking {
            row[index] = *choice;
            match self
                .validate_row(row)
                .and_then(|()| self.validate_downlink(std::iter::once(&row.to_vec())))
            {
                Ok(()) => return Ok(true),
                Err(Error::Plan(Rel99PlanError::Uplink(umts_dsp::DspError::Capacity))) => (),
                Err(Error::Plan(Rel99PlanError::Downlink(
                    umts_dsp::DspError::Capacity | umts_dsp::DspError::InvalidRateMatching,
                ))) => (),
                Err(Error::Capability(umts_rrc::capabilities::rel99::Error::Exceeded {
                    ..
                })) => (),
                Err(error) => return Err(error),
            }
        }
        row[index] = original;
        Ok(false)
    }

    fn project(&self, row: &[usize], interval: TransportInterval) -> Result<Vec<usize>, Error> {
        let mut result = row.to_vec();
        for (index, view) in self.views.iter().enumerate() {
            let original = &view.choices[row[index]].formats;
            let idle = &view.choices[0].formats;
            let exact = |choice: &Combination| {
                view.channels
                    .iter()
                    .enumerate()
                    .all(|(channel, transport)| {
                        choice.formats[channel]
                            == if (transport.interval as u8) < interval as u8 {
                                idle[channel]
                            } else {
                                original[channel]
                            }
                    })
            };
            let compatible = |choice: &Combination| {
                view.channels
                    .iter()
                    .enumerate()
                    .all(|(channel, transport)| {
                        match (transport.interval as u8).cmp(&(interval as u8)) {
                            std::cmp::Ordering::Less => choice.formats[channel] == idle[channel],
                            std::cmp::Ordering::Equal => {
                                choice.formats[channel] == original[channel]
                            }
                            std::cmp::Ordering::Greater => true,
                        }
                    })
            };
            result[index] = view
                .choices
                .iter()
                .position(exact)
                .or_else(|| view.choices.iter().position(compatible))
                .ok_or(Error::CombinationClosure {
                    direction: self.direction,
                    interval,
                })?;
        }
        Ok(result)
    }

    fn admit(&mut self, row: Vec<usize>) -> Result<Admission, Error> {
        self.admit_many([row])
    }

    fn coverage_key(
        &self,
        row: &[usize],
        interval: TransportInterval,
        empty_shorter: bool,
    ) -> (u8, Vec<usize>) {
        let mut formats = Vec::new();
        for (view, choice) in self.views.iter().zip(row) {
            for (index, channel) in view.channels.iter().enumerate() {
                if channel.interval as u8 > interval as u8 {
                    continue;
                }
                formats.push(
                    if empty_shorter && (channel.interval as u8) < interval as u8 {
                        view.choices[0].formats[index]
                    } else {
                        view.choices[*choice].formats[index]
                    },
                );
            }
        }
        (interval as u8, formats)
    }

    fn admit_many(
        &mut self,
        rows: impl IntoIterator<Item = Vec<usize>>,
    ) -> Result<Admission, Error> {
        let mut staged = Vec::new();
        let mut seen = BTreeSet::new();
        let mut coverage = BTreeSet::new();
        let mut formats = self.used_formats.clone();
        for row in rows {
            if !self.known.contains(&row) && seen.insert(row.clone()) {
                self.validate_row(&row)?;
                self.record_formats(&row, &mut formats);
                for interval in &self.intervals {
                    coverage.insert(self.coverage_key(&row, *interval, false));
                }
                staged.push(row);
            }
        }
        let mut cursor = 0;
        while cursor < staged.len() {
            if self.known.len() + staged.len() > self.limit {
                return Ok(Admission::Budget {
                    required: self.known.len() + staged.len(),
                });
            }
            if let Some(limits) = self.limits {
                check_storage(
                    self.direction,
                    umts_rrc::capabilities::rel99::Limit::Formats,
                    formats.len(),
                    limits.formats,
                )?;
            }
            // TS 25.331 8.6.5.2 requires each shorter-TTI empty projection, not just the requested row.
            for interval in &self.intervals {
                let required = self.coverage_key(&staged[cursor], *interval, true);
                if self.coverage.contains(&required) || coverage.contains(&required) {
                    continue;
                }
                let projected = self.project(&staged[cursor], *interval)?;
                if !self.known.contains(&projected) && seen.insert(projected.clone()) {
                    self.validate_row(&projected)?;
                    self.record_formats(&projected, &mut formats);
                    for interval in &self.intervals {
                        coverage.insert(self.coverage_key(&projected, *interval, false));
                    }
                    staged.push(projected);
                }
            }
            cursor += 1;
        }
        self.validate_downlink(staged.iter())?;
        for row in staged {
            self.known.insert(row.clone());
            self.ordered.push(row);
        }
        self.coverage.extend(coverage);
        self.used_formats = formats;
        Ok(Admission::Accepted)
    }

    fn require(&mut self, row: Vec<usize>) -> Result<(), Error> {
        self.require_many([row])
    }

    fn require_many(&mut self, rows: impl IntoIterator<Item = Vec<usize>>) -> Result<(), Error> {
        match self.admit_many(rows)? {
            Admission::Accepted => Ok(()),
            Admission::Budget { required } => Err(Error::CombinationBudget {
                direction: self.direction,
                required,
                maximum: self.limit,
            }),
        }
    }

    fn optional(&mut self, row: Vec<usize>) -> Result<(), Error> {
        match self.admit(row) {
            Ok(_)
            | Err(Error::CombinationClosure { .. })
            | Err(Error::Plan(Rel99PlanError::Uplink(umts_dsp::DspError::Capacity)))
            | Err(Error::Plan(Rel99PlanError::Downlink(
                umts_dsp::DspError::Capacity | umts_dsp::DspError::InvalidRateMatching,
            )))
            | Err(Error::Capability(umts_rrc::capabilities::rel99::Error::Exceeded { .. })) => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn validate_downlink<'b>(
        &'b self,
        staged: impl Iterator<Item = &'b Vec<usize>>,
    ) -> Result<(), Error> {
        let Some(budget) = self.downlink else {
            return Ok(());
        };
        let mut seen = BTreeSet::new();
        let rows = self
            .ordered
            .iter()
            .chain(staged)
            .filter(|row| seen.insert((*row).clone()))
            .map(|row| ProductRow {
                combination: Combination {
                    formats: self
                        .views
                        .iter()
                        .zip(row)
                        .flat_map(|(view, choice)| view.choices[*choice].formats.iter().copied())
                        .collect(),
                    gain: None,
                },
                added_active: false,
            })
            .collect::<Vec<_>>();
        budget.check(self.composition, &rows)
    }
}

fn check_storage(
    direction: DchDirection,
    limit: umts_rrc::capabilities::rel99::Limit,
    required: usize,
    supported: usize,
) -> Result<(), Error> {
    if required > supported {
        Err(Error::Capability(
            umts_rrc::capabilities::rel99::Error::Exceeded {
                direction: Some(direction),
                limit,
                required,
                supported,
            },
        ))
    } else {
        Ok(())
    }
}

fn advance(row: &mut [usize], lengths: &[usize], first_fast: bool) -> bool {
    for digit in 0..row.len() {
        let index = if first_fast {
            digit
        } else {
            row.len() - 1 - digit
        };
        row[index] += 1;
        if row[index] < lengths[index] {
            return true;
        }
        row[index] = 0;
    }
    false
}

#[cfg(test)]
pub(super) fn rows(
    composition: &Composition,
    direction: DchDirection,
    limit: usize,
    gain: GainFactors,
) -> Result<Vec<ProductRow>, Error> {
    budgeted_rows(composition, direction, limit, gain, None, None, None)
}

pub(super) fn budgeted_rows(
    composition: &Composition,
    direction: DchDirection,
    limit: usize,
    gain: GainFactors,
    limits: Option<DirectionLimits>,
    uplink: Option<&UplinkBudget>,
    downlink: Option<&DownlinkBudget>,
) -> Result<Vec<ProductRow>, Error> {
    let mut selected =
        Selection::with_budgets(composition, direction, limit, limits, uplink, downlink)?;
    let width = selected.views.len();
    let rigid: Vec<_> = selected
        .views
        .iter()
        .map(|view| {
            if view.policy.retain_all() {
                view.choices.len()
            } else {
                1
            }
        })
        .collect();
    let rigid_count = rigid
        .iter()
        .try_fold(1usize, |count, length| count.checked_mul(*length))
        .ok_or(Error::Capacity)?;
    if rigid_count > limit {
        if composition.existing && width > 1 {
            return replacement::rows(composition, selected, gain);
        }
        return Err(Error::CombinationBudget {
            direction,
            required: rigid_count,
            maximum: limit,
        });
    }
    let mut baseline = Vec::with_capacity(rigid_count);
    let mut row = vec![0; width];
    loop {
        baseline.push(row.clone());
        if !advance(&mut row, &rigid, composition.existing) {
            break;
        }
    }
    match selected.require_many(baseline) {
        Ok(()) => (),
        Err(
            Error::CombinationBudget { .. }
            | Error::Plan(Rel99PlanError::Uplink(umts_dsp::DspError::Capacity))
            | Error::Plan(Rel99PlanError::Downlink(
                umts_dsp::DspError::Capacity | umts_dsp::DspError::InvalidRateMatching,
            ))
            | Error::Capability(umts_rrc::capabilities::rel99::Error::Exceeded { .. }),
        ) if composition.existing && width > 1 => {
            return replacement::rows(composition, selected, gain);
        }
        Err(error) => return Err(error),
    }
    for index in 0..width {
        if selected.views[index].policy.retain_all() {
            continue;
        }
        let view = &selected.views[index];
        if view.channels.len() != 1 {
            return Err(Error::Identity);
        }
        let channel = &view.channels[0];
        let minimum = view
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                let format = channel.formats().nth(choice.formats[0])?;
                (format.blocks == 1 && format.rlc_bits != 0).then_some((format.rlc_bits, index))
            })
            .min()
            .map(|(_, index)| index);
        if let Some(minimum) = minimum {
            let mut row = vec![0; width];
            row[index] = minimum;
            selected.require(row)?;
        }
        let mut row = vec![0; width];
        if selected.maximize(&mut row, index)? {
            selected.require(row)?;
        }
    }
    let packets: Vec<_> = selected
        .views
        .iter()
        .enumerate()
        .filter_map(|(index, view)| {
            matches!(view.policy, GroupPolicy::Packet { .. }).then_some(index)
        })
        .collect();
    let mut rigid_row = vec![0; width];
    loop {
        for active_signalling in [false, true] {
            for first in 0..packets.len().max(1) {
                let mut row = rigid_row.clone();
                for (index, view) in selected.views.iter().enumerate() {
                    if let GroupPolicy::Packet { minimum } = view.policy {
                        row[index] =
                            usize::from(minimum[usize::from(direction == DchDirection::Downlink)]);
                    }
                }
                match selected.validate_row(&row) {
                    Ok(()) => (),
                    Err(Error::Plan(Rel99PlanError::Uplink(umts_dsp::DspError::Capacity))) => {
                        continue;
                    }
                    Err(Error::Capability(umts_rrc::capabilities::rel99::Error::Exceeded {
                        ..
                    })) => continue,
                    Err(error) => return Err(error),
                }
                if active_signalling {
                    for index in 0..width {
                        if matches!(selected.views[index].policy, GroupPolicy::Signalling) {
                            selected.maximize(&mut row, index)?;
                        }
                    }
                }
                for offset in 0..packets.len() {
                    selected.maximize(&mut row, packets[(first + offset) % packets.len()])?;
                }
                selected.require(row)?;
            }
        }
        if !advance(&mut rigid_row, &rigid, composition.existing) {
            break;
        }
    }
    let lengths: Vec<_> = selected
        .views
        .iter()
        .map(|view| view.choices.len())
        .collect();
    for choice in 1..lengths.iter().copied().max().unwrap_or(0) {
        for (index, length) in lengths.iter().enumerate() {
            if choice < *length && selected.ordered.len() < limit {
                let mut row = vec![0; width];
                row[index] = choice;
                selected.optional(row)?;
            }
        }
    }
    let mut row = vec![0; width];
    for _ in 0..MAX_OPTIONAL_CANDIDATES {
        if selected.ordered.len() == limit {
            break;
        }
        selected.optional(row.clone())?;
        if !advance(&mut row, &lengths, composition.existing) {
            break;
        }
    }
    Ok(finish_rows(composition, selected, gain))
}

fn finish_rows(
    composition: &Composition,
    selected: Selection<'_>,
    gain: GainFactors,
) -> Vec<ProductRow> {
    let direction = selected.direction;
    selected
        .ordered
        .into_iter()
        .map(|indices| {
            let mut row = ProductRow {
                combination: Combination {
                    formats: Vec::new(),
                    gain: (direction == DchDirection::Uplink).then_some(gain),
                },
                added_active: false,
            };
            for (index, (view, choice)) in selected.views.iter().zip(indices).enumerate() {
                let choice = &view.choices[choice];
                let active = composition.existing
                    && index != 0
                    && view
                        .channels
                        .iter()
                        .zip(&choice.formats)
                        .any(|(channel, index)| {
                            channel.formats().nth(*index).is_some_and(|format| {
                                format.blocks != 0
                                    && (format.rlc_bits != 0
                                        || !matches!(channel.crc, CrcSize::None))
                            })
                        });
                row = join_row(&row, choice, direction, gain, active);
            }
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: u8, interval: TransportInterval) -> DchTransport {
        DchTransport {
            id,
            interval,
            coding: ChannelCoding::Turbo,
            crc: CrcSize::Bits16,
            rate_matching_attribute: 200,
            groups: vec![DchFormatGroup {
                rlc_bits: 320,
                blocks: vec![0, 1],
            }],
        }
    }

    fn group(channels: Vec<DchTransport>, rows: &[&[usize]], retain_all: bool) -> Group {
        let choices: Vec<_> = rows
            .iter()
            .map(|row| Combination {
                formats: row.to_vec(),
                gain: None,
            })
            .collect();
        Group {
            uplink: channels.clone(),
            downlink: channels,
            ul_rows: choices.clone(),
            dl_rows: choices,
            policy: if retain_all {
                GroupPolicy::Retain
            } else {
                GroupPolicy::Signalling
            },
        }
    }

    fn composition(groups: Vec<Group>, existing: bool) -> Composition {
        Composition {
            groups,
            existing,
            bearers: Vec::new(),
            used_rb: BTreeSet::new(),
            used_ul_transport: BTreeSet::new(),
            used_dl_transport: BTreeSet::new(),
        }
    }

    #[test]
    fn complete_replacement_fits_existing_sixty_four_rows_plus_three_circuit_modes() {
        let mut packet = channel(2, TransportInterval::Ms20);
        packet.groups[0].blocks = (0..32).collect();
        let old_rows: Vec<Vec<usize>> = (0..2)
            .flat_map(|signalling| (0..32).map(move |packet| vec![signalling, packet]))
            .collect();
        let borrowed: Vec<_> = old_rows.iter().map(Vec::as_slice).collect();
        let old = group(
            vec![channel(1, TransportInterval::Ms40), packet],
            &borrowed,
            true,
        );
        let mut voice = channel(3, TransportInterval::Ms20);
        voice.groups[0].blocks = vec![0, 1, 2];
        let source = composition(
            vec![old, group(vec![voice], &[&[0], &[1], &[2]], true)],
            true,
        );
        let gain = GainFactors {
            control: 15,
            data: 15,
        };
        let result = rows(&source, DchDirection::Uplink, 64, gain).unwrap();
        assert!(result.len() <= 64);
        assert_eq!(result[0].combination.formats, [0, 0, 0]);
        for mode in 0..3 {
            assert!(
                result
                    .iter()
                    .any(|row| row.combination.formats == [0, 31, mode])
            );
            assert!(
                result
                    .iter()
                    .any(|row| row.combination.formats == [1, 0, mode])
            );
        }
        for row in &result {
            assert!(old_rows.contains(&row.combination.formats[..2].to_vec()));
        }
    }

    #[test]
    fn complete_replacement_preserves_voice_when_retained_product_exceeds_physical_capacity() {
        use umts_rrc::transport::ConvolutionalRate;

        let mut packet = channel(1, TransportInterval::Ms10);
        packet.groups[0].rlc_bits = 1272;
        packet.groups[0].blocks = vec![0, 1, 2, 3];
        let mut voice = channel(2, TransportInterval::Ms20);
        voice.coding = ChannelCoding::Convolutional(ConvolutionalRate::Third);
        voice.rate_matching_attribute = 190;
        let mut source = composition(
            vec![
                group(vec![packet], &[&[0], &[1], &[2], &[3]], true),
                group(vec![voice], &[&[0], &[1]], true),
            ],
            true,
        );
        let mut physical = crate::test_support::rel99::physical();
        physical.puncturing_percent = 76;
        let budget = UplinkBudget::new(&physical, NonZeroUsize::new(500_000).unwrap()).unwrap();
        let mut selected =
            Selection::with_budgets(&source, DchDirection::Uplink, 16, None, Some(&budget), None)
                .unwrap();
        let retained_formats = selected.used_formats.clone();
        let baseline: Vec<_> = (0..4)
            .flat_map(|packet| (0..2).map(move |voice| vec![packet, voice]))
            .collect();
        assert!(matches!(
            selected.require_many(baseline),
            Err(Error::Plan(Rel99PlanError::Uplink(
                umts_dsp::DspError::Capacity
            )))
        ));
        assert!(selected.ordered.is_empty());
        assert!(selected.known.is_empty());
        assert!(selected.coverage.is_empty());
        assert_eq!(selected.used_formats, retained_formats);
        selected.validate_row(&[3, 0]).unwrap();
        selected.validate_row(&[2, 1]).unwrap();
        let gain = GainFactors {
            control: 15,
            data: 15,
        };
        let result = budgeted_rows(
            &source,
            DchDirection::Uplink,
            16,
            gain,
            None,
            Some(&budget),
            None,
        )
        .unwrap();
        assert!(result.len() <= 16);
        assert_eq!(result[0].combination.formats, [0, 0]);
        for formats in [[3, 0], [0, 1], [2, 1]] {
            assert!(result.iter().any(|row| row.combination.formats == formats));
        }
        assert!(!result.iter().any(|row| row.combination.formats == [3, 1]));
        for row in &result {
            selected.validate_row(&row.combination.formats).unwrap();
        }
        source.existing = false;
        assert!(matches!(
            budgeted_rows(
                &source,
                DchDirection::Uplink,
                16,
                gain,
                None,
                Some(&budget),
                None,
            ),
            Err(Error::Plan(Rel99PlanError::Uplink(
                umts_dsp::DspError::Capacity
            )))
        ));
    }

    #[test]
    fn projection_budget_failure_does_not_publish_a_partial_selection() {
        let source = composition(
            vec![
                group(
                    vec![channel(1, TransportInterval::Ms20)],
                    &[&[0], &[1]],
                    false,
                ),
                group(
                    vec![channel(2, TransportInterval::Ms40)],
                    &[&[0], &[1]],
                    false,
                ),
            ],
            false,
        );
        let mut selected = Selection::new(&source, DchDirection::Uplink, 2).unwrap();
        selected.require(vec![0, 0]).unwrap();
        assert!(matches!(
            selected.admit(vec![1, 1]).unwrap(),
            Admission::Budget { required: 3 }
        ));
        assert_eq!(selected.ordered, vec![vec![0, 0]]);
        assert_eq!(selected.known.len(), 1);
        selected.require(vec![1, 0]).unwrap();
        assert_eq!(selected.ordered, vec![vec![0, 0], vec![1, 0]]);
    }

    #[test]
    fn retained_rows_without_a_required_tti_projection_are_not_silently_truncated() {
        let source = composition(
            vec![
                group(
                    vec![
                        channel(1, TransportInterval::Ms20),
                        channel(2, TransportInterval::Ms40),
                    ],
                    &[&[0, 0], &[1, 1]],
                    true,
                ),
                group(
                    vec![channel(3, TransportInterval::Ms20)],
                    &[&[0], &[1]],
                    false,
                ),
            ],
            true,
        );
        assert!(matches!(
            rows(
                &source,
                DchDirection::Uplink,
                3,
                GainFactors {
                    control: 15,
                    data: 12
                }
            ),
            Err(Error::CombinationClosure {
                interval: TransportInterval::Ms40,
                ..
            })
        ));
    }

    #[test]
    fn existing_closure_witness_can_use_different_longer_tti_formats() {
        let source = composition(
            vec![
                group(
                    vec![
                        channel(1, TransportInterval::Ms20),
                        channel(2, TransportInterval::Ms80),
                    ],
                    &[&[0, 0], &[0, 1], &[1, 0], &[1, 1]],
                    true,
                ),
                group(
                    vec![channel(3, TransportInterval::Ms40)],
                    &[&[0], &[1]],
                    false,
                ),
            ],
            true,
        );
        let mut selected = Selection::new(&source, DchDirection::Uplink, 4).unwrap();
        selected
            .require_many([vec![0, 0], vec![0, 1], vec![1, 0]])
            .unwrap();
        selected.require(vec![3, 1]).unwrap();
        assert_eq!(
            selected.ordered,
            vec![vec![0, 0], vec![0, 1], vec![1, 0], vec![3, 1]]
        );
    }
}
