use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use directories::UserDirs;
use ratatui::{
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use unicode_width::UnicodeWidthChar;
use zeroize::Zeroizing;

use crate::{
    framework::{process::ProcessSupervisor, start_external, ExternalTarget},
    tui::{
        render_split_divider, ActionId, ActionInvocation, ActionState, ActionUnavailable,
        ContextMenu, ContextMenuLayout, ContextMenuOutcome, ContextMenuStyle, EventReader,
        KeyChord, KeybindingResolution, KeybindingState, NavigationHistory, NavigationMap,
        NavigationRegion, ResolvedAction, Session, SessionOptions, SplitDividerStyle, SplitDrag,
        SplitFrame, SplitMinimums, SplitRatio,
    },
};

use super::{
    cache::{CachedItem, ItemKind, ReceiveCache, ReceiverLeaseAttempt, SaveConflictResolution},
    client::{LoginEvent, TailClient},
    config::Config,
    contributions::{
        self, TailActionContext, TailActionRegistry, TailActionTarget, TailCommand, TailSurface,
        AUTH_INLINE, DEVICE_CONTEXT, ITEM_CONTEXT, MODAL_INLINE, WORKSPACE_INLINE,
    },
    file_input::{classify, PastedInput},
    model::{Device, Readiness},
};

const ACCENT: Color = Color::Rgb(120, 155, 255);
const MUTED: Color = Color::Rgb(125, 132, 145);
const TEXT: Color = Color::Rgb(230, 234, 242);
const GOOD: Color = Color::Rgb(98, 204, 143);
const WARN: Color = Color::Rgb(241, 190, 85);
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const DEVICE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const RECEIVER_LEASE_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAX_SEND_HISTORY: usize = 50;

enum Mode {
    Workspace,
    ReviewFiles(FileReview),
    Ambiguous { input: PastedInput, recipient: Recipient },
    Search,
    Detail { preview: Option<Zeroizing<String>> },
    ConfirmDelete,
    ConfirmQuit(Box<Mode>),
    FileBrowser(FileBrowser),
    SaveBrowser(SaveBrowser),
    SaveConflict { item: CachedItem, directory: PathBuf },
    Auth,
}

enum Overlay {
    ContextMenu(ContextMenu<TailActionContext>),
}

enum Flow {
    Continue,
    Invoke(ActionInvocation<TailActionContext>),
    NavigateHistory(isize),
    PersistSplitRatio(SplitRatio),
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Recipient {
    id: String,
    name: String,
    target: String,
}

#[derive(Clone)]
enum SendPayload {
    Text(Zeroizing<String>),
    Files(Vec<PathBuf>),
}

enum SendStatus {
    Queued(SendPayload),
    Sending(SendPayload),
    Cancelling(SendPayload),
    Sent,
    Failed { payload: SendPayload, error: String },
    Cancelled,
}

struct SendEntry {
    id: u64,
    recipient: Recipient,
    description: String,
    status: SendStatus,
}

#[derive(Default)]
struct SendQueue {
    entries: VecDeque<SendEntry>,
    next_id: u64,
}

struct SendWork {
    id: u64,
    recipient: Recipient,
    payload: SendPayload,
}

#[derive(Default)]
struct Composer {
    value: Zeroizing<String>,
    cursor: usize,
    recipient: Option<Recipient>,
}

struct FileReview {
    recipient: Recipient,
    paths: Vec<PathBuf>,
    pasted_text: Option<Zeroizing<String>>,
}

enum BackendEvent {
    SendFinished { id: u64, result: Result<(), String> },
    ExternalOpenFinished { context: ExternalOpenContext, result: Result<(), String> },
}

enum ExternalOpenContext {
    Item { name: String },
    Login,
}

impl ExternalOpenContext {
    fn notice(self, result: Result<(), String>) -> String {
        match (self, result) {
            (Self::Item { name }, Ok(())) => format!("Opened {name}"),
            (Self::Item { name }, Err(error)) => format!("Could not open {name}: {error}"),
            (Self::Login, Ok(())) => "Opened login link".into(),
            (Self::Login, Err(error)) => format!("Could not open login link: {error}"),
        }
    }
}

struct ReceiverEvent {
    generation: u64,
    kind: ReceiverEventKind,
}

enum ReceiverEventKind {
    State(ReceiverState),
    Received(Vec<CachedItem>),
    RecoveryFailed(String),
    Retry { error: String, delay: Duration, retry_at: Instant },
}

#[derive(Clone, Copy, Debug)]
enum ReceiverState {
    Starting,
    Standby,
    Waiting,
    Importing,
    Retrying { retry_at: Instant },
}

impl ReceiverState {
    fn animated(&self) -> bool {
        matches!(self, Self::Starting | Self::Importing | Self::Retrying { .. })
    }
}

struct Operation {
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct LoginOperation {
    events: mpsc::Receiver<LoginEvent>,
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct BrowserEntry {
    path: PathBuf,
    name: String,
    directory: bool,
}

struct FileBrowser {
    directory: PathBuf,
    entries: Vec<BrowserEntry>,
    index: usize,
    selected: BTreeSet<PathBuf>,
    recipient: Option<Recipient>,
}

struct SaveBrowser {
    browser: FileBrowser,
    item: CachedItem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveRegion {
    Devices,
    Items,
    Composer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TailLocation {
    Workspace { active_region: ActiveRegion, peer_id: Option<String>, item_id: Option<String> },
    Detail { item_id: String },
    FileBrowser { directory: PathBuf, recipient: Option<Recipient> },
    SaveBrowser { item_id: String, directory: PathBuf },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RowTarget {
    Peer(usize),
    Item(usize),
    File(usize),
    SaveDirectory(usize),
}

#[derive(Default)]
struct UiRegions {
    device_panel: Option<Rect>,
    item_panel: Option<Rect>,
    composer_panel: Option<Rect>,
    rows: Vec<(Rect, RowTarget)>,
    inline_actions: Vec<(Rect, ActionId)>,
    split: Option<SplitFrame>,
    context_menu: Option<ContextMenuLayout>,
}

impl UiRegions {
    fn navigation(&self) -> NavigationMap<ActiveRegion> {
        let mut regions = Vec::new();
        if let Some(area) = self.device_panel {
            regions.push(NavigationRegion::new(ActiveRegion::Devices, area));
        }
        if let Some(area) = self.item_panel {
            regions.push(NavigationRegion::new(ActiveRegion::Items, area));
        }
        if let Some(area) = self.composer_panel {
            regions.push(NavigationRegion::new(ActiveRegion::Composer, area));
        }
        NavigationMap::new(regions)
    }
}

struct App {
    local: Option<Device>,
    peers: Vec<Device>,
    items: Vec<CachedItem>,
    peer_index: usize,
    item_index: usize,
    active_region: ActiveRegion,
    mode: Mode,
    overlay: Option<Overlay>,
    registry: TailActionRegistry,
    keybinding_state: KeybindingState,
    composer: Composer,
    sends: SendQueue,
    search: String,
    notice: Option<String>,
    login_url: Option<String>,
    auth_can_login: bool,
    watch: bool,
    receiver_state: ReceiverState,
    receiver_generation: u64,
    pointer_enabled: bool,
    split_ratio: SplitRatio,
    split_drag: Option<SplitDrag<()>>,
    history: NavigationHistory<TailLocation>,
    spinner: usize,
}

struct ActionDispatch<'a> {
    session: &'a mut Session,
    processes: &'a ProcessSupervisor,
    client: &'a TailClient,
    cache: &'a ReceiveCache,
    backend_tx: &'a mpsc::Sender<BackendEvent>,
    receiver_tx: &'a mpsc::Sender<ReceiverEvent>,
    operation: &'a mut Option<Operation>,
    external_open: &'a mut Option<JoinHandle<()>>,
    receiver: &'a mut Option<Operation>,
    login: &'a mut Option<LoginOperation>,
}

pub async fn run(
    processes: ProcessSupervisor,
    client: TailClient,
    cache: ReceiveCache,
    readiness: Readiness,
    mut config: Config,
) -> Result<()> {
    let pruned = cache.prune()?;
    let items = cache.list()?;
    let mut app = App::with_preferences(
        readiness,
        items,
        config.auto_receive(),
        config.mouse(),
        config.split_ratio(),
    );
    if pruned > 0 {
        app.notice = Some(format!("Evicted {pruned} expired item(s)"));
    }
    let mut session = Session::open(SessionOptions {
        mouse_capture: app.pointer_enabled,
        bracketed_paste: true,
    })?;
    let mut events = EventReader::start();
    let (backend_tx, mut backend_rx) = mpsc::channel(8);
    let (receiver_tx, mut receiver_rx) = mpsc::channel(8);
    let mut operation: Option<Operation> = None;
    let mut external_open: Option<JoinHandle<()>> = None;
    let mut receiver: Option<Operation> = None;
    let mut refresh_task: Option<JoinHandle<Result<Readiness, String>>> = None;
    let mut login: Option<LoginOperation> = None;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut refresh_tick = tokio::time::interval(DEVICE_REFRESH_INTERVAL);
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    refresh_tick.tick().await;
    if matches!(app.mode, Mode::Auth) && app.auth_can_login {
        login = Some(begin_login(&client));
    }
    if app.local.is_some() && app.watch {
        app.receiver_generation = app.receiver_generation.wrapping_add(1);
        receiver = Some(start_receiver(&client, &cache, &receiver_tx, app.receiver_generation));
    }
    let mut regions = UiRegions::default();

    loop {
        session.draw(|frame| regions = render(frame, &app))?;
        tokio::select! {
            _ = tick.tick(), if operation.is_some()
                || (receiver.is_some() && app.receiver_state.animated())
                || (matches!(app.mode, Mode::Auth) && app.auth_can_login) => {
                app.spinner = app.spinner.wrapping_add(1);
            }
            _ = refresh_tick.tick(), if app.can_refresh_devices() && login.is_none() && refresh_task.is_none() => {
                refresh_task = Some(start_device_refresh(&client));
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                if matches!(event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_)) {
                    app.notice = None;
                }
                let mut actions = ActionDispatch {
                    session: &mut session,
                    processes: &processes,
                    client: &client,
                    cache: &cache,
                    backend_tx: &backend_tx,
                    receiver_tx: &receiver_tx,
                    operation: &mut operation,
                    external_open: &mut external_open,
                    receiver: &mut receiver,
                    login: &mut login,
                };
                let flow = if let Some(Overlay::ContextMenu(menu)) = app.overlay.as_mut() {
                    let layout = regions
                        .context_menu
                        .as_ref()
                        .expect("an open context menu has rendered layout");
                    match menu.on_event(event, layout) {
                        ContextMenuOutcome::Dismissed => {
                            app.overlay = None;
                            Flow::Continue
                        }
                        ContextMenuOutcome::Unavailable { reason, .. } => {
                            app.overlay = None;
                            app.notice = Some(reason.into_owned());
                            Flow::Continue
                        }
                        ContextMenuOutcome::Invoke(invocation) => {
                            app.overlay = None;
                            Flow::Invoke(invocation)
                        }
                        ContextMenuOutcome::Captured => Flow::Continue,
                    }
                } else {
                    match event {
                    Event::Paste(raw) => {
                        handle_paste(&mut app, raw);
                        Flow::Continue
                    }
                    Event::Mouse(mouse) => {
                        if app.pointer_enabled {
                            handle_mouse(mouse, &mut app, &regions)
                        } else {
                            Flow::Continue
                        }
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press =>
                        handle_key(key, &mut app, &regions, actions.login.is_none()),
                    _ => Flow::Continue,
                    }
                };
                match flow {
                    Flow::Continue => {}
                    Flow::Invoke(invocation) => {
                        if matches!(actions.invoke(&mut app, invocation).await?, Flow::Quit) {
                            break;
                        }
                    }
                    Flow::NavigateHistory(delta) => {
                        actions.navigate_history(&mut app, delta)?;
                    }
                    Flow::PersistSplitRatio(ratio) => {
                        config.set_split_ratio(ratio)?;
                        app.notice = Some("Panel size saved".into());
                    }
                    Flow::Quit => break,
                }
            }
            event = backend_rx.recv() => {
                match event {
                    Some(BackendEvent::SendFinished { id, result }) => {
                        operation = None;
                        app.notice = app.sends.finish(id, result);
                        start_next_send(&mut app, &client, &backend_tx, &mut operation);
                    }
                    Some(BackendEvent::ExternalOpenFinished { context, result }) => {
                        if let Some(task) = external_open.take() {
                            let _ = task.await;
                        }
                        app.notice = Some(context.notice(result));
                    }
                    None => {}
                }
            }
            event = receiver_rx.recv() => {
                match event {
                    Some(event) if event.generation != app.receiver_generation => {}
                    Some(ReceiverEvent { kind: ReceiverEventKind::State(state), .. }) => {
                        let resumed = matches!(app.receiver_state, ReceiverState::Retrying { .. })
                            && matches!(state, ReceiverState::Standby | ReceiverState::Waiting | ReceiverState::Importing);
                        app.receiver_state = state;
                        if resumed {
                            app.notice = Some("Receive connection restored".into());
                        }
                        if matches!(state, ReceiverState::Standby) {
                            refresh_received_items(&mut app, &cache)?;
                        }
                    }
                    Some(ReceiverEvent { kind: ReceiverEventKind::Received(items), .. }) => {
                        let count = items.len();
                        refresh_received_items(&mut app, &cache)?;
                        app.notice = Some(format!("Received {count} item(s)"));
                    }
                    Some(ReceiverEvent { kind: ReceiverEventKind::RecoveryFailed(error), .. }) => {
                        refresh_received_items(&mut app, &cache)?;
                        app.notice = Some(format!(
                            "A previous receive could not be recovered: {error}"
                        ));
                    }
                    Some(ReceiverEvent { kind: ReceiverEventKind::Retry { error, delay, retry_at }, .. }) => {
                        app.receiver_state = ReceiverState::Retrying { retry_at };
                        app.notice = Some(format!(
                            "Receive interrupted: {error}. Retrying in {}s…",
                            delay.as_secs()
                        ));
                    }
                    None => {}
                }
            }
            refreshed = finish_device_refresh(&mut refresh_task), if refresh_task.is_some() => {
                refresh_task = None;
                if login.is_none() {
                    match refreshed {
                    Ok(readiness) => {
                        let begin_auth = matches!(readiness, Readiness::NeedsLogin)
                            && !app.auth_can_login;
                        app.reconcile_readiness(readiness);
                        if begin_auth {
                            login = Some(begin_login(&client));
                        }
                        sync_receiver(
                            &mut app,
                            &client,
                            &cache,
                            &receiver_tx,
                            &mut receiver,
                        ).await?;
                        start_next_send(&mut app, &client, &backend_tx, &mut operation);
                    }
                    Err(error) => {
                        app.notice = Some(format!("Could not refresh Tailscale devices: {error}"));
                    }
                    }
                }
            }
            event = recv_login(&mut login), if login.is_some() => {
                match event {
                    Some(LoginEvent::Url(url)) => {
                        app.login_url = Some(url);
                        app.notice = Some("Open the link to authenticate".into());
                    }
                    Some(LoginEvent::Ready(Readiness::Ready { local, peers })) => {
                        app.reconcile_readiness(Readiness::Ready { local, peers });
                        sync_receiver(
                            &mut app,
                            &client,
                            &cache,
                            &receiver_tx,
                            &mut receiver,
                        ).await?;
                        start_next_send(&mut app, &client, &backend_tx, &mut operation);
                        finish_login(&mut login).await;
                    }
                    Some(LoginEvent::Failed(error)) => {
                        app.notice = Some(error);
                        finish_login(&mut login).await;
                    }
                    Some(LoginEvent::Cancelled) => {
                        app.notice = Some("Authentication cancelled; Enter retries".into());
                        finish_login(&mut login).await;
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(operation) = operation {
        let _ = operation.cancel.send(true);
        let _ = operation.task.await;
    }
    if let Some(task) = external_open {
        task.abort();
        let _ = task.await;
    }
    stop_operation(&mut receiver).await;
    cancel_login(&mut login).await;
    if let Some(task) = refresh_task {
        task.abort();
        let _ = task.await;
    }
    Ok(())
}

fn refresh_received_items(app: &mut App, cache: &ReceiveCache) -> Result<()> {
    let selected = app.selected_item().map(|item| item.id);
    app.items = cache.list()?;
    app.item_index = selected
        .and_then(|id| app.filtered_items().iter().position(|item| item.id == id))
        .unwrap_or(0);
    Ok(())
}

fn start_device_refresh(client: &TailClient) -> JoinHandle<Result<Readiness, String>> {
    let client = client.clone();
    tokio::spawn(async move { client.readiness().await.map_err(|error| format!("{error:#}")) })
}

fn spawn_external_open(
    processes: ProcessSupervisor,
    target: ExternalTarget,
    context: ExternalOpenContext,
    sender: mpsc::Sender<BackendEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = match start_external(&processes, target) {
            Ok(receipt) => receipt.completion().await,
            Err(error) => Err(error),
        }
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::ExternalOpenFinished { context, result }).await;
    })
}

async fn finish_device_refresh(
    task: &mut Option<JoinHandle<Result<Readiness, String>>>,
) -> Result<Readiness, String> {
    task.as_mut()
        .expect("refresh completion is selected only while a task exists")
        .await
        .map_err(|error| format!("device refresh task failed: {error}"))?
}

async fn recv_login(login: &mut Option<LoginOperation>) -> Option<LoginEvent> {
    match login {
        Some(operation) => operation.events.recv().await,
        None => std::future::pending().await,
    }
}

impl Recipient {
    fn from_device(device: &Device) -> Option<Self> {
        let target = device.send_target()?.to_owned();
        Some(Self {
            id: if device.id.is_empty() { target.clone() } else { device.id.clone() },
            name: device.name.clone(),
            target,
        })
    }
}

impl Composer {
    fn text(&self) -> &str {
        self.value.as_str()
    }

    fn cursor(&self) -> usize {
        self.cursor
    }

    fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    fn recipient(&self) -> Option<&Recipient> {
        self.recipient.as_ref()
    }

    fn can_retarget(&self, selected: Option<&Recipient>) -> bool {
        !self.is_empty()
            && self
                .recipient()
                .zip(selected)
                .is_some_and(|(current, selected)| current.id != selected.id)
    }

    fn accepts(&self, recipient: &Recipient) -> bool {
        self.is_empty() || self.recipient().is_some_and(|current| current.id == recipient.id)
    }

    fn retarget(&mut self, recipient: Recipient) {
        if !self.is_empty() {
            self.recipient = Some(recipient);
        }
    }

    fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.recipient = None;
    }

    fn insert(&mut self, text: &str, selected: Option<Recipient>) -> bool {
        if text.is_empty() {
            return true;
        }
        if self.recipient.is_none() {
            let Some(recipient) = selected else { return false };
            self.recipient = Some(recipient);
        }
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
        true
    }

    fn take(&mut self) -> Option<(Recipient, Zeroizing<String>)> {
        if self.is_empty() {
            return None;
        }
        let recipient = self.recipient.take()?;
        let text = std::mem::take(&mut self.value);
        self.cursor = 0;
        Some((recipient, text))
    }

    fn apply_edit_key(&mut self, key: KeyEvent, selected: Option<Recipient>) -> bool {
        match key.code {
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_vertical(-1),
            KeyCode::Down => self.move_vertical(1),
            KeyCode::Home => self.cursor = line_start(&self.value, self.cursor),
            KeyCode::End => self.cursor = line_end(&self.value, self.cursor),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => self.cursor = 0,
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.value.len();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self.clear(),
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_previous_word();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                return self.insert(&character.to_string(), selected);
            }
            _ => {}
        }
        true
    }

    fn place_cursor(&mut self, logical_row: usize, display_column: usize) {
        let mut start = 0;
        for _ in 0..logical_row {
            let Some(next_line) = self.value[start..].find('\n') else {
                self.cursor = self.value.len();
                return;
            };
            start += next_line + 1;
        }
        let end = line_end(&self.value, start);
        self.cursor = byte_at_display_column(&self.value, start, end, display_column);
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = previous_boundary(&self.value, self.cursor);
        self.value.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.release_recipient_if_empty();
    }

    fn delete(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let next = next_boundary(&self.value, self.cursor);
        self.value.replace_range(self.cursor..next, "");
        self.release_recipient_if_empty();
    }

    fn move_left(&mut self) {
        self.cursor = previous_boundary(&self.value, self.cursor);
    }

    fn move_right(&mut self) {
        self.cursor = next_boundary(&self.value, self.cursor);
    }

    fn move_vertical(&mut self, delta: isize) {
        let start = line_start(&self.value, self.cursor);
        let column = self.value[start..self.cursor]
            .chars()
            .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
            .sum();
        let target_start = if delta.is_negative() {
            if start == 0 {
                return;
            }
            line_start(&self.value, start.saturating_sub(1))
        } else {
            let end = line_end(&self.value, self.cursor);
            if end == self.value.len() {
                return;
            }
            end + 1
        };
        let target_end = line_end(&self.value, target_start);
        self.cursor = byte_at_display_column(&self.value, target_start, target_end, column);
    }

    fn delete_previous_word(&mut self) {
        let mut start = self.cursor;
        while start > 0 {
            let previous = previous_boundary(&self.value, start);
            let character = self.value[previous..start].chars().next().expect("character boundary");
            if !character.is_whitespace() {
                break;
            }
            start = previous;
        }
        while start > 0 {
            let previous = previous_boundary(&self.value, start);
            let character = self.value[previous..start].chars().next().expect("character boundary");
            if character.is_whitespace() {
                break;
            }
            start = previous;
        }
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.release_recipient_if_empty();
    }

    fn release_recipient_if_empty(&mut self) {
        if self.is_empty() {
            self.recipient = None;
        }
    }
}

impl SendPayload {
    fn description(&self) -> String {
        match self {
            Self::Text(text) => format!("text · {}", human_bytes(text.len() as u64)),
            Self::Files(paths) => match paths.as_slice() {
                [path] => path
                    .file_name()
                    .map_or_else(|| "1 file".into(), |name| name.to_string_lossy().into_owned()),
                _ => format!("{} files", paths.len()),
            },
        }
    }
}

impl SendQueue {
    fn enqueue(&mut self, recipient: Recipient, payload: SendPayload) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let description = payload.description();
        self.entries.push_back(SendEntry {
            id,
            recipient,
            description,
            status: SendStatus::Queued(payload),
        });
        id
    }

    fn start_next(&mut self) -> Option<SendWork> {
        let entry =
            self.entries.iter_mut().find(|entry| matches!(entry.status, SendStatus::Queued(_)))?;
        let status = std::mem::replace(&mut entry.status, SendStatus::Sent);
        let SendStatus::Queued(payload) = status else { unreachable!("queued entry changed") };
        let work =
            SendWork { id: entry.id, recipient: entry.recipient.clone(), payload: payload.clone() };
        entry.status = SendStatus::Sending(payload);
        Some(work)
    }

    fn finish(&mut self, id: u64, result: Result<(), String>) -> Option<String> {
        let entry = self.entries.iter_mut().find(|entry| entry.id == id)?;
        let previous = std::mem::replace(&mut entry.status, SendStatus::Sent);
        let (payload, cancelling) = match previous {
            SendStatus::Sending(payload) => (payload, false),
            SendStatus::Cancelling(payload) => (payload, true),
            other => {
                entry.status = other;
                return Some("A stale send completion was ignored".into());
            }
        };
        let notice = match result {
            Ok(()) => format!("Sent {} to {}", entry.description, entry.recipient.name),
            Err(_) if cancelling => {
                drop(payload);
                entry.status = SendStatus::Cancelled;
                format!("Cancelled send to {}", entry.recipient.name)
            }
            Err(error) => {
                let notice = format!("Send to {} failed: {error}", entry.recipient.name);
                entry.status = SendStatus::Failed { payload, error };
                notice
            }
        };
        self.prune_completed();
        Some(notice)
    }

    fn mark_cancelling(&mut self) -> bool {
        let Some(entry) =
            self.entries.iter_mut().find(|entry| matches!(entry.status, SendStatus::Sending(_)))
        else {
            return false;
        };
        let previous = std::mem::replace(&mut entry.status, SendStatus::Sent);
        let SendStatus::Sending(payload) = previous else { unreachable!("selected active send") };
        entry.status = SendStatus::Cancelling(payload);
        true
    }

    fn retry_failed(&mut self) -> usize {
        let mut retained = VecDeque::with_capacity(self.entries.len());
        let mut retries = VecDeque::new();
        while let Some(mut entry) = self.entries.pop_front() {
            let previous = std::mem::replace(&mut entry.status, SendStatus::Sent);
            match previous {
                SendStatus::Failed { payload, .. } => {
                    entry.status = SendStatus::Queued(payload);
                    retries.push_back(entry);
                }
                status => {
                    entry.status = status;
                    retained.push_back(entry);
                }
            }
        }
        let count = retries.len();
        retained.append(&mut retries);
        self.entries = retained;
        count
    }

    fn active(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.status, SendStatus::Sending(_) | SendStatus::Cancelling(_)))
    }

    fn queued_count(&self) -> usize {
        self.entries.iter().filter(|entry| matches!(entry.status, SendStatus::Queued(_))).count()
    }

    fn failed_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.status, SendStatus::Failed { .. }))
            .count()
    }

    fn unfinished_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.status,
                    SendStatus::Queued(_) | SendStatus::Sending(_) | SendStatus::Cancelling(_)
                )
            })
            .count()
    }

    fn latest_status(&self) -> Option<String> {
        let entry = self.entries.back()?;
        let state = match &entry.status {
            SendStatus::Queued(_) => "queued",
            SendStatus::Sending(_) => "sending",
            SendStatus::Cancelling(_) => "cancelling",
            SendStatus::Sent => "sent",
            SendStatus::Failed { error, .. } => {
                return Some(format!(
                    "failed {} → {} · {}",
                    entry.description,
                    entry.recipient.name,
                    error.lines().next().unwrap_or("unknown error")
                ));
            }
            SendStatus::Cancelled => "cancelled",
        };
        Some(format!("{state} {} → {}", entry.description, entry.recipient.name))
    }

    fn prune_completed(&mut self) {
        while self
            .entries
            .iter()
            .filter(|entry| matches!(entry.status, SendStatus::Sent | SendStatus::Cancelled))
            .count()
            > MAX_SEND_HISTORY
        {
            let Some(index) = self
                .entries
                .iter()
                .position(|entry| matches!(entry.status, SendStatus::Sent | SendStatus::Cancelled))
            else {
                break;
            };
            self.entries.remove(index);
        }
    }
}

