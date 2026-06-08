use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use crossterm::event::{self, Event};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

const POLL_INTERVAL: Duration = Duration::from_millis(80);

/// Bridges crossterm's blocking event poll into an async channel, so a tool's `tokio::select!`
/// loop can await keystrokes alongside its own async work (resurveys, network checks).
///
/// The reader thread is stopped and joined on `Drop`.
pub struct EventReader {
    receiver: UnboundedReceiver<Event>,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl EventReader {
    pub fn start() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let handle = thread::spawn(move || pump(sender, thread_running));

        Self {
            receiver,
            running,
            handle: Some(handle),
        }
    }

    /// Awaits the next terminal event. `None` once the reader thread has stopped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.receiver.recv().await
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn pump(sender: UnboundedSender<Event>, running: Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        match event::poll(POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(event) => {
                    if sender.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    }
}
