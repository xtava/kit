use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use super::{
    output::{ProcessInputHandle, ProcessOutputHandle},
    report::{ProcessFailureKind, ProcessFailureReport, ProcessReport, ProcessRunId},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainmentStrength {
    CompleteTree,
    ProcessGroup,
}

pub struct StartedProcess {
    pub session: ProcessSession,
    pub input: ProcessInputHandle,
    pub stdout: ProcessOutputHandle,
    pub stderr: ProcessOutputHandle,
}

impl StartedProcess {
    pub(crate) fn new(
        session: ProcessSession,
        input: ProcessInputHandle,
        stdout: ProcessOutputHandle,
        stderr: ProcessOutputHandle,
    ) -> Self {
        Self { session, input, stdout, stderr }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlAcknowledgement {
    Accepted,
    AlreadyStopping,
    AlreadyCompleted,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProcessControlError {
    #[error("process owner is unavailable")]
    OwnerUnavailable,
}

pub(crate) enum ControlRequest {
    Cancel { acknowledgement: oneshot::Sender<ControlAcknowledgement> },
    ForceKill { acknowledgement: oneshot::Sender<ControlAcknowledgement> },
    OwnerDropped,
}

pub(crate) type ProcessCompletion = Result<ProcessReport, ProcessFailureReport>;

pub(crate) fn process_session(
    run_id: ProcessRunId,
    containment: ContainmentStrength,
    mut owner_task_failed: ProcessFailureReport,
) -> (ProcessSession, mpsc::UnboundedReceiver<ControlRequest>, oneshot::Sender<ProcessCompletion>) {
    owner_task_failed.failure = ProcessFailureKind::OwnerTaskFailed;
    let (requests, request_receiver) = mpsc::unbounded_channel();
    let (completion_sender, completion) = oneshot::channel();
    (
        ProcessSession::new(run_id, containment, requests, completion, owner_task_failed),
        request_receiver,
        completion_sender,
    )
}

pub struct ProcessSession {
    run_id: ProcessRunId,
    containment: ContainmentStrength,
    control: ProcessControl,
    completion: Option<oneshot::Receiver<ProcessCompletion>>,
    owner_task_failed: Option<ProcessFailureReport>,
    completion_observed: bool,
}

impl ProcessSession {
    pub(crate) fn new(
        run_id: ProcessRunId,
        containment: ContainmentStrength,
        requests: mpsc::UnboundedSender<ControlRequest>,
        completion: oneshot::Receiver<ProcessCompletion>,
        owner_task_failed: ProcessFailureReport,
    ) -> Self {
        Self {
            run_id,
            containment,
            control: ProcessControl::new(requests),
            completion: Some(completion),
            owner_task_failed: Some(owner_task_failed),
            completion_observed: false,
        }
    }

    pub fn run_id(&self) -> ProcessRunId {
        self.run_id
    }

    pub fn containment(&self) -> ContainmentStrength {
        self.containment
    }

    pub fn control(&self) -> ProcessControl {
        self.control.clone()
    }

    pub async fn wait(mut self) -> Result<ProcessReport, ProcessFailureReport> {
        let completion =
            self.completion.take().expect("a process session owns exactly one completion receiver");
        let report = match completion.await {
            Ok(report) => report,
            Err(_) => Err(self
                .owner_task_failed
                .take()
                .expect("a process session owns one owner-task failure report")),
        };
        self.completion_observed = true;
        report
    }
}

impl Drop for ProcessSession {
    fn drop(&mut self) {
        if !self.completion_observed {
            let _ = self.control.requests.send(ControlRequest::OwnerDropped);
        }
    }
}

#[derive(Clone)]
pub struct ProcessControl {
    requests: mpsc::UnboundedSender<ControlRequest>,
}

impl ProcessControl {
    pub(crate) fn new(requests: mpsc::UnboundedSender<ControlRequest>) -> Self {
        Self { requests }
    }

    pub async fn cancel(&self) -> Result<ControlAcknowledgement, ProcessControlError> {
        self.request(|acknowledgement| ControlRequest::Cancel { acknowledgement }).await
    }

    pub async fn force_kill(&self) -> Result<ControlAcknowledgement, ProcessControlError> {
        self.request(|acknowledgement| ControlRequest::ForceKill { acknowledgement }).await
    }

    async fn request(
        &self,
        request: impl FnOnce(oneshot::Sender<ControlAcknowledgement>) -> ControlRequest,
    ) -> Result<ControlAcknowledgement, ProcessControlError> {
        let (acknowledgement, acknowledged) = oneshot::channel();
        self.requests
            .send(request(acknowledgement))
            .map_err(|_| ProcessControlError::OwnerUnavailable)?;
        acknowledged.await.map_err(|_| ProcessControlError::OwnerUnavailable)
    }
}
