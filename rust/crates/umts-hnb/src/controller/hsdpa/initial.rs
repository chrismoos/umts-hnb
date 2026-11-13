// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa_session::{RetainedDpchPolicy, SchedulingAdmission, SchedulingPreparation},
    radio_resources::{HsdpaCodeSet, RadioResources},
    rel99_admission::{PreparedRabAssignment, RabAdmissionError, RejectedRabAssignment},
    rel99_planner::{ConfiguredPolicy, InitialOwnerSettings, OwnerBuildError, PreparedInitial},
};
use umts_rrc::hsdpa::InitialConfiguration;
use umts_types::CnDomain;

pub enum InitialCatalogAdmission {
    Deferred(ReceiveIdentity),
    Ready(Box<PreparedRabAssignment>),
}

pub(super) struct PendingInitial {
    pub(super) identity: ReceiveIdentity,
    request: Box<PreparedRabAssignment>,
    configuration: Arc<InitialConfiguration>,
    fallback: Option<Result<SingleFallback, String>>,
}

struct SingleFallback {
    configuration: Arc<InitialConfiguration>,
    codes: HsdpaCodeSet,
    scheduling: SchedulingAdmission,
}

impl SingleFallback {
    fn reserve(self) -> Result<(Arc<InitialConfiguration>, SchedulingPreparation), String> {
        let (primary, secondary) = super::preparation::reserve_carriers(
            self.configuration.clone(),
            self.codes,
            None,
            &self.scheduling,
        )
        .map_err(|error| format!("single-cell fallback catalog reservation: {error:?}"))?;
        debug_assert!(secondary.is_none());
        Ok((self.configuration, primary))
    }
}

impl<T: Transport> CommonController<T> {
    pub fn initial_hsdpa_preparation_available(&self) -> bool {
        self.hsdpa.as_ref().is_none_or(|sessions| {
            sessions.initial.is_none() && sessions.preparation.identity().is_none()
        })
    }

