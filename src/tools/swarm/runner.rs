use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    num::NonZeroU64,
    path::PathBuf,
    time::Duration,
};

use futures_util::{stream::FuturesUnordered, StreamExt};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::watch;

use crate::framework::process::{
    CommandSpec, CommandSpecError, DetachedControlError, DetachedLaunchTransaction,
    DetachedLifetimeRequirement, DetachedOutputPolicy, DetachedProcessReceipt, DetachedProcessSpec,
    DetachedProcessStatus, DetachedRecordPolicy, DetachedRollbackError, DetachedStartError,
    EnvironmentBase, ProcessEnvironment, ProcessEnvironmentError, ProcessFailureReport,
    ProcessLabel, ProcessRunId, ProcessSupervisor, TerminationPolicy,
};

use super::{
    codex::{CodexClient, CodexError, StructuredOutput, TransportEvent, TurnRequest, TurnResult},
    limiter::{PermitError, TurnPermit, TurnPermitPool},
    model::{
        AgentId, DebatePolicy, DevilOutput, ExpertOutput, ExpertRole, PlannerOutput,
        RebuttalOutput, RunStatus, Stage, SwarmEvent, SwarmId, SwarmOwner, SwarmProjection,
        SynthesisOutput, WaitReason,
    },
    prompts, report,
    store::{JournalSink, StoreError, SwarmStore},
};

const CONTROL_INTERVAL: Duration = Duration::from_millis(100);
const RETRY_DELAY: Duration = Duration::from_millis(100);
const TURN_PERMIT_LIMIT: usize = 8;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const DETACHED_RECORD_POLICY: DetachedRecordPolicy = match DetachedRecordPolicy::new(
    NonZeroU64::new(64 * 1024 * 1024).expect("detached record limit is nonzero"),
) {
    Ok(policy) => policy,
    Err(_) => panic!("swarm record limit exceeds the framework maximum"),
};

#[derive(Clone)]
pub struct RunOwner {
    store: SwarmStore,
    codex: CodexClient,
    permits: TurnPermitPool,
}

