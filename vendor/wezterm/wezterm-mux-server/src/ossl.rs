use anyhow::{anyhow, Context, Error};
use async_ossl::AsyncSslStream;
use config::TlsDomainServer;
use mux::{CountClass, RuntimeAdmission};
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslStream, SslVerifyMode};
use openssl::x509::X509;
use promise::spawn::{AdmittedTask, MainThreadExecutorHandle, TaskJoinError};
use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::pin::Pin;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use wezterm_mux_server_impl::PKI;

struct OpenSSLNetListener {
    acceptor: Arc<SslAcceptor>,
    listener: TcpListener,
    policy: Arc<wezterm_mux_server_impl::authorization::ServerPolicy>,
    admission: Arc<RuntimeAdmission>,
    executor: MainThreadExecutorHandle,
    cancel: wezterm_mux_server_impl::dispatch::DispatchCancel,
    workers: HashMap<u64, AdmittedTask<anyhow::Result<()>>>,
    next_worker_id: u64,
    completed_tx: SyncSender<u64>,
    completed_rx: Receiver<u64>,
    wake: ListenerWake,
    wake_reader: std::os::unix::net::UnixStream,
}

#[derive(Clone)]
struct ListenerWake(Arc<Mutex<std::os::unix::net::UnixStream>>);

impl ListenerWake {
    fn wake(&self) {
        let mut writer = self.0.lock().unwrap();
        match writer.write(&[1]) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(err) => log::error!("failed to wake TLS listener: {:#}", err),
        }
    }
}

struct WorkerWake(ListenerWake);

impl Drop for WorkerWake {
    fn drop(&mut self) {
        self.0.wake();
    }
}

