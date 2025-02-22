/* This Source Code Form is subject to the terms of the Mozilla Public
* License, v. 2.0. If a copy of the MPL was not distributed with this
* file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub mod error;
pub mod intermediate_representation;
pub mod parser;
pub mod util;

cfg_if::cfg_if! {
    if #[cfg(feature = "client-lib")] {
        pub mod client;
        pub use crate::client::*;
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub mod fixtures;

const SUPPORT_URL_LOADING: bool = true;
