use crate::authorization::{AdmittedAuthorizedRequest, ServerIssuedIdentity, ServerPolicy};
use crate::dispatch::AttachmentConnection;
use crate::PKI;
use anyhow::{anyhow, Context};
use codec::*;
use config::keyassignment::SpawnTabDomain as MuxSpawnTabDomain;
use config::TermConfig;
use futures::future::pending;
use futures::stream::{FuturesUnordered, StreamExt};
use mux::client::ClientId;
use mux::domain::SplitSource;
use mux::pane::{CachePolicy, Pane, PaneId};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{SplitSize, TabId};
use mux::{Mux, MuxNotification, PaneMutationKind};
use portable_pty::CommandBuilder;
use promise::spawn::{AdmittedTask, MainThreadExecutorHandle};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_runtime_admission::{CombinedPermit, CountClass, RuntimeAdmission};
use wezterm_term::terminal::Alert;
use wezterm_term::StableRowIndex;

const MAX_GET_LINES_RANGES: usize = 64;
const MAX_GET_LINES_TOTAL_ROWS: usize = 1_024;
const MAX_SEARCH_PATTERN_BYTES: usize = 65_536;
const MAX_ENCODED_INPUT_EVENT_BYTES: usize = 256;
const BRACKETED_PASTE_ENVELOPE_BYTES: usize = 12;
const MAX_SEARCH_RANGE_ROWS: usize = 10_512;
const MAX_SEARCH_RESULTS: u32 = 4_096;
const MAX_ADJUST_PANE_CELLS: usize = 1_024;

fn pane_paste_enveloped_byte_count(data_len: usize) -> anyhow::Result<usize> {
    data_len
        .checked_add(BRACKETED_PASTE_ENVELOPE_BYTES)
        .ok_or_else(|| anyhow!("pane paste byte envelope overflow"))
}

fn validate_pane_input_bytes(bytes: usize, kind: &'static str) -> anyhow::Result<()> {
    anyhow::ensure!(
        bytes <= wezterm_runtime_admission::MAX_PANE_INPUT_BYTES_PER_PANE,
        "{kind} has {bytes} bytes, exceeding {}",
        wezterm_runtime_admission::MAX_PANE_INPUT_BYTES_PER_PANE
    );
    Ok(())
}

fn validate_search_pattern_bytes(bytes: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        bytes <= MAX_SEARCH_PATTERN_BYTES,
        "search pattern has {bytes} bytes, exceeding {MAX_SEARCH_PATTERN_BYTES}"
    );
    Ok(())
}

#[derive(Clone)]
pub struct PduSender {
    func: Arc<dyn Fn(Pdu, u64) -> anyhow::Result<()> + Send + Sync>,
}

impl PduSender {
    pub fn send(&self, pdu: Pdu, serial: u64) -> anyhow::Result<()> {
        (self.func)(pdu, serial)
    }

    pub fn new<T>(f: T) -> Self
    where
        T: Fn(Pdu, u64) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self { func: Arc::new(f) }
    }
}

#[derive(Default, Debug)]
pub(crate) struct PerPane {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    mouse_grabbed: bool,
    is_alt_screen_active: bool,
    sent_initial_palette: bool,
    seqno: SequenceNo,
    config_generation: usize,
    pub(crate) notifications: Vec<Alert>,
}

#[derive(Debug)]
enum BootstrapState {
    AwaitingClient { proxy: Option<ClientId> },
    Established(ServerIssuedIdentity),
}

impl BootstrapState {
    fn request_phase(&self) -> ClientRequestPhase {
        match self {
            Self::AwaitingClient { .. } => ClientRequestPhase::Bootstrap,
            Self::Established(_) => ClientRequestPhase::Established,
        }
    }
}