fn previous_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor].char_indices().next_back().map_or(0, |(index, _)| index)
}

fn next_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..].chars().next().map_or(cursor, |character| cursor + character.len_utf8())
}

fn line_start(value: &str, cursor: usize) -> usize {
    value[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

fn line_end(value: &str, cursor: usize) -> usize {
    value[cursor..].find('\n').map_or(value.len(), |index| cursor + index)
}

fn byte_at_display_column(value: &str, start: usize, end: usize, column: usize) -> usize {
    let mut width = 0;
    for (index, character) in value[start..end].char_indices() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if column < width + character_width {
            return start + index;
        }
        width += character_width;
    }
    end
}

impl App {
    fn new(readiness: Readiness, items: Vec<CachedItem>) -> Self {
        Self::with_preferences(readiness, items, true, true, SplitRatio::new(440))
    }

    fn with_preferences(
        readiness: Readiness,
        items: Vec<CachedItem>,
        watch: bool,
        pointer_enabled: bool,
        split_ratio: SplitRatio,
    ) -> Self {
        let (local, peers, mode, notice, auth_can_login) = match readiness {
            Readiness::Ready { local, peers } => (Some(local), peers, Mode::Workspace, None, false),
            Readiness::NeedsLogin => {
                (None, Vec::new(), Mode::Auth, Some("Starting Tailscale authentication…".into()), true)
            }
            Readiness::CliUnavailable(error) => (None, Vec::new(), Mode::Auth, Some(format!("Tailscale CLI is unavailable. Install it and ensure `tailscale` is on PATH.\n{error}")), false),
            Readiness::DaemonUnavailable(error) => (None, Vec::new(), Mode::Auth, Some(format!("Start the Tailscale service, then press Enter to retry.\n{error}")), false),
            Readiness::PermissionDenied(error) => (None, Vec::new(), Mode::Auth, Some(format!("This user cannot access Tailscale. Fix local permissions, then press Enter.\n{error}")), false),
            Readiness::Unsupported(error) => (None, Vec::new(), Mode::Auth, Some(error), false),
        };
        let mut app = Self {
            local,
            peers,
            items,
            peer_index: 0,
            item_index: 0,
            active_region: ActiveRegion::Devices,
            mode,
            overlay: None,
            registry: contributions::registry().expect("Tail action contributions are valid"),
            keybinding_state: KeybindingState::default(),
            composer: Composer::default(),
            sends: SendQueue::default(),
            search: String::new(),
            notice,
            login_url: None,
            auth_can_login,
            watch,
            receiver_state: ReceiverState::Starting,
            receiver_generation: 0,
            pointer_enabled,
            split_ratio,
            split_drag: None,
            history: NavigationHistory::default(),
            spinner: 0,
        };
        app.visit_current_location();
        app
    }

    fn selected_peer(&self) -> Option<&Device> {
        self.filtered_peers().get(self.peer_index).copied()
    }

    fn selected_recipient(&self) -> Option<Recipient> {
        self.selected_peer().and_then(Recipient::from_device)
    }

    fn selected_item(&self) -> Option<&CachedItem> {
        self.filtered_items().get(self.item_index).copied()
    }

    fn filtered_peers(&self) -> Vec<&Device> {
        let query = self.search.to_lowercase();
        self.peers
            .iter()
            .filter(|peer| query.is_empty() || peer.name.to_lowercase().contains(&query))
            .collect()
    }

    fn filtered_items(&self) -> Vec<&CachedItem> {
        let query = self.search.to_lowercase();
        self.items
            .iter()
            .filter(|item| query.is_empty() || item.name.to_lowercase().contains(&query))
            .collect()
    }

    fn can_refresh_devices(&self) -> bool {
        !matches!(self.mode, Mode::Auth) || !self.auth_can_login
    }

    fn action_context(&self) -> TailActionContext {
        let target = match (&self.mode, self.active_region) {
            (Mode::Auth, _) => TailActionTarget::Auth,
            (Mode::Workspace, ActiveRegion::Devices | ActiveRegion::Composer) => self
                .selected_recipient()
                .map(|recipient| TailActionTarget::Device(recipient.target))
                .unwrap_or(TailActionTarget::None),
            (Mode::Workspace | Mode::Detail { .. } | Mode::ConfirmDelete, ActiveRegion::Items) => {
                self.selected_item()
                    .map(|item| TailActionTarget::Item {
                        id: item.id.to_string(),
                        text: item.kind == ItemKind::Text,
                    })
                    .unwrap_or(TailActionTarget::None)
            }
            _ => TailActionTarget::None,
        };
        let selected = self.selected_recipient();
        TailActionContext {
            surface: match &self.mode {
                Mode::Workspace => TailSurface::Workspace,
                Mode::ReviewFiles(review) => {
                    TailSurface::ReviewFiles { can_insert_text: review.pasted_text.is_some() }
                }
                Mode::Ambiguous { .. } => TailSurface::Ambiguous,
                Mode::Search => TailSurface::Search,
                Mode::Detail { .. } => TailSurface::Detail,
                Mode::ConfirmDelete => TailSurface::ConfirmDelete,
                Mode::ConfirmQuit(_) => TailSurface::ConfirmQuit,
                Mode::FileBrowser(browser) => {
                    TailSurface::FileBrowser { selected_files: !browser.selected.is_empty() }
                }
                Mode::SaveBrowser(_) => TailSurface::SaveBrowser,
                Mode::SaveConflict { .. } => TailSurface::SaveConflict,
                Mode::Auth => TailSurface::Auth,
            },
            target,
            receiving: self.watch,
            has_message: !self.composer.is_empty(),
            can_retarget_message: self.composer.can_retarget(selected.as_ref()),
            failed_sends: self.sends.failed_count(),
            active_send: self.sends.active(),
            login_url: self.login_url.is_some(),
            can_retry_login: matches!(self.mode, Mode::Auth),
        }
    }

    fn location(&self) -> Option<TailLocation> {
        let peer_id = self.selected_peer().map(|peer| peer.id.clone());
        match &self.mode {
            Mode::Workspace => Some(TailLocation::Workspace {
                active_region: self.active_region,
                peer_id,
                item_id: self.selected_item().map(|item| item.id.to_string()),
            }),
            Mode::Detail { .. } => self
                .selected_item()
                .map(|item| TailLocation::Detail { item_id: item.id.to_string() }),
            Mode::FileBrowser(browser) => Some(TailLocation::FileBrowser {
                directory: browser.directory.clone(),
                recipient: browser.recipient.clone(),
            }),
            Mode::SaveBrowser(save) => Some(TailLocation::SaveBrowser {
                item_id: save.item.id.to_string(),
                directory: save.browser.directory.clone(),
            }),
            Mode::ReviewFiles(_)
            | Mode::Ambiguous { .. }
            | Mode::Search
            | Mode::ConfirmDelete
            | Mode::ConfirmQuit(_)
            | Mode::SaveConflict { .. }
            | Mode::Auth => None,
        }
    }

    fn replace_current_location(&mut self) {
        if let Some(location) = self.location() {
            self.history.replace_current(location);
        }
    }

    fn visit_current_location(&mut self) {
        if let Some(location) = self.location() {
            self.history.visit(location);
        }
    }

    fn select_peer_id(&mut self, peer_id: Option<&str>) {
        let Some(peer_id) = peer_id else { return };
        if let Some(index) = self.filtered_peers().iter().position(|peer| peer.id == peer_id) {
            self.peer_index = index;
        }
    }

    fn select_item_id(&mut self, item_id: &str) -> bool {
        let Some(index) =
            self.filtered_items().iter().position(|item| item.id.to_string() == item_id)
        else {
            return false;
        };
        self.item_index = index;
        true
    }

    fn restore_action_target(&mut self, target: &TailActionTarget) -> bool {
        match target {
            TailActionTarget::Device(id) => {
                let Some(index) = self
                    .filtered_peers()
                    .iter()
                    .position(|peer| peer.send_target() == Some(id.as_str()))
                else {
                    return false;
                };
                self.peer_index = index;
                true
            }
            TailActionTarget::Item { id, .. } => {
                let Some(index) =
                    self.filtered_items().iter().position(|item| item.id.to_string() == *id)
                else {
                    return false;
                };
                self.item_index = index;
                true
            }
            TailActionTarget::Auth => matches!(self.mode, Mode::Auth),
            TailActionTarget::None => true,
        }
    }

    fn reconcile_readiness(&mut self, readiness: Readiness) {
        let Readiness::Ready { local, peers } = readiness else {
            let replacement = Self::new(readiness, std::mem::take(&mut self.items));
            self.local = replacement.local;
            self.peers = replacement.peers;
            self.mode = replacement.mode;
            self.notice = replacement.notice;
            self.login_url = None;
            self.auth_can_login = replacement.auth_can_login;
            self.peer_index = 0;
            return;
        };

        let selected =
            self.selected_peer().map(|device| (device.id.clone(), device.dns_name.clone()));
        let was_auth = matches!(self.mode, Mode::Auth);
        self.local = Some(local);
        self.peers = peers;
        let peers = self.filtered_peers();
        let preserved_index = selected.as_ref().and_then(|(id, dns_name)| {
            peers.iter().position(|peer| {
                (!id.is_empty() && peer.id == *id)
                    || (!dns_name.is_empty() && peer.dns_name == *dns_name)
            })
        });
        self.peer_index =
            preserved_index.unwrap_or_else(|| self.peer_index.min(peers.len().saturating_sub(1)));
        self.login_url = None;
        self.auth_can_login = false;
        if was_auth {
            self.mode = Mode::Workspace;
            self.notice = Some("Tailscale connected".into());
            self.visit_current_location();
        }
    }
}

impl FileBrowser {
    fn open(directory: PathBuf) -> Result<Self> {
        let directory = directory
            .canonicalize()
            .with_context(|| format!("open file browser at {}", directory.display()))?;
        let entries = browser_entries(&directory)?;
        Ok(Self { directory, entries, index: 0, selected: BTreeSet::new(), recipient: None })
    }

    fn open_for_send(directory: PathBuf, recipient: Recipient) -> Result<Self> {
        let mut browser = Self::open(directory)?;
        browser.recipient = Some(recipient);
        Ok(browser)
    }

    fn selected_entry(&self) -> Option<&BrowserEntry> {
        self.entries.get(self.index)
    }

    fn move_selection(&mut self, delta: isize) {
        if !self.entries.is_empty() {
            self.index =
                (self.index as isize + delta).clamp(0, self.entries.len() as isize - 1) as usize;
        }
    }

    fn enter(&mut self, directory: PathBuf) -> Result<()> {
        self.directory = directory.canonicalize()?;
        self.entries = browser_entries(&self.directory)?;
        self.index = 0;
        Ok(())
    }

    fn parent(&mut self) -> Result<()> {
        let Some(parent) = self.directory.parent().map(Path::to_owned) else { return Ok(()) };
        self.enter(parent)
    }

    fn toggle_selected_file(&mut self) {
        let Some(path) =
            self.selected_entry().filter(|entry| !entry.directory).map(|entry| entry.path.clone())
        else {
            return;
        };
        if !self.selected.remove(&path) {
            self.selected.insert(path);
        }
    }
}

fn browser_entries(directory: &Path) -> Result<Vec<BrowserEntry>> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read {}", directory.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            (metadata.is_dir() || metadata.is_file()).then(|| BrowserEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                directory: metadata.is_dir(),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| (!entry.directory, entry.name.to_lowercase()));
    Ok(entries)
}

