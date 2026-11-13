// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod proto {
    tonic::include_proto!("umts.hnb.management.v1");
}

pub mod bridge;
pub mod client;
pub mod json;
pub mod server;
pub mod service;