impl PerPane {
    fn compute_changes(
        &mut self,
        pane: &Arc<dyn Pane>,
        force_with_input_serial: Option<InputSerial>,
    ) -> Option<GetPaneRenderChangesResponse> {
        let mut changed = false;
        let mouse_grabbed = pane.is_mouse_grabbed();
        if mouse_grabbed != self.mouse_grabbed {
            changed = true;
        }

        let dims = pane.get_dimensions();
        let viewport_range =
            dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex;
        if dims != self.dimensions {
            changed = true;
        }

        let is_alt_screen_active = pane.is_alt_screen_active();
        let screen_changed = is_alt_screen_active != self.is_alt_screen_active;
        if screen_changed {
            changed = true;
        }

        let cursor_position = pane.get_cursor_position();
        if cursor_position != self.cursor_position {
            changed = true;
        }

        let title = pane.get_title();
        if title != self.title {
            changed = true;
        }

        let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
        if working_dir != self.working_dir {
            changed = true;
        }

        let old_seqno = self.seqno;
        self.seqno = pane.get_current_seqno();
        let mut all_dirty_lines = pane.get_changed_since(
            0..dims.physical_top + dims.viewport_rows as StableRowIndex,
            old_seqno,
        );
        if screen_changed {
            all_dirty_lines.add_range(viewport_range.clone());
        }
        if !all_dirty_lines.is_empty() {
            changed = true;
        }

        if !changed && !force_with_input_serial.is_some() {
            return None;
        }

        // Figure out what we're going to send as dirty lines vs bonus lines
        let (first_line, lines) = pane.get_lines(viewport_range);
        let mut bonus_lines = lines
            .into_iter()
            .enumerate()
            .filter_map(|(idx, mut line)| {
                let stable_row = first_line + idx as StableRowIndex;
                if all_dirty_lines.contains(stable_row) {
                    all_dirty_lines.remove(stable_row);
                    line.compress_for_scrollback();
                    Some((stable_row, line))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Always send the cursor's row, as that tends to the busiest and we don't
        // have a sequencing concept for our idea of the remote state.
        let (cursor_line_idx, mut lines) = pane.get_lines(cursor_position.y..cursor_position.y + 1);
        let mut cursor_line = lines.remove(0);
        cursor_line.compress_for_scrollback();
        bonus_lines.push((cursor_line_idx, cursor_line));

        self.cursor_position = cursor_position;
        self.title = title.clone();
        self.working_dir = working_dir.clone();
        self.dimensions = dims;
        self.mouse_grabbed = mouse_grabbed;
        self.is_alt_screen_active = is_alt_screen_active;

        let bonus_lines = bonus_lines.into();
        Some(GetPaneRenderChangesResponse {
            pane_id: pane.pane_id(),
            mouse_grabbed,
            dirty_lines: all_dirty_lines.iter().cloned().collect(),
            dimensions: dims,
            cursor_position,
            title,
            bonus_lines,
            working_dir: working_dir.map(Into::into),
            input_serial: force_with_input_serial,
            seqno: self.seqno,
        })
    }
}

fn maybe_push_pane_changes(
    pane: &Arc<dyn Pane>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
) -> anyhow::Result<()> {
    let mut per_pane = per_pane.lock().unwrap();
    if let Some(resp) = per_pane.compute_changes(pane, None) {
        sender.send(Pdu::GetPaneRenderChangesResponse(resp), 0)?;
    }

    let config = config::configuration();
    if per_pane.config_generation != config.generation() {
        per_pane.config_generation = config.generation();
        // If the config changed, it may have changed colors
        // in the palette that we need to push down, so we
        // synthesize a palette change notification to let
        // the client know
        per_pane.notifications.push(Alert::PaletteChanged);
        per_pane.sent_initial_palette = true;
    }

    if !per_pane.sent_initial_palette {
        per_pane.notifications.push(Alert::PaletteChanged);
        per_pane.sent_initial_palette = true;
    }
    for alert in per_pane.notifications.drain(..) {
        match alert {
            Alert::PaletteChanged => {
                sender.send(
                    Pdu::SetPalette(SetPalette {
                        pane_id: pane.pane_id(),
                        palette: Box::new(pane.palette()),
                    }),
                    0,
                )?;
            }
            alert => {
                sender.send(
                    Pdu::NotifyAlert(NotifyAlert {
                        pane_id: pane.pane_id(),
                        alert,
                    }),
                    0,
                )?;
            }
        }
    }
    Ok(())
}

pub struct SessionHandler {
    to_write_tx: PduSender,
    per_pane: HashMap<PaneId, Arc<Mutex<PerPane>>>,
    bootstrap: BootstrapState,
    policy: Arc<ServerPolicy>,
    admission: Arc<RuntimeAdmission>,
    executor: MainThreadExecutorHandle,
    tasks: FuturesUnordered<AdmittedTask<anyhow::Result<()>>>,
    pending_attachment_connection: Option<AttachmentConnection>,
}

pub(crate) struct RejectedRequest {
    serial: u64,
    reason: anyhow::Error,
    _decode_reservation: DecodeReservation,
    _inbound: CombinedPermit,
}

fn validate_stable_row_range(
    range: &std::ops::Range<StableRowIndex>,
    maximum: usize,
    kind: &'static str,
) -> anyhow::Result<usize> {
    let span = range
        .end
        .checked_sub(range.start)
        .filter(|span| *span >= 0)
        .and_then(|span| usize::try_from(span).ok())
        .ok_or_else(|| anyhow!("{kind} range is reversed or overflows"))?;
    if span > maximum {
        anyhow::bail!("{kind} range spans {span} rows, exceeding {maximum}");
    }
    Ok(span)
}

fn validate_terminal_size(size: &wezterm_term::TerminalSize) -> anyhow::Result<()> {
    use wezterm_runtime_admission::{
        MAX_SERVER_TERMINAL_COLS, MAX_SERVER_TERMINAL_PIXEL_HEIGHT,
        MAX_SERVER_TERMINAL_PIXEL_WIDTH, MAX_SERVER_TERMINAL_ROWS,
    };

    anyhow::ensure!(size.rows > 0, "terminal rows must be greater than zero");
    anyhow::ensure!(size.cols > 0, "terminal columns must be greater than zero");
    anyhow::ensure!(
        size.rows <= MAX_SERVER_TERMINAL_ROWS,
        "terminal rows {} exceed {}",
        size.rows,
        MAX_SERVER_TERMINAL_ROWS
    );
    anyhow::ensure!(
        size.cols <= MAX_SERVER_TERMINAL_COLS,
        "terminal columns {} exceed {}",
        size.cols,
        MAX_SERVER_TERMINAL_COLS
    );
    anyhow::ensure!(
        size.pixel_width <= MAX_SERVER_TERMINAL_PIXEL_WIDTH,
        "terminal pixel width {} exceeds {}",
        size.pixel_width,
        MAX_SERVER_TERMINAL_PIXEL_WIDTH
    );
    anyhow::ensure!(
        size.pixel_height <= MAX_SERVER_TERMINAL_PIXEL_HEIGHT,
        "terminal pixel height {} exceeds {}",
        size.pixel_height,
        MAX_SERVER_TERMINAL_PIXEL_HEIGHT
    );
    Ok(())
}

fn validate_request_semantics(
    pdu: &Pdu,
    pane_targets: PaneTargets,
    pane_exists: impl Fn(PaneId) -> bool,
    pane_tab_id: impl Fn(PaneId) -> Option<TabId>,
) -> anyhow::Result<()> {
    for pane_id in pane_targets.as_array().iter().flatten().copied() {
        if !pane_exists(pane_id) {
            anyhow::bail!("pane_id {} invalid", pane_id);
        }
    }
    match pdu {
        Pdu::SpawnV2(SpawnV2 {
            placement: TabSpawnPlacement::NewWindow { size, .. },
            ..
        })
        | Pdu::Resize(Resize { size, .. }) => validate_terminal_size(size)?,
        Pdu::SplitPane(SplitPane { split_request, .. }) => match split_request.size {
            SplitSize::Cells(cells) => anyhow::ensure!(
                (1..=MAX_ADJUST_PANE_CELLS).contains(&cells),
                "split cell size {cells} is outside 1..={MAX_ADJUST_PANE_CELLS}"
            ),
            SplitSize::Percent(percent) => anyhow::ensure!(
                (1..=100).contains(&percent),
                "split percentage {percent} is outside 1..=100"
            ),
        },
        Pdu::AdjustPaneSize(AdjustPaneSize { amount, .. }) => anyhow::ensure!(
            (1..=MAX_ADJUST_PANE_CELLS).contains(amount),
            "pane adjustment {amount} is outside 1..={MAX_ADJUST_PANE_CELLS}"
        ),
        Pdu::GetLines(GetLines { lines, .. }) => {
            anyhow::ensure!(
                lines.len() <= MAX_GET_LINES_RANGES,
                "GetLines has {} ranges, exceeding {}",
                lines.len(),
                MAX_GET_LINES_RANGES
            );
            let mut total = 0usize;
            for range in lines {
                total = total
                    .checked_add(validate_stable_row_range(
                        range,
                        MAX_GET_LINES_TOTAL_ROWS,
                        "GetLines",
                    )?)
                    .ok_or_else(|| anyhow!("GetLines row count overflow"))?;
                anyhow::ensure!(
                    total <= MAX_GET_LINES_TOTAL_ROWS,
                    "GetLines requests {total} rows, exceeding {MAX_GET_LINES_TOTAL_ROWS}"
                );
            }
        }
        Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
            pattern,
            range,
            limit,
            ..
        }) => {
            validate_search_pattern_bytes(pattern.len())?;
            validate_stable_row_range(range, MAX_SEARCH_RANGE_ROWS, "search")?;
            if let Some(limit) = limit {
                anyhow::ensure!(
                    *limit <= MAX_SEARCH_RESULTS,
                    "search result limit {limit} exceeds {MAX_SEARCH_RESULTS}"
                );
            }
        }
        Pdu::WriteToPane(WriteToPane { data, .. }) => {
            validate_pane_input_bytes(data.len(), "pane write")?
        }
        Pdu::SendPaste(SendPaste { data, .. }) => {
            let bytes = pane_paste_enveloped_byte_count(data.len())?;
            validate_pane_input_bytes(bytes, "pane paste")?;
        }
        Pdu::GetImageCell(GetImageCell { cell_idx, .. }) => anyhow::ensure!(
            *cell_idx < wezterm_runtime_admission::MAX_SERVER_TERMINAL_COLS,
            "image cell index {cell_idx} exceeds terminal column bound"
        ),
        _ => {}
    }
    match pdu {
        Pdu::Resize(Resize {
            containing_tab_id,
            pane_id,
            ..
        })
        | Pdu::SetPaneZoomed(SetPaneZoomed {
            containing_tab_id,
            pane_id,
            ..
        }) => anyhow::ensure!(
            pane_tab_id(*pane_id) == Some(*containing_tab_id),
            "pane {pane_id} is not owned by tab {containing_tab_id}"
        ),
        _ => {}
    }
    Ok(())
}

impl SessionHandler {
    pub fn new(
        to_write_tx: PduSender,
        policy: Arc<ServerPolicy>,
        admission: Arc<RuntimeAdmission>,
        executor: MainThreadExecutorHandle,
    ) -> anyhow::Result<Self> {
        if !Arc::ptr_eq(&admission, executor.admission()) {
            anyhow::bail!("session admission and executor admission must be identical");
        }
        policy.bind_admission(&admission)?;
        Ok(Self {
            to_write_tx,
            per_pane: HashMap::new(),
            bootstrap: BootstrapState::AwaitingClient { proxy: None },
            policy,
            admission,
            executor,
            tasks: FuturesUnordered::new(),
            pending_attachment_connection: None,
        })
    }

