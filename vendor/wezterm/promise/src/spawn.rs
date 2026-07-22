use anyhow::{anyhow, Result};
use async_executor::Executor;
use async_task::FallibleTask;
use flume::{bounded, Receiver, TryRecvError};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use thiserror::Error;
use wezterm_runtime_admission::{AdmissionError, CountClass, CountPermit, RuntimeAdmission};

pub use async_task::{Runnable, Task};
pub type SpawnFunc = Box<dyn FnOnce() + Send>;
pub type ScheduleFunc = Box<dyn Fn(Runnable) + Send + Sync + 'static>;

fn no_scheduler_configured(_: Runnable) {
    panic!("no scheduler has been configured");
}

lazy_static::lazy_static! {
    static ref ON_MAIN_THREAD: Mutex<ScheduleFunc> = Mutex::new(Box::new(no_scheduler_configured));
    static ref ON_MAIN_THREAD_LOW_PRI: Mutex<ScheduleFunc> = Mutex::new(Box::new(no_scheduler_configured));
    static ref SCOPED_EXECUTOR: Mutex<Option<Arc<Executor<'static>>>> = Mutex::new(None);
}

static SCHEDULER_CONFIGURED: AtomicBool = AtomicBool::new(false);

fn schedule_runnable(runnable: Runnable, high_pri: bool) {
    let func = if high_pri {
        ON_MAIN_THREAD.lock()
    } else {
        ON_MAIN_THREAD_LOW_PRI.lock()
    }
    .unwrap();
    func(runnable);
}

pub fn is_scheduler_configured() -> bool {
    SCHEDULER_CONFIGURED.load(Ordering::Relaxed)
}

/// Set callbacks for scheduling normal and low priority futures.
/// Why this and not "just tokio"?  In a GUI application there is typically
/// a special GUI processing loop that may need to run on the "main thread",
/// so we can't just run a tokio/mio loop in that context.
/// This particular crate has no real knowledge of how that plumbing works,
/// it just provides the abstraction for scheduling the work.
/// This function allows the embedding application to set that up.
pub fn set_schedulers(main: ScheduleFunc, low_pri: ScheduleFunc) {
    *ON_MAIN_THREAD.lock().unwrap() = Box::new(main);
    *ON_MAIN_THREAD_LOW_PRI.lock().unwrap() = Box::new(low_pri);
    SCHEDULER_CONFIGURED.store(true, Ordering::Relaxed);
}

/// Spawn a new thread to execute the provided function.
/// Returns a JoinHandle that implements the Future trait
/// and that can be used to await and yield the return value
/// from the thread.
/// Can be called from any thread.
pub fn spawn_into_new_thread<F, T>(f: F) -> Task<Result<T>>
where
    F: FnOnce() -> Result<T>,
    F: Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = bounded(1);

    // Holds the waker that may later observe
    // during the Future::poll call.
    struct WakerHolder {
        waker: Mutex<Option<Waker>>,
    }

    let holder = Arc::new(WakerHolder {
        waker: Mutex::new(None),
    });

    let thread_waker = Arc::clone(&holder);
    std::thread::spawn(move || {
        // Run the thread
        let res = f();
        // Pass the result back
        tx.send(res).unwrap();
        // If someone polled the thread before we got here,
        // they will have populated the waker; extract it
        // and wake up the scheduler so that it will poll
        // the result again.
        let mut waker = thread_waker.waker.lock().unwrap();
        if let Some(waker) = waker.take() {
            waker.wake();
        }
    });

    struct PendingResult<T> {
        rx: Receiver<Result<T>>,
        holder: Arc<WakerHolder>,
    }

    impl<T> std::future::Future for PendingResult<T> {
        type Output = Result<T>;

        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
            match self.rx.try_recv() {
                Ok(res) => Poll::Ready(res),
                Err(TryRecvError::Empty) => {
                    let mut waker = self.holder.waker.lock().unwrap();
                    waker.replace(cx.waker().clone());
                    Poll::Pending
                }
                Err(TryRecvError::Disconnected) => {
                    Poll::Ready(Err(anyhow!("thread terminated without providing a result")))
                }
            }
        }
    }

    spawn_into_main_thread(PendingResult { rx, holder })
}

