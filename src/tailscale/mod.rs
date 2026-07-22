//! Shared Tailscale CLI status, peer identity, and authentication mechanics.
//!
//! Tools keep their own transport, transfer, and presentation policy. This module owns only the
//! stable protocol boundary that more than one tool consumes.

mod client;
mod login_url;
mod model;

pub use client::{LoginEvent, TailscaleClient};
pub use login_url::{find_login_url, LoginUrl, LoginUrlError};
pub use model::{parse_status, Node, PeerSelectorError, Readiness, Status, StatusParseError};
