// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_types::{
    CnDomain, UeKey,
    identity::{IdentityError, Imsi, MAX_IMSI_DIGITS},
    time::{CHIPS_PER_SECOND, ChipInstant},
};

const MM: u8 = 5;
const GMM: u8 = 8;
const MM_REQUEST: u8 = 0x18;
const MM_RESPONSE: u8 = 0x19;
const MM_NULL: u8 = 0x30;
const SEND_SEQUENCE_LIMIT: u8 = 4;
const GMM_REQUEST: u8 = 0x15;
const GMM_RESPONSE: u8 = 0x16;
const PD_MASK: u8 = 0xf;
const MM_TYPE_MASK: u8 = 0x3f;
const SEND_SEQUENCE_SHIFT: u32 = 6;
const MOBILE_IDENTITY_TYPE_MASK: u8 = 7;
const IMSI_TYPE: u8 = 1;
const NO_IDENTITY_TYPE: u8 = 0;
const ODD_DIGITS: u8 = 8;
const DIGIT_SHIFT: u32 = 4;
const DIGIT_MASK: u8 = 0xf;
const RESPONSE_HEADER_BYTES: usize = 3;
const CS_NO_IDENTITY_BYTES: usize = 1;
const PS_NO_IDENTITY_BYTES: usize = 3;
const T3270_CHIPS: u64 = 12 * CHIPS_PER_SECOND;
const T3370_CHIPS: u64 = 6 * CHIPS_PER_SECOND;
const T3370_MAX_TRANSMISSIONS: u8 = 5;
const MIN_RRC_IMSI_DIGITS: usize = 6;

