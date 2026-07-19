use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use directories::UserDirs;
use ratatui::{
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use zeroize::Zeroizing;

use crate::{
    framework::{open_external, ExternalTarget},
    tui::{
        render_split_divider, ActionId, ActionInvocation, ActionState, ActionUnavailable,
        ContextMenu, ContextMenuLayout, ContextMenuOutcome, ContextMenuStyle, EventReader,
        KeyChord, NavigationHistory, NavigationMap, NavigationRegion, ResolvedAction, Session,
        SessionOptions, SplitDividerStyle, SplitDrag, SplitFrame, SplitMinimums, SplitRatio,
    },
};

use super::{
    cache::{CachedItem, ItemKind, ReceiveCache, SaveConflictResolution},
    client::{LoginEvent, TailClient},
    config::Config,
    contributions::{
        self, TailActionContext, TailActionRegistry, TailActionTarget, TailCommand, TailSurface,
        AUTH_INLINE, BROWSE_INLINE, DEVICE_CONTEXT, ITEM_CONTEXT, MODAL_INLINE,
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

enum Mode {
    Browse,
    Compose,
    Review,
    Ambiguous(PastedInput),
    Search,
    Detail { preview: Option<Zeroizing<String>> },
    ConfirmDelete,
    FileBrowser(FileBrowser),
    SaveBrowser(SaveBrowser),
    SaveConflict { item: CachedItem, directory: PathBuf },
    Auth,
    Busy,
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

#[derive(Clone)]
enum Draft {
    Text(Zeroizing<String>),
    Files(Vec<PathBuf>),
}

enum BackendEvent {
    Sent(Result<(), String>),
}

enum ReceiverEvent {
    Received(Vec<CachedItem>),
    Retry { error: String, delay: Duration },
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
}

struct SaveBrowser {
    browser: FileBrowser,
    item: CachedItem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveRegion {
    Devices,
    Items,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TailLocation {
    Browse { active_region: ActiveRegion, peer_id: Option<String>, item_id: Option<String> },
    Compose { peer_id: Option<String> },
    Review { peer_id: Option<String> },
    Detail { item_id: String },
    FileBrowser { directory: PathBuf },
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
    rows: Vec<(Rect, RowTarget)>,
    inline_actions: Vec<(Rect, ActionId)>,
    split: Option<SplitFrame>,
    context_menu: Option<ContextMenuLayout>,
}

impl UiRegions {
    fn navigation(&self) -> NavigationMap<ActiveRegion> {
        NavigationMap::new([
            NavigationRegion::new(ActiveRegion::Devices, self.device_panel.unwrap_or_default()),
            NavigationRegion::new(ActiveRegion::Items, self.item_panel.unwrap_or_default()),
        ])
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
    draft: Option<Draft>,
    composer: Zeroizing<String>,
    search: String,
    notice: Option<String>,
    login_url: Option<String>,
    auth_can_login: bool,
    watch: bool,
    pointer_enabled: bool,
    split_ratio: SplitRatio,
    split_drag: Option<SplitDrag<()>>,
    history: NavigationHistory<TailLocation>,
    spinner: usize,
}

struct ActionDispatch<'a> {
    session: &'a mut Session,
    client: &'a TailClient,
    cache: &'a ReceiveCache,
    backend_tx: &'a mpsc::Sender<BackendEvent>,
    receiver_tx: &'a mpsc::Sender<ReceiverEvent>,
    operation: &'a mut Option<Operation>,
    receiver: &'a mut Option<Operation>,
    login: &'a mut Option<LoginOperation>,
}

pub async fn run(
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
        receiver = Some(start_receiver(&client, &cache, &receiver_tx));
    }
    let mut regions = UiRegions::default();

    loop {
        session.draw(|frame| regions = render(frame, &app))?;
        tokio::select! {
            _ = tick.tick(), if matches!(app.mode, Mode::Busy) || (matches!(app.mode, Mode::Auth) && app.auth_can_login) => {
                app.spinner = app.spinner.wrapping_add(1);
            }
            _ = refresh_tick.tick(), if app.can_refresh_devices() && operation.is_none() && login.is_none() && refresh_task.is_none() => {
                refresh_task = Some(start_device_refresh(&client));
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                let mut actions = ActionDispatch {
                    session: &mut session,
                    client: &client,
                    cache: &cache,
                    backend_tx: &backend_tx,
                    receiver_tx: &receiver_tx,
                    operation: &mut operation,
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
                    Flow::Invoke(invocation) => actions.invoke(&mut app, invocation).await?,
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
                operation = None;
                app.mode = Mode::Browse;
                match event {
                    Some(BackendEvent::Sent(Ok(()))) => {
                        app.draft = None;
                        app.composer.clear();
                        app.notice = Some("Sent with Taildrop".into());
                    }
                    Some(BackendEvent::Sent(Err(error))) => app.notice = Some(error),
                    None => {}
                }
                app.visit_current_location();
            }
            event = receiver_rx.recv() => {
                match event {
                    Some(ReceiverEvent::Received(items)) => {
                        let count = items.len();
                        app.items = cache.list()?;
                        app.item_index = 0;
                        app.notice = Some(format!("Received {count} item(s)"));
                    }
                    Some(ReceiverEvent::Retry { error, delay }) => {
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
                if operation.is_none() && login.is_none() {
                    match refreshed {
                    Ok(readiness) => {
                        let begin_auth = matches!(readiness, Readiness::NeedsLogin)
                            && !app.auth_can_login;
                        app.reconcile_readiness(readiness);
                        if begin_auth {
                            login = Some(begin_login(&client));
                        }
                        sync_receiver(
                            &app,
                            &client,
                            &cache,
                            &receiver_tx,
                            &mut receiver,
                        ).await;
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
                            &app,
                            &client,
                            &cache,
                            &receiver_tx,
                            &mut receiver,
                        ).await;
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
    stop_operation(&mut receiver).await;
    cancel_login(&mut login).await;
    if let Some(task) = refresh_task {
        task.abort();
        let _ = task.await;
    }
    Ok(())
}

fn start_device_refresh(client: &TailClient) -> JoinHandle<Result<Readiness, String>> {
    let client = client.clone();
    tokio::spawn(async move { client.readiness().await.map_err(|error| format!("{error:#}")) })
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
            Readiness::Ready { local, peers } => (Some(local), peers, Mode::Browse, None, false),
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
            draft: None,
            composer: Zeroizing::new(String::new()),
            search: String::new(),
            notice,
            login_url: None,
            auth_can_login,
            watch,
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

    fn selected_target(&self) -> Option<&str> {
        self.selected_peer().and_then(Device::send_target)
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
        let target = match self.active_region {
            ActiveRegion::Devices => self
                .selected_peer()
                .filter(|peer| peer.send_target().is_some())
                .map(|peer| TailActionTarget::Device(peer.id.clone()))
                .unwrap_or(TailActionTarget::None),
            ActiveRegion::Items => self
                .selected_item()
                .map(|item| TailActionTarget::Item {
                    id: item.id.to_string(),
                    text: item.kind == ItemKind::Text,
                })
                .unwrap_or(TailActionTarget::None),
        };
        TailActionContext {
            surface: match &self.mode {
                Mode::Browse => TailSurface::Browse,
                Mode::Compose => TailSurface::Compose,
                Mode::Review => TailSurface::Review,
                Mode::Ambiguous(_) => TailSurface::Ambiguous,
                Mode::Search => TailSurface::Search,
                Mode::Detail { .. } => TailSurface::Detail,
                Mode::ConfirmDelete => TailSurface::ConfirmDelete,
                Mode::FileBrowser(browser) => {
                    TailSurface::FileBrowser { selected_files: !browser.selected.is_empty() }
                }
                Mode::SaveBrowser(_) => TailSurface::SaveBrowser,
                Mode::SaveConflict { .. } => TailSurface::SaveConflict,
                Mode::Auth => TailSurface::Auth,
                Mode::Busy => TailSurface::Busy,
            },
            target: if matches!(self.mode, Mode::Auth) { TailActionTarget::Auth } else { target },
            receiving: self.watch,
            has_draft: self.draft.is_some(),
            login_url: self.login_url.is_some(),
            can_retry_login: matches!(self.mode, Mode::Auth),
        }
    }

    fn location(&self) -> Option<TailLocation> {
        let peer_id = self.selected_peer().map(|peer| peer.id.clone());
        match &self.mode {
            Mode::Browse => Some(TailLocation::Browse {
                active_region: self.active_region,
                peer_id,
                item_id: self.selected_item().map(|item| item.id.to_string()),
            }),
            Mode::Compose => Some(TailLocation::Compose { peer_id }),
            Mode::Review => Some(TailLocation::Review { peer_id }),
            Mode::Detail { .. } => self
                .selected_item()
                .map(|item| TailLocation::Detail { item_id: item.id.to_string() }),
            Mode::FileBrowser(browser) => {
                Some(TailLocation::FileBrowser { directory: browser.directory.clone() })
            }
            Mode::SaveBrowser(save) => Some(TailLocation::SaveBrowser {
                item_id: save.item.id.to_string(),
                directory: save.browser.directory.clone(),
            }),
            Mode::Ambiguous(_)
            | Mode::Search
            | Mode::ConfirmDelete
            | Mode::SaveConflict { .. }
            | Mode::Auth
            | Mode::Busy => None,
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
                let Some(index) = self.filtered_peers().iter().position(|peer| peer.id == *id)
                else {
                    return false;
                };
                self.peer_index = index;
                self.active_region = ActiveRegion::Devices;
                true
            }
            TailActionTarget::Item { id, .. } => {
                let Some(index) =
                    self.filtered_items().iter().position(|item| item.id.to_string() == *id)
                else {
                    return false;
                };
                self.item_index = index;
                self.active_region = ActiveRegion::Items;
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
            self.mode = Mode::Browse;
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
        Ok(Self { directory, entries, index: 0, selected: BTreeSet::new() })
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
                        app.active_region = ActiveRegion::Devices;
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
        Mode::Browse => {
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
            TailLocation::Browse { active_region, peer_id, item_id } => {
                app.mode = Mode::Browse;
                app.active_region = *active_region;
                app.select_peer_id(peer_id.as_deref());
                if let Some(item_id) = item_id {
                    app.select_item_id(item_id);
                }
                Ok(true)
            }
            TailLocation::Compose { peer_id } => {
                app.select_peer_id(peer_id.as_deref());
                app.mode = Mode::Compose;
                Ok(true)
            }
            TailLocation::Review { peer_id } => {
                if app.draft.is_none() {
                    return Ok(false);
                }
                app.select_peer_id(peer_id.as_deref());
                app.mode = Mode::Review;
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
            TailLocation::FileBrowser { directory } => {
                app.mode = Mode::FileBrowser(FileBrowser::open(directory.clone())?);
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
    ) -> Result<()> {
        if !app.restore_action_target(&invocation.context.target) {
            app.notice = Some("That selection is no longer available".into());
            return Ok(());
        }
        let command = match app.registry.command_for(&invocation) {
            Ok(command) => command,
            Err(ActionUnavailable::Disabled { reason, .. }) => {
                app.notice = Some(reason.into_owned());
                return Ok(());
            }
            Err(ActionUnavailable::Unknown { action }) => {
                app.notice = Some(format!("Unknown action {action}"));
                return Ok(());
            }
        };
        app.replace_current_location();
        match command {
            TailCommand::Compose => {
                if !matches!(app.draft, Some(Draft::Text(_))) {
                    app.composer.clear();
                    app.draft = None;
                }
                app.mode = Mode::Compose;
            }
            TailCommand::ChooseFiles => {
                let mut browser = FileBrowser::open(std::env::current_dir()?)?;
                if let Some(Draft::Files(paths)) = &app.draft {
                    browser.selected.extend(paths.iter().cloned());
                }
                app.mode = Mode::FileBrowser(browser);
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
                    app.notice = Some(match open_external(ExternalTarget::Path(&item.payload())) {
                        Ok(()) => format!("Opening {}", item.name),
                        Err(error) => format!("Could not open {}: {error:#}", item.name),
                    });
                }
            }
            TailCommand::Delete => app.mode = Mode::ConfirmDelete,
            TailCommand::Search => {
                app.mode = Mode::Search;
                app.search.clear();
            }
            TailCommand::ToggleReceiving => {
                app.watch = !app.watch;
                sync_receiver(app, self.client, self.cache, self.receiver_tx, self.receiver).await;
                app.notice = Some(if app.watch {
                    "Automatic receiving resumed".into()
                } else {
                    "Automatic receiving paused".into()
                });
            }
            TailCommand::OpenLogin => {
                if let Some(url) = &app.login_url {
                    if let Err(error) = open_external(ExternalTarget::Url(url)) {
                        app.notice = Some(format!("Could not open login link: {error:#}"));
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
                sync_receiver(app, self.client, self.cache, self.receiver_tx, self.receiver).await;
                if app.auth_can_login {
                    *self.login = Some(begin_login(self.client));
                    app.notice = Some("Starting Tailscale authentication…".into());
                }
            }
            TailCommand::NextDevice => cycle_peer(app, 1),
            TailCommand::ReviewDraft => {
                if matches!(app.mode, Mode::Compose) && !app.composer.is_empty() {
                    app.draft = Some(Draft::Text(app.composer.clone()));
                }
                if app.draft.is_some() {
                    app.mode = Mode::Review;
                }
            }
            TailCommand::Send => start_send(app, self.client, self.backend_tx, self.operation)?,
            TailCommand::Back => match &app.mode {
                Mode::Review => {
                    self.navigate_history(app, -1)?;
                }
                Mode::SaveConflict { item, directory } => {
                    let item = item.clone();
                    let directory = directory.clone();
                    match FileBrowser::open(directory) {
                        Ok(browser) => app.mode = Mode::SaveBrowser(SaveBrowser { browser, item }),
                        Err(error) => {
                            app.mode = Mode::Browse;
                            app.notice = Some(format!("Could not browse: {error:#}"));
                        }
                    }
                }
                Mode::Ambiguous(_) => app.mode = Mode::Compose,
                Mode::Search | Mode::ConfirmDelete => app.mode = Mode::Browse,
                _ => self.navigate_history(app, -1)?,
            },
            TailCommand::UseFiles => {
                if let Mode::Ambiguous(PastedInput::Ambiguous { existing, .. }) = &app.mode {
                    app.draft = Some(Draft::Files(existing.clone()));
                    app.mode = Mode::Review;
                }
            }
            TailCommand::UseText => {
                if let Mode::Ambiguous(PastedInput::Ambiguous { raw, .. }) = &app.mode {
                    app.composer = Zeroizing::new(raw.clone());
                    app.draft = Some(Draft::Text(Zeroizing::new(raw.clone())));
                    app.mode = Mode::Review;
                }
            }
            TailCommand::ConfirmDelete => {
                if let Some(item) = app.selected_item().cloned() {
                    self.cache.delete(&item)?;
                    app.items = self.cache.list()?;
                    app.item_index = 0;
                }
                app.mode = Mode::Browse;
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
                            app.draft = Some(Draft::Files(vec![path]));
                            app.mode = Mode::Review;
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
            TailCommand::SendSelected => {
                if let Mode::FileBrowser(browser) = &app.mode {
                    app.draft = Some(Draft::Files(browser.selected.iter().cloned().collect()));
                    app.mode = Mode::Review;
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
                if let Some(operation) = self.operation.as_ref() {
                    let _ = operation.cancel.send(true);
                }
                app.notice = Some("Cancelling and reaping Tailscale process…".into());
            }
            TailCommand::ResumeReceiving => {
                app.watch = true;
                sync_receiver(app, self.client, self.cache, self.receiver_tx, self.receiver).await;
                app.notice = Some("Automatic receiving is active".into());
            }
            TailCommand::CancelLogin => {
                cancel_login(self.login).await;
                app.notice = Some("Authentication cancelled; Enter retries".into());
            }
        }
        app.visit_current_location();
        Ok(())
    }
}

fn handle_key(key: KeyEvent, app: &mut App, regions: &UiRegions, login_idle: bool) -> Flow {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return if matches!(app.mode, Mode::Busy) {
            Flow::Invoke(ActionInvocation::new(
                contributions::CANCEL_OPERATION,
                app.action_context(),
            ))
        } else {
            Flow::Quit
        };
    }
    if matches!(key.code, KeyCode::Char('q')) && matches!(app.mode, Mode::Browse | Mode::Auth) {
        return Flow::Quit;
    }
    if key.modifiers.is_empty() && app.location().is_some() {
        match key.code {
            KeyCode::Left => return Flow::NavigateHistory(-1),
            KeyCode::Right => return Flow::NavigateHistory(1),
            _ => {}
        }
    }
    let contributed = matches!(app.mode, Mode::Browse)
        .then(|| KeyChord::from_event(key))
        .flatten()
        .and_then(|chord| app.registry.resolve_keybinding(chord, app.action_context()));
    let action = match (&app.mode, key.code) {
        (Mode::Auth, KeyCode::Char('o')) => Some(contributions::OPEN_LOGIN),
        (Mode::Auth, KeyCode::Char('c')) => Some(contributions::COPY_LOGIN),
        (Mode::Auth, KeyCode::Enter) if login_idle => Some(contributions::RETRY_LOGIN),
        (Mode::Auth, KeyCode::Esc) => Some(contributions::CANCEL_LOGIN),
        (Mode::Compose | Mode::Review, KeyCode::Tab) => Some(contributions::NEXT_DEVICE),
        (Mode::Compose, KeyCode::Enter) if !app.composer.is_empty() => {
            Some(contributions::REVIEW_DRAFT)
        }
        (Mode::Browse, KeyCode::Enter)
            if app.active_region == ActiveRegion::Devices && app.draft.is_some() =>
        {
            Some(contributions::REVIEW_DRAFT)
        }
        (Mode::Review, KeyCode::Enter) => Some(contributions::SEND),
        (Mode::Ambiguous(_), KeyCode::Char('a')) => Some(contributions::USE_FILES),
        (Mode::Ambiguous(_), KeyCode::Char('t')) => Some(contributions::USE_TEXT),
        (Mode::Detail { .. }, KeyCode::Char('c')) => Some(contributions::COPY),
        (Mode::ConfirmDelete, KeyCode::Char('y')) => Some(contributions::CONFIRM_DELETE),
        (Mode::FileBrowser(_), KeyCode::Enter) => Some(contributions::OPEN_ENTRY),
        (Mode::FileBrowser(_), KeyCode::Char(' ')) => Some(contributions::TOGGLE_FILE),
        (Mode::FileBrowser(_), KeyCode::Char('s')) => Some(contributions::SEND_SELECTED),
        (Mode::FileBrowser(_) | Mode::SaveBrowser(_), KeyCode::Backspace) => {
            Some(contributions::PARENT_DIRECTORY)
        }
        (Mode::SaveBrowser(_), KeyCode::Enter) => Some(contributions::OPEN_ENTRY),
        (Mode::SaveBrowser(_), KeyCode::Char('s')) => Some(contributions::SAVE_HERE),
        (Mode::SaveConflict { .. }, KeyCode::Char('r')) => Some(contributions::KEEP_BOTH),
        (Mode::SaveConflict { .. }, KeyCode::Char('x')) => Some(contributions::REPLACE),
        (Mode::Busy, KeyCode::Esc) => Some(contributions::CANCEL_OPERATION),
        (
            Mode::Compose
            | Mode::Review
            | Mode::Ambiguous(_)
            | Mode::Search
            | Mode::Detail { .. }
            | Mode::ConfirmDelete
            | Mode::FileBrowser(_)
            | Mode::SaveBrowser(_)
            | Mode::SaveConflict { .. },
            KeyCode::Esc,
        )
        | (Mode::Search | Mode::Detail { .. }, KeyCode::Enter)
        | (Mode::ConfirmDelete, KeyCode::Char('n')) => Some(contributions::BACK),
        _ => None,
    };
    let invocation = contributed
        .or_else(|| action.map(|action| ActionInvocation::new(action, app.action_context())));
    if let Some(invocation) = invocation {
        return Flow::Invoke(invocation);
    }
    match &mut app.mode {
        Mode::Auth => {}
        Mode::Browse => match key.code {
            KeyCode::Tab => {
                app.active_region =
                    regions.navigation().next(app.active_region).unwrap_or(app.active_region);
            }
            KeyCode::BackTab => {
                app.active_region =
                    regions.navigation().previous(app.active_region).unwrap_or(app.active_region);
            }
            KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
            KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
            _ => {}
        },
        Mode::Compose => match key.code {
            KeyCode::Backspace => {
                app.composer.pop();
            }
            KeyCode::Char(character) => app.composer.push(character),
            _ => {}
        },
        Mode::Review | Mode::Ambiguous(_) | Mode::Detail { .. } | Mode::ConfirmDelete => {}
        Mode::Search => match key.code {
            KeyCode::Backspace => {
                app.search.pop();
                app.peer_index = 0;
                app.item_index = 0;
            }
            KeyCode::Char(character) => {
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
        Mode::SaveConflict { .. } | Mode::Busy => {}
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
    if !matches!(app.mode, Mode::Compose | Mode::Browse | Mode::FileBrowser(_))
        || app.selected_target().is_none()
    {
        return;
    }
    app.replace_current_location();
    match classify(raw) {
        PastedInput::Text(text) => {
            app.composer = Zeroizing::new(text.clone());
            app.draft = Some(Draft::Text(Zeroizing::new(text)));
            app.mode = Mode::Review;
        }
        PastedInput::Files(paths) => {
            app.draft = Some(Draft::Files(paths));
            app.mode = Mode::Review;
        }
        ambiguous @ PastedInput::Ambiguous { .. } => app.mode = Mode::Ambiguous(ambiguous),
    }
    app.visit_current_location();
}

fn move_selection(app: &mut App, delta: isize) {
    let len = match app.active_region {
        ActiveRegion::Devices => app.filtered_peers().len(),
        ActiveRegion::Items => app.filtered_items().len(),
    };
    let index = match app.active_region {
        ActiveRegion::Devices => &mut app.peer_index,
        ActiveRegion::Items => &mut app.item_index,
    };
    if len > 0 {
        *index = (*index as isize + delta).clamp(0, len as isize - 1) as usize;
    }
}

fn cycle_peer(app: &mut App, delta: usize) {
    let eligible = app
        .filtered_peers()
        .into_iter()
        .filter(|peer| peer.send_target().is_some())
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return;
    }
    let current_id = app.selected_peer().map(|peer| peer.id.as_str());
    let current =
        eligible.iter().position(|peer| Some(peer.id.as_str()) == current_id).unwrap_or(0);
    let next_id = &eligible[(current + delta) % eligible.len()].id;
    if let Some(index) = app.filtered_peers().iter().position(|peer| &peer.id == next_id) {
        app.peer_index = index;
    }
}

fn start_send(
    app: &mut App,
    client: &TailClient,
    sender: &mpsc::Sender<BackendEvent>,
    operation: &mut Option<Operation>,
) -> Result<()> {
    let target =
        app.selected_target().context("selected device cannot receive Taildrop")?.to_owned();
    let draft = app.draft.clone().context("nothing to send")?;
    let client = client.clone();
    let sender = sender.clone();
    let (cancel, cancel_receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let result = match draft {
            Draft::Text(text) => {
                client.send_text(&text_name(), text, &target, cancel_receiver).await
            }
            Draft::Files(paths) => client.send_files(&paths, &target, cancel_receiver).await,
        }
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::Sent(result)).await;
    });
    *operation = Some(Operation { cancel, task });
    app.mode = Mode::Busy;
    app.notice = Some("Sending…  Esc cancels".into());
    Ok(())
}

fn start_receiver(
    client: &TailClient,
    cache: &ReceiveCache,
    sender: &mpsc::Sender<ReceiverEvent>,
) -> Operation {
    let client = client.clone();
    let cache = cache.clone();
    let sender = sender.clone();
    let (cancel, cancel_receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut failures = 0_u32;
        loop {
            if *cancel_receiver.borrow() {
                break;
            }
            let result = async {
                let staging = cache.staging_directory()?;
                client.receive_into(staging.path(), true, cancel_receiver.clone()).await?;
                cache.import_staging(staging)
            }
            .await;
            match result {
                Ok(items) => {
                    failures = 0;
                    if !items.is_empty()
                        && sender.send(ReceiverEvent::Received(items)).await.is_err()
                    {
                        break;
                    }
                }
                Err(_) if *cancel_receiver.borrow() => break,
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let delay = receive_retry_delay(failures);
                    if sender
                        .send(ReceiverEvent::Retry { error: format!("{error:#}"), delay })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let mut retry_cancel = cancel_receiver.clone();
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        changed = retry_cancel.changed() => {
                            let _ = changed;
                            break;
                        }
                    }
                }
            }
        }
    });
    Operation { cancel, task }
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
    app: &App,
    client: &TailClient,
    cache: &ReceiveCache,
    sender: &mpsc::Sender<ReceiverEvent>,
    receiver: &mut Option<Operation>,
) {
    let should_run = app.watch && app.local.is_some();
    match (should_run, receiver.is_some()) {
        (true, false) => *receiver = Some(start_receiver(client, cache, sender)),
        (false, true) => stop_operation(receiver).await,
        _ => {}
    }
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
            app.mode = Mode::Browse;
            app.notice = Some(format!("Saved {}", path.display()));
        }
        Err(error) => app.notice = Some(format!("Could not save item: {error:#}")),
    }
    Ok(())
}

fn text_name() -> String {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs());
    format!("clipboard-{timestamp}.txt")
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
    let rows =
        Layout::vertical([Constraint::Length(3), Constraint::Min(10), Constraint::Length(3)])
            .split(frame.area());
    render_header(frame, rows[0], app);
    let mut regions = UiRegions::default();
    match &app.mode {
        Mode::Compose => render_compose(frame, rows[1], app),
        Mode::Review => render_review(frame, rows[1], app),
        Mode::Ambiguous(input) => render_ambiguous(frame, rows[1], input),
        Mode::Detail { preview } => {
            render_detail(frame, rows[1], app, preview.as_ref().map(|text| text.as_str()))
        }
        Mode::ConfirmDelete => render_confirm_delete(frame, rows[1], app),
        Mode::FileBrowser(browser) => render_file_browser(frame, rows[1], browser),
        Mode::SaveBrowser(save) => render_save_browser(frame, rows[1], save),
        Mode::SaveConflict { item, directory } => {
            render_save_conflict(frame, rows[1], item, directory)
        }
        Mode::Auth => render_auth(frame, rows[1], app),
        _ => render_browser(frame, rows[1], app, &mut regions),
    }
    match &app.mode {
        Mode::FileBrowser(browser) => {
            regions.rows.extend(
                row_regions(rows[1], browser.entries.len())
                    .map(|(area, index)| (area, RowTarget::File(index))),
            );
        }
        Mode::SaveBrowser(save) => {
            regions.rows.extend(
                row_regions(rows[1], save.browser.entries.len())
                    .map(|(area, index)| (area, RowTarget::SaveDirectory(index))),
            );
        }
        _ => {}
    }
    render_footer(frame, rows[2], app, &mut regions);
    if let Some(Overlay::ContextMenu(menu)) = &app.overlay {
        let layout = menu.layout(frame.area());
        menu.render(frame, &layout, ContextMenuStyle::default());
        regions.context_menu = Some(layout);
    }
    regions
}

fn row_regions(area: Rect, count: usize) -> impl Iterator<Item = (Rect, usize)> {
    let visible = count.min(usize::from(area.height.saturating_sub(2)));
    let width = area.width.saturating_sub(2);
    (0..visible).map(move |index| {
        (
            Rect::new(area.x.saturating_add(1), area.y.saturating_add(1 + index as u16), width, 1),
            index,
        )
    })
}

fn panel(title: &'static str, focused: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let local = app.local.as_ref().map_or("not connected", |device| device.name.as_str());
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
            Span::styled(if app.watch { "  ● watch" } else { "" }, Style::default().fg(GOOD)),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
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
    regions.rows.extend(
        row_regions(devices, app.filtered_peers().len())
            .map(|(area, index)| (area, RowTarget::Peer(index))),
    );
    regions.rows.extend(
        row_regions(inbox, app.filtered_items().len())
            .map(|(area, index)| (area, RowTarget::Item(index))),
    );
    let peers = app
        .filtered_peers()
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
            let style = if index == app.peer_index && app.active_region == ActiveRegion::Devices {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else {
                Style::default().fg(if peer.taildrop_target.is_some() && peer.online {
                    TEXT
                } else {
                    MUTED
                })
            };
            ListItem::new(format!(" {status} {:<24} {}", peer.name, peer.os)).style(style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(peers)
            .block(panel(" Devices · send to ", app.active_region == ActiveRegion::Devices)),
        devices,
    );
    let items = app
        .filtered_items()
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
    frame.render_widget(
        List::new(items)
            .block(panel(" Received · 30-day cache ", app.active_region == ActiveRegion::Items)),
        inbox,
    );
}

fn render_compose(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let recipient = app.selected_peer().map_or("none", |peer| peer.name.as_str());
    frame.render_widget(
        Paragraph::new(app.composer.as_str()).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(format!(" Share with {recipient} · paste text or drag files here "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
    );
}

fn render_review(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let recipient = app.selected_peer().map_or("none", |peer| peer.name.as_str());
    let content = match &app.draft {
        Some(Draft::Text(text)) => format!("Text · {} bytes\n\n{}", text.len(), text.as_str()),
        Some(Draft::Files(paths)) => format!(
            "{} file(s)\n\n{}",
            paths.len(),
            paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("\n")
        ),
        None => "Nothing selected".into(),
    };
    frame.render_widget(
        Paragraph::new(content).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(format!(" Review → {recipient} "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(WARN)),
        ),
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

fn render_file_browser(frame: &mut Frame<'_>, area: Rect, browser: &FileBrowser) {
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
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(
                    " Choose files · {} · {} selected ",
                    browser.directory.display(),
                    browser.selected.len()
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
    );
}

fn render_save_browser(frame: &mut Frame<'_>, area: Rect, save: &SaveBrowser) {
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
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(" Save {} in {} ", save.item.name, save.browser.directory.display()))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(GOOD)),
        ),
        area,
    );
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
    let controls = match app.mode {
        Mode::Browse => "drag files · ↑↓ select · Tab panels · ←/→ history · right-click actions",
        Mode::Compose => "paste or drag files · Tab recipient · Enter review · ←/→ history",
        Mode::Review => "Tab recipient · Enter send · ←/→ history",
        Mode::Search => "type to filter   Enter/Esc done",
        Mode::Detail { .. } => "c copy text · Enter/Esc back · ←/→ history",
        Mode::FileBrowser(_) => {
            "Enter open/send file   Space select   s send selected   Backspace parent   Esc cancel"
        }
        Mode::SaveBrowser(_) => "Enter open folder   s save here   Backspace parent   Esc cancel",
        Mode::SaveConflict { .. } => "r keep both   x replace   Esc choose another folder",
        Mode::Busy => "Esc cancel and reap",
        _ => "Esc back",
    };
    let notice = app.notice.as_deref().unwrap_or(controls);
    let notice = if matches!(app.mode, Mode::Busy)
        || (matches!(app.mode, Mode::Auth) && app.auth_can_login)
    {
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
        Mode::Browse => app.registry.resolve_menu(BROWSE_INLINE, &app.action_context()),
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
        assert!(screen.contains("drag file"));
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
        let items = cache.import_staging(staging).unwrap();
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
        app.mode = Mode::Review;
        app.draft = Some(Draft::Text(Zeroizing::new("keep this draft".into())));

        app.reconcile_readiness(Readiness::Ready {
            local: device("desktop", "100.64.0.1"),
            peers: vec![device("laptop", "100.64.0.2"), device("new-machine", "100.64.0.3")],
        });

        assert_eq!(app.peers.len(), 2);
        assert_eq!(app.selected_peer().map(|peer| peer.name.as_str()), Some("laptop"));
        assert!(matches!(app.mode, Mode::Review));
        assert!(
            matches!(app.draft, Some(Draft::Text(ref text)) if text.as_str() == "keep this draft")
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
        let items = cache.import_staging(staging).unwrap();
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
    fn paste_records_review_as_a_history_visit() {
        let mut app = ready_app();

        handle_paste(&mut app, "share this".into());

        assert!(matches!(app.mode, Mode::Review));
        assert!(matches!(app.history.current(), Some(TailLocation::Review { .. })));
        assert!(matches!(
            app.history.target(-1).map(|(_, location)| location),
            Some(TailLocation::Browse { .. })
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
            os: "linux".into(),
            online: true,
            addresses: vec![address.into()],
            taildrop_target: Some(address.into()),
        }
    }
}