fn handle_mouse(mouse: MouseEvent, app: &mut App, regions: &UiRegions) -> Flow {
    let position = Position { x: mouse.column, y: mouse.row };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.split_drag = regions.split.and_then(|split| {
                SplitDrag::begin((), split, app.split_ratio, mouse.column, mouse.row)
            });
            if app.split_drag.is_some() {
                return Flow::Continue;
            }
            if let Some((_, action)) =
                regions.inline_actions.iter().find(|(area, _)| area.contains(position))
            {
                return Flow::Invoke(ActionInvocation::new(*action, app.action_context()));
            }
            if let Some((_, target)) = regions.rows.iter().find(|(area, _)| area.contains(position))
            {
                match *target {
                    RowTarget::Peer(index) => {
                        app.peer_index = index;
                        app.active_region = ActiveRegion::Composer;
                    }
                    RowTarget::Item(index) => {
                        app.item_index = index;
                        app.active_region = ActiveRegion::Items;
                    }
                    RowTarget::File(index) => {
                        if let Mode::FileBrowser(browser) = &mut app.mode {
                            browser.index = index;
                        }
                    }
                    RowTarget::SaveDirectory(index) => {
                        if let Mode::SaveBrowser(save) = &mut app.mode {
                            save.browser.index = index;
                        }
                    }
                }
                return Flow::Continue;
            }
            if let Some(composer_panel) = regions.composer_panel {
                if composer_panel.contains(position) {
                    app.active_region = ActiveRegion::Composer;
                    let input = composer_input_area(composer_panel);
                    if input.contains(position) {
                        let (vertical, horizontal, _, _) = composer_viewport(&app.composer, input);
                        app.composer.place_cursor(
                            usize::from(vertical) + usize::from(position.y.saturating_sub(input.y)),
                            usize::from(horizontal)
                                + usize::from(position.x.saturating_sub(input.x)),
                        );
                    }
                    return Flow::Continue;
                }
            }
            if let Some(region) = regions.navigation().hit_test(mouse.column, mouse.row) {
                app.active_region = region;
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            app.split_drag = None;
            if let Some((_, target)) = regions.rows.iter().find(|(area, _)| area.contains(position))
            {
                let menu = match *target {
                    RowTarget::Peer(index) => {
                        app.peer_index = index;
                        app.active_region = ActiveRegion::Devices;
                        DEVICE_CONTEXT
                    }
                    RowTarget::Item(index) => {
                        app.item_index = index;
                        app.active_region = ActiveRegion::Items;
                        ITEM_CONTEXT
                    }
                    _ => return Flow::Continue,
                };
                let context = app.action_context();
                let items = app.registry.resolve_menu(menu, &context);
                app.overlay = ContextMenu::open(position, context, items).map(Overlay::ContextMenu);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if app.split_drag.is_some() => {
            if let Some(ratio) = app.split_drag.and_then(|drag| {
                regions.split.and_then(|split| drag.ratio_for_column((), split, mouse.column))
            }) {
                app.split_ratio = ratio;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(drag) = app.split_drag.take() {
                if drag.changed(app.split_ratio) {
                    return Flow::PersistSplitRatio(app.split_ratio);
                }
            }
        }
        MouseEventKind::ScrollUp => {
            move_mouse_selection(app, regions, position, -3);
        }
        MouseEventKind::ScrollDown => {
            move_mouse_selection(app, regions, position, 3);
        }
        _ => {}
    }
    Flow::Continue
}

fn move_mouse_selection(app: &mut App, regions: &UiRegions, position: Position, delta: isize) {
    match &mut app.mode {
        Mode::Workspace => {
            if regions.device_panel.is_some_and(|area| area.contains(position)) {
                app.active_region = ActiveRegion::Devices;
            } else if regions.item_panel.is_some_and(|area| area.contains(position)) {
                app.active_region = ActiveRegion::Items;
            }
            move_selection(app, delta);
        }
        Mode::FileBrowser(browser) => browser.move_selection(delta),
        Mode::SaveBrowser(save) => save.browser.move_selection(delta),
        _ => {}
    }
}

impl ActionDispatch<'_> {
    fn navigate_history(&mut self, app: &mut App, delta: isize) -> Result<()> {
        app.replace_current_location();
        let Some((cursor, location)) =
            app.history.target(delta).map(|(cursor, location)| (cursor, location.clone()))
        else {
            let direction = if delta.is_negative() { "back" } else { "forward" };
            app.notice = Some(format!("No {direction} history"));
            return Ok(());
        };
        if self.restore_location(app, &location)? {
            app.history.select(cursor);
            app.notice = Some(if delta.is_negative() {
                "History back".into()
            } else {
                "History forward".into()
            });
        } else {
            app.notice = Some("That history entry is no longer available".into());
        }
        Ok(())
    }

    fn restore_location(&self, app: &mut App, location: &TailLocation) -> Result<bool> {
        match location {
            TailLocation::Workspace { active_region, peer_id, item_id } => {
                app.mode = Mode::Workspace;
                app.active_region = *active_region;
                app.select_peer_id(peer_id.as_deref());
                if let Some(item_id) = item_id {
                    app.select_item_id(item_id);
                }
                Ok(true)
            }
            TailLocation::Detail { item_id } => {
                if !app.select_item_id(item_id) {
                    return Ok(false);
                }
                app.active_region = ActiveRegion::Items;
                open_detail(app, self.cache);
                Ok(true)
            }
            TailLocation::FileBrowser { directory, recipient } => {
                let mut browser = FileBrowser::open(directory.clone())?;
                browser.recipient = recipient.clone();
                app.mode = Mode::FileBrowser(browser);
                Ok(true)
            }
            TailLocation::SaveBrowser { item_id, directory } => {
                let Some(item) =
                    app.items.iter().find(|item| item.id.to_string() == *item_id).cloned()
                else {
                    return Ok(false);
                };
                let browser = FileBrowser::open(directory.clone())?;
                app.mode = Mode::SaveBrowser(SaveBrowser { browser, item });
                Ok(true)
            }
        }
    }

    async fn invoke(
        &mut self,
        app: &mut App,
        invocation: ActionInvocation<TailActionContext>,
    ) -> Result<Flow> {
        if !app.restore_action_target(&invocation.context.target) {
            app.notice = Some("That selection is no longer available".into());
            return Ok(Flow::Continue);
        }
        let command = match app.registry.command_for(&invocation) {
            Ok(command) => command,
            Err(ActionUnavailable::Disabled { reason, .. }) => {
                app.notice = Some(reason.into_owned());
                return Ok(Flow::Continue);
            }
            Err(ActionUnavailable::Unknown { action }) => {
                app.notice = Some(format!("Unknown action {action}"));
                return Ok(Flow::Continue);
            }
        };
        app.replace_current_location();
        match command {
            TailCommand::FocusComposer => app.active_region = ActiveRegion::Composer,
            TailCommand::ChooseFiles => {
                let recipient =
                    app.selected_recipient().context("select a device that supports Taildrop")?;
                let browser = FileBrowser::open_for_send(std::env::current_dir()?, recipient)?;
                app.mode = Mode::FileBrowser(browser);
            }
            TailCommand::SendText => {
                let (recipient, text) = app.composer.take().context("type a message first")?;
                app.sends.enqueue(recipient, SendPayload::Text(text));
                app.active_region = ActiveRegion::Composer;
                start_next_send(app, self.client, self.backend_tx, self.operation);
            }
            TailCommand::SendFiles => {
                let Mode::ReviewFiles(review) = &app.mode else {
                    return Ok(Flow::Continue);
                };
                let recipient = review.recipient.clone();
                let paths = review.paths.clone();
                app.sends.enqueue(recipient, SendPayload::Files(paths));
                app.mode = Mode::Workspace;
                app.active_region = ActiveRegion::Composer;
                start_next_send(app, self.client, self.backend_tx, self.operation);
            }
            TailCommand::ClearComposer => {
                app.composer.clear();
                app.notice = Some("Message cleared".into());
            }
            TailCommand::RetargetComposer => {
                if let Some(recipient) = app.selected_recipient() {
                    app.composer.retarget(recipient.clone());
                    app.notice = Some(format!("Message now targets {}", recipient.name));
                }
            }
            TailCommand::RetryFailed => {
                let count = app.sends.retry_failed();
                app.notice = Some(format!("Queued {count} failed send(s) again"));
                start_next_send(app, self.client, self.backend_tx, self.operation);
            }
            TailCommand::Inspect => open_detail(app, self.cache),
            TailCommand::Copy => {
                if let Some(item) = app.selected_item().cloned() {
                    app.notice = Some(
                        match self.cache.read_text(&item).and_then(|text| self.session.copy(&text))
                        {
                            Ok(()) => format!("Copied {}", item.name),
                            Err(error) => format!("Could not copy {}: {error:#}", item.name),
                        },
                    );
                }
            }
            TailCommand::Save => {
                if let Some(item) = app.selected_item().cloned() {
                    let directory = UserDirs::new()
                        .and_then(|dirs| dirs.download_dir().map(PathBuf::from))
                        .unwrap_or(std::env::current_dir()?);
                    match FileBrowser::open(directory) {
                        Ok(browser) => app.mode = Mode::SaveBrowser(SaveBrowser { browser, item }),
                        Err(error) => app.notice = Some(format!("Could not browse: {error:#}")),
                    }
                }
            }
            TailCommand::Open => {
                if let Some(item) = app.selected_item().cloned() {
                    if self.external_open.is_some() {
                        app.notice = Some("Another external target is still opening".into());
                    } else {
                        let name = item.name.clone();
                        app.notice = Some(format!("Opening {name}…"));
                        *self.external_open = Some(spawn_external_open(
                            self.processes.clone(),
                            ExternalTarget::Path(item.payload()),
                            ExternalOpenContext::Item { name },
                            self.backend_tx.clone(),
                        ));
                    }
                }
            }
            TailCommand::Delete => app.mode = Mode::ConfirmDelete,
            TailCommand::Search => {
                app.mode = Mode::Search;
                app.search.clear();
            }
            TailCommand::ToggleReceiving => {
                app.watch = !app.watch;
                sync_receiver(app, self.client, self.cache, self.receiver_tx, self.receiver)
                    .await?;
                app.notice = Some(if app.watch {
                    "Automatic receiving resumed".into()
                } else {
                    "Automatic receiving paused".into()
                });
            }
            TailCommand::OpenLogin => {
                if let Some(url) = app.login_url.clone() {
                    if self.external_open.is_some() {
                        app.notice = Some("Another external target is still opening".into());
                    } else {
                        app.notice = Some("Opening login link…".into());
                        *self.external_open = Some(spawn_external_open(
                            self.processes.clone(),
                            ExternalTarget::Url(url),
                            ExternalOpenContext::Login,
                            self.backend_tx.clone(),
                        ));
                    }
                }
            }
            TailCommand::CopyLogin => {
                if let Some(url) = &app.login_url {
                    app.notice = Some(match self.session.copy(url) {
                        Ok(()) => "Login link copied".into(),
                        Err(error) => format!("Could not copy login link: {error:#}"),
                    });
                }
            }
            TailCommand::RetryLogin => {
                cancel_login(self.login).await;
                app.reconcile_readiness(self.client.readiness().await?);
                sync_receiver(app, self.client, self.cache, self.receiver_tx, self.receiver)
                    .await?;
                start_next_send(app, self.client, self.backend_tx, self.operation);
                if app.auth_can_login {
                    *self.login = Some(begin_login(self.client));
                    app.notice = Some("Starting Tailscale authentication…".into());
                }
            }
            TailCommand::Back => match &app.mode {
                Mode::ReviewFiles(_)
                | Mode::Ambiguous { .. }
                | Mode::Search
                | Mode::ConfirmDelete => {
                    app.mode = Mode::Workspace;
                }
                Mode::ConfirmQuit(_) => {
                    let previous = std::mem::replace(&mut app.mode, Mode::Workspace);
                    let Mode::ConfirmQuit(previous) = previous else {
                        unreachable!("matched quit confirmation")
                    };
                    app.mode = *previous;
                }
                Mode::SaveConflict { item, directory } => {
                    let item = item.clone();
                    let directory = directory.clone();
                    match FileBrowser::open(directory) {
                        Ok(browser) => app.mode = Mode::SaveBrowser(SaveBrowser { browser, item }),
                        Err(error) => {
                            app.mode = Mode::Workspace;
                            app.notice = Some(format!("Could not browse: {error:#}"));
                        }
                    }
                }
                _ => self.navigate_history(app, -1)?,
            },
            TailCommand::UseFiles => {
                if let Mode::Ambiguous {
                    input: PastedInput::Ambiguous { raw, existing, .. },
                    recipient,
                } = &app.mode
                {
                    app.mode = Mode::ReviewFiles(FileReview {
                        recipient: recipient.clone(),
                        paths: existing.clone(),
                        pasted_text: Some(Zeroizing::new(raw.clone())),
                    });
                }
            }
            TailCommand::UseText => {
                let choice = match &app.mode {
                    Mode::Ambiguous { input: PastedInput::Ambiguous { raw, .. }, recipient } => {
                        Some((raw.clone(), recipient.clone()))
                    }
                    Mode::ReviewFiles(review) => review
                        .pasted_text
                        .as_ref()
                        .map(|text| (text.to_string(), review.recipient.clone())),
                    _ => None,
                };
                if let Some((raw, recipient)) = choice {
                    if app.composer.accepts(&recipient)
                        && app.composer.insert(&raw, Some(recipient.clone()))
                    {
                        app.mode = Mode::Workspace;
                        app.active_region = ActiveRegion::Composer;
                    } else {
                        let current = app
                            .composer
                            .recipient()
                            .map_or("another device", |current| current.name.as_str());
                        app.notice = Some(format!(
                            "The existing message targets {current}; send or clear it first"
                        ));
                    }
                }
            }
            TailCommand::ConfirmDelete => {
                if let Some(item) = app.selected_item().cloned() {
                    self.cache.delete(&item)?;
                    app.items = self.cache.list()?;
                    app.item_index = 0;
                }
                app.mode = Mode::Workspace;
                app.notice = Some("Deleted cached item".into());
            }
            TailCommand::OpenEntry => match &mut app.mode {
                Mode::FileBrowser(browser) => {
                    if let Some(entry) = browser.selected_entry() {
                        let path = entry.path.clone();
                        if entry.directory {
                            if let Err(error) = browser.enter(path) {
                                app.notice = Some(format!("Could not open directory: {error:#}"));
                            }
                        } else {
                            let Some(recipient) = browser.recipient.clone() else {
                                app.notice =
                                    Some("The original recipient is no longer available".into());
                                return Ok(Flow::Continue);
                            };
                            app.mode = Mode::ReviewFiles(FileReview {
                                recipient,
                                paths: vec![path],
                                pasted_text: None,
                            });
                        }
                    }
                }
                Mode::SaveBrowser(save) => {
                    if let Some(entry) =
                        save.browser.selected_entry().filter(|entry| entry.directory)
                    {
                        let path = entry.path.clone();
                        if let Err(error) = save.browser.enter(path) {
                            app.notice = Some(format!("Could not open directory: {error:#}"));
                        }
                    }
                }
                _ => {}
            },
            TailCommand::ToggleFile => {
                if let Mode::FileBrowser(browser) = &mut app.mode {
                    browser.toggle_selected_file();
                }
            }
            TailCommand::ReviewSelected => {
                if let Mode::FileBrowser(browser) = &app.mode {
                    let Some(recipient) = browser.recipient.clone() else {
                        app.notice = Some("The original recipient is no longer available".into());
                        return Ok(Flow::Continue);
                    };
                    app.mode = Mode::ReviewFiles(FileReview {
                        recipient,
                        paths: browser.selected.iter().cloned().collect(),
                        pasted_text: None,
                    });
                }
            }
            TailCommand::ParentDirectory => match &mut app.mode {
                Mode::FileBrowser(browser) => {
                    if let Err(error) = browser.parent() {
                        app.notice = Some(format!("Could not open parent: {error:#}"));
                    }
                }
                Mode::SaveBrowser(save) => {
                    if let Err(error) = save.browser.parent() {
                        app.notice = Some(format!("Could not open parent: {error:#}"));
                    }
                }
                _ => {}
            },
            TailCommand::SaveHere => {
                if let Mode::SaveBrowser(save) = &app.mode {
                    let item = save.item.clone();
                    let directory = save.browser.directory.clone();
                    if self.cache.destination_path(&item, &directory).exists() {
                        app.mode = Mode::SaveConflict { item, directory };
                    } else {
                        finish_save(
                            app,
                            self.cache,
                            &item,
                            &directory,
                            SaveConflictResolution::Rename,
                        )?;
                    }
                }
            }
            TailCommand::KeepBoth => {
                if let Mode::SaveConflict { item, directory } = &app.mode {
                    let item = item.clone();
                    let directory = directory.clone();
                    finish_save(
                        app,
                        self.cache,
                        &item,
                        &directory,
                        SaveConflictResolution::Rename,
                    )?;
                }
            }
            TailCommand::Replace => {
                if let Mode::SaveConflict { item, directory } = &app.mode {
                    let item = item.clone();
                    let directory = directory.clone();
                    finish_save(
                        app,
                        self.cache,
                        &item,
                        &directory,
                        SaveConflictResolution::Replace,
                    )?;
                }
            }
            TailCommand::CancelOperation => {
                if app.sends.mark_cancelling() {
                    let operation = self
                        .operation
                        .as_ref()
                        .expect("an active send owns a supervised operation");
                    let _ = operation.cancel.send(true);
                    app.notice = Some("Cancelling and reaping the active send…".into());
                }
            }
            TailCommand::ResumeReceiving => {
                app.watch = true;
                sync_receiver(app, self.client, self.cache, self.receiver_tx, self.receiver)
                    .await?;
                app.notice = Some("Automatic receiving is active".into());
            }
            TailCommand::CancelLogin => {
                cancel_login(self.login).await;
                app.notice = Some("Authentication cancelled; Enter retries".into());
            }
            TailCommand::ConfirmQuit => return Ok(Flow::Quit),
            TailCommand::Quit => return Ok(request_quit(app)),
        }
        app.visit_current_location();
        Ok(Flow::Continue)
    }
}

fn handle_key(key: KeyEvent, app: &mut App, regions: &UiRegions, login_idle: bool) -> Flow {
    if matches!(key.code, KeyCode::Char('c' | 'q')) && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return request_quit(app);
    }
    if key.code == KeyCode::Char('q')
        && (matches!(app.mode, Mode::Auth)
            || (matches!(app.mode, Mode::Workspace) && app.active_region != ActiveRegion::Composer))
    {
        return request_quit(app);
    }
    if key.modifiers.is_empty()
        && app.location().is_some()
        && !(matches!(app.mode, Mode::Workspace) && app.active_region == ActiveRegion::Composer)
    {
        match key.code {
            KeyCode::Left => return Flow::NavigateHistory(-1),
            KeyCode::Right => return Flow::NavigateHistory(1),
            _ => {}
        }
    }
    let contributed = if matches!(app.mode, Mode::Workspace)
        && (app.active_region != ActiveRegion::Composer
            || key.modifiers.contains(KeyModifiers::CONTROL))
    {
        let Some(chord) = KeyChord::from_event(key) else {
            app.keybinding_state.cancel();
            return Flow::Continue;
        };
        let context = app.action_context();
        match app.registry.resolve_keybinding(&mut app.keybinding_state, chord, context) {
            KeybindingResolution::Invoke(invocation) => Some(invocation),
            KeybindingResolution::Pending => return Flow::Continue,
            KeybindingResolution::Unmatched | KeybindingResolution::UnmatchedSequence { .. } => {
                None
            }
        }
    } else {
        app.keybinding_state.cancel();
        None
    };
    let action = match (&app.mode, key.code) {
        (Mode::Auth, KeyCode::Char('o')) => Some(contributions::OPEN_LOGIN),
        (Mode::Auth, KeyCode::Char('c')) => Some(contributions::COPY_LOGIN),
        (Mode::Auth, KeyCode::Enter) if login_idle => Some(contributions::RETRY_LOGIN),
        (Mode::Auth, KeyCode::Esc) => Some(contributions::CANCEL_LOGIN),
        (Mode::Workspace, KeyCode::Enter)
            if app.active_region == ActiveRegion::Composer
                && key.modifiers.is_empty()
                && !app.composer.is_empty() =>
        {
            Some(contributions::SEND_TEXT)
        }
        (Mode::Workspace, KeyCode::Enter)
            if app.active_region == ActiveRegion::Devices && key.modifiers.is_empty() =>
        {
            Some(contributions::FOCUS_COMPOSER)
        }
        (Mode::ReviewFiles(_), KeyCode::Enter) => Some(contributions::SEND_FILES),
        (Mode::ReviewFiles(_), KeyCode::Char('t')) => Some(contributions::USE_TEXT),
        (Mode::Ambiguous { .. }, KeyCode::Char('a')) => Some(contributions::USE_FILES),
        (Mode::Ambiguous { .. }, KeyCode::Char('t')) => Some(contributions::USE_TEXT),
        (Mode::Detail { .. }, KeyCode::Char('c')) => Some(contributions::COPY),
        (Mode::ConfirmDelete, KeyCode::Char('y')) => Some(contributions::CONFIRM_DELETE),
        (Mode::ConfirmQuit(_), KeyCode::Char('y' | 'q') | KeyCode::Enter) => {
            Some(contributions::CONFIRM_QUIT)
        }
        (Mode::FileBrowser(_), KeyCode::Enter) => Some(contributions::OPEN_ENTRY),
        (Mode::FileBrowser(_), KeyCode::Char(' ')) => Some(contributions::TOGGLE_FILE),
        (Mode::FileBrowser(_), KeyCode::Char('s')) => Some(contributions::REVIEW_SELECTED),
        (Mode::FileBrowser(_) | Mode::SaveBrowser(_), KeyCode::Backspace) => {
            Some(contributions::PARENT_DIRECTORY)
        }
        (Mode::SaveBrowser(_), KeyCode::Enter) => Some(contributions::OPEN_ENTRY),
        (Mode::SaveBrowser(_), KeyCode::Char('s')) => Some(contributions::SAVE_HERE),
        (Mode::SaveConflict { .. }, KeyCode::Char('r')) => Some(contributions::KEEP_BOTH),
        (Mode::SaveConflict { .. }, KeyCode::Char('x')) => Some(contributions::REPLACE),
        (
            Mode::ReviewFiles(_)
            | Mode::Ambiguous { .. }
            | Mode::Search
            | Mode::Detail { .. }
            | Mode::ConfirmDelete
            | Mode::ConfirmQuit(_)
            | Mode::FileBrowser(_)
            | Mode::SaveBrowser(_)
            | Mode::SaveConflict { .. },
            KeyCode::Esc,
        )
        | (Mode::Search | Mode::Detail { .. }, KeyCode::Enter)
        | (Mode::ConfirmDelete | Mode::ConfirmQuit(_), KeyCode::Char('n')) => {
            Some(contributions::BACK)
        }
        _ => None,
    };
    let invocation = contributed
        .or_else(|| action.map(|action| ActionInvocation::new(action, app.action_context())));
    if let Some(invocation) = invocation {
        return Flow::Invoke(invocation);
    }
    match &mut app.mode {
        Mode::Auth => {}
        Mode::Workspace => match key.code {
            KeyCode::Tab => {
                app.active_region =
                    regions.navigation().next(app.active_region).unwrap_or(app.active_region);
            }
            KeyCode::BackTab => {
                app.active_region =
                    regions.navigation().previous(app.active_region).unwrap_or(app.active_region);
            }
            KeyCode::Esc if app.active_region == ActiveRegion::Composer => {
                app.active_region = ActiveRegion::Devices;
            }
            KeyCode::Enter
                if app.active_region == ActiveRegion::Composer
                    && key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                let selected = app.selected_recipient();
                if !app.composer.insert("\n", selected) {
                    app.notice = Some("Select a device that supports Taildrop".into());
                }
            }
            _ if app.active_region == ActiveRegion::Composer => {
                let selected = app.selected_recipient();
                if !app.composer.apply_edit_key(key, selected) {
                    app.notice = Some("Select a device that supports Taildrop".into());
                }
            }
            KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
            KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
            _ => {}
        },
        Mode::ReviewFiles(_)
        | Mode::Ambiguous { .. }
        | Mode::Detail { .. }
        | Mode::ConfirmDelete
        | Mode::ConfirmQuit(_) => {}
        Mode::Search => match key.code {
            KeyCode::Backspace => {
                app.search.pop();
                app.peer_index = 0;
                app.item_index = 0;
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.search.push(character);
                app.peer_index = 0;
                app.item_index = 0;
            }
            _ => {}
        },
        Mode::FileBrowser(browser) => match key.code {
            KeyCode::Up | KeyCode::Char('k') => browser.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => browser.move_selection(1),
            _ => {}
        },
        Mode::SaveBrowser(save) => match key.code {
            KeyCode::Up | KeyCode::Char('k') => save.browser.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => save.browser.move_selection(1),
            _ => {}
        },
        Mode::SaveConflict { .. } => {}
    }
    Flow::Continue
}

