// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use num_complex::Complex32;
use umts_dsp::{
    carrier::CarrierOffsetHz,
    dedicated::{
        CarrierTrackingSettings, LikelihoodSettings, UplinkBranch, UplinkControlFormat,
        UplinkDpchCode, UplinkGains,
    },
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
};
use umts_types::time::{CHIPS_PER_SECOND, CHIPS_PER_SLOT};

pub(super) fn settings(code: u32) -> ChipReceiveSettings {
    ChipReceiveSettings {
        code: Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(code).unwrap()).unwrap()),
        control: UplinkControlFormat::new(0).unwrap(),
        carrier: CarrierTrackingSettings {
            initial: CarrierOffsetHz::new(0.0).unwrap(),
            maximum_absolute_hz: 2000.0,
            update_fraction: 0.5,
            minimum_coherence: 0.8,
        },
        likelihood: LikelihoodSettings::new(128.0).unwrap(),
        tfci: TfciAcceptance {
            minimum_score: 0.5,
            minimum_margin: 0.1,
        },
        maximum_physical_bits: count(57600),
    }
}
pub(super) fn receiver(slot: u32, config: Arc<UplinkTfcs>) -> DedicatedChipReceiver {
    receiver_at(slot, 1, 0, config)
}
pub(super) fn receiver_at(
    slot: u32,
    generation: u64,
    frame: u64,
    config: Arc<UplinkTfcs>,
) -> DedicatedChipReceiver {
    let gains = config
        .configured()
        .combinations()
        .iter()
        .map(|combination| (combination.tfci, UplinkGains::new(8, 15).unwrap()))
        .collect();
    let session = DedicatedReceiveSession::new(
        identity(slot, generation),
        FrameAnchor {
            time: at(frame),
            cfn: frame as u8,
        },
        config,
        limits(8),
    )
    .unwrap();
    DedicatedChipReceiver::new(session, settings(0x123456 + slot), gains).unwrap()
}
pub(super) struct WaveformSettings {
    pub frame: u64,
    pub voice: usize,
    pub packet: usize,
    pub seed: usize,
    pub gain: f32,
    pub cfo: f64,
    pub code: u32,
}
pub(super) fn waveform(config: &UplinkTfcs, settings: WaveformSettings) -> Vec<Complex32> {
    let WaveformSettings {
        frame,
        voice,
        packet,
        seed,
        gain,
        cfo,
        code,
    } = settings;
    let physical = physical(config, voice, packet, frame as u8, seed);
    let layout = config.allocation(tfci(voice, packet)).unwrap().layout();
    let mut codes = Vec::new();
    for index in 0..layout.len() {
        let channel = layout.channel(index).unwrap();
        let mut values = vec![0.0; usize::from(channel.code().spreading_factor())];
        channel.code().write(&mut values).unwrap();
        codes.push(values);
    }
    let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    UplinkLongSequence::new(UplinkScramblingCode::new(code).unwrap(), 0)
        .unwrap()
        .write(&mut scrambling);
    let pilots = [
        62_u8, 38, 45, 36, 53, 62, 60, 52, 46, 63, 45, 55, 52, 39, 39,
    ];
    let control_word = tfci(voice, packet).encode();
    (0..CHIPS_PER_FRAME as usize)
        .map(|chip| {
            let slot = chip / CHIPS_PER_SLOT as usize;
            let symbol = chip % CHIPS_PER_SLOT as usize / 256;
            let bit = if symbol < 6 {
                pilots[slot] & (1 << (5 - symbol)) != 0
            } else if symbol < 8 {
                control_word[2 * slot + symbol - 6] == Bit::One
            } else {
                false
            };
            let mut value = Complex32::new(0.0, if bit { -8.0 / 15.0 } else { 8.0 / 15.0 });
            for (index, code) in codes.iter().enumerate() {
                let amplitude = physical[index][chip / code.len()] as f32 * code[chip % code.len()];
                match layout.channel(index).unwrap().branch() {
                    UplinkBranch::I => value.re += amplitude,
                    UplinkBranch::Q => value.im += amplitude,
                }
            }
            let phase = 0.7
                + slot as f64 * 0.07
                + std::f64::consts::TAU * cfo * (frame * CHIPS_PER_FRAME + chip as u64) as f64
                    / CHIPS_PER_SECOND as f64;
            value
                * scrambling[chip]
                * Complex32::from_polar(gain * (1.0 + (slot % 3) as f32 * 0.1), phase as f32)
        })
        .collect()
}
fn feed(
    receiver: &mut DedicatedChipReceiver,
    frame: u64,
    input: &[Complex32],
) -> Vec<ChipProgress> {
    let identity = receiver.session().identity();
    let mut progress = Vec::new();
    for (slot, chips) in input.chunks_exact(CHIPS_PER_SLOT as usize).enumerate() {
        let time = ChipInstant {
            index: at(frame).index + slot as u64 * CHIPS_PER_SLOT,
            ..at(frame)
        };
        progress.push(receiver.receive_slot(identity, time, chips).unwrap());
        receiver.session_mut().service().unwrap();
    }
    receiver.session_mut().service().unwrap();
    progress
}

