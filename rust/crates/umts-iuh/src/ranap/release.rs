// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CAUSE, Criticality, Error, ExtensionList, IU_RELEASE, Ie, Limits, Outcome, Packet, Protocol,
    rab,
};
use rasn::{AsnType, Decode, Decoder, Encode};
const DATA_VOLUME_REPORT_LIST: u16 = 31;
const DATA_VOLUME_REPORT_ITEM: u16 = 30;

const IU_RELEASE_REQUEST: u8 = 11;
const UE_SIGNALLING_RELEASE: u16 = 40;
const USER_INACTIVITY: u16 = 16;
const RADIO_CONNECTION_LOST: u16 = 46;
const UTRAN_GENERATED_RELEASE: u16 = 15;

// TS 25.413 8.4 and 9.1.6 use a connection-oriented request, not a local RUA disconnect.
pub fn request_for_ue_signalling_release(limits: Limits) -> Result<Vec<u8>, Error> {
    request(UE_SIGNALLING_RELEASE, limits)
}

pub fn request_for_user_inactivity(limits: Limits) -> Result<Vec<u8>, Error> {
    request(USER_INACTIVITY, limits)
}
/// TS 25.413 8.4 and 9.2.1.4: the RNC asks the CN to release a connection whose handset is lost.
pub fn request_for_radio_connection_lost(limits: Limits) -> Result<Vec<u8>, Error> {
    request(RADIO_CONNECTION_LOST, limits)
}

// TS 25.413 8.4.2 and 9.2.1.4 permit release for a UTRAN-generated reason.
pub fn request_for_utran_generated_release(limits: Limits) -> Result<Vec<u8>, Error> {
    request(UTRAN_GENERATED_RELEASE, limits)
}

fn request(cause: u16, limits: Limits) -> Result<Vec<u8>, Error> {
    Packet {
        protocol: Protocol::Ranap,
        outcome: Outcome::Initiating,
        procedure: IU_RELEASE_REQUEST,
        criticality: Criticality::Ignore,
        ies: vec![Ie::new(
            CAUSE,
            Criticality::Ignore,
            &rab::Cause::from_code(cause)?,
        )?],
        extensions: vec![],
    }
    .encode(limits)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub cause: rab::Cause,
}

pub(super) struct DecodedCommand {
    pub command: Command,
    pub notify_ies: Vec<u16>,
}

impl Command {
    pub fn encode(&self, limits: Limits) -> Result<Vec<u8>, Error> {
        Packet {
            protocol: Protocol::Ranap,
            outcome: Outcome::Initiating,
            procedure: IU_RELEASE,
            criticality: Criticality::Reject,
            ies: vec![Ie::new(CAUSE, Criticality::Ignore, &self.cause)?],
            extensions: vec![],
        }
        .encode(limits)
    }

    pub(super) fn from_packet(packet: Packet) -> Result<DecodedCommand, Error> {
        if packet.protocol != Protocol::Ranap
            || packet.procedure != IU_RELEASE
            || packet.outcome != Outcome::Initiating
            || packet.criticality != Criticality::Reject
        {
            return Err(Error::State);
        }
        let notify_ies = packet.check(&[CAUSE], &[])?;
        let cause = packet.required(CAUSE)?;
        if cause.criticality != Criticality::Ignore {
            return Err(Error::Value(CAUSE));
        }
        Ok(DecodedCommand {
            command: Self {
                cause: cause.decode()?,
            },
            notify_ies,
        })
    }
}

