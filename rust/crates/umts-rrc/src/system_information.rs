// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_codec::{CodecError, SibType, validate_system_information};

use crate::{
    EncodedInformation,
    bits::BitWriter,
    cell_selection::SelectionMeasure,
    generated_information,
    ue_timers::{TimerError, UeTimerProfile},
};
use umts_rrc_schema::information_elements as ie;

const URA_IDENTITY_BITS: usize = 16;
const MIN_DRX: u8 = 6;
const MAX_DRX: u8 = 9;
const MIN_UL_INTERFERENCE_DBM: i16 = -110;
const MAX_UL_INTERFERENCE_DBM: i16 = -70;
const UL_INTERFERENCE_BITS: usize = 6;
const MAX_PRACH: usize = 16;
const PRACH_COUNT_BITS: usize = 4;
const PERSISTENCE_BITS: usize = 3;
const MAX_PERSISTENCE: u8 = 8;
const EXPIRATION_BITS: usize = 3;
const MAX_EXPIRATION: u8 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemInformationError {
    NoDomains,
    DrxCoefficient,
    Interference,
    Persistence,
    Expiration,
    Timer(TimerError),
    Codec(CodecError),
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum NetworkMode {
    I = 0,
    II = 1,
}

pub struct CsDomainInformation {
    pub periodic_update_decihours: u8,
    pub attach_detach: bool,
    pub drx_coefficient: u8,
}

pub struct PsDomainInformation {
    pub routing_area_code: u8,
    pub network_mode: NetworkMode,
    pub drx_coefficient: u8,
}

pub struct Sib1 {
    pub location_area_code: u16,
    pub cs: Option<CsDomainInformation>,
    pub ps: Option<PsDomainInformation>,
}

impl Sib1 {
    pub fn encode(&self) -> Result<EncodedInformation, SystemInformationError> {
        self.encode_with_timers(&UeTimerProfile::default())
    }

    // TS 24.008 10.5.1.12 defines the two-octet common and domain NAS contents.
    pub fn encode_with_timers(
        &self,
        timers: &UeTimerProfile,
    ) -> Result<EncodedInformation, SystemInformationError> {
        let count = usize::from(self.cs.is_some()) + usize::from(self.ps.is_some());
        if count == 0 {
            return Err(SystemInformationError::NoDomains);
        }
        if self
            .cs
            .as_ref()
            .map(|domain| domain.drx_coefficient)
            .into_iter()
            .chain(self.ps.as_ref().map(|domain| domain.drx_coefficient))
            .any(|value| !(MIN_DRX..=MAX_DRX).contains(&value))
        {
            return Err(SystemInformationError::DrxCoefficient);
        }
        let mut domains = Vec::with_capacity(count);
        if let Some(domain) = &self.cs {
            domains.push(ie::CNDomainSysInfo {
                cn_domain_identity: ie::CNDomainIdentity::cs_domain,
                cn_type: ie::CNDomainSysInfoCnType::gsm_MAP(ie::NASSystemInformationGSMMAP(
                    vec![
                        domain.periodic_update_decihours,
                        u8::from(domain.attach_detach),
                    ]
                    .into(),
                )),
                cn_drx_cycle_length_coeff: ie::CNDRXCycleLengthCoefficient(domain.drx_coefficient),
            });
        }
        if let Some(domain) = &self.ps {
            domains.push(ie::CNDomainSysInfo {
                cn_domain_identity: ie::CNDomainIdentity::ps_domain,
                cn_type: ie::CNDomainSysInfoCnType::gsm_MAP(ie::NASSystemInformationGSMMAP(
                    vec![domain.routing_area_code, domain.network_mode as u8].into(),
                )),
                cn_drx_cycle_length_coeff: ie::CNDRXCycleLengthCoefficient(domain.drx_coefficient),
            });
        }
        let timers = timers.generated().map_err(SystemInformationError::Timer)?;
        generated_information::encode(
            SibType::Type1,
            &ie::SysInfoType1 {
                cn_common_gsm_map_nas_sys_info: ie::NASSystemInformationGSMMAP(
                    self.location_area_code.to_be_bytes().to_vec().into(),
                ),
                cn_domain_sys_info_list: ie::CNDomainSysInfoList(domains),
                ue_conn_timers_and_constants: timers.connected,
                ue_idle_timers_and_constants: Some(timers.idle),
                v3a0_non_critical_extensions: timers.extensions,
            },
        )
        .map_err(SystemInformationError::Codec)
    }
}

/// TS 25.331 8.6.2.1: a UE in URA_PCH whose URA identity is not listed here starts a URA update.
pub struct Sib2 {
    pub ura_identity: u16,
}

impl Sib2 {
    pub fn encode(&self) -> Result<EncodedInformation, SystemInformationError> {
        let mut identity = rasn::types::FixedBitString::<URA_IDENTITY_BITS>::default();
        for index in 0..URA_IDENTITY_BITS {
            identity.set(
                index,
                self.ura_identity & (1 << (URA_IDENTITY_BITS - 1 - index)) != 0,
            );
        }
        generated_information::encode(
            SibType::Type2,
            &ie::SysInfoType2::new(ie::URAIdentityList(vec![ie::URAIdentity(identity)]), None),
        )
        .map_err(SystemInformationError::Codec)
    }
}

pub struct Sib11 {
    pub selection_measure: SelectionMeasure,
}

impl Sib11 {
    // TS 25.331 8.1.1.6.11 and MeasurementControlSysInfo specify the idle/FACH measurement quantity.
    pub fn encode(&self) -> Result<EncodedInformation, SystemInformationError> {
        use ie::MeasurementControlSysInfoUseOfHCSHcsNotUsedCellSelectQualityMeasure as Quantity;
        let quantity = match self.selection_measure {
            SelectionMeasure::Rscp => Quantity::cpich_RSCP(
                ie::MeasurementControlSysInfoUseOfHCSHcsNotUsedCellSelectQualityMeasureCpichRSCP::new(
                    None, None,
                ),
            ),
            SelectionMeasure::EcNo { .. } => Quantity::cpich_Ec_N0(
                ie::MeasurementControlSysInfoUseOfHCSHcsNotUsedCellSelectQualityMeasureCpichEcN0::new(
                    None, None,
                ),
            ),
        };
        let value = ie::SysInfoType11::new(
            false,
            None,
            ie::MeasurementControlSysInfo::new(
                ie::MeasurementControlSysInfoUseOfHCS::hcs_not_used(
                    ie::MeasurementControlSysInfoUseOfHCSHcsNotUsed::new(quantity, None),
                ),
                None,
                None,
            ),
            None,
        );
        generated_information::encode(SibType::Type11, &value)
            .map_err(SystemInformationError::Codec)
    }
}

pub struct Sib7 {
    pub uplink_interference_dbm: i16,
    pub sib5_persistence: Vec<u8>,
    pub sib6_persistence: Option<Vec<u8>>,
    pub expiration_time_factor: Option<u8>,
}

impl Sib7 {
    // TS 25.331 SysInfoType7 and UL-Interference use physical dBm, not ADC dBFS.
    pub fn encode(&self) -> Result<EncodedInformation, SystemInformationError> {
        if !(MIN_UL_INTERFERENCE_DBM..=MAX_UL_INTERFERENCE_DBM)
            .contains(&self.uplink_interference_dbm)
        {
            return Err(SystemInformationError::Interference);
        }
        for list in std::iter::once(&self.sib5_persistence).chain(self.sib6_persistence.iter()) {
            if list.is_empty()
                || list.len() > MAX_PRACH
                || list
                    .iter()
                    .any(|value| !(1..=MAX_PERSISTENCE).contains(value))
            {
                return Err(SystemInformationError::Persistence);
            }
        }
        if self
            .expiration_time_factor
            .is_some_and(|value| !(1..=MAX_EXPIRATION).contains(&value))
        {
            return Err(SystemInformationError::Expiration);
        }
        let mut output = BitWriter::default();
        output.flag(self.sib6_persistence.is_some());
        output.flag(self.expiration_time_factor.is_some());
        output.flag(false);
        output.flag(false);
        output.write(
            (self.uplink_interference_dbm - MIN_UL_INTERFERENCE_DBM) as u64,
            UL_INTERFERENCE_BITS,
        );
        write_persistence(&mut output, &self.sib5_persistence);
        if let Some(values) = &self.sib6_persistence {
            write_persistence(&mut output, values);
        }
        if let Some(value) = self.expiration_time_factor {
            output.write(u64::from(value - 1), EXPIRATION_BITS);
        }
        checked(SibType::Type7, output)
    }
}

fn write_persistence(output: &mut BitWriter, values: &[u8]) {
    output.write((values.len() - 1) as u64, PRACH_COUNT_BITS);
    for value in values {
        output.write(u64::from(value - 1), PERSISTENCE_BITS);
    }
}

pub(crate) fn checked(
    kind: SibType,
    output: BitWriter,
) -> Result<EncodedInformation, SystemInformationError> {
    validate_system_information(kind, &output.bytes, output.length)
        .map_err(SystemInformationError::Codec)?;
    Ok(EncodedInformation {
        bytes: output.bytes,
        bit_length: output.length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs() -> CsDomainInformation {
        CsDomainInformation {
            periodic_update_decihours: 10,
            attach_detach: true,
            drx_coefficient: 6,
        }
    }
    fn ps() -> PsDomainInformation {
        PsDomainInformation {
            routing_area_code: 1,
            network_mode: NetworkMode::II,
            drx_coefficient: 9,
        }
    }

    #[test]
    fn sib1_supports_cs_ps_and_combined_domain_advertisement() {
        for (cs, ps) in [
            (Some(cs()), None),
            (None, Some(ps())),
            (Some(cs()), Some(ps())),
        ] {
            let encoded = Sib1 {
                location_area_code: 0x1234,
                cs,
                ps,
            }
            .encode()
            .unwrap();
            assert!(encoded.bit_length > 30 && encoded.bit_length < 100);
        }
        assert!(matches!(
            Sib1 {
                location_area_code: 1,
                cs: None,
                ps: None
            }
            .encode(),
            Err(SystemInformationError::NoDomains)
        ));
    }

    #[test]
    fn sib2_lists_one_ura_identity_in_sixteen_bits() {
        let encoded = Sib2 {
            ura_identity: 0xa5c3,
        }
        .encode()
        .unwrap();
        // Extension absent, list length 1 of 1..8, then the identity.
        assert_eq!(encoded.bit_length, 20);
        assert_eq!(encoded.bytes, [0x0a, 0x5c, 0x30]);
        for identity in [0, 100, 0xa5c3, u16::MAX] {
            let encoded = Sib2 {
                ura_identity: identity,
            }
            .encode()
            .unwrap();
            let decoded: ie::SysInfoType2 = generated_information::decode(&encoded).unwrap();
            assert!(decoded.non_critical_extensions.is_none());
            assert_eq!(decoded.ura_identity_list.0.len(), 1);
            let bits = &decoded.ura_identity_list.0[0].0;
            let value = bits
                .iter()
                .take(URA_IDENTITY_BITS)
                .fold(0_u16, |value, bit| (value << 1) | u16::from(*bit));
            assert_eq!(value, identity);
        }
    }

    #[test]
    fn sib7_handles_optional_lists_and_range_extremes() {
        for interference in [-110, -90, -70] {
            for optional in [false, true] {
                let encoded = Sib7 {
                    uplink_interference_dbm: interference,
                    sib5_persistence: vec![1, 8],
                    sib6_persistence: optional.then(|| vec![8; 16]),
                    expiration_time_factor: optional.then_some(8),
                }
                .encode()
                .unwrap();
                assert!(encoded.bit_length < 100);
            }
        }
    }

    #[test]
    fn invalid_units_ranges_and_empty_prach_lists_are_rejected() {
        let mut value = Sib7 {
            uplink_interference_dbm: -39,
            sib5_persistence: vec![1],
            sib6_persistence: None,
            expiration_time_factor: None,
        };
        assert!(matches!(
            value.encode(),
            Err(SystemInformationError::Interference)
        ));
        value.uplink_interference_dbm = -100;
        value.sib5_persistence.clear();
        assert!(matches!(
            value.encode(),
            Err(SystemInformationError::Persistence)
        ));
        value.sib5_persistence = vec![9];
        assert!(matches!(
            value.encode(),
            Err(SystemInformationError::Persistence)
        ));
        value.sib5_persistence = vec![1];
        value.expiration_time_factor = Some(0);
        assert!(matches!(
            value.encode(),
            Err(SystemInformationError::Expiration)
        ));
    }

    #[test]
    fn literal_sib1_vector_preserves_nas_octet_order_and_spare_bits() {
        let encoded = Sib1 {
            location_area_code: 0x1234,
            cs: Some(cs()),
            ps: None,
        }
        .encode()
        .unwrap();
        assert_eq!(encoded.bit_length, 61);
        assert_eq!(
            encoded.bytes,
            [0x44, 0x48, 0xd0, 0x08, 0x50, 0x08, 0xac, 0x40]
        );
    }

    #[test]
    fn literal_sib7_vector_preserves_interference_and_optional_field_positions() {
        let encoded = Sib7 {
            uplink_interference_dbm: -100,
            sib5_persistence: vec![1],
            sib6_persistence: None,
            expiration_time_factor: Some(1),
        }
        .encode()
        .unwrap();
        assert_eq!(encoded.bit_length, 20);
        assert_eq!(encoded.bytes, [0x42, 0x80, 0x00]);
    }

    #[test]
    fn sib11_quantities_match_independent_vectors_without_inventing_measurements() {
        for (selection_measure, expected) in [
            (SelectionMeasure::Rscp, [0x00, 0x00]),
            (
                SelectionMeasure::EcNo {
                    hysteresis2_db: Some(2),
                },
                [0x01, 0x00],
            ),
        ] {
            let encoded = Sib11 { selection_measure }.encode().unwrap();
            assert_eq!(encoded.bit_length, 10);
            assert_eq!(encoded.bytes, expected);
            let decoded: ie::SysInfoType11 = generated_information::decode(&encoded).unwrap();
            assert!(!decoded.sib12indicator);
            assert!(decoded.fach_measurement_occasion_info.is_none());
            assert!(decoded.v4b0_non_critical_extensions.is_none());
            let control = decoded.measurement_control_sys_info;
            assert!(control.traffic_volume_meas_sys_info.is_none());
            assert!(control.dummy.is_none());
            let ie::MeasurementControlSysInfoUseOfHCS::hcs_not_used(hcs) = control.use_of_hcs
            else {
                panic!("unexpected HCS policy")
            };
            assert!(hcs.inter_ratmeasurement_sys_info.is_none());
            use ie::MeasurementControlSysInfoUseOfHCSHcsNotUsedCellSelectQualityMeasure as Quantity;
            match (selection_measure, hcs.cell_select_quality_measure) {
                (SelectionMeasure::Rscp, Quantity::cpich_RSCP(value)) => {
                    assert!(value.intra_freq_measurement_sys_info.is_none());
                    assert!(value.inter_freq_measurement_sys_info.is_none());
                }
                (SelectionMeasure::EcNo { .. }, Quantity::cpich_Ec_N0(value)) => {
                    assert!(value.intra_freq_measurement_sys_info.is_none());
                    assert!(value.inter_freq_measurement_sys_info.is_none());
                }
                _ => panic!("measurement quantity changed"),
            }
        }
    }
}
