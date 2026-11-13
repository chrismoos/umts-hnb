// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_schema::information_elements as ie;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerField {
    T300,
    N300,
    T301,
    N301,
    T302,
    N302,
    T304,
    N304,
    T305,
    T307,
    T308,
    T309,
    T310,
    N310,
    T311,
    T312,
    N312,
    T313,
    N313,
    T314,
    T315,
    N315,
    T316,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerError {
    pub field: TimerField,
    pub value: u32,
}

#[derive(Clone, Debug)]
pub struct ConnectedTimers {
    pub t301_ms: u32,
    pub n301: u8,
    pub t302_ms: u32,
    pub n302: u8,
    pub t304_ms: u32,
    pub n304: u8,
    pub t305_minutes: Option<u16>,
    pub t307_seconds: u8,
    pub t308_ms: u16,
    pub t309_seconds: u8,
    pub t310_ms: u16,
    pub n310: u8,
    pub t311_ms: u16,
    pub t312_seconds: u8,
    pub n312: u16,
    pub t313_seconds: u8,
    pub n313: u16,
    pub t314_seconds: u8,
    pub t315_seconds: u16,
    pub n315: u16,
    pub t316_seconds: Option<u8>,
}

impl Default for ConnectedTimers {
    // TS 25.331 10.3.3.43 defines these physical-unit defaults.
    fn default() -> Self {
        Self {
            t301_ms: 2000,
            n301: 2,
            t302_ms: 4000,
            n302: 3,
            t304_ms: 2000,
            n304: 2,
            t305_minutes: Some(30),
            t307_seconds: 30,
            t308_ms: 160,
            t309_seconds: 5,
            t310_ms: 160,
            n310: 4,
            t311_ms: 2000,
            t312_seconds: 1,
            n312: 1,
            t313_seconds: 3,
            n313: 20,
            t314_seconds: 12,
            t315_seconds: 180,
            n315: 1,
            t316_seconds: Some(30),
        }
    }
}

#[derive(Clone, Debug)]
pub struct IdleTimers {
    pub t300_ms: u32,
    pub n300: u8,
    pub t312_seconds: u8,
    pub n312: u16,
}

