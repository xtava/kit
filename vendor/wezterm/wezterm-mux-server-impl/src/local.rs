use anyhow::{anyhow, Context as _};
use config::{create_user_owned_dirs, UnixDomain};
use promise::spawn::{AdmittedTask, MainThreadExecutorHandle};
use smol::io::AsyncWriteExt as _;
use std::collections::HashMap;
use std::io::Write;
use std::net::Shutdown;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use wezterm_runtime_admission::{
    AdmissionError, AttachmentPermit, CountClass, CountPermit, RuntimeAdmission, MAX_ATTACHMENTS,
    MAX_REJECTION_WRITERS,
};
use wezterm_uds::{UnixListener, UnixStream};

const MAX_ATTACH_REJECTED_FRAME_BYTES: usize = 16;

pub struct LocalListener {
    listener: UnixListener,
    control: LocalListenerControl,
    policy: Arc<crate::authorization::ServerPolicy>,
    admission: Arc<RuntimeAdmission>,
    executor: MainThreadExecutorHandle,
    workers: HashMap<u64, AdmittedTask<anyhow::Result<()>>>,
    next_worker_id: u64,
    completed_tx: smol::channel::Sender<u64>,
    completed_rx: smol::channel::Receiver<u64>,
    attach_rejected_frame: Arc<[u8]>,
    #[cfg(unix)]
    wake_reader: std::os::unix::net::UnixStream,
}

#[derive(Clone)]
pub struct LocalListenerControl {
    cancel: crate::dispatch::DispatchCancel,
    active: Arc<(Mutex<()>, Condvar)>,
    active_count: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    fatal_error: Arc<Mutex<Option<anyhow::Error>>>,
    connection_failures: Arc<AtomicUsize>,
    async_refusals: Arc<AtomicUsize>,
    sync_refusals: Arc<AtomicUsize>,
    #[cfg(unix)]
    wake_writer: Arc<Mutex<std::os::unix::net::UnixStream>>,
}

impl LocalListenerControl {
    pub fn shutdown(&self) {
        self.cancel.cancel();
        self.wake();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn wait(&self) {
        let (lock, ready) = &*self.active;
        let mut guard = lock.lock().unwrap();
        while !self.is_stopped() || self.active_count.load(Ordering::Acquire) != 0 {
            guard = ready.wait(guard).unwrap();
        }
    }

    pub fn take_fatal_error(&self) -> Option<anyhow::Error> {
        self.fatal_error.lock().unwrap().take()
    }

    pub fn connection_failures(&self) -> usize {
        self.connection_failures.load(Ordering::Acquire)
    }

    pub fn async_refusals(&self) -> usize {
        self.async_refusals.load(Ordering::Acquire)
    }

    pub fn sync_refusals(&self) -> usize {
        self.sync_refusals.load(Ordering::Acquire)
    }

    fn record_fatal_error(&self, error: anyhow::Error) {
        let mut first = self.fatal_error.lock().unwrap();
        if first.is_none() {
            *first = Some(error);
        }
    }

    fn record_connection_failure(&self, error: anyhow::Error) {
        self.connection_failures.fetch_add(1, Ordering::AcqRel);
        log::debug!("local attachment ended with an isolated failure: {error:#}");
    }

    fn record_async_refusal(&self) {
        self.async_refusals
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                Some(count.saturating_add(1))
            })
            .expect("saturating refusal update cannot fail");
    }

    fn record_sync_refusal(&self) {
        self.sync_refusals
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                Some(count.saturating_add(1))
            })
            .expect("saturating refusal update cannot fail");
    }

    fn wake(&self) {
        #[cfg(unix)]
        {
            let mut writer = self.wake_writer.lock().unwrap();
            match writer.write(&[1]) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(err) => self.record_fatal_error(err.into()),
            }
        }
    }
}

struct ActiveConnection(LocalListenerControl);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.active_count.fetch_sub(1, Ordering::AcqRel);
        self.0.active.1.notify_all();
        self.0.wake();
    }
}

