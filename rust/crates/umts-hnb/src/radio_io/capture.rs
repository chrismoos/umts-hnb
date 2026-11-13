// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem::size_of,
    path::{Path, PathBuf},
};

mod recording;
pub(super) use recording::{RxRecording, TxRecording};
pub mod live;

const HEADER_BYTES: usize = 32;
const METADATA_HEADER_BYTES: usize = 40;
const BLOCK_HEADER_BYTES: usize = 24;
const SAMPLE_BYTES: usize = 8;
const METADATA_BYTES: usize = 40;
const TX_MAX_RECORDS: usize = 16_384;
const RX_MAX_RECORDS: usize = 65_536;
pub(crate) const MAX_LIMIT_MB: u32 = 512;
const MAX_TX_LIMIT_MB: u32 = 1_024;
const MAX_RX_LIMIT_MB: u32 = 2_048;
const FORMAT: u32 = 3;
const TX_INCOMPLETE: u32 = 32;
const RX_DISCONTINUITY: u32 = 2;
const UNKNOWN_ACCEPTED: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureError {
    Invalid(&'static str),
    Capacity,
    TooManySamples,
    TooManyRecords,
    NonFiniteSample,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Capacity => formatter.write_str("capture budget exhausted"),
            Self::TooManySamples => formatter.write_str("capture sample count exceeds u32"),
            Self::TooManyRecords => formatter.write_str("capture record count exceeds bound"),
            Self::NonFiniteSample => formatter.write_str("capture contains a non-finite sample"),
        }
    }
}

impl std::error::Error for CaptureError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
struct CaptureMetadata {
    first_sample: u64,
    first_tick: u64,
    host_elapsed_us: u64,
    attempted: u32,
    accepted: u32,
    flags: u32,
    reserved: u32,
}

const _: () = assert!(size_of::<CaptureMetadata>() == METADATA_BYTES);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSummary {
    pub directory: PathBuf,
    pub epoch: u64,
    pub tick_rate: u64,
    pub records: usize,
    pub incomplete_records: usize,
    pub attempted_samples: u64,
    pub raw_bytes: usize,
    pub metadata_bytes: usize,
}

struct CaptureFormat {
    raw_magic: &'static [u8; 8],
    metadata_magic: &'static [u8; 8],
    raw_name: &'static str,
    metadata_name: &'static str,
    maximum_records: usize,
    maximum_limit_mb: u32,
}

const TX_FORMAT: CaptureFormat = CaptureFormat {
    raw_magic: b"UMTSTX01",
    metadata_magic: b"UMTSTXM1",
    raw_name: "raw-tx-0.bin",
    metadata_name: "raw-tx-0.meta",
    maximum_records: TX_MAX_RECORDS,
    maximum_limit_mb: MAX_TX_LIMIT_MB,
};

const RX_FORMAT: CaptureFormat = CaptureFormat {
    raw_magic: b"UMTSRX01",
    metadata_magic: b"UMTSRXM1",
    raw_name: "raw-rx-0.bin",
    metadata_name: "raw-rx-0.meta",
    maximum_records: RX_MAX_RECORDS,
    maximum_limit_mb: MAX_RX_LIMIT_MB,
};

struct CaptureStorage {
    directory: PathBuf,
    format: &'static CaptureFormat,
    epoch: u64,
    tick_rate: u64,
    raw: Vec<u8>,
    metadata: Vec<CaptureMetadata>,
    attempted_samples: u64,
    incomplete_records: usize,
    exhaustion: Option<CaptureError>,
    raw_limit: usize,
    record_limit: usize,
}

