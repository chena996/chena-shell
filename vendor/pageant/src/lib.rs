//! Pageant SSH agent transport protocol implementation.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

#[cfg(windows)]
mod pageant_impl;

#[cfg(windows)]
pub use pageant_impl::*;
