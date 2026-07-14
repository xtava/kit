//! Single-flight process action execution outside the terminal event loop.

use tokio::sync::mpsc;

use super::host::{self, ActionError, ProcessAction};
use super::model::ProcessKey;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ActionRequest {
    pub(super) key: ProcessKey,
    pub(super) action: ProcessAction,
}

pub(super) struct ActionResult {
    pub(super) request: ActionRequest,
    pub(super) result: Result<(), ActionError>,
}

pub(super) struct ActionController {
    sender: mpsc::Sender<ActionResult>,
    receiver: mpsc::Receiver<ActionResult>,
    active: Option<ActionRequest>,
}

impl ActionController {
    pub(super) fn new() -> Self {
        let (sender, receiver) = mpsc::channel(1);
        Self { sender, receiver, active: None }
    }

    pub(super) fn start(
        &mut self,
        key: ProcessKey,
        action: ProcessAction,
    ) -> Result<ActionRequest, ActionRequest> {
        if let Some(active) = self.active {
            return Err(active);
        }
        let request = ActionRequest { key, action };
        self.active = Some(request);
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            let result = host::send_action(key, action);
            let _ = sender.blocking_send(ActionResult { request, result });
        });
        Ok(request)
    }

    pub(super) async fn recv(&mut self) -> ActionResult {
        let result = self.receiver.recv().await.expect("controller retains its completion sender");
        debug_assert_eq!(self.active, Some(result.request));
        self.active = None;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_flight_clears_after_a_typed_failure() {
        let mut controller = ActionController::new();
        let key = ProcessKey { pid: 1, start_token: 0 };
        let request = controller.start(key, ProcessAction::ForceTerminate).unwrap();
        assert_eq!(controller.start(key, ProcessAction::ForceTerminate), Err(request));

        let result = controller.recv().await;
        assert_eq!(result.request, request);
        assert!(result.result.is_err());
        assert!(controller.start(key, ProcessAction::ForceTerminate).is_ok());
    }
}
