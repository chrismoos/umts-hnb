// SPDX-License-Identifier: AGPL-3.0-or-later

use super::DedicatedError;
use crate::establishment::DomainStarts;
use umts_rrc_schema::{information_elements as ie, pdu_definitions as pdu};
use umts_security::counters::Start;

const START_BITS: usize = 20;
const TM_ACTIVATION_MULTIPLE: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupCounters {
    new_bearer_start: Option<Start>,
    domain_starts: DomainStarts,
    transparent_cfn: Option<u8>,
}
impl SetupCounters {
    pub(super) fn from_complete(
        complete: &pdu::RadioBearerSetupComplete,
    ) -> Result<Self, DedicatedError> {
        Self::parse(
            complete.start_value.as_ref().map(start),
            complete.ul_counter_synchronisation_info.as_ref(),
            complete.count_c_activation_time.as_ref(),
        )
    }
    fn parse(
        new_bearer_start: Option<Start>,
        sync: Option<&ie::ULCounterSynchronisationInfo>,
        activation: Option<&ie::ActivationTime>,
    ) -> Result<Self, DedicatedError> {
        let domain_starts = sync
            .map(|info| {
                DomainStarts::from_wire(&info.start_list).map_err(|_| DedicatedError::Bearer)
            })
            .transpose()?
            .unwrap_or_default();
        let transparent_cfn = activation.map(|time| time.0);
        if transparent_cfn.is_some_and(|cfn| !cfn.is_multiple_of(TM_ACTIVATION_MULTIPLE)) {
            return Err(DedicatedError::Activation);
        }
        Ok(Self {
            new_bearer_start,
            domain_starts,
            transparent_cfn,
        })
    }
    pub fn new_bearer_start(self) -> Option<Start> {
        self.new_bearer_start
    }
    pub fn domain_starts(self) -> DomainStarts {
        self.domain_starts
    }
    /// This wire CFN is not an extended frame or proof of the response-to-activation timing margin.
    pub fn transparent_cfn(self) -> Option<u8> {
        self.transparent_cfn
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseCounters(SetupCounters);
impl ReleaseCounters {
    pub(super) fn from_complete(
        complete: &pdu::RadioBearerReleaseComplete,
    ) -> Result<Self, DedicatedError> {
        SetupCounters::parse(
            None,
            complete.ul_counter_synchronisation_info.as_ref(),
            complete.count_c_activation_time.as_ref(),
        )
        .map(Self)
    }
    pub fn domain_starts(self) -> DomainStarts {
        self.0.domain_starts()
    }
    /// This wire CFN is not an extended frame or proof of the response-to-activation timing margin.
    pub fn transparent_cfn(self) -> Option<u8> {
        self.0.transparent_cfn()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconfigurationCounters(SetupCounters);
impl ReconfigurationCounters {
    pub(crate) fn from_complete(
        complete: &pdu::RadioBearerReconfigurationComplete,
    ) -> Result<Self, DedicatedError> {
        SetupCounters::parse(
            None,
            complete.ul_counter_synchronisation_info.as_ref(),
            complete.count_c_activation_time.as_ref(),
        )
        .map(Self)
    }
    pub fn domain_starts(self) -> DomainStarts {
        self.0.domain_starts()
    }
    /// This wire CFN is not an extended frame or proof of the response-to-activation timing margin.
    pub fn transparent_cfn(self) -> Option<u8> {
        self.0.transparent_cfn()
    }
}
fn start(value: &ie::STARTValue) -> Start {
    // FixedBitString uses more backing bytes than the twenty significant wire bits.
    let value = value
        .0
        .iter()
        .take(START_BITS)
        .fold(0, |word, bit| (word << 1) | u32::from(*bit));
    Start::new(value).expect("twenty START bits")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rasn::types::FixedBitString;

    fn value(word: u32) -> ie::STARTValue {
        let mut bits = FixedBitString::default();
        for index in 0..START_BITS {
            bits.set(index, word & (1 << (START_BITS - index - 1)) != 0);
        }
        ie::STARTValue(bits)
    }
    fn complete() -> pdu::RadioBearerSetupComplete {
        pdu::RadioBearerSetupComplete::new(
            ie::RRCTransactionIdentifier(0),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn setup_start_and_domain_updates_are_distinct_values_not_a_latest_value_merge() {
        let mut response = complete();
        response.start_value = Some(value(0xabcde));
        response.count_c_activation_time = Some(ie::ActivationTime(0));
        response.ul_counter_synchronisation_info = Some(ie::ULCounterSynchronisationInfo::new(
            None,
            ie::STARTList(vec![
                ie::STARTSingle::new(ie::CNDomainIdentity::cs_domain, value(3)),
                ie::STARTSingle::new(ie::CNDomainIdentity::ps_domain, value(7)),
            ]),
        ));
        let parsed = SetupCounters::from_complete(&response).unwrap();
        assert_eq!(parsed.new_bearer_start().unwrap().get(), 0xabcde);
        assert_eq!(
            parsed.domain_starts(),
            DomainStarts {
                cs: Some(3),
                ps: Some(7)
            }
        );
        assert_eq!(parsed.transparent_cfn(), Some(0));
        let empty = SetupCounters::from_complete(&complete()).unwrap();
        assert_eq!(empty.new_bearer_start(), None);
        assert_eq!(empty.domain_starts(), DomainStarts::default());
        assert_eq!(empty.transparent_cfn(), None);
    }

    #[test]
    fn setup_counter_validation_rejects_duplicate_domain_and_unaligned_cfn() {
        let mut response = complete();
        for cfn in 0..=u8::MAX {
            response.count_c_activation_time = Some(ie::ActivationTime(cfn));
            assert_eq!(
                SetupCounters::from_complete(&response).is_ok(),
                cfn.is_multiple_of(TM_ACTIVATION_MULTIPLE)
            );
        }
        response.count_c_activation_time = None;
        for domains in [
            vec![],
            vec![ie::CNDomainIdentity::cs_domain; 2],
            vec![ie::CNDomainIdentity::ps_domain; 3],
        ] {
            response.ul_counter_synchronisation_info = Some(ie::ULCounterSynchronisationInfo::new(
                None,
                ie::STARTList(
                    domains
                        .into_iter()
                        .map(|domain| ie::STARTSingle::new(domain, value(0)))
                        .collect(),
                ),
            ));
            assert_eq!(
                SetupCounters::from_complete(&response),
                Err(DedicatedError::Bearer)
            );
        }
    }

    #[test]
    fn release_keeps_domain_starts_distinct_and_validates_tm_activation() {
        let mut response = pdu::RadioBearerReleaseComplete::new(
            ie::RRCTransactionIdentifier(0),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        response.ul_counter_synchronisation_info = Some(ie::ULCounterSynchronisationInfo::new(
            None,
            ie::STARTList(vec![
                ie::STARTSingle::new(ie::CNDomainIdentity::cs_domain, value(3)),
                ie::STARTSingle::new(ie::CNDomainIdentity::ps_domain, value(7)),
            ]),
        ));
        for cfn in 0..=u8::MAX {
            response.count_c_activation_time = Some(ie::ActivationTime(cfn));
            let parsed = ReleaseCounters::from_complete(&response);
            if cfn.is_multiple_of(TM_ACTIVATION_MULTIPLE) {
                let parsed = parsed.unwrap();
                assert_eq!(
                    parsed.domain_starts(),
                    DomainStarts {
                        cs: Some(3),
                        ps: Some(7)
                    }
                );
                assert_eq!(parsed.transparent_cfn(), Some(cfn));
            } else {
                assert_eq!(parsed, Err(DedicatedError::Activation));
            }
        }
        response.count_c_activation_time = None;
        response.ul_counter_synchronisation_info = Some(ie::ULCounterSynchronisationInfo::new(
            None,
            ie::STARTList(vec![
                ie::STARTSingle::new(
                    ie::CNDomainIdentity::cs_domain,
                    value(3)
                );
                2
            ]),
        ));
        assert_eq!(
            ReleaseCounters::from_complete(&response),
            Err(DedicatedError::Bearer)
        );
    }

    #[test]
    fn reconfiguration_retains_each_domain_start_and_rejects_invalid_counter_fields() {
        let mut response = pdu::RadioBearerReconfigurationComplete::new(
            ie::RRCTransactionIdentifier(0),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            ReconfigurationCounters::from_complete(&response)
                .unwrap()
                .domain_starts(),
            DomainStarts::default()
        );
        response.ul_counter_synchronisation_info = Some(ie::ULCounterSynchronisationInfo::new(
            None,
            ie::STARTList(vec![
                ie::STARTSingle::new(ie::CNDomainIdentity::cs_domain, value(3)),
                ie::STARTSingle::new(ie::CNDomainIdentity::ps_domain, value(7)),
            ]),
        ));
        for cfn in 0..=u8::MAX {
            response.count_c_activation_time = Some(ie::ActivationTime(cfn));
            let result = ReconfigurationCounters::from_complete(&response);
            if cfn.is_multiple_of(TM_ACTIVATION_MULTIPLE) {
                let counters = result.unwrap();
                assert_eq!(
                    counters.domain_starts(),
                    DomainStarts {
                        cs: Some(3),
                        ps: Some(7)
                    }
                );
                assert_eq!(counters.transparent_cfn(), Some(cfn));
            } else {
                assert_eq!(result, Err(DedicatedError::Activation));
            }
        }
        response.count_c_activation_time = None;
        let info = response.ul_counter_synchronisation_info.as_mut().unwrap();
        info.start_list.0.push(ie::STARTSingle::new(
            ie::CNDomainIdentity::cs_domain,
            value(5),
        ));
        assert_eq!(
            ReconfigurationCounters::from_complete(&response),
            Err(DedicatedError::Bearer)
        );
    }
}
