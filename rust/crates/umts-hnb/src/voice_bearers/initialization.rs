// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{dedicated_bearers::BearerId, rel99_planner::RabPlan};
use umts_iuh::{
    Domain,
    ranap::rab::{Mode, SetupOrModify, qos},
    wire::Criticality,
};
use umts_rrc::transport::TransportInterval;
use umts_types::CnDomain;
use umts_user_plane::{
    WireError,
    iuup::{
        initialization::{
            Combination, DataPduType, Formats, Initialization, MAX_RFCIS, MAX_SUBFLOWS,
            ModeVersion, Rfci, Versions,
        },
        negotiation::Cause,
    },
};

const FRAME_MILLISECONDS: u128 = 10;
const MILLISECONDS_PER_SECOND: u128 = 1000;
const FIRST_VERSION: u8 = 1;
const VERSION_ONE_MASK: u16 = 1;
const MAX_REQUESTED_COMBINATIONS: usize = 64;

#[derive(Debug)]
pub enum Error {
    Protocol(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_iuh::Error,
    ),
    Wire(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        WireError,
    ),
    Mapping,
    Unsupported,
    Capacity,
    Rate,
}

#[derive(Debug)]
pub struct Proposal {
    pub frames: Box<[Initialization]>,
    pub encoding: ModeVersion,
    pub versions: Versions,
}

#[derive(Debug)]
pub struct Contract {
    requested: Box<[Box<[u16]>]>,
    requested_versions: u16,
    guaranteed: Option<qos::Rates>,
    interval_ms: u128,
}

impl Contract {
    pub fn storage_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + std::mem::size_of_val(self.requested.as_ref())
            + self
                .requested
                .iter()
                .map(|lengths| std::mem::size_of_val(lengths.as_ref()))
                .sum::<usize>()
    }

    pub fn validate(
        &self,
        formats: &Formats,
        final_frame: bool,
        offered: Versions,
    ) -> Result<(), Cause> {
        if offered.mask() & !self.requested_versions != 0 {
            return Err(Cause::Version);
        }
        if formats.subflows() != self.requested[0].len() {
            return Err(Cause::Value);
        }
        if !self.meets_guarantee(formats.first().lengths()) {
            return Err(Cause::Rfci);
        }
        if !final_frame {
            return Ok(());
        }
        let present = |lengths: &[u16]| formats.rows().any(|row| row.lengths() == lengths);
        if self.requested.iter().all(|lengths| present(lengths)) {
            return Ok(());
        }
        // TS 25.415 6.5.2.1 permits a requested subset only when RANAP includes version 1.
        if self.requested_versions & VERSION_ONE_MASK != 0
            && self
                .requested
                .iter()
                .any(|lengths| present(lengths) && self.meets_guarantee(lengths))
        {
            return Ok(());
        }
        Err(Cause::Rfci)
    }

    fn meets_guarantee(&self, lengths: &[u16]) -> bool {
        let payload =
            lengths.iter().map(|bits| u128::from(*bits)).sum::<u128>() * MILLISECONDS_PER_SECOND;
        self.guaranteed.is_none_or(|rates| {
            rates
                .uplink()
                .into_iter()
                .chain(rates.downlink())
                .all(|rate| u128::from(rate) * self.interval_ms <= payload)
        })
    }
}

pub fn contract_from_rab(
    rab: &RabPlan,
    request: &SetupOrModify,
    implemented: Versions,
) -> Result<Contract, Error> {
    let validated = checked(rab, request, implemented)?;
    let mut requested = Vec::new();
    requested
        .try_reserve_exact(rab.combinations.len())
        .map_err(|_| Error::Capacity)?;
    requested.extend(rab.combinations.iter().map(|row| row.subflow_bits.clone()));
    Ok(Contract {
        requested: requested.into_boxed_slice(),
        requested_versions: request
            .first
            .user_plane
            .as_ref()
            .ok_or(Error::Mapping)?
            .version_mask(),
        guaranteed: rab.guaranteed,
        interval_ms: validated.interval_ms,
    })
}

