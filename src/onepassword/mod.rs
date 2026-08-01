//! Shared 1Password process boundary.
//!
//! The official `op` CLI owns authentication, secret resolution, and output masking. This module
//! owns Kit's one typed invocation path: validated references, bounded zeroizing secret reads, and
//! fixed masking-on `op run` execution.

mod client;
mod environment;
mod sensitive;

pub use client::{OpClient, OpError, PreparedOpRun, SecretReference, SecretReferenceError};
pub use environment::{parse_dotenv, DotenvParseError, EnvironmentFileError, OpEnvironment};
pub use sensitive::{SecretBytes, SecretBytesError, MAX_SECRET_BYTES};

pub(crate) use client::StderrPolicy;
pub(crate) use sensitive::SensitiveBuffer;