#[test]
fn chip_carrier_tfci_gain_reliability_and_mixed_coding_reach_retained_transport_payloads() {
    let config = configuration();
    for gain in [1e-9, 1.0, 1e9] {
        let mut receiver = receiver(0, Arc::clone(&config));
        for frame in 0..4 {
            let (voice, packet) = if frame < 2 {
                (0, 0)
            } else if frame == 2 {
                (1, 1)
            } else {
                (1, 2)
            };
            let samples = waveform(
                &config,
                WaveformSettings {
                    frame,
                    voice,
                    packet,
                    seed: 7,
                    gain,
                    cfo: 1200.0,
                    code: 0x123456,
                },
            );
            let progress = feed(&mut receiver, frame, &samples);
            assert!(
                matches!(progress.last(), Some(ChipProgress::Frame { tfci: Some(selected), .. }) if *selected == tfci(voice, packet))
            );
        }
        let received = consume(receiver.session_mut());
        assert_eq!(received.len(), 3);
        assert_eq!(received[0].1, payload(&config, id(5), 1, 7));
        assert_eq!(received[1].1, payload(&config, id(1), 1, 7));
        assert_eq!(received[2].1, payload(&config, id(5), 2, 7));
        assert_eq!(receiver.counters().accepted_frames, 4);
    }
}

#[test]
fn dch_initial_suffix_is_ignored_until_a_complete_radio_frame() {
    let config = configuration();
    for first in 1..15 {
        let mut receiver = receiver(0, config.clone());
        for slot in first..15 {
            assert!(matches!(
                receiver
                    .receive_slot(
                        identity(0, 1),
                        ChipInstant {
                            index: at(0).index + slot * CHIPS_PER_SLOT,
                            ..at(0)
                        },
                        &[Complex32::default(); CHIPS_PER_SLOT as usize]
                    )
                    .unwrap(),
                ChipProgress::AwaitFrame
            ));
        }
        let samples = waveform(
            &config,
            WaveformSettings {
                frame: 1,
                voice: 1,
                packet: 1,
                seed: 7,
                gain: 1.0,
                cfo: 0.0,
                code: 0x123456,
            },
        );
        let progress = feed(&mut receiver, 1, &samples);
        assert!(
            matches!(progress.last(),Some(ChipProgress::Frame {tfci:Some(selected),..}) if *selected==tfci(1,1))
        );
        assert_eq!(receiver.counters().ignored_slots, 15 - first);
        assert_eq!(receiver.counters().slots, 15);
        assert_eq!(receiver.counters().complete_frames, 1);
    }
}

