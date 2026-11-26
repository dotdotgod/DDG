//! Client-side multi-host transport library for connecting to `ddg-agents-host`
//! over local Unix sockets, OpenSSH, `docker exec -i`, or container SSHD.

pub mod agent;
pub mod client;
pub mod dial;
pub mod install;
pub mod profile;
pub mod programs;
pub mod remote_update;

pub(crate) const AGENT_ACTIVITY_V1: &str = "agent_activity_v1";

pub use client::{
    CatalogChange, CatalogSubscription, ConnectionState, HostClient, HostEvent,
    MessageStreamHandle, RelayStreamHandle, StreamError, TabStreamHandle,
};
pub use dial::{
    DialError, DialErrorKind, DialPolicy, classify_ssh_stderr, dial, dial_and_handshake,
    dial_with_policy, sanitize_dial_error,
};
pub use install::{
    RemoteInstallInfo, RemotePlatform, ResolvedBinaries, build_atomic_upload_script,
    default_dist_dir, install_remote_host, parse_uname_output,
};
pub use profile::{
    HostProfile, HostTransport, HostsConfig, discover_docker_containers, discover_ssh_config_hosts,
    parse_docker_input, parse_ssh_input, split_safe_tokens,
};
