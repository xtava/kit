use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    KeyChord, KeybindingPlacement, MenuId, MenuPlacement,
};

pub(super) const COMPOSE: ActionId = ActionId::new("tail.share.compose");
pub(super) const CHOOSE_FILES: ActionId = ActionId::new("tail.share.chooseFiles");
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
pub(super) const NEXT_DEVICE: ActionId = ActionId::new("tail.share.nextDevice");
pub(super) const REVIEW_DRAFT: ActionId = ActionId::new("tail.share.review");
pub(super) const SEND: ActionId = ActionId::new("tail.share.send");
pub(super) const BACK: ActionId = ActionId::new("tail.navigation.back");
pub(super) const USE_FILES: ActionId = ActionId::new("tail.ambiguous.useFiles");
pub(super) const USE_TEXT: ActionId = ActionId::new("tail.ambiguous.useText");
pub(super) const CONFIRM_DELETE: ActionId = ActionId::new("tail.item.confirmDelete");
pub(super) const OPEN_ENTRY: ActionId = ActionId::new("tail.browser.openEntry");
pub(super) const TOGGLE_FILE: ActionId = ActionId::new("tail.browser.toggleFile");
pub(super) const SEND_SELECTED: ActionId = ActionId::new("tail.browser.sendSelected");
pub(super) const PARENT_DIRECTORY: ActionId = ActionId::new("tail.browser.parentDirectory");
pub(super) const SAVE_HERE: ActionId = ActionId::new("tail.save.here");
pub(super) const KEEP_BOTH: ActionId = ActionId::new("tail.save.keepBoth");
pub(super) const REPLACE: ActionId = ActionId::new("tail.save.replace");
pub(super) const CANCEL_OPERATION: ActionId = ActionId::new("tail.operation.cancel");
pub(super) const RESUME_RECEIVING: ActionId = ActionId::new("tail.receive.resume");
pub(super) const CANCEL_LOGIN: ActionId = ActionId::new("tail.auth.cancel");

