use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;

use anyhow::Result;

use super::activity::AgentKind;
use super::client::CompletedSession;

#[cfg(target_os = "linux")]
const SYSTEM_SOUND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(target_os = "macos")]
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    fn NSBeep();
}

type DeliveryFuture =
    Pin<Box<dyn Future<Output = (CompletionNotifier, Result<()>)> + Send + 'static>>;

pub(super) enum NotificationRequest {
    Preview { body: String },
    Completions(CompletionBatch),
}

pub(super) enum CompletionBatch {
    Single { kind: AgentKind, session_title: String },
    Multiple { additional: NonZeroUsize },
}

struct SystemNotification {
    title: String,
    body: String,
}

#[derive(Default)]
struct PendingNotifications {
    completions: Option<CompletionBatch>,
    preview_body: Option<String>,
}

pub(super) struct CompletionDelivery {
    notifier: Option<CompletionNotifier>,
    in_flight: Option<DeliveryFuture>,
    pending: PendingNotifications,
}

#[derive(Default)]
struct CompletionNotifier {
    #[cfg(target_os = "linux")]
    connection: Option<zbus::Connection>,
}

impl NotificationRequest {
    pub(super) fn preview(session_title: Option<&str>) -> Self {
        Self::Preview { body: session_title.unwrap_or("System sound is ready").to_owned() }
    }

    fn into_notification(self) -> SystemNotification {
        match self {
            Self::Preview { body } => {
                SystemNotification { title: "Console notification preview".to_owned(), body }
            }
            Self::Completions(batch) => batch.into_notification(),
        }
    }
}

impl CompletionBatch {
    pub(super) fn new(
        first: CompletedSession,
        additional: &[CompletedSession],
        mut title_for: impl FnMut(usize) -> Option<String>,
    ) -> Self {
        match NonZeroUsize::new(additional.len()) {
            Some(additional) => Self::Multiple { additional },
            None => Self::Single {
                kind: first.kind,
                session_title: title_for(first.session_id)
                    .unwrap_or_else(|| "A Console session is ready".to_owned()),
            },
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Single { .. } => 1,
            Self::Multiple { additional } => additional.get().saturating_add(1),
        }
    }

    fn merge(self, other: Self) -> Self {
        let total = self.len().saturating_add(other.len());
        Self::Multiple {
            additional: NonZeroUsize::new(total.saturating_sub(1))
                .expect("merging two non-empty completion batches leaves additional completions"),
        }
    }

    fn into_notification(self) -> SystemNotification {
        match self {
            Self::Single { kind, session_title } => SystemNotification {
                title: format!("{} finished", kind.label()),
                body: session_title,
            },
            Self::Multiple { additional } => {
                let count = additional.get().saturating_add(1);
                SystemNotification {
                    title: format!("{count} agents finished"),
                    body: format!("{count} Console sessions are ready"),
                }
            }
        }
    }
}

impl Default for CompletionDelivery {
    fn default() -> Self {
        Self {
            notifier: Some(CompletionNotifier::default()),
            in_flight: None,
            pending: PendingNotifications::default(),
        }
    }
}

impl CompletionDelivery {
    pub(super) fn enqueue(&mut self, request: NotificationRequest) {
        if self.in_flight.is_none() {
            self.start(request);
            return;
        }
        match request {
            NotificationRequest::Completions(batch) => {
                self.pending.completions = Some(match self.pending.completions.take() {
                    Some(pending) => pending.merge(batch),
                    None => batch,
                });
            }
            NotificationRequest::Preview { body } => {
                self.pending.preview_body = Some(body);
            }
        }
    }

    pub(super) const fn is_active(&self) -> bool {
        self.in_flight.is_some()
    }

    pub(super) async fn wait(&mut self) -> Result<()> {
        let (notifier, result) =
            self.in_flight.as_mut().expect("notification wait is guarded by is_active").await;
        self.in_flight = None;
        self.notifier = Some(notifier);
        self.start_next();
        result
    }

    fn start_next(&mut self) {
        let next =
            self.pending.completions.take().map(NotificationRequest::Completions).or_else(|| {
                self.pending.preview_body.take().map(|body| NotificationRequest::Preview { body })
            });
        if let Some(request) = next {
            self.start(request);
        }
    }

    fn start(&mut self, request: NotificationRequest) {
        let mut notifier = self.notifier.take().expect("idle delivery owns its notifier");
        let notification = request.into_notification();
        self.in_flight = Some(Box::pin(async move {
            let result = notifier.system_sound(&notification).await;
            (notifier, result)
        }));
    }
}

#[cfg(target_os = "linux")]
impl CompletionNotifier {
    async fn system_sound(&mut self, notification: &SystemNotification) -> Result<()> {
        let result = self.try_system_sound(notification).await;
        if result.is_err() {
            self.connection = None;
        }
        result
    }

    async fn try_system_sound(&mut self, notification: &SystemNotification) -> Result<()> {
        use std::collections::HashMap;

        use anyhow::Context as _;
        use zbus::zvariant::{OwnedValue, Value};

        if self.connection.is_none() {
            self.connection = Some(
                tokio::time::timeout(SYSTEM_SOUND_TIMEOUT, zbus::Connection::session())
                    .await
                    .context("timed out connecting to the local desktop session bus")?
                    .context("connect to the local desktop session bus")?,
            );
        }
        let connection = self.connection.as_ref().expect("connection initialized above");
        let proxy = tokio::time::timeout(
            SYSTEM_SOUND_TIMEOUT,
            zbus::Proxy::new(
                connection,
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Notification",
            ),
        )
        .await
        .context("timed out connecting to the desktop notification portal")?
        .context("connect to the desktop notification portal")?;
        let mut payload = HashMap::<&str, OwnedValue>::new();
        payload.insert("title", Value::from(notification.title.clone()).try_into()?);
        payload.insert("body", Value::from(notification.body.clone()).try_into()?);
        payload.insert("priority", Value::from("normal").try_into()?);
        payload.insert("sound", Value::from("default").try_into()?);
        tokio::time::timeout(
            SYSTEM_SOUND_TIMEOUT,
            proxy.call::<_, _, ()>("AddNotification", &("kit-console-ready", payload)),
        )
        .await
        .context("timed out asking the desktop notification portal to play the system sound")?
        .context("ask the desktop notification portal to play the system sound")
    }
}

#[cfg(target_os = "macos")]
impl CompletionNotifier {
    async fn system_sound(&mut self, _notification: &SystemNotification) -> Result<()> {
        unsafe { NSBeep() };
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl CompletionNotifier {
    async fn system_sound(&mut self, _notification: &SystemNotification) -> Result<()> {
        anyhow::bail!("system sound is unavailable on this platform")
    }
}
