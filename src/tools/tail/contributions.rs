use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement, MenuId, MenuPlacement,
};

pub(super) const FOCUS_COMPOSER: ActionId = ActionId::new("tail.share.focusComposer");
pub(super) const CHOOSE_FILES: ActionId = ActionId::new("tail.share.chooseFiles");
pub(super) const SEND_TEXT: ActionId = ActionId::new("tail.share.sendText");
pub(super) const SEND_FILES: ActionId = ActionId::new("tail.share.sendFiles");
pub(super) const CLEAR_COMPOSER: ActionId = ActionId::new("tail.share.clearComposer");
pub(super) const RETARGET_COMPOSER: ActionId = ActionId::new("tail.share.retargetComposer");
pub(super) const RETRY_FAILED: ActionId = ActionId::new("tail.share.retryFailed");
pub(super) const INSPECT: ActionId = ActionId::new("tail.item.inspect");
pub(super) const COPY: ActionId = ActionId::new("tail.item.copy");
pub(super) const SAVE: ActionId = ActionId::new("tail.item.save");
pub(super) const OPEN: ActionId = ActionId::new("tail.item.open");
pub(super) const DELETE: ActionId = ActionId::new("tail.item.delete");
pub(super) const SEARCH: ActionId = ActionId::new("tail.scope.search");
pub(super) const TOGGLE_RECEIVING: ActionId = ActionId::new("tail.receive.toggle");
pub(super) const OPEN_LOGIN: ActionId = ActionId::new("tail.auth.open");
pub(super) const COPY_LOGIN: ActionId = ActionId::new("tail.auth.copy");
pub(super) const RETRY_LOGIN: ActionId = ActionId::new("tail.auth.retry");
pub(super) const BACK: ActionId = ActionId::new("tail.navigation.back");
pub(super) const USE_FILES: ActionId = ActionId::new("tail.ambiguous.useFiles");
pub(super) const USE_TEXT: ActionId = ActionId::new("tail.ambiguous.useText");
pub(super) const CONFIRM_DELETE: ActionId = ActionId::new("tail.item.confirmDelete");
pub(super) const OPEN_ENTRY: ActionId = ActionId::new("tail.browser.openEntry");
pub(super) const TOGGLE_FILE: ActionId = ActionId::new("tail.browser.toggleFile");
pub(super) const REVIEW_SELECTED: ActionId = ActionId::new("tail.browser.reviewSelected");
pub(super) const PARENT_DIRECTORY: ActionId = ActionId::new("tail.browser.parentDirectory");
pub(super) const SAVE_HERE: ActionId = ActionId::new("tail.save.here");
pub(super) const KEEP_BOTH: ActionId = ActionId::new("tail.save.keepBoth");
pub(super) const REPLACE: ActionId = ActionId::new("tail.save.replace");
pub(super) const CANCEL_OPERATION: ActionId = ActionId::new("tail.operation.cancel");
pub(super) const RESUME_RECEIVING: ActionId = ActionId::new("tail.receive.resume");
pub(super) const CANCEL_LOGIN: ActionId = ActionId::new("tail.auth.cancel");
pub(super) const CONFIRM_QUIT: ActionId = ActionId::new("tail.session.confirmQuit");
pub(super) const QUIT: ActionId = ActionId::new("tail.session.quit");

