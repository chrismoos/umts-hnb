// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{common_control_tx::CommonControlTxError, common_tx::CommonTxError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FachTarget {
    Common,
    Dedicated {
        key: UeKey,
        bearer: SignallingBearer,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FachWork {
    Idle,
    Published(FachTarget),
    Blocked {
        target: FachTarget,
        error: SignallingError,
    },
}

impl CommonSignalling {
    pub fn epoch(&self) -> umts_types::RadioEpoch {
        self.registry.epoch()
    }

    pub fn tick_one(&mut self, now: ChipInstant) -> Result<(), SignallingError> {
        if now.epoch != self.epoch() {
            return Err(SignallingError::Origin);
        }
        let slot = self.timer_cursor;
        self.timer_cursor = (slot + 1) % self.registry.capacity();
        if let Some((key, _)) = self.registry.at_slot(slot) {
            let context = self.registry.get_mut(key).expect("registered timer owner");
            context.control.tick(now).map_err(SignallingError::Am)?;
            context.nas_bearer.tick(now).map_err(SignallingError::Am)?;
            if self.request_cpich_measurements {
                reports::request_cpich(context, key, &self.measurement_reporting)?;
                if let Some(target) = self.secondary_cpich_target {
                    reports::request_secondary_cpich(
                        context,
                        key,
                        &self.measurement_reporting,
                        target,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn service_fach_some(
        &mut self,
        source: &mut dyn CommonTxTarget,
        now: ChipInstant,
        start: ChipInstant,
        budget: NonZeroUsize,
    ) -> Result<FachWork, SignallingError> {
        if now.epoch != self.epoch() || start.epoch != now.epoch || start.index < now.index {
            return Err(SignallingError::Origin);
        }
        let lanes = 1 + 3 * u64::from(self.registry.capacity());
        for _ in 0..(budget.get() as u64).min(lanes) {
            let cursor = self.transmit_cursor;
            self.transmit_cursor = (cursor + 1) % lanes;
            let target = if cursor == 0 {
                if !self.rb0.has_pending() {
                    continue;
                }
                FachTarget::Common
            } else {
                let index = cursor - 1;
                let Some((key, context)) = self.registry.at_slot((index / 3) as u32) else {
                    continue;
                };
                if context.dedicated_route.is_some() {
                    continue;
                }
                let bearer = match index % 3 {
                    0 => SignallingBearer::ControlAm,
                    1 => SignallingBearer::NasAm,
                    _ => SignallingBearer::Unacknowledged,
                };
                FachTarget::Dedicated { key, bearer }
            };
            let result = match target {
                FachTarget::Common => self.publish_rb0(source, start),
                FachTarget::Dedicated { key, bearer } => {
                    self.publish_bearer(key, bearer, source, now, start)
                }
            };
            match result {
                Ok(true) => return Ok(FachWork::Published(target)),
                Ok(false) => {}
                Err(error) if backpressure(&error) => {
                    return Ok(FachWork::Blocked { target, error });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(FachWork::Idle)
    }
}

pub(crate) fn backpressure(error: &SignallingError) -> bool {
    matches!(
        error,
        SignallingError::Capacity
            | SignallingError::Common(
                CommonControlTxError::Capacity
                    | CommonControlTxError::Rlc(umts_rlc::unacknowledged::UmError::Capacity)
                    | CommonControlTxError::Transmit(
                        CommonTxError::Capacity | CommonTxError::Conflict | CommonTxError::Late
                    )
            )
            | SignallingError::Am(
                CommonAmError::Capacity
                    | CommonAmError::Rlc(umts_rlc::acknowledged::AmError::Capacity)
                    | CommonAmError::Transmit(
                        CommonTxError::Capacity | CommonTxError::Conflict | CommonTxError::Late
                    )
            )
            | SignallingError::Um(
                CommonUmError::Rlc(umts_rlc::unacknowledged::UmError::Capacity)
                    | CommonUmError::Fach(crate::common_transport::FachError::Transmit(
                        CommonTxError::Capacity | CommonTxError::Conflict | CommonTxError::Late
                    ))
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        signalling::tests::{controller, establish},
        test_support::{at, count, source},
    };

    #[test]
    fn shared_fach_rotates_across_common_and_every_active_ue_signalling_lane() {
        let mut source = source(false);
        let mut radio = controller(&source);
        let keys = [
            establish(&mut radio, &source, 0),
            establish(&mut radio, &source, 1),
        ];
        for key in keys {
            let context = radio.registry.get_mut(key).unwrap();
            let bytes = Bits::new(&[0x12, 0x34], 16).unwrap();
            context.control.enqueue(key, bytes).unwrap();
            context.nas_bearer.enqueue(key, bytes).unwrap();
            context.um.enqueue(key, bytes).unwrap();
        }
        let mut selected = Vec::new();
        for index in 0..7 {
            let work = radio
                .service_fach_some(
                    &mut source,
                    at(0),
                    at(index * umts_types::time::CHIPS_PER_FRAME),
                    count(25),
                )
                .unwrap();
            let FachWork::Published(target) = work else {
                panic!("ready lane was not scheduled: {work:?}")
            };
            selected.push(target);
        }
        assert!(selected.contains(&FachTarget::Common));
        for key in keys {
            for bearer in [
                SignallingBearer::ControlAm,
                SignallingBearer::NasAm,
                SignallingBearer::Unacknowledged,
            ] {
                assert!(selected.contains(&FachTarget::Dedicated { key, bearer }));
            }
        }
    }
}
