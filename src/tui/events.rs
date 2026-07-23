use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crossterm::event::{Event, EventStream, MouseEventKind};
use futures_util::task::{waker_ref, ArcWake};
use futures_util::Stream;
use tokio::sync::Notify;

const MAX_MOTION_DRAIN: usize = 256;

/// Demand-driven terminal input shared by every interactive Kit tool.
///
/// Crossterm owns the blocking terminal reader and exposes at most one outstanding wake task.
/// Kit retains at most one semantic event so a pointer-motion burst cannot grow an unbounded
/// channel. Only adjacent, replaceable `Moved` events are coalesced; every key, paste, resize,
/// focus, button, drag, and scroll event remains ordered and lossless.
pub struct EventReader {
    core: ReaderCore<EventStream>,
}

impl EventReader {
    pub fn start() -> Self {
        Self { core: ReaderCore::new(EventStream::new()) }
    }

    /// Awaits the next terminal event. `None` after crossterm closes or reports an error.
    pub async fn recv(&mut self) -> Option<Event> {
        self.core.recv().await
    }

    /// Returns the next event immediately available without blocking.
    pub fn try_recv(&mut self) -> Option<Event> {
        self.core.try_recv()
    }
}

struct ReaderCore<S> {
    stream: S,
    wake: Arc<StreamWake>,
    pending: Option<Event>,
    closed: bool,
}

impl<S> ReaderCore<S>
where
    S: Stream<Item = io::Result<Event>> + Unpin,
{
    fn new(stream: S) -> Self {
        Self { stream, wake: Arc::new(StreamWake::default()), pending: None, closed: false }
    }

    async fn recv(&mut self) -> Option<Event> {
        if let Some(event) = self.pending.take() {
            return Some(event);
        }
        let first = self.next_raw().await?;
        Some(self.coalesce_motion(first))
    }

    fn try_recv(&mut self) -> Option<Event> {
        if let Some(event) = self.pending.take() {
            return Some(event);
        }
        let first = self.poll_raw()?;
        Some(self.coalesce_motion(first))
    }

    async fn next_raw(&mut self) -> Option<Event> {
        loop {
            if let Some(event) = self.poll_raw() {
                return Some(event);
            }
            if self.closed {
                return None;
            }
            self.wake.notify.notified().await;
        }
    }

    fn poll_raw(&mut self) -> Option<Event> {
        if self.closed {
            return None;
        }
        let waker = waker_ref(&self.wake);
        let mut context = Context::from_waker(&waker);
        match Pin::new(&mut self.stream).poll_next(&mut context) {
            Poll::Ready(Some(Ok(event))) => Some(event),
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                self.closed = true;
                None
            }
            Poll::Pending => None,
        }
    }

    fn coalesce_motion(&mut self, first: Event) -> Event {
        if !is_replaceable_motion(&first) {
            return first;
        }
        let mut latest = first;
        for _ in 1..MAX_MOTION_DRAIN {
            let Some(next) = self.poll_raw() else {
                break;
            };
            if is_replaceable_motion(&next) {
                latest = next;
            } else {
                self.pending = Some(next);
                break;
            }
        }
        latest
    }
}

#[derive(Default)]
struct StreamWake {
    notify: Notify,
}

impl ArcWake for StreamWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.notify.notify_one();
    }
}

