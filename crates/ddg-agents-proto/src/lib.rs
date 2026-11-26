//! Shared wire framing, JSON-RPC 2.0 messages, minimal `ddg-terminal-server`
//! and `ddg-agents-host` contracts, plugin extension types, and OpenRPC schema
//! generation for DDG Agents.

pub mod frame;
pub mod holder;
pub mod host;
pub mod id;
pub mod openrpc;
pub mod plugin;
pub mod rpc;
pub mod terminal;
