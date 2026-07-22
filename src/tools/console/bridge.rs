use anyhow::{bail, Context, Result};
use tokio::io::{AsyncWriteExt, BufReader};

use crate::framework::process::ProcessSupervisor;

use super::client::{console_socket_path, probe_console_socket, ConsoleSocketProbe};
use super::service::{self, ConsoleStatus};

const COPY_BUFFER_BYTES: usize = 32 * 1024;

/// Forward one bounded opaque mux byte stream between stdio and the local native agent socket.
///
/// The bridge does not decode requests, create sessions, resolve a shell, or inherit any client
/// identity. Its stdout is protocol-only; every diagnostic is returned to the process boundary and
/// rendered on stderr by `main`.
pub async fn run() -> Result<()> {
    let processes =
        ProcessSupervisor::bootstrap().context("starting Console bridge supervision")?;
    let status = service::status(&processes).await?;
    if !matches!(status, ConsoleStatus::Ready { .. }) {
        bail!("Console bridge requires a ready native service: {}", status.text());
    }

    // Close the status connection before opening the sole opaque bridge stream, then revalidate the
    // path to close the inspection/connect race as far as Unix path ownership permits.
    match probe_console_socket()? {
        ConsoleSocketProbe::Ready => {}
        ConsoleSocketProbe::Missing { path } => {
            bail!("Console agent socket {} is missing", path.display())
        }
        ConsoleSocketProbe::WrongOwner { path, expected_uid, actual_uid } => bail!(
            "Console path {} is owned by uid {}, expected uid {}",
            path.display(),
            actual_uid,
            expected_uid
        ),
        ConsoleSocketProbe::Rejected { path, detail } => {
            bail!("Console agent socket {} was rejected: {detail}", path.display())
        }
    }

    let path = console_socket_path()?;
    let socket = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to Console agent socket {}", path.display()))?;
    let (socket_read, mut socket_write) = socket.into_split();
    let mut stdin = BufReader::with_capacity(COPY_BUFFER_BYTES, tokio::io::stdin());
    let mut socket_read = BufReader::with_capacity(COPY_BUFFER_BYTES, socket_read);
    let mut stdout = tokio::io::stdout();

    let client_to_agent = async {
        tokio::io::copy(&mut stdin, &mut socket_write)
            .await
            .context("forwarding Console client bytes to the agent")?;
        socket_write.shutdown().await.context("closing the Console agent input half")
    };
    let agent_to_client = async {
        tokio::io::copy(&mut socket_read, &mut stdout)
            .await
            .context("forwarding Console agent bytes to the client")?;
        stdout.flush().await.context("flushing Console bridge stdout")
    };
    tokio::try_join!(client_to_agent, agent_to_client)?;
    Ok(())
}