struct WorkerWake(LocalListenerControl);

impl Drop for WorkerWake {
    fn drop(&mut self) {
        self.0.wake();
    }
}

enum AcceptedAdmission {
    Attachment(AttachmentPermit),
    Rejection(CountPermit),
    Close,
}

enum ListenerEvent {
    Accepted(UnixStream),
    Wake,
}

impl LocalListener {
    pub fn new(
        listener: UnixListener,
        policy: Arc<crate::authorization::ServerPolicy>,
        admission: Arc<RuntimeAdmission>,
        executor: MainThreadExecutorHandle,
    ) -> anyhow::Result<Self> {
        if !Arc::ptr_eq(&admission, executor.admission()) {
            anyhow::bail!("local-listener admission and executor admission must be identical");
        }
        #[cfg(unix)]
        let (wake_reader, wake_writer) = std::os::unix::net::UnixStream::pair()
            .context("creating local-listener cancellation socket")?;
        #[cfg(unix)]
        wake_writer
            .set_nonblocking(true)
            .context("making local-listener cancellation writer nonblocking")?;

        let completion_capacity = MAX_ATTACHMENTS
            .checked_add(MAX_REJECTION_WRITERS)
            .ok_or_else(|| anyhow!("local-listener completion capacity overflow"))?;
        let (completed_tx, completed_rx) = smol::channel::bounded(completion_capacity);
        let mut attach_rejected_frame = Vec::new();
        codec::Pdu::AttachRejected(codec::AttachRejected {}).encode(
            &mut attach_rejected_frame,
            0,
            &admission,
        )?;
        anyhow::ensure!(
            attach_rejected_frame.len() <= MAX_ATTACH_REJECTED_FRAME_BYTES,
            "AttachRejected frame exceeds its fixed pre-bootstrap bound"
        );
        Ok(Self {
            listener,
            control: LocalListenerControl {
                cancel: crate::dispatch::DispatchCancel::new(),
                active: Arc::new((Mutex::new(()), Condvar::new())),
                active_count: Arc::new(AtomicUsize::new(0)),
                stopped: Arc::new(AtomicBool::new(false)),
                fatal_error: Arc::new(Mutex::new(None)),
                connection_failures: Arc::new(AtomicUsize::new(0)),
                async_refusals: Arc::new(AtomicUsize::new(0)),
                sync_refusals: Arc::new(AtomicUsize::new(0)),
                #[cfg(unix)]
                wake_writer: Arc::new(Mutex::new(wake_writer)),
            },
            policy,
            admission,
            executor,
            workers: HashMap::new(),
            next_worker_id: 0,
            completed_tx,
            completed_rx,
            attach_rejected_frame: attach_rejected_frame.into(),
            #[cfg(unix)]
            wake_reader,
        })
    }

    pub fn with_domain(
        unix_dom: &UnixDomain,
        policy: Arc<crate::authorization::ServerPolicy>,
        admission: Arc<RuntimeAdmission>,
        executor: MainThreadExecutorHandle,
    ) -> anyhow::Result<Self> {
        let listener = safely_create_sock_path(unix_dom)?;
        Self::new(listener, policy, admission, executor)
    }

    fn admit_accepted(&self) -> Result<AcceptedAdmission, AdmissionError> {
        match self.admission.try_attachment() {
            Ok(attachment) => Ok(AcceptedAdmission::Attachment(attachment)),
            Err(AdmissionError::CapacityExceeded { .. }) => {
                match self.admission.try_count(CountClass::RejectionWriter, 1) {
                    Ok(rejection) => Ok(AcceptedAdmission::Rejection(rejection)),
                    Err(AdmissionError::CapacityExceeded { .. }) => Ok(AcceptedAdmission::Close),
                    Err(AdmissionError::ShuttingDown) => Ok(AcceptedAdmission::Close),
                    Err(err) => Err(err),
                }
            }
            Err(AdmissionError::ShuttingDown) => Ok(AcceptedAdmission::Close),
            Err(err) => Err(err),
        }
    }