pub(super) const DEVICE_CONTEXT: MenuId = MenuId::new("tail.device.context");
pub(super) const ITEM_CONTEXT: MenuId = MenuId::new("tail.item.context");
pub(super) const WORKSPACE_INLINE: MenuId = MenuId::new("tail.workspace.inline");
pub(super) const AUTH_INLINE: MenuId = MenuId::new("tail.auth.inline");
pub(super) const MODAL_INLINE: MenuId = MenuId::new("tail.modal.inline");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TailSurface {
    Workspace,
    ReviewFiles { can_insert_text: bool },
    Ambiguous,
    Search,
    Detail,
    ConfirmDelete,
    ConfirmQuit,
    FileBrowser { selected_files: bool },
    SaveBrowser,
    SaveConflict,
    Auth,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TailActionTarget {
    Device(String),
    Item { id: String, text: bool },
    Auth,
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TailActionContext {
    pub surface: TailSurface,
    pub target: TailActionTarget,
    pub receiving: bool,
    pub has_message: bool,
    pub can_retarget_message: bool,
    pub failed_sends: usize,
    pub active_send: bool,
    pub login_url: bool,
    pub can_retry_login: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TailCommand {
    FocusComposer,
    ChooseFiles,
    SendText,
    SendFiles,
    ClearComposer,
    RetargetComposer,
    RetryFailed,
    Inspect,
    Copy,
    Save,
    Open,
    Delete,
    Search,
    ToggleReceiving,
    OpenLogin,
    CopyLogin,
    RetryLogin,
    Back,
    UseFiles,
    UseText,
    ConfirmDelete,
    OpenEntry,
    ToggleFile,
    ReviewSelected,
    ParentDirectory,
    SaveHere,
    KeepBoth,
    Replace,
    CancelOperation,
    ResumeReceiving,
    CancelLogin,
    ConfirmQuit,
    Quit,
}

pub(super) type TailActionRegistry = ActionRegistry<TailActionContext, TailCommand>;

pub(super) fn registry() -> Result<TailActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    contribute_actions(&mut builder);
    builder.build()
}

fn contribute_actions(builder: &mut ActionRegistryBuilder<TailActionContext, TailCommand>) {
    for (id, title, command, enablement) in [
        (
            FOCUS_COMPOSER,
            "Write message",
            TailCommand::FocusComposer,
            device as fn(&TailActionContext) -> ActionState,
        ),
        (CHOOSE_FILES, "Choose files", TailCommand::ChooseFiles, device),
        (SEND_TEXT, "Send", TailCommand::SendText, message),
        (SEND_FILES, "Send files", TailCommand::SendFiles, reviewed_files),
        (CLEAR_COMPOSER, "Clear message", TailCommand::ClearComposer, message),
        (RETARGET_COMPOSER, "Move message here", TailCommand::RetargetComposer, retarget_message),
        (RETRY_FAILED, "Retry failed", TailCommand::RetryFailed, failed_sends),
        (INSPECT, "View details", TailCommand::Inspect, item),
        (COPY, "Copy to clipboard", TailCommand::Copy, text_item),
        (SAVE, "Save as...", TailCommand::Save, item),
        (OPEN, "Open", TailCommand::Open, item),
        (DELETE, "Delete from cache...", TailCommand::Delete, item),
        (SEARCH, "Search", TailCommand::Search, always),
        (TOGGLE_RECEIVING, "Pause or resume receiving", TailCommand::ToggleReceiving, always),
        (OPEN_LOGIN, "Open login link", TailCommand::OpenLogin, login_url),
        (COPY_LOGIN, "Copy login link", TailCommand::CopyLogin, login_url),
        (RETRY_LOGIN, "Retry connection", TailCommand::RetryLogin, retry_login),
        (BACK, "Back", TailCommand::Back, always),
        (USE_FILES, "Use files", TailCommand::UseFiles, always),
        (USE_TEXT, "Insert as text", TailCommand::UseText, always),
        (CONFIRM_DELETE, "Delete", TailCommand::ConfirmDelete, item),
        (OPEN_ENTRY, "Open selected", TailCommand::OpenEntry, always),
        (TOGGLE_FILE, "Select file", TailCommand::ToggleFile, always),
        (REVIEW_SELECTED, "Review selected", TailCommand::ReviewSelected, selected_files),
        (PARENT_DIRECTORY, "Parent folder", TailCommand::ParentDirectory, always),
        (SAVE_HERE, "Save here", TailCommand::SaveHere, always),
        (KEEP_BOTH, "Keep both", TailCommand::KeepBoth, always),
        (REPLACE, "Replace", TailCommand::Replace, always),
        (CANCEL_OPERATION, "Cancel send", TailCommand::CancelOperation, active_send),
        (RESUME_RECEIVING, "Resume receiving", TailCommand::ResumeReceiving, always),
        (CANCEL_LOGIN, "Cancel login", TailCommand::CancelLogin, always),
        (CONFIRM_QUIT, "Quit now", TailCommand::ConfirmQuit, always),
        (QUIT, "Quit", TailCommand::Quit, always),
    ] {
        builder.register_action(ActionSpec {
            id,
            title,
            command,
            enablement,
            command_palette: CommandPalettePlacement::Hidden,
        });
    }

    for (menu, action, group, group_order, order, when) in [
        (
            DEVICE_CONTEXT,
            FOCUS_COMPOSER,
            "share",
            10,
            10,
            device_visible as fn(&TailActionContext) -> bool,
        ),
        (DEVICE_CONTEXT, CHOOSE_FILES, "share", 10, 20, device_visible),
        (ITEM_CONTEXT, INSPECT, "inspect", 10, 10, item_visible),
        (ITEM_CONTEXT, COPY, "consume", 20, 10, text_item_visible),
        (ITEM_CONTEXT, SAVE, "consume", 20, 20, item_visible),
        (ITEM_CONTEXT, OPEN, "consume", 20, 30, item_visible),
        (ITEM_CONTEXT, DELETE, "destructive", 30, 10, item_visible),
        (WORKSPACE_INLINE, SEND_TEXT, "message", 10, 10, workspace_visible),
        (WORKSPACE_INLINE, FOCUS_COMPOSER, "message", 10, 20, device_visible),
        (WORKSPACE_INLINE, CHOOSE_FILES, "message", 10, 30, device_visible),
        (WORKSPACE_INLINE, RETARGET_COMPOSER, "message", 10, 40, can_retarget_visible),
        (WORKSPACE_INLINE, CLEAR_COMPOSER, "message", 10, 50, has_message_visible),
        (WORKSPACE_INLINE, CANCEL_OPERATION, "transfers", 20, 10, active_send_visible),
        (WORKSPACE_INLINE, RETRY_FAILED, "transfers", 20, 20, failed_sends_visible),
        (WORKSPACE_INLINE, INSPECT, "item", 30, 10, item_visible),
        (WORKSPACE_INLINE, COPY, "item", 30, 20, text_item_visible),
        (WORKSPACE_INLINE, SAVE, "item", 30, 30, item_visible),
        (WORKSPACE_INLINE, SEARCH, "scope", 40, 10, workspace_visible),
        (WORKSPACE_INLINE, TOGGLE_RECEIVING, "scope", 40, 20, workspace_visible),
        (AUTH_INLINE, OPEN_LOGIN, "auth", 10, 10, login_url_visible),
        (AUTH_INLINE, COPY_LOGIN, "auth", 10, 20, login_url_visible),
        (AUTH_INLINE, RETRY_LOGIN, "auth", 10, 30, retry_visible),
        (AUTH_INLINE, CANCEL_LOGIN, "navigation", 20, 10, auth_visible),
        (MODAL_INLINE, SEND_FILES, "primary", 10, 10, review_files),
        (MODAL_INLINE, USE_TEXT, "primary", 10, 20, text_choice),
        (MODAL_INLINE, USE_FILES, "primary", 10, 10, ambiguous),
        (MODAL_INLINE, COPY, "primary", 10, 10, detail),
        (MODAL_INLINE, CONFIRM_DELETE, "destructive", 20, 10, confirm_delete),
        (MODAL_INLINE, OPEN_ENTRY, "primary", 10, 10, file_or_save_browser),
        (MODAL_INLINE, TOGGLE_FILE, "primary", 10, 20, file_browser),
        (MODAL_INLINE, REVIEW_SELECTED, "primary", 10, 30, file_browser),
        (MODAL_INLINE, PARENT_DIRECTORY, "navigation", 20, 10, file_or_save_browser),
        (MODAL_INLINE, SAVE_HERE, "primary", 10, 20, save_browser),
        (MODAL_INLINE, KEEP_BOTH, "primary", 10, 10, save_conflict),
        (MODAL_INLINE, REPLACE, "destructive", 20, 10, save_conflict),
        (MODAL_INLINE, CONFIRM_QUIT, "destructive", 20, 10, confirm_quit),
        (MODAL_INLINE, CANCEL_OPERATION, "transfers", 15, 10, active_send_anywhere),
        (MODAL_INLINE, RETRY_FAILED, "transfers", 15, 20, failed_sends_anywhere),
        (MODAL_INLINE, BACK, "navigation", 20, 20, modal_back),
    ] {
        builder.place_menu(MenuPlacement { menu, action, group, group_order, order, when });
    }

    for (code, modifiers, action, when) in [
        (
            KeyCode::Char('p'),
            KeyModifiers::NONE,
            FOCUS_COMPOSER,
            device_visible as fn(&TailActionContext) -> bool,
        ),
        (KeyCode::Char('f'), KeyModifiers::NONE, CHOOSE_FILES, device_visible),
        (KeyCode::Char('f'), KeyModifiers::CONTROL, CHOOSE_FILES, workspace_visible),
        (KeyCode::Enter, KeyModifiers::NONE, INSPECT, item_visible),
        (KeyCode::Char('c'), KeyModifiers::NONE, COPY, text_item_visible),
        (KeyCode::Char('s'), KeyModifiers::NONE, SAVE, item_visible),
        (KeyCode::Char('o'), KeyModifiers::NONE, OPEN, item_visible),
        (KeyCode::Char('d'), KeyModifiers::NONE, DELETE, item_visible),
        (KeyCode::Char('/'), KeyModifiers::NONE, SEARCH, workspace_visible),
        (KeyCode::Char('w'), KeyModifiers::NONE, TOGGLE_RECEIVING, workspace_visible),
        (KeyCode::Char('r'), KeyModifiers::NONE, RESUME_RECEIVING, workspace_visible),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, modifiers).into(),
            action,
            when,
        });
    }
}

