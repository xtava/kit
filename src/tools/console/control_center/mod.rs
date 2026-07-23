//! Machine discovery and control-center policy for Console.
//!
//! The focused modules below keep model projection, action contributions, application effects,
//! and rendering under one canonical control-center owner.

mod actions;
mod application;
mod model;
mod render;

#[cfg(test)]
mod tests;

pub(crate) use application::run;
pub(crate) use model::{ConnectedSessionOutcome, ControlCenterOutcome, MachineConnectionRequest};