    pub(crate) fn client_request_phase(&self) -> ClientRequestPhase {
        self.bootstrap.request_phase()
    }

    fn authorize_request(&self, operation: RequestOperation, pdu: &Pdu) -> anyhow::Result<()> {
        match (&self.bootstrap, pdu) {
            (BootstrapState::AwaitingClient { .. }, Pdu::SetClientId(_)) => Ok(()),
            (
                BootstrapState::AwaitingClient { .. },
                Pdu::Ping(_)
                | Pdu::GetCodecVersion(_)
                | Pdu::GetBuildIdentity(_)
                | Pdu::GetTlsCreds(_),
            ) => self.policy.authorize_bootstrap(operation, pdu),
            (BootstrapState::AwaitingClient { .. }, _) => {
                anyhow::bail!(
                    "request {} requires an established client identity",
                    pdu.pdu_name()
                )
            }
            (BootstrapState::Established(_), Pdu::SetClientId(_)) => {
                anyhow::bail!("client identity is already established")
            }
            (BootstrapState::Established(identity), _) => {
                self.policy.authorize(identity, operation, pdu)
            }
        }
    }

    fn register_client(
        &mut self,
        mut client_id: ClientId,
        is_proxy: bool,
        resume_token: Option<AttachmentResumeToken>,
    ) -> anyhow::Result<(SetClientIdResponse, Option<Arc<ClientId>>)> {
        let BootstrapState::AwaitingClient { proxy } = &mut self.bootstrap else {
            anyhow::bail!("client identity is already established");
        };

        if is_proxy {
            anyhow::ensure!(
                resume_token.is_none(),
                "proxy registration cannot resume an attachment"
            );
            if proxy.is_some() {
                anyhow::bail!("proxy identity is already registered");
            }
            self.policy.authorize_proxy_registration(&client_id)?;
            proxy.replace(client_id);
            return Ok((SetClientIdResponse { resume_token: None }, None));
        }

        if let Some(proxy) = proxy.as_ref() {
            client_id.ssh_auth_sock = proxy.ssh_auth_sock.clone();
            // This presentation string is coupled with mux/src/ssh_agent.
            client_id.hostname = format!("{} (via proxy pid {})", client_id.hostname, proxy.pid);
        }
        let established =
            self.policy
                .establish_identity(proxy.as_ref(), client_id, resume_token)?;
        let issued_client_id = established
            .is_new
            .then(|| Arc::clone(established.identity.client_id()));
        let response = SetClientIdResponse {
            resume_token: Some(established.resume_token),
        };
        self.bootstrap = BootstrapState::Established(established.identity);
        self.pending_attachment_connection = Some(established.connection);
        Ok((response, issued_client_id))
    }

    pub(crate) fn take_attachment_connection(&mut self) -> Option<AttachmentConnection> {
        self.pending_attachment_connection.take()
    }

    fn schedule<F>(&mut self, future: F) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>> + 'static,
    {
        let identity = match &self.bootstrap {
            BootstrapState::Established(identity) => Some(identity.clone()),
            BootstrapState::AwaitingClient { .. } => None,
        };
        let policy = Arc::clone(&self.policy);
        let task = self.executor.local().try_spawn_local(async move {
            if let Some(identity) = identity.as_ref() {
                policy.ensure_current(identity)?;
            }
            future.await
        })?;
        self.tasks.push(task);
        Ok(())
    }

    fn schedule_pane_task<F>(
        &self,
        serial: u64,
        pane_id: PaneId,
        kind: PaneMutationKind,
        apply: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce() -> anyhow::Result<()> + 'static,
    {
        match Mux::get().try_enqueue_pane_mutation_local(pane_id, kind, apply) {
            Ok(()) => Ok(()),
            Err(error) => self.to_write_tx.send(
                Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: pane mutation was not accepted: {error:#}"),
                }),
                serial,
            ),
        }
    }

    pub async fn wait_for_task(&mut self) -> anyhow::Result<()> {
        if self.tasks.is_empty() {
            pending::<()>().await;
            unreachable!("pending future completed")
        }

        self.tasks
            .next()
            .await
            .expect("non-empty task set ended")??;
        Ok(())
    }