pub(super) const DEVICE_CONTEXT: MenuId = MenuId::new("tail.device.context");
pub(super) const ITEM_CONTEXT: MenuId = MenuId::new("tail.item.context");
pub(super) const BROWSE_INLINE: MenuId = MenuId::new("tail.browse.inline");
pub(super) const AUTH_INLINE: MenuId = MenuId::new("tail.auth.inline");
pub(super) const MODAL_INLINE: MenuId = MenuId::new("tail.modal.inline");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TailSurface {
    Browse,
    Compose,
    Review,
    Ambiguous,
    Search,
    Detail,
    ConfirmDelete,
    FileBrowser { selected_files: bool },
    SaveBrowser,
    SaveConflict,
    Auth,
    Busy,
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
    pub has_draft: bool,
    pub login_url: bool,
    pub can_retry_login: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TailCommand {
    Compose,
    ChooseFiles,
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
    NextDevice,
    ReviewDraft,
    Send,
    Back,
    UseFiles,
    UseText,
    ConfirmDelete,
    OpenEntry,
    ToggleFile,
    SendSelected,
    ParentDirectory,
    SaveHere,
    KeepBoth,
    Replace,
    CancelOperation,
    ResumeReceiving,
    CancelLogin,
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
            COMPOSE,
            "Share text",
            TailCommand::Compose,
            device as fn(&TailActionContext) -> ActionState,
        ),
        (CHOOSE_FILES, "Share files", TailCommand::ChooseFiles, device),
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
        (NEXT_DEVICE, "Next device", TailCommand::NextDevice, always),
        (REVIEW_DRAFT, "Review", TailCommand::ReviewDraft, draft),
        (SEND, "Send", TailCommand::Send, draft),
        (BACK, "Back", TailCommand::Back, always),
        (USE_FILES, "Use files", TailCommand::UseFiles, always),
        (USE_TEXT, "Use as text", TailCommand::UseText, always),
        (CONFIRM_DELETE, "Delete", TailCommand::ConfirmDelete, item),
        (OPEN_ENTRY, "Open selected", TailCommand::OpenEntry, always),
        (TOGGLE_FILE, "Select file", TailCommand::ToggleFile, always),
        (SEND_SELECTED, "Review selected", TailCommand::SendSelected, selected_files),
        (PARENT_DIRECTORY, "Parent folder", TailCommand::ParentDirectory, always),
        (SAVE_HERE, "Save here", TailCommand::SaveHere, always),
        (KEEP_BOTH, "Keep both", TailCommand::KeepBoth, always),
        (REPLACE, "Replace", TailCommand::Replace, always),
        (CANCEL_OPERATION, "Cancel", TailCommand::CancelOperation, always),
        (RESUME_RECEIVING, "Resume receiving", TailCommand::ResumeReceiving, always),
        (CANCEL_LOGIN, "Cancel login", TailCommand::CancelLogin, always),
    ] {
        builder.register_action(ActionSpec { id, title, command, enablement });
    }

    for (menu, action, group, group_order, order, when) in [
        (
            DEVICE_CONTEXT,
            COMPOSE,
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
        (BROWSE_INLINE, COMPOSE, "primary", 10, 10, device_visible),
        (BROWSE_INLINE, CHOOSE_FILES, "primary", 10, 20, device_visible),
        (BROWSE_INLINE, INSPECT, "primary", 10, 10, item_visible),
        (BROWSE_INLINE, COPY, "primary", 10, 20, text_item_visible),
        (BROWSE_INLINE, SAVE, "primary", 10, 30, item_visible),
        (BROWSE_INLINE, OPEN, "primary", 10, 40, item_visible),
        (BROWSE_INLINE, DELETE, "destructive", 20, 10, item_visible),
        (BROWSE_INLINE, SEARCH, "scope", 30, 10, browse_visible),
        (BROWSE_INLINE, TOGGLE_RECEIVING, "scope", 30, 20, browse_visible),
        (AUTH_INLINE, OPEN_LOGIN, "auth", 10, 10, login_url_visible),
        (AUTH_INLINE, COPY_LOGIN, "auth", 10, 20, login_url_visible),
        (AUTH_INLINE, RETRY_LOGIN, "auth", 10, 30, retry_visible),
        (AUTH_INLINE, CANCEL_LOGIN, "navigation", 20, 10, auth_visible),
        (MODAL_INLINE, NEXT_DEVICE, "recipient", 10, 10, compose_or_review),
        (MODAL_INLINE, REVIEW_DRAFT, "primary", 20, 10, compose),
        (MODAL_INLINE, SEND, "primary", 20, 10, review),
        (MODAL_INLINE, USE_FILES, "primary", 20, 10, ambiguous),
        (MODAL_INLINE, USE_TEXT, "primary", 20, 20, ambiguous),
        (MODAL_INLINE, COPY, "primary", 20, 10, detail),
        (MODAL_INLINE, CONFIRM_DELETE, "destructive", 30, 10, confirm_delete),
        (MODAL_INLINE, OPEN_ENTRY, "primary", 20, 10, file_or_save_browser),
        (MODAL_INLINE, TOGGLE_FILE, "primary", 20, 20, file_browser),
        (MODAL_INLINE, SEND_SELECTED, "primary", 20, 30, file_browser),
        (MODAL_INLINE, PARENT_DIRECTORY, "navigation", 30, 10, file_or_save_browser),
        (MODAL_INLINE, SAVE_HERE, "primary", 20, 20, save_browser),
        (MODAL_INLINE, KEEP_BOTH, "primary", 20, 10, save_conflict),
        (MODAL_INLINE, REPLACE, "destructive", 30, 10, save_conflict),
        (MODAL_INLINE, CANCEL_OPERATION, "primary", 20, 10, busy),
        (MODAL_INLINE, BACK, "navigation", 30, 20, modal_back),
    ] {
        builder.place_menu(MenuPlacement { menu, action, group, group_order, order, when });
    }

    for (code, action, when) in [
        (KeyCode::Char('p'), COMPOSE, device_visible as fn(&TailActionContext) -> bool),
        (KeyCode::Char('f'), CHOOSE_FILES, device_visible),
        (KeyCode::Enter, INSPECT, item_visible),
        (KeyCode::Char('c'), COPY, text_item_visible),
        (KeyCode::Char('s'), SAVE, item_visible),
        (KeyCode::Char('o'), OPEN, item_visible),
        (KeyCode::Char('d'), DELETE, item_visible),
        (KeyCode::Char('/'), SEARCH, browse_visible),
        (KeyCode::Char('w'), TOGGLE_RECEIVING, browse_visible),
        (KeyCode::Char('r'), RESUME_RECEIVING, browse_visible),
    ] {
        builder.bind_key(KeybindingPlacement {
            chord: KeyChord::new(code, KeyModifiers::NONE),
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

fn draft(context: &TailActionContext) -> ActionState {
    if context.has_draft {
        ActionState::Enabled
    } else {
        ActionState::disabled("add text or files first")
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

fn browse_visible(context: &TailActionContext) -> bool {
    !matches!(context.target, TailActionTarget::Auth)
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

fn compose_or_review(context: &TailActionContext) -> bool {
    matches!(context.surface, TailSurface::Compose | TailSurface::Review)
}

fn compose(context: &TailActionContext) -> bool {
    context.surface == TailSurface::Compose
}

fn review(context: &TailActionContext) -> bool {
    context.surface == TailSurface::Review
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

fn busy(context: &TailActionContext) -> bool {
    context.surface == TailSurface::Busy
}

fn modal_back(context: &TailActionContext) -> bool {
    !matches!(context.surface, TailSurface::Browse | TailSurface::Auth | TailSurface::Busy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(target: TailActionTarget) -> TailActionContext {
        TailActionContext {
            surface: TailSurface::Browse,
            target,
            receiving: true,
            has_draft: false,
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
    fn device_context_only_exposes_share_actions() {
        let registry = registry().unwrap();
        let menu = registry
            .resolve_menu(DEVICE_CONTEXT, &context(TailActionTarget::Device("peer".into())));
        let ids = menu.items().iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![COMPOSE, CHOOSE_FILES]);
    }
}