fn get_scoped() -> Option<Arc<Executor<'static>>> {
    SCOPED_EXECUTOR.lock().unwrap().as_ref().map(Arc::clone)
}

/// Spawn a future into the main thread; it will be polled in the
/// main thread.
/// This function can be called from any thread.
/// If you are on the main thread already, consider using
/// spawn() instead to lift the `Send` requirement.
pub fn spawn_into_main_thread<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    if let Some(executor) = get_scoped() {
        return executor.spawn(future);
    }
    let (runnable, task) = async_task::spawn(future, |runnable| schedule_runnable(runnable, true));
    runnable.schedule();
    task
}

/// Spawn a future into the main thread; it will be polled in
/// the main thread in the low priority queue--all other normal
/// priority items will be drained before considering low priority
/// spawns.
/// If you are on the main thread already, consider using `spawn_with_low_priority`
/// instead to lift the `Send` requirement.
pub fn spawn_into_main_thread_with_low_priority<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    if let Some(executor) = get_scoped() {
        return executor.spawn(future);
    }
    let (runnable, task) = async_task::spawn(future, |runnable| schedule_runnable(runnable, false));
    runnable.schedule();
    task
}

/// Spawn a future with normal priority.
pub fn spawn<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let (runnable, task) =
        async_task::spawn_local(future, |runnable| schedule_runnable(runnable, true));
    runnable.schedule();
    task
}

/// Spawn a future with low priority; it will be polled only after
/// all other normal priority items are processed.
pub fn spawn_with_low_priority<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let (runnable, task) =
        async_task::spawn_local(future, |runnable| schedule_runnable(runnable, false));
    runnable.schedule();
    task
}

/// Block the current thread until the passed future completes.
pub use async_io::block_on;

enum ExecutorItem {
    Sync {
        func: SpawnFunc,
        _permit: CountPermit,
    },
    Runnable {
        runnable: Runnable,
        // Legacy promise::spawn entry points retain a permit for each
        // reachable runnable. New headless owners use try_spawn[_local],
        // which retains its permit in the future for the whole task lifetime.
        _legacy_permit: Option<CountPermit>,
    },
}

impl ExecutorItem {
    fn run(self) {
        match self {
            Self::Sync { func, .. } => func(),
            Self::Runnable { runnable, .. } => {
                runnable.run();
            }
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ScheduleError {
    #[error("executor admission refused: {0}")]
    Admission(AdmissionError),
    #[error("main-thread scheduler is not configured")]
    MainThreadSchedulerNotConfigured,
    #[error("executor is shutting down")]
    ShuttingDown,
    #[error("executor runnable queue is full")]
    Full,
    #[error("executor runnable queue is disconnected")]
    Disconnected,
}

impl From<AdmissionError> for ScheduleError {
    fn from(error: AdmissionError) -> Self {
        match error {
            AdmissionError::ShuttingDown => Self::ShuttingDown,
            other => Self::Admission(other),
        }
    }
}

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("executor scheduling failed: {0}")]
    Scheduling(#[from] ScheduleError),
    #[error("executor runnable queue is disconnected")]
    Disconnected,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TaskJoinError {
    #[error("task was cancelled")]
    Cancelled,
    #[error("task scheduling failed: {0}")]
    Scheduling(ScheduleError),
    #[error("task runnable was lost without a scheduling error")]
    RunnableLost,
}

impl TaskJoinError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

#[derive(Default)]
struct AdmittedTaskState {
    cancel_requested: AtomicBool,
    task_waker: Mutex<Option<Waker>>,
    scheduling_error: Mutex<Option<ScheduleError>>,
}

impl AdmittedTaskState {
    fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::Release);
        let waker = self.task_waker.lock().unwrap().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn register_task_waker(&self, waker: &Waker) {
        let mut slot = self.task_waker.lock().unwrap();
        if !slot
            .as_ref()
            .is_some_and(|registered| registered.will_wake(waker))
        {
            *slot = Some(waker.clone());
        }
    }

    fn clear_task_waker(&self) {
        let waker = self.task_waker.lock().unwrap().take();
        drop(waker);
    }

    fn record_scheduling_error(&self, error: ScheduleError) {
        let mut slot = self.scheduling_error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(error);
        }
    }

    fn scheduling_error(&self) -> Option<ScheduleError> {
        self.scheduling_error.lock().unwrap().clone()
    }
}

enum TaskOutcome<R> {
    Completed(R),
    Cancelled,
}

struct AdmittedFuture<F> {
    future: Pin<Box<F>>,
    state: Arc<AdmittedTaskState>,
    _permit: CountPermit,
}

impl<F: Future> Future for AdmittedFuture<F> {
    type Output = TaskOutcome<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.state.cancel_requested.load(Ordering::Acquire) {
            this.state.clear_task_waker();
            return Poll::Ready(TaskOutcome::Cancelled);
        }

