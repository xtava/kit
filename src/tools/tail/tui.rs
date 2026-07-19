use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use directories::UserDirs;
use ratatui::{
    layout::{Constraint, Layout, Rect},
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
    tui::{EventReader, Session, SessionOptions},
};

use super::{
    cache::{CachedItem, ItemKind, ReceiveCache, SaveConflictResolution},
    client::{LoginEvent, TailClient},
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
    Actions(usize),
    FileBrowser(FileBrowser),
    SaveBrowser(SaveBrowser),
    SaveConflict { item: CachedItem, directory: PathBuf },
    Auth,
    Busy,
}

#[derive(Clone)]
enum Draft {
    Text(Zeroizing<String>),
    Files(Vec<PathBuf>),
}

enum BackendEvent {
    Sent(Result<(), String>),
    Received(Result<Vec<CachedItem>, String>),
}

struct Operation {
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

struct App {
    local: Option<Device>,
    peers: Vec<Device>,
    items: Vec<CachedItem>,
    peer_index: usize,
    item_index: usize,
    focus_devices: bool,
    mode: Mode,
    draft: Option<Draft>,
    composer: Zeroizing<String>,
    search: String,
    notice: Option<String>,
    login_url: Option<String>,
    auth_can_login: bool,
    watch: bool,
    spinner: usize,
}

pub async fn run(client: TailClient, cache: ReceiveCache, readiness: Readiness) -> Result<()> {
    let pruned = cache.prune()?;
    let items = cache.list()?;
    let mut app = App::new(readiness, items);
    if pruned > 0 {
        app.notice = Some(format!("Evicted {pruned} expired item(s)"));
    }
    let mut session =
        Session::open(SessionOptions { mouse_capture: false, bracketed_paste: true })?;
    let mut events = EventReader::start();
    let (backend_tx, mut backend_rx) = mpsc::channel(8);
    let mut operation: Option<Operation> = None;
    let mut refresh_task: Option<JoinHandle<Result<Readiness, String>>> = None;
    let mut login = None;
    let mut login_cancel = None;
    let mut login_task: Option<JoinHandle<()>> = None;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut refresh_tick = tokio::time::interval(DEVICE_REFRESH_INTERVAL);
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    refresh_tick.tick().await;
    if matches!(app.mode, Mode::Auth) && app.auth_can_login {
        begin_login(&client, &mut login, &mut login_cancel, &mut login_task);
    }

    loop {
        session.draw(|frame| render(frame, &app))?;
        tokio::select! {
            _ = tick.tick(), if matches!(app.mode, Mode::Busy) || (matches!(app.mode, Mode::Auth) && app.auth_can_login) => {
                app.spinner = app.spinner.wrapping_add(1);
            }
            _ = refresh_tick.tick(), if app.can_refresh_devices() && operation.is_none() && login.is_none() && refresh_task.is_none() => {
                refresh_task = Some(start_device_refresh(&client));
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    Event::Paste(raw) => handle_paste(&mut app, raw),
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if handle_key(
                            key,
                            &mut app,
                            &mut session,
                            &client,
                            &cache,
                            &backend_tx,
                            &mut operation,
                            &mut login,
                            &mut login_cancel,
                            &mut login_task,
                        ).await? {
                            break;
                        }
                    }
                    _ => {}
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
                    Some(BackendEvent::Received(Ok(items))) => {
                        let count = items.len();
                        app.items = cache.list()?;
                        app.item_index = 0;
                        app.focus_devices = false;
                        app.notice = Some(if count == 0 { "No new Taildrops".into() } else { format!("Received {count} item(s)") });
                        if app.watch {
                            start_receive(&mut app, &client, &cache, &backend_tx, &mut operation)?;
                        }
                    }
                    Some(BackendEvent::Received(Err(error))) => {
                        app.watch = false;
                        app.notice = Some(error);
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
                            begin_login(&client, &mut login, &mut login_cancel, &mut login_task);
                        }
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
                        login = None;
                        login_cancel = None;
                        if let Some(task) = login_task.take() { let _ = task.await; }
                    }
                    Some(LoginEvent::Failed(error)) => {
                        app.notice = Some(error);
                        login = None;
                        login_cancel = None;
                        if let Some(task) = login_task.take() { let _ = task.await; }
                    }
                    Some(LoginEvent::Cancelled) => {
                        app.notice = Some("Authentication cancelled; Enter retries".into());
                        login = None;
                        login_cancel = None;
                        if let Some(task) = login_task.take() { let _ = task.await; }
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
    if let Some(cancel) = login_cancel {
        let _ = cancel.send(true);
    }
    if let Some(task) = login_task {
        let _ = task.await;
    }
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

async fn recv_login(login: &mut Option<mpsc::Receiver<LoginEvent>>) -> Option<LoginEvent> {
    match login {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

impl App {
    fn new(readiness: Readiness, items: Vec<CachedItem>) -> Self {
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
        Self {
            local,
            peers,
            items,
            peer_index: 0,
            item_index: 0,
            focus_devices: true,
            mode,
            draft: None,
            composer: Zeroizing::new(String::new()),
            search: String::new(),
            notice,
            login_url: None,
            auth_can_login,
            watch: false,
            spinner: 0,
        }
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

#[allow(clippy::too_many_arguments)]
async fn handle_key(
    key: KeyEvent,
    app: &mut App,
    session: &mut Session,
    client: &TailClient,
    cache: &ReceiveCache,
    backend_tx: &mpsc::Sender<BackendEvent>,
    operation: &mut Option<Operation>,
    login: &mut Option<mpsc::Receiver<LoginEvent>>,
    login_cancel: &mut Option<watch::Sender<bool>>,
    login_task: &mut Option<JoinHandle<()>>,
) -> Result<bool> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if matches!(app.mode, Mode::Busy) {
            if let Some(operation) = operation.as_ref() {
                let _ = operation.cancel.send(true);
            }
            app.watch = false;
            app.notice = Some("Cancelling and reaping Tailscale process…".into());
            return Ok(false);
        }
        cancel_login(login, login_cancel, login_task).await;
        return Ok(true);
    }
    if matches!(key.code, KeyCode::Char('q')) && matches!(app.mode, Mode::Browse | Mode::Auth) {
        cancel_login(login, login_cancel, login_task).await;
        return Ok(true);
    }
    match &mut app.mode {
        Mode::Auth => match key.code {
            KeyCode::Char('o') => {
                if let Some(url) = &app.login_url {
                    if let Err(error) = open_external(ExternalTarget::Url(url)) {
                        app.notice = Some(format!("Could not open login link: {error:#}"));
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(url) = &app.login_url {
                    app.notice = Some(match session.copy(url) {
                        Ok(()) => "Login link copied".into(),
                        Err(error) => format!("Could not copy login link: {error:#}"),
                    });
                }
            }
            KeyCode::Enter if login.is_none() => {
                if !app.auth_can_login {
                    app.reconcile_readiness(client.readiness().await?);
                }
                if app.auth_can_login {
                    begin_login(client, login, login_cancel, login_task);
                    app.notice = Some("Starting Tailscale authentication…".into());
                }
            }
            KeyCode::Esc => {
                cancel_login(login, login_cancel, login_task).await;
                app.notice = Some("Authentication cancelled; Enter retries".into());
            }
            _ => {}
        },
        Mode::Browse => match key.code {
            KeyCode::Tab | KeyCode::Left | KeyCode::Right => app.focus_devices = !app.focus_devices,
            KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
            KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
            KeyCode::Char('p') if app.selected_target().is_some() => {
                if !matches!(app.draft, Some(Draft::Text(_))) {
                    app.composer.clear();
                    app.draft = None;
                }
                app.mode = Mode::Compose;
            }
            KeyCode::Char('f') if app.selected_target().is_some() => {
                let mut browser = FileBrowser::open(std::env::current_dir()?)?;
                if let Some(Draft::Files(paths)) = &app.draft {
                    browser.selected.extend(paths.iter().cloned());
                }
                app.mode = Mode::FileBrowser(browser);
            }
            KeyCode::Char('/') => {
                app.mode = Mode::Search;
                app.search.clear();
            }
            KeyCode::Char('r') => start_receive(app, client, cache, backend_tx, operation)?,
            KeyCode::Char('w') => {
                app.watch = true;
                start_receive(app, client, cache, backend_tx, operation)?;
            }
            KeyCode::Char('c') if !app.focus_devices => {
                if let Some(item) = app.selected_item().cloned() {
                    app.notice =
                        Some(match cache.read_text(&item).and_then(|text| session.copy(&text)) {
                            Ok(()) => format!("Copied {}", item.name),
                            Err(error) => format!("Could not copy {}: {error:#}", item.name),
                        });
                }
            }
            KeyCode::Char('s') if !app.focus_devices => {
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
            KeyCode::Char('o') if !app.focus_devices => {
                if let Some(item) = app.selected_item().cloned() {
                    if let Err(error) = open_external(ExternalTarget::Path(&item.payload())) {
                        app.notice = Some(format!("Could not open {}: {error:#}", item.name));
                    }
                }
            }
            KeyCode::Char('d') if !app.focus_devices && app.selected_item().is_some() => {
                app.mode = Mode::ConfirmDelete
            }
            KeyCode::Char(' ') if !app.focus_devices && app.selected_item().is_some() => {
                app.mode = Mode::Actions(0)
            }
            KeyCode::Enter if !app.focus_devices && app.selected_item().is_some() => {
                open_detail(app, cache)
            }
            KeyCode::Enter if app.focus_devices && app.draft.is_some() => app.mode = Mode::Review,
            _ => {}
        },
        Mode::Compose => match key.code {
            KeyCode::Esc => app.mode = Mode::Browse,
            KeyCode::Tab => cycle_peer(app, 1),
            KeyCode::Enter if !app.composer.is_empty() => {
                app.draft = Some(Draft::Text(app.composer.clone()));
                app.mode = Mode::Review;
            }
            KeyCode::Backspace => {
                app.composer.pop();
            }
            KeyCode::Char(character) => app.composer.push(character),
            _ => {}
        },
        Mode::Review => match key.code {
            KeyCode::Esc => {
                app.mode = if matches!(app.draft, Some(Draft::Text(_))) {
                    Mode::Compose
                } else {
                    Mode::Browse
                }
            }
            KeyCode::Tab => cycle_peer(app, 1),
            KeyCode::Enter => start_send(app, client, backend_tx, operation)?,
            _ => {}
        },
        Mode::Ambiguous(input) => match key.code {
            KeyCode::Char('a') => {
                if let PastedInput::Ambiguous { existing, .. } = input {
                    app.draft = Some(Draft::Files(existing.clone()));
                }
                app.mode = Mode::Review;
            }
            KeyCode::Char('t') => {
                if let PastedInput::Ambiguous { raw, .. } = input {
                    app.composer = Zeroizing::new(raw.clone());
                    app.draft = Some(Draft::Text(Zeroizing::new(raw.clone())));
                }
                app.mode = Mode::Review;
            }
            KeyCode::Esc => app.mode = Mode::Compose,
            _ => {}
        },
        Mode::Search => match key.code {
            KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Browse,
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
        Mode::Detail { .. } => match key.code {
            KeyCode::Char('c') => {
                if let Some(item) = app.selected_item().cloned() {
                    app.notice =
                        Some(match cache.read_text(&item).and_then(|text| session.copy(&text)) {
                            Ok(()) => format!("Copied {}", item.name),
                            Err(error) => format!("Could not copy {}: {error:#}", item.name),
                        });
                }
            }
            KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Browse,
            _ => {}
        },
        Mode::ConfirmDelete => match key.code {
            KeyCode::Char('y') => {
                if let Some(item) = app.selected_item().cloned() {
                    cache.delete(&item)?;
                    app.items = cache.list()?;
                    app.item_index = 0;
                }
                app.mode = Mode::Browse;
                app.notice = Some("Deleted cached item".into());
            }
            KeyCode::Esc | KeyCode::Char('n') => app.mode = Mode::Browse,
            _ => {}
        },
        Mode::Actions(index) => match key.code {
            KeyCode::Esc => app.mode = Mode::Browse,
            KeyCode::Up | KeyCode::Char('k') => *index = index.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *index = (*index + 1).min(3),
            KeyCode::Enter => {
                let action = *index;
                let item = app.selected_item().cloned();
                match (action, item) {
                    (0, Some(item)) if item.kind == ItemKind::Text => {
                        app.notice = Some(
                            match cache.read_text(&item).and_then(|text| session.copy(&text)) {
                                Ok(()) => format!("Copied {}", item.name),
                                Err(error) => format!("Could not copy {}: {error:#}", item.name),
                            },
                        );
                        app.mode = Mode::Browse;
                    }
                    (0, Some(_)) => {
                        app.notice =
                            Some("Copy is available for UTF-8 text items up to 1 MiB".into());
                    }
                    (1, Some(item)) => {
                        let directory = UserDirs::new()
                            .and_then(|dirs| dirs.download_dir().map(PathBuf::from))
                            .unwrap_or(std::env::current_dir()?);
                        match FileBrowser::open(directory) {
                            Ok(browser) => {
                                app.mode = Mode::SaveBrowser(SaveBrowser { browser, item })
                            }
                            Err(error) => {
                                app.mode = Mode::Browse;
                                app.notice = Some(format!("Could not browse: {error:#}"));
                            }
                        }
                    }
                    (2, Some(item)) => {
                        app.notice =
                            Some(match open_external(ExternalTarget::Path(&item.payload())) {
                                Ok(()) => format!("Opening {}", item.name),
                                Err(error) => format!("Could not open {}: {error:#}", item.name),
                            });
                        app.mode = Mode::Browse;
                    }
                    (3, Some(_)) => app.mode = Mode::ConfirmDelete,
                    _ => app.mode = Mode::Browse,
                }
            }
            _ => {}
        },
        Mode::FileBrowser(browser) => match key.code {
            KeyCode::Esc => app.mode = Mode::Browse,
            KeyCode::Up | KeyCode::Char('k') => browser.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => browser.move_selection(1),
            KeyCode::Backspace => {
                if let Err(error) = browser.parent() {
                    app.notice = Some(format!("Could not open parent: {error:#}"));
                }
            }
            KeyCode::Char(' ') => browser.toggle_selected_file(),
            KeyCode::Char('s') if !browser.selected.is_empty() => {
                app.draft = Some(Draft::Files(browser.selected.iter().cloned().collect()));
                app.mode = Mode::Review;
            }
            KeyCode::Enter => {
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
            _ => {}
        },
        Mode::SaveBrowser(save) => match key.code {
            KeyCode::Esc => app.mode = Mode::Browse,
            KeyCode::Up | KeyCode::Char('k') => save.browser.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => save.browser.move_selection(1),
            KeyCode::Backspace => {
                if let Err(error) = save.browser.parent() {
                    app.notice = Some(format!("Could not open parent: {error:#}"));
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = save.browser.selected_entry().filter(|entry| entry.directory) {
                    let path = entry.path.clone();
                    if let Err(error) = save.browser.enter(path) {
                        app.notice = Some(format!("Could not open directory: {error:#}"));
                    }
                }
            }
            KeyCode::Char('s') => {
                let item = save.item.clone();
                let directory = save.browser.directory.clone();
                if cache.destination_path(&item, &directory).exists() {
                    app.mode = Mode::SaveConflict { item, directory };
                } else {
                    finish_save(app, cache, &item, &directory, SaveConflictResolution::Rename)?;
                }
            }
            _ => {}
        },
        Mode::SaveConflict { item, directory } => match key.code {
            KeyCode::Char('r') => {
                let item = item.clone();
                let directory = directory.clone();
                finish_save(app, cache, &item, &directory, SaveConflictResolution::Rename)?;
            }
            KeyCode::Char('x') => {
                let item = item.clone();
                let directory = directory.clone();
                finish_save(app, cache, &item, &directory, SaveConflictResolution::Replace)?;
            }
            KeyCode::Esc => {
                let item = item.clone();
                match FileBrowser::open(directory.clone()) {
                    Ok(browser) => app.mode = Mode::SaveBrowser(SaveBrowser { browser, item }),
                    Err(error) => {
                        app.mode = Mode::Browse;
                        app.notice = Some(format!("Could not browse: {error:#}"));
                    }
                }
            }
            _ => {}
        },
        Mode::Busy => {
            if key.code == KeyCode::Esc {
                if let Some(operation) = operation.as_ref() {
                    let _ = operation.cancel.send(true);
                }
                app.watch = false;
                app.notice = Some("Cancelling and reaping Tailscale process…".into());
            }
        }
    }
    Ok(false)
}

async fn cancel_login(
    login: &mut Option<mpsc::Receiver<LoginEvent>>,
    cancel: &mut Option<watch::Sender<bool>>,
    task: &mut Option<JoinHandle<()>>,
) {
    if let Some(cancel) = cancel.take() {
        let _ = cancel.send(true);
    }
    if let Some(task) = task.take() {
        let _ = task.await;
    }
    *login = None;
}

fn begin_login(
    client: &TailClient,
    login: &mut Option<mpsc::Receiver<LoginEvent>>,
    cancel: &mut Option<watch::Sender<bool>>,
    task: &mut Option<JoinHandle<()>>,
) {
    let (receiver, cancel_sender, login_task) = client.start_login();
    *login = Some(receiver);
    *cancel = Some(cancel_sender);
    *task = Some(login_task);
}

fn handle_paste(app: &mut App, raw: String) {
    if !matches!(app.mode, Mode::Compose | Mode::Browse | Mode::FileBrowser(_))
        || app.selected_target().is_none()
    {
        return;
    }
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
}

fn move_selection(app: &mut App, delta: isize) {
    let len =
        if app.focus_devices { app.filtered_peers().len() } else { app.filtered_items().len() };
    let index = if app.focus_devices { &mut app.peer_index } else { &mut app.item_index };
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

fn start_receive(
    app: &mut App,
    client: &TailClient,
    cache: &ReceiveCache,
    sender: &mpsc::Sender<BackendEvent>,
    operation: &mut Option<Operation>,
) -> Result<()> {
    let staging = cache.staging_directory()?;
    let wait = app.watch;
    let client = client.clone();
    let cache = cache.clone();
    let sender = sender.clone();
    let (cancel, cancel_receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let result = async {
            client.receive_into(staging.path(), wait, cancel_receiver).await?;
            cache.import_staging(staging)
        }
        .await
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::Received(result)).await;
    });
    *operation = Some(Operation { cancel, task });
    app.mode = Mode::Busy;
    app.notice = Some(
        if wait {
            "Waiting for the next Taildrop…  Esc cancels"
        } else {
            "Receiving…  Esc cancels"
        }
        .into(),
    );
    Ok(())
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

fn render(frame: &mut Frame<'_>, app: &App) {
    let rows =
        Layout::vertical([Constraint::Length(3), Constraint::Min(10), Constraint::Length(3)])
            .split(frame.area());
    render_header(frame, rows[0], app);
    match &app.mode {
        Mode::Compose => render_compose(frame, rows[1], app),
        Mode::Review => render_review(frame, rows[1], app),
        Mode::Ambiguous(input) => render_ambiguous(frame, rows[1], input),
        Mode::Detail { preview } => {
            render_detail(frame, rows[1], app, preview.as_ref().map(|text| text.as_str()))
        }
        Mode::ConfirmDelete => render_confirm_delete(frame, rows[1], app),
        Mode::Actions(index) => render_actions(frame, rows[1], app, *index),
        Mode::FileBrowser(browser) => render_file_browser(frame, rows[1], browser),
        Mode::SaveBrowser(save) => render_save_browser(frame, rows[1], save),
        Mode::SaveConflict { item, directory } => {
            render_save_conflict(frame, rows[1], item, directory)
        }
        Mode::Auth => render_auth(frame, rows[1], app),
        _ => render_browser(frame, rows[1], app),
    }
    render_footer(frame, rows[2], app);
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

fn render_browser(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let columns = if area.width < 80 {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area)
    } else {
        Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)]).split(area)
    };
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
            let style = if index == app.peer_index && app.focus_devices {
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
        List::new(peers).block(panel(" Devices · send to ", app.focus_devices)),
        columns[0],
    );
    let items = app
        .filtered_items()
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let kind = if item.kind == ItemKind::Text { "text" } else { "file" };
            let style = if index == app.item_index && !app.focus_devices {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(format!(" {:<28} {:>8}  {kind}", item.name, human_bytes(item.bytes)))
                .style(style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(panel(" Received · 30-day cache ", !app.focus_devices)),
        columns[1],
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

fn render_actions(frame: &mut Frame<'_>, area: Rect, app: &App, selected: usize) {
    let text_item = app.selected_item().is_some_and(|item| item.kind == ItemKind::Text);
    let actions =
        [("Copy to clipboard", text_item), ("Save…", true), ("Open", true), ("Delete…", true)];
    let items = actions
        .iter()
        .enumerate()
        .map(|(index, (label, enabled))| {
            let suffix = if *enabled { "" } else { " · text only" };
            let style = if index == selected {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else if *enabled {
                Style::default().fg(TEXT)
            } else {
                Style::default().fg(MUTED)
            };
            ListItem::new(format!(" {label}{suffix}")).style(style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(panel(" Item actions ", true)), area);
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

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let controls = match app.mode {
        Mode::Browse => "p paste/share   f files   drag file   Enter resume draft   r receive   w watch   Tab switch   / search   c copy   s save   o open   d delete   q quit",
        Mode::Compose => "paste or drag files   Tab recipient   Enter review   Esc keep draft",
        Mode::Review => "Tab recipient   Enter send   Esc keep/edit draft",
        Mode::Search => "type to filter   Enter/Esc done",
        Mode::Actions(_) => "↑↓ choose   Enter run   Esc cancel",
        Mode::Detail { .. } => "c copy text   Enter/Esc back",
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
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                notice,
                Style::default().fg(if app.notice.is_some() { WARN } else { MUTED }),
            ),
            Line::styled(controls, Style::default().fg(MUTED)),
        ]),
        area,
    );
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
        terminal.draw(|frame| render(frame, &app)).unwrap();
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
        terminal.draw(|frame| render(frame, &app)).unwrap();
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
        terminal.draw(|frame| render(frame, &app)).unwrap();
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
        app.focus_devices = false;
        open_detail(&mut app, &cache);

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
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