#[test]
fn inconsistent_air_tfci_discards_partial_tti_and_recovers_at_next_boundary() {
    let config = configuration();
    let mut receiver = receiver(0, Arc::clone(&config));
    for frame in 0..4 {
        let voice = if frame == 1 { 2 } else { 1 };
        let samples = waveform(
            &config,
            WaveformSettings {
                frame,
                voice,
                packet: 1,
                seed: 7,
                gain: 1.0,
                cfo: 0.0,
                code: 0x123456,
            },
        );
        let progress = feed(&mut receiver, frame, &samples);
        if frame == 0 {
            let received = consume(receiver.session_mut());
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].1, payload(&config, id(5), 1, 7));
        } else if frame == 1 {
            assert!(matches!(
                progress.last(),
                Some(ChipProgress::Frame { tfci: None, .. })
            ));
            assert!(consume(receiver.session_mut()).is_empty());
            assert_eq!(receiver.counters().inconsistent_tfci, 1);
        }
    }
    let received = consume(receiver.session_mut());
    assert_eq!(received.len(), 3);
    assert!(received.iter().all(|(metadata, _)| metadata.crc_checked));
    assert_eq!(received[0].1, payload(&config, id(5), 1, 7));
    assert_eq!(received[1].1, payload(&config, id(1), 1, 7));
    assert_eq!(received[2].1, payload(&config, id(5), 1, 7));
    assert_eq!(receiver.counters().accepted_frames, 3);
    assert_eq!(receiver.session().counters().missing_frames, 1);
}

#[test]
fn missing_chip_slots_wait_for_new_frame_without_poisoning_other_receive_history() {
    let config = configuration();
    let mut receiver = receiver(0, Arc::clone(&config));
    let input = waveform(
        &config,
        WaveformSettings {
            frame: 0,
            voice: 1,
            packet: 1,
            seed: 7,
            gain: 1.0,
            cfo: 0.0,
            code: 0x123456,
        },
    );
    let key = receiver.session().identity();
    assert_eq!(
        receiver
            .receive_slot(identity(0, 2), at(0), &input[..2560])
            .unwrap_err(),
        ReceiveError::Identity
    );
    for slot in (0..15).filter(|slot| *slot != 4) {
        let time = ChipInstant {
            index: at(0).index + slot as u64 * CHIPS_PER_SLOT,
            ..at(0)
        };
        receiver
            .receive_slot(key, time, &input[slot * 2560..(slot + 1) * 2560])
            .unwrap();
    }
    assert_eq!(receiver.counters().interrupted_frames, 1);
    assert_eq!(receiver.counters().accepted_frames, 0);
    let input = waveform(
        &config,
        WaveformSettings {
            frame: 1,
            voice: 0,
            packet: 1,
            seed: 9,
            gain: 1.0,
            cfo: 0.0,
            code: 0x123456,
        },
    );
    feed(&mut receiver, 1, &input);
    let received = consume(receiver.session_mut());
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].1, payload(&config, id(5), 1, 9));
    assert_eq!(receiver.session().counters().missing_frames, 1);
}

#[test]
fn silence_has_no_tfci_decision_and_later_frame_can_decode_without_threshold_gain_hacks() {
    let config = configuration();
    let mut receiver = receiver(0, Arc::clone(&config));
    let progress = feed(
        &mut receiver,
        0,
        &vec![Complex32::default(); CHIPS_PER_FRAME as usize],
    );
    assert!(matches!(
        progress.last(),
        Some(ChipProgress::Frame { tfci: None, .. })
    ));
    assert_eq!(receiver.counters().rejected_tfci, 1);
    let samples = waveform(
        &config,
        WaveformSettings {
            frame: 1,
            voice: 0,
            packet: 1,
            seed: 3,
            gain: 1e-9,
            cfo: 0.0,
            code: 0x123456,
        },
    );
    feed(&mut receiver, 1, &samples);
    assert_eq!(
        consume(receiver.session_mut())[0].1,
        payload(&config, id(5), 1, 3)
    );
}