    pub fn prepare_initial_hsdpa(
        &mut self,
        request: Box<PreparedRabAssignment>,
        policy: &RetainedDpchPolicy,
        codes: HsdpaCodeSet,
        secondary: Option<SecondaryHsdpaAdmission>,
        scheduling: &SchedulingAdmission,
        resources: &mut RadioResources,
    ) -> Result<InitialCatalogAdmission, Box<RejectedRabAssignment>> {
        let radio = request.radio();
        let key = radio.allocation.key();
        let identity = radio.allocation.receive_identity();
        let definition = radio.allocation.plan().configuration();
        let dual = self
            .radio
            .capabilities(key)
            .and_then(|report| policy.selected_dual_cell(report))
            .zip(secondary);
        let category = dual
            .map(|(category, _)| category.per_cell_category())
            .or_else(|| {
                self.radio
                    .capabilities(key)
                    .and_then(|report| policy.selected_category(report))
            });
        if !definition
            .bearers()
            .iter()
            .any(|bearer| bearer.domain == Some(CnDomain::Ps))
            || category.is_none()
        {
            if let Err(error) = policy.validate_rel99_packet_fallback(definition) {
                return Err(request.reject(resources, RabAdmissionError::Hsdpa(error.to_string())));
            }
            return Ok(InitialCatalogAdmission::Ready(request));
        }
        let prepared = (|| {
            let assigned = self
                .radio
                .assigned(key)
                .ok_or_else(|| "initial HSDPA identity is absent".to_owned())?;
            let configuration = match dual {
                Some((category, secondary)) => policy.construct_configuration_dual(
                    definition.clone(),
                    category,
                    codes,
                    assigned.crnti(),
                    secondary,
                ),
                None => policy.construct_configuration(
                    definition.clone(),
                    category.expect("supported category"),
                    codes,
                    assigned.crnti(),
                ),
            }
            .map_err(|error| format!("initial HSDPA configuration: {error:#}"))?;
            let fallback_category = dual.and_then(|_| {
                self.radio
                    .capabilities(key)
                    .and_then(|report| policy.selected_category(report))
            });
            let mut fallback = fallback_category.map(|category| {
                let configuration = policy
                    .construct_configuration(definition.clone(), category, codes, assigned.crnti())
                    .map_err(|error| format!("single-cell fallback configuration: {error:#}"))?;
                Ok::<_, String>(SingleFallback {
                    configuration,
                    codes,
                    scheduling: SchedulingAdmission {
                        settings: scheduling.settings,
                        budget: scheduling.budget,
                        feedback: scheduling.feedback,
                        cell: scheduling.cell.clone(),
                    },
                })
            });
            match super::preparation::reserve_carriers(
                configuration.clone(),
                codes,
                dual.map(|(_, secondary)| secondary),
                scheduling,
            ) {
                Ok((primary, secondary)) => {
                    Ok((configuration, primary, secondary, fallback, false))
                }
                Err(error) if fallback.is_some() && super::preparation::capacity_error(&error) => {
                    let (configuration, primary) =
                        fallback.take().expect("dual fallback")?.reserve()?;
                    Ok((configuration, primary, None, None, true))
                }
                Err(error) => Err(format!("initial HSDPA catalog reservation: {error:?}")),
            }
        })();
        let (configuration, preparation, secondary_preparation, fallback, used_fallback) =
            match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    return Err(request.reject(resources, RabAdmissionError::Hsdpa(error)));
                }
            };
        let Some(sessions) = &mut self.hsdpa else {
            return Err(request.reject(resources, RabAdmissionError::Construction));
        };
        if sessions.initial.is_some() || sessions.preparation.identity().is_some() {
            return Err(request.reject(
                resources,
                RabAdmissionError::Hsdpa("initial HSDPA catalog worker is occupied".to_owned()),
            ));
        }
        if let Err(error) =
            sessions
                .preparation
                .submit_carriers(identity, preparation, secondary_preparation)
        {
            return Err(request.reject(resources, RabAdmissionError::Hsdpa(error.to_owned())));
        }
        sessions.single_cell_fallback[key.slot as usize] = used_fallback.then_some(identity);
        if used_fallback {
            tracing::info!(
                ?identity,
                "Initial HSDPA selecting single-cell fallback: dual catalog reservation capacity"
            );
        }
        sessions.initial = Some(PendingInitial {
            identity,
            request,
            configuration,
            fallback,
        });
        Ok(InitialCatalogAdmission::Deferred(identity))
    }

    pub fn service_initial_hsdpa(
        &mut self,
        policy: &ConfiguredPolicy,
        resources: &mut RadioResources,
        settings: impl FnOnce(&PreparedInitial) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<Option<Result<ReceiveIdentity, Box<RejectedRabAssignment>>>, String> {
        let Some(sessions) = &mut self.hsdpa else {
            return Ok(None);
        };
        let Some(initial) = &sessions.initial else {
            return Ok(None);
        };
        if sessions.preparation.identity() != Some(initial.identity) {
            return Err("initial HSDPA catalog owner mismatch".to_owned());
        }
        sessions.preparation.poll().map_err(str::to_owned)?;
        if !sessions.preparation.ready(initial.identity) {
            return Ok(None);
        }
        let prepared = sessions.preparation.take().expect("ready initial catalog");
        let mut initial = sessions.initial.take().expect("retained initial request");
        let selected_single = sessions.single_cell_fallback[initial.identity.ue.slot as usize]
            == Some(initial.identity);
        if selected_single {
            sessions.single_cell_fallback[initial.identity.ue.slot as usize] = None;
        }
        if prepared.capacity_only_failure() && initial.fallback.is_some() {
            // Drop both dual owners before reserving the replacement single catalog.
            drop(prepared);
            let replacement = initial
                .fallback
                .take()
                .expect("one fallback")
                .and_then(SingleFallback::reserve);
            let (configuration, preparation) = match replacement {
                Ok(replacement) => replacement,
                Err(error) => {
                    return Ok(Some(Err(initial
                        .request
                        .reject(resources, RabAdmissionError::Hsdpa(error)))));
                }
            };
            if let Err(error) =
                sessions
                    .preparation
                    .submit_carriers(initial.identity, preparation, None)
            {
                return Ok(Some(Err(initial
                    .request
                    .reject(resources, RabAdmissionError::Hsdpa(error.to_owned())))));
            }
            sessions.single_cell_fallback[initial.identity.ue.slot as usize] =
                Some(initial.identity);
            initial.configuration = configuration;
            tracing::info!(identity = ?initial.identity, "Initial HSDPA selecting single-cell fallback: dual catalog build capacity");
            sessions.initial = Some(initial);
            return Ok(None);
        }
        if let Some(error) = prepared.non_capacity_error() {
            return Ok(Some(Err(initial.request.reject(
                resources,
                RabAdmissionError::Hsdpa(format!("initial HSDPA catalog construction: {error:?}")),
            ))));
        }
        let secondary = match prepared.secondary.transpose() {
            Ok(secondary) => secondary,
            Err(error) => {
                return Ok(Some(Err(initial.request.reject(
                    resources,
                    RabAdmissionError::Hsdpa(format!(
                        "secondary initial catalog construction: {error:?}"
                    )),
                ))));
            }
        };
        if initial.configuration.secondary_cell().is_some() != secondary.is_some() {
            return Ok(Some(Err(initial.request.reject(
                resources,
                RabAdmissionError::Hsdpa(
                    "initial secondary catalog is inconsistent with RRC configuration".to_owned(),
                ),
            ))));
        }
        let catalog = match prepared.resources {
            Ok(resources) => crate::rel99_admission::hsdpa::PreparedCatalog {
                secondary,
                configuration: initial.configuration,
                resources,
            },
            Err(error) => {
                return Ok(Some(Err(initial.request.reject(
                    resources,
                    RabAdmissionError::Hsdpa(format!(
                        "initial HSDPA catalog construction: {error:?}"
                    )),
                ))));
            }
        };
        let result = self.begin_prepared_rab_dch(initial.request, policy, resources, settings);
        if let Ok(identity) = result {
            if selected_single {
                self.hsdpa
                    .as_mut()
                    .expect("configured HSDPA")
                    .single_cell_fallback[identity.ue.slot as usize] = Some(identity);
            }
            self.admission
                .as_mut()
                .expect("accepted initial admission")
                .attach_hsdpa_catalog(identity, catalog);
            tracing::info!(
                ?identity,
                catalog_ms = prepared.elapsed.as_secs_f64() * 1000.0,
                "Initial HSDPA catalog prepared before bearer publication"
            );
            Ok(Some(Ok(identity)))
        } else {
            Ok(Some(result))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        hsdpa_session::{
            CellSchedulingBudget, DownlinkPower, SchedulingBudget, SchedulingSettings,
        },
        test_support::{at, count, rel99, source},
    };
    use std::num::NonZeroU8;
    use umts_rrc::hsdpa::*;

    fn fixture() -> (
        Arc<InitialConfiguration>,
        Arc<InitialConfiguration>,
        SecondaryHsdpaAdmission,
        SchedulingAdmission,
    ) {
        let source = source(true);
        let mut radio =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = radio
            .reserve_hsdpa(NonZeroU8::new(2).unwrap(), NonZeroU8::MIN)
            .unwrap();
        let codes = *lease.codes();
        let dedicated = rel99::configuration(false, true, 10);
        let category = umts_types::hsdpa::DualCellCategory::new(21).unwrap();
        let make = |dual| {
            let mut sizes = [None; 8];
            sizes[0] = Some(count(if dual { 320 } else { 324 }));
            let parameters = ActivationParameters {
                h_rnti: 0x8421,
                harq: HarqConfiguration::implicit(
                    if dual {
                        category.per_cell_category()
                    } else {
                        category.single_cell_fallback()
                    },
                    NonZeroU8::new(6).unwrap(),
                )
                .unwrap(),
                queues: QueueSet::new(vec![QueueConfiguration::new(0, 0, sizes, 100, 32).unwrap()])
                    .unwrap(),
                control: ControlChannels::new(&codes.control_codes().collect::<Vec<_>>()).unwrap(),
                measurement: MeasurementFeedback::new(6, FeedbackCycle::Ms2, NonZeroU8::MIN, 8)
                    .unwrap(),
                acknowledgement: AckFeedback::new(8, 8, NonZeroU8::MIN).unwrap(),
            };
            let mappings = vec![MappedBearer {
                bearer: 20,
                flow: 0,
                logical: Some(1),
            }];
            if dual {
                InitialConfiguration::new_mac_ehs(dedicated.clone(), parameters, mappings)
                    .unwrap()
                    .with_secondary_cell(
                        SecondaryCellConfiguration::new(
                            0x8421,
                            10662,
                            470,
                            ControlChannels::new(&codes.control_codes().collect::<Vec<_>>())
                                .unwrap(),
                            6,
                            false,
                        )
                        .unwrap(),
                    )
                    .unwrap()
            } else {
                InitialConfiguration::new(dedicated.clone(), parameters, mappings).unwrap()
            }
        };
        let budget = SchedulingBudget {
            maximum_formats: count(1024),
            maximum_coded_bits: count(100_000),
            maximum_plan_bytes: count(16_000_000),
            maximum_buffer_bytes: count(2_000_000),
            maximum_encoder_bytes: count(1_000_000),
            maximum_total_bytes: count(40_000_000),
        };
        let single = budget.for_carriers(1).unwrap().maximum_total_bytes.get();
        let limits = crate::hsdpa_rx::EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
        let scheduling = SchedulingAdmission {
            settings: SchedulingSettings {
                power: DownlinkPower::new(0.1, 0.1).unwrap(),
                maximum_cqi_age: NonZeroU64::new(38_400).unwrap(),
                selection_budget: count(1024),
                cqi_offset_limit: 0,
                cqi_target_bler_percent: 10,
            },
            budget,
            feedback: crate::hsdpa_rx::FeedbackPolicy {
                acknowledgement: limits,
                cqi: limits,
            },
            cell: CellSchedulingBudget::new(count(single * 2)),
        };
        let secondary = SecondaryHsdpaAdmission {
            primary: umts_dsp::scrambling::PrimaryScramblingCode::new(470).unwrap(),
            uarfcn_dl: 10662,
            codes,
            measurement_power_offset_db: 3.0,
            cpich_amplitude: 0.1,
            control_amplitude: 0.1,
            power_limit: None,
            transmit_gamma_db: None,
            downlink_origin: Some(at(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)),
        };
        (
            Arc::new(make(true)),
            Arc::new(make(false)),
            secondary,
            scheduling,
        )
    }

    #[test]
    fn unsupported_packet_mode_rejects_initial_admission_without_leaking_radio_resources() {
        use crate::{
            controller::tests::duplex::environment,
            core_link::tests::register,
            rel99_planner::tests::admission::rabs::{configured, offer},
            signalling::tests::{COMPLETE, peer, send_capability_report},
        };
        use umts_config::PacketMode;
        use umts_rrc::establishment::TransactionId;
        for modes in [
            None,
            Some(vec![PacketMode::HspaPlus]),
            Some(vec![PacketMode::DcHspa]),
        ] {
            let mut env = environment();
            let source = source(false);
            let controller = &mut env.controller;
            let (key, context, _) = register(
                &mut controller.core,
                &env._peer,
                &mut controller.radio,
                &source,
                0,
                3,
            );
            controller
                .radio
                .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
                .unwrap();
            let mut ue = peer(&source);
            ue.enqueue(&COMPLETE).unwrap();
            ue.publish_next(at(0), |_| Ok::<_, ()>(()))
                .unwrap()
                .unwrap();
            send_capability_report(&mut controller.radio, key, &mut ue);
            assert_eq!(controller.radio.process_one(), Some(Ok(key)));
            let pending = offer(
                &mut controller.core,
                &mut controller.radio,
                &env._peer,
                context,
                6,
            );
            let configured = configured(&controller.radio, key);
            let mut resources =
                RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
            let before = resources.outstanding();
            let prepared = controller
                .prepare_rab_dch(pending, &configured, &mut resources)
                .unwrap_or_else(|error| panic!("{:?}", error.error));
            assert!(resources.outstanding() > before);
            let config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../rust/test-support/fixtures/config/rel99.yaml"
            )))
            .unwrap();
            let policy = RetainedDpchPolicy::new(&config.hspa.downlink, 6.0)
                .unwrap()
                .with_packet_modes(modes.as_deref());
            let (_, _, secondary, scheduling) = fixture();
            let result = controller.prepare_initial_hsdpa(
                prepared,
                &policy,
                secondary.codes,
                Some(secondary),
                &scheduling,
                &mut resources,
            );
            match (modes.is_none(), result) {
                (true, Ok(InitialCatalogAdmission::Ready(request))) => {
                    let _rejected = request.reject(&mut resources, RabAdmissionError::Construction);
                }
                (false, Err(rejected)) => {
                    assert!(
                        matches!(rejected.error, RabAdmissionError::Hsdpa(ref error) if error.contains("packet_modes"))
                    );
                    assert!(rejected.rollback_error.is_none());
                    assert!(rejected.allocation.is_none());
                }
                _ => panic!("unexpected packet mode admission"),
            }
            assert_eq!(resources.outstanding(), before);
        }
    }

    #[test]
    fn peer_reservation_survives_failed_dual_and_rebuilt_single_without_leaks() {
        let (dual, single, secondary, scheduling) = fixture();
        let total = scheduling.cell.available_bytes();
        let (peer, _) = super::super::preparation::reserve_carriers(
            single.clone(),
            secondary.codes,
            None,
            &scheduling,
        )
        .unwrap();
        let available = scheduling.cell.available_bytes();
        assert!(matches!(
            super::super::preparation::reserve_carriers(
                dual.clone(),
                secondary.codes,
                Some(secondary),
                &scheduling,
            ),
            Err(Error::Capacity)
        ));
        assert_eq!(scheduling.cell.available_bytes(), available);
        let fallback = SingleFallback {
            configuration: single.clone(),
            codes: secondary.codes,
            scheduling: SchedulingAdmission {
                settings: scheduling.settings,
                budget: scheduling.budget,
                feedback: scheduling.feedback,
                cell: scheduling.cell.clone(),
            },
        };
        let (selected, primary) = fallback.reserve().unwrap();
        assert!(Arc::ptr_eq(&selected, &single));
        assert!(!Arc::ptr_eq(&selected, &dual));
        assert!(selected.secondary_cell().is_none());
        assert!(!selected.uses_mac_ehs());
        assert_eq!(selected.parameters().harq.category().value(), 9);
        assert!(matches!(
            super::super::preparation::reserve_carriers(
                single.clone(),
                secondary.codes,
                None,
                &scheduling,
            ),
            Err(Error::Capacity)
        ));
        drop(primary);
        assert_eq!(scheduling.cell.available_bytes(), available);
        drop(peer);
        assert_eq!(scheduling.cell.available_bytes(), total);
        let (primary, secondary_owner) = super::super::preparation::reserve_carriers(
            dual,
            secondary.codes,
            Some(secondary),
            &scheduling,
        )
        .unwrap();
        assert!(secondary_owner.is_some());
        drop((primary, secondary_owner));
        assert_eq!(scheduling.cell.available_bytes(), total);
    }

    #[test]
    fn actual_plan_capacity_error_is_fallback_eligible_and_releases_dual_owners() {
        let (dual, single, secondary, mut scheduling) = fixture();
        scheduling.cell = CellSchedulingBudget::new(count(100_000_000));
        let original_budget = scheduling.budget;
        scheduling.budget.maximum_formats = count(100_000);
        scheduling.budget.maximum_plan_bytes = count(1);
        scheduling.budget.maximum_buffer_bytes = count(16_000_000);
        scheduling.budget.maximum_encoder_bytes = count(16_000_000);
        let total = scheduling.cell.available_bytes();
        let (primary, secondary_owner) = super::super::preparation::reserve_carriers(
            dual,
            secondary.codes,
            Some(secondary),
            &scheduling,
        )
        .unwrap();
        let error = match primary.build() {
            Err(error) => error,
            Ok(_) => panic!("one-byte plan budget must fail"),
        };
        assert_eq!(error, Error::Rate(umts_dsp::DspError::Capacity));
        assert!(super::super::preparation::capacity_error(&error));
        drop(secondary_owner);
        assert_eq!(scheduling.cell.available_bytes(), total);
        scheduling.budget = original_budget;
        let fallback = SingleFallback {
            configuration: single,
            codes: secondary.codes,
            scheduling,
        };
        let (configuration, primary) = fallback.reserve().unwrap();
        assert!(configuration.secondary_cell().is_none());
        drop(primary.build().unwrap());
    }

    #[test]
    fn invalid_secondary_is_explicit_and_never_consumes_primary_or_peer_budget() {
        let (dual, single, mut secondary, scheduling) = fixture();
        let (peer, _) =
            super::super::preparation::reserve_carriers(single, secondary.codes, None, &scheduling)
                .unwrap();
        let available = scheduling.cell.available_bytes();
        secondary.cpich_amplitude = f32::NAN;
        assert!(matches!(
            super::super::preparation::reserve_carriers(
                dual,
                secondary.codes,
                Some(secondary),
                &scheduling,
            ),
            Err(Error::Rate(_))
        ));
        assert_eq!(scheduling.cell.available_bytes(), available);
        drop(peer);
    }
}