fn request_quit(app: &mut App) -> Flow {
    let at_risk = app.sends.unfinished_count()
        + app.sends.failed_count()
        + usize::from(!app.composer.is_empty());
    if at_risk == 0 {
        return Flow::Quit;
    }
    if !matches!(app.mode, Mode::ConfirmQuit(_)) {
        let previous = std::mem::replace(&mut app.mode, Mode::Workspace);
        app.mode = Mode::ConfirmQuit(Box::new(previous));
        app.notice = Some(format!("{at_risk} send(s) would be abandoned"));
    }
    Flow::Continue
}

async fn cancel_login(login: &mut Option<LoginOperation>) {
    if let Some(operation) = login.take() {
        let _ = operation.cancel.send(true);
        let _ = operation.task.await;
    }
}

async fn finish_login(login: &mut Option<LoginOperation>) {
    if let Some(operation) = login.take() {
        let _ = operation.task.await;
    }
}

fn begin_login(client: &TailClient) -> LoginOperation {
    let (events, cancel, task) = client.start_login();
    LoginOperation { events, cancel, task }
}

fn handle_paste(app: &mut App, raw: String) {
    if !matches!(app.mode, Mode::Workspace | Mode::FileBrowser(_)) {
        return;
    }
    app.replace_current_location();
    match classify(raw) {
        PastedInput::Text(text) => {
            if !matches!(app.mode, Mode::Workspace) {
                app.notice = Some("Finish choosing files before pasting a message".into());
            } else if app.composer.insert(&text, app.selected_recipient()) {
                app.active_region = ActiveRegion::Composer;
            } else {
                app.notice = Some("Select a device that supports Taildrop".into());
            }
        }
        PastedInput::Files { raw, paths } => {
            let recipient = match &app.mode {
                Mode::FileBrowser(browser) => browser.recipient.clone(),
                _ => app.selected_recipient(),
            };
            if let Some(recipient) = recipient {
                app.mode = Mode::ReviewFiles(FileReview {
                    recipient,
                    paths,
                    pasted_text: Some(Zeroizing::new(raw)),
                });
            } else {
                app.notice = Some("Select a device that supports Taildrop".into());
            }
        }
        ambiguous @ PastedInput::Ambiguous { .. } => {
            if let Some(recipient) = app.selected_recipient() {
                app.mode = Mode::Ambiguous { input: ambiguous, recipient };
            } else {
                app.notice = Some("Select a device that supports Taildrop".into());
            }
        }
    }
    app.visit_current_location();
}

