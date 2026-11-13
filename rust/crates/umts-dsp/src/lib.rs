// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod bits;
pub mod broadcast;
pub mod carrier;
pub mod carrier_mixer;
pub mod channel_codes;
pub mod code_blocks;
pub mod coding;
pub mod common_downlink;
pub mod convolutional;
pub mod crc;
pub mod crest_reduction;
pub mod dedicated;
pub mod dedicated_transport;
pub mod downlink_transport;
mod fft_geometry;
pub mod fractional_sampling;
pub mod hsdpa;
pub mod hsupa;
pub mod indicators;
pub mod interleaving;
pub mod pilots;
pub mod prach;
pub mod prach_message;
pub mod prach_search;
pub mod prach_transport;
pub mod pulse_shaping;
pub mod rate_matching;
pub mod receive_filter;
pub mod sample_validation;
pub mod scrambling;
pub mod secondary_downlink;
pub mod soft_bits;
pub mod synchronization;
pub mod tfci;
pub mod transport_blocks;
pub mod turbo;
pub mod uplink_scrambling;
pub mod waveform;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DspError {
    InvalidLength,
    InvalidCode,
    NonFiniteEvidence,
    Capacity,
    InvalidRateMatching,
    InvalidAmplitude,
    InvalidFilter,
    InvalidFrequency,
    DiscontinuousTime,
}

impl From<umts_types::channel_codes::ChannelCodeError> for DspError {
    fn from(error: umts_types::channel_codes::ChannelCodeError) -> Self {
        match error {
            umts_types::channel_codes::ChannelCodeError::InvalidCode => Self::InvalidCode,
            umts_types::channel_codes::ChannelCodeError::InvalidLength => Self::InvalidLength,
        }
    }
}