pub fn rrc_imsi(
    identity: &umts_rrc_schema::information_elements::InitialUEIdentity,
) -> Result<Option<Imsi>, Error> {
    let umts_rrc_schema::information_elements::InitialUEIdentity::imsi(imsi) = identity else {
        return Ok(None);
    };
    if !(MIN_RRC_IMSI_DIGITS..=MAX_IMSI_DIGITS).contains(&imsi.0.len()) {
        return Err(Error::MobileIdentity(IdentityError::Length));
    }
    let mut digits = [0; MAX_IMSI_DIGITS];
    for (index, digit) in imsi.0.iter().enumerate() {
        digits[index] = digit.0;
    }
    Imsi::from_digit_values(&digits[..imsi.0.len()])
        .map(Some)
        .map_err(Error::MobileIdentity)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Header,
    MobileIdentity(IdentityError),
    IdentityType,
    Origin,
    Time,
    Phase,
    ConflictingResponse,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Length => f.write_str("Invalid NAS identification message length"),
            Self::Header => f.write_str("Invalid NAS identification header"),
            Self::MobileIdentity(error) => write!(f, "Invalid NAS mobile identity: {error}"),
            Self::IdentityType => {
                f.write_str("NAS response does not contain the requested identity")
            }
            Self::Origin => f.write_str("NAS identification UE generation does not match"),
            Self::Time => f.write_str("Invalid NAS identification radio time"),
            Self::Phase => f.write_str("NAS identification procedure is not in the required phase"),
            Self::ConflictingResponse => {
                f.write_str("Conflicting NAS identity response before upper admission")
            }
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    Imsi(Imsi),
    Unavailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response {
    pub identity: Identity,
    pub send_sequence: Option<u8>,
}
pub fn interworking_null(send_sequence: u8) -> Result<[u8; 2], Error> {
    if send_sequence >= SEND_SEQUENCE_LIMIT {
        return Err(Error::Header);
    }
    // TS 24.008 9.2.19: network-only interworking, never transmitted on the radio interface.
    Ok([MM, MM_NULL | (send_sequence << SEND_SEQUENCE_SHIFT)])
}
pub fn request(domain: CnDomain) -> [u8; 3] {
    // TS 24.008 9.2.10 and 9.4.12 use IMSI type 1 with spare/force-to-standby bits clear.
    match domain {
        CnDomain::Cs => [MM, MM_REQUEST, IMSI_TYPE],
        CnDomain::Ps => [GMM, GMM_REQUEST, IMSI_TYPE],
    }
}
pub fn decode_response(domain: CnDomain, nas: &[u8]) -> Result<Option<Response>, Error> {
    if nas.len() < 2 {
        return Err(Error::Length);
    }
    let (pd, message_type) = match domain {
        CnDomain::Cs => (MM, MM_RESPONSE),
        CnDomain::Ps => (GMM, GMM_RESPONSE),
    };
    if nas[0] & PD_MASK != pd {
        return Ok(None);
    }
    let actual_type = match domain {
        CnDomain::Cs => nas[1] & MM_TYPE_MASK,
        CnDomain::Ps => nas[1],
    };
    if actual_type != message_type {
        return Ok(None);
    }
    if nas[0] != pd {
        return Err(Error::Header);
    }
    let length = usize::from(*nas.get(2).ok_or(Error::Length)?);
    if length == 0 || nas.len() != RESPONSE_HEADER_BYTES + length {
        return Err(Error::Length);
    }
    let body = &nas[RESPONSE_HEADER_BYTES..];
    let send_sequence = match domain {
        CnDomain::Cs => Some(nas[1] >> SEND_SEQUENCE_SHIFT),
        CnDomain::Ps => None,
    };
    let identity = match body[0] & MOBILE_IDENTITY_TYPE_MASK {
        NO_IDENTITY_TYPE => {
            // TS 24.008 10.5.1.4 uses one all-zero octet for MM and three for GMM No Identity.
            let expected = match domain {
                CnDomain::Cs => CS_NO_IDENTITY_BYTES,
                CnDomain::Ps => PS_NO_IDENTITY_BYTES,
            };
            if body.len() != expected || body.iter().any(|byte| *byte != 0) {
                return Err(Error::IdentityType);
            }
            Identity::Unavailable
        }
        IMSI_TYPE => {
            let odd = body[0] & ODD_DIGITS != 0;
            let count = 1 + (body.len() - 1) * 2 - usize::from(!odd);
            if count > MAX_IMSI_DIGITS {
                return Err(Error::MobileIdentity(IdentityError::Length));
            }
            let mut digits = [0; MAX_IMSI_DIGITS];
            digits[0] = body[0] >> DIGIT_SHIFT;
            for index in 1..count {
                let byte = body[1 + (index - 1) / 2];
                let shift = ((index - 1) % 2) as u32 * DIGIT_SHIFT;
                digits[index] = (byte >> shift) & DIGIT_MASK;
            }
            if !odd
                && body
                    .last()
                    .is_none_or(|byte| byte >> DIGIT_SHIFT != DIGIT_MASK)
            {
                return Err(Error::MobileIdentity(IdentityError::Filler));
            }
            Identity::Imsi(
                Imsi::from_digit_values(&digits[..count]).map_err(Error::MobileIdentity)?,
            )
        }
        _ => return Err(Error::IdentityType),
    };
    Ok(Some(Response {
        identity,
        send_sequence,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortReason {
    Timeout,
    RadioFailure,
    Superseded,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Awaiting {
        deadline: ChipInstant,
        transmissions: u8,
    },
    Ready(Response),
    Complete(Response),
    Aborted(AbortReason),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    Unrelated,
    Ready,
    Duplicate,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    Waiting,
    Retransmitted,
    Expired,
    Inactive,
}
#[derive(Debug, PartialEq, Eq)]
pub enum PublishError<E> {
    Procedure(Error),
    Downstream(E),
}

#[derive(Clone)]
pub struct Identification {
    key: UeKey,
    domain: CnDomain,
    phase: Phase,
    observed: ChipInstant,
}
impl Identification {
    pub fn new(key: UeKey, domain: CnDomain, now: ChipInstant) -> Result<Self, Error> {
        if key.epoch != now.epoch {
            return Err(Error::Origin);
        }
        Ok(Self {
            key,
            domain,
            phase: Phase::Idle,
            observed: now,
        })
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn key(&self) -> UeKey {
        self.key
    }
    pub fn domain(&self) -> CnDomain {
        self.domain
    }
    fn time(&mut self, now: ChipInstant) -> Result<(), Error> {
        if now.epoch != self.key.epoch || now.index < self.observed.index {
            return Err(Error::Time);
        }
        self.observed = now;
        Ok(())
    }
    fn deadline(&self, now: ChipInstant) -> Result<ChipInstant, Error> {
        let duration = match self.domain {
            CnDomain::Cs => T3270_CHIPS,
            CnDomain::Ps => T3370_CHIPS,
        };
        Ok(ChipInstant {
            epoch: now.epoch,
            index: now.index.checked_add(duration).ok_or(Error::Time)?,
        })
    }
    pub fn begin_with<T, E>(
        &mut self,
        now: ChipInstant,
        publish: impl FnOnce(&[u8; 3]) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        self.time(now).map_err(PublishError::Procedure)?;
        if self.phase != Phase::Idle {
            return Err(PublishError::Procedure(Error::Phase));
        }
        let deadline = self.deadline(now).map_err(PublishError::Procedure)?;
        let result = publish(&request(self.domain)).map_err(PublishError::Downstream)?;
        self.phase = Phase::Awaiting {
            deadline,
            transmissions: 1,
        };
        Ok(result)
    }
    pub fn observe_response(
        &mut self,
        key: UeKey,
        domain: CnDomain,
        now: ChipInstant,
        nas: &[u8],
    ) -> Result<Observation, Error> {
        if key != self.key {
            return Err(Error::Origin);
        }
        if domain != self.domain {
            return Ok(Observation::Unrelated);
        }
        self.time(now)?;
        if !matches!(
            self.phase,
            Phase::Awaiting { .. } | Phase::Ready(_) | Phase::Complete(_)
        ) {
            return Ok(Observation::Unrelated);
        }
        let Some(response) = decode_response(domain, nas)? else {
            return Ok(Observation::Unrelated);
        };
        if let Phase::Ready(previous) | Phase::Complete(previous) = self.phase {
            return if previous.identity == response.identity {
                Ok(Observation::Duplicate)
            } else {
                Err(Error::ConflictingResponse)
            };
        }
        // TS 24.008 4.3.3.2 and 4.7.8.3 stop the timer on receipt, not on later registration admission.
        self.phase = Phase::Ready(response);
        Ok(Observation::Ready)
    }
    pub fn publish_with<T, E>(
        &mut self,
        publish: impl FnOnce(Response) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        let Phase::Ready(response) = self.phase else {
            return Err(PublishError::Procedure(Error::Phase));
        };
        let result = publish(response).map_err(PublishError::Downstream)?;
        self.phase = Phase::Complete(response);
        Ok(result)
    }
    pub fn poll_with<E>(
        &mut self,
        now: ChipInstant,
        publish: impl FnOnce(&[u8; 3]) -> Result<(), E>,
    ) -> Result<Poll, PublishError<E>> {
        self.time(now).map_err(PublishError::Procedure)?;
        let Phase::Awaiting {
            deadline,
            transmissions,
        } = self.phase
        else {
            return Ok(Poll::Inactive);
        };
        if now.index < deadline.index {
            return Ok(Poll::Waiting);
        }
        if self.domain == CnDomain::Cs || transmissions == T3370_MAX_TRANSMISSIONS {
            self.phase = Phase::Aborted(AbortReason::Timeout);
            return Ok(Poll::Expired);
        }
        let deadline = self.deadline(now).map_err(PublishError::Procedure)?;
        publish(&request(self.domain)).map_err(PublishError::Downstream)?;
        self.phase = Phase::Awaiting {
            deadline,
            transmissions: transmissions + 1,
        };
        Ok(Poll::Retransmitted)
    }
    pub fn abort(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        reason: AbortReason,
    ) -> Result<(), Error> {
        if key != self.key {
            return Err(Error::Origin);
        }
        self.time(now)?;
        self.phase = Phase::Aborted(reason);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
