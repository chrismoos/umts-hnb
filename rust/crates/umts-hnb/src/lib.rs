// SPDX-License-Identifier: AGPL-3.0-or-later

mod access;
mod access_message;
#[cfg(test)]
mod access_rx;
#[cfg(test)]
mod broadcast;
pub mod cell;
mod common_am;
mod common_control;
mod common_control_tx;
mod common_rx;
mod common_transport;
pub mod common_tx;
mod common_um;
mod connection_security;
mod controller;
mod core_link;
mod dedicated_bearers;
mod dedicated_controller;
mod dedicated_rx;
mod dedicated_tx;
mod epoch;
mod hsdpa_packet;
mod hsdpa_rx;
mod hsdpa_session;
mod hsdpa_tx;
mod hsupa_rx;
mod hsupa_session;
mod hsupa_tx;
mod packet_bearers;
mod packet_rabs;
mod rab_handoff;
mod radio_assignment;
mod radio_io;
mod radio_resources;
mod rel99_admission;
mod rel99_plan;
mod rel99_planner;
mod rel99_traffic;
mod serving;
mod signalling;
mod tx_evidence;
mod voice_bearers;

#[cfg(test)]
mod test_support;

mod application;
mod main_management;
mod thread_scheduling;
mod uplink_noise;
pub use application::run_cli as run;