/// One entry of the RABs Data Volume Report List (TS 25.413 9.1.7, 9.2.1.49).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeReport {
    pub rab: u8,
    /// Downlink octets accepted from the core but never delivered over the radio.
    pub unsuccessfully_transmitted: u32,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct VolumeReportItem {
    id: rab::RabId,
    volumes: Option<rab::Volumes>,
    extensions: Option<ExtensionList>,
}
// TS 25.413 8.5.2 permits no report IEs only when no data-volume or sequence reports are due.
pub fn complete_without_reports(limits: Limits) -> Result<Vec<u8>, Error> {
    complete(&[], limits)
}
/// TS 25.413 8.5.2: Iu Release Complete with the RABs Data Volume Report List for the PS RABs
/// whose assignment requested data volume reporting.
pub fn complete(reports: &[VolumeReport], limits: Limits) -> Result<Vec<u8>, Error> {
    let mut ies = Vec::new();
    if !reports.is_empty() {
        let items = reports
            .iter()
            .map(|report| {
                Ok(VolumeReportItem {
                    id: rab::RabId::new(report.rab),
                    volumes: Some(rab::Volumes::new(vec![rab::Volume::new(
                        report.unsuccessfully_transmitted,
                        None,
                    )])?),
                    extensions: None,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        ies.push(rab::encode_list(
            DATA_VOLUME_REPORT_LIST,
            DATA_VOLUME_REPORT_ITEM,
            &items,
            limits,
        )?);
    }
    Packet {
        protocol: Protocol::Ranap,
        outcome: Outcome::Successful,
        procedure: IU_RELEASE,
        criticality: Criticality::Reject,
        ies,
        extensions: vec![],
    }
    .encode(limits)
}

#[cfg(test)]
mod tests {
    use super::super::Inbound;
    use super::*;

    #[test]
    fn ue_signalling_release_request_matches_independent_live_aper() {
        assert_eq!(
            request_for_ue_signalling_release(Limits::default()).unwrap(),
            [0, 11, 0x40, 9, 0, 0, 1, 0, 4, 0x40, 2, 9, 0xc0]
        );
    }

    #[test]
    fn utran_generated_release_request_uses_radio_network_cause() {
        let limits = Limits::default();
        let bytes = request_for_utran_generated_release(limits).unwrap();
        // TS 25.413 ASN.1: root choice 0, INTEGER (1..64) value 15.
        assert_eq!(bytes, [0, 11, 0x40, 9, 0, 0, 1, 0, 4, 0x40, 2, 3, 0x80]);
        let packet = Packet::decode(Protocol::Ranap, &bytes, limits).unwrap();
        assert_eq!(packet.outcome, Outcome::Initiating);
        assert_eq!(packet.procedure, 11);
        assert_eq!(packet.criticality, Criticality::Ignore);
        assert_eq!(packet.ies.len(), 1);
        let ie = packet.required(CAUSE).unwrap();
        assert_eq!(ie.criticality, Criticality::Ignore);
        let cause: rab::Cause = ie.decode().unwrap();
        assert!(matches!(cause, rab::Cause::Radio(_)));
        assert_eq!(cause.code(), 15);
    }

    #[test]
    fn release_wire_matches_independent_aper() {
        let command = Command {
            cause: rab::Cause::from_code(83).unwrap(),
        };
        let bytes = command.encode(Limits::default()).unwrap();
        assert_eq!(bytes, [0, 1, 0, 8, 0, 0, 1, 0, 4, 0x40, 1, 0x22]);
        assert_eq!(
            super::super::decode(&bytes, Limits::default())
                .unwrap()
                .message,
            Inbound::IuRelease(command)
        );
        assert_eq!(
            complete_without_reports(Limits::default()).unwrap(),
            complete(&[], Limits::default()).unwrap()
        );
        let reported = complete(
            &[VolumeReport {
                rab: 5,
                unsuccessfully_transmitted: 1234,
            }],
            Limits::default(),
        )
        .unwrap();
        let packet = Packet::decode(Protocol::Ranap, &reported, Limits::default()).unwrap();
        assert_eq!(packet.procedure, IU_RELEASE);
        assert_eq!(packet.outcome, Outcome::Successful);
        assert!(packet.required(DATA_VOLUME_REPORT_LIST).is_ok());
        assert_eq!(
            complete_without_reports(Limits::default()).unwrap(),
            [0x20, 1, 0, 3, 0, 0, 0]
        );
    }

    #[test]
    fn malformed_release_never_becomes_a_command() {
        let command = Command {
            cause: rab::Cause::from_code(83).unwrap(),
        };
        let bytes = command.encode(Limits::default()).unwrap();
        let base = Packet::decode(Protocol::Ranap, &bytes, Limits::default()).unwrap();
        for mutation in 0..4 {
            let mut packet = base.clone();
            match mutation {
                0 => packet.criticality = Criticality::Ignore,
                1 => packet.ies.clear(),
                2 => packet.ies[0].criticality = Criticality::Reject,
                _ => packet.ies.push(packet.ies[0].clone()),
            }
            let wire = packet.encode(Limits::default());
            assert!(
                wire.is_err() || super::super::decode(&wire.unwrap(), Limits::default()).is_err()
            );
        }
    }
}
