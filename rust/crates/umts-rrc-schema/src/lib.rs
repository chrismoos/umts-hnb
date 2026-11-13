// SPDX-License-Identifier: AGPL-3.0-or-later

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/rrc.rs"));
}

pub use generated::{
    class_definitions, constant_definitions, information_elements, internode_definitions,
    pdu_definitions,
};