#[derive(Clone)]
pub struct SwarmLauncher {
    store: SwarmStore,
    executable: PathBuf,
    processes: ProcessSupervisor,
    startup_timeout: Duration,
}

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("construct detached swarm owner command: {0}")]
    Command(#[from] CommandSpecError),
    #[error("construct detached swarm owner environment: {0}")]
    Environment(#[from] ProcessEnvironmentError),
    #[error("start detached swarm owner: {0}")]
    Start(#[from] DetachedStartError),
    #[error("inspect or stop detached swarm owner: {0}")]
    Control(#[from] DetachedControlError),
    #[error("resolve current Kit executable: {0}")]
    Executable(#[source] std::io::Error),
    #[error("swarm owner for {id} exited before publishing RunStarted")]
    Exited { id: SwarmId },
    #[error("swarm owner for {id} had an infrastructure failure: {report:?}")]
    InfrastructureFailure { id: SwarmId, report: Box<ProcessFailureReport> },
    #[error("swarm owner for {id} did not publish RunStarted within five seconds")]
    Timeout { id: SwarmId },
    #[error("commit detached swarm owner for {id}: {message}")]
    Commit { id: SwarmId, message: String },
    #[error("rollback detached swarm owner launch: {0}")]
    Rollback(#[source] DetachedRollbackError<Box<LaunchError>>),
    #[error("{cause}; committed launch cleanup failed ({cleanup}); recovery receipt: {receipt}")]
    Cleanup { cause: Box<LaunchError>, cleanup: DetachedControlError, receipt: String },
    #[error("{cause}; persist terminal failed-launch state: {source}")]
    TerminalState {
        cause: Box<LaunchError>,
        #[source]
        source: LaunchFailurePersistenceError,
    },
}

#[derive(Debug, Error)]
pub enum LaunchFailurePersistenceError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Report(#[from] super::report::ReportError),
    #[error("cannot persist failed launch from derived {0:?} state")]
    InvalidState(RunStatus),
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Codex(Box<CodexError>),
    #[error(transparent)]
    Permit(#[from] PermitError),
    #[error(transparent)]
    Report(#[from] super::report::ReportError),
    #[error("serialize deterministic stage prompt: {0}")]
    Prompt(#[from] serde_json::Error),
    #[error("swarm was cancelled")]
    Cancelled,
    #[error("parallel stage stopped after a mandatory peer failed")]
    StageCancelled,
    #[error("read cancellation request: {0}")]
    Control(String),
    #[error("agent {agent} exhausted retries: {message}")]
    AgentExhausted { agent: AgentId, message: String },
    #[error("{cause}; Codex turn finalization failed ({cleanup})")]
    Finalize { cause: Box<RunnerError>, cleanup: Box<CodexError> },
}

impl From<CodexError> for RunnerError {
    fn from(error: CodexError) -> Self {
        Self::Codex(Box::new(error))
    }
}

#[derive(Clone, Debug)]
enum ControlState {
    Running,
    Cancelled,
    Failed(String),
}

#[derive(Clone)]
struct AgentTurn {
    agent: AgentId,
    stage: Stage,
    prompt: String,
    resume_thread_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct HardenedRecord {
    agent: AgentId,
    role: ExpertRole,
    first_pass: ExpertOutput,
    rebuttal: Option<RebuttalOutput>,
}

impl SwarmLauncher {
    pub fn installed(store: SwarmStore, processes: ProcessSupervisor) -> Result<Self, LaunchError> {
        let executable = std::env::current_exe().map_err(LaunchError::Executable)?;
        Ok(Self { store, executable, processes, startup_timeout: STARTUP_TIMEOUT })
    }

    pub fn new(store: SwarmStore, executable: PathBuf, processes: ProcessSupervisor) -> Self {
        Self { store, executable, processes, startup_timeout: STARTUP_TIMEOUT }
    }

    #[cfg(test)]
    fn with_timeout(
        store: SwarmStore,
        executable: PathBuf,
        processes: ProcessSupervisor,
        startup_timeout: Duration,
    ) -> Self {
        Self { store, executable, processes, startup_timeout }
    }

    pub async fn launch(&self, id: &SwarmId) -> Result<ProcessRunId, LaunchError> {
        let spec = self.store.load_spec(id)?;
        let command = CommandSpec::new(
            self.executable.as_os_str().to_owned(),
            vec![OsString::from("swarm"), OsString::from("__drive"), id.as_str().into()],
            spec.working_directory,
            ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?,
            ProcessLabel::new(format!("Swarm {id} owner")).expect("swarm ids are safe labels"),
        )?;
        let transaction = self
            .processes
            .launch_detached(DetachedProcessSpec::new(
                command,
                DetachedOutputPolicy::Record(DETACHED_RECORD_POLICY),
                DetachedOutputPolicy::Record(DETACHED_RECORD_POLICY),
                DetachedLifetimeRequirement::InvocationIndependent,
                TerminationPolicy::new(Duration::from_secs(2)),
            ))
            .await?;
        let process_run_id = transaction.receipt().run_id();
        let prepared = async {
            let journal = self.store.start_journal(id)?;
            journal
                .append(SwarmEvent::RunStarted {
                    owner: SwarmOwner::new(transaction.receipt().clone()),
                })
                .await?;
            journal.shutdown().await?;
            Ok::<(), LaunchError>(())
        }
        .await;
        if let Err(cause) = prepared {
            let cleanup = rollback_swarm_launch(transaction, cause).await;
            return Err(self.finish_failed_launch(id, cleanup).await);
        }

        let receipt = match transaction.commit() {
            Ok(receipt) => receipt,
            Err(error) => {
                let cause = LaunchError::Commit { id: id.clone(), message: error.to_string() };
                let cleanup = rollback_swarm_launch(error.into_transaction(), cause).await;
                return Err(self.finish_failed_launch(id, cleanup).await);
            }
        };

        let startup = async {
            let deadline = tokio::time::Instant::now() + self.startup_timeout;
            loop {
                let journal = self.store.read_journal(id)?;
                if journal.records.len() > 1 {
                    return Ok(());
                }
                match self.processes.inspect_detached(&receipt).await? {
                    DetachedProcessStatus::Running | DetachedProcessStatus::Stopping => {}
                    DetachedProcessStatus::Completed(_) => {
                        return Err(LaunchError::Exited { id: id.clone() });
                    }
                    DetachedProcessStatus::Failed(report) => {
                        return Err(LaunchError::InfrastructureFailure {
                            id: id.clone(),
                            report: Box::new(report),
                        });
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(LaunchError::Timeout { id: id.clone() });
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        .await;

        match startup {
            Ok(()) => Ok(process_run_id),
            Err(cause) => {
                let cleanup =
                    cleanup_committed_swarm_launch(&self.processes, &receipt, cause).await;
                Err(self.finish_failed_launch(id, cleanup).await)
            }
        }
    }

    async fn finish_failed_launch(
        &self,
        id: &SwarmId,
        cleanup: LaunchCleanupOutcome,
    ) -> LaunchError {
        let cause = match cleanup {
            LaunchCleanupOutcome::TerminalConfirmed(cause) => cause,
            LaunchCleanupOutcome::Unconfirmed(cause) => return cause,
        };
        match self.persist_failed_launch(id, &cause).await {
            Ok(()) => cause,
            Err(source) => LaunchError::TerminalState { cause: Box::new(cause), source },
        }
    }

    async fn persist_failed_launch(
        &self,
        id: &SwarmId,
        cause: &LaunchError,
    ) -> Result<(), LaunchFailurePersistenceError> {
        let current = self.store.read_journal(id)?.projection;
        let projection = match current.status {
            RunStatus::Queued => return Ok(()),
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled => current,
            RunStatus::Running | RunStatus::Cancelling => {
                let journal = self.store.start_journal(id)?;
                let event = if current.status == RunStatus::Cancelling {
                    SwarmEvent::RunCancelled {}
                } else {
                    SwarmEvent::RunFailed { error: launch_failure_journal_message(cause) }
                };
                let receipt = journal.append(event).await?;
                journal.shutdown().await?;
                receipt.projection
            }
            RunStatus::Orphaned | RunStatus::Unavailable => {
                return Err(LaunchFailurePersistenceError::InvalidState(current.status));
            }
        };
        self.store.write_result(&projection)?;
        report::write(&self.store, &projection)?;
        Ok(())
    }
}

fn launch_failure_journal_message(error: &LaunchError) -> String {
    match error {
        // The encoded receipt is control authority for an explicit recovery command. Keep it in
        // the returned operator error, never in the journal projection, result, or report.
        LaunchError::Cleanup { cause, .. } => launch_failure_journal_message(cause),
        _ => error.to_string(),
    }
}

enum LaunchCleanupOutcome {
    TerminalConfirmed(LaunchError),
    Unconfirmed(LaunchError),
}

async fn rollback_swarm_launch(
    transaction: DetachedLaunchTransaction,
    cause: LaunchError,
) -> LaunchCleanupOutcome {
    match transaction.rollback(Box::new(cause)).await {
        Ok(cause) => LaunchCleanupOutcome::TerminalConfirmed(*cause),
        Err(error) => LaunchCleanupOutcome::Unconfirmed(LaunchError::Rollback(error)),
    }
}

async fn cleanup_committed_swarm_launch(
    processes: &ProcessSupervisor,
    receipt: &DetachedProcessReceipt,
    cause: LaunchError,
) -> LaunchCleanupOutcome {
    match processes.stop_detached(receipt).await {
        Ok(_) => match processes.forget_detached(receipt).await {
            Ok(()) => LaunchCleanupOutcome::TerminalConfirmed(cause),
            Err(cleanup) => LaunchCleanupOutcome::TerminalConfirmed(LaunchError::Cleanup {
                cause: Box::new(cause),
                cleanup,
                receipt: receipt.encode(),
            }),
        },
        Err(cleanup) => LaunchCleanupOutcome::Unconfirmed(LaunchError::Cleanup {
            cause: Box::new(cause),
            cleanup,
            receipt: receipt.encode(),
        }),
    }
}

impl RunOwner {
    pub fn installed(
        store: SwarmStore,
        working_directory: PathBuf,
        processes: ProcessSupervisor,
    ) -> Result<Self, RunnerError> {
        Self::new(store, CodexClient::installed(working_directory, processes), TURN_PERMIT_LIMIT)
    }

    pub fn new(
        store: SwarmStore,
        codex: CodexClient,
        permit_limit: usize,
    ) -> Result<Self, RunnerError> {
        let permits = TurnPermitPool::new(store.root(), permit_limit)?;
        Ok(Self { store, codex, permits })
    }

    pub async fn drive_detached(&self, id: &SwarmId) -> Result<SwarmProjection, RunnerError> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        loop {
            let projection = self.store.read_journal(id)?.projection;
            if projection.owner.is_some() {
                match self.store.start_journal(id) {
                    Ok(journal) => return self.drive_with_journal(id, journal).await,
                    Err(StoreError::WriterBusy(_)) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(RunnerError::Control(
                    "detached owner receipt was not published before startup deadline".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn drive(
        &self,
        id: &SwarmId,
        owner: SwarmOwner,
    ) -> Result<SwarmProjection, RunnerError> {
        let journal = self.store.start_journal(id)?;
        journal.append(SwarmEvent::RunStarted { owner }).await?;
        self.drive_with_journal(id, journal).await
    }

    async fn drive_with_journal(
        &self,
        id: &SwarmId,
        journal: super::store::JournalHandle,
    ) -> Result<SwarmProjection, RunnerError> {
        let spec = self.store.load_spec(id)?;
        if spec.working_directory != self.codex.working_directory() {
            return Err(RunnerError::Control(
                "run owner Codex working directory differs from immutable spec".to_owned(),
            ));
        }
        let sink = journal.sink();
        let (control_sender, control) = watch::channel(ControlState::Running);
        let store = self.store.clone();
        let control_id = id.clone();
        let control_task = tokio::spawn(async move {
            loop {
                match store.cancellation_requested(&control_id) {
                    Ok(true) => {
                        let _ = control_sender.send(ControlState::Cancelled);
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        let _ = control_sender.send(ControlState::Failed(error.to_string()));
                        return;
                    }
                }
                tokio::time::sleep(CONTROL_INTERVAL).await;
            }
        });

        let outcome = self.run_graph(&spec, &sink, control).await;
        control_task.abort();
        match outcome {
            Ok(result) => {
                sink.append(SwarmEvent::RunSucceeded { result }).await?;
            }
            Err(RunnerError::Cancelled) => {
                sink.append(SwarmEvent::CancellationAccepted {}).await?;
                sink.append(SwarmEvent::RunCancelled {}).await?;
                self.store.clear_cancellation(id)?;
            }
            Err(error) => {
                sink.append(SwarmEvent::RunFailed { error: error.to_string() }).await?;
            }
        }
        journal.shutdown().await?;
        let projection = self.store.read_journal(id)?.projection;
        self.store.write_result(&projection)?;
        report::write(&self.store, &projection)?;
        Ok(projection)
    }

    async fn run_graph(
        &self,
        spec: &super::model::SwarmSpec,
        journal: &JournalSink,
        control: watch::Receiver<ControlState>,
    ) -> Result<SynthesisOutput, RunnerError> {
        check_control(&control)?;
        journal.append(SwarmEvent::StageStarted { stage: Stage::Planning }).await?;
        let planner = AgentTurn {
            agent: AgentId::new("planner").expect("static agent id"),
            stage: Stage::Planning,
            prompt: prompts::planner(&spec.prompt),
            resume_thread_id: None,
        };
        let (_planning_cancel, planning_cancelled) = watch::channel(false);
        let planner_result = self
            .run_agent::<PlannerOutput>(spec, journal, planner, control.clone(), planning_cancelled)
            .await?;
        let mut roles = Vec::new();
        for (index, role) in planner_result.output.roles.iter().cloned().enumerate() {
            let agent = AgentId::new(format!("expert-{}", index + 1)).expect("numeric agent id");
            journal
                .append(SwarmEvent::RolePlanned { agent: agent.clone(), role: role.clone() })
                .await?;
            roles.push((agent, role));
        }
        journal.append(SwarmEvent::StageSucceeded { stage: Stage::Planning }).await?;

        check_control(&control)?;
        journal.append(SwarmEvent::StageStarted { stage: Stage::Experts }).await?;
        let expert_turns = roles
            .iter()
            .map(|(agent, role)| AgentTurn {
                agent: agent.clone(),
                stage: Stage::Experts,
                prompt: prompts::expert(&spec.prompt, agent.as_str(), role),
                resume_thread_id: None,
            })
            .collect();
        let expert_results =
            self.run_parallel::<ExpertOutput>(spec, journal, expert_turns, control.clone()).await?;
        journal.append(SwarmEvent::StageSucceeded { stage: Stage::Experts }).await?;

        let mut rebuttals = None;
        if spec.debate == DebatePolicy::Enabled {
            check_control(&control)?;
            journal.append(SwarmEvent::StageStarted { stage: Stage::Debate }).await?;
            let mut turns = Vec::new();
            for (index, (agent, role)) in roles.iter().enumerate() {
                let peers = roles
                    .iter()
                    .enumerate()
                    .filter(|(peer_index, _)| *peer_index != index)
                    .map(|(peer_index, (peer_agent, peer_role))| {
                        (
                            peer_agent.to_string(),
                            peer_role.clone(),
                            expert_results[peer_index].output.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                turns.push(AgentTurn {
                    agent: agent.clone(),
                    stage: Stage::Debate,
                    prompt: prompts::rebuttal(
                        &spec.prompt,
                        agent.as_str(),
                        role,
                        &expert_results[index].output,
                        &peers,
                    )?,
                    resume_thread_id: Some(expert_results[index].thread_id.clone()),
                });
            }
            let results =
                self.run_parallel::<RebuttalOutput>(spec, journal, turns, control.clone()).await?;
            journal.append(SwarmEvent::StageSucceeded { stage: Stage::Debate }).await?;
            rebuttals = Some(results);
        }

        let records = roles
            .iter()
            .enumerate()
            .map(|(index, (agent, role))| HardenedRecord {
                agent: agent.clone(),
                role: role.clone(),
                first_pass: expert_results[index].output.clone(),
                rebuttal: rebuttals.as_ref().map(|results| results[index].output.clone()),
            })
            .collect::<Vec<_>>();

        check_control(&control)?;
        journal.append(SwarmEvent::StageStarted { stage: Stage::Devil }).await?;
        let (_devil_cancel, devil_cancelled) = watch::channel(false);
        let devil = self
            .run_agent::<DevilOutput>(
                spec,
                journal,
                AgentTurn {
                    agent: AgentId::new("devil").expect("static agent id"),
                    stage: Stage::Devil,
                    prompt: prompts::devil(&spec.prompt, &records)?,
                    resume_thread_id: None,
                },
                control.clone(),
                devil_cancelled,
            )
            .await?;
        journal.append(SwarmEvent::StageSucceeded { stage: Stage::Devil }).await?;

        check_control(&control)?;
        journal.append(SwarmEvent::StageStarted { stage: Stage::Synthesis }).await?;
        let (_synthesis_cancel, synthesis_cancelled) = watch::channel(false);
        let synthesis = self
            .run_agent::<SynthesisOutput>(
                spec,
                journal,
                AgentTurn {
                    agent: AgentId::new("synthesis").expect("static agent id"),
                    stage: Stage::Synthesis,
                    prompt: prompts::synthesis(&spec.prompt, &records, &devil.output)?,
                    resume_thread_id: None,
                },
                control.clone(),
                synthesis_cancelled,
            )
            .await?;
        journal.append(SwarmEvent::StageSucceeded { stage: Stage::Synthesis }).await?;
        check_control(&control)?;
        Ok(synthesis.output)
    }

    async fn run_parallel<T: StructuredOutput>(
        &self,
        spec: &super::model::SwarmSpec,
        journal: &JournalSink,
        turns: Vec<AgentTurn>,
        control: watch::Receiver<ControlState>,
    ) -> Result<Vec<TurnResult<T>>, RunnerError> {
        let (stage_sender, _) = watch::channel(false);
        let mut futures = FuturesUnordered::new();
        let count = turns.len();
        for (index, turn) in turns.into_iter().enumerate() {
            let agent_control = control.clone();
            let agent_stage = stage_sender.subscribe();
            futures.push(async move {
                (index, self.run_agent::<T>(spec, journal, turn, agent_control, agent_stage).await)
            });
        }
        let mut ordered: Vec<Option<TurnResult<T>>> = vec![None; count];
        let mut failure = None;
        while let Some((index, result)) = futures.next().await {
            match result {
                Ok(result) => ordered[index] = Some(result),
                Err(RunnerError::StageCancelled) => {}
                Err(RunnerError::Cancelled) => {
                    if failure.is_none() {
                        failure = Some(RunnerError::Cancelled);
                        let _ = stage_sender.send(true);
                    }
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                        let _ = stage_sender.send(true);
                    }
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(ordered
            .into_iter()
            .map(|result| result.expect("all parallel agents succeeded"))
            .collect())
    }

    async fn run_agent<T: StructuredOutput>(
        &self,
        spec: &super::model::SwarmSpec,
        journal: &JournalSink,
        turn: AgentTurn,
        mut control: watch::Receiver<ControlState>,
        mut stage_cancelled: watch::Receiver<bool>,
    ) -> Result<TurnResult<T>, RunnerError> {
        journal
            .append(SwarmEvent::AgentPrompted {
                agent: turn.agent.clone(),
                stage: turn.stage,
                prompt: turn.prompt.clone(),
            })
            .await?;
        let max_attempts = spec.retry_limit.saturating_add(1);
        let mut last_error = String::new();
        for attempt in 1..=max_attempts {
            if attempt > 1 {
                journal
                    .append(SwarmEvent::AgentWaiting {
                        agent: turn.agent.clone(),
                        stage: turn.stage,
                        reason: WaitReason::RetryBackoff,
                    })
                    .await?;
                wait_or_cancel(RETRY_DELAY, &mut control, &mut stage_cancelled).await?;
            }
            journal
                .append(SwarmEvent::AgentWaiting {
                    agent: turn.agent.clone(),
                    stage: turn.stage,
                    reason: WaitReason::TurnPermit,
                })
                .await?;
            let _permit = self.acquire_permit(&mut control, &mut stage_cancelled).await?;
            journal
                .append(SwarmEvent::AgentStarted {
                    agent: turn.agent.clone(),
                    stage: turn.stage,
                    attempt,
                })
                .await?;
            match self
                .run_attempt::<T>(spec, journal, &turn, &mut control, &mut stage_cancelled)
                .await
            {
                Ok(result) => {
                    journal
                        .append(SwarmEvent::AgentSucceeded {
                            agent: turn.agent.clone(),
                            output: result.output.agent_output(),
                            usage: result.usage.clone(),
                        })
                        .await?;
                    return Ok(result);
                }
                Err(RunnerError::Cancelled) => return Err(RunnerError::Cancelled),
                Err(RunnerError::StageCancelled) => return Err(RunnerError::StageCancelled),
                Err(error) => {
                    last_error = error.to_string();
                    journal
                        .append(SwarmEvent::AgentFailed {
                            agent: turn.agent.clone(),
                            attempt,
                            error: last_error.clone(),
                        })
                        .await?;
                }
            }
        }
        Err(RunnerError::AgentExhausted { agent: turn.agent, message: last_error })
    }

    async fn run_attempt<T: StructuredOutput>(
        &self,
        spec: &super::model::SwarmSpec,
        journal: &JournalSink,
        turn: &AgentTurn,
        control: &mut watch::Receiver<ControlState>,
        stage_cancelled: &mut watch::Receiver<bool>,
    ) -> Result<TurnResult<T>, RunnerError> {
        let request = TurnRequest {
            prompt: turn.prompt.clone(),
            model: spec.model.clone(),
            reasoning: spec.reasoning,
        };
        let mut codex = match turn.resume_thread_id.as_deref() {
            Some(thread_id) => self.codex.resume::<T>(thread_id, request).await?,
            None => self.codex.start::<T>(request).await?,
        };
        loop {
            tokio::select! {
                changed = control.changed() => {
                    let cause = match changed {
                        Ok(()) => check_control(control).err(),
                        Err(_) => Some(RunnerError::Control("control monitor stopped".to_owned())),
                    };
                    if let Some(cause) = cause {
                        return Err(finalize_codex_turn(codex, cause).await);
                    }
                }
                changed = stage_cancelled.changed() => {
                    if changed.is_err() || *stage_cancelled.borrow() {
                        return Err(finalize_codex_turn(codex, RunnerError::StageCancelled).await);
                    }
                }
                event = codex.next_event() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(error) => {
                            return Err(
                                finalize_codex_turn(codex, RunnerError::from(error)).await
                            );
                        }
                    };
                    let Some(event) = event else { break };
                    let recorded = match event {
                        TransportEvent::ThreadStarted { thread_id } => {
                            journal.append(SwarmEvent::ThreadStarted {
                                agent: turn.agent.clone(),
                                thread_id,
                            }).await.map(|_| ()).map_err(RunnerError::Store)
                        }
                        TransportEvent::TurnStarted => Ok(()),
                        TransportEvent::Item { lifecycle, item } => {
                            journal.append(SwarmEvent::Item {
                                agent: turn.agent.clone(),
                                lifecycle,
                                item,
                            }).await.map(|_| ()).map_err(RunnerError::Store)
                        }
                        TransportEvent::TurnCompleted { .. } => Ok(()),
                        TransportEvent::TurnFailed { message }
                        | TransportEvent::Error { message } => {
                            Err(RunnerError::from(CodexError::TurnFailed(message)))
                        }
                    };
                    if let Err(cause) = recorded {
                        return Err(finalize_codex_turn(codex, cause).await);
                    }
                }
            }
        }
        codex.finish().await.map_err(RunnerError::from)
    }

    async fn acquire_permit(
        &self,
        control: &mut watch::Receiver<ControlState>,
        stage_cancelled: &mut watch::Receiver<bool>,
    ) -> Result<TurnPermit, RunnerError> {
        loop {
            check_control(control)?;
            if *stage_cancelled.borrow() {
                return Err(RunnerError::StageCancelled);
            }
            if let Some(permit) = self.permits.try_acquire()? {
                return Ok(permit);
            }
            wait_or_cancel(CONTROL_INTERVAL, control, stage_cancelled).await?;
        }
    }
}

async fn finalize_codex_turn<T: StructuredOutput>(
    codex: super::codex::CodexTurn<T>,
    cause: RunnerError,
) -> RunnerError {
    match codex.terminate().await {
        Ok(()) => cause,
        Err(cleanup) => {
            RunnerError::Finalize { cause: Box::new(cause), cleanup: Box::new(cleanup) }
        }
    }
}

fn check_control(control: &watch::Receiver<ControlState>) -> Result<(), RunnerError> {
    match &*control.borrow() {
        ControlState::Running => Ok(()),
        ControlState::Cancelled => Err(RunnerError::Cancelled),
        ControlState::Failed(message) => Err(RunnerError::Control(message.clone())),
    }
}

async fn wait_or_cancel(
    duration: Duration,
    control: &mut watch::Receiver<ControlState>,
    stage_cancelled: &mut watch::Receiver<bool>,
) -> Result<(), RunnerError> {
    tokio::select! {
        _ = tokio::time::sleep(duration) => {}
        changed = control.changed() => {
            changed.map_err(|_| RunnerError::Control("control monitor stopped".to_owned()))?;
        }
        changed = stage_cancelled.changed() => {
            if changed.is_err() || *stage_cancelled.borrow() {
                return Err(RunnerError::StageCancelled);
            }
        }
    }
    check_control(control)?;
    if *stage_cancelled.borrow() {
        return Err(RunnerError::StageCancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supervisor(directory: &std::path::Path) -> ProcessSupervisor {
        ProcessSupervisor::for_test(directory.join("processes")).unwrap()
    }
    use crate::tools::swarm::{
        model::{NodeStatus, RunStatus, SwarmEventRecord},
        store::NewSwarmSpec,
    };

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kit-swarm-runner-{name}-{}", std::process::id()))
    }

    fn write_fake(directory: &std::path::Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let executable = directory.join("fake-codex");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
dir=$(dirname "$0")
prompt=$(cat)
stage=$(printf '%s\n' "$prompt" | sed -n 's/^STAGE: //p' | head -n 1)
agent=$(printf '%s\n' "$prompt" | sed -n 's/^AGENT_ID: //p' | head -n 1)
resume=""
previous=""
for argument in "$@"; do
  if [ "$previous" = "resume" ]; then resume="$argument"; fi
  previous="$argument"
done
thread=${resume:-thread-$agent}
while ! mkdir "$dir/log.lock" 2>/dev/null; do sleep 0.01; done
printf '%s|%s|%s\n' "$stage" "$agent" "$resume" >> "$dir/calls.log"
printf '%s' "$prompt" > "$dir/prompt-$stage-$agent-$$"
count_file="$dir/count-$stage-$agent"
count=$(cat "$count_file" 2>/dev/null || printf '0')
count=$((count + 1))
printf '%s' "$count" > "$count_file"
rmdir "$dir/log.lock"
printf '{"type":"thread.started","thread_id":"%s"}\n' "$thread"
fail_until=$(cat "$dir/fail-$stage-$agent" 2>/dev/null || printf '0')
if [ "$count" -le "$fail_until" ]; then
  printf '{"type":"turn.failed","error":{"message":"scripted failure"}}\n'
  exit 0
fi
case "$stage" in
  planning)
    output='{"roles":[{"title":"Systems","mandate":"Own architecture","perspective":"System integrity"},{"title":"Operations","mandate":"Own runtime","perspective":"Operational safety"},{"title":"Product","mandate":"Own usability","perspective":"User value"}]}'
    ;;
  experts)
    output="{\"analysis\":\"analysis $agent\",\"findings\":[\"finding $agent\"],\"recommendation\":\"recommendation $agent\"}"
    if [ -f "$dir/slow" ]; then sleep 2; else sleep 0.05; fi
    ;;
  debate)
    output="{\"revised_analysis\":\"revised $agent\",\"accepted_challenges\":[\"accepted\"],\"rejected_challenges\":[\"rejected\"],\"recommendation\":\"hardened $agent\"}"
    sleep 0.05
    ;;
  devil)
    output='{"strongest_objections":["objection"],"failure_modes":["failure"],"required_corrections":["correction"]}'
    ;;
  synthesis)
    output='{"answer":"final answer","consensus":["consensus"],"dissent":["dissent"],"confidence":"high"}'
    ;;
  *) exit 9 ;;
esac
escaped=$(printf '%s' "$output" | sed 's/\\/\\\\/g; s/"/\\"/g')
printf '{"type":"turn.started"}\n'
printf '{"type":"item.started","item":{"id":"reason-%s","type":"reasoning","text":"working"}}\n' "$agent"
printf '{"type":"item.completed","item":{"id":"message-%s","type":"agent_message","text":"%s"}}\n' "$agent" "$escaped"
printf '{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":4,"reasoning_output_tokens":1}}\n'
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        executable
    }

    fn write_executable(path: &std::path::Path, script: &str) {
        std::fs::write(path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[tokio::test]
    async fn detached_launcher_requires_exact_identity_and_terminalizes_failed_startup() {
        let directory = root("launcher");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let store = SwarmStore::at(directory.join("state")).unwrap();
        let processes = supervisor(&directory);
        let create_spec = || NewSwarmSpec {
            prompt: "Launch deterministically".to_owned(),
            working_directory: directory.clone(),
            model: None,
            reasoning: super::super::model::ReasoningEffort::Low,
            debate: DebatePolicy::Disabled,
            retry_limit: 0,
        };

        let healthy = store.create(create_spec()).unwrap();
        let healthy_executable = directory.join("healthy-owner");
        write_executable(
            &healthy_executable,
            &format!(
                r#"#!/bin/sh
id="$3"
events="{}/runs/$id/events.jsonl"
while [ ! -s "$events" ]; do sleep 0.01; done
printf '{{"sequence":2,"at_ms":2,"event":{{"type":"stage_started","stage":"planning"}}}}\n' >> "$events"
sleep 2
"#,
                store.root().display()
            ),
        );
        let process_run_id =
            SwarmLauncher::new(store.clone(), healthy_executable, processes.clone())
                .launch(&healthy.id)
                .await
                .unwrap();
        let owner = store.read_journal(&healthy.id).unwrap().projection.owner.unwrap();
        assert_eq!(owner.process_run_id(), process_run_id);
        let receipt = owner.receipt().unwrap();
        processes.stop_detached(&receipt).await.unwrap();
        processes.forget_detached(&receipt).await.unwrap();

        let exited = store.create(create_spec()).unwrap();
        let exited_executable = directory.join("exited-owner");
        write_executable(&exited_executable, "#!/bin/sh\nexit 3\n");
        assert!(matches!(
            SwarmLauncher::new(store.clone(), exited_executable, processes.clone())
                .launch(&exited.id)
                .await,
            Err(LaunchError::Exited { .. })
        ));
        assert_eq!(store.read_journal(&exited.id).unwrap().projection.status, RunStatus::Failed);
        assert!(store.valid_result(&exited.id).unwrap().is_some());
        store.delete(&exited.id).unwrap();
        assert!(!store.run_dir(&exited.id).exists());

        let timed_out = store.create(create_spec()).unwrap();
        let timeout_executable = directory.join("timeout-owner");
        write_executable(&timeout_executable, "#!/bin/sh\nsleep 5\n");
        assert!(matches!(
            SwarmLauncher::with_timeout(
                store.clone(),
                timeout_executable,
                processes,
                Duration::from_millis(100)
            )
            .launch(&timed_out.id)
            .await,
            Err(LaunchError::Timeout { .. })
        ));
        assert_eq!(store.read_journal(&timed_out.id).unwrap().projection.status, RunStatus::Failed);
        assert!(store.valid_result(&timed_out.id).unwrap().is_some());
        store.delete(&timed_out.id).unwrap();
        assert!(!store.run_dir(&timed_out.id).exists());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn full_fake_graph_preserves_isolation_barriers_and_exact_resumes() {
        let directory = root("full-graph");
        let _ = std::fs::remove_dir_all(&directory);
        let executable = write_fake(&directory);
        let store = SwarmStore::at(directory.join("state")).unwrap();
        let spec = store
            .create(NewSwarmSpec {
                prompt: "Choose the strongest design".to_owned(),
                working_directory: directory.clone(),
                model: Some("fake-model".to_owned()),
                reasoning: super::super::model::ReasoningEffort::Low,
                debate: DebatePolicy::Enabled,
                retry_limit: 0,
            })
            .unwrap();
        let client = CodexClient::new(executable, directory.clone(), supervisor(&directory));
        let runner = RunOwner::new(store.clone(), client, 8).unwrap();
        let owner = SwarmOwner::fixture();
        let projection = runner.drive(&spec.id, owner).await.unwrap();

        assert_eq!(projection.status, RunStatus::Succeeded);
        assert_eq!(projection.result.as_ref().unwrap().answer, "final answer");
        assert_eq!(projection.completed_stages.len(), 5);
        assert_eq!(projection.nodes.len(), 6);
        for index in 1..=3 {
            let id = AgentId::new(format!("expert-{index}")).unwrap();
            let node = projection.nodes.iter().find(|node| node.agent == id).unwrap();
            assert_eq!(node.status, NodeStatus::Succeeded);
            assert_eq!(node.outputs.len(), 2);
            assert_eq!(node.threads.len(), 2);
            assert_eq!(node.timings.len(), 2);
            assert!(node
                .timings
                .iter()
                .all(|timing| timing.last_event_at_ms >= timing.started_at_ms));
            assert_eq!(node.usage.reasoning_output_tokens, 2);
            assert_eq!(node.threads[0].thread_id, format!("thread-expert-{index}"));
            assert_eq!(node.threads[1].thread_id, node.threads[0].thread_id);
            let first_prompt =
                &node.prompts.iter().find(|prompt| prompt.stage == Stage::Experts).unwrap().prompt;
            assert!(!first_prompt.contains("PEER_RECORDS"));
            let debate_prompt =
                &node.prompts.iter().find(|prompt| prompt.stage == Stage::Debate).unwrap().prompt;
            assert!(debate_prompt.contains("PEER_RECORDS"));
        }

        let journal = store.read_journal(&spec.id).unwrap();
        let expert_starts: Vec<&SwarmEventRecord> = journal
            .records
            .iter()
            .filter(|record| {
                matches!(&record.event, SwarmEvent::AgentStarted { stage: Stage::Experts, .. })
            })
            .collect();
        let first_expert_success = journal
            .records
            .iter()
            .find(|record| {
                matches!(
                    &record.event,
                    SwarmEvent::AgentSucceeded { ref agent, .. }
                        if agent.as_str().starts_with("expert-")
                )
            })
            .unwrap();
        assert_eq!(expert_starts.len(), 3);
        assert!(expert_starts.iter().all(|record| record.sequence < first_expert_success.sequence));
        assert_eq!(
            store.valid_result(&spec.id).unwrap().unwrap().terminal_sequence,
            journal.projection.last_sequence
        );
        let report = std::fs::read_to_string(store.run_dir(&spec.id).join("report.md")).unwrap();
        assert!(report.contains("## Answer\n\nfinal answer"));

        let calls = std::fs::read_to_string(directory.join("calls.log")).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        assert_eq!(calls.len(), 9);
        for index in 1..=3 {
            let expected = format!("debate|expert-{index}|thread-expert-{index}");
            assert!(calls.iter().any(|call| *call == expected));
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn retries_are_bounded_and_successful_retry_continues_graph() {
        let directory = root("retry");
        let _ = std::fs::remove_dir_all(&directory);
        let executable = write_fake(&directory);
        std::fs::write(directory.join("fail-experts-expert-1"), b"1").unwrap();
        let store = SwarmStore::at(directory.join("state")).unwrap();
        let spec = store
            .create(NewSwarmSpec {
                prompt: "Retry deterministically".to_owned(),
                working_directory: directory.clone(),
                model: None,
                reasoning: super::super::model::ReasoningEffort::Low,
                debate: DebatePolicy::Disabled,
                retry_limit: 1,
            })
            .unwrap();
        let runner = RunOwner::new(
            store.clone(),
            CodexClient::new(executable, directory.clone(), supervisor(&directory)),
            8,
        )
        .unwrap();
        let projection = runner.drive(&spec.id, SwarmOwner::fixture()).await.unwrap();
        assert_eq!(projection.status, RunStatus::Succeeded);
        let expert =
            projection.nodes.iter().find(|node| node.agent.as_str() == "expert-1").unwrap();
        assert_eq!(expert.attempt, 2);
        assert_eq!(
            expert.threads.iter().filter(|thread| thread.stage == Stage::Experts).count(),
            2
        );
        let failures = store
            .read_journal(&spec.id)
            .unwrap()
            .records
            .into_iter()
            .filter(|record| {
                matches!(
                    record.event,
                    SwarmEvent::AgentFailed { ref agent, attempt: 1, .. }
                        if agent.as_str() == "expert-1"
                )
            })
            .count();
        assert_eq!(failures, 1);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn exhausted_mandatory_stage_fails_without_downstream_synthesis() {
        let directory = root("exhausted");
        let _ = std::fs::remove_dir_all(&directory);
        let executable = write_fake(&directory);
        std::fs::write(directory.join("fail-devil-devil"), b"9").unwrap();
        let store = SwarmStore::at(directory.join("state")).unwrap();
        let spec = store
            .create(NewSwarmSpec {
                prompt: "Fail deterministically".to_owned(),
                working_directory: directory.clone(),
                model: None,
                reasoning: super::super::model::ReasoningEffort::Low,
                debate: DebatePolicy::Disabled,
                retry_limit: 1,
            })
            .unwrap();
        let runner = RunOwner::new(
            store.clone(),
            CodexClient::new(executable, directory.clone(), supervisor(&directory)),
            8,
        )
        .unwrap();
        let projection = runner.drive(&spec.id, SwarmOwner::fixture()).await.unwrap();
        assert_eq!(projection.status, RunStatus::Failed);
        let journal = store.read_journal(&spec.id).unwrap();
        assert_eq!(
            journal
                .records
                .iter()
                .filter(|record| matches!(
                    &record.event,
                    SwarmEvent::AgentFailed { agent, .. } if agent.as_str() == "devil"
                ))
                .count(),
            2
        );
        assert!(!journal.records.iter().any(|record| matches!(
            &record.event,
            SwarmEvent::StageStarted { stage: Stage::Synthesis }
        )));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn independent_cancellation_stops_parallel_children_before_terminal_event() {
        let directory = root("cancel");
        let _ = std::fs::remove_dir_all(&directory);
        let executable = write_fake(&directory);
        std::fs::write(directory.join("slow"), b"").unwrap();
        let store = SwarmStore::at(directory.join("state")).unwrap();
        let spec = store
            .create(NewSwarmSpec {
                prompt: "Cancel deterministically".to_owned(),
                working_directory: directory.clone(),
                model: None,
                reasoning: super::super::model::ReasoningEffort::Low,
                debate: DebatePolicy::Enabled,
                retry_limit: 0,
            })
            .unwrap();
        let runner = RunOwner::new(
            store.clone(),
            CodexClient::new(executable, directory.clone(), supervisor(&directory)),
            8,
        )
        .unwrap();
        let run_id = spec.id.clone();
        let task = tokio::spawn(async move { runner.drive(&run_id, SwarmOwner::fixture()).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let started = store
                .read_journal(&spec.id)
                .map(|journal| {
                    journal
                        .records
                        .iter()
                        .filter(|record| {
                            matches!(
                                &record.event,
                                SwarmEvent::AgentStarted { stage: Stage::Experts, .. }
                            )
                        })
                        .count()
                })
                .unwrap_or(0);
            if started == 3 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "expert stage did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        store.request_cancellation(&spec.id).unwrap();
        let projection = task.await.unwrap().unwrap();
        assert_eq!(projection.status, RunStatus::Cancelled);
        let journal = store.read_journal(&spec.id).unwrap();
        let accepted = journal
            .records
            .iter()
            .position(|record| matches!(&record.event, SwarmEvent::CancellationAccepted {}))
            .unwrap();
        let terminal = journal
            .records
            .iter()
            .position(|record| matches!(&record.event, SwarmEvent::RunCancelled {}))
            .unwrap();
        assert!(accepted < terminal);
        let _ = std::fs::remove_dir_all(directory);
    }
}
