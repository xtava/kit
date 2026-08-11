use std::sync::Arc;

use anyhow::{anyhow, Result};
use crossterm::event::{Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::onepassword::{OpError, SecretBytes};
use crate::tui::{
    render_vertical_scrollbar, ActionId, ActionInvocation, EventReader, FuzzyIndex, KeyChord,
    KeybindingResolution, KeybindingState, LineEditor, ScrollbarDrag, ScrollbarLayout,
    ScrollbarStyle, SearchMode, SelectableRegion, SelectionOutcome, Session, SessionOptions,
    TextSelection, Viewport, ViewportMetrics,
};

use super::actions::{
    self, SecretsActionContext, SecretsActionMode, SecretsActionRegistry, SecretsCommand,
};
use super::model::{AccountSummary, CreateLoginRequest, ItemRef, ItemSummary, VaultSummary};
use super::op::{LoginField, OpClient};
use super::sensitive::SensitiveInput;

pub async fn run(
    client: OpClient,
    accounts: Vec<AccountSummary>,
    requested_account: Option<String>,
) -> Result<()> {
    let selected_account = select_requested_account(&accounts, requested_account.as_deref())?;
    let registry = actions::registry()?;
    let mut session =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let account = match selected_account {
        Some(index) => accounts.get(index).cloned(),
        None => choose_account(&mut session, &mut events, &accounts, &registry).await?,
    };
    let Some(account) = account else { return Ok(()) };
    let mut app = App::new(account, registry);
    let (tx, mut rx) = mpsc::unbounded_channel();

    start_load(&mut app, &client, &tx);
    let mut regions = UiRegions::default();

    loop {
        app.update_search();
        session.draw(|frame| regions = render(frame, &mut app))?;

        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    Event::Key(key) if key.is_press() => {
                        match app.text_selection.on_key(key) {
                            SelectionOutcome::CopyReady(text) => session.copy(&text)?,
                            SelectionOutcome::Captured | SelectionOutcome::Changed => continue,
                            SelectionOutcome::Unhandled | SelectionOutcome::EdgeScroll { .. } => {
                                if handle_key(key, &mut app, &client, &tx) { break; }
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        if let Some(command) = app.handle_mouse(mouse, &regions) {
                            if handle_resolved_command(command, &mut app, &client, &tx) { break; }
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            Some(result) = rx.recv() => {
                handle_result(result, &mut app, &client, &tx, &mut session);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionSurface {
    PublicDetail,
}

#[derive(Default)]
struct UiRegions {
    item_rows: Vec<(Rect, usize)>,
    items: Option<Rect>,
    scrollbar: Option<ScrollbarLayout>,
    selectable: Vec<SelectableRegion<SelectionSurface>>,
    create_fields: Vec<(Rect, CreateField)>,
    actions: Vec<(Rect, ActionId)>,
}

fn select_requested_account(
    accounts: &[AccountSummary],
    requested: Option<&str>,
) -> Result<Option<usize>> {
    if let Some(requested) = requested {
        return accounts
            .iter()
            .position(|account| account.matches(requested))
            .map(Some)
            .ok_or_else(|| {
                anyhow!("1Password account selector did not match a configured account")
            });
    }
    Ok((accounts.len() == 1).then_some(0))
}

async fn choose_account(
    session: &mut Session,
    events: &mut EventReader,
    accounts: &[AccountSummary],
    registry: &SecretsActionRegistry,
) -> Result<Option<AccountSummary>> {
    let mut selection = 0;
    let mut viewport = Viewport::default();
    let mut keybindings = KeybindingState::default();
    let mut rows = Vec::new();
    loop {
        session.draw(|frame| {
            rows = render_account_picker(frame, accounts, selection, &mut viewport)
        })?;
        let Some(event) = events.recv().await else { return Ok(None) };
        if let Event::Mouse(mouse) = event {
            match mouse.kind {
                MouseEventKind::ScrollUp => selection = selection.saturating_sub(1),
                MouseEventKind::ScrollDown => {
                    selection = (selection + 1).min(accounts.len().saturating_sub(1))
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some((_, index)) = rows
                        .iter()
                        .find(|(area, _)| area.contains(Position::new(mouse.column, mouse.row)))
                    {
                        return Ok(accounts.get(*index).cloned());
                    }
                }
                _ => {}
            }
            continue;
        }
        let Event::Key(key) = event else { continue };
        if !key.is_press() {
            continue;
        }
        let Some(chord) = KeyChord::from_event(key) else { continue };
        let context = SecretsActionContext {
            mode: SecretsActionMode::AccountPicker,
            busy: false,
            has_item: !accounts.is_empty(),
            selected_login: false,
            has_vaults: false,
        };
        let invocation = match registry.resolve_keybinding(&mut keybindings, chord, context) {
            KeybindingResolution::Invoke(invocation) => invocation,
            KeybindingResolution::Pending
            | KeybindingResolution::Unmatched
            | KeybindingResolution::UnmatchedSequence { .. } => continue,
        };
        match registry.command_for(&invocation)? {
            SecretsCommand::Previous => selection = selection.saturating_sub(1),
            SecretsCommand::Next => {
                selection = (selection + 1).min(accounts.len().saturating_sub(1))
            }
            SecretsCommand::Activate => return Ok(accounts.get(selection).cloned()),
            SecretsCommand::Quit | SecretsCommand::Cancel => return Ok(None),
            _ => {}
        }
    }
}

enum Screen {
    Browse,
    Search,
    Create(CreateForm),
    Confirm(Confirmation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationKind {
    RotatePassword,
    Archive,
}

impl MutationKind {
    fn verb(self) -> &'static str {
        match self {
            Self::RotatePassword => "Rotate password",
            Self::Archive => "Archive item",
        }
    }

    fn warning(self) -> &'static str {
        match self {
            Self::RotatePassword => {
                "This replaces the current password with a generated 32-character password."
            }
            Self::Archive => "The item moves to 1Password Archive; it is not permanently deleted.",
        }
    }
}

struct Confirmation {
    action: MutationKind,
    item: ItemSummary,
}

struct App {
    account: AccountSummary,
    vaults: Vec<VaultSummary>,
    index: FuzzyIndex<ItemSummary>,
    visible: Vec<Arc<ItemSummary>>,
    selection: usize,
    query: LineEditor,
    open_item: Option<ItemRef>,
    pending: Option<&'static str>,
    notice: Option<Notice>,
    screen: Screen,
    item_viewport: Viewport,
    item_metrics: ViewportMetrics,
    scrollbar_drag: Option<ScrollbarDrag>,
    text_selection: TextSelection<SelectionSurface>,
    detail_revision: u64,
    registry: SecretsActionRegistry,
    keybindings: KeybindingState,
}

impl App {
    fn new(account: AccountSummary, registry: SecretsActionRegistry) -> Self {
        Self {
            account,
            vaults: Vec::new(),
            index: FuzzyIndex::new(SearchMode::Text, || {}),
            visible: Vec::new(),
            selection: 0,
            query: LineEditor::default(),
            open_item: None,
            pending: None,
            notice: None,
            screen: Screen::Browse,
            item_viewport: Viewport::default(),
            item_metrics: ViewportMetrics::default(),
            scrollbar_drag: None,
            text_selection: TextSelection::default(),
            detail_revision: 0,
            registry,
            keybindings: KeybindingState::default(),
        }
    }

    fn selected_item(&self) -> Option<&ItemSummary> {
        self.visible.get(self.selection).map(Arc::as_ref)
    }

    fn select_relative(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        self.selection = self.selection.saturating_add_signed(delta).min(last);
        self.clear_open_item();
    }

    fn clear_open_item(&mut self) {
        self.open_item = None;
        self.detail_revision = self.detail_revision.wrapping_add(1);
        self.text_selection.clear();
    }

    fn item_is_open(&self, reference: &ItemRef) -> bool {
        self.open_item.as_ref() == Some(reference)
    }

    fn update_search(&mut self) {
        let Some(matches) = self.index.search(self.query.value()) else {
            return;
        };
        let selected_id = self.selected_item().map(|item| item.reference.item_id.clone());
        self.visible = matches.into_iter().map(|matched| matched.item).collect();
        self.selection = selected_id
            .and_then(|id| self.visible.iter().position(|item| item.reference.item_id == id))
            .unwrap_or(0)
            .min(self.visible.len().saturating_sub(1));
    }

    fn replace_items(&mut self, items: Vec<ItemSummary>) {
        self.index.replace(items.into_iter().map(|item| {
            let text = item.search_text();
            (item, text)
        }));
        self.visible.clear();
        self.selection = 0;
        self.clear_open_item();
        self.update_search();
    }

    fn busy(&self) -> bool {
        self.pending.is_some()
    }

    fn action_context(&self) -> SecretsActionContext {
        let mode = match &self.screen {
            Screen::Browse => SecretsActionMode::Browse,
            Screen::Search => SecretsActionMode::Search,
            Screen::Create(form) => {
                SecretsActionMode::Create { vault_field: form.field() == CreateField::Vault }
            }
            Screen::Confirm(_) => SecretsActionMode::Confirm,
        };
        SecretsActionContext {
            mode,
            busy: self.busy(),
            has_item: self.selected_item().is_some(),
            selected_login: self.selected_item().is_some_and(ItemSummary::is_login),
            has_vaults: !self.vaults.is_empty(),
        }
    }

    fn command_for_key(&mut self, key: KeyEvent) -> Option<SecretsCommand> {
        let chord = KeyChord::from_event(key)?;
        let context = self.action_context();
        let invocation =
            match self.registry.resolve_keybinding(&mut self.keybindings, chord, context) {
                KeybindingResolution::Invoke(invocation) => invocation,
                KeybindingResolution::Pending
                | KeybindingResolution::Unmatched
                | KeybindingResolution::UnmatchedSequence { .. } => return None,
            };
        self.registry.command_for(&invocation).ok()
    }

    fn command_for_action(&self, action: ActionId) -> Option<SecretsCommand> {
        self.registry.command_for(&ActionInvocation::new(action, self.action_context())).ok()
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, regions: &UiRegions) -> Option<SecretsCommand> {
        let position = Position::new(mouse.column, mouse.row);
        if let Some(drag) = self.scrollbar_drag {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(scrollbar) = regions.scrollbar {
                        let top = drag.top_for_row(scrollbar, position.y);
                        self.item_viewport.set_top(top, self.item_metrics);
                        self.selection = top;
                        self.clear_open_item();
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => self.scrollbar_drag = None,
                _ => {}
            }
            return None;
        }
        if let Some(scrollbar) = regions.scrollbar.filter(|scrollbar| scrollbar.contains(position))
        {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some(drag) = ScrollbarDrag::begin(scrollbar, position) {
                    self.scrollbar_drag = Some(drag);
                } else {
                    let top = scrollbar.top_for_track_row(position.y);
                    self.item_viewport.set_top(top, self.item_metrics);
                    self.selection = top;
                    self.clear_open_item();
                }
            }
            return None;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((_, action)) =
                regions.actions.iter().find(|(area, _)| area.contains(position))
            {
                return self.command_for_action(*action);
            }
            if let Some((_, field)) =
                regions.create_fields.iter().find(|(area, _)| area.contains(position))
            {
                if let Screen::Create(form) = &mut self.screen {
                    form.focus = CreateField::ALL
                        .iter()
                        .position(|candidate| candidate == field)
                        .unwrap_or(form.focus);
                }
                return None;
            }
        }
        if regions.items.is_some_and(|area| area.contains(position)) {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.select_relative(-3);
                    return None;
                }
                MouseEventKind::ScrollDown => {
                    self.select_relative(3);
                    return None;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some((_, index)) =
                        regions.item_rows.iter().find(|(area, _)| area.contains(position))
                    {
                        if self.selection == *index {
                            return self.command_for_action(actions::ACTIVATE);
                        } else {
                            self.selection = *index;
                            self.clear_open_item();
                        }
                    }
                    return None;
                }
                _ => {}
            }
        }
        let _ = self.text_selection.on_mouse(mouse);
        None
    }
}

struct Notice {
    text: String,
    error: bool,
}

impl Notice {
    fn info(text: impl Into<String>) -> Self {
        Self { text: text.into(), error: false }
    }

    fn error(error: impl std::fmt::Display) -> Self {
        Self { text: error.to_string(), error: true }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CreateField {
    Vault,
    Title,
    Username,
    Url,
    Password,
}

impl CreateField {
    const ALL: [Self; 5] = [Self::Vault, Self::Title, Self::Username, Self::Url, Self::Password];
}

struct CreateForm {
    focus: usize,
    vault: usize,
    title: LineEditor,
    username: LineEditor,
    url: LineEditor,
    password: SensitiveInput,
}

impl CreateForm {
    fn new(vault: usize) -> Self {
        Self {
            focus: 1,
            vault,
            title: LineEditor::default(),
            username: LineEditor::default(),
            url: LineEditor::default(),
            password: SensitiveInput::default(),
        }
    }

    fn field(&self) -> CreateField {
        CreateField::ALL[self.focus]
    }

    fn move_focus(&mut self, delta: isize) {
        self.focus = self.focus.saturating_add_signed(delta).min(CreateField::ALL.len() - 1);
    }
}

enum AsyncResult {
    Loaded(Result<(Vec<VaultSummary>, Vec<ItemSummary>), OpError>),
    Field { item: ItemRef, field: LoginField, result: Result<SecretBytes, OpError> },
    Mutation { success: &'static str, result: Result<(), OpError> },
}

fn handle_key(
    key: KeyEvent,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> bool {
    let command = app.command_for_key(key);
    let screen = std::mem::replace(&mut app.screen, Screen::Browse);
    let next = match screen {
        Screen::Browse => handle_browse_command(command, app, client, tx),
        Screen::Search => handle_search_input(command, Some(key), app),
        Screen::Create(form) => handle_create_input(command, Some(key), form, app, client, tx),
        Screen::Confirm(confirmation) => {
            handle_confirmation_command(command, confirmation, app, client, tx)
        }
    };
    let Some(screen) = next else { return true };
    app.screen = screen;
    false
}

fn handle_resolved_command(
    command: SecretsCommand,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> bool {
    let screen = std::mem::replace(&mut app.screen, Screen::Browse);
    let next = match screen {
        Screen::Browse => handle_browse_command(Some(command), app, client, tx),
        Screen::Search => handle_search_input(Some(command), None, app),
        Screen::Create(form) => handle_create_input(Some(command), None, form, app, client, tx),
        Screen::Confirm(confirmation) => {
            handle_confirmation_command(Some(command), confirmation, app, client, tx)
        }
    };
    let Some(screen) = next else { return true };
    app.screen = screen;
    false
}

fn handle_browse_command(
    command: Option<SecretsCommand>,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> Option<Screen> {
    match command {
        Some(SecretsCommand::Quit) => return None,
        Some(SecretsCommand::Search) => return Some(Screen::Search),
        Some(SecretsCommand::Previous) => app.select_relative(-1),
        Some(SecretsCommand::Next) => app.select_relative(1),
        Some(SecretsCommand::PageUp) => {
            app.select_relative(-(app.item_metrics.visible_len().max(1) as isize))
        }
        Some(SecretsCommand::PageDown) => {
            app.select_relative(app.item_metrics.visible_len().max(1) as isize)
        }
        Some(SecretsCommand::First) => {
            app.selection = 0;
            app.clear_open_item();
        }
        Some(SecretsCommand::Last) => {
            app.selection = app.visible.len().saturating_sub(1);
            app.clear_open_item();
        }
        Some(SecretsCommand::Activate) => {
            app.open_item = app.selected_item().map(|item| item.reference.clone());
            app.detail_revision = app.detail_revision.wrapping_add(1);
            app.text_selection.clear();
        }
        Some(SecretsCommand::CopyUsername) => {
            start_field_copy(app, client, tx, LoginField::Username)
        }
        Some(SecretsCommand::ConfirmOrCopyPassword) => {
            start_field_copy(app, client, tx, LoginField::Password)
        }
        Some(SecretsCommand::CreateOrReject) => {
            app.clear_open_item();
            return Some(Screen::Create(CreateForm::new(0)));
        }
        Some(SecretsCommand::RotatePassword) => {
            return Some(begin_confirmation(app, MutationKind::RotatePassword));
        }
        Some(SecretsCommand::Archive) => {
            return Some(begin_confirmation(app, MutationKind::Archive));
        }
        Some(SecretsCommand::Refresh) => start_load(app, client, tx),
        Some(SecretsCommand::Cancel) => app.clear_open_item(),
        _ => {}
    }
    Some(Screen::Browse)
}

fn handle_search_input(
    command: Option<SecretsCommand>,
    key: Option<KeyEvent>,
    app: &mut App,
) -> Option<Screen> {
    match command {
        Some(SecretsCommand::Quit) => return None,
        Some(SecretsCommand::Cancel | SecretsCommand::Activate) => return Some(Screen::Browse),
        Some(SecretsCommand::Previous) => app.select_relative(-1),
        Some(SecretsCommand::Next) => app.select_relative(1),
        None if key.is_some() => {
            let key = key.expect("checked above");
            app.query.apply_key(key);
            app.clear_open_item();
        }
        _ => {}
    }
    Some(Screen::Search)
}

fn handle_create_input(
    command: Option<SecretsCommand>,
    key: Option<KeyEvent>,
    mut form: CreateForm,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> Option<Screen> {
    match command {
        Some(SecretsCommand::Quit) => return None,
        Some(SecretsCommand::Cancel) => return Some(Screen::Browse),
        Some(SecretsCommand::Save) => return Some(submit_create(form, app, client, tx)),
        Some(SecretsCommand::NextField) => form.move_focus(1),
        Some(SecretsCommand::PreviousField) => form.move_focus(-1),
        Some(SecretsCommand::PreviousVault) => form.vault = form.vault.saturating_sub(1),
        Some(SecretsCommand::NextVault) => {
            form.vault = (form.vault + 1).min(app.vaults.len().saturating_sub(1))
        }
        Some(SecretsCommand::Activate) => {
            if form.field() == CreateField::Password {
                return Some(submit_create(form, app, client, tx));
            } else {
                form.move_focus(1);
            }
        }
        None if key.is_some() => {
            let key = key.expect("checked above");
            match form.field() {
                CreateField::Vault => {}
                CreateField::Title => form.title.apply_key(key),
                CreateField::Username => form.username.apply_key(key),
                CreateField::Url => form.url.apply_key(key),
                CreateField::Password => form.password.apply_key(key),
            }
        }
        _ => {}
    }
    Some(Screen::Create(form))
}

fn handle_confirmation_command(
    command: Option<SecretsCommand>,
    confirmation: Confirmation,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> Option<Screen> {
    match command {
        Some(SecretsCommand::Quit) => None,
        Some(SecretsCommand::ConfirmOrCopyPassword | SecretsCommand::Activate) => {
            start_mutation(app, client, tx, confirmation.action, confirmation.item.reference);
            Some(Screen::Browse)
        }
        Some(SecretsCommand::CreateOrReject | SecretsCommand::Cancel) => Some(Screen::Browse),
        _ => Some(Screen::Confirm(confirmation)),
    }
}

fn begin_confirmation(app: &mut App, action: MutationKind) -> Screen {
    if let Some(item) = app.selected_item().cloned() {
        app.clear_open_item();
        Screen::Confirm(Confirmation { action, item })
    } else {
        Screen::Browse
    }
}

fn submit_create(
    mut form: CreateForm,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> Screen {
    if app.busy() {
        return Screen::Create(form);
    }
    let title = form.title.value().trim().to_owned();
    if title.is_empty() {
        app.notice = Some(Notice::error("Title is required"));
        return Screen::Create(form);
    };
    let Some(vault) = app.vaults.get(form.vault).map(|vault| vault.id.clone()) else {
        app.notice = Some(Notice::error("No vault selected"));
        return Screen::Create(form);
    };
    let password = (!form.password.is_empty()).then(|| form.password.take());
    let request = CreateLoginRequest {
        account_id: app.account.id.clone(),
        vault_id: vault,
        title,
        username: form.username.value().to_owned(),
        url: form.url.value().to_owned(),
        password,
    };
    start_create(app, client, tx, request);
    Screen::Browse
}

fn start_load(app: &mut App, client: &OpClient, tx: &UnboundedSender<AsyncResult>) {
    if app.busy() {
        return;
    }
    app.pending = Some("Loading 1Password metadata…");
    app.notice = None;
    let account = app.account.id.clone();
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            let vaults = client.vaults(&account).await?;
            let items = client.items(&account).await?;
            Ok((vaults, items))
        }
        .await;
        let _ = tx.send(AsyncResult::Loaded(result));
    });
}

fn start_field_copy(
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
    field: LoginField,
) {
    let Some(item) = app.selected_item() else { return };
    if !app.item_is_open(&item.reference) {
        app.notice = Some(Notice::error("Open this item with Enter before copying a field"));
        return;
    }
    if !item.is_login() {
        app.notice = Some(Notice::error("Username/password copy is limited to Login items"));
        return;
    }
    let reference = item.reference.clone();
    app.pending = Some(match field {
        LoginField::Username => "Fetching username for clipboard…",
        LoginField::Password => "Fetching password for clipboard…",
    });
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = client.field(&reference, field).await;
        let _ = tx.send(AsyncResult::Field { item: reference, field, result });
    });
}

fn start_create(
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
    request: CreateLoginRequest,
) {
    app.pending = Some("Creating Login item…");
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = client.create_login(request).await;
        let _ = tx.send(AsyncResult::Mutation { success: "Login created", result });
    });
}

fn start_mutation(
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
    action: MutationKind,
    reference: ItemRef,
) {
    let (pending, success) = match action {
        MutationKind::RotatePassword => ("Rotating password…", "Password rotated"),
        MutationKind::Archive => ("Archiving item…", "Item archived"),
    };
    app.pending = Some(pending);
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = match action {
            MutationKind::RotatePassword => client.rotate_password(&reference).await,
            MutationKind::Archive => client.archive(&reference).await,
        };
        let _ = tx.send(AsyncResult::Mutation { success, result });
    });
}

fn handle_result(
    result: AsyncResult,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
    session: &mut Session,
) {
    app.pending = None;
    match result {
        AsyncResult::Loaded(result) => match result {
            Ok((vaults, items)) => {
                app.vaults = vaults;
                app.replace_items(items);
                app.notice = Some(Notice::info(format!("Loaded {} items", app.index.len())));
            }
            Err(error) => app.notice = Some(Notice::error(error)),
        },
        AsyncResult::Field { item, field, result } => match result {
            Ok(value) if app.item_is_open(&item) => {
                app.notice = Some(match session.copy(value.as_str()) {
                    Ok(()) => Notice::info(format!(
                        "{} copied via OSC 52; the terminal/OS clipboard now owns it",
                        field.label()
                    )),
                    Err(error) => {
                        Notice::error(format!("Could not copy {}: {error}", field.label()))
                    }
                });
            }
            Ok(_) => {}
            Err(error) => app.notice = Some(Notice::error(error)),
        },
        AsyncResult::Mutation { success, result } => match result {
            Ok(()) => {
                app.notice = Some(Notice::info(format!("{success}; refreshing from 1Password")));
                refresh_account(app, client, tx);
            }
            Err(error) => app.notice = Some(Notice::error(error)),
        },
    }
}

fn refresh_account(app: &mut App, client: &OpClient, tx: &UnboundedSender<AsyncResult>) {
    start_load(app, client, tx);
}

fn render(frame: &mut Frame<'_>, app: &mut App) -> UiRegions {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    render_header(frame, rows[0], &app.account.label, app.query.value());
    let mut regions = UiRegions::default();
    match &app.screen {
        Screen::Create(form) => render_create(frame, rows[1], app, form, &mut regions),
        Screen::Confirm(confirmation) => {
            render_confirmation(frame, rows[1], confirmation, &mut regions)
        }
        Screen::Browse | Screen::Search => render_browser(frame, rows[1], app, &mut regions),
    }
    render_footer(frame, rows[2], app, &mut regions);
    if !matches!(app.screen, Screen::Browse | Screen::Search) {
        regions.selectable.clear();
    }
    let selectable = regions.selectable.clone();
    app.text_selection.capture_frame(
        frame,
        &selectable,
        Style::default().bg(Color::DarkGray).add_modifier(Modifier::REVERSED),
    );
    regions
}

fn render_account_picker(
    frame: &mut Frame<'_>,
    accounts: &[AccountSummary],
    selection: usize,
    viewport: &mut Viewport,
) -> Vec<(Rect, usize)> {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(2)])
        .split(frame.area());
    render_header(frame, rows[0], "select account", "");
    let inner = panel(" select account ").inner(rows[1]);
    let metrics = ViewportMetrics::new(accounts.len(), usize::from(inner.height));
    viewport.ensure_visible(selection, metrics);
    let visible = viewport.visible_range(metrics);
    render_accounts(frame, rows[1], accounts, selection, visible.clone());
    frame.render_widget(
        Paragraph::new(Line::styled(
            "  ↑/↓ select · Enter continue · q/Esc quit",
            Style::default().fg(Color::DarkGray),
        )),
        rows[2],
    );
    visible
        .enumerate()
        .map(|(offset, index)| (Rect::new(inner.x, inner.y + offset as u16, inner.width, 1), index))
        .collect()
}

fn render_header(frame: &mut Frame<'_>, area: Rect, account: &str, query: &str) {
    let search = if query.is_empty() { "".to_owned() } else { format!("  /{query}") };
    let line = Line::from(vec![
        Span::styled(
            " secrets ",
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(account, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(search, Style::default().fg(Color::Yellow)),
    ]);
    frame.render_widget(Paragraph::new(line).block(panel(" 1Password ")), area);
}

fn render_accounts(
    frame: &mut Frame<'_>,
    area: Rect,
    accounts: &[AccountSummary],
    selection: usize,
    visible: std::ops::Range<usize>,
) {
    let top = visible.start;
    let items = accounts[visible].iter().map(|account| ListItem::new(account.label.as_str()));
    let mut state = ListState::default().with_selected(Some(selection.saturating_sub(top)));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(" select account "))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn render_browser(frame: &mut Frame<'_>, area: Rect, app: &mut App, regions: &mut UiRegions) {
    if area.width >= 72 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(area);
        render_items(frame, columns[0], app, regions);
        render_detail(frame, columns[1], app, regions);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        render_items(frame, rows[0], app, regions);
        render_detail(frame, rows[1], app, regions);
    }
}

fn render_items(frame: &mut Frame<'_>, area: Rect, app: &mut App, regions: &mut UiRegions) {
    let inner = panel(" items ").inner(area);
    app.item_metrics = ViewportMetrics::new(app.visible.len(), usize::from(inner.height));
    app.item_viewport.ensure_visible(app.selection, app.item_metrics);
    let visible = app.item_viewport.visible_range(app.item_metrics);
    let top = visible.start;
    let items = app.visible[visible.clone()].iter().map(|item| {
        ListItem::new(Line::from(vec![
            Span::styled(&item.title, Style::default().fg(Color::White)),
            Span::styled(format!("  {}", item.vault_name), Style::default().fg(Color::DarkGray)),
        ]))
    });
    let title =
        if matches!(&app.screen, Screen::Search) { " items · searching " } else { " items " };
    let mut state = ListState::default()
        .with_selected((!app.visible.is_empty()).then_some(app.selection.saturating_sub(top)));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("› "),
        area,
        &mut state,
    );
    regions.items = Some(inner);
    regions.item_rows = visible
        .enumerate()
        .map(|(offset, index)| (Rect::new(inner.x, inner.y + offset as u16, inner.width, 1), index))
        .collect();
    regions.scrollbar = ScrollbarLayout::vertical_right(inner, app.item_metrics, top);
    if let Some(scrollbar) = regions.scrollbar {
        render_vertical_scrollbar(
            frame,
            scrollbar,
            app.scrollbar_drag.is_some(),
            ScrollbarStyle {
                track_color: Color::DarkGray,
                thumb_color: Color::Gray,
                active_thumb_color: Color::Cyan,
                track_symbol: "│",
                thumb_symbol: "┃",
            },
        );
    }
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let selected = app.selected_item();
    let (lines, safe_rows) = if let Some(item) =
        selected.filter(|item| app.item_is_open(&item.reference))
    {
        let mut lines = vec![
            Line::styled(
                &item.title,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Line::from(format!("{} · {}", item.category, item.vault_name)),
        ];
        let mut safe_rows = vec![0, 1];
        if item.is_login() {
            lines.push(Line::from(vec![
                Span::styled("username  ", Style::default().fg(Color::DarkGray)),
                Span::styled("u to fetch and copy", Style::default().fg(Color::Cyan)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("password  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "•••••••••••• · y to fetch and copy",
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        }
        for url in &item.urls {
            safe_rows.push(lines.len());
            lines.push(Line::from(vec![
                Span::styled("url       ", Style::default().fg(Color::DarkGray)),
                Span::raw(url),
            ]));
        }
        if !item.tags.is_empty() {
            safe_rows.push(lines.len());
            lines.push(Line::from(vec![
                Span::styled("tags      ", Style::default().fg(Color::DarkGray)),
                Span::raw(item.tags.join(", ")),
            ]));
        }
        (lines, safe_rows)
    } else if app.selected_item().is_some() {
        (
            vec![Line::styled(
                "Press Enter to open item metadata",
                Style::default().fg(Color::DarkGray),
            )],
            Vec::new(),
        )
    } else {
        (vec![Line::styled("No matching items", Style::default().fg(Color::DarkGray))], Vec::new())
    };
    let block = panel(" detail ");
    let inner = block.inner(area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    for row in safe_rows.into_iter().filter(|row| *row < usize::from(inner.height)) {
        regions.selectable.push(SelectableRegion::new(
            SelectionSurface::PublicDetail,
            Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            row as i64,
            0,
            app.detail_revision,
        ));
    }
}

fn render_create(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    form: &CreateForm,
    regions: &mut UiRegions,
) {
    let vault = app.vaults.get(form.vault).map(|vault| vault.name.as_str()).unwrap_or("none");
    let password = form.password.concealed();
    let rows = vec![
        form_line("Vault", vault, form.field() == CreateField::Vault, "←/→"),
        form_line("Title", form.title.value(), form.field() == CreateField::Title, ""),
        form_line("Username", form.username.value(), form.field() == CreateField::Username, ""),
        form_line("URL", form.url.value(), form.field() == CreateField::Url, ""),
        form_line(
            "Password",
            &password,
            form.field() == CreateField::Password,
            if form.password.is_empty() { "empty = generate" } else { "manual" },
        ),
        Line::raw(""),
        Line::styled(
            "Tab/Shift-Tab move · Ctrl-S save · Esc cancel",
            Style::default().fg(Color::DarkGray),
        ),
    ];
    let block = panel(" new Login ");
    let inner = block.inner(area);
    frame.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }).block(block), area);
    regions.create_fields = CreateField::ALL
        .into_iter()
        .take(usize::from(inner.height.min(CreateField::ALL.len() as u16)))
        .enumerate()
        .map(|(index, field)| (Rect::new(inner.x, inner.y + index as u16, inner.width, 1), field))
        .collect();
}

fn form_line<'a>(label: &'a str, value: &'a str, selected: bool, hint: &'a str) -> Line<'a> {
    let marker = if selected { "›" } else { " " };
    Line::from(vec![
        Span::styled(
            format!("{marker} {label:<10}"),
            Style::default().fg(if selected { Color::Cyan } else { Color::DarkGray }),
        ),
        Span::raw(value),
        Span::styled(
            if hint.is_empty() { String::new() } else { format!("  {hint}") },
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn render_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    confirmation: &Confirmation,
    regions: &mut UiRegions,
) {
    let verb = confirmation.action.verb();
    let warning = confirmation.action.warning();
    let title = &confirmation.item.title;
    let vault = &confirmation.item.vault_name;
    let lines = vec![
        Line::styled(
            format!("{verb}: {title}"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Line::from(format!("Vault: {vault}")),
        Line::raw(""),
        Line::from(warning),
        Line::raw(""),
        Line::styled("[ Confirm ]   [ Cancel ]", Style::default().fg(Color::Cyan)),
    ];
    let block = panel(" confirm ");
    let inner = block.inner(area);
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center).wrap(Wrap { trim: false }).block(block),
        area,
    );
    if inner.height > 5 {
        let total_width = 24_u16.min(inner.width);
        let start = inner.x + inner.width.saturating_sub(total_width) / 2;
        if total_width >= 11 {
            regions.actions.push((Rect::new(start, inner.y + 5, 11, 1), actions::ACTIVATE));
        }
        if total_width >= 24 {
            regions.actions.push((Rect::new(start + 14, inner.y + 5, 10, 1), actions::CANCEL));
        }
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let status = if let Some(pending) = app.pending {
        Line::styled(format!("  ⠋ {pending}"), Style::default().fg(Color::Yellow))
    } else if let Some(notice) = app.notice.as_ref() {
        Line::styled(
            format!("  {}", notice.text),
            Style::default().fg(if notice.error { Color::Red } else { Color::Green }),
        )
    } else {
        Line::raw("")
    };
    let controls = [
        ("/ search", actions::SEARCH),
        ("Enter open", actions::ACTIVATE),
        ("u username", actions::COPY_USERNAME),
        ("y password", actions::CONFIRM_OR_COPY_PASSWORD),
        ("n new", actions::CREATE_OR_REJECT),
        ("g rotate", actions::ROTATE_PASSWORD),
        ("d archive", actions::ARCHIVE),
        ("R refresh", actions::REFRESH),
        ("q quit", actions::QUIT),
    ];
    let mut spans = vec![Span::raw("  ")];
    let mut x = area.x.saturating_add(2);
    for (index, (label, action)) in controls.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            x = x.saturating_add(3);
        }
        let width = label.len() as u16;
        spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
        if area.height > 1 && x < area.right() {
            regions.actions.push((
                Rect::new(x, area.y + 1, width.min(area.right().saturating_sub(x)), 1),
                action,
            ));
        }
        x = x.saturating_add(width);
    }
    let help = Line::from(spans);
    frame.render_widget(Paragraph::new(vec![status, help]), area);
}

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
}

#[cfg(test)]
mod tests {
    use super::super::model::{AccountId, ItemId, VaultId};
    use super::*;

    fn account() -> AccountSummary {
        AccountSummary {
            id: AccountId::new("account".to_owned()).unwrap(),
            label: "Personal".to_owned(),
            selectors: vec!["my".to_owned()],
        }
    }

    fn item(id: &str, title: &str) -> ItemSummary {
        ItemSummary {
            reference: ItemRef {
                account_id: account().id,
                vault_id: VaultId::new("vault".to_owned()).unwrap(),
                item_id: ItemId::new(id.to_owned()).unwrap(),
            },
            title: title.to_owned(),
            vault_name: "Private".to_owned(),
            category: "LOGIN".to_owned(),
            tags: Vec::new(),
            urls: Vec::new(),
            additional_information: None,
        }
    }

    #[test]
    fn explicit_account_selector_is_deterministic() {
        let accounts = vec![account()];
        assert_eq!(select_requested_account(&accounts, Some("my")).unwrap(), Some(0));
        assert!(select_requested_account(&accounts, Some("missing")).is_err());
    }

    #[test]
    fn changing_selection_clears_detail() {
        let mut app = App::new(account(), actions::registry().unwrap());
        app.visible = vec![Arc::new(item("one", "One")), Arc::new(item("two", "Two"))];
        app.open_item = Some(app.visible[0].reference.clone());
        app.select_relative(1);
        assert!(app.open_item.is_none());
    }

    #[test]
    fn closing_detail_revokes_pending_field_delivery() {
        let mut app = App::new(account(), actions::registry().unwrap());
        let reference = item("one", "One").reference;
        app.open_item = Some(reference.clone());
        assert!(app.item_is_open(&reference));

        app.clear_open_item();

        assert!(!app.item_is_open(&reference));
    }

    #[test]
    fn pending_operation_blocks_a_second_load() {
        let mut app = App::new(account(), actions::registry().unwrap());
        app.pending = Some("busy");
        let (tx, mut rx): (UnboundedSender<AsyncResult>, mpsc::UnboundedReceiver<AsyncResult>) =
            mpsc::unbounded_channel();
        start_load(&mut app, &OpClient::new(), &tx);
        assert!(rx.try_recv().is_err());
        assert_eq!(app.pending, Some("busy"));
    }
}