fn move_selection(app: &mut App, delta: isize) {
    let len = match app.active_region {
        ActiveRegion::Devices => app.filtered_peers().len(),
        ActiveRegion::Items => app.filtered_items().len(),
        ActiveRegion::Composer => return,
    };
    let index = match app.active_region {
        ActiveRegion::Devices => &mut app.peer_index,
        ActiveRegion::Items => &mut app.item_index,
        ActiveRegion::Composer => return,
    };
    if len > 0 {
        *index = (*index as isize + delta).clamp(0, len as isize - 1) as usize;
    }
}

fn start_next_send(
    app: &mut App,
    client: &TailClient,
    sender: &mpsc::Sender<BackendEvent>,
    operation: &mut Option<Operation>,
) {
    if operation.is_some() || app.local.is_none() {
        return;
    }
    let Some(work) = app.sends.start_next() else { return };
    let client = client.clone();
    let sender = sender.clone();
    let (cancel, cancel_receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let result = match work.payload {
            SendPayload::Text(text) => {
                client
                    .send_text(&text_name(work.id), text, &work.recipient.target, cancel_receiver)
                    .await
            }
            SendPayload::Files(paths) => {
                client.send_files(&paths, &work.recipient.target, cancel_receiver).await
            }
        }
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::SendFinished { id: work.id, result }).await;
    });
    *operation = Some(Operation { cancel, task });
}

fn start_receiver(
    client: &TailClient,
    cache: &ReceiveCache,
    sender: &mpsc::Sender<ReceiverEvent>,
    generation: u64,
) -> Operation {
    let client = client.clone();
    let cache = cache.clone();
    let sender = sender.clone();
    let (cancel, mut cancel_receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut failures = 0_u32;
        let lease = loop {
            if *cancel_receiver.borrow() {
                return;
            }
            match cache.try_receiver_lease() {
                Ok(ReceiverLeaseAttempt::Acquired(lease)) => break lease,
                Ok(ReceiverLeaseAttempt::Busy) => {
                    failures = 0;
                    if !send_receiver_event(
                        &sender,
                        &mut cancel_receiver,
                        generation,
                        ReceiverEventKind::State(ReceiverState::Standby),
                    )
                    .await
                        || receiver_delay(&mut cancel_receiver, RECEIVER_LEASE_RETRY_INTERVAL).await
                    {
                        return;
                    }
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let delay = receive_retry_delay(failures);
                    if !send_receiver_retry(
                        &sender,
                        &mut cancel_receiver,
                        generation,
                        &error,
                        delay,
                    )
                    .await
                        || receiver_delay(&mut cancel_receiver, delay).await
                    {
                        return;
                    }
                }
            }
        };

        failures = 0;
        if !send_receiver_event(
            &sender,
            &mut cancel_receiver,
            generation,
            ReceiverEventKind::State(ReceiverState::Importing),
        )
        .await
        {
            return;
        }
        match lease.recover_staging() {
            Ok(items) if !items.is_empty() => {
                if !send_receiver_event(
                    &sender,
                    &mut cancel_receiver,
                    generation,
                    ReceiverEventKind::Received(items),
                )
                .await
                {
                    return;
                }
            }
            Ok(_) => {}
            Err(error) => {
                if !send_receiver_event(
                    &sender,
                    &mut cancel_receiver,
                    generation,
                    ReceiverEventKind::RecoveryFailed(format!("{error:#}")),
                )
                .await
                {
                    return;
                }
            }
        }

        loop {
            if *cancel_receiver.borrow() {
                break;
            }
            if !send_receiver_event(
                &sender,
                &mut cancel_receiver,
                generation,
                ReceiverEventKind::State(ReceiverState::Waiting),
            )
            .await
            {
                break;
            }
            let received = async {
                let staging = cache.staging_directory()?;
                client.receive_into(staging.path(), cancel_receiver.clone()).await?;
                Ok::<_, anyhow::Error>(staging)
            }
            .await;
            let result = match received {
                Ok(staging) => {
                    let completed = staging.complete();
                    completed.and_then(|staging| {
                        let _ = sender.try_send(ReceiverEvent {
                            generation,
                            kind: ReceiverEventKind::State(ReceiverState::Importing),
                        });
                        cache.import_staging(staging)
                    })
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(items) => {
                    failures = 0;
                    if !items.is_empty()
                        && !send_receiver_event(
                            &sender,
                            &mut cancel_receiver,
                            generation,
                            ReceiverEventKind::Received(items),
                        )
                        .await
                    {
                        break;
                    }
                }
                Err(_) if *cancel_receiver.borrow() => break,
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let delay = receive_retry_delay(failures);
                    if !send_receiver_retry(
                        &sender,
                        &mut cancel_receiver,
                        generation,
                        &error,
                        delay,
                    )
                    .await
                        || receiver_delay(&mut cancel_receiver, delay).await
                    {
                        break;
                    }
                }
            }
        }
        drop(lease);
    });
    Operation { cancel, task }
}

