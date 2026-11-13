// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rrc::establishment::DomainStarts;
use umts_rrc::nas::NasPublishError;

pub(super) fn setup_domains(rabs: &[umts_rrc::dedicated::NewRab]) -> [bool; 2] {
    let mut domains = [false; 2];
    for rab in rabs {
        domains[match rab.domain {
            CnDomain::Cs => 0,
            CnDomain::Ps => 1,
        }] = true;
    }
    domains
}

pub(super) fn setup_starts(
    counters: umts_rrc::dedicated::SetupCounters,
    domains: [bool; 2],
) -> DomainStarts {
    let start = counters.new_bearer_start().map(|start| start.get());
    // TS 25.331 8.2.2.3 calculates the per-domain list after saving the new-bearer START.
    let recalculated = counters.domain_starts();
    DomainStarts {
        cs: recalculated.cs.or(if domains[0] { start } else { None }),
        ps: recalculated.ps.or(if domains[1] { start } else { None }),
    }
}

impl ControlSecurity<'_> {
    pub(super) fn admit_starts_with<T>(
        &mut self,
        key: UeKey,
        protection: ControlProtection,
        starts: DomainStarts,
        admit: impl FnOnce(&mut BearerCiphers) -> Result<T, SignallingError>,
    ) -> Result<T, SignallingError> {
        let publication = self
            .change
            .as_ref()
            .ok_or(SignallingError::Phase)?
            .protection;
        if !publication.matches(protection) {
            return Err(SignallingError::Nas(NasError::SecurityRequired));
        }
        self.router
            .update_starts_with(key, starts, || admit(self.ciphers))
            .map_err(|error| match error {
                NasPublishError::Procedure(error) => SignallingError::Nas(error),
                NasPublishError::Downstream(error) => error,
            })
    }
}

#[cfg(test)]
mod tests;
