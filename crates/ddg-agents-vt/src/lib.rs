//! Safe Ghostty VT 1.0.1 wrapper shared by `ddg-terminal-server` and `ddg-agents-client`.

mod ffi;
mod osc7501;
pub mod relay;
mod terminal;

pub use relay::{Relay, RelayOutput};
pub use terminal::{
    Cell, DEFAULT_CONTINUATION_MAX_BYTES, DEFAULT_SCROLLBACK_MAX_BYTES, Vt, style_runs,
};