async fn send_receiver_event(
    sender: &mpsc::Sender<ReceiverEvent>,
    cancel: &mut watch::Receiver<bool>,
    generation: u64,
    kind: ReceiverEventKind,
) -> bool {
    if *cancel.borrow() {
        return false;
    }
    tokio::select! {
        biased;
        changed = cancel.changed() => {
            let _ = changed;
            false
        }
        result = sender.send(ReceiverEvent { generation, kind }) => result.is_ok(),
    }
}

async fn send_receiver_retry(
    sender: &mpsc::Sender<ReceiverEvent>,
    cancel: &mut watch::Receiver<bool>,
    generation: u64,
    error: &anyhow::Error,
    delay: Duration,
) -> bool {
    send_receiver_event(
        sender,
        cancel,
        generation,
        ReceiverEventKind::Retry {
            error: format!("{error:#}"),
            delay,
            retry_at: Instant::now() + delay,
        },
    )
    .await
}

async fn receiver_delay(cancel: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        changed = cancel.changed() => {
            let _ = changed;
            true
        }
    }
}

fn receive_retry_delay(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 | 1 => 1,
        2 => 2,
        3 => 5,
        4 => 10,
        _ => 30,
    })
}

async fn sync_receiver(
    app: &mut App,
    client: &TailClient,
    cache: &ReceiveCache,
    sender: &mpsc::Sender<ReceiverEvent>,
    receiver: &mut Option<Operation>,
) -> Result<()> {
    let should_run = app.watch && app.local.is_some();
    match (should_run, receiver.is_some()) {
        (true, false) => {
            app.receiver_generation = app.receiver_generation.wrapping_add(1);
            app.receiver_state = ReceiverState::Starting;
            *receiver = Some(start_receiver(client, cache, sender, app.receiver_generation));
        }
        (false, true) => {
            app.receiver_generation = app.receiver_generation.wrapping_add(1);
            stop_operation(receiver).await;
            refresh_received_items(app, cache)?;
        }
        _ => {}
    }
    Ok(())
}

async fn stop_operation(operation: &mut Option<Operation>) {
    if let Some(operation) = operation.take() {
        let _ = operation.cancel.send(true);
        let _ = operation.task.await;
    }
}

fn finish_save(
    app: &mut App,
    cache: &ReceiveCache,
    item: &CachedItem,
    directory: &Path,
    conflict: SaveConflictResolution,
) -> Result<()> {
    match cache.save_to(item, directory, conflict) {
        Ok(path) => {
            app.items = cache.list()?;
            app.item_index = 0;
            app.mode = Mode::Workspace;
            app.notice = Some(format!("Saved {}", path.display()));
        }
        Err(error) => app.notice = Some(format!("Could not save item: {error:#}")),
    }
    Ok(())
}

fn text_name(send_id: u64) -> String {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_millis());
    format!("clipboard-{timestamp}-{send_id}.txt")
}

fn open_detail(app: &mut App, cache: &ReceiveCache) {
    let Some(item) = app.selected_item().cloned() else { return };
    let preview = if item.kind == ItemKind::Text {
        match cache.read_text(&item) {
            Ok(text) => Some(Zeroizing::new(text)),
            Err(error) => {
                app.notice = Some(format!("Could not read {}: {error:#}", item.name));
                None
            }
        }
    } else {
        None
    };
    app.mode = Mode::Detail { preview };
}

fn render(frame: &mut Frame<'_>, app: &App) -> UiRegions {
    let mut regions = UiRegions::default();
    let (header, body, composer, footer) = if matches!(app.mode, Mode::Workspace) {
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(6),
            Constraint::Length(3),
        ])
        .split(frame.area());
        (rows[0], rows[1], Some(rows[2]), rows[3])
    } else {
        let rows =
            Layout::vertical([Constraint::Length(2), Constraint::Min(6), Constraint::Length(3)])
                .split(frame.area());
        (rows[0], rows[1], None, rows[2])
    };
    render_header(frame, header, app, &mut regions);
    match &app.mode {
        Mode::Workspace => {
            render_browser(frame, body, app, &mut regions);
            render_composer(
                frame,
                composer.expect("workspace layout includes composer"),
                app,
                &mut regions,
            );
        }
        Mode::ReviewFiles(review) => render_review_files(frame, body, review),
        Mode::Ambiguous { input, .. } => render_ambiguous(frame, body, input),
        Mode::Search => render_browser(frame, body, app, &mut regions),
        Mode::Detail { preview } => {
            render_detail(frame, body, app, preview.as_ref().map(|text| text.as_str()))
        }
        Mode::ConfirmDelete => render_confirm_delete(frame, body, app),
        Mode::ConfirmQuit(_) => render_confirm_quit(frame, body, app),
        Mode::FileBrowser(browser) => {
            let offset = render_file_browser(frame, body, browser);
            regions.rows.extend(
                row_regions(body, browser.entries.len(), offset)
                    .map(|(area, index)| (area, RowTarget::File(index))),
            );
        }
        Mode::SaveBrowser(save) => {
            let offset = render_save_browser(frame, body, save);
            regions.rows.extend(
                row_regions(body, save.browser.entries.len(), offset)
                    .map(|(area, index)| (area, RowTarget::SaveDirectory(index))),
            );
        }
        Mode::SaveConflict { item, directory } => {
            render_save_conflict(frame, body, item, directory)
        }
        Mode::Auth => render_auth(frame, body, app),
    }
    render_footer(frame, footer, app, &mut regions);
    if let Some(Overlay::ContextMenu(menu)) = &app.overlay {
        let layout = menu.layout(frame.area());
        menu.render(frame, &layout, ContextMenuStyle::default());
        regions.context_menu = Some(layout);
    }
    regions
}

fn row_regions(area: Rect, count: usize, offset: usize) -> impl Iterator<Item = (Rect, usize)> {
    let visible = count.saturating_sub(offset).min(usize::from(area.height.saturating_sub(2)));
    let width = area.width.saturating_sub(2);
    (0..visible).map(move |visible_index| {
        (
            Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(1 + visible_index as u16),
                width,
                1,
            ),
            offset + visible_index,
        )
    })
}

fn panel(title: &'static str, focused: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let local = app.local.as_ref().map_or("not connected", |device| device.name.as_str());
    let (receiver, receiver_color) = receiver_indicator(app);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " kit tail ",
                Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(local, Style::default().fg(TEXT)),
            Span::styled(
                if app.local.is_some() { "  ● live" } else { "" },
                Style::default().fg(GOOD),
            ),
            Span::styled(receiver, Style::default().fg(receiver_color)),
            Span::styled(
                if app.sends.active() {
                    format!("  {} sending", SPINNER[app.spinner % SPINNER.len()])
                } else {
                    String::new()
                },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                if app.sends.queued_count() > 0 {
                    format!("  {} queued", app.sends.queued_count())
                } else {
                    String::new()
                },
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if app.sends.failed_count() > 0 {
                    format!("  {} failed", app.sends.failed_count())
                } else {
                    String::new()
                },
                Style::default().fg(WARN),
            ),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
    let width = area.width.min(8);
    if width > 0 {
        let quit = Rect::new(area.right().saturating_sub(width), area.y, width, 1);
        frame.render_widget(
            Paragraph::new(" × Quit ")
                .alignment(Alignment::Right)
                .style(Style::default().fg(MUTED)),
            quit,
        );
        regions.inline_actions.push((quit, contributions::QUIT));
    }
}

fn receiver_indicator(app: &App) -> (String, Color) {
    if !app.watch || app.local.is_none() {
        return (String::new(), GOOD);
    }
    match app.receiver_state {
        ReceiverState::Starting => {
            (format!("  {} receiver", SPINNER[app.spinner % SPINNER.len()]), ACCENT)
        }
        ReceiverState::Standby => ("  ○ receiver elsewhere".into(), MUTED),
        ReceiverState::Waiting => ("  ● waiting".into(), GOOD),
        ReceiverState::Importing => {
            (format!("  {} receiving", SPINNER[app.spinner % SPINNER.len()]), ACCENT)
        }
        ReceiverState::Retrying { retry_at } => {
            let remaining = retry_at.saturating_duration_since(Instant::now());
            let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
            (format!("  ⚠ retry {seconds}s"), WARN)
        }
    }
}

fn render_browser(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let (devices, inbox) = if area.width < 80 {
        let columns =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
        (columns[0], columns[1])
    } else {
        let split = SplitFrame::horizontal(area, app.split_ratio, SplitMinimums::new(30, 36));
        render_split_divider(
            frame,
            split,
            app.split_drag.is_some(),
            SplitDividerStyle {
                idle_color: MUTED,
                active_color: ACCENT,
                idle_line: "│",
                idle_grip: "┋",
                active_line: "┃",
            },
        );
        regions.split = Some(split);
        (split.first, split.second)
    };
    regions.device_panel = Some(devices);
    regions.item_panel = Some(inbox);
    let filtered_peers = app.filtered_peers();
    let peers = filtered_peers
        .iter()
        .enumerate()
        .map(|(index, peer)| {
            let status = if peer.taildrop_target.is_none() {
                "×"
            } else if peer.online {
                "●"
            } else {
                "○"
            };
            let style = if index == app.peer_index {
                if app.active_region == ActiveRegion::Devices {
                    Style::default().fg(Color::Black).bg(ACCENT)
                } else {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                }
            } else {
                Style::default().fg(if peer.taildrop_target.is_some() && peer.online {
                    TEXT
                } else {
                    MUTED
                })
            };
            ListItem::new(format!(" {status} {:<24} {}", peer.name, peer.operating_system.label()))
                .style(style)
        })
        .collect::<Vec<_>>();
    let mut peer_state =
        ListState::default().with_selected((!filtered_peers.is_empty()).then_some(app.peer_index));
    frame.render_stateful_widget(
        List::new(peers)
            .block(panel(" Devices · send to ", app.active_region == ActiveRegion::Devices)),
        devices,
        &mut peer_state,
    );
    regions.rows.extend(
        row_regions(devices, filtered_peers.len(), peer_state.offset())
            .map(|(area, index)| (area, RowTarget::Peer(index))),
    );
    let filtered_items = app.filtered_items();
    let items = filtered_items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let kind = if item.kind == ItemKind::Text { "text" } else { "file" };
            let style = if index == app.item_index && app.active_region == ActiveRegion::Items {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(format!(" {:<28} {:>8}  {kind}", item.name, human_bytes(item.bytes)))
                .style(style)
        })
        .collect::<Vec<_>>();
    let mut item_state =
        ListState::default().with_selected((!filtered_items.is_empty()).then_some(app.item_index));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(" Received · 30-day cache ", app.active_region == ActiveRegion::Items)),
        inbox,
        &mut item_state,
    );
    regions.rows.extend(
        row_regions(inbox, filtered_items.len(), item_state.offset())
            .map(|(area, index)| (area, RowTarget::Item(index))),
    );
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    regions.composer_panel = Some(area);
    let selected = app.selected_recipient();
    let recipient = app.composer.recipient().or(selected.as_ref());
    let recipient_name = recipient.map_or("select a device", |recipient| recipient.name.as_str());
    let locked = app.composer.can_retarget(selected.as_ref());
    let title = if locked {
        format!(" Message → {recipient_name} · recipient locked to this draft ")
    } else {
        format!(" Message → {recipient_name} ")
    };
    let focused = app.active_region == ActiveRegion::Composer;
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    let activity = if let Some(latest) = app.sends.latest_status() {
        let queued = app.sends.queued_count();
        let failed = app.sends.failed_count();
        let spinner = if app.sends.active() {
            format!("{} ", SPINNER[app.spinner % SPINNER.len()])
        } else {
            String::new()
        };
        format!("{spinner}{latest}  ·  {queued} queued  ·  {failed} failed")
    } else {
        "Ready · Enter sends and keeps this editor open".into()
    };
    frame.render_widget(
        Paragraph::new(activity).style(Style::default().fg(if app.sends.failed_count() > 0 {
            WARN
        } else {
            MUTED
        })),
        rows[0],
    );
    let input = rows[1];
    let (vertical_scroll, horizontal_scroll, cursor_row, cursor_column) =
        composer_viewport(&app.composer, input);
    let content = if app.composer.is_empty() {
        "Type or paste here…  Drag files anywhere in the TUI."
    } else {
        app.composer.text()
    };
    frame.render_widget(
        Paragraph::new(content)
            .style(Style::default().fg(if app.composer.is_empty() { MUTED } else { TEXT }))
            .scroll((vertical_scroll, horizontal_scroll)),
        input,
    );
    if focused && app.overlay.is_none() && input.width > 0 && input.height > 0 {
        frame.set_cursor_position(Position {
            x: input.x.saturating_add(cursor_column),
            y: input.y.saturating_add(cursor_row),
        });
    }
}

fn composer_input_area(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(2),
        area.width.saturating_sub(2),
        area.height.saturating_sub(3),
    )
}

fn composer_viewport(composer: &Composer, area: Rect) -> (u16, u16, u16, u16) {
    let before_cursor = &composer.text()[..composer.cursor()];
    let line = before_cursor.rsplit('\n').next().unwrap_or_default();
    let logical_row = before_cursor.chars().filter(|character| *character == '\n').count();
    let logical_column = line
        .chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum::<usize>();
    let visible_height = usize::from(area.height.max(1));
    let visible_width = usize::from(area.width.max(1));
    let vertical = logical_row.saturating_sub(visible_height.saturating_sub(1));
    let horizontal = logical_column.saturating_sub(visible_width.saturating_sub(1));
    (
        u16::try_from(vertical).unwrap_or(u16::MAX),
        u16::try_from(horizontal).unwrap_or(u16::MAX),
        u16::try_from(logical_row.saturating_sub(vertical)).unwrap_or(u16::MAX),
        u16::try_from(logical_column.saturating_sub(horizontal)).unwrap_or(u16::MAX),
    )
}

