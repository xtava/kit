use std::sync::Arc;

use super::model::{AccountSummary, CreateLoginRequest, ItemRef, ItemSummary, VaultSummary};
use super::op::{LoginField, OpClient, OpError};
use super::sensitive::{SecretBytes, SensitiveInput};
use crate::tui::{EventReader, FuzzyIndex, LineEditor, SearchMode, Session, SessionOptions};
use anyhow::{anyhow, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::mpsc::{self, UnboundedSender};

pub async fn run(
    client: OpClient,
    accounts: Vec<AccountSummary>,
    requested_account: Option<String>,
) -> Result<()> {
    let selected_account = select_requested_account(&accounts, requested_account.as_deref())?;
    let mut session = Session::open(SessionOptions::default())?;
    let mut events = EventReader::start();
    let account = match selected_account {
        Some(index) => accounts.get(index).cloned(),
        None => choose_account(&mut session, &mut events, &accounts).await?,
    };
    let Some(account) = account else { return Ok(()) };
    let mut app = App::new(account);
    let (tx, mut rx) = mpsc::unbounded_channel();

    start_load(&mut app, &client, &tx);

    loop {
        app.update_search();
        session.draw(|frame| render(frame, &app))?;

        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                let Event::Key(key) = event else { continue };
                if key.is_press() && handle_key(key, &mut app, &client, &tx) {
                    break;
                }
            }
            Some(result) = rx.recv() => {
                handle_result(result, &mut app, &client, &tx, &mut session);
            }
        }
    }
    Ok(())
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
) -> Result<Option<AccountSummary>> {
    let mut selection = 0;
    loop {
        session.draw(|frame| render_account_picker(frame, accounts, selection))?;
        let Some(event) = events.recv().await else { return Ok(None) };
        let Event::Key(key) = event else { continue };
        if !key.is_press() {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up => selection = selection.saturating_sub(1),
            KeyCode::Down => selection = (selection + 1).min(accounts.len().saturating_sub(1)),
            KeyCode::Enter => return Ok(accounts.get(selection).cloned()),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
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
}

impl App {
    fn new(account: AccountSummary) -> Self {
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
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    let screen = std::mem::replace(&mut app.screen, Screen::Browse);
    let next = match screen {
        Screen::Browse => handle_browse_key(key, app, client, tx),
        Screen::Search => Some(handle_search_key(key, app)),
        Screen::Create(form) => Some(handle_create_key(key, form, app, client, tx)),
        Screen::Confirm(confirmation) => {
            Some(handle_confirmation_key(key, confirmation, app, client, tx))
        }
    };
    let Some(screen) = next else { return true };
    app.screen = screen;
    false
}

fn handle_browse_key(
    key: KeyEvent,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> Option<Screen> {
    match key.code {
        KeyCode::Char('q') => return None,
        KeyCode::Char('/') => return Some(Screen::Search),
        KeyCode::Up | KeyCode::Char('k') => app.select_relative(-1),
        KeyCode::Down | KeyCode::Char('j') => app.select_relative(1),
        KeyCode::Home => {
            app.selection = 0;
            app.clear_open_item();
        }
        KeyCode::End => {
            app.selection = app.visible.len().saturating_sub(1);
            app.clear_open_item();
        }
        KeyCode::Enter if !app.busy() => {
            app.open_item = app.selected_item().map(|item| item.reference.clone())
        }
        KeyCode::Char('u') if !app.busy() => {
            start_field_copy(app, client, tx, LoginField::Username)
        }
        KeyCode::Char('y') if !app.busy() => {
            start_field_copy(app, client, tx, LoginField::Password)
        }
        KeyCode::Char('n') if !app.busy() && !app.vaults.is_empty() => {
            app.clear_open_item();
            return Some(Screen::Create(CreateForm::new(0)));
        }
        KeyCode::Char('g') if !app.busy() => {
            return Some(begin_confirmation(app, MutationKind::RotatePassword));
        }
        KeyCode::Char('d') if !app.busy() => {
            return Some(begin_confirmation(app, MutationKind::Archive));
        }
        KeyCode::Char('R') if !app.busy() => start_load(app, client, tx),
        KeyCode::Esc => app.open_item = None,
        _ => {}
    }
    Some(Screen::Browse)
}

fn handle_search_key(key: KeyEvent, app: &mut App) -> Screen {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => return Screen::Browse,
        KeyCode::Up => app.select_relative(-1),
        KeyCode::Down => app.select_relative(1),
        _ => {
            app.query.apply_key(key);
            app.clear_open_item();
        }
    }
    Screen::Search
}

fn handle_create_key(
    key: KeyEvent,
    mut form: CreateForm,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> Screen {
    if key.code == KeyCode::Esc {
        return Screen::Browse;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        return submit_create(form, app, client, tx);
    }
    match key.code {
        KeyCode::Tab => form.move_focus(1),
        KeyCode::BackTab => form.move_focus(-1),
        KeyCode::Left if form.field() == CreateField::Vault => {
            form.vault = form.vault.saturating_sub(1)
        }
        KeyCode::Right if form.field() == CreateField::Vault => {
            form.vault = (form.vault + 1).min(app.vaults.len().saturating_sub(1))
        }
        KeyCode::Enter => {
            if form.field() == CreateField::Password {
                return submit_create(form, app, client, tx);
            } else {
                form.move_focus(1);
            }
        }
        _ => match form.field() {
            CreateField::Vault => {}
            CreateField::Title => form.title.apply_key(key),
            CreateField::Username => form.username.apply_key(key),
            CreateField::Url => form.url.apply_key(key),
            CreateField::Password => form.password.apply_key(key),
        },
    }
    Screen::Create(form)
}

fn handle_confirmation_key(
    key: KeyEvent,
    confirmation: Confirmation,
    app: &mut App,
    client: &OpClient,
    tx: &UnboundedSender<AsyncResult>,
) -> Screen {
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter if !app.busy() => {
            start_mutation(app, client, tx, confirmation.action, confirmation.item.reference);
            Screen::Browse
        }
        KeyCode::Char('n') | KeyCode::Esc => Screen::Browse,
        _ => Screen::Confirm(confirmation),
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

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    render_header(frame, rows[0], &app.account.label, app.query.value());
    match &app.screen {
        Screen::Create(form) => render_create(frame, rows[1], app, form),
        Screen::Confirm(confirmation) => render_confirmation(frame, rows[1], confirmation),
        Screen::Browse | Screen::Search => render_browser(frame, rows[1], app),
    }
    render_footer(frame, rows[2], app);
}

fn render_account_picker(frame: &mut Frame<'_>, accounts: &[AccountSummary], selection: usize) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(2)])
        .split(frame.area());
    render_header(frame, rows[0], "select account", "");
    render_accounts(frame, rows[1], accounts, selection);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "  ↑/↓ select · Enter continue · q/Esc quit",
            Style::default().fg(Color::DarkGray),
        )),
        rows[2],
    );
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
) {
    let items = accounts.iter().map(|account| ListItem::new(account.label.as_str()));
    let mut state = ListState::default().with_selected(Some(selection));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(" select account "))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn render_browser(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width >= 72 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(area);
        render_items(frame, columns[0], app);
        render_detail(frame, columns[1], app);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        render_items(frame, rows[0], app);
        render_detail(frame, rows[1], app);
    }
}