struct Checked<'a> {
    parameters: &'a qos::Parameters,
    versions: Versions,
    interval_ms: u128,
    largest: usize,
}

fn checked<'a>(
    rab: &RabPlan,
    request: &'a SetupOrModify,
    implemented: Versions,
) -> Result<Checked<'a>, Error> {
    request.validate_new(Domain::Cs).map_err(Error::Protocol)?;
    let parameters = request.first.parameters.as_ref().ok_or(Error::Mapping)?;
    let user_plane = request.first.user_plane.as_ref().ok_or(Error::Mapping)?;
    if let Some(ie) = parameters
        .unknown_extension_ies()
        .find(|ie| ie.criticality == Criticality::Reject)
    {
        return Err(Error::Protocol(umts_iuh::Error::UnknownRejectIe(ie.id)));
    }
    if rab.domain != CnDomain::Cs
        || rab.rab != request.first.id.get()
        || user_plane.mode != Mode::PredefinedSdu
        || rab.maximum != parameters.effective_maximum().map_err(Error::Protocol)?
        || rab.guaranteed != parameters.effective_guaranteed().map_err(Error::Protocol)?
        || rab.maximum_sdu_bits != parameters.maximum_sdu_bits.get()
        || rab.bearers.len() != parameters.subflows.as_slice().len()
        || !(1..=MAX_SUBFLOWS).contains(&rab.bearers.len())
        || rab
            .bearers
            .iter()
            .enumerate()
            .any(|(index, id)| BearerId::new(*id).is_err() || rab.bearers[..index].contains(id))
    {
        return Err(Error::Mapping);
    }
    let count = parameters.combination_count();
    if count == 0 || count > MAX_REQUESTED_COMBINATIONS {
        return Err(Error::Capacity);
    }
    if rab.combinations.len() != count {
        return Err(Error::Mapping);
    }
    let admitted_versions = rab.circuit_versions.ok_or(Error::Mapping)?;
    if admitted_versions == 0 || admitted_versions & !user_plane.version_mask() != 0 {
        return Err(Error::Mapping);
    }
    let versions = Versions::new(admitted_versions)
        .map_err(Error::Wire)?
        .intersection(implemented)
        .ok_or(Error::Unsupported)?;
    let frames = match rab.circuit_interval.ok_or(Error::Mapping)? {
        TransportInterval::Ms10 => 1,
        TransportInterval::Ms20 => 2,
        TransportInterval::Ms40 => 4,
        TransportInterval::Ms80 => 8,
    };
    let interval_ms = frames * FRAME_MILLISECONDS;
    let maximum = [rab.maximum.uplink(), rab.maximum.downlink()];
    if maximum.iter().any(Option::is_none) {
        return Err(Error::Unsupported);
    }
    let mut largest = 0;
    let mut largest_bits = 0;
    for (index, row) in rab.combinations.iter().enumerate() {
        if *row != parameters.combination(index).map_err(Error::Protocol)?
            || row.subflow_bits.len() != rab.bearers.len()
        {
            return Err(Error::Mapping);
        }
        let bits: u128 = row.subflow_bits.iter().map(|bits| u128::from(*bits)).sum();
        let payload = bits * MILLISECONDS_PER_SECOND;
        if maximum
            .iter()
            .flatten()
            .any(|rate| payload > u128::from(*rate) * interval_ms)
            || row
                .bitrate
                .is_some_and(|rate| rate != 0 && u128::from(rate) * interval_ms != payload)
        {
            return Err(Error::Rate);
        }
        if bits > largest_bits {
            largest = index;
            largest_bits = bits;
        }
    }
    // TS 25.415 6.5.2.1 prohibits a first combination below the guaranteed bitrate.
    if rab.guaranteed.is_some_and(|rates| {
        rates
            .uplink()
            .into_iter()
            .chain(rates.downlink())
            .any(|rate| u128::from(rate) * interval_ms > largest_bits * MILLISECONDS_PER_SECOND)
    }) {
        return Err(Error::Rate);
    }
    Ok(Checked {
        parameters,
        versions,
        interval_ms,
        largest,
    })
}