fn render_review_files(frame: &mut Frame<'_>, area: Rect, review: &FileReview) {
    let paths =
        review.paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("\n");
    let alternative = if review.pasted_text.is_some() {
        "\n\nt  Insert the original paste as text instead"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{} file(s) → {}\n\n{}{}",
            review.paths.len(),
            review.recipient.name,
            paths,
            alternative,
        ))
        .wrap(Wrap { trim: false })
        .block(panel(" Confirm file send ", true)),
        area,
    );
}

fn render_ambiguous(frame: &mut Frame<'_>, area: Rect, input: &PastedInput) {
    let PastedInput::Ambiguous { existing, missing, .. } = input else { return };
    frame.render_widget(Paragraph::new(format!("This paste mixes {} existing file(s) with text or missing paths:\n\n{}\n\na  Add existing files\nt  Treat the entire paste as text", existing.len(), missing.join("\n"))).wrap(Wrap { trim: false }).block(panel(" What did you mean? ", true)), area);
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, app: &App, preview: Option<&str>) {
    let text = app.selected_item().map_or_else(
        || "No item".into(),
        |item| {
            let details = format!(
                "Kind: {:?} · Size: {}\nReceived: {} · Expires: {}\nCache ID: {}",
                item.kind,
                human_bytes(item.bytes),
                item.received_at,
                item.expires_at(),
                item.id,
            );
            if item.kind == ItemKind::Text {
                format!(
                    "{}\n\n{}\n\n────────────────────\n{}\n\nc  Copy to clipboard",
                    item.name,
                    preview.unwrap_or("Text preview unavailable"),
                    details,
                )
            } else {
                format!("{}\n\n{}", item.name, details)
            }
        },
    );
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(panel(" Received item ", true)),
        area,
    );
}

fn render_confirm_delete(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let name = app.selected_item().map_or("this item", |item| item.name.as_str());
    frame.render_widget(Paragraph::new(format!("Delete {name} from Kit's receive cache?\n\nThis does not touch saved copies.\n\ny  delete     n/Esc  cancel")).block(panel(" Confirm deletion ", true)), area);
}

fn render_confirm_quit(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let unfinished = app.sends.unfinished_count();
    let failed = app.sends.failed_count();
    let draft = if app.composer.is_empty() {
        String::new()
    } else {
        "\nThe unsent message draft will be discarded.".into()
    };
    frame.render_widget(
        Paragraph::new(format!(
            "Quit Kit Tail?\n\n{unfinished} active or queued send(s) will be cancelled.\n{failed} failed send(s) will lose their in-memory retry payload.{draft}\n\nReceived cache items are safe.\n\ny/Enter  quit now     n/Esc  stay",
        ))
        .wrap(Wrap { trim: false })
        .block(panel(" Sends are still in this session ", true)),
        area,
    );
}

fn render_file_browser(frame: &mut Frame<'_>, area: Rect, browser: &FileBrowser) -> usize {
    let items = browser
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let marker = if entry.directory {
                "▸"
            } else if browser.selected.contains(&entry.path) {
                "✓"
            } else {
                " "
            };
            let style = if index == browser.index {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else if entry.directory {
                Style::default().fg(ACCENT)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(format!(" {marker} {}", entry.name)).style(style)
        })
        .collect::<Vec<_>>();
    let recipient = browser
        .recipient
        .as_ref()
        .map_or(String::new(), |recipient| format!(" → {}", recipient.name));
    let mut state =
        ListState::default().with_selected((!browser.entries.is_empty()).then_some(browser.index));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .title(format!(
                    " Choose files{recipient} · {} · {} selected ",
                    browser.directory.display(),
                    browser.selected.len()
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
        &mut state,
    );
    state.offset()
}

fn render_save_browser(frame: &mut Frame<'_>, area: Rect, save: &SaveBrowser) -> usize {
    let items = save
        .browser
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let style = if index == save.browser.index {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else if entry.directory {
                Style::default().fg(ACCENT)
            } else {
                Style::default().fg(MUTED)
            };
            let marker = if entry.directory { "▸" } else { " " };
            ListItem::new(format!(" {marker} {}", entry.name)).style(style)
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default()
        .with_selected((!save.browser.entries.is_empty()).then_some(save.browser.index));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .title(format!(" Save {} in {} ", save.item.name, save.browser.directory.display()))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(GOOD)),
        ),
        area,
        &mut state,
    );
    state.offset()
}

fn render_save_conflict(frame: &mut Frame<'_>, area: Rect, item: &CachedItem, directory: &Path) {
    frame.render_widget(
        Paragraph::new(format!(
            "{} already exists in {}.\n\nr  Keep both (rename the new item)\nx  Replace the existing file safely\nEsc  Choose another folder",
            item.name,
            directory.display()
        ))
        .wrap(Wrap { trim: false })
        .block(panel(" Filename conflict ", true)),
        area,
    );
}

fn render_auth(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let body = match &app.login_url {
        Some(url) => format!("Tailscale needs authentication.\n\n{url}\n\no  Open link\nc  Copy link\n\nKit will continue automatically when this device is connected."),
        None if app.auth_can_login => "Connecting to Tailscale…\n\nWaiting for the authentication link.\n\nEsc  cancel".into(),
        None => format!("{}\n\nEnter  retry preflight", app.notice.as_deref().unwrap_or("Tailscale is unavailable")),
    };
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(panel(" Connect Tailscale ", true)),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let controls = match (&app.mode, app.active_region) {
        (Mode::Workspace, ActiveRegion::Composer) => {
            "Enter send · Alt/Shift+Enter newline · Tab focus · Ctrl+F files · Esc devices"
        }
        (Mode::Workspace, ActiveRegion::Devices) => {
            "↑↓ device · Enter write · Tab focus · drag files · right-click actions"
        }
        (Mode::Workspace, ActiveRegion::Items) => {
            "↑↓ received · Enter inspect · Tab focus · right-click actions"
        }
        (Mode::ReviewFiles(_), _) => {
            "Enter send files · t insert as text when available · Esc back"
        }
        (Mode::Search, _) => "type to filter   Enter/Esc done",
        (Mode::Detail { .. }, _) => "c copy text · Enter/Esc back · ←/→ history",
        (Mode::FileBrowser(_), _) => {
            "Enter open/review file · Space select · s review selected · Backspace parent · Esc"
        }
        (Mode::SaveBrowser(_), _) => {
            "Enter open folder   s save here   Backspace parent   Esc cancel"
        }
        (Mode::SaveConflict { .. }, _) => "r keep both   x replace   Esc choose another folder",
        (Mode::ConfirmQuit(_), _) => "Enter/y quit now · n/Esc stay",
        _ => "Esc back",
    };
    let notice = app.notice.as_deref().unwrap_or(controls);
    let notice = if matches!(app.mode, Mode::Auth) && app.auth_can_login {
        format!("{} {notice}", SPINNER[app.spinner % SPINNER.len()])
    } else {
        notice.to_owned()
    };
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            notice,
            Style::default().fg(if app.notice.is_some() { WARN } else { MUTED }),
        )),
        rows[0],
    );
    let actions = match app.mode {
        Mode::Workspace => app.registry.resolve_menu(WORKSPACE_INLINE, &app.action_context()),
        Mode::Auth => app.registry.resolve_menu(AUTH_INLINE, &app.action_context()),
        _ => app.registry.resolve_menu(MODAL_INLINE, &app.action_context()),
    };
    render_inline_actions(frame, rows[1], actions.items(), regions);
}

