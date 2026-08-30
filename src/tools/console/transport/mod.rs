//! Authenticated tailnet transport for a remote Console mux connection.
//!
//! The target gateway and client relay own the versioned handshake and opaque byte transport.
//! Machine discovery, mux reconnect, and presentation remain with the Console orchestrator.

mod gateway;
pub(crate) mod protocol;
mod tailnet;

pub(crate) use gateway::{GatewayControl, PreparedGateway, CONSOLE_GATEWAY_PORT};
pub(crate) use tailnet::{
    PreparedRelayEpoch, RelayEpochFailure, RelayEpochOutcomeKind, RelayEpochProvider, TailnetRelay,
    TailnetRelayError,
};