impl Default for IdleTimers {
    // TS 25.331 10.3.3.44 gives T300=1000 ms, N300=3, T312=1 s and N312=1.
    fn default() -> Self {
        Self {
            t300_ms: 1000,
            n300: 3,
            t312_seconds: 1,
            n312: 1,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct UeTimerProfile {
    // TS 25.331 10.2.48.8.4 defaults an absent connected block but leaves absent idle behavior unspecified.
    pub connected: Option<ConnectedTimers>,
    pub idle: IdleTimers,
}

impl UeTimerProfile {
    pub fn release_reference() -> Self {
        Self {
            connected: Some(ConnectedTimers {
                t308_ms: 320,
                t314_seconds: 8,
                t315_seconds: 30,
                ..ConnectedTimers::default()
            }),
            idle: IdleTimers {
                t300_ms: 2000,
                n300: 5,
                t312_seconds: 10,
                ..IdleTimers::default()
            },
        }
    }

    pub(crate) fn generated(&self) -> Result<GeneratedTimers, TimerError> {
        let connected = self
            .connected
            .as_ref()
            .map(ConnectedTimers::generated)
            .transpose()?;
        let idle = self.idle.generated()?;
        let conn_n312 = self
            .connected
            .as_ref()
            .map(|v| extension_count(v.n312, TimerField::N312))
            .transpose()?
            .flatten();
        let conn_n315 = self
            .connected
            .as_ref()
            .map(|v| extension_count(v.n315, TimerField::N315))
            .transpose()?
            .flatten();
        let idle_n312 = extension_count(self.idle.n312, TimerField::N312)?;
        let extensions = if conn_n312.is_some() || conn_n315.is_some() || idle_n312.is_some() {
            Some(ie::SysInfoType1V3a0NonCriticalExtensions {
                sys_info_type1_v3a0ext: ie::SysInfoType1V3a0extIEs {
                    ue_conn_timers_and_constants_v3a0ext: ie::UEConnTimersAndConstantsV3a0ext {
                        n_312: conn_n312,
                        n_315: conn_n315.map(|value| match value {
                            ie::N312ext::s2 => ie::N315ext::s2,
                            ie::N312ext::s4 => ie::N315ext::s4,
                            ie::N312ext::s10 => ie::N315ext::s10,
                            ie::N312ext::s20 => ie::N315ext::s20,
                        }),
                    },
                    ue_idle_timers_and_constants_v3a0ext: ie::UEIdleTimersAndConstantsV3a0ext {
                        n_312: idle_n312,
                    },
                },
                v860_non_critical_extentions: None,
            })
        } else {
            None
        };
        Ok(GeneratedTimers {
            connected,
            idle,
            extensions,
        })
    }
}

pub(crate) struct GeneratedTimers {
    pub connected: Option<ie::UEConnTimersAndConstants>,
    pub idle: ie::UEIdleTimersAndConstants,
    pub extensions: Option<ie::SysInfoType1V3a0NonCriticalExtensions>,
}

fn mapped<T: Copy, V: Into<u32>>(
    value: V,
    field: TimerField,
    values: &[(u32, T)],
) -> Result<T, TimerError> {
    let value = value.into();
    values
        .iter()
        .find_map(|&(physical, encoded)| (physical == value).then_some(encoded))
        .ok_or(TimerError { field, value })
}

fn bounded(value: u8, field: TimerField, min: u8, max: u8) -> Result<u8, TimerError> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(TimerError {
            field,
            value: u32::from(value),
        })
    }
}

macro_rules! timer_enum {
    ($value:expr,$field:ident,$ty:ident,{$($physical:literal=>$variant:ident),+$(,)?})=>{
        mapped($value,TimerField::$field,&[$(($physical,ie::$ty::$variant)),+])
    };
}

macro_rules! access_timeout {
    ($value:expr,$field:ident,$ty:ident)=>{
        timer_enum!($value,$field,$ty,{100=>ms100,200=>ms200,400=>ms400,600=>ms600,800=>ms800,1000=>ms1000,1200=>ms1200,1400=>ms1400,1600=>ms1600,1800=>ms1800,2000=>ms2000,3000=>ms3000,4000=>ms4000,6000=>ms6000,8000=>ms8000})
    };
}

fn extension_count(value: u16, field: TimerField) -> Result<Option<ie::N312ext>, TimerError> {
    match value {
        1 | 50 | 100 | 200 | 400 | 600 | 800 | 1000 => Ok(None),
        _ => timer_enum!(value,N312,N312ext,{2=>s2,4=>s4,10=>s10,20=>s20})
            .map(Some)
            .map_err(|_| TimerError {
                field,
                value: u32::from(value),
            }),
    }
}

fn base_n312(value: u16) -> Result<ie::N312, TimerError> {
    if extension_count(value, TimerField::N312)?.is_some() {
        return Ok(ie::N312::s1);
    }
    timer_enum!(value,N312,N312,{1=>s1,50=>s50,100=>s100,200=>s200,400=>s400,600=>s600,800=>s800,1000=>s1000})
}

impl IdleTimers {
    fn generated(&self) -> Result<ie::UEIdleTimersAndConstants, TimerError> {
        Ok(ie::UEIdleTimersAndConstants {
            t_300: access_timeout!(self.t300_ms, T300, T300)?,
            n_300: ie::N300(bounded(self.n300, TimerField::N300, 0, 7)?),
            t_312: ie::T312(bounded(self.t312_seconds, TimerField::T312, 1, 15)?),
            n_312: base_n312(self.n312)?,
        })
    }
}

impl ConnectedTimers {
    fn generated(&self) -> Result<ie::UEConnTimersAndConstants, TimerError> {
        let n315 = match base_n312(self.n315).map_err(|_| TimerError {
            field: TimerField::N315,
            value: u32::from(self.n315),
        })? {
            ie::N312::s1 => ie::N315::s1,
            ie::N312::s50 => ie::N315::s50,
            ie::N312::s100 => ie::N315::s100,
            ie::N312::s200 => ie::N315::s200,
            ie::N312::s400 => ie::N315::s400,
            ie::N312::s600 => ie::N315::s600,
            ie::N312::s800 => ie::N315::s800,
            ie::N312::s1000 => ie::N315::s1000,
        };
        Ok(ie::UEConnTimersAndConstants {
            t_301: access_timeout!(self.t301_ms, T301, T301)?,
            n_301: ie::N301(bounded(self.n301, TimerField::N301, 0, 7)?),
            t_302: access_timeout!(self.t302_ms, T302, T302)?,
            n_302: ie::N302(bounded(self.n302, TimerField::N302, 0, 7)?),
            t_304: timer_enum!(self.t304_ms,T304,T304,{100=>ms100,200=>ms200,400=>ms400,1000=>ms1000,2000=>ms2000})?,
            n_304: ie::N304(bounded(self.n304, TimerField::N304, 0, 7)?),
            t_305: match self.t305_minutes {
                None => ie::T305::noUpdate,
                Some(value) => {
                    timer_enum!(value,T305,T305,{5=>m5,10=>m10,30=>m30,60=>m60,120=>m120,360=>m360,720=>m720})?
                }
            },
            t_307: timer_enum!(self.t307_seconds,T307,T307,{5=>s5,10=>s10,15=>s15,20=>s20,30=>s30,40=>s40,50=>s50})?,
            t_308: timer_enum!(self.t308_ms,T308,T308,{40=>ms40,80=>ms80,160=>ms160,320=>ms320})?,
            t_309: ie::T309(bounded(self.t309_seconds, TimerField::T309, 1, 8)?),
            t_310: timer_enum!(self.t310_ms,T310,T310,{40=>ms40,80=>ms80,120=>ms120,160=>ms160,200=>ms200,240=>ms240,280=>ms280,320=>ms320})?,
            n_310: ie::N310(bounded(self.n310, TimerField::N310, 0, 7)?),
            t_311: timer_enum!(self.t311_ms,T311,T311,{250=>ms250,500=>ms500,750=>ms750,1000=>ms1000,1250=>ms1250,1500=>ms1500,1750=>ms1750,2000=>ms2000})?,
            t_312: ie::T312(bounded(self.t312_seconds, TimerField::T312, 1, 15)?),
            n_312: base_n312(self.n312)?,
            t_313: ie::T313(bounded(self.t313_seconds, TimerField::T313, 0, 15)?),
            n_313: timer_enum!(self.n313,N313,N313,{1=>s1,2=>s2,4=>s4,10=>s10,20=>s20,50=>s50,100=>s100,200=>s200})?,
            t_314: timer_enum!(self.t314_seconds,T314,T314,{0=>s0,2=>s2,4=>s4,6=>s6,8=>s8,12=>s12,16=>s16,20=>s20})?,
            t_315: timer_enum!(self.t315_seconds,T315,T315,{0=>s0,10=>s10,30=>s30,60=>s60,180=>s180,600=>s600,1200=>s1200,1800=>s1800})?,
            n_315: n315,
            t_316: match self.t316_seconds {
                None => ie::T316::s_inf,
                Some(value) => {
                    timer_enum!(value,T316,T316,{0=>s0,10=>s10,20=>s20,30=>s30,40=>s40,50=>s50})?
                }
            },
            // TS 25.331 10.3.3.43 assigns infinity to every T317 code in Release 5 and later.
            t_317: ie::T317::infinity4,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bits::BitWriter,
        system_information::{CsDomainInformation, Sib1, SystemInformationError},
    };

    fn information() -> Sib1 {
        Sib1 {
            location_area_code: 0x1234,
            cs: Some(CsDomainInformation {
                periodic_update_decihours: 10,
                attach_detach: true,
                drx_coefficient: 6,
            }),
            ps: None,
        }
    }

    fn decoded(profile: &UeTimerProfile) -> ie::SysInfoType1 {
        let encoded = information().encode_with_timers(profile).unwrap();
        rasn::uper::decode(&encoded.bytes).unwrap()
    }

    #[test]
    fn connected_absence_uses_defaults_and_idle_is_always_explicit() {
        let absent = decoded(&UeTimerProfile::default());
        assert!(absent.ue_conn_timers_and_constants.is_none());
        assert!(absent.ue_idle_timers_and_constants.is_some());
        assert!(absent.v3a0_non_critical_extensions.is_none());
        let explicit = decoded(&UeTimerProfile {
            connected: Some(ConnectedTimers::default()),
            idle: IdleTimers::default(),
        });
        assert!(explicit.ue_conn_timers_and_constants.is_some());
        assert!(explicit.ue_idle_timers_and_constants.is_some());
    }

    #[test]
    fn explicit_defaults_match_the_independent_option_bitmap_and_idle_fields() {
        let encoded = information()
            .encode_with_timers(&UeTimerProfile {
                connected: Some(ConnectedTimers::default()),
                idle: IdleTimers::default(),
            })
            .unwrap();
        let baseline: [u8; 6] = [0x04, 0x48, 0xd0, 0x08, 0x50, 0x08];
        let mut expected = BitWriter::default();
        for bit in 0..47 {
            let value = if bit < 2 {
                1
            } else {
                u64::from((baseline[bit / 8] >> (7 - bit % 8)) & 1)
            };
            expected.write(value, 1);
        }
        expected.write(0, 22);
        for (value, width) in [(5, 4), (3, 3), (1, 4), (0, 3)] {
            expected.write(value, width);
        }
        assert_eq!(encoded.bit_length, 83);
        assert_eq!(encoded.bit_length, expected.length);
        assert_eq!(encoded.bytes, expected.bytes);
    }

    #[test]
    fn reference_profile_preserves_access_release_and_domain_recovery_durations() {
        let value = decoded(&UeTimerProfile::release_reference());
        let connected = value.ue_conn_timers_and_constants.unwrap();
        assert_eq!(connected.t_308, ie::T308::ms320);
        assert_eq!(connected.t_314, ie::T314::s8);
        assert_eq!(connected.t_315, ie::T315::s30);
        assert_eq!(connected.t_312.0, 1);
        assert_eq!(connected.t_313.0, 3);
        assert_eq!(connected.n_313, ie::N313::s20);
        assert_eq!(connected.t_317, ie::T317::infinity4);
        let idle = value.ue_idle_timers_and_constants.unwrap();
        assert_eq!(idle.t_300, ie::T300::ms2000);
        assert_eq!(idle.n_300.0, 5);
        assert_eq!(idle.t_312.0, 10);
        assert_eq!(idle.n_312, ie::N312::s1);
    }

    #[test]
    fn all_synchronization_counts_choose_base_or_extension_without_changing_other_container() {
        for count in [1, 2, 4, 10, 20, 50, 100, 200, 400, 600, 800, 1000] {
            for connected_present in [false, true] {
                let profile = UeTimerProfile {
                    connected: connected_present.then_some(ConnectedTimers {
                        n312: count,
                        n315: count,
                        ..ConnectedTimers::default()
                    }),
                    idle: IdleTimers {
                        n312: count,
                        ..IdleTimers::default()
                    },
                };
                let value = decoded(&profile);
                assert_eq!(
                    value.ue_conn_timers_and_constants.is_some(),
                    connected_present
                );
                assert_eq!(
                    value.v3a0_non_critical_extensions.is_some(),
                    [2, 4, 10, 20].contains(&count)
                );
                if let Some(extension) = value.v3a0_non_critical_extensions {
                    let expected = extension_count(count, TimerField::N312).unwrap().unwrap();
                    assert_eq!(
                        extension
                            .sys_info_type1_v3a0ext
                            .ue_idle_timers_and_constants_v3a0ext
                            .n_312,
                        Some(expected)
                    );
                    assert_eq!(
                        extension
                            .sys_info_type1_v3a0ext
                            .ue_conn_timers_and_constants_v3a0ext
                            .n_312,
                        connected_present.then_some(expected)
                    );
                    assert_eq!(
                        value.ue_idle_timers_and_constants.unwrap().n_312,
                        ie::N312::s1
                    );
                }
            }
        }
    }

    #[test]
    fn unsupported_units_spares_and_unused_zero_t312_are_rejected() {
        type Mutation = (fn(&mut ConnectedTimers), TimerField);
        let cases: &[Mutation] = &[
            (|v| v.t301_ms = 300, TimerField::T301),
            (|v| v.n301 = 8, TimerField::N301),
            (|v| v.t302_ms = 9000, TimerField::T302),
            (|v| v.n302 = 8, TimerField::N302),
            (|v| v.t304_ms = 600, TimerField::T304),
            (|v| v.n304 = 8, TimerField::N304),
            (|v| v.t305_minutes = Some(20), TimerField::T305),
            (|v| v.t307_seconds = 60, TimerField::T307),
            (|v| v.t308_ms = 200, TimerField::T308),
            (|v| v.t309_seconds = 0, TimerField::T309),
            (|v| v.t310_ms = 100, TimerField::T310),
            (|v| v.n310 = 8, TimerField::N310),
            (|v| v.t311_ms = 100, TimerField::T311),
            (|v| v.t312_seconds = 0, TimerField::T312),
            (|v| v.n312 = 3, TimerField::N312),
            (|v| v.t313_seconds = 16, TimerField::T313),
            (|v| v.n313 = 1000, TimerField::N313),
            (|v| v.t314_seconds = 10, TimerField::T314),
            (|v| v.t315_seconds = 20, TimerField::T315),
            (|v| v.n315 = 3, TimerField::N315),
            (|v| v.t316_seconds = Some(60), TimerField::T316),
        ];
        for (change, field) in cases {
            let mut connected = ConnectedTimers::default();
            change(&mut connected);
            let profile = UeTimerProfile {
                connected: Some(connected),
                idle: IdleTimers::default(),
            };
            let Err(SystemInformationError::Timer(error)) =
                information().encode_with_timers(&profile)
            else {
                panic!("accepted invalid timer {field:?}")
            };
            assert_eq!(error.field, *field);
        }
        for idle in [
            IdleTimers {
                t300_ms: 300,
                ..IdleTimers::default()
            },
            IdleTimers {
                n300: 8,
                ..IdleTimers::default()
            },
            IdleTimers {
                t312_seconds: 0,
                ..IdleTimers::default()
            },
            IdleTimers {
                n312: 3,
                ..IdleTimers::default()
            },
        ] {
            assert!(
                information()
                    .encode_with_timers(&UeTimerProfile {
                        connected: None,
                        idle
                    })
                    .is_err()
            );
        }
    }

    #[test]
    fn valid_zero_recovery_and_infinity_are_not_silently_replaced() {
        let value = decoded(&UeTimerProfile {
            connected: Some(ConnectedTimers {
                t305_minutes: None,
                t313_seconds: 0,
                t314_seconds: 0,
                t315_seconds: 0,
                t316_seconds: None,
                ..ConnectedTimers::default()
            }),
            idle: IdleTimers::default(),
        });
        let connected = value.ue_conn_timers_and_constants.unwrap();
        assert_eq!(connected.t_305, ie::T305::noUpdate);
        assert_eq!(connected.t_313.0, 0);
        assert_eq!(connected.t_314, ie::T314::s0);
        assert_eq!(connected.t_315, ie::T315::s0);
        assert_eq!(connected.t_316, ie::T316::s_inf);
        assert!(value.ue_idle_timers_and_constants.is_some());
    }

    #[test]
    fn timer_profiles_preserve_cs_ps_and_combined_nas_information() {
        use crate::system_information::{NetworkMode, PsDomainInformation};
        for (cs_present, ps_present) in [(true, false), (false, true), (true, true)] {
            for profile in [
                UeTimerProfile::default(),
                UeTimerProfile::release_reference(),
                UeTimerProfile {
                    connected: Some(ConnectedTimers {
                        n312: 10,
                        n315: 4,
                        ..ConnectedTimers::default()
                    }),
                    idle: IdleTimers::default(),
                },
                UeTimerProfile {
                    connected: None,
                    idle: IdleTimers {
                        n312: 2,
                        ..IdleTimers::default()
                    },
                },
            ] {
                let mut info = information();
                if !cs_present {
                    info.cs = None;
                }
                info.ps = ps_present.then_some(PsDomainInformation {
                    routing_area_code: 7,
                    network_mode: NetworkMode::II,
                    drx_coefficient: 9,
                });
                let encoded = info.encode_with_timers(&profile).unwrap();
                let value: ie::SysInfoType1 = rasn::uper::decode(&encoded.bytes).unwrap();
                assert_eq!(
                    value.cn_domain_sys_info_list.0.len(),
                    usize::from(cs_present) + usize::from(ps_present)
                );
                assert_eq!(
                    value.ue_conn_timers_and_constants.is_some(),
                    profile.connected.is_some()
                );
                assert!(value.ue_idle_timers_and_constants.is_some());
                for domain in &value.cn_domain_sys_info_list.0 {
                    let ie::CNDomainSysInfoCnType::gsm_MAP(nas) = &domain.cn_type else {
                        panic!("unexpected core domain")
                    };
                    match domain.cn_domain_identity {
                        ie::CNDomainIdentity::cs_domain => {
                            assert!(cs_present);
                            assert_eq!(nas.0.as_ref(), &[10, 1]);
                            assert_eq!(domain.cn_drx_cycle_length_coeff.0, 6);
                        }
                        ie::CNDomainIdentity::ps_domain => {
                            assert!(ps_present);
                            assert_eq!(nas.0.as_ref(), &[7, 1]);
                            assert_eq!(domain.cn_drx_cycle_length_coeff.0, 9);
                        }
                    }
                }
            }
        }
    }
}
