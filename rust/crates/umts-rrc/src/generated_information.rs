// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::EncodedInformation;
use rasn::{Decode, Encode, types::BitStr};
use umts_rrc_codec::{CodecError, SibType, validate_system_information};

pub(crate) fn encode<T: Encode + Decode>(
    kind: SibType,
    value: &T,
) -> Result<EncodedInformation, CodecError> {
    let bytes = rasn::uper::encode(value).map_err(|_| CodecError::Encode)?;
    let bits = BitStr::from_slice(&bytes);
    let mut decoder =
        rasn::uper::de::Decoder::<0, 0>::new(bits, rasn::uper::de::DecoderOptions::unaligned());
    T::decode(&mut decoder).map_err(|_| CodecError::Decode)?;
    let bit_length = bits.len() - decoder.input().len();
    validate_system_information(kind, &bytes, bit_length)?;
    Ok(EncodedInformation { bytes, bit_length })
}

pub(crate) fn decode<T: Decode>(encoded: &EncodedInformation) -> Result<T, CodecError> {
    let bits = BitStr::from_slice(&encoded.bytes)
        .get(..encoded.bit_length)
        .ok_or(CodecError::InvalidBitLength)?;
    let mut decoder =
        rasn::uper::de::Decoder::<0, 0>::new(bits, rasn::uper::de::DecoderOptions::unaligned());
    let value = T::decode(&mut decoder).map_err(|_| CodecError::Decode)?;
    if !decoder.input().is_empty() {
        return Err(CodecError::InvalidBitLength);
    }
    Ok(value)
}
