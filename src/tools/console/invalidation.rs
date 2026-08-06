use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use wezterm_mux::{Mux, MuxNotification};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ConsoleInvalidation {
    pub pane_output: bool,
    pub topology: bool,
    pub removed_panes: Vec<usize>,
}

pub(super) struct ConsoleInvalidations {
    state: Arc<InvalidationState>,
    receiver: mpsc::Receiver<()>,
}

impl ConsoleInvalidations {
    pub fn subscribe(mux: Arc<Mux>) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        let state = Arc::new(InvalidationState::default());
        let subscriber_state = Arc::clone(&state);
        mux.subscribe(move |notification| {
            if subscriber_state.closed.load(Ordering::Acquire) {
                return false;
            }
            subscriber_state.publish(&notification, &sender);
            true
        });
        Self { state, receiver }
    }

    pub async fn recv(&mut self) -> Option<ConsoleInvalidation> {
        self.receiver.recv().await?;
        Some(self.state.take())
    }
}

impl Drop for ConsoleInvalidations {
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Release);
        self.receiver.close();
        // Mux subscribers unregister only after returning false. A content-free notification makes
        // cleanup immediate instead of retaining this Console until unrelated pane activity.
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::Empty);
        }
    }
}

#[derive(Default)]
struct InvalidationState {
    closed: AtomicBool,
    pane_output: AtomicBool,
    topology: AtomicBool,
    removed_panes: Mutex<Vec<usize>>,
}

impl InvalidationState {
    fn publish(&self, notification: &MuxNotification, sender: &mpsc::Sender<()>) {
        if self.record(notification) {
            let _ = sender.try_send(());
        }
    }

    fn record(&self, notification: &MuxNotification) -> bool {
        let target = match notification {
            MuxNotification::PaneOutput(_) | MuxNotification::TabResized(_) => &self.pane_output,
            MuxNotification::PaneRemoved(pane_id) => {
                self.removed_panes.lock().unwrap().push(*pane_id);
                &self.topology
            }
            MuxNotification::PaneAdded(_)
            | MuxNotification::WindowCreated(_)
            | MuxNotification::WindowRemoved(_)
            | MuxNotification::WindowInvalidated(_)
            | MuxNotification::TabAddedToWindow { .. }
            | MuxNotification::TabTitleChanged { .. } => &self.topology,
            MuxNotification::ActiveWorkspaceChanged(_)
            | MuxNotification::Alert { .. }
            | MuxNotification::AssignClipboard { .. }
            | MuxNotification::Empty
            | MuxNotification::PaneFocused(_)
            | MuxNotification::SaveToDownloads { .. }
            | MuxNotification::WindowTitleChanged { .. }
            | MuxNotification::WindowWorkspaceChanged(_)
            | MuxNotification::WorkspaceRenamed { .. } => return false,
        };
        !target.swap(true, Ordering::AcqRel)
    }

    fn take(&self) -> ConsoleInvalidation {
        ConsoleInvalidation {
            pane_output: self.pane_output.swap(false, Ordering::AcqRel),
            topology: self.topology.swap(false, Ordering::AcqRel),
            removed_panes: std::mem::take(&mut *self.removed_panes.lock().unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use wezterm_mux::MuxNotification;

    use super::{ConsoleInvalidation, ConsoleInvalidations, InvalidationState};

    #[test]
    fn pending_state_coalesces_output_and_topology_without_losing_either_class() {
        let state = InvalidationState::default();
        assert!(state.record(&MuxNotification::PaneOutput(7)));
        assert!(!state.record(&MuxNotification::PaneOutput(8)));
        assert!(state.record(&MuxNotification::PaneRemoved(7)));
        assert_eq!(
            state.take(),
            ConsoleInvalidation { pane_output: true, topology: true, removed_panes: vec![7] }
        );
        assert_eq!(state.take(), ConsoleInvalidation::default());
    }

    #[test]
    fn presentation_only_notifications_do_not_wake_console() {
        let state = InvalidationState::default();
        assert!(!state.record(&MuxNotification::PaneFocused(3)));
        assert!(!state.record(&MuxNotification::WindowTitleChanged {
            window_id: 2,
            title: "ignored".to_owned(),
        }));
        assert_eq!(state.take(), ConsoleInvalidation::default());
    }

    #[test]
    fn terminal_resize_projects_content_without_reconciling_topology() {
        let state = InvalidationState::default();
        assert!(state.record(&MuxNotification::TabResized(7)));
        assert_eq!(
            state.take(),
            ConsoleInvalidation { pane_output: true, topology: false, removed_panes: Vec::new() }
        );
    }

    #[tokio::test]
    async fn capacity_one_wake_coalesces_output_and_topology_bursts() {
        let (sender, receiver) = mpsc::channel(1);
        let state = Arc::new(InvalidationState::default());
        let mut invalidations = ConsoleInvalidations { state: Arc::clone(&state), receiver };

        for pane_id in 0..10_000 {
            state.publish(&MuxNotification::PaneOutput(pane_id), &sender);
        }
        state.publish(&MuxNotification::PaneRemoved(7), &sender);

        assert_eq!(
            invalidations.recv().await,
            Some(ConsoleInvalidation { pane_output: true, topology: true, removed_panes: vec![7] })
        );
        assert!(invalidations.receiver.try_recv().is_err());
        assert_eq!(state.take(), ConsoleInvalidation::default());
    }
}
