// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc::{dedicated::Rel99Configuration, transport::CrcSize};
use umts_types::CnDomain;

pub(super) const MAX_TRANSPORTS: usize = 32;
const SRB1: u8 = 1;
const SRB2: u8 = 1 << 1;
const SRB3: u8 = 1 << 2;
const ACKNOWLEDGED_SRBS: u8 = SRB2 | SRB3;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct TransportDescription {
    pub bearer_mask: u32,
    signalling_mask: u8,
    circuit: bool,
    packet: bool,
    pub crc_bits: u8,
}

impl TransportDescription {
    pub fn role(self) -> &'static str {
        match (self.signalling_mask, self.circuit, self.packet) {
            (SRB1, false, false) => "srb1",
            (SRB2, false, false) => "srb2",
            (SRB3, false, false) => "srb3",
            (ACKNOWLEDGED_SRBS, false, false) => "srb2+srb3",
            (0, true, false) => "cs",
            (0, false, true) => "ps-data",
            (0, false, false) => "unknown",
            (_, false, false) => "signaling",
            _ => "mixed",
        }
    }
}

pub(super) fn describe(
    configuration: &Rel99Configuration,
) -> [Option<TransportDescription>; MAX_TRANSPORTS] {
    describe_channels(
        configuration.downlink().transports(),
        configuration.bearers(),
    )
}

fn describe_channels(
    transports: &[umts_rrc::dedicated::DchTransport],
    bearers: &[umts_rrc::dedicated::Rel99Bearer],
) -> [Option<TransportDescription>; MAX_TRANSPORTS] {
    let mut descriptions = [None; MAX_TRANSPORTS];
    for transport in transports {
        let Some(output) = transport
            .id
            .checked_sub(1)
            .and_then(|id| descriptions.get_mut(usize::from(id)))
        else {
            continue;
        };
        *output = Some(TransportDescription {
            crc_bits: match transport.crc {
                CrcSize::None => 0,
                CrcSize::Bits8 => 8,
                CrcSize::Bits12 => 12,
                CrcSize::Bits16 => 16,
                CrcSize::Bits24 => 24,
            },
            ..TransportDescription::default()
        });
    }
    for bearer in bearers {
        for mapping in &bearer.mapping.downlink {
            let Some(description) = mapping
                .transport
                .checked_sub(1)
                .and_then(|id| descriptions.get_mut(usize::from(id)))
                .and_then(Option::as_mut)
            else {
                continue;
            };
            if let Some(bit) = bearer
                .id
                .checked_sub(1)
                .and_then(|id| 1u32.checked_shl(u32::from(id)))
            {
                description.bearer_mask |= bit;
            }
            match bearer.domain {
                Some(CnDomain::Cs) => description.circuit = true,
                Some(CnDomain::Ps) => description.packet = true,
                None => {
                    if let Some(bit) = bearer
                        .id
                        .checked_sub(1)
                        .and_then(|id| 1u8.checked_shl(u32::from(id)))
                    {
                        description.signalling_mask |= bit;
                    }
                }
            }
        }
    }
    descriptions
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_rrc::dedicated::{BearerMapping, DchTransport, LogicalMapping, Rel99Bearer, Rel99Rlc};
    use umts_rrc::transport::{ChannelCoding, TransportInterval};

    #[test]
    fn role_uses_announced_mapping_instead_of_transport_number() {
        let transport = DchTransport {
            id: 17,
            interval: TransportInterval::Ms40,
            coding: ChannelCoding::Turbo,
            crc: CrcSize::Bits16,
            rate_matching_attribute: 200,
            groups: vec![],
        };
        let bearer = |id| Rel99Bearer {
            id,
            domain: None,
            rlc: Rel99Rlc::Unacknowledged,
            mapping: BearerMapping {
                uplink: vec![],
                downlink: vec![LogicalMapping {
                    transport: 17,
                    logical: Some(id),
                }],
            },
        };
        let layout = describe_channels(&[transport], &[bearer(2), bearer(3)]);
        assert!(layout[1].is_none());
        let description = layout[16].unwrap();
        assert_eq!(description.role(), "srb2+srb3");
        assert_eq!(description.bearer_mask, u32::from(ACKNOWLEDGED_SRBS));
        assert_eq!(description.crc_bits, 16);
    }
}