impl CaptureStorage {
    fn allocate(
        directory: PathBuf,
        limit_mb: u32,
        sample_rate: f64,
        epoch: u64,
        tick_rate: u64,
        format: &'static CaptureFormat,
        create_directory: bool,
    ) -> io::Result<Self> {
        if limit_mb == 0 || limit_mb > format.maximum_limit_mb {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture limit exceeds the format's bounded allocation budget",
            ));
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture sample rate must be finite and positive",
            ));
        }
        if tick_rate == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture tick rate must be positive",
            ));
        }
        let budget = usize::try_from(limit_mb)
            .ok()
            .and_then(|mb| mb.checked_mul(1024 * 1024))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "capture size overflow"))?;
        let minimum = HEADER_BYTES
            .checked_add(METADATA_HEADER_BYTES)
            .and_then(|value| value.checked_add(BLOCK_HEADER_BYTES + SAMPLE_BYTES))
            .and_then(|value| value.checked_add(METADATA_BYTES))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "capture size overflow"))?;
        if budget < minimum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture budget is too small",
            ));
        }
        if create_directory {
            fs::create_dir(&directory)?;
        }
        let record_capacity = ((budget - HEADER_BYTES - METADATA_HEADER_BYTES)
            / (BLOCK_HEADER_BYTES + SAMPLE_BYTES + METADATA_BYTES))
            .min(format.maximum_records);
        let metadata_payload = record_capacity
            .checked_mul(METADATA_BYTES)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata size overflow"))?;
        let raw_capacity = budget
            .checked_sub(METADATA_HEADER_BYTES)
            .and_then(|value| value.checked_sub(metadata_payload))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "capture size overflow"))?;
        if raw_capacity < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture raw area is too small",
            ));
        }
        let mut raw = Vec::new();
        raw.try_reserve_exact(raw_capacity).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "capture sample allocation failed",
            )
        })?;
        raw.resize(raw_capacity, 0);
        std::hint::black_box(&raw);
        raw.clear();
        write_header(&mut raw, format.raw_magic, sample_rate, 0);
        let mut metadata = Vec::new();
        metadata.try_reserve_exact(record_capacity).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "capture metadata allocation failed",
            )
        })?;
        metadata.resize(
            record_capacity,
            CaptureMetadata {
                first_sample: 0,
                first_tick: 0,
                host_elapsed_us: 0,
                attempted: 0,
                accepted: 0,
                flags: 0,
                reserved: 0,
            },
        );
        std::hint::black_box(&metadata);
        metadata.clear();
        Ok(Self {
            directory,
            format,
            epoch,
            tick_rate,
            raw,
            metadata,
            attempted_samples: 0,
            incomplete_records: 0,
            exhaustion: None,
            raw_limit: raw_capacity,
            record_limit: record_capacity,
        })
    }

    fn append(
        &mut self,
        first_sample: u64,
        first_tick: u64,
        host_elapsed_us: u64,
        samples: &[Complex32],
        accepted: Option<usize>,
        flags: u32,
    ) -> Result<(), CaptureError> {
        if let Some(error) = self.exhaustion {
            return Err(error);
        }
        if samples.is_empty() {
            return Err(CaptureError::Invalid("capture record cannot be empty"));
        }
        if samples.len() > u32::MAX as usize {
            return Err(CaptureError::TooManySamples);
        }
        if accepted.is_some_and(|count| count > samples.len()) {
            return Err(CaptureError::Invalid(
                "accepted samples exceed attempted samples",
            ));
        }
        let payload_bytes = samples
            .len()
            .checked_mul(SAMPLE_BYTES)
            .ok_or(CaptureError::TooManySamples)?;
        let block_bytes = BLOCK_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or(CaptureError::TooManySamples)?;
        let attempted_samples = self
            .attempted_samples
            .checked_add(samples.len() as u64)
            .ok_or(CaptureError::TooManySamples)?;
        if self.metadata.len() == self.record_limit {
            self.exhaustion = Some(CaptureError::TooManyRecords);
            return Err(CaptureError::TooManyRecords);
        }
        let end = self
            .raw
            .len()
            .checked_add(block_bytes)
            .ok_or(CaptureError::Capacity)?;
        if end > self.raw_limit {
            self.exhaustion = Some(CaptureError::Capacity);
            return Err(CaptureError::Capacity);
        }
        if samples
            .iter()
            .any(|sample| !sample.re.is_finite() || !sample.im.is_finite())
        {
            return Err(CaptureError::NonFiniteSample);
        }
        let accepted_value = match accepted {
            Some(count) => u32::try_from(count).map_err(|_| CaptureError::TooManySamples)?,
            None => UNKNOWN_ACCEPTED,
        };
        let metadata = CaptureMetadata {
            first_sample,
            first_tick,
            host_elapsed_us,
            attempted: samples.len() as u32,
            accepted: accepted_value,
            flags,
            reserved: 0,
        };
        append_u64(&mut self.raw, first_sample);
        append_u64(&mut self.raw, host_elapsed_us);
        append_u32(&mut self.raw, samples.len() as u32);
        append_u32(&mut self.raw, flags);
        for sample in samples {
            self.raw.extend_from_slice(&sample.re.to_le_bytes());
            self.raw.extend_from_slice(&sample.im.to_le_bytes());
        }
        self.metadata.push(metadata);
        self.attempted_samples = attempted_samples;
        self.incomplete_records += usize::from(flags & TX_INCOMPLETE != 0);
        Ok(())
    }

    fn finish(self) -> io::Result<CaptureSummary> {
        let raw_bytes = self.raw.len();
        let metadata_payload = self
            .metadata
            .len()
            .checked_mul(METADATA_BYTES)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata size overflow"))?;
        let metadata_bytes = METADATA_HEADER_BYTES
            .checked_add(metadata_payload)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata size overflow"))?;
        let raw_path = self.directory.join(self.format.raw_name);
        let metadata_path = self.directory.join(self.format.metadata_name);
        write_create_new(&raw_path, &self.raw)?;
        let mut metadata_file = create_new(&metadata_path)?;
        write_metadata_header(
            &mut metadata_file,
            self.format.metadata_magic,
            self.epoch,
            self.tick_rate,
            self.metadata.len() as u64,
        )?;
        for record in &self.metadata {
            metadata_file.write_all(&record.first_sample.to_le_bytes())?;
            metadata_file.write_all(&record.first_tick.to_le_bytes())?;
            metadata_file.write_all(&record.host_elapsed_us.to_le_bytes())?;
            metadata_file.write_all(&record.attempted.to_le_bytes())?;
            metadata_file.write_all(&record.accepted.to_le_bytes())?;
            metadata_file.write_all(&record.flags.to_le_bytes())?;
            metadata_file.write_all(&record.reserved.to_le_bytes())?;
        }
        metadata_file.sync_all()?;
        Ok(CaptureSummary {
            directory: self.directory,
            epoch: self.epoch,
            tick_rate: self.tick_rate,
            records: self.metadata.len(),
            incomplete_records: self.incomplete_records,
            attempted_samples: self.attempted_samples,
            raw_bytes,
            metadata_bytes,
        })
    }
}

fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn write_create_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = create_new(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn write_header(output: &mut Vec<u8>, magic: &[u8; 8], sample_rate: f64, index: u64) {
    output.extend_from_slice(magic);
    append_u32(output, 1);
    append_u32(output, FORMAT);
    output.extend_from_slice(&sample_rate.to_le_bytes());
    append_u64(output, index);
}

fn write_metadata_header(
    file: &mut File,
    magic: &[u8; 8],
    epoch: u64,
    tick_rate: u64,
    records: u64,
) -> io::Result<()> {
    file.write_all(magic)?;
    file.write_all(&1_u32.to_le_bytes())?;
    file.write_all(&(METADATA_BYTES as u32).to_le_bytes())?;
    file.write_all(&epoch.to_le_bytes())?;
    file.write_all(&tick_rate.to_le_bytes())?;
    file.write_all(&records.to_le_bytes())
}

fn append_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn append_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}