struct ThreadWake(Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

enum ListenerEvent {
    Accepted(TcpStream),
    Wake,
}

fn join_task<R>(mut task: AdmittedTask<R>) -> Result<R, TaskJoinError> {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = TaskContext::from_waker(&waker);
    loop {
        match Pin::new(&mut task).poll(&mut context) {
            Poll::Ready(result) => return result,
            Poll::Pending => thread::park(),
        }
    }
}

impl OpenSSLNetListener {
    pub fn new(
        listener: TcpListener,
        acceptor: SslAcceptor,
        policy: Arc<wezterm_mux_server_impl::authorization::ServerPolicy>,
        admission: Arc<RuntimeAdmission>,
        executor: MainThreadExecutorHandle,
    ) -> anyhow::Result<Self> {
        if !Arc::ptr_eq(&admission, executor.admission()) {
            anyhow::bail!("TLS listener and executor must share one runtime admission owner");
        }
        let completion_capacity = admission
            .count_capacity(CountClass::Attachment)
            .checked_add(admission.count_capacity(CountClass::RejectionWriter))
            .ok_or_else(|| anyhow!("TLS listener completion capacity overflow"))?;
        let (completed_tx, completed_rx) = sync_channel(completion_capacity);
        let (wake_reader, wake_writer) =
            std::os::unix::net::UnixStream::pair().context("creating TLS-listener wake socket")?;
        wake_writer
            .set_nonblocking(true)
            .context("making TLS-listener wake writer nonblocking")?;

        Ok(Self {
            listener,
            acceptor: Arc::new(acceptor),
            policy,
            admission,
            executor,
            cancel: wezterm_mux_server_impl::dispatch::DispatchCancel::new(),
            workers: HashMap::new(),
            next_worker_id: 0,
            completed_tx,
            completed_rx,
            wake: ListenerWake(Arc::new(Mutex::new(wake_writer))),
            wake_reader,
        })
    }

    /// Authenticates the peer.
    /// The requirements are:
    /// * The peer must have a certificate
    /// * The peer certificate must be trusted
    /// * The peer certificate must include a CN string that is
    ///   either an exact match for the unix username of the
    ///   user running this mux server instance, or must match
    ///   a special encoded prefix set up by a proprietary PKI
    ///   infrastructure in an environment used by the author.
    fn verify_peer_cert<T>(stream: &SslStream<T>) -> anyhow::Result<()> {
        let cert = stream
            .ssl()
            .peer_certificate()
            .ok_or_else(|| anyhow!("no peer cert"))?;
        let subject = cert.subject_name();
        let cn = subject
            .entries_by_nid(openssl::nid::Nid::COMMONNAME)
            .next()
            .ok_or_else(|| anyhow!("cert has no CN"))?;
        let cn_str = cn.data().as_utf8()?.to_string();

        let wanted_unix_name = std::env::var("USER")?;

        if wanted_unix_name == cn_str {
            log::trace!(
                "Peer certificate CN `{}` == $USER `{}`",
                cn_str,
                wanted_unix_name
            );
            Ok(())
        } else {
            // Some environments that are used by the author of this
            // program encode the CN in the form `user:unixname/DATA`
            let maybe_encoded = format!("user:{}/", wanted_unix_name);
            if cn_str.starts_with(&maybe_encoded) {
                log::trace!(
                    "Peer certificate CN `{}` matches $USER `{}`",
                    cn_str,
                    wanted_unix_name
                );
                Ok(())
            } else {
                anyhow::bail!("CN `{}` did not match $USER `{}`", cn_str, wanted_unix_name);
            }
        }
    }

    fn allocate_worker_id(&mut self) -> anyhow::Result<u64> {
        let worker_id = self.next_worker_id;
        self.next_worker_id = self
            .next_worker_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("TLS listener worker identity overflow"))?;
        Ok(worker_id)
    }

    fn schedule_rejection(&mut self, stream: TcpStream) -> anyhow::Result<()> {
        let rejection = match self.admission.try_count(CountClass::RejectionWriter, 1) {
            Ok(rejection) => rejection,
            Err(err) => {
                log::debug!("closing TLS attachment synchronously: {:#}", err);
                drop(stream);
                return Ok(());
            }
        };
        let worker_id = self.allocate_worker_id()?;
        let completed = self.completed_tx.clone();
        let worker_wake = WorkerWake(self.wake.clone());
        match self.executor.try_spawn(async move {
            let _worker_wake = worker_wake;
            let _rejection = rejection;
            let result = stream
                .shutdown(Shutdown::Both)
                .context("closing an over-capacity TLS attachment");
            completed
                .try_send(worker_id)
                .map_err(|_| anyhow!("TLS listener completion queue is saturated"))?;
            result
        }) {
            Ok(task) => {
                self.workers.insert(worker_id, task);
            }
            Err(err) => {
                // Dropping the refused future closes the stream and releases
                // both its rejection and runnable reservations synchronously.
                log::debug!("closing TLS attachment after scheduler refusal: {:#}", err);
            }
        }
        Ok(())
    }

    fn handle_accepted(&mut self, stream: TcpStream) -> anyhow::Result<()> {
        let attachment = match self.admission.try_attachment() {
            Ok(attachment) => attachment,
            Err(err) => {
                log::debug!("refusing TLS attachment: {:#}", err);
                return self.schedule_rejection(stream);
            }
        };

        stream.set_nodelay(true).ok();
        let stream = match self.acceptor.accept(stream) {
            Ok(stream) => stream,
            Err(err) => {
                log::error!("failed TlsAcceptor: {}", err);
                return Ok(());
            }
        };
        if let Err(err) = Self::verify_peer_cert(&stream) {
            log::error!("problem with peer cert: {}", err);
            return Ok(());
        }

        let policy = self.policy.clone();
        let cancel = self.cancel.clone();
        let executor = self.executor.clone();
        let worker_id = self.allocate_worker_id()?;
        let completed = self.completed_tx.clone();
        let worker_wake = WorkerWake(self.wake.clone());
        match self.executor.try_spawn(async move {
            let _worker_wake = worker_wake;
            let result = wezterm_mux_server_impl::dispatch::process(
                AsyncSslStream::new(stream),
                cancel,
                policy,
                Arc::new(attachment),
                executor,
            )
            .await;
            completed
                .try_send(worker_id)
                .map_err(|_| anyhow!("TLS listener completion queue is saturated"))?;
            result
        }) {
            Ok(task) => {
                self.workers.insert(worker_id, task);
            }
            Err(err) => {
                // The refused future owns the stream and attachment permit, so
                // dropping it performs a synchronous content-free close.
                log::error!("failed to schedule TLS attachment: {:#}", err);
            }
        }
        Ok(())
    }

    fn observe_worker(task: AdmittedTask<anyhow::Result<()>>) {
        match join_task(task) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => log::error!("TLS attachment failed: {:#}", err),
            Err(err) if err.is_cancelled() => {}
            Err(err) => log::error!("TLS attachment task failed: {:#}", err),
        }
    }

    fn reap_finished(&mut self) {
        while let Ok(worker_id) = self.completed_rx.try_recv() {
            if let Some(task) = self.workers.remove(&worker_id) {
                Self::observe_worker(task);
            }
        }

        let finished = self
            .workers
            .iter()
            .filter_map(|(worker_id, task)| task.is_finished().then_some(*worker_id))
            .collect::<Vec<_>>();
        for worker_id in finished {
            if let Some(task) = self.workers.remove(&worker_id) {
                Self::observe_worker(task);
            }
        }
    }

    fn cancel_and_join_workers(&mut self) {
        for task in self.workers.values() {
            task.cancel();
        }
        for (_, task) in std::mem::take(&mut self.workers) {
            Self::observe_worker(task);
        }
    }

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
                return Err(err).context("waiting for TLS-listener readiness");
            }

            let failure = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
            if descriptors[1].revents & failure != 0 {
                return Err(anyhow!("TLS-listener wake descriptor failed"));
            }
            if descriptors[0].revents & failure != 0 {
                return Err(anyhow!("TLS-listener descriptor failed"));
            }
            if descriptors[1].revents & libc::POLLIN != 0 {
                let mut bytes = [0; 64];
                self.wake_reader
                    .read(&mut bytes)
                    .context("draining TLS-listener wake signal")?;
                return Ok(ListenerEvent::Wake);
            }
            if descriptors[0].revents & libc::POLLIN != 0 {
                let (stream, _) = self.listener.accept().context("accepting TLS attachment")?;
                return Ok(ListenerEvent::Accepted(stream));
            }
        }
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let result = (|| {
            while !self.cancel.is_cancelled() {
                let event = self.next_event()?;
                self.reap_finished();
                match event {
                    ListenerEvent::Accepted(stream) => self.handle_accepted(stream)?,
                    ListenerEvent::Wake => {}
                }
            }
            Ok(())
        })();

        self.cancel.cancel();
        self.cancel_and_join_workers();
        result
    }
}