    pub async fn cancel_and_join_tasks(&mut self) -> anyhow::Result<()> {
        for task in self.tasks.iter_mut() {
            task.cancel();
        }
        while let Some(joined) = self.tasks.next().await {
            match joined {
                Ok(result) => result?,
                Err(err) if err.is_cancelled() => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn per_pane(&mut self, pane_id: PaneId) -> Arc<Mutex<PerPane>> {
        Arc::clone(
            self.per_pane
                .entry(pane_id)
                .or_insert_with(|| Arc::new(Mutex::new(PerPane::default()))),
        )
    }

    pub(crate) fn remove_pane_projection(&mut self, pane_id: PaneId) {
        self.per_pane.remove(&pane_id);
    }

    pub(crate) fn schedule_pane_alert(
        &mut self,
        pane_id: PaneId,
        alert: Alert,
    ) -> anyhow::Result<()> {
        if Mux::get().get_pane(pane_id).is_none() {
            self.remove_pane_projection(pane_id);
            return Ok(());
        }

        self.per_pane(pane_id)
            .lock()
            .unwrap()
            .notifications
            .push(alert);
        self.schedule_pane_push(pane_id)
    }

    pub fn schedule_pane_push(&mut self, pane_id: PaneId) -> anyhow::Result<()> {
        if Mux::get().get_pane(pane_id).is_none() {
            self.remove_pane_projection(pane_id);
            return Ok(());
        }

        let permit = match self.admission.try_count(CountClass::PanePushJob, 1) {
            Ok(permit) => permit,
            Err(err) => {
                return Err(err.into());
            }
        };
        let sender = self.to_write_tx.clone();
        let per_pane = self.per_pane(pane_id);
        self.schedule(async move {
            let _permit = permit;
            let mux = Mux::get();
            let Some(pane) = mux.get_pane(pane_id) else {
                return Ok(());
            };
            maybe_push_pane_changes(&pane, sender, per_pane)?;
            Ok(())
        })
    }

    pub(crate) fn admit_request(
        &self,
        decoded: AdmittedDecodedPdu,
        inbound: CombinedPermit,
    ) -> Result<AdmittedAuthorizedRequest, RejectedRequest> {
        let (serial, pdu, decode_reservation) = decoded.into_parts();
        let admission = (|| {
            let metadata = pdu.request_metadata()?;
            validate_request_semantics(
                &pdu,
                metadata.pane_targets,
                |pane_id| Mux::get().get_pane(pane_id).is_some(),
                |pane_id| {
                    Mux::get()
                        .resolve_pane_id(pane_id)
                        .map(|(_, _, tab_id)| tab_id)
                },
            )?;
            let split_domain_id = match &pdu {
                Pdu::SplitPane(split) => Some(resolve_split_spawn_domain_id(split)?),
                _ => None,
            };
            self.authorize_request(metadata.operation, &pdu)
                .context("request authorization denied")?;
            let identity = match &self.bootstrap {
                BootstrapState::Established(identity) => Some(identity.clone()),
                BootstrapState::AwaitingClient { .. } => None,
            };
            Ok::<_, anyhow::Error>((metadata.operation, identity, split_domain_id))
        })();
        match admission {
            Ok((operation, identity, split_domain_id)) => Ok(AdmittedAuthorizedRequest::new(
                serial,
                pdu,
                operation,
                identity,
                split_domain_id,
                decode_reservation,
                inbound,
            )),
            Err(reason) => Err(RejectedRequest {
                serial,
                reason,
                _decode_reservation: decode_reservation,
                _inbound: inbound,
            }),
        }
    }

    pub(crate) fn reject_request(&self, rejected: RejectedRequest) -> anyhow::Result<()> {
        self.to_write_tx.send(
            Pdu::ErrorResponse(ErrorResponse {
                reason: format!("Error: {:#}", rejected.reason),
            }),
            rejected.serial,
        )
    }

    pub fn process_one(&mut self, request: AdmittedAuthorizedRequest) -> anyhow::Result<()> {
        let AdmittedAuthorizedRequest {
            serial,
            pdu,
            operation: _operation,
            identity: authorized_identity,
            split_domain_id,
            decode_reservation,
            inbound,
        } = request;
        let authorized_client_id = authorized_identity
            .as_ref()
            .map(|identity| Arc::clone(identity.client_id()));
        if let Some(identity) = authorized_identity.as_ref() {
            self.policy.ensure_current(identity)?;
        }
        let start = Instant::now();
        let sender = self.to_write_tx.clone();
        let pdu_name = pdu.pdu_name();

        let send_response = move |result: anyhow::Result<Pdu>| {
            let _inbound = inbound;
            let _decode_reservation = decode_reservation;
            let pdu = match result {
                Ok(pdu) => pdu,
                Err(err) => Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: {err:#}"),
                }),
            };
            log::trace!("{} processing time {:?}", serial, start.elapsed());
            sender.send(pdu, serial)
        };

        fn catch<F, SND>(f: F, send_response: SND) -> anyhow::Result<()>
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: FnOnce(anyhow::Result<Pdu>) -> anyhow::Result<()>,
        {
            send_response(f())
        }

        // Caller-controlled input may update mux liveness only after the request has crossed the
        // server-authoritative bootstrap and authorization gate.  Keeping this below
        // `authorize_request` is part of the no-side-effects-before-authorization contract.
        if let Some(client_id) = authorized_client_id.as_deref() {
            if pdu.is_user_input() {
                Mux::get().client_had_input(client_id);
            }
        }

        match pdu {
            Pdu::Ping(Ping {}) => send_response(Ok(Pdu::Pong(Pong {}))),
            Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id,
                workspace,
            }) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        let mut window = mux
                            .get_window_mut(window_id)
                            .ok_or_else(|| anyhow!("window {} is invalid", window_id))?;
                        window.set_workspace(&workspace);
                        Ok(Pdu::UnitResponse(UnitResponse {}))
                    },
                    send_response,
                )
            }),
            Pdu::SetClientId(SetClientId {
                client_id,
                is_proxy,
                resume_token,
            }) => {
                let response = match self.register_client(client_id, is_proxy, resume_token) {
                    Ok((response, Some(client_id))) => {
                        self.schedule(async move {
                            let mux = Mux::get();
                            mux.register_client(client_id);
                            Ok(())
                        })?;
                        response
                    }
                    Ok((response, None)) => response,
                    Err(err) => {
                        return send_response(Err(err.context("client registration denied")));
                    }
                };
                send_response(Ok(Pdu::SetClientIdResponse(response)))
            }
            Pdu::SetFocusedPane(SetFocusedPane { pane_id }) => {
                let client_id = authorized_client_id
                    .clone()
                    .expect("focused-pane mutation requires an authorized identity");
                self.schedule(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(Some(client_id));

                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;

                            let (_domain_id, window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
                            {
                                let mut window =
                                    mux.get_window_mut(window_id).ok_or_else(|| {
                                        anyhow::anyhow!("window {window_id} not found")
                                    })?;
                                let tab_idx = window.idx_by_id(tab_id).ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "tab {tab_id} isn't really in window {window_id}!?"
                                    )
                                })?;
                                window.save_and_then_set_active(tab_idx);
                            }
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;
                            tab.set_active_pane(&pane);

                            mux.record_focus_for_current_identity(pane_id);
                            mux.notify(mux::MuxNotification::PaneFocused(pane_id));

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
            }
            Pdu::GetClientList(GetClientList) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        let clients = mux.iter_clients();
                        Ok(Pdu::GetClientListResponse(GetClientListResponse {
                            clients,
                        }))
                    },
                    send_response,
                )
            }),
            Pdu::ListPanes(ListPanes {}) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        let mut tabs = vec![];
                        let mut tab_titles = vec![];
                        let mut window_titles = HashMap::new();
                        for window_id in mux.iter_windows().into_iter() {
                            let window = mux.get_window(window_id).unwrap();
                            window_titles.insert(window_id, window.get_title().to_string());
                            for tab in window.iter() {
                                tabs.push(tab.codec_pane_tree());
                                tab_titles.push(tab.get_title());
                            }
                        }
                        log::trace!("ListPanes {tabs:#?} {tab_titles:?}");
                        Ok(Pdu::ListPanesResponse(ListPanesResponse {
                            tabs,
                            tab_titles,
                            window_titles,
                        }))
                    },
                    send_response,
                )
            }),

            Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        mux.rename_workspace(&old_workspace, &new_workspace);
                        Ok(Pdu::UnitResponse(UnitResponse {}))
                    },
                    send_response,
                )
            }),

            Pdu::WriteToPane(WriteToPane { pane_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                let bytes = data.len();
                self.schedule_pane_task(
                    serial,
                    pane_id,
                    PaneMutationKind::Write { bytes },
                    move || {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let pane = mux
                                    .get_pane(pane_id)
                                    .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                                pane.writer().write_all(&data)?;
                                maybe_push_pane_changes(&pane, sender, per_pane)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        )
                    },
                )
            }
            Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                pane_id,
                erase_mode,
            }) => self.schedule_pane_task(
                serial,
                pane_id,
                PaneMutationKind::EraseScrollback,
                move || {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.erase_scrollback(erase_mode);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                },
            ),
            Pdu::KillPane(KillPane { pane_id }) => {
                self.schedule_pane_task(serial, pane_id, PaneMutationKind::Kill, move || {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            // Killing a live child causes its reader to schedule canonical pruning
                            // after EOF. A held pane has already reached EOF, so no later lifecycle
                            // event exists; pruning here observes its killed marker and removes it.
                            // The prune owner only removes panes that report dead, so a live reader
                            // is never joined synchronously from this request thread.
                            pane.kill();
                            mux.prune_dead_windows();
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
            }
            Pdu::SendPaste(SendPaste { pane_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                let bytes = pane_paste_enveloped_byte_count(data.len())?;
                self.schedule_pane_task(
                    serial,
                    pane_id,
                    PaneMutationKind::Input { bytes },
                    move || {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let pane = mux
                                    .get_pane(pane_id)
                                    .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                                pane.send_paste(&data)?;
                                maybe_push_pane_changes(&pane, sender, per_pane)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        )
                    },
                )
            }

            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern,
                range,
                limit,
            }) => {
                use mux::pane::Pattern;
                let limit = Some(limit.unwrap_or(MAX_SEARCH_RESULTS));

                async fn do_search(
                    pane_id: TabId,
                    pattern: Pattern,
                    range: std::ops::Range<StableRowIndex>,
                    limit: Option<u32>,
                ) -> anyhow::Result<Pdu> {
                    let mux = Mux::get();
                    let pane = mux
                        .get_pane(pane_id)
                        .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                    pane.search(pattern, range, limit).await.map(|results| {
                        Pdu::SearchScrollbackResponse(SearchScrollbackResponse { results })
                    })
                }

                self.schedule(async move {
                    let result = do_search(pane_id, pattern, range, limit).await;
                    send_response(result)
                })
            }

            Pdu::SetPaneZoomed(SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            }) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        let pane = mux
                            .get_pane(pane_id)
                            .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                        let tab = mux
                            .get_tab(containing_tab_id)
                            .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                        match tab.get_zoomed_pane() {
                            Some(p) => {
                                let is_zoomed = p.pane_id() == pane_id;
                                if is_zoomed != zoomed {
                                    tab.set_zoomed(false);
                                    if zoomed {
                                        tab.set_active_pane(&pane);
                                        tab.set_zoomed(zoomed);
                                    }
                                }
                            }
                            None => {
                                if zoomed {
                                    tab.set_active_pane(&pane);
                                    tab.set_zoomed(zoomed);
                                }
                            }
                        }
                        Ok(Pdu::UnitResponse(UnitResponse {}))
                    },
                    send_response,
                )
            }),

            Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction }) => {
                self.schedule(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            let panes = tab.iter_panes_ignoring_zoom();
                            let pane_id = tab
                                .get_pane_direction(direction, true)
                                .map(|pane_index| panes[pane_index].pane.pane_id());

                            Ok(Pdu::GetPaneDirectionResponse(GetPaneDirectionResponse {
                                pane_id,
                            }))
                        },
                        send_response,
                    )
                })
            }

            Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction }) => self
                .schedule(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            tab.activate_pane_direction(direction);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                }),

            Pdu::Resize(Resize {
                containing_tab_id,
                pane_id,
                size,
            }) => self.schedule_pane_task(serial, pane_id, PaneMutationKind::Resize, move || {
                catch(
                    move || {
                        let mux = Mux::get();
                        let pane = mux
                            .get_pane(pane_id)
                            .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                        pane.resize(size)?;
                        let tab = mux
                            .get_tab(containing_tab_id)
                            .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                        tab.rebuild_splits_sizes_from_contained_panes();
                        Ok(Pdu::UnitResponse(UnitResponse {}))
                    },
                    send_response,
                )
            }),

            Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                self.schedule_pane_task(
                    serial,
                    pane_id,
                    PaneMutationKind::Input {
                        bytes: MAX_ENCODED_INPUT_EVENT_BYTES,
                    },
                    move || {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let pane = mux
                                    .get_pane(pane_id)
                                    .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                                pane.key_down(event.key, event.modifiers)?;

                                // For a key press, we want to always send back the
                                // cursor position so that the predictive echo doesn't
                                // leave the cursor in the wrong place
                                let mut per_pane = per_pane.lock().unwrap();
                                if let Some(resp) =
                                    per_pane.compute_changes(&pane, Some(input_serial))
                                {
                                    sender.send(Pdu::GetPaneRenderChangesResponse(resp), 0)?;
                                }
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        )
                    },
                )
            }
            Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                self.schedule_pane_task(
                    serial,
                    pane_id,
                    PaneMutationKind::Input {
                        bytes: MAX_ENCODED_INPUT_EVENT_BYTES,
                    },
                    move || {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let pane = mux
                                    .get_pane(pane_id)
                                    .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                                pane.mouse_event(event)?;
                                maybe_push_pane_changes(&pane, sender, per_pane)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        )
                    },
                )
            }

            Pdu::SpawnV2(spawn) => {
                let client_id = authorized_client_id
                    .clone()
                    .expect("spawn mutation requires an authorized identity");
                let spawn_permit = match self.policy.reserve_spawn() {
                    Ok(permit) => permit,
                    Err(error) => return send_response(Err(error)),
                };
                self.schedule(async move {
                    let _spawn_permit = spawn_permit;
                    let result = domain_spawn_v2(spawn, client_id).await;
                    send_response(result)
                })
            }

            Pdu::SplitPane(split) => {
                let client_id = authorized_client_id
                    .clone()
                    .expect("split mutation requires an authorized identity");
                let domain = split_domain_id
                    .map(MuxSpawnTabDomain::DomainId)
                    .expect("split requests resolve their target domain before authorization");
                self.schedule(async move {
                    let result = split_pane(split, domain, client_id).await;
                    send_response(result)
                })
            }

            Pdu::MovePaneToNewTab(request) => {
                let client_id = authorized_client_id
                    .clone()
                    .expect("move mutation requires an authorized identity");
                self.schedule(async move {
                    let result = move_pane(request, client_id).await;
                    send_response(result)
                })
            }

            Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }) => self
                .schedule(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let cursor_position = pane.get_cursor_position();
                            let dimensions = pane.get_dimensions();
                            Ok(Pdu::GetPaneRenderableDimensionsResponse(
                                GetPaneRenderableDimensionsResponse {
                                    pane_id,
                                    cursor_position,
                                    dimensions,
                                },
                            ))
                        },
                        send_response,
                    )
                }),

            Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id, .. }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                self.schedule(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let is_alive = match mux.get_pane(pane_id) {
                                Some(pane) => {
                                    maybe_push_pane_changes(&pane, sender, per_pane)?;
                                    true
                                }
                                None => false,
                            };
                            Ok(Pdu::LivenessResponse(LivenessResponse {
                                pane_id,
                                is_alive,
                            }))
                        },
                        send_response,
                    )
                })
            }

            Pdu::GetLines(GetLines { pane_id, lines }) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        let pane = mux
                            .get_pane(pane_id)
                            .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                        let mut lines_and_indices = vec![];

                        for range in lines {
                            let (first_row, lines) = pane.get_lines(range);
                            for (idx, mut line) in lines.into_iter().enumerate() {
                                let stable_row = first_row + idx as StableRowIndex;
                                line.compress_for_scrollback();
                                lines_and_indices.push((stable_row, line));
                            }
                        }
                        Ok(Pdu::GetLinesResponse(GetLinesResponse {
                            pane_id,
                            lines: lines_and_indices.into(),
                        }))
                    },
                    send_response,
                )
            }),

            Pdu::GetImageCell(GetImageCell {
                pane_id,
                line_idx,
                cell_idx,
                data_hash,
            }) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        let mut data = None;

                        let pane = mux
                            .get_pane(pane_id)
                            .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                        let (_, lines) = pane.get_lines(line_idx..line_idx + 1);
                        'found_data: for line in lines {
                            if let Some(cell) = line.get_cell(cell_idx) {
                                if let Some(images) = cell.attrs().images() {
                                    for im in images {
                                        if im.image_data().hash() == data_hash {
                                            data.replace(im.image_data().clone());
                                            break 'found_data;
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Pdu::GetImageCellResponse(GetImageCellResponse {
                            pane_id,
                            data,
                        }))
                    },
                    send_response,
                )
            }),

            Pdu::GetCodecVersion(_) => {
                match std::env::current_exe().context("resolving current_exe") {
                    Err(err) => send_response(Err(err)),
                    Ok(executable_path) => {
                        send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                            codec_vers: CODEC_VERSION,
                            version_string: config::wezterm_version().to_owned(),
                            executable_path,
                            config_file_path: std::env::var_os("WEZTERM_CONFIG_FILE")
                                .map(Into::into),
                        })))
                    }
                }
            }

            Pdu::GetBuildIdentity(_) => send_response(Ok(Pdu::GetBuildIdentityResponse(
                GetBuildIdentityResponse {
                    identity: self.policy.build_identity().clone(),
                },
            ))),

            Pdu::GetTlsCreds(_) => catch(
                move || {
                    let client_cert_pem = PKI.generate_client_cert()?;
                    let ca_cert_pem = PKI.ca_pem_string()?;
                    Ok(Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                        client_cert_pem,
                        ca_cert_pem,
                    }))
                },
                send_response,
            ),
            Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
                self.schedule(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("no such window {window_id}"))?;

                            window.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
            }
            Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        let tab = mux
                            .get_tab(tab_id)
                            .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;

                        tab.set_title(&title);

                        Ok(Pdu::UnitResponse(UnitResponse {}))
                    },
                    send_response,
                )
            }),
            Pdu::SetPalette(SetPalette { pane_id, palette }) => {
                self.schedule_pane_task(serial, pane_id, PaneMutationKind::SetPalette, move || {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                            match pane.get_config() {
                                Some(config) => match config.downcast_ref::<TermConfig>() {
                                    Some(tc) => tc.set_client_palette(*palette),
                                    None => {
                                        log::error!(
                                            "pane {pane_id} doesn't \
                                                have TermConfig as its config! \
                                                Ignoring client palette update"
                                        );
                                    }
                                },
                                None => {
                                    let config = TermConfig::new();
                                    config.set_client_palette(*palette);
                                    pane.set_config(Arc::new(config));
                                }
                            }

                            mux.notify(MuxNotification::Alert {
                                pane_id,
                                alert: Alert::PaletteChanged,
                            });

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
            }

            Pdu::AdjustPaneSize(AdjustPaneSize {
                pane_id,
                direction,
                amount,
            }) => self.schedule(async move {
                catch(
                    move || {
                        let mux = Mux::get();
                        let (_pane_domain_id, _window_id, tab_id) = mux
                            .resolve_pane_id(pane_id)
                            .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

                        let tab = match mux.get_tab(tab_id) {
                            Some(tab) => tab,
                            None => {
                                return Err(anyhow!("Failed to retrieve tab with ID {}", tab_id))
                            }
                        };

                        tab.adjust_pane_size(direction, amount);
                        Ok(Pdu::UnitResponse(UnitResponse {}))
                    },
                    send_response,
                )
            }),

            Pdu::ServiceDrainRequest(ServiceDrainRequest { action }) => {
                let Some(identity) = authorized_identity.clone() else {
                    return send_response(Err(anyhow!(
                        "service drain requires an established attachment"
                    )));
                };
                let policy = Arc::clone(&self.policy);
                self.schedule(async move {
                    send_response(
                        policy
                            .apply_service_drain(&identity, action)
                            .await
                            .map(Pdu::ServiceDrainResult),
                    )
                })
            }

            Pdu::Invalid { .. } => send_response(Err(anyhow!("invalid PDU {pdu_name}"))),
            Pdu::Pong { .. }
            | Pdu::ListPanesResponse { .. }
            | Pdu::SetClipboard { .. }
            | Pdu::NotifyAlert { .. }
            | Pdu::SpawnResponse { .. }
            | Pdu::GetPaneRenderChangesResponse { .. }
            | Pdu::UnitResponse { .. }
            | Pdu::LivenessResponse { .. }
            | Pdu::GetPaneDirectionResponse { .. }
            | Pdu::SearchScrollbackResponse { .. }
            | Pdu::GetLinesResponse { .. }
            | Pdu::GetCodecVersionResponse { .. }
            | Pdu::GetBuildIdentityResponse { .. }
            | Pdu::WindowWorkspaceChanged { .. }
            | Pdu::GetTlsCredsResponse { .. }
            | Pdu::GetClientListResponse { .. }
            | Pdu::PaneRemoved { .. }
            | Pdu::PaneFocused { .. }
            | Pdu::TabResized { .. }
            | Pdu::GetImageCellResponse { .. }
            | Pdu::MovePaneToNewTabResponse { .. }
            | Pdu::TabAddedToWindow { .. }
            | Pdu::GetPaneRenderableDimensionsResponse { .. }
            | Pdu::SetClientIdResponse { .. }
            | Pdu::AttachRejected { .. }
            | Pdu::ServiceDrainResult { .. }
            | Pdu::ErrorResponse { .. } => {
                send_response(Err(anyhow!("expected a request, got {pdu_name}")))
            }
        }
    }
}

