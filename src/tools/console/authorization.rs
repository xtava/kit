use anyhow::bail;
use wezterm_codec::{Pdu, RequestOperation};
use wezterm_mux::client::ClientId;
use wezterm_mux_server_impl::authorization::{RequestAuthorizer, ServerIssuedIdentity};

/// Kit Console's closed host policy for its private per-user mux socket.
///
/// Generic WezTerm code classifies requests and validates attachment identity.
/// This policy decides which classified operations the Console product exposes.
pub struct ConsoleAuthorizer;

fn authorize_bootstrap_operation(operation: RequestOperation) -> anyhow::Result<()> {
    match operation {
        RequestOperation::Ping
        | RequestOperation::GetCodecVersion
        | RequestOperation::GetBuildIdentity
        | RequestOperation::RegisterClient => Ok(()),
        RequestOperation::ListPanes
        | RequestOperation::Spawn
        | RequestOperation::WriteToPane
        | RequestOperation::SendKey
        | RequestOperation::SendMouse
        | RequestOperation::SendPaste
        | RequestOperation::Resize
        | RequestOperation::SetZoom
        | RequestOperation::GetLines
        | RequestOperation::GetRenderChanges
        | RequestOperation::GetTlsCredentials
        | RequestOperation::SearchScrollback
        | RequestOperation::SplitPane
        | RequestOperation::KillPane
        | RequestOperation::GetClientList
        | RequestOperation::SetWindowWorkspace
        | RequestOperation::SetFocusedPane
        | RequestOperation::GetImageCell
        | RequestOperation::MovePaneToNewTab
        | RequestOperation::ActivatePaneDirection
        | RequestOperation::GetRenderableDimensions
        | RequestOperation::SetPalette
        | RequestOperation::SetTabTitle
        | RequestOperation::SetWindowTitle
        | RequestOperation::RenameWorkspace
        | RequestOperation::EraseScrollback
        | RequestOperation::GetPaneDirection
        | RequestOperation::AdjustPaneSize
        | RequestOperation::ServiceDrain => {
            bail!("ordinary request {operation:?} is invalid during bootstrap")
        }
    }
}

fn authorize_established_operation(operation: RequestOperation) -> anyhow::Result<()> {
    match operation {
        RequestOperation::Ping
        | RequestOperation::ListPanes
        | RequestOperation::Spawn
        | RequestOperation::WriteToPane
        | RequestOperation::SendKey
        | RequestOperation::SendMouse
        | RequestOperation::SendPaste
        | RequestOperation::Resize
        | RequestOperation::GetLines
        | RequestOperation::GetRenderChanges
        | RequestOperation::SearchScrollback
        | RequestOperation::KillPane
        | RequestOperation::GetImageCell
        | RequestOperation::GetRenderableDimensions
        | RequestOperation::EraseScrollback
        | RequestOperation::GetPaneDirection
        | RequestOperation::SetPalette
        | RequestOperation::SetTabTitle
        | RequestOperation::SetWindowTitle
        | RequestOperation::ServiceDrain => Ok(()),
        RequestOperation::GetCodecVersion
        | RequestOperation::GetBuildIdentity
        | RequestOperation::RegisterClient
        | RequestOperation::GetTlsCredentials
        | RequestOperation::GetClientList
        | RequestOperation::SetWindowWorkspace
        | RequestOperation::SetZoom
        | RequestOperation::SplitPane
        | RequestOperation::SetFocusedPane
        | RequestOperation::MovePaneToNewTab
        | RequestOperation::ActivatePaneDirection
        | RequestOperation::AdjustPaneSize
        | RequestOperation::RenameWorkspace => {
            bail!("request {operation:?} is not exposed by Kit Console")
        }
    }
}

impl RequestAuthorizer for ConsoleAuthorizer {
    fn authorize_registration(
        &self,
        proxy: Option<&ClientId>,
        client_id: &ClientId,
        is_proxy: bool,
    ) -> anyhow::Result<()> {
        if is_proxy || proxy.is_some() {
            bail!("Kit Console does not accept proxy registration")
        }
        if client_id.ssh_auth_sock.is_some() {
            bail!("Kit Console client registration must not forward an SSH agent")
        }
        Ok(())
    }

    fn authorize_bootstrap(
        &self,
        operation: RequestOperation,
        _request: &Pdu,
    ) -> anyhow::Result<()> {
        authorize_bootstrap_operation(operation)
    }

    fn authorize(
        &self,
        _identity: &ServerIssuedIdentity,
        operation: RequestOperation,
        _request: &Pdu,
    ) -> anyhow::Result<()> {
        authorize_established_operation(operation)
    }
}

#[cfg(test)]
mod tests {
    use super::{authorize_bootstrap_operation, authorize_established_operation};
    use wezterm_codec::RequestOperation;

    #[test]
    fn console_bootstrap_exposes_only_the_closed_handshake() {
        assert!(authorize_bootstrap_operation(RequestOperation::GetCodecVersion).is_ok());
        assert!(authorize_bootstrap_operation(RequestOperation::GetBuildIdentity).is_ok());
        assert!(authorize_bootstrap_operation(RequestOperation::RegisterClient).is_ok());
        assert!(authorize_bootstrap_operation(RequestOperation::Spawn).is_err());
        assert!(authorize_bootstrap_operation(RequestOperation::ListPanes).is_err());
    }

    #[test]
    fn embedded_wezterm_terminal_authorization() {
        assert!(authorize_established_operation(RequestOperation::SendKey).is_ok());
        assert!(authorize_established_operation(RequestOperation::SendMouse).is_ok());
        assert!(authorize_established_operation(RequestOperation::GetLines).is_ok());
        assert!(authorize_established_operation(RequestOperation::SetTabTitle).is_ok());
        assert!(authorize_established_operation(RequestOperation::GetTlsCredentials).is_err());
        assert!(authorize_established_operation(RequestOperation::GetClientList).is_err());
        assert!(authorize_established_operation(RequestOperation::SetPalette).is_ok());
        assert!(authorize_established_operation(RequestOperation::SetWindowTitle).is_ok());
        assert!(authorize_established_operation(RequestOperation::SplitPane).is_err());
        assert!(authorize_established_operation(RequestOperation::MovePaneToNewTab).is_err());
        assert!(authorize_established_operation(RequestOperation::SetFocusedPane).is_err());
        assert!(authorize_established_operation(RequestOperation::AdjustPaneSize).is_err());
        assert!(authorize_established_operation(RequestOperation::RenameWorkspace).is_err());
    }
}