pub fn spawn_tls_listener(
    tls_server: &TlsDomainServer,
    policy: Arc<wezterm_mux_server_impl::authorization::ServerPolicy>,
    admission: Arc<RuntimeAdmission>,
    executor: MainThreadExecutorHandle,
) -> Result<JoinHandle<()>, Error> {
    openssl::init();

    let mut acceptor = SslAcceptor::mozilla_modern(SslMethod::tls())?;

    let cert_file = tls_server
        .pem_cert
        .clone()
        .unwrap_or_else(|| PKI.server_pem());
    acceptor
        .set_certificate_file(&cert_file, SslFiletype::PEM)
        .context(format!(
            "set_certificate_file to {} for TLS listener",
            cert_file.display()
        ))?;

    if let Some(chain_file) = tls_server.pem_ca.as_ref() {
        acceptor
            .set_certificate_chain_file(&chain_file)
            .context(format!(
                "set_certificate_chain_file to {} for TLS listener",
                chain_file.display()
            ))?;
    }

    let key_file = tls_server
        .pem_private_key
        .clone()
        .unwrap_or_else(|| PKI.server_pem());
    acceptor
        .set_private_key_file(&key_file, SslFiletype::PEM)
        .context(format!(
            "set_private_key_file to {} for TLS listener",
            key_file.display()
        ))?;

    fn load_cert(name: &Path) -> anyhow::Result<X509> {
        let cert_bytes = std::fs::read(name)?;
        log::trace!("loaded {}", name.display());
        Ok(X509::from_pem(&cert_bytes)?)
    }
    for name in &tls_server.pem_root_certs {
        if name.is_dir() {
            for entry in std::fs::read_dir(name)? {
                if let Ok(cert) = load_cert(&entry?.path()) {
                    acceptor.cert_store_mut().add_cert(cert).ok();
                }
            }
        } else {
            acceptor.cert_store_mut().add_cert(load_cert(name)?)?;
        }
    }

    acceptor
        .cert_store_mut()
        .add_cert(load_cert(&PKI.ca_pem())?)?;

    acceptor.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);

    let acceptor = acceptor.build();

    log::error!("listening with TLS on {:?}", tls_server.bind_address);

    let mut net_listener = OpenSSLNetListener::new(
        TcpListener::bind(&tls_server.bind_address).with_context(|| {
            format!(
                "error binding to mux_server_bind_address {}",
                tls_server.bind_address,
            )
        })?,
        acceptor,
        policy,
        admission,
        executor,
    )?;
    Ok(thread::spawn(move || {
        if let Err(err) = net_listener.run() {
            log::error!("TLS mux listener failed: {:#}", err);
        }
    }))
}