        this.state.register_task_waker(cx.waker());
        if this.state.cancel_requested.load(Ordering::Acquire) {
            this.state.clear_task_waker();
            return Poll::Ready(TaskOutcome::Cancelled);
        }

        match this.future.as_mut().poll(cx) {
            Poll::Ready(output) => {
                this.state.clear_task_waker();
                Poll::Ready(TaskOutcome::Completed(output))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<F> Drop for AdmittedFuture<F> {
    fn drop(&mut self) {
        self.state.clear_task_waker();
    }
}

/// A cancellable, joinable executor task holding one runnable permit until its
/// future is completed, cancelled, or destroyed after a scheduling failure.
#[must_use = "dropping a task cancels it; retain and join it for lifecycle proof"]
pub struct AdmittedTask<R> {
    task: FallibleTask<TaskOutcome<R>>,
    state: Arc<AdmittedTaskState>,
}

impl<R> AdmittedTask<R> {
    /// Requests cancellation synchronously. The mux thread must continue
    /// ticking until this task is finished and joined.
    pub fn cancel(&self) {
        self.state.request_cancel();
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn cancel_and_join(mut self) -> Result<Option<R>, TaskJoinError> {
        self.cancel();
        match (&mut self).await {
            Ok(output) => Ok(Some(output)),
            Err(TaskJoinError::Cancelled) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl<R> Future for AdmittedTask<R> {
    type Output = Result<R, TaskJoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(error) = this.state.scheduling_error() {
            return Poll::Ready(Err(TaskJoinError::Scheduling(error)));
        }

        match Pin::new(&mut this.task).poll(cx) {
            Poll::Ready(Some(TaskOutcome::Completed(output))) => Poll::Ready(Ok(output)),
            Poll::Ready(Some(TaskOutcome::Cancelled)) => Poll::Ready(Err(TaskJoinError::Cancelled)),
            Poll::Ready(None) => Poll::Ready(Err(match this.state.scheduling_error() {
                Some(error) => TaskJoinError::Scheduling(error),
                None => TaskJoinError::RunnableLost,
            })),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone)]
pub struct SimpleExecutorHandle {
    tx: flume::Sender<ExecutorItem>,
    admission: Arc<RuntimeAdmission>,
}

impl SimpleExecutorHandle {
    fn admission(&self) -> &Arc<RuntimeAdmission> {
        &self.admission
    }

    pub fn try_schedule(&self, func: SpawnFunc) -> Result<(), ScheduleError> {
        let permit = self
            .admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .map_err(ScheduleError::from)?;
        let item = ExecutorItem::Sync {
            func,
            _permit: permit,
        };
        self.try_enqueue(item).map_err(|(error, _item)| error)
    }

    pub fn try_spawn<F, R>(&self, future: F) -> Result<AdmittedTask<R>, ScheduleError>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let permit = self
            .admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .map_err(ScheduleError::from)?;
        let state = Arc::new(AdmittedTaskState::default());
        let admitted = AdmittedFuture {
            future: Box::pin(future),
            state: Arc::clone(&state),
            _permit: permit,
        };
        let scheduler = self.clone();
        let failure_state = Arc::clone(&state);
        let (runnable, task) = async_task::spawn(admitted, move |runnable| {
            if let Err((error, runnable)) = scheduler.try_enqueue_runnable(runnable, None) {
                failure_state.record_scheduling_error(error);
                drop(runnable);
            }
        });
        self.try_enqueue_runnable(runnable, None)
            .map_err(|(error, _runnable)| error)?;
        Ok(AdmittedTask {
            task: task.fallible(),
            state,
        })
    }

    /// Creates a mux-thread-only spawning handle. The returned task handle is
    /// still Send when its output is Send; only creation and runnable execution
    /// are confined to the thread that owns the SimpleExecutor.
    pub fn local(&self) -> LocalSimpleExecutorHandle {
        LocalSimpleExecutorHandle {
            handle: self.clone(),
            _thread_bound: PhantomData,
        }
    }

    pub fn begin_shutdown(&self) {
        self.admission.begin_shutdown();
    }

    pub fn is_shutting_down(&self) -> bool {
        self.admission.is_shutting_down()
    }

    fn try_enqueue_runnable(
        &self,
        runnable: Runnable,
        legacy_permit: Option<CountPermit>,
    ) -> Result<(), (ScheduleError, Runnable)> {
        let item = ExecutorItem::Runnable {
            runnable,
            _legacy_permit: legacy_permit,
        };
        self.try_enqueue(item).map_err(|(error, item)| match item {
            ExecutorItem::Runnable { runnable, .. } => (error, runnable),
            ExecutorItem::Sync { .. } => unreachable!("queued a runnable item"),
        })
    }

    fn try_enqueue(&self, item: ExecutorItem) -> Result<(), (ScheduleError, ExecutorItem)> {
        if self.admission.is_shutting_down() {
            return Err((ScheduleError::ShuttingDown, item));
        }
        self.tx.try_send(item).map_err(|error| match error {
            flume::TrySendError::Full(item) => (ScheduleError::Full, item),
            flume::TrySendError::Disconnected(item) => (ScheduleError::Disconnected, item),
        })
    }

    fn try_schedule_legacy_runnable(
        &self,
        runnable: Runnable,
    ) -> Result<(), (ScheduleError, Runnable)> {
        let permit = match self
            .admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .map_err(ScheduleError::from)
        {
            Ok(permit) => permit,
            Err(error) => return Err((error, runnable)),
        };
        self.try_enqueue_runnable(runnable, Some(permit))
    }
}

#[derive(Clone)]
enum MainThreadExecutorBackend {
    Bounded(SimpleExecutorHandle),
    Global,
}

/// An admitted handle for work that must execute on the main thread.
///
/// Headless applications use the explicitly bounded `SimpleExecutor` backend,
/// while interactive applications use their already-configured global main-
/// thread scheduler. Both backends retain one `ExecutorRunnable` permit for the
/// complete lifetime of each task.
#[derive(Clone)]
pub struct MainThreadExecutorHandle {
    admission: Arc<RuntimeAdmission>,
    backend: MainThreadExecutorBackend,
}

impl MainThreadExecutorHandle {
    pub fn from_simple(executor: SimpleExecutorHandle) -> Self {
        Self {
            admission: Arc::clone(executor.admission()),
            backend: MainThreadExecutorBackend::Bounded(executor),
        }
    }

    pub fn from_global(admission: Arc<RuntimeAdmission>) -> Result<Self, ScheduleError> {
        if !is_scheduler_configured() {
            return Err(ScheduleError::MainThreadSchedulerNotConfigured);
        }
        Ok(Self {
            admission,
            backend: MainThreadExecutorBackend::Global,
        })
    }

    pub fn admission(&self) -> &Arc<RuntimeAdmission> {
        &self.admission
    }

    pub fn try_spawn<F, R>(&self, future: F) -> Result<AdmittedTask<R>, ScheduleError>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        match &self.backend {
            MainThreadExecutorBackend::Bounded(executor) => executor.try_spawn(future),
            MainThreadExecutorBackend::Global => self.try_spawn_global(future),
        }
    }

    pub fn local(&self) -> LocalMainThreadExecutorHandle {
        LocalMainThreadExecutorHandle {
            handle: self.clone(),
            _thread_bound: PhantomData,
        }
    }

    pub fn begin_shutdown(&self) {
        self.admission.begin_shutdown();
    }

    pub fn is_shutting_down(&self) -> bool {
        self.admission.is_shutting_down()
    }

    fn try_spawn_global<F, R>(&self, future: F) -> Result<AdmittedTask<R>, ScheduleError>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        self.ensure_global_scheduler()?;
        let permit = self
            .admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .map_err(ScheduleError::from)?;
        let state = Arc::new(AdmittedTaskState::default());
        let admitted = AdmittedFuture {
            future: Box::pin(future),
            state: Arc::clone(&state),
            _permit: permit,
        };
        let scheduler = self.clone();
        let failure_state = Arc::clone(&state);
        let (runnable, task) = async_task::spawn(admitted, move |runnable| {
            if let Err((error, runnable)) = scheduler.try_schedule_global(runnable) {
                failure_state.record_scheduling_error(error);
                drop(runnable);
            }
        });
        self.try_schedule_global(runnable)
            .map_err(|(error, _runnable)| error)?;
        Ok(AdmittedTask {
            task: task.fallible(),
            state,
        })
    }

    fn ensure_global_scheduler(&self) -> Result<(), ScheduleError> {
        if !is_scheduler_configured() {
            return Err(ScheduleError::MainThreadSchedulerNotConfigured);
        }
        if self.admission.is_shutting_down() {
            return Err(ScheduleError::ShuttingDown);
        }
        Ok(())
    }

    fn try_schedule_global(&self, runnable: Runnable) -> Result<(), (ScheduleError, Runnable)> {
        if let Err(error) = self.ensure_global_scheduler() {
            return Err((error, runnable));
        }
        schedule_runnable(runnable, true);
        Ok(())
    }
}

/// A main-thread-only view that can create admitted non-Send futures.
#[derive(Clone)]
pub struct LocalMainThreadExecutorHandle {
    handle: MainThreadExecutorHandle,
    _thread_bound: PhantomData<Rc<()>>,
}

impl LocalMainThreadExecutorHandle {
    pub fn try_spawn_local<F, R>(&self, future: F) -> Result<AdmittedTask<R>, ScheduleError>
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        match &self.handle.backend {
            MainThreadExecutorBackend::Bounded(executor) => {
                executor.local().try_spawn_local(future)
            }
            MainThreadExecutorBackend::Global => self.try_spawn_local_global(future),
        }
    }

    fn try_spawn_local_global<F, R>(&self, future: F) -> Result<AdmittedTask<R>, ScheduleError>
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        self.handle.ensure_global_scheduler()?;
        let permit = self
            .handle
            .admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .map_err(ScheduleError::from)?;
        let state = Arc::new(AdmittedTaskState::default());
        let admitted = AdmittedFuture {
            future: Box::pin(future),
            state: Arc::clone(&state),
            _permit: permit,
        };
        let scheduler = self.handle.clone();
        let failure_state = Arc::clone(&state);
        let (runnable, task) = async_task::spawn_local(admitted, move |runnable| {
            if let Err((error, runnable)) = scheduler.try_schedule_global(runnable) {
                failure_state.record_scheduling_error(error);
                drop(runnable);
            }
        });
        self.handle
            .try_schedule_global(runnable)
            .map_err(|(error, _runnable)| error)?;
        Ok(AdmittedTask {
            task: task.fallible(),
            state,
        })
    }
}

/// A handle that can create non-Send futures only on the mux executor thread.
#[derive(Clone)]
pub struct LocalSimpleExecutorHandle {
    handle: SimpleExecutorHandle,
    _thread_bound: PhantomData<Rc<()>>,
}

impl LocalSimpleExecutorHandle {
    pub fn try_spawn_local<F, R>(&self, future: F) -> Result<AdmittedTask<R>, ScheduleError>
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        let permit = self
            .handle
            .admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .map_err(ScheduleError::from)?;
        let state = Arc::new(AdmittedTaskState::default());
        let admitted = AdmittedFuture {
            future: Box::pin(future),
            state: Arc::clone(&state),
            _permit: permit,
        };
        let scheduler = self.handle.clone();
        let failure_state = Arc::clone(&state);
        let (runnable, task) = async_task::spawn_local(admitted, move |runnable| {
            if let Err((error, runnable)) = scheduler.try_enqueue_runnable(runnable, None) {
                failure_state.record_scheduling_error(error);
                drop(runnable);
            }
        });
        self.handle
            .try_enqueue_runnable(runnable, None)
            .map_err(|(error, _runnable)| error)?;
        Ok(AdmittedTask {
            task: task.fallible(),
            state,
        })
    }
}

enum ExecutorTick {
    Item(Result<ExecutorItem, flume::RecvError>),
    SchedulingFault(Result<ScheduleError, flume::RecvError>),
}

pub struct SimpleExecutor {
    rx: Receiver<ExecutorItem>,
    scheduling_fault_rx: Receiver<ScheduleError>,
    _scheduling_fault_guard: flume::Sender<ScheduleError>,
    handle: SimpleExecutorHandle,
}

impl SimpleExecutor {
    pub fn new(admission: Arc<RuntimeAdmission>) -> Self {
        let capacity = admission.count_capacity(CountClass::ExecutorRunnable);
        Self::with_queue_capacity(admission, capacity)
    }

