use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SecretsActionMode {
    AccountPicker,
    Browse,
    Search,
    Create { vault_field: bool },
    Confirm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SecretsActionContext {
    pub mode: SecretsActionMode,
    pub busy: bool,
    pub has_item: bool,
    pub selected_login: bool,
    pub has_vaults: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SecretsCommand {
    Quit,
    Cancel,
    Previous,
    Next,
    PageUp,
    PageDown,
    First,
    Last,
    Activate,
    Search,
    CopyUsername,
    ConfirmOrCopyPassword,
    CreateOrReject,
    RotatePassword,
    Archive,
    Refresh,
    NextField,
    PreviousField,
    PreviousVault,
    NextVault,
    Save,
}

pub(super) type SecretsActionRegistry = ActionRegistry<SecretsActionContext, SecretsCommand>;

pub(super) const QUIT: ActionId = ActionId::new("secrets.quit");
pub(super) const CANCEL: ActionId = ActionId::new("secrets.cancel");
pub(super) const ACTIVATE: ActionId = ActionId::new("secrets.activate");
pub(super) const SEARCH: ActionId = ActionId::new("secrets.search");
pub(super) const COPY_USERNAME: ActionId = ActionId::new("secrets.copyUsername");
pub(super) const CONFIRM_OR_COPY_PASSWORD: ActionId =
    ActionId::new("secrets.confirmOrCopyPassword");
pub(super) const CREATE_OR_REJECT: ActionId = ActionId::new("secrets.createOrReject");
pub(super) const ROTATE_PASSWORD: ActionId = ActionId::new("secrets.rotatePassword");
pub(super) const ARCHIVE: ActionId = ActionId::new("secrets.archive");
pub(super) const REFRESH: ActionId = ActionId::new("secrets.refresh");

pub(super) fn registry() -> Result<SecretsActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    for (order, (id, title, command, enablement)) in [
        action("secrets.quit", "Quit", SecretsCommand::Quit, enabled),
        action("secrets.cancel", "Cancel", SecretsCommand::Cancel, enabled),
        action("secrets.previous", "Move up", SecretsCommand::Previous, enabled),
        action("secrets.next", "Move down", SecretsCommand::Next, enabled),
        action("secrets.pageUp", "Page up", SecretsCommand::PageUp, has_item),
        action("secrets.pageDown", "Page down", SecretsCommand::PageDown, has_item),
        action("secrets.first", "First item", SecretsCommand::First, has_item),
        action("secrets.last", "Last item", SecretsCommand::Last, has_item),
        action("secrets.activate", "Open or accept", SecretsCommand::Activate, can_activate),
        action("secrets.search", "Search", SecretsCommand::Search, browse),
        action(
            "secrets.copyUsername",
            "Copy username",
            SecretsCommand::CopyUsername,
            can_copy_login,
        ),
        action(
            "secrets.confirmOrCopyPassword",
            "Confirm or copy password",
            SecretsCommand::ConfirmOrCopyPassword,
            can_confirm_or_copy_password,
        ),
        action(
            "secrets.createOrReject",
            "Create login or reject confirmation",
            SecretsCommand::CreateOrReject,
            can_create_or_reject,
        ),
        action(
            "secrets.rotatePassword",
            "Rotate password…",
            SecretsCommand::RotatePassword,
            can_mutate,
        ),
        action("secrets.archive", "Archive…", SecretsCommand::Archive, can_mutate),
        action("secrets.refresh", "Refresh", SecretsCommand::Refresh, browse_idle),
        action("secrets.nextField", "Next field", SecretsCommand::NextField, create),
        action("secrets.previousField", "Previous field", SecretsCommand::PreviousField, create),
        action(
            "secrets.previousVault",
            "Previous vault",
            SecretsCommand::PreviousVault,
            create_vault,
        ),
        action("secrets.nextVault", "Next vault", SecretsCommand::NextVault, create_vault),
        action("secrets.save", "Save login", SecretsCommand::Save, create_idle),
    ]
    .into_iter()
    .enumerate()
    {
        builder.register_action(ActionSpec {
            id: ActionId::new(id),
            title,
            command,
            enablement,
            command_palette: CommandPalettePlacement::Visible {
                group: "Secrets",
                group_order: 10,
                order: order as i16,
            },
        });
    }

    bind(&mut builder, KeyCode::Char('q'), KeyModifiers::NONE, "secrets.quit", account_or_browse);
    bind(&mut builder, KeyCode::Char('c'), KeyModifiers::CONTROL, "secrets.quit", always);
    bind(&mut builder, KeyCode::Esc, KeyModifiers::NONE, "secrets.cancel", always);
    bind(&mut builder, KeyCode::Up, KeyModifiers::NONE, "secrets.previous", navigable);
    bind(&mut builder, KeyCode::Char('k'), KeyModifiers::NONE, "secrets.previous", browse_only);
    bind(&mut builder, KeyCode::Down, KeyModifiers::NONE, "secrets.next", navigable);
    bind(&mut builder, KeyCode::Char('j'), KeyModifiers::NONE, "secrets.next", browse_only);
    bind(&mut builder, KeyCode::PageUp, KeyModifiers::NONE, "secrets.pageUp", browse_only);
    bind(&mut builder, KeyCode::PageDown, KeyModifiers::NONE, "secrets.pageDown", browse_only);
    bind(&mut builder, KeyCode::Home, KeyModifiers::NONE, "secrets.first", browse_only);
    bind(&mut builder, KeyCode::End, KeyModifiers::NONE, "secrets.last", browse_only);
    bind(&mut builder, KeyCode::Enter, KeyModifiers::NONE, "secrets.activate", always);
    bind(&mut builder, KeyCode::Char('/'), KeyModifiers::NONE, "secrets.search", browse_only);
    bind(&mut builder, KeyCode::Char('u'), KeyModifiers::NONE, "secrets.copyUsername", browse_only);
    bind(
        &mut builder,
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        "secrets.confirmOrCopyPassword",
        browse_or_confirm,
    );
    bind(
        &mut builder,
        KeyCode::Char('n'),
        KeyModifiers::NONE,
        "secrets.createOrReject",
        browse_or_confirm,
    );
    bind(
        &mut builder,
        KeyCode::Char('g'),
        KeyModifiers::NONE,
        "secrets.rotatePassword",
        browse_only,
    );
    bind(&mut builder, KeyCode::Char('d'), KeyModifiers::NONE, "secrets.archive", browse_only);
    bind(&mut builder, KeyCode::Char('R'), KeyModifiers::SHIFT, "secrets.refresh", browse_only);
    bind(&mut builder, KeyCode::Tab, KeyModifiers::NONE, "secrets.nextField", create_only);
    bind(&mut builder, KeyCode::BackTab, KeyModifiers::SHIFT, "secrets.previousField", create_only);
    bind(
        &mut builder,
        KeyCode::Left,
        KeyModifiers::NONE,
        "secrets.previousVault",
        create_vault_only,
    );
    bind(&mut builder, KeyCode::Right, KeyModifiers::NONE, "secrets.nextVault", create_vault_only);
    bind(&mut builder, KeyCode::Char('s'), KeyModifiers::CONTROL, "secrets.save", create_only);
    builder.build()
}

type Enablement = fn(&SecretsActionContext) -> ActionState;

fn action(
    id: &'static str,
    title: &'static str,
    command: SecretsCommand,
    enablement: Enablement,
) -> (&'static str, &'static str, SecretsCommand, Enablement) {
    (id, title, command, enablement)
}

fn bind(
    builder: &mut ActionRegistryBuilder<SecretsActionContext, SecretsCommand>,
    code: KeyCode,
    modifiers: KeyModifiers,
    action: &'static str,
    when: fn(&SecretsActionContext) -> bool,
) {
    builder.bind_key(KeybindingPlacement {
        binding: KeyChord::new(code, modifiers).into(),
        action: ActionId::new(action),
        when,
    });
}

fn enabled(_: &SecretsActionContext) -> ActionState {
    ActionState::Enabled
}

fn disabled(reason: &'static str) -> ActionState {
    ActionState::disabled(reason)
}

fn browse(context: &SecretsActionContext) -> ActionState {
    if context.mode == SecretsActionMode::Browse {
        enabled(context)
    } else {
        disabled("not browsing")
    }
}

fn has_item(context: &SecretsActionContext) -> ActionState {
    if context.has_item {
        enabled(context)
    } else {
        disabled("no selected item")
    }
}

fn can_activate(context: &SecretsActionContext) -> ActionState {
    match context.mode {
        SecretsActionMode::Browse | SecretsActionMode::Confirm if context.busy => {
            disabled("operation in progress")
        }
        SecretsActionMode::Browse if !context.has_item => disabled("no selected item"),
        _ => enabled(context),
    }
}

fn can_copy_login(context: &SecretsActionContext) -> ActionState {
    if !context.busy && context.has_item && context.selected_login {
        enabled(context)
    } else {
        disabled("selected item is not an available login")
    }
}

fn can_confirm_or_copy_password(context: &SecretsActionContext) -> ActionState {
    if context.mode == SecretsActionMode::Confirm && !context.busy {
        enabled(context)
    } else {
        can_copy_login(context)
    }
}

fn can_create_or_reject(context: &SecretsActionContext) -> ActionState {
    if context.mode == SecretsActionMode::Confirm {
        enabled(context)
    } else if context.mode == SecretsActionMode::Browse && !context.busy && context.has_vaults {
        enabled(context)
    } else {
        disabled("no writable vault is available")
    }
}

fn can_mutate(context: &SecretsActionContext) -> ActionState {
    if context.mode == SecretsActionMode::Browse && !context.busy && context.has_item {
        enabled(context)
    } else {
        disabled("no available item is selected")
    }
}

fn browse_idle(context: &SecretsActionContext) -> ActionState {
    if context.mode == SecretsActionMode::Browse && !context.busy {
        enabled(context)
    } else {
        disabled("operation in progress")
    }
}

fn create(context: &SecretsActionContext) -> ActionState {
    if matches!(context.mode, SecretsActionMode::Create { .. }) {
        enabled(context)
    } else {
        disabled("new-login form is closed")
    }
}

fn create_idle(context: &SecretsActionContext) -> ActionState {
    if matches!(context.mode, SecretsActionMode::Create { .. }) && !context.busy {
        enabled(context)
    } else {
        disabled("new-login form is unavailable")
    }
}

fn create_vault(context: &SecretsActionContext) -> ActionState {
    if matches!(context.mode, SecretsActionMode::Create { vault_field: true }) {
        enabled(context)
    } else {
        disabled("vault field is not focused")
    }
}

fn always(_: &SecretsActionContext) -> bool {
    true
}

fn account_or_browse(context: &SecretsActionContext) -> bool {
    matches!(context.mode, SecretsActionMode::AccountPicker | SecretsActionMode::Browse)
}

fn navigable(context: &SecretsActionContext) -> bool {
    matches!(
        context.mode,
        SecretsActionMode::AccountPicker | SecretsActionMode::Browse | SecretsActionMode::Search
    )
}

fn browse_only(context: &SecretsActionContext) -> bool {
    context.mode == SecretsActionMode::Browse
}

fn browse_or_confirm(context: &SecretsActionContext) -> bool {
    matches!(context.mode, SecretsActionMode::Browse | SecretsActionMode::Confirm)
}

fn create_only(context: &SecretsActionContext) -> bool {
    matches!(context.mode, SecretsActionMode::Create { .. })
}

fn create_vault_only(context: &SecretsActionContext) -> bool {
    matches!(context.mode, SecretsActionMode::Create { vault_field: true })
}