fn is_replaceable_motion(event: &Event) -> bool {
    matches!(event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use futures_util::stream;
    use futures_util::Stream;

    use super::{ReaderCore, MAX_MOTION_DRAIN};

    #[derive(Clone, Default)]
    struct StreamControl {
        state: Arc<Mutex<ControlledState>>,
    }

    #[derive(Default)]
    struct ControlledState {
        events: VecDeque<io::Result<Event>>,
        waker: Option<Waker>,
        closed: bool,
    }

    struct ControlledStream {
        state: Arc<Mutex<ControlledState>>,
    }

    impl StreamControl {
        fn stream(&self) -> ControlledStream {
            ControlledStream { state: Arc::clone(&self.state) }
        }

        fn push(&self, event: Event) {
            let waker = {
                let mut state = self.state.lock().unwrap();
                state.events.push_back(Ok(event));
                state.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }

        fn close(&self) {
            let waker = {
                let mut state = self.state.lock().unwrap();
                state.closed = true;
                state.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    impl Stream for ControlledStream {
        type Item = io::Result<Event>;

        fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let mut state = self.state.lock().unwrap();
            if let Some(event) = state.events.pop_front() {
                Poll::Ready(Some(event))
            } else if state.closed {
                Poll::Ready(None)
            } else {
                state.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }

    fn mouse(kind: MouseEventKind, column: u16) -> Event {
        Event::Mouse(MouseEvent { kind, column, row: 4, modifiers: KeyModifiers::NONE })
    }

    fn key(character: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
    }

    fn reader(
        events: Vec<io::Result<Event>>,
    ) -> ReaderCore<impl futures_util::Stream<Item = io::Result<Event>>> {
        ReaderCore::new(stream::iter(events))
    }

    #[tokio::test]
    async fn adjacent_pointer_motion_collapses_to_latest_position() {
        let mut reader = reader(vec![
            Ok(mouse(MouseEventKind::Moved, 1)),
            Ok(mouse(MouseEventKind::Moved, 2)),
            Ok(mouse(MouseEventKind::Moved, 3)),
        ]);

        assert_eq!(reader.recv().await, Some(mouse(MouseEventKind::Moved, 3)));
        assert_eq!(reader.recv().await, None);
    }

    #[tokio::test]
    async fn semantic_event_is_an_ordering_barrier_between_motion_bursts() {
        let mut reader = reader(vec![
            Ok(mouse(MouseEventKind::Moved, 1)),
            Ok(key('x')),
            Ok(mouse(MouseEventKind::Moved, 2)),
        ]);

        assert_eq!(reader.recv().await, Some(mouse(MouseEventKind::Moved, 1)));
        assert_eq!(reader.try_recv(), Some(key('x')));
        assert_eq!(reader.recv().await, Some(mouse(MouseEventKind::Moved, 2)));
    }

    #[tokio::test]
    async fn drag_scroll_press_and_release_are_never_coalesced() {
        let events = vec![
            mouse(MouseEventKind::Drag(MouseButton::Left), 1),
            mouse(MouseEventKind::ScrollDown, 2),
            mouse(MouseEventKind::Down(MouseButton::Left), 3),
            mouse(MouseEventKind::Up(MouseButton::Left), 4),
        ];
        let mut reader = reader(events.iter().cloned().map(Ok).collect());

        for event in events {
            assert_eq!(reader.recv().await, Some(event));
        }
    }

    #[tokio::test]
    async fn admitted_motion_is_returned_before_a_stream_error_closes_reader() {
        let mut reader = reader(vec![
            Ok(mouse(MouseEventKind::Moved, 7)),
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "terminal closed")),
        ]);

        assert_eq!(reader.recv().await, Some(mouse(MouseEventKind::Moved, 7)));
        assert_eq!(reader.recv().await, None);
    }

    #[tokio::test]
    async fn cancelling_a_pending_wait_does_not_consume_the_next_event() {
        let control = StreamControl::default();
        let mut reader = ReaderCore::new(control.stream());

        assert!(tokio::time::timeout(Duration::from_millis(1), reader.recv()).await.is_err());
        control.push(key('x'));

        assert_eq!(reader.recv().await, Some(key('x')));
    }

    #[tokio::test]
    async fn pending_reader_wakes_and_finishes_when_the_source_closes() {
        let control = StreamControl::default();
        let mut reader = ReaderCore::new(control.stream());
        let close = async {
            tokio::task::yield_now().await;
            control.close();
        };

        let (event, ()) = tokio::join!(reader.recv(), close);

        assert_eq!(event, None);
    }

    #[tokio::test]
    async fn sustained_motion_saturation_stays_bounded_and_preserves_buttons() {
        let motion_count = MAX_MOTION_DRAIN * 16 + 1;
        let mut events = Vec::with_capacity(motion_count + 2);
        for column in 0..motion_count {
            events.push(Ok(mouse(MouseEventKind::Moved, column as u16)));
        }
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 8);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 8);
        events.push(Ok(down.clone()));
        events.push(Ok(up.clone()));
        let mut reader = reader(events);

        for batch in 0..=16 {
            let expected = ((batch + 1) * MAX_MOTION_DRAIN).min(motion_count) - 1;
            assert_eq!(reader.recv().await, Some(mouse(MouseEventKind::Moved, expected as u16)));
        }
        assert_eq!(reader.pending.as_ref(), Some(&down));
        assert_eq!(reader.recv().await, Some(down));
        assert_eq!(reader.recv().await, Some(up));
        assert_eq!(reader.recv().await, None);
    }
}