    fn with_queue_capacity(admission: Arc<RuntimeAdmission>, capacity: usize) -> Self {
        let (tx, rx) = bounded(capacity);
        let (scheduling_fault_tx, scheduling_fault_rx) = bounded(1);
        let handle = SimpleExecutorHandle { tx, admission };

        let install_scheduler = |scheduler: SimpleExecutorHandle| {
            let fault_tx = scheduling_fault_tx.clone();
            Box::new(move |runnable| {
                if let Err((error, runnable)) = scheduler.try_schedule_legacy_runnable(runnable) {
                    // One fault is sufficient to terminate the executor. The
                    // failed runnable is cancelled only after its typed fault
                    // has become observable to tick().
                    let _ = fault_tx.try_send(error);
                    drop(runnable);
                }
            }) as ScheduleFunc
        };
        set_schedulers(
            install_scheduler(handle.clone()),
            install_scheduler(handle.clone()),
        );
        Self {
            rx,
            scheduling_fault_rx,
            _scheduling_fault_guard: scheduling_fault_tx,
            handle,
        }
    }

    pub fn handle(&self) -> SimpleExecutorHandle {
        self.handle.clone()
    }

    pub fn tick(&self) -> Result<(), ExecutorError> {
        match flume::Selector::new()
            .recv(&self.scheduling_fault_rx, ExecutorTick::SchedulingFault)
            .recv(&self.rx, ExecutorTick::Item)
            .wait()
        {
            ExecutorTick::SchedulingFault(Ok(error)) => Err(error.into()),
            ExecutorTick::SchedulingFault(Err(_)) => Err(ExecutorError::Disconnected),
            ExecutorTick::Item(Ok(item)) => {
                item.run();
                match self.scheduling_fault_rx.try_recv() {
                    Ok(error) => Err(error.into()),
                    Err(TryRecvError::Empty) => Ok(()),
                    Err(TryRecvError::Disconnected) => Err(ExecutorError::Disconnected),
                }
            }
            ExecutorTick::Item(Err(_)) => Err(ExecutorError::Disconnected),
        }
    }
}

#[cfg(test)]
mod simple_executor_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wezterm_runtime_admission::{RuntimeRole, CLIENT_REACHABLE_RUNNABLES};

