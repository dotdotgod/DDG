//! Shared update policy and integrity, with optional GitHub and archive support.

#[cfg(feature = "network")]
pub mod archive;
#[cfg(feature = "network")]
pub mod bootstrap;
pub mod cache;
pub mod config;
pub mod error;
#[cfg(feature = "network")]
pub mod github;
pub mod integrity;
pub mod macos;
pub mod manager;
pub mod manifest;
pub mod redact;
pub mod release;
pub mod rollback;
pub mod state;
pub mod swap;
pub mod version;

pub use error::{ApplyOutcome, UpdateError, UpdateErrorCode};

#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "network"))]
mod tests_integrity;
#[cfg(test)]
mod tests_macos;
#[cfg(test)]
mod tests_state;
