use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use wezterm_mux::domain::{Domain, LocalDomain};
use wezterm_mux::{Mux, RuntimeAdmission, RuntimeRole};
use wezterm_mux_server_impl::authorization::ServerPolicy;
use wezterm_mux_server_impl::local::LocalListener;
use wezterm_mux_server_impl::local::LocalListenerControl;
use wezterm_promise::spawn::{
    AdmittedTask, MainThreadExecutorHandle, SimpleExecutor, SimpleExecutorHandle,
};

use super::authorization::ConsoleAuthorizer;
use super::client::{console_lock_path, console_runtime_dir, console_socket_path, unix_domain};

struct AgentControl {
    shutdown: Arc<AtomicBool>,
    listener: LocalListenerControl,
    executor: SimpleExecutorHandle,
}

impl AgentControl {
    fn request_shutdown(&self) -> Option<AdmittedTask<anyhow::Result<()>>> {
        self.shutdown.store(true, Ordering::Release);
        self.listener.shutdown();
        // Wake a blocked executor tick. Keeping the admitted task in the async owner until the
        // runtime thread exits makes this control edge bounded and joinable.
        self.executor.try_spawn(async { Ok(()) }).ok()
    }
}

/// Run the Console mux on its one process-global owner thread until the service is terminated.
pub async fn run() -> Result<()> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("installing the Console interrupt handler")?;
    let mut broken_pipe = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::pipe())
        .context("installing the Console broken-pipe handler")?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the Console termination handler")?;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let owner = std::thread::Builder::new()
        .name("kit-console-runtime".to_owned())
        .spawn(move || {
            let result = run_owner(ready_tx);
            let _ = done_tx.send(());
            result
        })
        .context("starting the Console runtime owner")?;

    let control = match ready_rx.await {
        Ok(control) => control,
        Err(_) => {
            let _ = done_rx.await;
            return join_owner(owner);
        }
    };

    let signal_result = loop {
        tokio::select! {
            result = &mut done_rx => {
                result.context("Console runtime owner completion channel closed")?;
                return join_owner(owner);
            }
            _ = interrupt.recv() => break Ok(()),
            _ = broken_pipe.recv() => continue,
            _ = terminate.recv() => break Ok(()),
        }
    };
    let _wake = control.request_shutdown();
    let completion_result =
        done_rx.await.context("Console runtime owner completion channel closed");
    let owner_result = join_owner(owner);
    signal_result.and(completion_result).and(owner_result)
}

fn join_owner(owner: std::thread::JoinHandle<Result<()>>) -> Result<()> {
    owner.join().map_err(|_| anyhow::anyhow!("Console runtime owner panicked"))?
}

fn run_owner(ready: tokio::sync::oneshot::Sender<AgentControl>) -> Result<()> {
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true).context("initializing headless WezTerm config")?;
    let mut config = wezterm_config::Config::default_config();
    config.exit_behavior = wezterm_config::ExitBehavior::Hold;
    config.exit_behavior_messaging = wezterm_config::ExitBehaviorMessaging::Terse;
    wezterm_config::use_this_configuration(config);

    let runtime_dir = console_runtime_dir()?;
    super::runtime::prepare(&runtime_dir)?;
    wezterm_config::create_user_owned_dirs(&wezterm_config::RUNTIME_DIR)
        .context("creating the embedded WezTerm runtime directory")?;
    let lock_path = console_lock_path()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening Console agent lock {}", lock_path.display()))?;
    let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            bail!("a Console agent is already running");
        }
        return Err(error).context("locking the Console agent lock file");
    }

    let admission = RuntimeAdmission::new(RuntimeRole::Server)?;
    wezterm_blob_leases::register_storage(
        Arc::new(
            wezterm_blob_leases::simple_tempdir::SimpleTempDir::new_in(&runtime_dir)
                .context("creating Console blob storage")?,
        ),
        Arc::clone(&admission),
    )
    .context("registering Console blob storage")?;

    let socket_path = console_socket_path()?;
    std::env::set_var("WEZTERM_UNIX_SOCKET", &socket_path);
    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
    let executor = Arc::new(SimpleExecutor::new(Arc::clone(&admission)));
    let mux =
        Arc::new(Mux::new_headless(Some(domain), Arc::clone(&admission), Arc::clone(&executor)));
    Mux::set_mux(&mux)?;

    let policy = ServerPolicy::new(Arc::new(ConsoleAuthorizer), super::build_identity()?);
    let mut listener = LocalListener::with_domain(
        &unix_domain()?,
        policy,
        Arc::clone(&admission),
        MainThreadExecutorHandle::from_simple(executor.handle()),
    )?;
    let listener_control = listener.control();
    let listener_thread = std::thread::Builder::new()
        .name("kit-console-listener".to_owned())
        .spawn(move || listener.run())
        .context("starting Console listener thread")?;

    let shutdown = Arc::new(AtomicBool::new(false));
    ready
        .send(AgentControl {
            shutdown: Arc::clone(&shutdown),
            listener: listener_control.clone(),
            executor: executor.handle(),
        })
        .map_err(|_| anyhow::anyhow!("Console runtime owner was abandoned during startup"))?;

    let result: Result<()> = loop {
        if shutdown.load(Ordering::Acquire) {
            break Ok(());
        }
        if let Err(error) = mux.tick_headless() {
            break Err(error.context("ticking Console mux runtime"));
        }
    };

    listener_control.shutdown();
    listener_control.wait();
    let listener_result =
        listener_thread.join().map_err(|_| anyhow::anyhow!("Console listener thread panicked"))?;
    let listener_error = listener_control
        .take_fatal_error()
        .map_or(Ok(()), |error| Err(error.context("Console listener failed")));
    Mux::shutdown();
    let socket_cleanup = match std::fs::remove_file(&socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("removing Console agent socket {}", socket_path.display())),
    };
    wezterm_blob_leases::clear_storage();
    drop(lock);

    result.and(listener_result).and(listener_error).and(socket_cleanup)
}