    fn schedule_connection(
        &mut self,
        stream: UnixStream,
        attachment: AttachmentPermit,
    ) -> anyhow::Result<()> {
        self.control.active_count.fetch_add(1, Ordering::AcqRel);
        let active = ActiveConnection(self.control.clone());
        let cancel = self.control.cancel.clone();
        let policy = self.policy.clone();
        let executor = self.executor.clone();
        let worker_id = self.allocate_worker_id()?;
        let completed = self.completed_tx.clone();
        let control = self.control.clone();
        let task = self.executor.try_spawn(async move {
            let _active = active;
            let result =
                crate::dispatch::process(stream, cancel, policy, Arc::new(attachment), executor)
                    .await;
            if completed.try_send(worker_id).is_err() {
                control.record_fatal_error(anyhow!("local-listener completion queue is saturated"));
            }
            control.wake();
            result
        })?;
        self.workers.insert(worker_id, task);
        Ok(())
    }

    fn schedule_rejection(
        &mut self,
        stream: UnixStream,
        rejection: CountPermit,
    ) -> anyhow::Result<()> {
        let control = self.control.clone();
        let worker_id = self.allocate_worker_id()?;
        let completed = self.completed_tx.clone();
        let attach_rejected_frame = Arc::clone(&self.attach_rejected_frame);
        match self.executor.try_spawn(async move {
            let _wake = WorkerWake(control.clone());
            let _rejection = rejection;
            let result: anyhow::Result<()> = async {
                let mut stream = smol::Async::new(stream)
                    .context("making an over-capacity local attachment asynchronous")?;
                stream
                    .write_all(attach_rejected_frame.as_ref())
                    .await
                    .context("writing AttachRejected to an over-capacity local attachment")?;
                stream
                    .flush()
                    .await
                    .context("flushing AttachRejected to an over-capacity local attachment")?;
                stream
                    .get_ref()
                    .shutdown(Shutdown::Both)
                    .context("closing an over-capacity local attachment")?;
                Ok(())
            }
            .await;
            if completed.try_send(worker_id).is_err() {
                control.record_fatal_error(anyhow!("local-listener completion queue is saturated"));
            }
            control.wake();
            result
        }) {
            Ok(task) => {
                self.control.record_async_refusal();
                self.workers.insert(worker_id, task);
                Ok(())
            }
            Err(err) => {
                // The refused future owns the stream and permit, so dropping it closes the
                // transport without leaving an unowned rejection worker.
                self.control.record_sync_refusal();
                Err(err.into())
            }
        }
    }

    fn reject_synchronously(&self, mut stream: UnixStream) {
        let result = (|| -> anyhow::Result<()> {
            stream
                .write_all(self.attach_rejected_frame.as_ref())
                .context("writing synchronous AttachRejected")?;
            stream
                .flush()
                .context("flushing synchronous AttachRejected")?;
            stream
                .shutdown(Shutdown::Both)
                .context("closing a synchronously rejected local attachment")?;
            Ok(())
        })();
        if let Err(error) = result {
            self.control.record_fatal_error(error);
        }
    }

    fn handle_accepted(&mut self, stream: UnixStream) -> anyhow::Result<()> {
        match self.admit_accepted()? {
            AcceptedAdmission::Attachment(attachment) => {
                if let Err(err) = self.schedule_connection(stream, attachment) {
                    self.control.record_fatal_error(err);
                }
                Ok(())
            }
            AcceptedAdmission::Rejection(rejection) => {
                if let Err(err) = self.schedule_rejection(stream, rejection) {
                    self.control.record_fatal_error(err);
                }
                Ok(())
            }
            AcceptedAdmission::Close => {
                self.control.record_sync_refusal();
                self.reject_synchronously(stream);
                Ok(())
            }
        }
    }

