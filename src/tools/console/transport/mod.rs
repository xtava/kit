//! Supervised transport for a remote Console mux connection.
//!
//! The transport owns only opaque bytes and the OpenSSH process lifecycle. Machine discovery,
//! authentication preflight, remote service state, mux reconnect, and presentation remain with the
//! Console orchestrator.

mod relay;

pub(crate) use relay::{
    PreparedRelayEpoch, RelayEpochFailure, RelayEpochProvider, SshRelay, SshRelayError,
};
