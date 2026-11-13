// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_tx::Amplitudes;
use umts_dsp::DspError;

#[derive(Clone, Copy)]
pub struct DownlinkPower {
    cpich_rms: f32,
    control_rms: f32,
    total_power_limit: Option<f32>,
    allocated_gamma_db: Option<f32>,
}

impl DownlinkPower {
    pub(super) fn control_amplitude(self) -> f32 {
        self.control_rms
    }

    pub fn new(cpich_rms: f32, control_rms: f32) -> Result<Self, DspError> {
        if [cpich_rms, control_rms]
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0 || *value > 1.0)
        {
            return Err(DspError::InvalidAmplitude);
        }
        Ok(Self {
            cpich_rms,
            control_rms,
            total_power_limit: None,
            allocated_gamma_db: None,
        })
    }

    pub fn with_allocation(mut self, maximum: f32, gamma_db: f32) -> Result<Self, DspError> {
        if !maximum.is_finite()
            || maximum <= 0.0
            || maximum > 1.0
            || !gamma_db.is_finite()
            || !(-6.0..=13.0).contains(&gamma_db)
            || (gamma_db * 2.0).fract() != 0.0
        {
            return Err(DspError::InvalidAmplitude);
        }
        self.total_power_limit = Some(maximum);
        self.allocated_gamma_db = Some(gamma_db);
        Ok(self)
    }

    fn data_amplitude(
        self,
        rate: Rate,
        codes: u8,
        measurement_gamma_db: f32,
    ) -> Result<f32, DspError> {
        let gamma = self.allocated_gamma_db.unwrap_or(measurement_gamma_db);
        let data = rate.amplitude_per_code(codes, self.cpich_rms, gamma)?;
        if self.total_power_limit.is_some_and(|maximum| {
            f64::from(data).powi(2) * f64::from(codes)
                > f64::from(maximum) * (1.0 + 8.0 * f64::from(f32::EPSILON))
        }) {
            return Err(DspError::InvalidAmplitude);
        }
        Ok(data)
    }
}

impl Session {
    pub fn packet_amplitudes_on(
        &self,
        carrier: umts_types::hsdpa::CarrierId,
        selected: SelectedPacket,
        power: DownlinkPower,
    ) -> Result<Amplitudes, Error> {
        let parameters = self.procedure.construction().parameters();
        let reference = selected.rate.reference;
        if !self.links[carrier.index()].rates.contains(selected.rate)
            || selected.format.table != reference.table
        {
            return Err(Error::Rate(DspError::InvalidCode));
        }
        if !selected
            .rate
            .allocation_codes(self.links[carrier.index()].rates.codes().data_count().get())
            .any(|codes| codes == selected.format.codes)
            || selected.format.modulation != reference.modulation
        {
            return Err(Error::Rate(DspError::InvalidCode));
        }
        if matches!(selected.candidate.work, PacketWork::New { .. })
            && selected.format.transport_bits != reference.transport_bits
        {
            return Err(Error::Rate(DspError::InvalidLength));
        }
        let assignment = umts_dsp::hsdpa::ScchAssignment::new(
            selected.format.first_code,
            selected.format.codes,
            selected.format.modulation,
            selected.format.size_index,
            selected.candidate.process.value(),
            selected.format.version,
            false,
        )
        .and_then(|assignment| assignment.with_table(selected.format.table))
        .and_then(|assignment| assignment.on_control(selected.format.control_list_number))
        .map_err(Error::Rate)?;
        assignment
            .validate_category(
                parameters.harq.category(),
                selected.format.transport_bits.get(),
            )
            .map_err(Error::Rate)?;
        if matches!(selected.candidate.work, PacketWork::New { .. })
            && (reference.table.transport_bits(
                selected.format.modulation,
                selected.format.codes,
                selected.format.size_index,
            ) != Ok(selected.format.transport_bits)
                || (matches!(parameters.harq.category().value(), 13 | 19)
                    && selected.format.modulation == umts_dsp::hsdpa::Modulation::Qam64
                    && selected.format.size_index > 52))
        {
            return Err(Error::Rate(DspError::InvalidLength));
        }
        // TS 25.214 6A.2.1 defines a CQI reference, distinct from NodeB-controlled HS-PDSCH power.
        let data = power
            .data_amplitude(
                selected.rate,
                selected.format.codes,
                f32::from(parameters.measurement.power_offset_half_db()) * 0.5,
            )
            .map_err(Error::Rate)?;
        Ok(Amplitudes {
            control: power.control_rms,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_dsp::hsdpa::Cqi;
    use umts_types::hsdpa::Category;

    fn rate() -> Rate {
        let cqi = Cqi::new(30).unwrap();
        let reference = cqi.reference(Category::new(14).unwrap()).unwrap();
        Rate {
            cqi,
            reference,
            assignment: reference.assignment(1, 0, false).unwrap(),
        }
    }

    #[test]
    fn measurement_reference_never_changes_allocated_data_power() {
        let rate = rate();
        let reference = DownlinkPower::new(0.0971628, 0.02).unwrap();
        let allocated = reference.with_allocation(0.1512866, 12.0).unwrap();
        let expected = reference.data_amplitude(rate, 15, 12.0).unwrap();
        for gamma in [-6.0, 6.0, 9.0, 12.0, 13.0] {
            assert_eq!(allocated.data_amplitude(rate, 15, gamma).unwrap(), expected);
        }
        assert!(reference.data_amplitude(rate, 15, 9.0).unwrap() < expected);
        assert!(
            reference
                .with_allocation(0.01, 12.0)
                .unwrap()
                .data_amplitude(rate, 15, 9.0)
                .is_err()
        );
    }

    #[test]
    fn allocation_rejects_unrepresentable_power_and_gamma() {
        let power = DownlinkPower::new(0.1, 0.1).unwrap();
        for gamma in [-6.5, 13.5, 6.25, f32::NAN] {
            assert!(power.with_allocation(0.1, gamma).is_err());
        }
        for maximum in [0.0, -0.1, 1.1, f32::NAN] {
            assert!(power.with_allocation(maximum, 12.0).is_err());
        }
    }
}