    fn allocate_worker_id(&mut self) -> anyhow::Result<u64> {
        let id = self.next_worker_id;
        self.next_worker_id = self
            .next_worker_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("local-listener worker identity overflow"))?;
        Ok(id)
    }

    fn reap_finished(&mut self) {
        while let Ok(worker_id) = self.completed_rx.try_recv() {
            if let Some(task) = self.workers.remove(&worker_id) {
                match smol::block_on(task) {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => self.control.record_connection_failure(err),
                    Err(err) if err.is_cancelled() => {}
                    Err(err) => self.control.record_fatal_error(err.into()),
                }
            }
        }
        let finished = self
            .workers
            .iter()
            .filter_map(|(worker_id, task)| task.is_finished().then_some(*worker_id))
            .collect::<Vec<_>>();
        for worker_id in finished {
            if let Some(task) = self.workers.remove(&worker_id) {
                match smol::block_on(task) {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => self.control.record_connection_failure(err),
                    Err(err) if err.is_cancelled() => {}
                    Err(err) => self.control.record_fatal_error(err.into()),
                }
            }
        }
    }

    fn cancel_and_join_workers(&mut self) {
        for task in self.workers.values() {
            task.cancel();
        }
        for (_, task) in std::mem::take(&mut self.workers) {
            match smol::block_on(task) {
                Ok(Ok(())) => {}
                Ok(Err(err)) => self.control.record_connection_failure(err),
                Err(err) if err.is_cancelled() => {}
                Err(err) => self.control.record_fatal_error(err.into()),
            }
        }
    }

    #[cfg(unix)]
    fn next_event(&mut self) -> anyhow::Result<ListenerEvent> {
        loop {
            let mut descriptors = [
                libc::pollfd {
                    fd: self.listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.wake_reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let result = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    -1,
                )
            };
            if result < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err).context("waiting for local-listener readiness");
            }
            let failure = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
            if descriptors[1].revents & failure != 0 {
                if self.control.cancel.is_cancelled() {
                    return Ok(ListenerEvent::Wake);
                }
                anyhow::bail!("local-listener wake descriptor failed");
            }
            if descriptors[0].revents & failure != 0 {
                anyhow::bail!("local-listener descriptor failed");
            }
            if descriptors[1].revents & libc::POLLIN != 0 {
                let mut byte = [0; 64];
                std::io::Read::read(&mut self.wake_reader, &mut byte)
                    .context("draining the local-listener wake signal")?;
                return Ok(ListenerEvent::Wake);
            }
            if descriptors[0].revents & libc::POLLIN != 0 {
                let (stream, _) = self
                    .listener
                    .accept()
                    .context("accepting local attachment")?;
                return Ok(ListenerEvent::Accepted(stream));
            }
        }
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        let result = (|| {
            while !self.control.cancel.is_cancelled() {
                let event = self.next_event()?;
                self.reap_finished();
                match event {
                    ListenerEvent::Accepted(stream) => self.handle_accepted(stream)?,
                    ListenerEvent::Wake => {}
                }
            }
            Ok(())
        })();

        self.control.cancel.cancel();
        self.cancel_and_join_workers();
        self.control.stopped.store(true, Ordering::Release);
        self.control.active.1.notify_all();
        result
    }

    pub fn control(&self) -> LocalListenerControl {
        self.control.clone()
    }
}