    static EXECUTOR_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct ControlledFuture {
        state: Arc<ControlledState>,
    }

    #[derive(Default)]
    struct ControlledState {
        ready: AtomicBool,
        waker: Mutex<Option<Waker>>,
    }

    impl ControlledState {
        fn wake(&self) {
            self.ready.store(true, Ordering::Release);
            let waker = self.waker.lock().unwrap().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    impl Future for ControlledFuture {
        type Output = usize;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.state.ready.load(Ordering::Acquire) {
                Poll::Ready(42)
            } else {
                self.state.waker.lock().unwrap().replace(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn controlled_future() -> (ControlledFuture, Arc<ControlledState>) {
        let state = Arc::new(ControlledState::default());
        (
            ControlledFuture {
                state: Arc::clone(&state),
            },
            state,
        )
    }

    #[test]
    fn scheduled_work_runs_and_releases_its_permit() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let ran = Arc::new(AtomicUsize::new(0));
        let task_ran = Arc::clone(&ran);
        executor
            .handle()
            .try_schedule(Box::new(move || {
                task_ran.fetch_add(1, Ordering::Relaxed);
            }))
            .unwrap();

        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);
        executor.tick().unwrap();
        assert_eq!(ran.load(Ordering::Relaxed), 1);
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn full_queue_returns_a_typed_error_and_releases_refused_permit() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::with_queue_capacity(Arc::clone(&admission), 1);
        let handle = executor.handle();
        handle.try_schedule(Box::new(|| {})).unwrap();

        assert_eq!(
            handle.try_schedule(Box::new(|| {})),
            Err(ScheduleError::Full)
        );
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);
        executor.tick().unwrap();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn async_initial_full_is_typed_and_releases_its_task_permit() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::with_queue_capacity(Arc::clone(&admission), 0);

        assert!(matches!(
            executor.handle().try_spawn(async { 42 }),
            Err(ScheduleError::Full)
        ));
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn admission_boundary_is_role_correct_and_releases_after_drain() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let handle = executor.handle();
        for _ in 0..CLIENT_REACHABLE_RUNNABLES {
            handle.try_schedule(Box::new(|| {})).unwrap();
        }
        assert!(matches!(
            handle.try_schedule(Box::new(|| {})),
            Err(ScheduleError::Admission(
                AdmissionError::CapacityExceeded { .. }
            ))
        ));
        assert_eq!(
            admission.count_usage(CountClass::ExecutorRunnable),
            CLIENT_REACHABLE_RUNNABLES
        );
        for _ in 0..CLIENT_REACHABLE_RUNNABLES {
            executor.tick().unwrap();
        }
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn shutdown_returns_a_typed_error_without_queueing() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        executor.handle().begin_shutdown();

        assert_eq!(
            executor.handle().try_schedule(Box::new(|| {})),
            Err(ScheduleError::ShuttingDown)
        );
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn disconnected_queue_returns_a_typed_error_and_releases_permit() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let handle = executor.handle();
        drop(executor);

        assert_eq!(
            handle.try_schedule(Box::new(|| {})),
            Err(ScheduleError::Disconnected)
        );
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn replacing_legacy_schedulers_does_not_disconnect_explicit_executor() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let first_admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let first = SimpleExecutor::new(Arc::clone(&first_admission));
        first.handle().try_schedule(Box::new(|| {})).unwrap();

        let second_admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let _second = SimpleExecutor::new(second_admission);

        first.tick().unwrap();
        assert_eq!(first_admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn async_task_retains_permit_across_first_and_later_wakes() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let (future, state) = controlled_future();
        let task = executor.handle().try_spawn(future).unwrap();

        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);
        executor.tick().unwrap();
        assert!(!task.is_finished());
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);

        state.wake();
        executor.tick().unwrap();
        assert!(task.is_finished());
        assert_eq!(block_on(task).unwrap(), 42);
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn global_main_thread_backend_retains_its_task_permit_across_wakes() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let handle = MainThreadExecutorHandle::from_global(Arc::clone(&admission)).unwrap();
        let (future, state) = controlled_future();
        let task = handle.try_spawn(future).unwrap();

        // One permit belongs to the complete admitted task and one belongs to
        // the SimpleExecutor-backed global scheduler's queued runnable.
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 2);
        executor.tick().unwrap();
        assert!(!task.is_finished());
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);

