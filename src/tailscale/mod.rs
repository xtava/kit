//! Shared Tailscale CLI status, peer identity, and authentication mechanics.
//!
//! Tools keep their own transport, transfer, and presentation policy. This module owns only the
//! stable protocol boundary that more than one tool consumes.

mod client;
mod login_url;
mod model;
mod mutagen_ssh;
mod ssh;

pub use client::{LoginEvent, TailscaleClient};
pub use login_url::{find_login_url, LoginUrl, LoginUrlError};
pub use model::{
    parse_status, Node, OperatingSystem, PeerSelectorError, Readiness, Status, StatusParseError,
};
pub use mutagen_ssh::{
    prepare_mutagen_ssh_directory, prepare_mutagen_ssh_transport, MutagenSshTransport,
};
pub use ssh::{
    prepare_known_hosts_file, RemoteCommand, SshConnectionKind, TailscaleSshError,
    TailscaleSshProcessError, TailscaleSshStateError, TailscaleSshTarget,
};
