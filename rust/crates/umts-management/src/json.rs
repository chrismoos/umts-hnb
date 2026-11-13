// SPDX-License-Identifier: AGPL-3.0-or-later

use prost::Name;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use serde::de::Error;
use std::sync::LazyLock;

static DESCRIPTORS: LazyLock<Result<DescriptorPool, prost_reflect::DescriptorError>> =
    LazyLock::new(|| {
        DescriptorPool::decode(
            include_bytes!(concat!(env!("OUT_DIR"), "/management-descriptors.bin")).as_ref(),
        )
    });

fn descriptor<T: Name>() -> Result<MessageDescriptor, serde_json::Error> {
    DESCRIPTORS
        .as_ref()
        .map_err(serde_json::Error::custom)?
        .get_message_by_name(&T::full_name())
        .ok_or_else(|| serde_json::Error::custom("message is not in the management protocol"))
}

pub fn to_value<T: Name>(message: &T) -> Result<serde_json::Value, serde_json::Error> {
    let dynamic = DynamicMessage::decode(descriptor::<T>()?, message.encode_to_vec().as_slice())
        .map_err(serde_json::Error::custom)?;
    serde_json::to_value(dynamic)
}

pub fn from_value<T: Name + Default>(value: serde_json::Value) -> Result<T, serde_json::Error> {
    DynamicMessage::deserialize(descriptor::<T>()?, value)?
        .transcode_to()
        .map_err(serde_json::Error::custom)
}