fn always(_: &TailActionContext) -> ActionState {
    ActionState::Enabled
}

fn device(context: &TailActionContext) -> ActionState {
    if matches!(context.target, TailActionTarget::Device(_)) {
        ActionState::Enabled
    } else {
        ActionState::disabled("select a device that supports Taildrop")
    }
}

fn item(context: &TailActionContext) -> ActionState {
    if matches!(context.target, TailActionTarget::Item { .. }) {
        ActionState::Enabled
    } else {
        ActionState::disabled("select a received item")
    }
}

fn text_item(context: &TailActionContext) -> ActionState {
    if matches!(context.target, TailActionTarget::Item { text: true, .. }) {
        ActionState::Enabled
    } else {
        ActionState::disabled("clipboard copy is available for text items")
    }
}

fn message(context: &TailActionContext) -> ActionState {
    if context.has_message {
        ActionState::Enabled
    } else {
        ActionState::disabled("type a message first")
    }
}

fn retarget_message(context: &TailActionContext) -> ActionState {
    if context.can_retarget_message {
        ActionState::Enabled
    } else {
        ActionState::disabled("the message already targets this device")
    }
}

fn failed_sends(context: &TailActionContext) -> ActionState {
    if context.failed_sends > 0 {
        ActionState::Enabled
    } else {
        ActionState::disabled("there are no failed sends")
    }
}