#[test]
fn two_chip_owners_decode_a_shared_composite_carrier_and_account_sample_storage() {
    let config = configuration();
    let first = Box::new(receiver(0, Arc::clone(&config)));
    let second = Box::new(receiver(1, Arc::clone(&config)));
    let a = first.resources();
    let b = second.resources();
    assert!(a.physical_sample_bytes > 600000);
    let resources = ReceiveResources {
        physical_sample_bytes: a.physical_sample_bytes + b.physical_sample_bytes,
        coded_bits: a.coded_bits + b.coded_bits,
        output_bytes: a.output_bytes + b.output_bytes,
        demultiplexed_bits: a.demultiplexed_bits + b.demultiplexed_bits,
        channels: a.channels + b.channels,
    };
    let mut shard = DedicatedRxShard::<DedicatedChipReceiver>::new(
        epoch(),
        ShardLimits {
            maximum_ues: count(2),
            maximum_sessions_per_step: count(2),
            resources,
        },
    )
    .unwrap();
    assert!(shard.install(first).is_ok());
    assert!(shard.install(second).is_ok());
    let mut samples = waveform(
        &config,
        WaveformSettings {
            frame: 0,
            voice: 0,
            packet: 1,
            seed: 1,
            gain: 1.0,
            cfo: 0.0,
            code: 0x123456,
        },
    );
    let peer = waveform(
        &config,
        WaveformSettings {
            frame: 0,
            voice: 0,
            packet: 1,
            seed: 2,
            gain: 0.8,
            cfo: 0.0,
            code: 0x123457,
        },
    );
    for (sample, peer) in samples.iter_mut().zip(peer) {
        *sample += peer;
    }
    for slot in 0..15 {
        let time = ChipInstant {
            index: at(0).index + slot * CHIPS_PER_SLOT,
            ..at(0)
        };
        for owner in 0..2 {
            shard
                .session_mut(identity(owner, 1))
                .unwrap()
                .receive_slot(
                    identity(owner, 1),
                    time,
                    &samples[slot as usize * 2560..(slot as usize + 1) * 2560],
                )
                .unwrap();
        }
        shard.service(|_, result| {
            result.unwrap();
        });
    }
    shard.service(|_, result| {
        result.unwrap();
    });
    for owner in 0..2 {
        let receiver = shard.session_mut(identity(owner, 1)).unwrap();
        let received = consume(receiver.session_mut());
        assert_eq!(received.len(), 1);
        assert_eq!(
            received[0].1,
            payload(&config, id(5), 1, owner as usize + 1)
        );
    }
    shard.remove(identity(0, 1)).unwrap().session_mut().retire();
    assert_eq!(shard.used(), b);
}

#[test]
fn chip_configuration_requires_exact_gain_coverage_and_bounds_sample_buffers() {
    let config = configuration();
    let gains: Vec<_> = config
        .configured()
        .combinations()
        .iter()
        .map(|combination| (combination.tfci, UplinkGains::new(8, 15).unwrap()))
        .collect();
    assert!(
        DedicatedChipReceiver::new(
            session(0, 2, Arc::clone(&config)),
            settings(0x123456),
            gains[..1].to_vec()
        )
        .is_err()
    );
    let mut no_tfci = settings(0x123456);
    no_tfci.control = UplinkControlFormat::new(1).unwrap();
    assert!(
        DedicatedChipReceiver::new(session(0, 2, Arc::clone(&config)), no_tfci, gains.clone())
            .is_err()
    );
    let mut too_small = settings(0x123456);
    too_small.maximum_physical_bits = count(1);
    assert!(DedicatedChipReceiver::new(session(0, 2, config), too_small, gains).is_err());
}

#[test]
fn retiring_a_partial_chip_frame_does_not_allow_configuration_to_replay_its_radio_time() {
    let config = configuration();
    let first = Box::new(receiver(0, Arc::clone(&config)));
    let resources = first.resources();
    let mut shard = DedicatedRxShard::<DedicatedChipReceiver>::new(
        epoch(),
        ShardLimits {
            maximum_ues: count(1),
            maximum_sessions_per_step: count(1),
            resources,
        },
    )
    .unwrap();
    assert!(shard.install(first).is_ok());
    shard
        .session_mut(identity(0, 1))
        .unwrap()
        .receive_slot(identity(0, 1), at(0), &[Complex32::default(); 2560])
        .unwrap();
    shard.remove(identity(0, 1)).unwrap().session_mut().retire();
    let gains: Vec<_> = config
        .configured()
        .combinations()
        .iter()
        .map(|combination| (combination.tfci, UplinkGains::new(8, 15).unwrap()))
        .collect();
    let make = |frame, cfn| {
        let session = DedicatedReceiveSession::new(
            identity(0, 2),
            FrameAnchor {
                time: at(frame),
                cfn,
            },
            Arc::clone(&config),
            limits(8),
        )
        .unwrap();
        DedicatedChipReceiver::new(session, settings(0x123456), gains.clone()).unwrap()
    };
    assert!(matches!(
        shard.install(Box::new(make(0, 0))),
        Err((ReceiveError::Time, _))
    ));
    assert!(shard.install(Box::new(make(2, 2))).is_ok());
}
