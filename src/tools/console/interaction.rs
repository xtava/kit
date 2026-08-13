//! Pure Console interaction policy.
//!
//! The renderer projects this state and the async shell executes its decisions. Keeping control
//! and layout transitions here prevents frame-local geometry or stale menu context from becoming
//! an authority source.

use crate::tui::SplitRatio;

use super::client::SessionControl;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionAccess {
    Synchronizing,
    Available,
    ControlledBySelf,
    ControlledByOther,
}

impl SessionAccess {
    pub(super) const fn permits_terminal_input(self) -> bool {
        matches!(self, Self::ControlledBySelf)
    }

    pub(super) const fn supports_local_terminal_tools(self) -> bool {
        !matches!(self, Self::Synchronizing)
    }

    pub(super) const fn primary_control(self) -> Option<ControlOperation> {
        match self {
            Self::Synchronizing => None,
            Self::Available => Some(ControlOperation::Acquire),
            Self::ControlledBySelf => Some(ControlOperation::Release),
            Self::ControlledByOther => Some(ControlOperation::Take),
        }
    }
}

impl From<SessionControl> for SessionAccess {
    fn from(control: SessionControl) -> Self {
        match control {
            SessionControl::Synchronizing => Self::Synchronizing,
            SessionControl::Uncontrolled => Self::Available,
            SessionControl::Controller => Self::ControlledBySelf,
            SessionControl::Observer => Self::ControlledByOther,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ControlIntent {
    Activate,
    Primary,
    Take,
    Release,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ControlOperation {
    Acquire,
    Take,
    Release,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InteractionDecision {
    FocusTerminal,
    Control(ControlOperation),
    Wait,
    Unavailable(&'static str),
}

/// Resolve a semantic intent against the latest authoritative access state.
///
/// In particular, callers should store `Primary` in frame hit maps rather than storing a derived
/// release/take command. That makes a click safe even if control changes between draw and dispatch.
pub(super) const fn resolve_control(
    intent: ControlIntent,
    access: SessionAccess,
) -> InteractionDecision {
    match intent {
        ControlIntent::Activate => match access {
            SessionAccess::Synchronizing => InteractionDecision::Wait,
            SessionAccess::Available => InteractionDecision::Control(ControlOperation::Acquire),
            SessionAccess::ControlledBySelf | SessionAccess::ControlledByOther => {
                InteractionDecision::FocusTerminal
            }
        },
        ControlIntent::Primary => match access.primary_control() {
            Some(operation) => InteractionDecision::Control(operation),
            None => InteractionDecision::Wait,
        },
        ControlIntent::Take => match access {
            SessionAccess::ControlledByOther => {
                InteractionDecision::Control(ControlOperation::Take)
            }
            SessionAccess::Available => InteractionDecision::Control(ControlOperation::Acquire),
            SessionAccess::ControlledBySelf => {
                InteractionDecision::Unavailable("this client already controls the session")
            }
            SessionAccess::Synchronizing => InteractionDecision::Wait,
        },
        ControlIntent::Release => match access {
            SessionAccess::ControlledBySelf => {
                InteractionDecision::Control(ControlOperation::Release)
            }
            SessionAccess::Synchronizing => InteractionDecision::Wait,
            SessionAccess::Available | SessionAccess::ControlledByOther => {
                InteractionDecision::Unavailable("this client does not control the session")
            }
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LayoutPreference {
    Split { sidebar_ratio: SplitRatio },
    TerminalOnly { restore_ratio: SplitRatio },
}

impl LayoutPreference {
    pub(super) const fn split(sidebar_ratio: SplitRatio) -> Self {
        Self::Split { sidebar_ratio }
    }

    pub(super) const fn restore_ratio(self) -> SplitRatio {
        match self {
            Self::Split { sidebar_ratio } => sidebar_ratio,
            Self::TerminalOnly { restore_ratio } => restore_ratio,
        }
    }

    pub(super) const fn terminal_only(self) -> Self {
        Self::TerminalOnly { restore_ratio: self.restore_ratio() }
    }

    pub(super) const fn split_view(self) -> Self {
        Self::Split { sidebar_ratio: self.restore_ratio() }
    }

    pub(super) const fn with_ratio(self, sidebar_ratio: SplitRatio) -> Self {
        match self {
            Self::Split { .. } => Self::Split { sidebar_ratio },
            Self::TerminalOnly { .. } => Self::TerminalOnly { restore_ratio: sidebar_ratio },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_access_has_one_primary_control_operation() {
        assert_eq!(SessionAccess::Synchronizing.primary_control(), None);
        assert_eq!(SessionAccess::Available.primary_control(), Some(ControlOperation::Acquire));
        assert_eq!(
            SessionAccess::ControlledBySelf.primary_control(),
            Some(ControlOperation::Release)
        );
        assert_eq!(
            SessionAccess::ControlledByOther.primary_control(),
            Some(ControlOperation::Take)
        );
    }

    #[test]
    fn activation_acquires_only_an_available_session() {
        assert_eq!(
            resolve_control(ControlIntent::Activate, SessionAccess::Available),
            InteractionDecision::Control(ControlOperation::Acquire)
        );
        assert_eq!(
            resolve_control(ControlIntent::Activate, SessionAccess::ControlledByOther),
            InteractionDecision::FocusTerminal
        );
        assert_eq!(
            resolve_control(ControlIntent::Activate, SessionAccess::Synchronizing),
            InteractionDecision::Wait
        );
    }

    #[test]
    fn observer_input_focuses_without_stealing_and_keeps_local_tools() {
        assert_eq!(
            resolve_control(ControlIntent::Activate, SessionAccess::ControlledByOther),
            InteractionDecision::FocusTerminal
        );
        assert!(!SessionAccess::ControlledByOther.permits_terminal_input());
        assert!(SessionAccess::ControlledByOther.supports_local_terminal_tools());
    }

    #[test]
    fn stale_primary_intent_resolves_against_current_access() {
        assert_eq!(
            resolve_control(ControlIntent::Primary, SessionAccess::Available),
            InteractionDecision::Control(ControlOperation::Acquire)
        );
        assert_eq!(
            resolve_control(ControlIntent::Primary, SessionAccess::ControlledBySelf),
            InteractionDecision::Control(ControlOperation::Release)
        );
        assert_eq!(
            resolve_control(ControlIntent::Primary, SessionAccess::ControlledByOther),
            InteractionDecision::Control(ControlOperation::Take)
        );
    }
}
