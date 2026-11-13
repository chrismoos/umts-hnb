// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_iuh::NasSelector;
use umts_rrc_schema::information_elements as ie;
const IDNNS_BYTES: usize = 2;

pub fn convert(value: &ie::IntraDomainNasNodeSelector) -> Result<NasSelector, ()> {
    // TS 25.331 10.3.1.6 defines a fixed 16-bit value also used in TS 25.468 IDNNS.
    let bytes = rasn::uper::encode(value).map_err(|_| ())?;
    if bytes.len() != IDNNS_BYTES {
        return Err(());
    }
    rasn::uper::decode(&bytes).map_err(|_| ())
}
