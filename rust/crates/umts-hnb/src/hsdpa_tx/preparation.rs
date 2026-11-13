// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_dsp::hsdpa::Modulation;

pub struct WaveformBuffer {
    pub(super) storage: Box<Storage>,
}

impl Default for WaveformBuffer {
    fn default() -> Self {
        Self {
            storage: Box::new(Storage {
                metadata: None,
                control: vec![Complex32::default(); BURST_CHIPS].into_boxed_slice(),
                data: vec![Complex32::default(); BURST_CHIPS].into_boxed_slice(),
            }),
        }
    }
}

pub struct PreparationError {
    pub error: Error,
    pub buffer: WaveformBuffer,
}

pub struct Preparation<'a> {
    pub transmission: ScheduledTransmission,
    pub plan: &'a mut TransmissionPlan,
    pub coded: &'a [Bit],
    pub control_code: u16,
    pub amplitudes: Amplitudes,
}

pub struct WaveformPreparer {
    pub(super) pool: HsdpaCodeSet,
    pub(super) primary: PrimaryScramblingCode,
    pub(super) control: Box<[(u16, ScchSpreader)]>,
    data: PdschSpreader,
    labels: Box<[u8]>,
}

impl WaveformPreparer {
    pub fn new(pool: HsdpaCodeSet, primary: PrimaryScramblingCode) -> Result<Self, Error> {
        let control = pool
            .control_codes()
            .map(|code| {
                ScchSpreader::new(primary, code.index()).map(|spreader| (code.index(), spreader))
            })
            .collect::<Result<Box<[_]>, _>>()
            .map_err(Error::Dsp)?;
        let data = PdschSpreader::new(
            primary,
            u16::from(pool.first_data()),
            usize::from(pool.data_count().get()),
        )
        .map_err(Error::Dsp)?;
        Ok(Self {
            pool,
            primary,
            control,
            data,
            labels: vec![
                0;
                usize::from(pool.data_count().get()) * Modulation::Qam64.bits_per_code()
                    / Modulation::Qam64.bits_per_symbol()
            ]
            .into_boxed_slice(),
        })
    }

    /// Preparation reuses storage. The HARQ owner retains the coded block across retransmissions.
    pub fn prepare(
        &mut self,
        mut request: Preparation<'_>,
        mut buffer: WaveformBuffer,
    ) -> Result<PreparedTransmission, PreparationError> {
        let assignment = request.plan.assignment();
        match self.fill(&mut request, &mut buffer) {
            Ok((start, end)) => {
                buffer.storage.metadata = Some(TransmissionMetadata {
                    transmission: request.transmission,
                    pool: self.pool,
                    primary: self.primary,
                    control_code: request.control_code,
                    first_code: assignment.first_code(),
                    code_count: assignment.code_count(),
                    nominal_data_power: f64::from(request.amplitudes.data).powi(2)
                        * f64::from(assignment.code_count()),
                    start,
                    end,
                });
                Ok(PreparedTransmission {
                    storage: buffer.storage,
                })
            }
            Err(error) => Err(PreparationError { error, buffer }),
        }
    }

    fn fill(
        &mut self,
        request: &mut Preparation<'_>,
        buffer: &mut WaveformBuffer,
    ) -> Result<(ChipInstant, ChipInstant), Error> {
        let transmission = request.transmission;
        let assignment = request.plan.assignment();
        if transmission.key.process().value() != assignment.harq_process()
            || transmission.new_data != assignment.new_data()
            || transmission.key.ue().epoch != transmission.downlink.epoch
        {
            return Err(Error::Identity);
        }
        let first = assignment
            .first_code()
            .checked_sub(self.pool.first_data())
            .ok_or(Error::Codes)?;
        let data = self
            .data
            .subset(usize::from(first)..usize::from(first) + usize::from(assignment.code_count()))
            .map_err(Error::Dsp)?;
        let control = self
            .control
            .iter()
            .find(|(code, _)| *code == request.control_code)
            .ok_or(Error::Codes)?;
        let number = self
            .control
            .iter()
            .position(|(code, _)| *code == request.control_code)
            .ok_or(Error::Codes)?
            + 1;
        if assignment.table() == umts_dsp::hsdpa::TransportTable::OctetAligned
            && usize::from(assignment.control_list_number()) != number
        {
            return Err(Error::Codes);
        }
        let channels = ChannelUse::new(
            self.pool,
            transmission.downlink,
            request.control_code,
            assignment.first_code(),
            assignment.code_count(),
        )?;
        let start = channels.start();
        let end = channels.end();
        let labels = self
            .labels
            .get_mut(..request.plan.physical_symbols())
            .ok_or(Error::Capacity)?;
        request
            .plan
            .prepare_labels(request.coded, labels)
            .map_err(Error::Dsp)?;
        buffer.storage.control.fill(Complex32::default());
        buffer.storage.data.fill(Complex32::default());
        for (slot, (output, data_output)) in buffer
            .storage
            .control
            .chunks_exact_mut(CHIPS_PER_SLOT as usize)
            .zip(
                buffer
                    .storage
                    .data
                    .chunks_exact_mut(CHIPS_PER_SLOT as usize),
            )
            .enumerate()
        {
            if slot < TTI_SLOTS {
                control
                    .1
                    .add_slot(
                        request.plan.control_bits(),
                        start,
                        slot,
                        request.amplitudes.control,
                        output,
                    )
                    .map_err(Error::Dsp)?;
            }
            if slot >= CONTROL_LEAD_SLOTS {
                data.add_slot_labels(
                    assignment.modulation(),
                    labels,
                    transmission.downlink,
                    slot - CONTROL_LEAD_SLOTS,
                    request.amplitudes.data,
                    data_output,
                )
                .map_err(Error::Dsp)?;
            }
        }
        Ok((start, end))
    }
}
