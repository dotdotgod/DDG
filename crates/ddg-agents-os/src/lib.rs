//! OS primitives for DDG state and local daemon coordination.

#![cfg(any(unix, windows))]

#[cfg(windows)]
mod windows;

pub mod fs;
pub mod ipc;
pub mod lock;
pub mod paths;
pub mod process;

pub mod replace;

#[cfg(all(test, unix))]
mod tests;