pub fn from_rab(
    rab: &RabPlan,
    request: &SetupOrModify,
    implemented: Versions,
) -> Result<Proposal, Error> {
    let Checked {
        parameters,
        versions,
        largest,
        ..
    } = checked(rab, request, implemented)?;
    let count = rab.combinations.len();
    if count > MAX_RFCIS {
        return Err(Error::Capacity);
    }
    // TS 25.415 6.5.2.1 recommends the lowest version sufficient for the initialization information.
    let encoding = ModeVersion::new(
        u8::try_from(versions.mask().trailing_zeros()).map_err(|_| Error::Unsupported)?
            + FIRST_VERSION,
    )
    .map_err(Error::Wire)?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(count).map_err(|_| Error::Capacity)?;
    for index in std::iter::once(largest).chain((0..count).filter(|index| *index != largest)) {
        rows.push(
            Combination::new(
                Rfci::new(u8::try_from(index).map_err(|_| Error::Capacity)?)
                    .map_err(Error::Wire)?,
                &rab.combinations[index].subflow_bits,
                None,
            )
            .map_err(Error::Wire)?,
        );
    }
    // TS 25.415 6.5.1.1 selects PDU Type 1 only when every subflow requests no error detection.
    let data = if parameters
        .subflows
        .as_slice()
        .iter()
        .all(|flow| flow.delivery == qos::Delivery::NoDetection)
    {
        DataPduType::NoCrc
    } else {
        DataPduType::Crc
    };
    let frame = Initialization::new(&rows, versions, data, false).map_err(Error::Wire)?;
    Ok(Proposal {
        frames: vec![frame].into_boxed_slice(),
        encoding,
        versions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_iuh::ranap::rab::{Address, Association, First, Second, Transport, UserPlane};

    const RAB: u8 = 7;
    const VOICE_RATE: u32 = 12_200;
    const PEER_PORT: u16 = 4000;

    fn fixture(rows: &[&[u16]], detection: bool) -> (RabPlan, SetupOrModify) {
        let flows = (0..rows[0].len())
            .map(|index| {
                let detected = detection && index == 0;
                qos::Subflow::new(
                    qos::ResidualError::new(1, 6).unwrap(),
                    if detected {
                        qos::Delivery::Yes
                    } else {
                        qos::Delivery::NoDetection
                    },
                    detected.then(|| qos::SduError::new(1, 3).unwrap()),
                    Some(
                        qos::Formats::new(
                            rows.iter()
                                .map(|row| qos::Format::new(Some(row[index]), None).unwrap())
                                .collect(),
                        )
                        .unwrap(),
                    ),
                )
                .unwrap()
            })
            .collect();
        let maximum_bits = rows
            .iter()
            .map(|row| row.iter().copied().sum::<u16>())
            .max()
            .unwrap();
        let mut parameters = qos::Parameters::new(
            qos::TrafficClass::Conversational,
            qos::Asymmetry::SymmetricBidirectional,
            qos::MaximumRates::new(&[VOICE_RATE]).unwrap(),
            qos::DeliveryOrder::Requested,
            qos::MaxSduBits::new(maximum_bits).unwrap(),
            qos::Subflows::new(flows).unwrap(),
        );
        parameters.guaranteed = Some(qos::GuaranteedRates::new(&[VOICE_RATE]).unwrap());
        parameters.transfer_delay = Some(80);
        parameters.source_statistics = Some(qos::SourceStatistics::Speech);
        let mut first = First::modification(RAB);
        first.user_plane = Some(UserPlane::new(Mode::PredefinedSdu, 3).unwrap());
        first.transport = Some(Transport::new(
            Address::from_ip(Domain::Cs, "127.0.0.2".parse().unwrap()).unwrap(),
            Association::binding([PEER_PORT.to_be_bytes()[0], PEER_PORT.to_be_bytes()[1], 0, 0]),
        ));
        first.parameters = Some(parameters);
        let request = SetupOrModify {
            first,
            second: Second::default(),
        };
        let plan = metadata(&request);
        (plan, request)
    }

    fn metadata(request: &SetupOrModify) -> RabPlan {
        let parameters = request.first.parameters.as_ref().unwrap();
        RabPlan {
            domain: CnDomain::Cs,
            rab: request.first.id.get(),
            bearers: (0..parameters.subflows.as_slice().len())
                .map(|index| 10 + index as u8)
                .collect(),
            maximum: parameters.effective_maximum().unwrap(),
            guaranteed: parameters.effective_guaranteed().unwrap(),
            packet_payload_budget_bits: None,
            packet_capacity: None,
            maximum_sdu_bits: parameters.maximum_sdu_bits.get(),
            circuit_interval: Some(TransportInterval::Ms20),
            circuit_versions: Some(request.first.user_plane.as_ref().unwrap().version_mask()),
            combinations: (0..parameters.combination_count())
                .map(|index| parameters.combination(index).unwrap())
                .collect(),
        }
    }

    fn proposal(plan: &RabPlan, request: &SetupOrModify) -> Proposal {
        from_rab(plan, request, Versions::new(3).unwrap()).unwrap()
    }

    #[test]
    fn exact_indexed_requested_rows_are_reordered_largest_first_without_catalog_extras() {
        let (plan, request) = fixture(&[&[0, 0, 0], &[39, 0, 0], &[81, 103, 60]], true);
        let proposal = proposal(&plan, &request);
        assert_eq!(proposal.frames.len(), 1);
        let frame = &proposal.frames[0];
        assert!(!frame.chained());
        assert_eq!(frame.data_type(), DataPduType::Crc);
        assert_eq!(proposal.encoding.number(), 1);
        assert_eq!(proposal.versions.mask(), 3);
        let rows: Vec<_> = frame
            .rows()
            .map(|row| (row.rfci().get(), row.lengths().to_vec()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (2, vec![81, 103, 60]),
                (0, vec![0, 0, 0]),
                (1, vec![39, 0, 0])
            ]
        );
        assert!(frame.rows().all(|row| row.ipti().is_none()));
        let mut bytes = vec![0; frame.encoded_bytes()];
        let length = frame.encode(&mut bytes).unwrap();
        assert_eq!(Initialization::decode(&bytes[..length]).unwrap(), *frame);
    }

    #[test]
    fn tied_largest_rows_keep_original_order_and_no_detection_selects_type_one() {
        let (plan, request) = fixture(&[&[0, 0, 0], &[81, 103, 60], &[75, 109, 60]], false);
        let result = proposal(&plan, &request);
        assert_eq!(result.frames[0].data_type(), DataPduType::NoCrc);
        assert_eq!(
            result.frames[0]
                .rows()
                .map(|row| row.rfci().get())
                .collect::<Vec<_>>(),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn offered_versions_are_admitted_intersection_and_encoding_is_lowest_offered() {
        let (mut plan, request) = fixture(&[&[81, 103, 60]], true);
        plan.circuit_versions = Some(2);
        let result = proposal(&plan, &request);
        assert_eq!(result.versions.mask(), 2);
        assert_eq!(result.encoding.number(), 2);
        assert!(matches!(
            from_rab(&plan, &request, Versions::new(1).unwrap()),
            Err(Error::Unsupported)
        ));
        plan.circuit_versions = Some(4);
        assert!(matches!(
            from_rab(&plan, &request, Versions::new(7).unwrap()),
            Err(Error::Mapping)
        ));
    }

    #[test]
    fn mismatched_identity_subflows_rates_and_requested_combinations_are_rejected() {
        let (plan, request) = fixture(&[&[0, 0, 0], &[81, 103, 60]], true);
        for index in 0..7 {
            let mut modified = plan.clone();
            match index {
                0 => modified.domain = CnDomain::Ps,
                1 => modified.rab += 1,
                2 => modified.bearers[1] = modified.bearers[0],
                3 => {
                    modified.bearers.pop().unwrap();
                }
                4 => modified.maximum_sdu_bits -= 1,
                5 => modified.guaranteed = None,
                6 => modified.combinations.swap(0, 1),
                _ => unreachable!(),
            }
            assert!(matches!(
                from_rab(&modified, &request, Versions::new(3).unwrap()),
                Err(Error::Mapping)
            ));
        }
    }

    #[test]
    fn first_row_and_every_other_row_must_fit_actual_admitted_interval() {
        let (mut plan, mut request) = fixture(&[&[0, 0, 0], &[81, 103, 60]], true);
        plan.circuit_interval = Some(TransportInterval::Ms10);
        assert!(matches!(
            from_rab(&plan, &request, Versions::new(3).unwrap()),
            Err(Error::Rate)
        ));
        plan.circuit_interval = Some(TransportInterval::Ms40);
        assert!(matches!(
            from_rab(&plan, &request, Versions::new(3).unwrap()),
            Err(Error::Rate)
        ));
        let params = request.first.parameters.as_mut().unwrap();
        params.guaranteed = Some(qos::GuaranteedRates::new(&[6000]).unwrap());
        plan = metadata(&request);
        plan.circuit_interval = Some(TransportInterval::Ms40);
        assert!(from_rab(&plan, &request, Versions::new(3).unwrap()).is_ok());
    }

    #[test]
    fn sixty_four_requested_rows_fail_explicitly_without_truncation() {
        let rows = vec![&[81, 103, 60][..]; MAX_RFCIS + 1];
        let (plan, request) = fixture(&rows, true);
        assert!(matches!(
            from_rab(&plan, &request, Versions::new(3).unwrap()),
            Err(Error::Capacity)
        ));
        let rows = vec![&[81, 103, 60][..]; MAX_RFCIS];
        let (plan, request) = fixture(&rows, true);
        assert_eq!(
            proposal(&plan, &request).frames[0].rows().count(),
            MAX_RFCIS
        );
    }

    #[test]
    fn absent_or_zero_explicit_bitrate_does_not_replace_geometry_or_interval() {
        let (_, mut request) = fixture(&[&[81, 103, 60]], true);
        for rate in [None, Some(0), Some(VOICE_RATE)] {
            let parameters = request.first.parameters.as_mut().unwrap();
            let flows = parameters
                .subflows
                .as_slice()
                .iter()
                .zip([81, 103, 60])
                .map(|(flow, bits)| {
                    qos::Subflow::new(
                        flow.residual_error.clone(),
                        flow.delivery,
                        flow.sdu_error.clone(),
                        Some(
                            qos::Formats::new(vec![qos::Format::new(Some(bits), rate).unwrap()])
                                .unwrap(),
                        ),
                    )
                    .unwrap()
                })
                .collect();
            parameters.subflows = qos::Subflows::new(flows).unwrap();
            assert!(from_rab(&metadata(&request), &request, Versions::new(3).unwrap()).is_ok());
        }
        let parameters = request.first.parameters.as_mut().unwrap();
        let flows = parameters
            .subflows
            .as_slice()
            .iter()
            .zip([81, 103, 60])
            .map(|(flow, bits)| {
                qos::Subflow::new(
                    flow.residual_error.clone(),
                    flow.delivery,
                    flow.sdu_error.clone(),
                    Some(
                        qos::Formats::new(vec![
                            qos::Format::new(Some(bits), Some(11_000)).unwrap(),
                        ])
                        .unwrap(),
                    ),
                )
                .unwrap()
            })
            .collect();
        parameters.subflows = qos::Subflows::new(flows).unwrap();
        assert!(matches!(
            from_rab(&metadata(&request), &request, Versions::new(3).unwrap()),
            Err(Error::Rate)
        ));
    }

    #[test]
    fn asymmetric_directional_rate_limits_are_not_collapsed_into_one_value() {
        let (_, mut request) = fixture(&[&[81, 103, 60]], true);
        let parameters = request.first.parameters.as_mut().unwrap();
        parameters.asymmetry = qos::Asymmetry::AsymmetricBidirectional;
        parameters.maximum = qos::MaximumRates::new(&[6000, VOICE_RATE]).unwrap();
        parameters.guaranteed = Some(qos::GuaranteedRates::new(&[6000, VOICE_RATE]).unwrap());
        assert!(matches!(
            from_rab(&metadata(&request), &request, Versions::new(3).unwrap()),
            Err(Error::Rate)
        ));
        let parameters = request.first.parameters.as_mut().unwrap();
        parameters.maximum = qos::MaximumRates::new(&[VOICE_RATE, VOICE_RATE + 1000]).unwrap();
        assert!(from_rab(&metadata(&request), &request, Versions::new(3).unwrap()).is_ok());
    }

    fn formats(rows: &[(u8, &[u16])]) -> Formats {
        let rows: Vec<_> = rows
            .iter()
            .map(|(rfci, lengths)| {
                Combination::new(Rfci::new(*rfci).unwrap(), lengths, None).unwrap()
            })
            .collect();
        Formats::from_initialization(
            &Initialization::new(&rows, Versions::new(3).unwrap(), DataPduType::NoCrc, false)
                .unwrap(),
        )
    }

    #[test]
    fn receiver_contract_matches_compound_lengths_not_peer_assigned_rfci() {
        let (mut plan, mut request) = fixture(&[&[0, 0, 0], &[39, 0, 0], &[81, 103, 60]], true);
        request.first.user_plane = Some(UserPlane::new(Mode::PredefinedSdu, 2).unwrap());
        plan.circuit_versions = Some(2);
        let contract = contract_from_rab(&plan, &request, Versions::new(3).unwrap()).unwrap();
        let all = formats(&[(21, &[81, 103, 60]), (7, &[0, 0, 0]), (10, &[39, 0, 0])]);
        assert_eq!(
            contract.validate(&all, true, Versions::new(2).unwrap()),
            Ok(())
        );
        let missing = formats(&[(21, &[81, 103, 60]), (10, &[39, 0, 0])]);
        assert_eq!(
            contract.validate(&missing, true, Versions::new(2).unwrap()),
            Err(Cause::Rfci)
        );
    }

    #[test]
    fn receiver_chain_prefix_defers_coverage_but_never_first_row_guarantee() {
        let (mut plan, mut request) = fixture(&[&[0, 0, 0], &[39, 0, 0], &[81, 103, 60]], true);
        request.first.user_plane = Some(UserPlane::new(Mode::PredefinedSdu, 2).unwrap());
        plan.circuit_versions = Some(2);
        let contract = contract_from_rab(&plan, &request, Versions::new(3).unwrap()).unwrap();
        let prefix = formats(&[(5, &[81, 103, 60])]);
        assert_eq!(
            contract.validate(&prefix, false, Versions::new(2).unwrap()),
            Ok(())
        );
        assert_eq!(
            contract.validate(&prefix, true, Versions::new(2).unwrap()),
            Err(Cause::Rfci)
        );
        let too_low = formats(&[(9, &[39, 0, 0]), (3, &[81, 103, 60]), (8, &[0, 0, 0])]);
        for final_frame in [false, true] {
            assert_eq!(
                contract.validate(&too_low, final_frame, Versions::new(2).unwrap()),
                Err(Cause::Rfci)
            );
        }
    }

    #[test]
    fn version_one_alternative_allows_only_subset_with_a_requested_sufficient_row() {
        let (plan, request) = fixture(&[&[0, 0, 0], &[39, 0, 0], &[81, 103, 60]], true);
        let contract = contract_from_rab(&plan, &request, Versions::new(3).unwrap()).unwrap();
        let highest = formats(&[(17, &[81, 103, 60])]);
        assert_eq!(
            contract.validate(&highest, true, Versions::new(2).unwrap()),
            Ok(())
        );
        let extra_and_low_requested = formats(&[(17, &[80, 104, 60]), (4, &[39, 0, 0])]);
        assert_eq!(
            contract.validate(&extra_and_low_requested, true, Versions::new(3).unwrap()),
            Err(Cause::Rfci)
        );
        let with_requested = formats(&[(17, &[80, 104, 60]), (4, &[81, 103, 60])]);
        assert_eq!(
            contract.validate(&with_requested, true, Versions::new(3).unwrap()),
            Ok(())
        );
    }

    #[test]
    fn peer_may_offer_locally_unsupported_versions_only_when_originally_requested() {
        let (mut plan, mut request) = fixture(&[&[81, 103, 60]], true);
        request.first.user_plane = Some(UserPlane::new(Mode::PredefinedSdu, 5).unwrap());
        plan.circuit_versions = Some(1);
        let contract = contract_from_rab(&plan, &request, Versions::new(1).unwrap()).unwrap();
        let formats = formats(&[(11, &[81, 103, 60])]);
        assert_eq!(
            contract.validate(&formats, true, Versions::new(5).unwrap()),
            Ok(())
        );
        assert_eq!(
            contract.validate(&formats, true, Versions::new(7).unwrap()),
            Err(Cause::Version)
        );
    }

    #[test]
    fn sixty_four_requested_rows_can_be_received_as_a_legal_version_one_subset() {
        let rows: Vec<_> = (0..MAX_REQUESTED_COMBINATIONS)
            .map(|index| vec![81 + index as u16, 103 - index as u16, 60])
            .collect();
        let references: Vec<_> = rows.iter().map(Vec::as_slice).collect();
        let (plan, mut request) = fixture(&references, true);
        let contract = contract_from_rab(&plan, &request, Versions::new(3).unwrap()).unwrap();
        assert_eq!(
            contract.validate(
                &formats(&[(62, &[81, 103, 60])]),
                true,
                Versions::new(3).unwrap()
            ),
            Ok(())
        );
        assert!(matches!(
            from_rab(&plan, &request, Versions::new(3).unwrap()),
            Err(Error::Capacity)
        ));
        request.first.user_plane = Some(UserPlane::new(Mode::PredefinedSdu, 2).unwrap());
        let strict =
            contract_from_rab(&metadata(&request), &request, Versions::new(3).unwrap()).unwrap();
        assert_eq!(
            strict.validate(
                &formats(&[(62, &[81, 103, 60])]),
                true,
                Versions::new(2).unwrap()
            ),
            Err(Cause::Rfci)
        );
    }

    #[test]
    fn receiver_contract_storage_counts_owned_rows_and_subflow_lengths() {
        let (plan, request) = fixture(&[&[0, 0, 0], &[81, 103, 60]], true);
        let contract = contract_from_rab(&plan, &request, Versions::new(3).unwrap()).unwrap();
        assert_eq!(
            contract.storage_bytes(),
            std::mem::size_of::<Contract>()
                + 2 * std::mem::size_of::<Box<[u16]>>()
                + 6 * std::mem::size_of::<u16>()
        );
    }

    #[test]
    fn receiver_contract_requires_each_directional_guarantee_and_correct_subflow_count() {
        let (_, mut request) = fixture(&[&[39, 0, 0], &[81, 103, 60]], true);
        let parameters = request.first.parameters.as_mut().unwrap();
        parameters.asymmetry = qos::Asymmetry::AsymmetricBidirectional;
        parameters.maximum = qos::MaximumRates::new(&[VOICE_RATE, VOICE_RATE]).unwrap();
        parameters.guaranteed = Some(qos::GuaranteedRates::new(&[0, VOICE_RATE]).unwrap());
        let contract =
            contract_from_rab(&metadata(&request), &request, Versions::new(3).unwrap()).unwrap();
        assert_eq!(
            contract.validate(
                &formats(&[(3, &[39, 0, 0])]),
                false,
                Versions::new(3).unwrap()
            ),
            Err(Cause::Rfci)
        );
        assert_eq!(
            contract.validate(&formats(&[(3, &[244])]), true, Versions::new(3).unwrap()),
            Err(Cause::Value)
        );
        assert_eq!(
            contract.validate(
                &formats(&[(3, &[81, 103, 60])]),
                true,
                Versions::new(3).unwrap()
            ),
            Ok(())
        );
    }
}