fn resolve_split_spawn_domain_id(split: &SplitPane) -> anyhow::Result<usize> {
    match split.domain {
        SplitSpawnDomain::TargetPaneDomain => {
            let (domain_id, _, _) = Mux::get()
                .resolve_pane_id(split.target_pane_id)
                .ok_or_else(|| anyhow!("pane_id {} invalid", split.target_pane_id))?;
            Ok(domain_id)
        }
    }
}

fn command_builder_from_wire(command: EnvironmentFreeCommand) -> CommandBuilder {
    match command {
        EnvironmentFreeCommand::DefaultLoginShell => CommandBuilder::new_default_prog(),
        EnvironmentFreeCommand::Program { program, args } => {
            let mut command = CommandBuilder::new(program.as_str());
            command.args(args);
            command
        }
    }
}

fn mux_tab_spawn_domain(domain: TabSpawnDomain) -> MuxSpawnTabDomain {
    match domain {
        TabSpawnDomain::DefaultDomain => MuxSpawnTabDomain::DefaultDomain,
        TabSpawnDomain::DomainName(name) => MuxSpawnTabDomain::DomainName(name),
        TabSpawnDomain::DomainId(id) => MuxSpawnTabDomain::DomainId(id),
    }
}

async fn split_pane(
    split: SplitPane,
    domain: MuxSpawnTabDomain,
    client_id: Arc<ClientId>,
) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(Some(client_id));

    let (_pane_domain_id, window_id, tab_id) = mux
        .resolve_pane_id(split.target_pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", split.target_pane_id))?;

    let source = match split.source {
        SplitSpawnSource::MovePane { pane_id } => SplitSource::MovePane(pane_id),
        SplitSpawnSource::Spawn {
            command,
            command_dir,
        } => SplitSource::Spawn {
            command: Some(command_builder_from_wire(command)),
            command_dir,
        },
    };

    let (pane, size) = mux
        .split_pane(split.target_pane_id, split.split_request, source, domain)
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id,
        window_id,
        size,
    }))
}

