// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

const SETUP_LIST: u16 = 52;
const SETUP_ITEM: u16 = 51;
const RELEASED_LIST: u16 = 43;
const RELEASED_ITEM: u16 = 42;
const QUEUED_LIST: u16 = 38;
const QUEUED_ITEM: u16 = 37;
const FAILED_LIST: u16 = 35;
const FAILED_ITEM: u16 = 34;
const RELEASE_FAILED_LIST: u16 = 39;
const DIAGNOSTICS: u16 = 9;

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Volume {
    pub unsuccessfully_transmitted: u32,
    pub reference: Option<u8>,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Volume {
    pub fn new(unsuccessfully_transmitted: u32, reference: Option<u8>) -> Self {
        Self {
            unsuccessfully_transmitted,
            reference,
            extensions: None,
        }
    }
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=2"))]
pub struct Volumes(SequenceOf<Volume>);
impl Volumes {
    pub fn new(values: Vec<Volume>) -> Result<Self, Error> {
        if values.is_empty() || values.len() > 2 {
            return Err(Error::Length);
        }
        Ok(Self(values))
    }
    pub fn values(&self) -> &[Volume] {
        &self.0
    }
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Setup {
    pub id: RabId,
    pub address: Option<Address>,
    pub association: Option<Association>,
    pub volumes: Option<Volumes>,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Setup {
    pub fn new(id: u8) -> Self {
        Self {
            id: RabId::new(id),
            address: None,
            association: None,
            volumes: None,
            extensions: None,
        }
    }
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Released {
    pub id: RabId,
    pub volumes: Option<Volumes>,
    pub downlink_gtp_sequence: Option<u16>,
    pub uplink_gtp_sequence: Option<u16>,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Released {
    pub fn new(id: u8) -> Self {
        Self {
            id: RabId::new(id),
            volumes: None,
            downlink_gtp_sequence: None,
            uplink_gtp_sequence: None,
            extensions: None,
        }
    }
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Queued {
    pub id: RabId,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Queued {
    pub fn new(id: u8) -> Self {
        Self {
            id: RabId::new(id),
            extensions: None,
        }
    }
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Failed {
    pub id: RabId,
    pub cause: Cause,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Failed {
    pub fn new(id: u8, cause: Cause) -> Self {
        Self {
            id: RabId::new(id),
            cause,
            extensions: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssignmentResponse {
    pub setup: Vec<Setup>,
    pub released: Vec<Released>,
    pub queued: Vec<Queued>,
    pub failed: Vec<Failed>,
    pub release_failed: Vec<Failed>,
}
impl AssignmentResponse {
    pub fn validate(&self) -> Result<(), Error> {
        if self.setup.is_empty()
            && self.released.is_empty()
            && self.queued.is_empty()
            && self.failed.is_empty()
            && self.release_failed.is_empty()
        {
            return Err(Error::Length);
        }
        for item in self.failed.iter().chain(&self.release_failed) {
            Cause::from_code(item.cause.code())?;
        }
        unique_ids(
            self.setup
                .iter()
                .map(|i| i.id.get())
                .chain(self.released.iter().map(|i| i.id.get()))
                .chain(self.queued.iter().map(|i| i.id.get()))
                .chain(self.failed.iter().map(|i| i.id.get()))
                .chain(self.release_failed.iter().map(|i| i.id.get())),
        )
    }
    pub fn encode(&self, limits: Limits) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let mut ies = Vec::new();
        macro_rules! list {
            ($values:expr,$id:expr,$item:expr) => {
                if !$values.is_empty() {
                    ies.push(encode_list($id, $item, $values, limits)?);
                }
            };
        }
        list!(&self.setup, SETUP_LIST, SETUP_ITEM);
        list!(&self.released, RELEASED_LIST, RELEASED_ITEM);
        list!(&self.queued, QUEUED_LIST, QUEUED_ITEM);
        list!(&self.failed, FAILED_LIST, FAILED_ITEM);
        list!(&self.release_failed, RELEASE_FAILED_LIST, FAILED_ITEM);
        // TS 25.413 RAB Assignment declares OUTCOME, not SUCCESSFUL OUTCOME.
        Packet {
            protocol: Protocol::Ranap,
            outcome: Outcome::Intermediate,
            procedure: RAB_ASSIGNMENT,
            criticality: Criticality::Reject,
            ies,
            extensions: Vec::new(),
        }
        .encode(limits)
    }
    pub fn decode(bytes: &[u8], limits: Limits) -> Result<(Self, Vec<u16>), Error> {
        let packet = Packet::decode(Protocol::Ranap, bytes, limits)?;
        if packet.procedure != RAB_ASSIGNMENT || packet.outcome != Outcome::Intermediate {
            return Err(Error::Unsupported);
        }
        let mut notify = packet.check(
            &[
                SETUP_LIST,
                RELEASED_LIST,
                QUEUED_LIST,
                FAILED_LIST,
                RELEASE_FAILED_LIST,
                DIAGNOSTICS,
            ],
            &[],
        )?;
        let response = Self {
            setup: decode_list(&packet, SETUP_LIST, SETUP_ITEM, limits, &mut notify)?,
            released: decode_list(&packet, RELEASED_LIST, RELEASED_ITEM, limits, &mut notify)?,
            queued: decode_list(&packet, QUEUED_LIST, QUEUED_ITEM, limits, &mut notify)?,
            failed: decode_list(&packet, FAILED_LIST, FAILED_ITEM, limits, &mut notify)?,
            release_failed: decode_list(
                &packet,
                RELEASE_FAILED_LIST,
                FAILED_ITEM,
                limits,
                &mut notify,
            )?,
        };
        response.validate()?;
        for item in &response.setup {
            inspect_extensions(item.extensions.as_ref(), &mut notify)?;
            inspect_volumes(item.volumes.as_ref(), &mut notify)?;
        }
        for item in &response.released {
            inspect_extensions(item.extensions.as_ref(), &mut notify)?;
            inspect_volumes(item.volumes.as_ref(), &mut notify)?;
        }
        for item in &response.queued {
            inspect_extensions(item.extensions.as_ref(), &mut notify)?;
        }
        for item in response.failed.iter().chain(&response.release_failed) {
            inspect_extensions(item.extensions.as_ref(), &mut notify)?;
        }
        Ok((response, notify))
    }
}
fn inspect_volumes(volumes: Option<&Volumes>, notify: &mut Vec<u16>) -> Result<(), Error> {
    if let Some(volumes) = volumes {
        for volume in &volumes.0 {
            inspect_extensions(volume.extensions.as_ref(), notify)?;
        }
    }
    Ok(())
}
