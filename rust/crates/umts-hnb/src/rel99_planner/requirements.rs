// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_iuh::{
    Domain,
    ranap::rab::{self, SetupOrModify, qos},
    wire::Criticality,
};
use umts_types::CnDomain;

const MAXIMUM_SUBFLOWS: usize = 7;
const MAXIMUM_COMBINATIONS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Protocol(umts_iuh::Error),
    Unsupported,
    Capacity,
}

impl From<umts_iuh::Error> for Error {
    fn from(error: umts_iuh::Error) -> Self {
        match error {
            umts_iuh::Error::Unsupported => Self::Unsupported,
            error => Self::Protocol(error),
        }
    }
}

pub(super) struct Requirements<'a> {
    pub domain: CnDomain,
    pub rab: u8,
    pub nas: Option<u8>,
    pub parameters: &'a qos::Parameters,
    pub maximum: qos::Rates,
    pub guaranteed: Option<qos::Rates>,
    pub combinations: Vec<qos::Combination>,
    pub user_plane: &'a rab::UserPlane,
}

impl<'a> Requirements<'a> {
    pub(super) fn new(domain: CnDomain, request: &'a SetupOrModify) -> Result<Self, Error> {
        request
            .validate_new(match domain {
                CnDomain::Cs => Domain::Cs,
                CnDomain::Ps => Domain::Ps,
            })
            .map_err(Error::from)?;
        let parameters = request
            .first
            .parameters
            .as_ref()
            .expect("validated new RAB parameters");
        if let Some(extension) = parameters
            .unknown_extension_ies()
            .find(|ie| ie.criticality == Criticality::Reject)
        {
            return Err(Error::Protocol(umts_iuh::Error::UnknownRejectIe(
                extension.id,
            )));
        }
        let count = parameters.combination_count();
        if !(1..=MAXIMUM_SUBFLOWS).contains(&parameters.subflows.as_slice().len())
            || !(1..=MAXIMUM_COMBINATIONS).contains(&count)
        {
            return Err(Error::Capacity);
        }
        let maximum = parameters.effective_maximum().map_err(Error::from)?;
        let guaranteed = parameters.effective_guaranteed().map_err(Error::from)?;
        let mut combinations = Vec::new();
        combinations
            .try_reserve_exact(count)
            .map_err(|_| Error::Capacity)?;
        for index in 0..count {
            combinations.push(parameters.combination(index).map_err(Error::from)?);
        }
        let nas = request.first.nas_synchronization_value();
        Ok(Self {
            domain,
            rab: request.first.id.get(),
            nas,
            parameters,
            maximum,
            guaranteed,
            combinations,
            user_plane: request
                .first
                .user_plane
                .as_ref()
                .expect("validated new RAB user plane"),
        })
    }
}

#[cfg(test)]
mod tests;