fn render_inline_actions(
    frame: &mut Frame<'_>,
    area: Rect,
    actions: &[ResolvedAction],
    regions: &mut UiRegions,
) {
    let mut spans = Vec::new();
    let mut x = area.x;
    for action in actions {
        let shortcut =
            action.primary_keybinding().map_or_else(|| "·".into(), |key| key.to_string());
        let label = format!(" {shortcut} {} ", action.title);
        let width = u16::try_from(label.chars().count())
            .unwrap_or(u16::MAX)
            .min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        spans.push(Span::styled(
            label,
            Style::default().fg(match action.state {
                ActionState::Enabled => ACCENT,
                ActionState::Disabled { .. } => MUTED,
            }),
        ));
        regions.inline_actions.push((Rect::new(x, area.y, width, 1), action.id));
        x = x.saturating_add(width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{MouseEvent, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    #[test]
    fn browser_storyboard_renders_devices_cache_and_controls() {
        let readiness = Readiness::Ready {
            local: device("desktop", "100.64.0.1"),
            peers: vec![device("laptop", "100.64.0.2")],
        };
        let app = App::new(readiness, Vec::new());
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(screen.contains("kit tail"));
        assert!(screen.contains("laptop"));
        assert!(screen.contains("Received · 30-day cache"));
        assert!(screen.contains("Message → laptop"));
        assert!(screen.contains("keeps this editor open"));
    }

    #[test]
    fn narrow_storyboard_stacks_without_panicking() {
        let app = ready_app();
        let mut terminal = Terminal::new(TestBackend::new(48, 16)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
        let screen = screen(&terminal);
        assert!(screen.contains("kit tail"));
        assert!(screen.contains("Devices · send to"));
        assert!(screen.contains("Received · 30-day cache"));
        assert!(screen.contains("Message → laptop"));
    }

    #[test]
    fn receiver_header_distinguishes_waiting_standby_and_retrying() {
        let mut app = ready_app();

        app.receiver_state = ReceiverState::Waiting;
        assert_eq!(receiver_indicator(&app).0, "  ● waiting");

        app.receiver_state = ReceiverState::Standby;
        assert_eq!(receiver_indicator(&app).0, "  ○ receiver elsewhere");

        app.receiver_state =
            ReceiverState::Retrying { retry_at: Instant::now() + Duration::from_secs(5) };
        assert!(receiver_indicator(&app).0.starts_with("  ⚠ retry "));

        app.watch = false;
        assert!(receiver_indicator(&app).0.is_empty());
    }

    #[test]
    fn file_browser_storyboard_shows_files_and_selection_controls() {
        let directory =
            std::env::temp_dir().join(format!("kit-tail-browser-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("drop me.txt"), "payload").unwrap();
        let mut app = ready_app();
        app.mode = Mode::FileBrowser(FileBrowser::open(directory.clone()).unwrap());
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
        let screen = screen(&terminal);
        assert!(screen.contains("Choose files"));
        assert!(screen.contains("drop me.txt"));
        assert!(screen.contains("Space select"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn received_text_detail_shows_the_paste_and_copy_action() {
        let root = std::env::temp_dir().join(format!("kit-tail-preview-{}", uuid::Uuid::new_v4()));
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let staging = cache.staging_directory().unwrap();
        fs::write(staging.path().join("clipboard.txt"), "the actual paste\nsecond line").unwrap();
        let items = cache.import_staging(staging.complete().unwrap()).unwrap();
        let mut app = App::new(
            Readiness::Ready {
                local: device("desktop", "100.64.0.1"),
                peers: vec![device("laptop", "100.64.0.2")],
            },
            items,
        );
        app.active_region = ActiveRegion::Items;
        open_detail(&mut app, &cache);

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
        let screen = screen(&terminal);
        assert!(screen.contains("the actual paste"));
        assert!(screen.contains("second line"));
        assert!(screen.contains("Copy to clipboard"));

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_refresh_adds_new_devices_without_resetting_the_draft() {
        let mut app = ready_app();
        let recipient = app.selected_recipient().unwrap();
        assert!(app.composer.insert("keep this draft", Some(recipient)));

        app.reconcile_readiness(Readiness::Ready {
            local: device("desktop", "100.64.0.1"),
            peers: vec![device("laptop", "100.64.0.2"), device("new-machine", "100.64.0.3")],
        });

        assert_eq!(app.peers.len(), 2);
        assert_eq!(app.selected_peer().map(|peer| peer.name.as_str()), Some("laptop"));
        assert!(matches!(app.mode, Mode::Workspace));
        assert_eq!(app.composer.text(), "keep this draft");
        assert_eq!(
            app.composer.recipient().map(|recipient| recipient.name.as_str()),
            Some("laptop")
        );
    }

    #[test]
    fn live_refresh_preserves_the_selected_device_by_identity() {
        let mut app = App::new(
            Readiness::Ready {
                local: device("desktop", "100.64.0.1"),
                peers: vec![device("alpha", "100.64.0.2"), device("zulu", "100.64.0.3")],
            },
            Vec::new(),
        );
        app.peer_index = 1;

        app.reconcile_readiness(Readiness::Ready {
            local: device("desktop", "100.64.0.1"),
            peers: vec![
                device("aardvark", "100.64.0.4"),
                device("alpha", "100.64.0.2"),
                device("zulu", "100.64.0.3"),
            ],
        });

        assert_eq!(app.selected_peer().map(|peer| peer.name.as_str()), Some("zulu"));
    }

    #[test]
    fn wide_browser_uses_shared_navigation_and_resizable_split() {
        let app = ready_app();
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        let mut regions = UiRegions::default();
        terminal.draw(|frame| regions = render(frame, &app)).unwrap();

        assert!(regions.split.is_some());
        assert_eq!(regions.navigation().next(ActiveRegion::Devices), Some(ActiveRegion::Items));
    }

    #[test]
    fn right_click_opens_the_shared_item_context_menu() {
        let root = std::env::temp_dir().join(format!("kit-tail-menu-{}", uuid::Uuid::new_v4()));
        let cache = ReceiveCache::at(root.clone()).unwrap();
        let staging = cache.staging_directory().unwrap();
        fs::write(staging.path().join("clipboard.txt"), "copy me").unwrap();
        let items = cache.import_staging(staging.complete().unwrap()).unwrap();
        let mut app = App::new(
            Readiness::Ready {
                local: device("desktop", "100.64.0.1"),
                peers: vec![device("laptop", "100.64.0.2")],
            },
            items,
        );
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        let mut regions = UiRegions::default();
        terminal.draw(|frame| regions = render(frame, &app)).unwrap();
        let item_row = regions
            .rows
            .iter()
            .find(|(_, target)| matches!(target, RowTarget::Item(0)))
            .map(|(area, _)| *area)
            .unwrap();
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: item_row.x,
                row: item_row.y,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &regions,
        );

        assert!(matches!(app.overlay, Some(Overlay::ContextMenu(_))));
        terminal.draw(|frame| regions = render(frame, &app)).unwrap();
        assert!(regions.context_menu.is_some());
        let screen = screen(&terminal);
        assert!(screen.contains("Copy to clipboard"));
        assert!(screen.contains("Delete from cache"));
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dragging_split_persists_the_shared_ratio() {
        let root = std::env::temp_dir().join(format!("kit-tail-split-{}", uuid::Uuid::new_v4()));
        let store = crate::framework::ConfigStore::rooted(root.clone());
        let mut config = Config::load(store.clone()).unwrap();
        let mut app = ready_app();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let mut regions = UiRegions::default();
        terminal.draw(|frame| regions = render(frame, &app)).unwrap();
        let split = regions.split.unwrap();
        let row = split.separator.y;

        for event in [
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: split.separator.x,
                row,
                modifiers: KeyModifiers::NONE,
            },
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: split.separator.x + 10,
                row,
                modifiers: KeyModifiers::NONE,
            },
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: split.separator.x + 10,
                row,
                modifiers: KeyModifiers::NONE,
            },
        ] {
            if let Flow::PersistSplitRatio(ratio) = handle_mouse(event, &mut app, &regions) {
                config.set_split_ratio(ratio).unwrap();
            }
        }

        assert_ne!(app.split_ratio, SplitRatio::new(440));
        assert_eq!(Config::load(store).unwrap().split_ratio(), app.split_ratio);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn key_input_returns_a_typed_action_intent() {
        let mut app = ready_app();
        let flow = handle_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
            &mut app,
            &UiRegions::default(),
            true,
        );

        assert!(matches!(
            flow,
            Flow::Invoke(ActionInvocation { action, .. })
                if action == contributions::TOGGLE_RECEIVING
        ));
    }

    #[test]
    fn quit_input_returns_flow_without_running_side_effects() {
        let mut app = ready_app();
        let flow = handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut app,
            &UiRegions::default(),
            true,
        );

        assert!(matches!(flow, Flow::Quit));
    }

    #[test]
    fn arrow_keys_navigate_lists_and_emit_history_intents() {
        let mut app = App::new(
            Readiness::Ready {
                local: device("desktop", "100.64.0.1"),
                peers: vec![device("laptop", "100.64.0.2"), device("tablet", "100.64.0.3")],
            },
            Vec::new(),
        );
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        let mut regions = UiRegions::default();
        terminal.draw(|frame| regions = render(frame, &app)).unwrap();

        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app, &regions, true,),
            Flow::Continue
        ));
        assert_eq!(app.selected_peer().map(|peer| peer.name.as_str()), Some("tablet"));

        handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut app, &regions, true);
        assert_eq!(app.active_region, ActiveRegion::Items);

        handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut app, &regions, true);
        assert_eq!(app.active_region, ActiveRegion::Composer);

        handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &mut app, &regions, true);
        assert_eq!(app.active_region, ActiveRegion::Items);

        handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &mut app, &regions, true);
        assert_eq!(app.active_region, ActiveRegion::Devices);

        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &mut app, &regions, true,),
            Flow::NavigateHistory(-1)
        ));
        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &mut app, &regions, true,),
            Flow::NavigateHistory(1)
        ));
    }

    #[test]
    fn pasted_text_stays_in_the_persistent_workspace() {
        let mut app = ready_app();

        handle_paste(&mut app, "share this".into());

        assert!(matches!(app.mode, Mode::Workspace));
        assert_eq!(app.active_region, ActiveRegion::Composer);
        assert_eq!(app.composer.text(), "share this");
        assert_eq!(
            app.composer.recipient().map(|recipient| recipient.name.as_str()),
            Some("laptop")
        );
    }

    #[test]
    fn composer_edits_in_place_and_keeps_the_original_recipient() {
        let first = Recipient::from_device(&device("laptop", "100.64.0.2")).unwrap();
        let second = Recipient::from_device(&device("tablet", "100.64.0.3")).unwrap();
        let mut composer = Composer::default();
        assert!(composer.insert("helo", Some(first.clone())));
        composer
            .apply_edit_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), Some(second.clone()));
        composer.apply_edit_key(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            Some(second.clone()),
        );
        composer
            .apply_edit_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), Some(second.clone()));
        assert!(composer.insert("\nsecond line", Some(second.clone())));

        assert_eq!(composer.text(), "hello\nsecond line");
        assert!(composer.can_retarget(Some(&second)));
        let (recipient, text) = composer.take().unwrap();
        assert_eq!(recipient, first);
        assert_eq!(text.as_str(), "hello\nsecond line");
        assert!(composer.is_empty());
    }

    #[test]
    fn vertical_composer_movement_preserves_the_display_column_for_wide_text() {
        let recipient = Recipient::from_device(&device("laptop", "100.64.0.2")).unwrap();
        let mut composer = Composer::default();
        assert!(composer.insert("界x\nab", Some(recipient)));

        composer.move_vertical(-1);
        assert_eq!(composer.cursor(), "界".len());
        composer.move_vertical(1);
        assert_eq!(composer.cursor(), composer.text().len());
    }

    #[test]
    fn arrow_keys_edit_the_composer_instead_of_navigating_history() {
        let mut app = ready_app();
        app.active_region = ActiveRegion::Composer;
        let recipient = app.selected_recipient().unwrap();
        assert!(app.composer.insert("abc", Some(recipient)));

        let flow = handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut app,
            &UiRegions::default(),
            true,
        );

        assert!(matches!(flow, Flow::Continue));
        assert_eq!(app.composer.cursor(), 2);
        handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut app,
            &UiRegions::default(),
            true,
        );
        assert_eq!(app.composer.text(), "abqc");
    }

    #[test]
    fn send_queue_serializes_rapid_messages_without_retargeting_them() {
        let first = Recipient::from_device(&device("laptop", "100.64.0.2")).unwrap();
        let second = Recipient::from_device(&device("tablet", "100.64.0.3")).unwrap();
        let mut queue = SendQueue::default();
        let first_id =
            queue.enqueue(first.clone(), SendPayload::Text(Zeroizing::new("one".into())));
        let second_id =
            queue.enqueue(second.clone(), SendPayload::Text(Zeroizing::new("two".into())));

        let work = queue.start_next().unwrap();
        assert_eq!(work.id, first_id);
        assert_eq!(work.recipient, first);
        assert_eq!(queue.queued_count(), 1);
        queue.finish(first_id, Ok(()));

        let work = queue.start_next().unwrap();
        assert_eq!(work.id, second_id);
        assert_eq!(work.recipient, second);
    }

    #[test]
    fn retry_moves_failed_sends_behind_items_already_queued() {
        let recipient = Recipient::from_device(&device("laptop", "100.64.0.2")).unwrap();
        let mut queue = SendQueue::default();
        let failed_id =
            queue.enqueue(recipient.clone(), SendPayload::Text(Zeroizing::new("first".into())));
        queue.start_next().unwrap();
        let waiting_id =
            queue.enqueue(recipient, SendPayload::Text(Zeroizing::new("already waiting".into())));
        queue.finish(failed_id, Err("offline".into()));

        assert_eq!(queue.retry_failed(), 1);
        assert_eq!(queue.start_next().unwrap().id, waiting_id);
        queue.finish(waiting_id, Ok(()));
        assert_eq!(queue.start_next().unwrap().id, failed_id);
    }

    #[test]
    fn cancelling_an_active_send_does_not_cancel_the_queue() {
        let recipient = Recipient::from_device(&device("laptop", "100.64.0.2")).unwrap();
        let mut queue = SendQueue::default();
        let active_id =
            queue.enqueue(recipient.clone(), SendPayload::Text(Zeroizing::new("cancel me".into())));
        queue.start_next().unwrap();
        let queued_id =
            queue.enqueue(recipient, SendPayload::Text(Zeroizing::new("keep me".into())));

        assert!(queue.mark_cancelling());
        queue.finish(active_id, Err("operation cancelled".into()));
        assert_eq!(queue.queued_count(), 1);
        assert_eq!(queue.start_next().unwrap().id, queued_id);
    }

    #[test]
    fn quitting_with_retryable_or_pending_sends_requires_confirmation() {
        let mut app = ready_app();
        let recipient = app.selected_recipient().unwrap();
        app.sends.enqueue(recipient, SendPayload::Text(Zeroizing::new("do not lose me".into())));

        assert!(matches!(request_quit(&mut app), Flow::Continue));
        assert!(matches!(app.mode, Mode::ConfirmQuit(_)));
        assert!(app.notice.as_deref().is_some_and(|notice| notice.contains("abandoned")));
    }

    #[test]
    fn quitting_with_an_unsent_draft_requires_confirmation() {
        let mut app = ready_app();
        let recipient = app.selected_recipient().unwrap();
        assert!(app.composer.insert("do not lose this draft", Some(recipient)));

        assert!(matches!(request_quit(&mut app), Flow::Continue));
        assert!(matches!(app.mode, Mode::ConfirmQuit(_)));
        assert!(app.notice.as_deref().is_some_and(|notice| notice.contains("abandoned")));
    }

    #[tokio::test]
    async fn receiver_event_send_yields_to_cancellation_when_the_channel_is_full() {
        let (sender, _receiver) = mpsc::channel(1);
        sender
            .send(ReceiverEvent {
                generation: 1,
                kind: ReceiverEventKind::State(ReceiverState::Waiting),
            })
            .await
            .unwrap();
        let (cancel, mut cancel_receiver) = watch::channel(false);
        cancel.send(true).unwrap();

        assert!(
            !send_receiver_event(
                &sender,
                &mut cancel_receiver,
                1,
                ReceiverEventKind::State(ReceiverState::Standby),
            )
            .await
        );
    }

    #[test]
    fn pasted_existing_path_requires_a_file_or_text_decision() {
        let directory =
            std::env::temp_dir().join(format!("kit-tail-paste-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("literal path.txt");
        fs::write(&path, "payload").unwrap();
        let mut app = ready_app();

        handle_paste(&mut app, format!("'{}'", path.display()));

        let Mode::ReviewFiles(review) = &app.mode else { panic!("expected file review") };
        assert_eq!(review.paths, vec![path]);
        assert!(review.pasted_text.is_some());
        assert_eq!(review.recipient.name, "laptop");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn live_refresh_cannot_retarget_a_nonempty_message() {
        let mut app = App::new(
            Readiness::Ready {
                local: device("desktop", "100.64.0.1"),
                peers: vec![device("laptop", "100.64.0.2"), device("tablet", "100.64.0.3")],
            },
            Vec::new(),
        );
        let laptop = app.selected_recipient().unwrap();
        assert!(app.composer.insert("for laptop", Some(laptop.clone())));

        app.reconcile_readiness(Readiness::Ready {
            local: device("desktop", "100.64.0.1"),
            peers: vec![device("tablet", "100.64.0.3")],
        });

        assert_eq!(app.selected_peer().map(|peer| peer.name.as_str()), Some("tablet"));
        assert_eq!(app.composer.recipient(), Some(&laptop));
        assert!(app.composer.can_retarget(app.selected_recipient().as_ref()));
    }

    #[test]
    fn long_device_lists_keep_the_selected_row_visible_and_clickable() {
        let peers = (0..30)
            .map(|index| device(&format!("peer-{index:02}"), &format!("100.64.0.{index}")))
            .collect();
        let mut app = App::new(
            Readiness::Ready { local: device("desktop", "100.64.0.1"), peers },
            Vec::new(),
        );
        app.peer_index = 25;
        let mut terminal = Terminal::new(TestBackend::new(110, 20)).unwrap();
        let mut regions = UiRegions::default();

        terminal.draw(|frame| regions = render(frame, &app)).unwrap();

        assert!(screen(&terminal).contains("peer-25"));
        assert!(regions.rows.iter().any(|(_, target)| *target == RowTarget::Peer(25)));
    }

    #[test]
    fn clicking_the_composer_focuses_it_and_places_the_cursor() {
        let mut app = ready_app();
        let recipient = app.selected_recipient().unwrap();
        assert!(app.composer.insert("abc", Some(recipient)));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut regions = UiRegions::default();
        terminal.draw(|frame| regions = render(frame, &app)).unwrap();
        let input = composer_input_area(regions.composer_panel.unwrap());

        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: input.x + 1,
                row: input.y,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &regions,
        );

        assert_eq!(app.active_region, ActiveRegion::Composer);
        assert_eq!(app.composer.cursor(), 1);
    }

    #[test]
    fn quit_is_always_a_mouse_action_in_the_header() {
        let mut app = ready_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut regions = UiRegions::default();
        terminal.draw(|frame| regions = render(frame, &app)).unwrap();
        let quit = regions
            .inline_actions
            .iter()
            .find(|(_, action)| *action == contributions::QUIT)
            .map(|(area, _)| *area)
            .unwrap();

        let flow = handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: quit.x,
                row: quit.y,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &regions,
        );

        assert!(matches!(
            flow,
            Flow::Invoke(ActionInvocation { action, .. }) if action == contributions::QUIT
        ));
    }

    fn ready_app() -> App {
        App::new(
            Readiness::Ready {
                local: device("desktop", "100.64.0.1"),
                peers: vec![device("laptop", "100.64.0.2")],
            },
            Vec::new(),
        )
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    fn device(name: &str, address: &str) -> Device {
        Device {
            id: name.into(),
            name: name.into(),
            dns_name: format!("{name}.example.ts.net"),
            operating_system: crate::tailscale::OperatingSystem::Linux,
            online: true,
            addresses: vec![address.into()],
            taildrop_target: Some(address.into()),
        }
    }
}