fn active_send(context: &TailActionContext) -> ActionState {
    if context.active_send {
        ActionState::Enabled
    } else {
        ActionState::disabled("there is no active send")
    }
}

fn reviewed_files(context: &TailActionContext) -> ActionState {
    if matches!(context.surface, TailSurface::ReviewFiles { .. }) {
        ActionState::Enabled
    } else {
        ActionState::disabled("choose files first")
    }
}

fn login_url(context: &TailActionContext) -> ActionState {
    if context.login_url {
        ActionState::Enabled
    } else {
        ActionState::disabled("waiting for the login link")
    }
}

fn retry_login(context: &TailActionContext) -> ActionState {
    if context.can_retry_login {
        ActionState::Enabled
    } else {
        ActionState::disabled("login is already in progress")
    }
}

fn selected_files(context: &TailActionContext) -> ActionState {
    if matches!(context.surface, TailSurface::FileBrowser { selected_files: true }) {
        ActionState::Enabled
    } else {
        ActionState::disabled("select at least one file")
    }
}

fn device_visible(context: &TailActionContext) -> bool {
    matches!(context.target, TailActionTarget::Device(_))
}

fn item_visible(context: &TailActionContext) -> bool {
    matches!(context.target, TailActionTarget::Item { .. })
}

fn text_item_visible(context: &TailActionContext) -> bool {
    matches!(context.target, TailActionTarget::Item { text: true, .. })
}