async fn domain_spawn_v2(spawn: SpawnV2, client_id: Arc<ClientId>) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(Some(client_id));
    let (window_id, size, workspace) = match spawn.placement {
        TabSpawnPlacement::ExistingWindow { window_id } => {
            let window = mux
                .get_window(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            (
                Some(window_id),
                tab.get_size(),
                window.get_workspace().to_string(),
            )
        }
        TabSpawnPlacement::NewWindow { size, workspace } => (None, size, workspace),
    };

    let (tab, pane, window_id) = mux
        .spawn_tab_or_window(
            window_id,
            mux_tab_spawn_domain(spawn.domain),
            Some(command_builder_from_wire(spawn.command)),
            spawn.command_dir,
            size,
            None, // optional current pane_id
            workspace,
            None, // optional gui window position
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id: tab.tab_id(),
        window_id,
        size: tab.get_size(),
    }))
}

async fn move_pane(request: MovePaneToNewTab, client_id: Arc<ClientId>) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(Some(client_id));

    let (tab, window_id) = mux
        .move_pane_to_new_tab(
            request.pane_id,
            request.window_id,
            request.workspace_for_new_window,
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::MovePaneToNewTabResponse(MovePaneToNewTabResponse {
        tab_id: tab.tab_id(),
        window_id,
    }))
}

#[cfg(test)]
mod owned_task_tests {
    use super::*;
    use mux::tab::SplitRequest;
    use wezterm_runtime_admission::{CountClass, RuntimeAdmission, RuntimeRole};

    fn handler() -> (
        Arc<RuntimeAdmission>,
        promise::spawn::SimpleExecutor,
        SessionHandler,
    ) {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let executor = promise::spawn::SimpleExecutor::new(Arc::clone(&admission));
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
        let sender = PduSender::new(|_, _| Ok(()));
        let handler = SessionHandler::new(
            sender,
            policy,
            Arc::clone(&admission),
            MainThreadExecutorHandle::from_simple(executor.handle()),
        )
        .unwrap();
        (admission, executor, handler)
    }

