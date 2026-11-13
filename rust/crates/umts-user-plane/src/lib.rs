// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod gtpu;
pub mod iu_cs;
pub mod iuup;
pub mod media;
pub mod packet;
pub mod rtp;
pub mod uplink_media;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    Length,
    Header,
    Capacity,
    HeaderCrc,
    Unsupported(u8),
    RequiredExtension(u8),
    Version(u8),
}