fn workspace_visible(context: &TailActionContext) -> bool {
    context.surface == TailSurface::Workspace
}

fn has_message_visible(context: &TailActionContext) -> bool {
    workspace_visible(context) && context.has_message
}

fn can_retarget_visible(context: &TailActionContext) -> bool {
    workspace_visible(context) && context.can_retarget_message
}

fn failed_sends_visible(context: &TailActionContext) -> bool {
    workspace_visible(context) && context.failed_sends > 0
}

fn active_send_visible(context: &TailActionContext) -> bool {
    workspace_visible(context) && context.active_send
}

fn active_send_anywhere(context: &TailActionContext) -> bool {
    context.surface != TailSurface::Auth && context.active_send
}

fn failed_sends_anywhere(context: &TailActionContext) -> bool {
    context.surface != TailSurface::Auth && context.failed_sends > 0
}

fn login_url_visible(context: &TailActionContext) -> bool {
    matches!(context.target, TailActionTarget::Auth) && context.login_url
}

fn retry_visible(context: &TailActionContext) -> bool {
    matches!(context.target, TailActionTarget::Auth) && context.can_retry_login
}

fn auth_visible(context: &TailActionContext) -> bool {
    context.surface == TailSurface::Auth
}

fn review_files(context: &TailActionContext) -> bool {
    matches!(context.surface, TailSurface::ReviewFiles { .. })
}

fn text_choice(context: &TailActionContext) -> bool {
    matches!(
        context.surface,
        TailSurface::ReviewFiles { can_insert_text: true } | TailSurface::Ambiguous
    )
}

fn ambiguous(context: &TailActionContext) -> bool {
    context.surface == TailSurface::Ambiguous
}

fn detail(context: &TailActionContext) -> bool {
    context.surface == TailSurface::Detail
}

fn confirm_delete(context: &TailActionContext) -> bool {
    context.surface == TailSurface::ConfirmDelete
}

fn confirm_quit(context: &TailActionContext) -> bool {
    context.surface == TailSurface::ConfirmQuit
}

fn file_browser(context: &TailActionContext) -> bool {
    matches!(context.surface, TailSurface::FileBrowser { .. })
}

fn save_browser(context: &TailActionContext) -> bool {
    context.surface == TailSurface::SaveBrowser
}

fn file_or_save_browser(context: &TailActionContext) -> bool {
    file_browser(context) || save_browser(context)
}

fn save_conflict(context: &TailActionContext) -> bool {
    context.surface == TailSurface::SaveConflict
}

fn modal_back(context: &TailActionContext) -> bool {
    !matches!(context.surface, TailSurface::Workspace | TailSurface::Auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(target: TailActionTarget) -> TailActionContext {
        TailActionContext {
            surface: TailSurface::Workspace,
            target,
            receiving: true,
            has_message: false,
            can_retarget_message: false,
            failed_sends: 0,
            active_send: false,
            login_url: false,
            can_retry_login: false,
        }
    }

    #[test]
    fn item_context_exposes_text_actions() {
        let registry = registry().unwrap();
        let menu = registry.resolve_menu(
            ITEM_CONTEXT,
            &context(TailActionTarget::Item { id: "one".into(), text: true }),
        );
        let ids = menu.items().iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![INSPECT, COPY, SAVE, OPEN, DELETE]);
    }

    #[test]
    fn device_context_exposes_the_persistent_share_actions() {
        let registry = registry().unwrap();
        let menu = registry
            .resolve_menu(DEVICE_CONTEXT, &context(TailActionTarget::Device("peer".into())));
        let ids = menu.items().iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![FOCUS_COMPOSER, CHOOSE_FILES]);
    }
}