    #[test]
    fn wire_commands_are_rebuilt_with_agent_owned_process_policy() {
        let login = command_builder_from_wire(EnvironmentFreeCommand::DefaultLoginShell);
        assert!(login.is_default_prog());
        assert!(login.get_controlling_tty());
        assert_eq!(login.iter_extra_env_as_str().count(), 0);

        let program = command_builder_from_wire(
            EnvironmentFreeCommand::try_from_argv(["bash", "-lc", "echo ready"]).unwrap(),
        );
        assert_eq!(
            program.get_argv(),
            &vec![
                std::ffi::OsString::from("bash"),
                std::ffi::OsString::from("-lc"),
                std::ffi::OsString::from("echo ready"),
            ]
        );
        assert!(program.get_controlling_tty());
        assert_eq!(program.iter_extra_env_as_str().count(), 0);
    }

    fn terminal_size(
        rows: usize,
        cols: usize,
        pixel_width: usize,
        pixel_height: usize,
    ) -> wezterm_term::TerminalSize {
        wezterm_term::TerminalSize {
            rows,
            cols,
            pixel_width,
            pixel_height,
            dpi: 0,
        }
    }

    fn spawn_with_size(size: wezterm_term::TerminalSize) -> Pdu {
        Pdu::SpawnV2(SpawnV2 {
            domain: TabSpawnDomain::DefaultDomain,
            placement: TabSpawnPlacement::NewWindow {
                size,
                workspace: "default".to_owned(),
            },
            command: EnvironmentFreeCommand::DefaultLoginShell,
            command_dir: None,
        })
    }

    fn resize_with_size(size: wezterm_term::TerminalSize) -> Pdu {
        Pdu::Resize(Resize {
            containing_tab_id: 1,
            pane_id: 1,
            size,
        })
    }

    fn assert_spawn_and_resize_size(size: wezterm_term::TerminalSize, expected: bool) {
        for request in [spawn_with_size(size), resize_with_size(size)] {
            assert_eq!(
                validate_request_semantics(&request, |_| true, |_| Some(1)).is_ok(),
                expected,
                "unexpected validation result for {request:?}"
            );
        }
    }

    fn split_with_size(size: SplitSize) -> Pdu {
        Pdu::SplitPane(SplitPane {
            target_pane_id: 1,
            split_request: SplitRequest {
                direction: mux::tab::SplitDirection::Horizontal,
                target_is_second: true,
                top_level: false,
                size,
            },
            domain: SplitSpawnDomain::TargetPaneDomain,
            source: SplitSpawnSource::Spawn {
                command: EnvironmentFreeCommand::DefaultLoginShell,
                command_dir: None,
            },
        })
    }

    fn adjust_with_amount(amount: usize) -> Pdu {
        Pdu::AdjustPaneSize(AdjustPaneSize {
            pane_id: 1,
            direction: config::keyassignment::PaneDirection::Left,
            amount,
        })
    }

    fn get_lines(lines: Vec<std::ops::Range<StableRowIndex>>) -> Pdu {
        Pdu::GetLines(GetLines { pane_id: 1, lines })
    }

