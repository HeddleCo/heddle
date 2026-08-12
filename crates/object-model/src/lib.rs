// SPDX-License-Identifier: Apache-2.0
//! Heddle's content-addressed object model and stable codecs.

pub mod compact;
pub mod error;
pub mod object;

pub use error::{HeddleError, RecoveryDetails};
