// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Error, NormalDpch, Policy, Rel99Limits};
use umts_rrc::capabilities::{CapabilityReport, hsdpa::FddHsdpa, hsupa};
use umts_types::CnDomain;
use umts_types::time::CHIPS_PER_FRAME;

// TS 25.211 Table 11 normal DPCH formats, ordered by decreasing physical capacity.
const NORMAL_FORMATS: [(u16, u8); 8] = [
    (4, 16),
    (8, 16),
    (16, 16),
    (32, 8),
    (64, 8),
    (128, 8),
    (256, 8),
    (512, 4),
];
const EDCH_COEXISTENCE_MINIMUM_SF: u16 = 16;

pub(super) fn hsdpa(report: &CapabilityReport, policy: &Policy<'_>) -> bool {
    policy.hsdpa && matches!(report.hsdpa(), FddHsdpa::Supported(_))
}

pub(super) fn prefer_packet_capacity(
    physical: &mut [NormalDpch],
    hsdpa: bool,
    domains: impl Iterator<Item = CnDomain>,
) {
    if !hsdpa && domains.into_iter().any(|domain| domain == CnDomain::Ps) {
        physical.sort_by_key(|candidate| candidate.downlink_sf);
    }
}

pub(super) fn resolve(
    report: &CapabilityReport,
    policy: &Policy<'_>,
    combined: bool,
) -> Result<(Rel99Limits, Vec<NormalDpch>), Error> {
    let mut limits = Rel99Limits::from_report(report).map_err(Error::Capability)?;
    let hsdpa = hsdpa(report, policy);
    if hsdpa {
        limits = report
            .simultaneous_dch()
            .ok_or(Error::Unsupported)?
            .apply(limits);
    }
    let coexistence =
        combined && policy.hsupa && matches!(report.hsupa(), hsupa::FddHsupa::Supported(_));
    if !hsdpa && !coexistence {
        return Ok((limits, policy.physical.to_vec()));
    }
    if coexistence {
        limits = hsupa::simultaneous_dch_limits(limits);
    }
    let physical = policy
        .physical
        .iter()
        .map(|candidate| {
            let mut candidate = if hsdpa {
                adapt(candidate, &limits)?
            } else {
                candidate.clone()
            };
            if coexistence {
                adapt_uplink(&mut candidate)?;
            }
            Ok(candidate)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok((limits, physical))
}

pub(super) fn adapt_uplink(candidate: &mut NormalDpch) -> Result<(), Error> {
    // TS 25.306 Table 4.12-1 permits 2400 DPDCH bits per frame alongside E-DCH.
    candidate.uplink_dpdch_count = 1;
    candidate.uplink_minimum_sf = candidate.uplink_minimum_sf.max(EDCH_COEXISTENCE_MINIMUM_SF);
    candidate.validate().map_err(Error::Configuration)
}

pub(super) fn combined(domains: impl Iterator<Item = CnDomain>) -> bool {
    let mut cs = false;
    let mut ps = false;
    for domain in domains {
        match domain {
            CnDomain::Cs => cs = true,
            CnDomain::Ps => ps = true,
        }
    }
    cs && ps
}

fn adapt(candidate: &NormalDpch, limits: &Rel99Limits) -> Result<NormalDpch, Error> {
    let mut physical = candidate.clone();
    physical.downlink_codes.truncate(limits.downlink_codes);
    let bits = |sf| physical.downlink_codes.len() * 2 * CHIPS_PER_FRAME as usize / usize::from(sf);
    if bits(physical.downlink_sf) > limits.downlink_physical_bits {
        let (sf, pilots) = NORMAL_FORMATS
            .into_iter()
            .find(|(sf, _)| {
                *sf >= physical.downlink_sf && bits(*sf) <= limits.downlink_physical_bits
            })
            .ok_or(Error::Unsupported)?;
        physical.downlink_sf = sf;
        physical.downlink_pilot_bits = pilots;
    }
    physical.validate().map_err(Error::Configuration)?;
    Ok(physical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_rrc::capabilities::hsdpa::SimultaneousDch;

    #[test]
    fn all_simultaneous_profiles_adapt_physical_capacity_without_changing_uplink() {
        let config: umts_config::Config = serde_yaml::from_str(include_str!(
            "../../../../../rust/test-support/fixtures/config/rel99.yaml"
        ))
        .unwrap();
        let configured = super::super::ConfiguredPolicy::new(&config).unwrap();
        let candidate = &configured.policy().physical[0];
        let original = super::super::tests::limits();
        for (profile, sf, bits, blocks, tfcs) in [
            (SimultaneousDch::Kbps32, 64, 640, 8, 32),
            (SimultaneousDch::Kbps64, 32, 3840, 8, 48),
            (SimultaneousDch::Kbps128, 16, 3840, 16, 96),
            (SimultaneousDch::Kbps384, 8, 6400, 32, 128),
        ] {
            let limits = profile.apply(original);
            assert_eq!(limits.uplink, original.uplink);
            assert_eq!(limits.uplink_physical_bits, original.uplink_physical_bits);
            assert_eq!(limits.am_buffer_bytes, original.am_buffer_bytes);
            assert_eq!(limits.downlink.transport_bits, bits);
            assert_eq!(limits.downlink.convolutional_bits, 640);
            assert_eq!(limits.downlink.transport_blocks, blocks);
            assert_eq!(limits.downlink.combinations, tfcs);
            let adapted = adapt(candidate, &limits).unwrap();
            assert_eq!(adapted.downlink_sf, sf);
            assert_eq!(adapted.uplink_minimum_sf, candidate.uplink_minimum_sf);
            assert_eq!(adapted.uplink_scrambling, candidate.uplink_scrambling);
            assert_eq!(adapted.power_control, candidate.power_control);
        }
    }

    #[test]
    fn legal_narrower_candidate_is_preserved() {
        let config: umts_config::Config = serde_yaml::from_str(include_str!(
            "../../../../../rust/test-support/fixtures/config/rel99.yaml"
        ))
        .unwrap();
        let configured = super::super::ConfiguredPolicy::new(&config).unwrap();
        let limits = SimultaneousDch::Kbps64.apply(super::super::tests::limits());
        let mut candidate = configured.policy().physical[0].clone();
        candidate.downlink_sf = 64;
        candidate.downlink_pilot_bits = 8;
        let adapted = adapt(&candidate, &limits).unwrap();
        assert_eq!(adapted, candidate);
    }
}