    fn search(
        pattern_len: usize,
        range: std::ops::Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> Pdu {
        Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
            pane_id: 1,
            pattern: mux::pane::Pattern::Regex("x".repeat(pattern_len)),
            range,
            limit,
        })
    }

    fn assert_valid(request: &Pdu) {
        assert!(
            validate_request_semantics(request, |_| true, |_| Some(1)).is_ok(),
            "expected request to be valid: {:?}",
            request
        );
    }

    fn assert_rejected(request: &Pdu) {
        assert!(
            validate_request_semantics(request, |_| true, |_| Some(1)).is_err(),
            "expected request to be rejected: {:?}",
            request
        );
    }

    #[test]
    fn spawn_and_resize_geometry_accept_only_the_bounded_terminal_envelope() {
        use wezterm_runtime_admission::{
            MAX_SERVER_TERMINAL_COLS, MAX_SERVER_TERMINAL_PIXEL_HEIGHT,
            MAX_SERVER_TERMINAL_PIXEL_WIDTH, MAX_SERVER_TERMINAL_ROWS,
        };

        let minimum = terminal_size(1, 1, 0, 0);
        let maximum = terminal_size(
            MAX_SERVER_TERMINAL_ROWS,
            MAX_SERVER_TERMINAL_COLS,
            MAX_SERVER_TERMINAL_PIXEL_WIDTH,
            MAX_SERVER_TERMINAL_PIXEL_HEIGHT,
        );
        assert_spawn_and_resize_size(minimum, true);
        assert_spawn_and_resize_size(maximum, true);

        // Rows and columns have a non-zero lower bound; pixel dimensions intentionally do not.
        assert_spawn_and_resize_size(terminal_size(0, 1, 0, 0), false);
        assert_spawn_and_resize_size(terminal_size(1, 0, 0, 0), false);
        assert_spawn_and_resize_size(terminal_size(1, 1, 0, 0), true);

        for size in [
            terminal_size(MAX_SERVER_TERMINAL_ROWS + 1, 1, 0, 0),
            terminal_size(1, MAX_SERVER_TERMINAL_COLS + 1, 0, 0),
            terminal_size(1, 1, MAX_SERVER_TERMINAL_PIXEL_WIDTH + 1, 0),
            terminal_size(1, 1, 0, MAX_SERVER_TERMINAL_PIXEL_HEIGHT + 1),
            terminal_size(usize::MAX, 1, 0, 0),
            terminal_size(1, usize::MAX, 0, 0),
            terminal_size(1, 1, usize::MAX, 0),
            terminal_size(1, 1, 0, usize::MAX),
        ] {
            assert_spawn_and_resize_size(size, false);
        }

        for _ in 0..3 {
            assert_spawn_and_resize_size(maximum, true);
        }
    }

    #[test]
    fn split_and_adjust_sizes_reject_zero_overflow_and_unbounded_values() {
        for request in [
            split_with_size(SplitSize::Cells(1)),
            split_with_size(SplitSize::Cells(MAX_ADJUST_PANE_CELLS)),
            split_with_size(SplitSize::Percent(1)),
            split_with_size(SplitSize::Percent(100)),
            adjust_with_amount(1),
            adjust_with_amount(MAX_ADJUST_PANE_CELLS),
        ] {
            assert_valid(&request);
        }

        for request in [
            split_with_size(SplitSize::Cells(0)),
            split_with_size(SplitSize::Cells(MAX_ADJUST_PANE_CELLS + 1)),
            split_with_size(SplitSize::Cells(usize::MAX)),
            split_with_size(SplitSize::Percent(0)),
            split_with_size(SplitSize::Percent(101)),
            split_with_size(SplitSize::Percent(u8::MAX)),
            adjust_with_amount(0),
            adjust_with_amount(MAX_ADJUST_PANE_CELLS + 1),
            adjust_with_amount(usize::MAX),
        ] {
            assert_rejected(&request);
        }

        for _ in 0..3 {
            assert_valid(&split_with_size(SplitSize::Cells(MAX_ADJUST_PANE_CELLS)));
            assert_valid(&adjust_with_amount(MAX_ADJUST_PANE_CELLS));
        }
    }

    #[test]
    fn line_ranges_enforce_per_range_total_and_count_bounds_without_overflow() {
        assert_valid(&get_lines(vec![0..0]));
        assert_valid(&get_lines(vec![
            0..MAX_GET_LINES_TOTAL_ROWS as StableRowIndex,
        ]));
        assert_rejected(&get_lines(vec![
            0..MAX_GET_LINES_TOTAL_ROWS as StableRowIndex + 1,
        ]));
        assert_rejected(&get_lines(vec![10..0]));

        // StableRowIndex is isize. MIN..MAX makes checked_sub overflow without allocating input.
        assert_rejected(&get_lines(vec![StableRowIndex::MIN..StableRowIndex::MAX]));

        let exact_count = vec![0..0; MAX_GET_LINES_RANGES];
        assert_valid(&get_lines(exact_count));
        assert_rejected(&get_lines(vec![0..0; MAX_GET_LINES_RANGES + 1]));

        let repeated_maximum = vec![0..MAX_GET_LINES_TOTAL_ROWS as StableRowIndex; 2];
        assert_rejected(&get_lines(repeated_maximum));

        let exact_total = vec![0..16; MAX_GET_LINES_RANGES];
        assert_valid(&get_lines(exact_total));
    }

    #[test]
    fn search_bounds_cover_empty_exact_overflow_and_repeated_maximum_requests() {
        assert_valid(&search(0, 0..0, Some(0)));
        assert_valid(&search(
            MAX_SEARCH_PATTERN_BYTES,
            0..MAX_SEARCH_RANGE_ROWS as StableRowIndex,
            Some(MAX_SEARCH_RESULTS),
        ));

        assert_rejected(&search(
            MAX_SEARCH_PATTERN_BYTES + 1,
            0..MAX_SEARCH_RANGE_ROWS as StableRowIndex,
            Some(MAX_SEARCH_RESULTS),
        ));
        assert_rejected(&search(
            0,
            0..MAX_SEARCH_RANGE_ROWS as StableRowIndex + 1,
            Some(MAX_SEARCH_RESULTS),
        ));
        assert_rejected(&search(
            0,
            StableRowIndex::MIN..StableRowIndex::MAX,
            Some(MAX_SEARCH_RESULTS),
        ));
        assert_rejected(&search(0, 0..0, Some(MAX_SEARCH_RESULTS + 1)));
        assert_rejected(&search(0, 0..0, Some(u32::MAX)));
        assert!(validate_search_pattern_bytes(usize::MAX).is_err());

        for _ in 0..3 {
            assert_valid(&search(
                MAX_SEARCH_PATTERN_BYTES,
                0..MAX_SEARCH_RANGE_ROWS as StableRowIndex,
                Some(MAX_SEARCH_RESULTS),
            ));
        }
    }

    #[test]
    fn pane_input_sizes_reject_unbounded_counts_and_paste_count_overflow() {
        let maximum = wezterm_runtime_admission::MAX_PANE_INPUT_BYTES_PER_PANE;
        assert_valid(&Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![],
        }));
        assert_valid(&Pdu::SendPaste(SendPaste {
            pane_id: 1,
            data: String::new(),
        }));

        for _ in 0..3 {
            assert_valid(&Pdu::WriteToPane(WriteToPane {
                pane_id: 1,
                data: vec![0; maximum],
            }));
            assert_valid(&Pdu::SendPaste(SendPaste {
                pane_id: 1,
                data: "x".repeat(maximum - BRACKETED_PASTE_ENVELOPE_BYTES),
            }));
        }

        assert_rejected(&Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![0; maximum + 1],
        }));
        assert_rejected(&Pdu::SendPaste(SendPaste {
            pane_id: 1,
            data: "x".repeat(maximum - BRACKETED_PASTE_ENVELOPE_BYTES + 1),
        }));

        assert!(validate_pane_input_bytes(usize::MAX, "pane write").is_err());
        assert!(pane_paste_enveloped_byte_count(usize::MAX).is_err());
    }

    #[test]
    fn pane_and_tab_relationship_is_validated_before_authorization() {
        let request = Pdu::Resize(Resize {
            containing_tab_id: 9,
            pane_id: 7,
            size: wezterm_term::TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            },
        });
        assert!(validate_request_semantics(&request, |_| true, |_| Some(8)).is_err());
        assert!(validate_request_semantics(&request, |_| true, |_| Some(9)).is_ok());
    }

    #[test]
    fn completed_request_task_is_joined_and_releases_executor_permit() {
        let (admission, executor, mut handler) = handler();
        handler.schedule(async { Ok(()) }).unwrap();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);

        executor.tick().unwrap();
        smol::block_on(handler.wait_for_task()).unwrap();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn session_handler_rejects_a_distinct_executor_admission() {
        let session_admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let executor_admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let executor = promise::spawn::SimpleExecutor::new(executor_admission);
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
        let sender = PduSender::new(|_, _| Ok(()));
        let error = SessionHandler::new(
            sender,
            policy,
            session_admission,
            MainThreadExecutorHandle::from_simple(executor.handle()),
        )
        .err()
        .unwrap();

        assert!(error
            .to_string()
            .contains("session admission and executor admission must be identical"));
    }

    #[test]
    fn request_task_error_is_observed_by_dispatch_owner() {
        let (_admission, executor, mut handler) = handler();
        handler
            .schedule(async { Err(anyhow!("request task failed")) })
            .unwrap();

        executor.tick().unwrap();
        let error = smol::block_on(handler.wait_for_task()).unwrap_err();
        assert!(error.to_string().contains("request task failed"));
    }

    #[test]
    fn proxy_then_primary_issues_one_established_identity() {
        let (_admission, _executor, mut handler) = handler();
        let mut proxy = ClientId::new();
        proxy.pid = 42;
        proxy.ssh_auth_sock = Some("proxy-agent".to_string());
        let mut primary = ClientId::new();
        primary.hostname = "primary".to_string();

        let (proxy_response, proxy_client) = handler.register_client(proxy, true, None).unwrap();
        assert!(proxy_client.is_none());
        assert!(proxy_response.resume_token.is_none());
        assert_eq!(
            handler.client_request_phase(),
            ClientRequestPhase::Bootstrap
        );
        let token = AttachmentResumeToken::from_random_bytes([9; 32]);
        let (response, issued) = handler
            .register_client(primary, false, Some(token.clone()))
            .unwrap();
        let issued = issued.unwrap();

        assert_eq!(
            handler.client_request_phase(),
            ClientRequestPhase::Established
        );
        assert_eq!(response.resume_token, Some(token));
        assert_eq!(issued.ssh_auth_sock.as_deref(), Some("proxy-agent"));
        assert_eq!(issued.hostname, "primary (via proxy pid 42)");
        assert!(handler
            .register_client(ClientId::new(), false, None)
            .is_err());

        // This test exercises issuance only; avoid invoking mux registration
        // teardown without a process-global test mux.
        handler.bootstrap = BootstrapState::AwaitingClient { proxy: None };
    }

    #[test]
    fn repeated_proxy_registration_is_rejected_without_changing_phase() {
        let (_admission, _executor, mut handler) = handler();

        assert!(handler
            .register_client(ClientId::new(), true, None)
            .unwrap()
            .1
            .is_none());
        assert!(handler
            .register_client(ClientId::new(), true, None)
            .is_err());
        assert_eq!(
            handler.client_request_phase(),
            ClientRequestPhase::Bootstrap
        );
    }

    #[test]
    fn allow_all_policy_cannot_bypass_structural_bootstrap() {
        let (_admission, _executor, handler) = handler();
        let request = Pdu::ListPanes(ListPanes {});
        let operation = request.request_operation().unwrap();

        let error = handler.authorize_request(operation, &request).unwrap_err();

        assert!(error
            .to_string()
            .contains("requires an established client identity"));
    }

    #[test]
    fn cancellation_is_joined_and_releases_executor_permit() {
        let (admission, executor, mut handler) = handler();
        handler
            .schedule(async {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .unwrap();
        for task in handler.tasks.iter_mut() {
            task.cancel();
        }
        executor.tick().unwrap();

        smol::block_on(handler.cancel_and_join_tasks()).unwrap();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }
}