/// Take care when setting up the listener socket;
/// we need to be sure that the directory that we create it in
/// is owned by the user and has appropriate file permissions
/// that prevent other users from manipulating its contents.
fn safely_create_sock_path(unix_dom: &UnixDomain) -> anyhow::Result<UnixListener> {
    let sock_path = &unix_dom.socket_path();
    log::trace!("setting up {}", sock_path.display());

    let sock_dir = sock_path
        .parent()
        .ok_or_else(|| anyhow!("sock_path {} has no parent dir", sock_path.display()))?;

    create_user_owned_dirs(sock_dir)?;

    #[cfg(unix)]
    {
        use config::running_under_wsl;
        use std::os::unix::fs::PermissionsExt;

        if !running_under_wsl() && !unix_dom.skip_permissions_check {
            // Let's be sure that the ownership looks sane
            let meta = sock_dir.symlink_metadata()?;

            let permissions = meta.permissions();
            if (permissions.mode() & 0o22) != 0 {
                anyhow::bail!(
                    "The permissions for {} are insecure and currently \
                     allow other users to write to it (permissions={:?})",
                    sock_dir.display(),
                    permissions
                );
            }
        }
    }

    // We want to remove the socket if it exists.
    // However, on windows, we can't tell if the unix domain socket
    // exists using the methods on Path, so instead we just unconditionally
    // remove it and see what error occurs.
    match std::fs::remove_file(sock_path) {
        Ok(_) => {}
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => {}
            _ => return Err(err).context(format!("Unable to remove {}", sock_path.display())),
        },
    }

    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("Failed to bind to {}", sock_path.display()))?;

    config::set_sticky_bit(sock_path);

    Ok(listener)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use wezterm_runtime_admission::RuntimeRole;

    fn listener() -> (
        std::path::PathBuf,
        LocalListener,
        promise::spawn::SimpleExecutor,
    ) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = std::env::temp_dir().join(format!(
            "wezterm-local-listener-{}-{unique}.sock",
            std::process::id()
        ));
        let socket_listener = UnixListener::bind(&socket).unwrap();
        let policy = crate::authorization::ServerPolicy::new(
            Arc::new(crate::authorization::AllowAllRequests),
            codec::BuildIdentity {
                product: "test".to_string(),
                version: "test".to_string(),
                source_revision: None,
                source_dirty: None,
                embedded_wezterm_revision: None,
            },
        );
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let executor = promise::spawn::SimpleExecutor::new(Arc::clone(&admission));
        let listener = LocalListener::new(
            socket_listener,
            policy,
            admission,
            MainThreadExecutorHandle::from_simple(executor.handle()),
        )
        .unwrap();
        (socket, listener, executor)
    }

    fn assert_attach_rejected(client: &mut UnixStream) {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let notification = codec::Pdu::decode(
            client,
            codec::DecodeContext::server_to_client_notification(),
            &admission,
        )
        .unwrap()
        .into_notification()
        .unwrap();
        assert!(matches!(
            notification.pdu(),
            codec::Pdu::AttachRejected(codec::AttachRejected {})
        ));
    }

    #[test]
    fn local_listener_cancel_waits_for_run_loop_join() {
        let (socket, mut listener, _executor) = listener();
        let control = listener.control();
        let worker = std::thread::spawn(move || listener.run());

        control.shutdown();
        control.wait();
        worker.join().unwrap().unwrap();
        assert!(control.is_stopped());
        assert!(control.take_fatal_error().is_none());

        std::fs::remove_file(socket).ok();
    }

    #[test]
    fn local_listener_rejects_a_distinct_executor_admission() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = std::env::temp_dir().join(format!(
            "wezterm-local-listener-mismatch-{}-{unique}.sock",
            std::process::id()
        ));
        let socket_listener = UnixListener::bind(&socket).unwrap();
        let policy = crate::authorization::ServerPolicy::new(
            Arc::new(crate::authorization::AllowAllRequests),
            codec::BuildIdentity {
                product: "test".to_string(),
                version: "test".to_string(),
                source_revision: None,
                source_dirty: None,
                embedded_wezterm_revision: None,
            },
        );
        let listener_admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let executor_admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let executor = promise::spawn::SimpleExecutor::new(executor_admission);
        let error = LocalListener::new(
            socket_listener,
            policy,
            listener_admission,
            MainThreadExecutorHandle::from_simple(executor.handle()),
        )
        .err()
        .unwrap();

        assert!(error
            .to_string()
            .contains("listener admission and executor admission must be identical"));
        std::fs::remove_file(socket).ok();
    }

    #[test]
    fn connection_failure_is_observed_without_poisoning_the_listener() {
        let (socket, mut listener, executor) = listener();
        let task = listener
            .executor
            .try_spawn(async { Err(anyhow!("client protocol rejected")) })
            .unwrap();
        listener.workers.insert(0, task);

        executor.tick().unwrap();
        listener.reap_finished();

        assert_eq!(listener.control.connection_failures(), 1);
        assert!(listener.control.take_fatal_error().is_none());
        drop(listener);
        std::fs::remove_file(socket).ok();
    }

    #[test]
    fn accept_and_rejection_admission_have_exact_independent_boundaries() {
        let (socket, listener, _executor) = listener();
        assert!(listener.attach_rejected_frame.len() <= MAX_ATTACH_REJECTED_FRAME_BYTES);
        let mut attachments = Vec::new();
        for _ in 0..MAX_ATTACHMENTS {
            match listener.admit_accepted().unwrap() {
                AcceptedAdmission::Attachment(permit) => attachments.push(permit),
                _ => panic!("attachment capacity rejected early"),
            }
        }

        let mut rejections = Vec::new();
        for _ in 0..MAX_REJECTION_WRITERS {
            match listener.admit_accepted().unwrap() {
                AcceptedAdmission::Rejection(permit) => rejections.push(permit),
                _ => panic!("rejection-writer capacity rejected early"),
            }
        }
        assert!(matches!(
            listener.admit_accepted().unwrap(),
            AcceptedAdmission::Close
        ));

        assert_eq!(
            listener.admission.count_usage(CountClass::Attachment),
            MAX_ATTACHMENTS
        );
        assert_eq!(
            listener.admission.count_usage(CountClass::RejectionWriter),
            MAX_REJECTION_WRITERS
        );
        drop(rejections);
        drop(attachments);
        assert_eq!(listener.admission.count_usage(CountClass::Attachment), 0);
        assert_eq!(
            listener.admission.count_usage(CountClass::RejectionWriter),
            0
        );

        drop(listener);
        std::fs::remove_file(socket).ok();
    }

    #[test]
    fn over_capacity_connection_uses_joined_content_free_rejection_worker() {
        let (socket, mut listener, executor) = listener();
        let attachments = (0..MAX_ATTACHMENTS)
            .map(|_| listener.admission.try_attachment().unwrap())
            .collect::<Vec<_>>();
        let mut client = UnixStream::connect(&socket).unwrap();
        let (server, _) = listener.listener.accept().unwrap();

        listener.handle_accepted(server).unwrap();
        assert_eq!(listener.control.async_refusals(), 1);
        assert_eq!(listener.control.sync_refusals(), 0);
        assert_eq!(listener.workers.len(), 1);
        assert_eq!(
            listener.admission.count_usage(CountClass::RejectionWriter),
            1
        );

        executor.tick().unwrap();
        listener.reap_finished();
        assert!(listener.workers.is_empty());
        assert_eq!(
            listener.admission.count_usage(CountClass::RejectionWriter),
            0
        );
        assert_attach_rejected(&mut client);

        drop(client);
        drop(attachments);
        drop(listener);
        std::fs::remove_file(socket).ok();
    }

    #[test]
    fn rejection_saturation_falls_back_to_synchronous_typed_rejection() {
        let (socket, mut listener, _executor) = listener();
        let attachments = (0..MAX_ATTACHMENTS)
            .map(|_| listener.admission.try_attachment().unwrap())
            .collect::<Vec<_>>();
        let rejections = (0..MAX_REJECTION_WRITERS)
            .map(|_| {
                listener
                    .admission
                    .try_count(CountClass::RejectionWriter, 1)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut client = UnixStream::connect(&socket).unwrap();
        let (server, _) = listener.listener.accept().unwrap();

        listener.handle_accepted(server).unwrap();
        assert_eq!(listener.control.async_refusals(), 0);
        assert_eq!(listener.control.sync_refusals(), 1);
        assert!(listener.workers.is_empty());
        assert_attach_rejected(&mut client);

        drop(client);
        drop(rejections);
        drop(attachments);
        drop(listener);
        std::fs::remove_file(socket).ok();
    }
}
