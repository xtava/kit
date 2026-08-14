//! Pure Console layout preference.

use crate::tui::SplitRatio;

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