        state.wake();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 2);
        executor.tick().unwrap();
        assert_eq!(block_on(task).unwrap(), 42);
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn global_main_thread_backend_supports_local_futures() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let handle = MainThreadExecutorHandle::from_global(Arc::clone(&admission)).unwrap();
        let value = Rc::new(42);
        let task_value = Rc::clone(&value);
        let task = handle
            .local()
            .try_spawn_local(async move { *task_value })
            .unwrap();

        executor.tick().unwrap();
        assert_eq!(block_on(task).unwrap(), 42);
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn cancellation_is_synchronous_joinable_and_releases_after_drain() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let (future, _state) = controlled_future();
        let task = executor.handle().local().try_spawn_local(future).unwrap();
        executor.tick().unwrap();

        task.cancel();
        executor.tick().unwrap();
        assert!(matches!(block_on(task), Err(TaskJoinError::Cancelled)));
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn dropping_a_task_cancels_and_releases_after_executor_drain() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let (future, _state) = controlled_future();
        let task = executor.handle().try_spawn(future).unwrap();
        executor.tick().unwrap();
        drop(task);

        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);
        executor.tick().unwrap();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn later_wake_shutdown_is_reported_to_the_joiner() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let (future, state) = controlled_future();
        let task = executor.handle().try_spawn(future).unwrap();
        executor.tick().unwrap();

        executor.handle().begin_shutdown();
        state.wake();
        assert_eq!(
            block_on(task),
            Err(TaskJoinError::Scheduling(ScheduleError::ShuttingDown))
        );
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn later_wake_full_is_reported_and_releases_only_the_failed_task() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::with_queue_capacity(Arc::clone(&admission), 1);
        let (future, state) = controlled_future();
        let task = executor.handle().try_spawn(future).unwrap();
        executor.tick().unwrap();
        executor.handle().try_schedule(Box::new(|| {})).unwrap();

        state.wake();
        assert_eq!(
            block_on(task),
            Err(TaskJoinError::Scheduling(ScheduleError::Full))
        );
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);
        executor.tick().unwrap();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn later_wake_disconnect_is_reported_and_releases_the_task() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let (future, state) = controlled_future();
        let task = executor.handle().try_spawn(future).unwrap();
        executor.tick().unwrap();
        drop(executor);

        state.wake();
        assert_eq!(
            block_on(task),
            Err(TaskJoinError::Scheduling(ScheduleError::Disconnected))
        );
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn cancel_and_join_reports_expected_cancellation() {
        let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = SimpleExecutor::new(Arc::clone(&admission));
        let (future, _state) = controlled_future();
        let task = executor.handle().try_spawn(future).unwrap();
        executor.tick().unwrap();
        task.cancel();
        executor.tick().unwrap();

        assert_eq!(block_on(task.cancel_and_join()).unwrap(), None);
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn explicit_handle_and_send_output_task_have_cross_thread_handle_traits() {
        fn assert_send<T: Send>() {}
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<SimpleExecutorHandle>();
        assert_send_sync::<MainThreadExecutorHandle>();
        assert_send::<AdmittedTask<usize>>();
    }
}

pub struct ScopedExecutor {}

impl ScopedExecutor {
    pub fn new() -> Self {
        SCOPED_EXECUTOR
            .lock()
            .unwrap()
            .replace(Arc::new(Executor::new()));

        Self {}
    }

    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        get_scoped()
            .expect("SCOPED_EXECUTOR to be alive as long as ScopedExecutor")
            .run(future)
            .await
    }
}

impl Drop for ScopedExecutor {
    fn drop(&mut self) {
        SCOPED_EXECUTOR.lock().unwrap().take();
    }
}