fn render_items(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = app.visible.iter().map(|item| {
        ListItem::new(Line::from(vec![
            Span::styled(&item.title, Style::default().fg(Color::White)),
            Span::styled(format!("  {}", item.vault_name), Style::default().fg(Color::DarkGray)),
        ]))
    });
    let title =
        if matches!(&app.screen, Screen::Search) { " items · searching " } else { " items " };
    let mut state =
        ListState::default().with_selected((!app.visible.is_empty()).then_some(app.selection));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let selected = app.selected_item();
    let lines = if let Some(item) = selected.filter(|item| app.item_is_open(&item.reference)) {
        let mut lines = vec![
            Line::styled(
                &item.title,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Line::from(format!("{} · {}", item.category, item.vault_name)),
        ];
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
            lines.push(Line::from(vec![
                Span::styled("url       ", Style::default().fg(Color::DarkGray)),
                Span::raw(url),
            ]));
        }
        if !item.tags.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("tags      ", Style::default().fg(Color::DarkGray)),
                Span::raw(item.tags.join(", ")),
            ]));
        }
        lines
    } else if app.selected_item().is_some() {
        vec![Line::styled(
            "Press Enter to open item metadata",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        vec![Line::styled("No matching items", Style::default().fg(Color::DarkGray))]
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(panel(" detail ")),
        area,
    );
}

fn render_create(frame: &mut Frame<'_>, area: Rect, app: &App, form: &CreateForm) {
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
    frame.render_widget(
        Paragraph::new(rows).wrap(Wrap { trim: false }).block(panel(" new Login ")),
        area,
    );
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

fn render_confirmation(frame: &mut Frame<'_>, area: Rect, confirmation: &Confirmation) {
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
        Line::styled(
            "Press y/Enter to confirm · n/Esc to cancel",
            Style::default().fg(Color::Cyan),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false })
            .block(panel(" confirm ")),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
    let help = Line::styled(
        "  / search · Enter open · u username · y password · n new · g rotate · d archive · R refresh · q quit",
        Style::default().fg(Color::DarkGray),
    );
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
        let mut app = App::new(account());
        app.visible = vec![Arc::new(item("one", "One")), Arc::new(item("two", "Two"))];
        app.open_item = Some(app.visible[0].reference.clone());
        app.select_relative(1);
        assert!(app.open_item.is_none());
    }

    #[test]
    fn closing_detail_revokes_pending_field_delivery() {
        let mut app = App::new(account());
        let reference = item("one", "One").reference;
        app.open_item = Some(reference.clone());
        assert!(app.item_is_open(&reference));

        app.clear_open_item();

        assert!(!app.item_is_open(&reference));
    }

    #[test]
    fn pending_operation_blocks_a_second_load() {
        let mut app = App::new(account());
        app.pending = Some("busy");
        let (tx, mut rx): (UnboundedSender<AsyncResult>, mpsc::UnboundedReceiver<AsyncResult>) =
            mpsc::unbounded_channel();
        start_load(&mut app, &OpClient::new(), &tx);
        assert!(rx.try_recv().is_err());
        assert_eq!(app.pending, Some("busy"));
    }
}
